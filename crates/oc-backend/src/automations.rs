//! `automation.*`, `generate.video.*` and `blob.get` in the `server.experimental` group, as opencode 2.3.1 answers
//! them in a release build.
//!
//! Automations (`server/src/handlers/automation.ts`) replaced scheduled prompts in 2.3.1 and need
//! `experimental.automations` where they run. ocd does not port them yet, so every location reads as having them
//! off: the routes check their input, sessions, directories and stored rows like upstream, then answer
//! `403 ForbiddenError`. Video generation (`handlers/generate.ts`) answers `403` behind `experimental.video_generate`
//! the same way. Blobs (`handlers/blob.ts`, `core/src/blob-store.ts`) are files under `<data>/blobs/`, served
//! without a flag.

use oc_server::generated as api;
use oc_server::{ApiError, BlobFile};
use rusqlite::OptionalExtension;

use crate::engine::Engine;
use crate::util::defect;

/// `Automation.DisabledError`'s message.
pub const AUTOMATIONS_DISABLED: &str = "Automations are disabled; set experimental.automations in config";
/// What `generate.video.*` answers while `experimental.video_generate` is off.
pub const VIDEO_DISABLED: &str = "Video generation is disabled; enable experimental.video_generate";
/// `BlobStore.DIRECTORY`, under the data folder.
pub const BLOB_DIRECTORY: &str = "blobs";

fn disabled() -> ApiError {
    ApiError::forbidden(AUTOMATIONS_DISABLED)
}

fn invalid(message: String) -> ApiError {
    ApiError::invalid_request(message).with_kind("automation")
}

/// Node's `path.isAbsolute` for the platform.
fn is_absolute(input: &str) -> bool {
    std::path::Path::new(input).is_absolute() || (cfg!(windows) && (input.starts_with('/') || input.starts_with('\\')))
}

impl Engine {
    /// `create`: the owner session (404), the directory (its own, the payload's or the home folder; absolute and
    /// existing, else 400), then the flag where it runs (403).
    pub async fn automation_create(&self, payload: &api::AutomationCreatePayload) -> Result<api::AutomationInfo, ApiError> {
        let owner = payload.session_id.as_deref().map(|id| self.session(id)).transpose()?;
        let directory = match (&owner, &payload.directory) {
            (Some(session), _) => session.location.directory.clone(),
            (None, Some(directory)) => directory.clone(),
            (None, None) => self.home().display().to_string(),
        };
        if !is_absolute(&directory) {
            return Err(invalid(format!("Directory must be an absolute path: {directory}")));
        }
        if !std::path::Path::new(&directory).is_dir() {
            return Err(invalid(format!("Directory does not exist: {directory}")));
        }
        Err(disabled())
    }

    /// `list`: a session filter needs the session (404) and the flag where it lives; a directory filter must be
    /// absolute (400) and needs the flag there, unless the folder is gone. Rows show only where the flag allows
    /// them, which is nowhere.
    pub async fn automation_list(&self, req: &api::ExperimentalAutomationListRequest) -> Result<Vec<api::AutomationInfo>, ApiError> {
        if let Some(id) = &req.session_id {
            let session = self.session(id)?;
            if std::path::Path::new(&session.location.directory).is_dir() {
                return Err(disabled());
            }
        }
        if let Some(directory) = &req.directory {
            if !is_absolute(directory) {
                return Err(invalid(format!("Directory must be an absolute path: {directory}")));
            }
            if std::path::Path::new(directory).is_dir() {
                return Err(disabled());
            }
        }
        Ok(Vec::new())
    }

    /// `find`: an automation that is not stored is a 404; one that is reads as forbidden.
    pub async fn automation_find(&self, id: &str) -> Result<api::AutomationInfo, ApiError> {
        let stored = self
            .app
            .db
            .with(|c| c.query_row("SELECT 1 FROM automation WHERE id = ?", [id], |_| Ok(())).optional())
            .map_err(defect)?
            .is_some();
        if !stored {
            return Err(ApiError::automation_not_found(id));
        }
        Err(disabled())
    }

    /// `BlobStore.read`: the file named by the ID, its size, and the type read from its first 64 bytes.
    pub async fn blob_read(&self, id: &str) -> Result<BlobFile, ApiError> {
        let path = self.app.global.data.join(BLOB_DIRECTORY).join(id);
        let missing = || ApiError::blob_not_found(id);
        let meta = tokio::fs::metadata(&path).await.map_err(|_| missing())?;
        if !meta.is_file() {
            return Err(missing());
        }
        let head = {
            use tokio::io::AsyncReadExt;
            let mut file = tokio::fs::File::open(&path).await.map_err(|_| missing())?;
            let mut buf = Vec::with_capacity(64);
            (&mut file).take(64).read_to_end(&mut buf).await.map_err(|_| missing())?;
            buf
        };
        let mime = oc_ai::media::detect_media_type(&head).unwrap_or("application/octet-stream").to_string();
        Ok(BlobFile { path, mime, bytes: meta.len() })
    }
}
