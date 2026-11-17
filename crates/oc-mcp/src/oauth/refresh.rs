//! Refreshing a stored MCP OAuth credential one at a time (`Credential.refresh`, `core/src/credential.ts`) and the
//! refresh itself (`exchange` in `core/src/mcp/oauth.ts`).
//!
//! Refresh tokens rotate and the row is shared by every connection, Location and process using the database, so a
//! refresh runs under the in-process share plus the file lock `credential-refresh:<id>`, reads the row again and
//! only spends the refresh token while the row still holds it. The refresh uses the discovery the login saved
//! (`metadata.discovery`), or discovers as the login did and saves the result.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use url::Url;

use super::discovery::{self, AuthServerMetadata, ResourceMetadata};
use super::fetch::Fetcher;
use super::{
    Discovery, McpCredentialStore, client_from_credential, configured_discovery, discovery_from_credential,
    select_resource, to_credential, token_request,
};
use crate::config::RemoteConfig;
use crate::error::McpError;

/// `Effect.timeout("30 seconds")` around the refresh, reading the body included.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a refresh produced no credential.
#[derive(Debug, Clone)]
pub enum RefreshError {
    /// `RefreshRejected`: the authorization server refused the refresh token or the client (`invalid_grant`,
    /// `invalid_client`, `unauthorized_client`), or there is no client information. Only a new login helps.
    Rejected(String),
    /// `IssuerChanged`: discovery named another authorization server than the one that issued the credential. Its
    /// tokens are not sent there; the credential stays for its own server.
    IssuerChanged(String),
    /// Another `OAuthError` the token endpoint answered: only its code is reported, since its description can quote
    /// the server's whole response.
    OAuth { code: String, message: String },
    /// A network failure, an `HTTP <status>` from the token endpoint, the time limit or the lock.
    Failed(String),
    /// Reading or writing the store failed (an upstream defect).
    Defect(String),
}

impl RefreshError {
    /// The `temporarily_unavailable` reason: an OAuth error names only its code.
    pub fn reason(&self) -> String {
        match self {
            Self::OAuth { code, .. } => code.clone(),
            Self::Rejected(message) | Self::IssuerChanged(message) | Self::Failed(message) | Self::Defect(message) => {
                message.clone()
            }
        }
    }
}

/// Like other MCP clients: refreshing this close to expiry leaves short-lived tokens alone until they are due.
pub const REFRESH_AHEAD_MS: u64 = 30_000;

/// `Credential.expiring(value, now, margin)`: whether an access token is due for a refresh. `expires` 0 means the
/// issuer gave no lifetime.
pub fn expiring(credential: &Value, now: u64, margin_ms: u64) -> bool {
    let expires = credential.get("expires").and_then(Value::as_u64).unwrap_or(0);
    expires != 0 && expires <= now + margin_ms
}

type Pending = Shared<BoxFuture<'static, Result<Option<Value>, RefreshError>>>;

