//! The built-in fork notice plugin (`core/src/plugin/fork-notice.ts`, 2.3.0 #63235): every forked session gets a
//! synthetic `<fork-notice>` message, admitted without resuming the session, so its next turn knows the original
//! conversation keeps going on its own.

use crate::session::{SessionError, Sessions};

/// The plugin's ID in `plugin.list` (a `pre` plugin, after `opencode.agent`).
pub const PLUGIN_ID: &str = "opencode.fork-notice";

pub const NOTICE: &str = "<fork-notice>
This conversation is a fork of another conversation, copied up to the previous message. The original conversation keeps going on its own and may be active right now, as may other forks of it.
- Work started before the fork belongs to the original: background subagents, background shells, schedules, and follow-ups promised there. Their results and notifications go to the original, never here, and you can't continue those subagents. Don't wait for them, redo them, or report their results as yours; if it matters, check what already happened (files, git, PRs, CI).
- Files, repositories, branches and external services are shared with the original. Before changing something the original was working on (its uncommitted changes, branches, worktrees, PRs, deploys, messages), check its current state, and put work that diverges from it on a new branch or worktree.
- Otherwise work normally: new tasks need no extra checks, branches or questions.
- If it's unclear whether this fork or the original should do something, ask.
</fork-notice>";

/// The notice's message ID in the fork (`msg_fork_notice_<sessionID>`).
pub fn message_id(session_id: &str) -> String {
    format!("msg_fork_notice_{session_id}")
}

/// The plugin's `fork` hook: admits the notice into the fork (`resume: false`).
pub async fn admit(sessions: &Sessions, session_id: &str) -> Result<(), SessionError> {
    sessions
        .synthetic(session_id, Some(message_id(session_id)), NOTICE.to_string(), Some("Fork notice".to_string()), None, None)
        .await
        .map(|_| ())
}
