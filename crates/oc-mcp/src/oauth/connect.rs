//! The bearer tokens a connection sends (`connectProvider`, `core/src/mcp/oauth.ts`): the SDK's `AuthProvider`
//! with `token()` and `onUnauthorized`.
//!
//! It reads and refreshes only the credential row the caller selected, and one made without a credential stays
//! anonymous. A connection never starts a login, so a token the server refuses and that cannot be refreshed ends in
//! needs_auth.

use std::sync::Arc;

use futures::FutureExt;
use parking_lot::Mutex;
use reqwest::header::HeaderMap;
use serde_json::Value;
use url::Url;

use super::discovery::parse_www_authenticate;
use super::fetch::Fetcher;
use super::refresh::{self, REFRESH_AHEAD_MS, RefreshError};
use super::{McpCredentialStore, StoredCredential, discovery_from_credential, now_ms};
use crate::config::RemoteConfig;
use crate::error::McpError;

/// A connection's `AuthProvider`.
pub struct ConnectAuth {
    credential: Option<Credentialed>,
}

struct Credentialed {
    config: RemoteConfig,
    credentials: Arc<dyn McpCredentialStore>,
    id: String,
    fetch: Fetcher,
    // The token last sent. A 401 refreshes only while the row still holds it: another connection or process may
    // have refreshed it already.
    presented: Mutex<Option<String>>,
}

fn is_oauth(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("oauth")
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `connectProvider`: the provider for the credential the caller selected, or an anonymous one. `fetch` is the
/// server's logged fetch for the OAuth requests a refresh makes.
pub fn connect_provider(
    config: &RemoteConfig,
    credential: Option<StoredCredential>,
    credentials: Option<&Arc<dyn McpCredentialStore>>,
    fetch: Fetcher,
) -> ConnectAuth {
    let selected = credential.filter(|c| is_oauth(&c.value)).zip(credentials.cloned());
    ConnectAuth {
        credential: selected.map(|(found, credentials)| Credentialed {
            config: config.clone(),
            credentials,
            id: found.id,
            fetch: fetch.failing_server_errors(),
            presented: Mutex::new(None),
        }),
    }
}

impl ConnectAuth {
    /// A provider without a credential: no token, and a 401 ends in `UnauthorizedError`.
    pub fn anonymous() -> Self {
        Self { credential: None }
    }

    /// Whether the provider has `onUnauthorized` (only one made with an OAuth credential does).
    pub fn handles_unauthorized(&self) -> bool {
        self.credential.is_some()
    }

    /// `token()`: the stored access token. Refreshing ahead needs the discovery the login saved: a token that expires
    /// within 30 seconds is refreshed first, and if that fails the current token goes out and a 401 decides.
    pub async fn token(&self) -> Result<Option<String>, McpError> {
        let Some(credential) = &self.credential else { return Ok(None) };
        let current = credential.read().await?;
        let ahead = current.as_ref().is_some_and(|current| {
            !text(current, "refresh").is_empty()
                && discovery_from_credential(current).is_some()
                && refresh::expiring(current, now_ms(), REFRESH_AHEAD_MS)
        });
        let value = if ahead {
            match credential.refresh(|current| refresh::expiring(current, now_ms(), REFRESH_AHEAD_MS), None).await {
                Ok(value) => value,
                Err(_) => current,
            }
        } else {
            current
        };
        let token = value.map(|v| text(&v, "access").to_string());
        *credential.presented.lock() = token.clone();
        Ok(token.filter(|t| !t.is_empty()))
    }

    /// `onUnauthorized`: refreshes while the row still holds the rejected token. A rejected refresh (the credential is
    /// removed then), an issuer mismatch, a missing refresh token or a signed-out credential refuse with
    /// `UnauthorizedError` (needs_auth); anything else fails this attempt with `temporarily_unavailable` and keeps the
    /// credential for the next one.
    pub async fn on_unauthorized(&self, headers: &HeaderMap) -> Result<(), McpError> {
        let Some(credential) = &self.credential else { return Err(McpError::Unauthorized("Unauthorized".into())) };
        let refused =
            || McpError::Unauthorized(format!("MCP server \"{}\" requires user authorization", credential.config.url));
        let rejected = credential.presented.lock().clone();
        let current = credential.read().await?;
        if current.as_ref().is_none_or(|c| text(c, "refresh").is_empty()) {
            return Err(refused());
        }
        let resource_metadata_url = parse_www_authenticate(headers).resource_metadata_url;
        let stale = move |current: &Value| Some(text(current, "access")) == rejected.as_deref();
        match credential.refresh(stale, resource_metadata_url).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(RefreshError::Rejected(_)) | Err(RefreshError::IssuerChanged(_)) => Err(refused()),
            Err(RefreshError::Defect(message)) => Err(McpError::other(message)),
            // Not a lost login: keep the credential and fail this attempt so the next one tries again.
            Err(error) => Err(McpError::OAuth {
                code: "temporarily_unavailable".into(),
                message: format!("Could not refresh the OAuth token: {}", error.reason()),
            }),
        }
    }
}

impl Credentialed {
    async fn read(&self) -> Result<Option<Value>, McpError> {
        let row = self.credentials.get(&self.id).await.map_err(|e| McpError::other(e.to_string()))?;
        Ok(row.map(|r| r.value).filter(is_oauth))
    }

    /// `credentials.refresh(id, {stale, refresh: exchange})`; a rejected refresh removes the credential under the lock.
    async fn refresh<S>(&self, stale: S, resource_metadata_url: Option<Url>) -> Result<Option<Value>, RefreshError>
    where
        S: FnOnce(&Value) -> bool + Send + 'static,
    {
        let (config, fetch, credentials, id) =
            (self.config.clone(), self.fetch.clone(), self.credentials.clone(), self.id.clone());
        refresh::refresh_credential(self.credentials.clone(), &self.id, stale, move |current| {
            async move {
                let result = refresh::exchange(&config, &fetch, &current, resource_metadata_url.as_ref()).await;
                if let Err(RefreshError::Rejected(reason)) = &result {
                    tracing::warn!(credentialID = %id, reason = %reason, "mcp oauth credential invalidated");
                    credentials.remove(&id).await.map_err(|e| RefreshError::Defect(e.to_string()))?;
                }
                result
            }
            .boxed()
        })
        .await
    }
}
