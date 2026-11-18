//! Default tool services (formatter, ripgrep, shell, websearch) wired to core storage.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::app::App;

/// `WebSearchStore` over the `kv` table and the integration credentials.
pub struct WebSearchKv {
    pub app: Arc<App>,
}

#[async_trait]
impl oc_tools::websearch::WebSearchStore for WebSearchKv {
    async fn kv_get(&self, key: &str) -> Option<Value> {
        self.app.db.kv_get(key).ok().flatten()
    }
    async fn kv_set(&self, key: &str, value: Value) {
        let _ = self.app.db.kv_set(key, &value);
    }
    async fn kv_remove(&self, key: &str) {
        let _ = self.app.db.kv_remove(key);
    }
    async fn api_key(&self, integration_id: &str) -> Option<String> {
        let row: Option<String> = self
            .app
            .db
            .with(|c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT value FROM credential WHERE integration_id = ? ORDER BY active DESC, time_created DESC, id DESC LIMIT 1",
                    [integration_id],
                    |r| r.get(0),
                )
                .optional()
            })
            .ok()
            .flatten();
        let v: Value = serde_json::from_str(&row?).ok()?;
        (v["type"] == "key").then(|| v["key"].as_str().map(str::to_string)).flatten()
    }
}

pub fn default_services(app: &Arc<App>) -> oc_tools::tools::Services {
    let http = oc_util::http::client();
    let bus = app.bus.clone();
    // Upstream's Shell service is location-scoped, so its events carry the location of the shell's owner:
    // the session's location for tool and `!` shells, else the working directory. Exits reuse the
    // location recorded at creation.
    let locations: Arc<parking_lot::Mutex<std::collections::HashMap<String, oc_schema::types::LocationRef>>> =
        Arc::default();
    let sink: oc_tools::shell::service::EventSink = Arc::new(move |ev| {
        use oc_tools::shell::service::ShellEvent as E;
        // A daemon command names its location (also for one taken back after a restart, which was never created
        // here).
        let named = |location: &Option<oc_tools::shell::service::ShellLocation>| {
            location.as_ref().map(|l| oc_schema::types::LocationRef { directory: l.directory.clone(), workspace_id: l.workspace_id.clone() })
        };
        let location = match &ev {
            E::Created { info, location } => {
                let location = named(location).unwrap_or_else(|| {
                    let session = info.metadata.get("sessionID").and_then(Value::as_str);
                    session
                        .and_then(|s| bus.db().session_get(s).ok().flatten())
                        .map(|s| s.location)
                        .unwrap_or_else(|| oc_schema::types::LocationRef { directory: info.cwd.clone(), workspace_id: None })
                });
                locations.lock().insert(info.id.clone(), location.clone());
                Some(location)
            }
            E::Exited { id, location, .. } => named(location).or_else(|| locations.lock().get(id).cloned()),
            E::Deleted { id, location } => {
                let recorded = locations.lock().remove(id);
                named(location).or(recorded)
            }
        };
        let (kind, data) = shell_event(&ev);
        bus.publish(kind, data, location);
    });
    oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), http.clone())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), Some(sink)),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(WebSearchKv { app: app.clone() }), http.clone())),
        http,
    }
}

fn shell_event(ev: &oc_tools::shell::service::ShellEvent) -> (&'static str, Value) {
    use oc_tools::shell::service::ShellEvent as E;
    match ev {
        // A daemon command is announced with the `pid` and start time the daemon gave it (core/src/shell.ts
        // `Shell.Event.Created, { info: run.info }`); a spawned one with its own.
        E::Created { info, .. } => ("shell.created", serde_json::json!({"info": info})),
        E::Exited { id, exit, status, .. } => {
            // `exit` is omitted when the command has no exit code (killed, timed out, signalled).
            let mut data = serde_json::json!({ "id": id });
            if let Some(exit) = exit {
                data["exit"] = serde_json::json!(exit);
            }
            data["status"] = serde_json::json!(status);
            ("shell.exited", data)
        }
        E::Deleted { id, .. } => ("shell.deleted", serde_json::json!({"id": id})),
    }
}