// Concurrent callers for the same credential share the running refresh, including its failure.
static PENDING: Lazy<Mutex<HashMap<String, Pending>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn is_oauth(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("oauth")
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `Credential.refresh(id, {stale, refresh})`: refreshes a stored OAuth credential and saves the result. The row is
/// read again under the lock: when `stale` says it no longer needs a refresh, another caller already refreshed it
/// and that row is returned. When the row changes while `refresh` runs, the stored row wins. `Ok(None)` when the
/// credential is gone. The operation runs in its own task, so it finishes even when every caller goes away.
pub async fn refresh_credential<S, R>(
    store: Arc<dyn McpCredentialStore>,
    id: &str,
    stale: S,
    refresh: R,
) -> Result<Option<Value>, RefreshError>
where
    S: FnOnce(&Value) -> bool + Send + 'static,
    R: FnOnce(Value) -> BoxFuture<'static, Result<Value, RefreshError>> + Send + 'static,
{
    let pending = {
        let mut pending = PENDING.lock();
        match pending.get(id) {
            Some(running) => running.clone(),
            None => {
                let key = id.to_string();
                let task = tokio::spawn(async move {
                    let root = store.lock_root();
                    let lease = match &root {
                        Some(root) => match oc_util::flock::acquire(
                            root,
                            &format!("credential-refresh:{key}"),
                            oc_util::flock::Options::default(),
                        )
                        .await
                        {
                            Ok(lease) => Some(lease),
                            Err(error) => {
                                PENDING.lock().remove(&key);
                                return Err(RefreshError::Failed(error.to_string()));
                            }
                        },
                        None => None,
                    };
                    let result = locked(&*store, &key, stale, refresh).await;
                    if let Some(lease) = lease {
                        let _ = lease.release();
                    }
                    PENDING.lock().remove(&key);
                    result
                });
                let shared = async move { task.await.unwrap_or_else(|e| Err(RefreshError::Defect(e.to_string()))) }
                    .boxed()
                    .shared();
                pending.insert(id.to_string(), shared.clone());
                shared
            }
        }
    };
    pending.await
}

async fn locked<S, R>(store: &dyn McpCredentialStore, id: &str, stale: S, refresh: R) -> Result<Option<Value>, RefreshError>
where
    S: FnOnce(&Value) -> bool,
    R: FnOnce(Value) -> BoxFuture<'static, Result<Value, RefreshError>>,
{
    let read = || async {
        let row = store.get(id).await.map_err(|e| RefreshError::Defect(e.to_string()))?;
        Ok::<_, RefreshError>(row.map(|r| r.value).filter(is_oauth))
    };
    let Some(current) = read().await? else { return Ok(None) };
    if !stale(&current) {
        tracing::info!(credentialID = %id, reason = "already_refreshed", "credential refresh skipped");
        return Ok(Some(current));
    }
    let value = match tokio::time::timeout(REFRESH_TIMEOUT, refresh(current.clone())).await {
        Ok(result) => result?,
        Err(_) => return Err(RefreshError::Failed("Operation timed out after '30s'".into())),
    };
    let Some(latest) = read().await? else { return Ok(None) };
    if text(&latest, "access") != text(&current, "access") || text(&latest, "refresh") != text(&current, "refresh") {
        tracing::warn!(credentialID = %id, reason = "changed_meanwhile", "credential refresh discarded");
        return Ok(Some(latest));
    }
    store.update(id, value.clone()).await.map_err(|e| RefreshError::Defect(e.to_string()))?;
    Ok(Some(value))
}

const REJECTIONS: [&str; 3] = ["invalid_grant", "invalid_client", "unauthorized_client"];

fn classify(error: McpError) -> RefreshError {
    match error {
        McpError::OAuth { code, message } if REJECTIONS.contains(&code.as_str()) => RefreshError::Rejected(message),
        McpError::OAuth { code, message } => RefreshError::OAuth { code, message },
        other => RefreshError::Failed(other.to_string()),
    }
}

/// `exchange`: refreshes `current` with the discovery its login saved, or discovers as the login did (following the
/// 401's `resource_metadata` when given) and saves the result. A configured `auth_server_metadata_url` is read on
/// every refresh, so changing it applies at once, and no discovery is saved then. `fetch` should fail token-endpoint
/// answers 429/5xx ([`Fetcher::failing_server_errors`]).
pub async fn exchange(
    config: &RemoteConfig,
    fetch: &Fetcher,
    current: &Value,
    resource_metadata_url: Option<&Url>,
) -> Result<Value, RefreshError> {
    let oauth = config.oauth_config();
    let client = match oauth.and_then(|o| o.client_id.clone()) {
        Some(id) => {
            let mut client = Map::new();
            client.insert("client_id".into(), json!(id));
            if let Some(secret) = oauth.and_then(|o| o.client_secret.clone()) {
                client.insert("client_secret".into(), json!(secret));
            }
            Some(client)
        }
        None => client_from_credential(current),
    };
    let Some(client) = client else { return Err(RefreshError::Rejected("no OAuth client information".into())) };
    let saved = discovery_from_credential(current);
    let configured = configured_discovery(config, fetch).await.map_err(classify)?;
    let (authorization_server_url, metadata, resource_metadata): (
        String,
        Option<AuthServerMetadata>,
        Option<ResourceMetadata>,
    ) = match (&configured, &saved) {
        (Some(state), _) => (
            state.authorization_server_url.clone(),
            state.authorization_server_metadata.clone(),
            state.resource_metadata.clone(),
        ),
        (None, Some(saved)) => (
            saved.authorization_server.clone(),
            discovery::discover_authorization_server(fetch, &saved.authorization_server).await.map_err(classify)?,
            None,
        ),
        (None, None) => {
            let server_url =
                Url::parse(&config.url).map_err(|_| RefreshError::Failed(format!("Invalid URL: {}", config.url)))?;
            let info = discovery::discover_server_info(fetch, &server_url, resource_metadata_url)
                .await
                .map_err(classify)?;
            (info.authorization_server_url, info.authorization_server_metadata, info.resource_metadata)
        }
    };
    let found = metadata.as_ref().map(|m| m.issuer().to_string()).unwrap_or_else(|| authorization_server_url.clone());
    let trim = |s: &str| s.strip_suffix('/').unwrap_or(s).to_string();
    if let Some(issuer) = current.get("metadata").and_then(|m| m.get("issuer")).and_then(Value::as_str) {
        if trim(&found) != trim(issuer) {
            return Err(RefreshError::IssuerChanged(format!("issued by {issuer}, but discovery names {found}")));
        }
    }
    let resource = match &saved {
        Some(saved) => saved.resource.clone(),
        None => select_resource(config, resource_metadata.as_ref().map(|r| r.resource()))
            .map_err(classify)?
            .to_string(),
    };
    let resource_url = Url::parse(&resource).map_err(|_| RefreshError::Failed(format!("Invalid URL: {resource}")))?;
    let refresh = current.get("refresh").and_then(Value::as_str).unwrap_or("").to_string();
    let params = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh.clone()),
    ];
    let mut tokens = token_request(fetch, &authorization_server_url, metadata.as_ref(), params, &client, &resource_url)
        .await
        .map_err(classify)?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh);
    }
    tokens.issuer = Some(found);
    let discovery = configured.is_none().then(|| Discovery { authorization_server: authorization_server_url, resource });
    let method_id = current.get("methodID").and_then(Value::as_str).unwrap_or("");
    Ok(to_credential(method_id, &config.url, &tokens, client_from_credential(current).as_ref(), discovery.as_ref()))
}
