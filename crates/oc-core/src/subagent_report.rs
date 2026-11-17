//! Subagent reports per busy period (`core/session/execution/subagent-report.ts`, 2.3.0).
//!
//! When a child session's busy period settles, its report to the parent is written to the `job` table (the
//! "outbox") inside the terminal event's commit, together with the claim release. After the commit the report is
//! admitted into the parent's inbox as a steering `<subagent>` notice, the row is acknowledged, and the parent is
//! woken. A child started by the `subagent` tool reports through its backgrounded job's notification ID; a child
//! driven directly (a session created with a `parentID` and prompted over HTTP) reports with a fresh one, but only
//! when the period produced an assistant message, failed or was stopped.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

use oc_schema::Message;
use oc_tools::host::{JobRecovery, JobStatus};

use crate::app::App;
use crate::job::{Jobs, SubagentOutboxReport, SubagentReportState, NO_TEXT};

/// How a busy period ended (`SessionExecution.terminal`), without the shutdown interruption that reports nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Succeeded,
    Failed(String),
    /// A user (or other non-shutdown) interruption.
    Interrupted,
}

/// A busy period's boundary: the `session.execution.started` sequence and the job generation bound to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Period {
    pub started_seq: i64,
    pub bound_generation: Option<u64>,
}

enum PostCommit {
    None,
    Cleanup(String),
    Deliver(SubagentOutboxReport),
}

#[derive(Default)]
pub struct SubagentReport {
    periods: Mutex<HashMap<String, Period>>,
}

/// The session columns a settlement reads inside its commit.
struct Row {
    parent_id: Option<String>,
    agent: Option<String>,
    title: Option<String>,
}

fn session_row(c: &Connection, id: &str) -> rusqlite::Result<Option<Row>> {
    c.query_row("SELECT parent_id, agent, title FROM session_v2 WHERE id = ?", [id], |r| {
        Ok(Row { parent_id: r.get(0)?, agent: r.get(1)?, title: r.get(2)? })
    })
    .optional()
}

/// `SessionStore.lastAssistantAfter`: the newest completed, error-free assistant message above the sequence bound,
/// read newest first in pages of 20.
pub fn last_assistant_after(c: &Connection, session: &str, after_seq: Option<i64>) -> anyhow::Result<Option<Message>> {
    let mut cursor: Option<i64> = None;
    loop {
        let mut rows: Vec<(i64, String, String, String)> = {
            let mut s = c.prepare(
                "SELECT seq, id, type, data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
                 AND (?2 IS NULL OR seq > ?2) AND (?3 IS NULL OR seq < ?3) ORDER BY seq DESC LIMIT 20",
            )?;
            let r = s.query_map(rusqlite::params![session, after_seq, cursor], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            r.collect::<rusqlite::Result<_>>()?
        };
        oc_storage::stored::restore(c, &mut rows)?;
        for (_, id, kind, data) in &rows {
            let message = Message::from_row(id, kind, data).map_err(|e| anyhow::anyhow!("{e}"))?;
            if let Message::Assistant(a) = &message {
                if a.time.completed.is_some() && a.error.is_none() {
                    return Ok(Some(message));
                }
            }
        }
        if rows.len() < 20 {
            return Ok(None);
        }
        cursor = rows.last().map(|r| r.0);
    }
}

/// A prepared settlement: [`Settlement::commit`] runs inside the terminal event's commit, then
/// [`Settlement::deliver`] after it.
pub struct Settlement {
    session_id: String,
    outcome: Outcome,
    period: Option<Period>,
    job: SubagentReportState,
    /// Whether anything beyond the claim release can happen.
    reports: bool,
    running: Vec<serde_json::Value>,
    post: Mutex<PostCommit>,
}

impl SubagentReport {
    pub fn started(&self, session: &str, period: Period) {
        self.periods.lock().insert(session.to_string(), period);
    }

    pub fn clear(&self, session: &str) {
        self.periods.lock().remove(session);
    }

    /// `prepare`: claim the job generation of this busy period and snapshot the child's outstanding work.
    pub fn prepare(&self, app: &App, session: &str, outcome: Outcome) -> Settlement {
        let period = self.periods.lock().remove(session);
        let job = app.jobs.prepare_subagent_report(session, period.and_then(|p| p.bound_generation));
        let parent = app.db.session_get(session).ok().flatten().and_then(|s| s.parent_id);
        let reports = parent.is_some() || job != SubagentReportState::None;
        let running = match (&parent, app.coordinator.get()) {
            (Some(_), Some(c)) => crate::job::outstanding(app, session, &c.draining()),
            (Some(_), None) => crate::job::outstanding(app, session, &[]),
            _ => vec![],
        };
        Settlement {
            session_id: session.to_string(),
            outcome,
            period,
            job,
            reports,
            running,
            post: Mutex::new(PostCommit::None),
        }
    }
}

impl Settlement {
    /// Runs inside the terminal event's `BEGIN IMMEDIATE` commit: SQL on that connection only.
    pub fn commit(&self, c: &Connection) -> anyhow::Result<()> {
        *self.post.lock() = PostCommit::None;
        crate::session::Sessions::release(c, &self.session_id)?;
        if !self.reports {
            return Ok(());
        }
        let child = session_row(c, &self.session_id)?;
        let parent_id = child.as_ref().and_then(|c| c.parent_id.clone());
        let parent = match &parent_id {
            Some(p) => session_row(c, p)?,
            None => None,
        };
        let (Some(child), Some(parent_id), Some(_)) = (child, parent_id, parent) else {
            if let SubagentReportState::Background { notification_id, .. } = &self.job {
                Jobs::delete_background_row(c, notification_id)?;
                *self.post.lock() = PostCommit::Cleanup(notification_id.clone());
            }
            return Ok(());
        };
        let background = matches!(self.job, SubagentReportState::Background { .. });
        let (status, body) = match &self.outcome {
            Outcome::Failed(message) => (JobStatus::Error, message.clone()),
            Outcome::Interrupted => (JobStatus::Cancelled, "Subagent cancelled".to_string()),
            Outcome::Succeeded => {
                // A corrupt message row must not abort the release or skip the outbox row.
                match last_assistant_after(c, &self.session_id, self.period.map(|p| p.started_seq)) {
                    Err(e) => (JobStatus::Error, e.to_string()),
                    Ok(Some(message)) => (JobStatus::Completed, crate::job::subagent_text(Some(&message))),
                    // An empty wake or control-only period has no result to report unless a job promised one.
                    Ok(None) if background => (JobStatus::Completed, NO_TEXT.to_string()),
                    Ok(None) => return Ok(()),
                }
            }
        };
        let formatted = crate::job::format_body(&body, &self.session_id, &self.running);
        let (notification_id, recovery, started_at) = match &self.job {
            SubagentReportState::Background { notification_id, recovery, started_at } => {
                (notification_id.clone(), recovery.clone(), Some(*started_at))
            }
            SubagentReportState::None => (
                oc_schema::id::message(),
                JobRecovery::Subagent {
                    parent_session_id: parent_id,
                    child_session_id: self.session_id.clone(),
                    agent: child.agent.unwrap_or_else(|| "subagent".into()),
                    description: child.title.unwrap_or_else(|| "Subagent".into()),
                },
                None,
            ),
        };
        let completed = status == JobStatus::Completed;
        let report = SubagentOutboxReport {
            notification_id,
            recovery,
            status,
            output: completed.then(|| formatted.clone()),
            error: (!completed).then_some(formatted),
            started_at,
        };
        Jobs::record_subagent_report(c, &report)?;
        *self.post.lock() = PostCommit::Deliver(report);
        Ok(())
    }

    /// Post-commit delivery: acknowledge a report owed to nobody, or admit the report in the parent, acknowledge
    /// its row and wake the parent (unless it has a staged revert). A conflicting notification ID keeps the row for
    /// the next boot; a parent deleted meanwhile acknowledges it.
    pub async fn deliver(&self, app: &Arc<App>) -> anyhow::Result<()> {
        let post = std::mem::replace(&mut *self.post.lock(), PostCommit::None);
        match post {
            PostCommit::None => Ok(()),
            PostCommit::Cleanup(nid) => {
                app.jobs.complete_background(&nid);
                Ok(())
            }
            // Admitted by the global execution layer, outside any location (no envelope location).
            PostCommit::Deliver(report) => crate::bus::without_location_async(deliver_report(app, report)).await,
        }
    }
}

async fn deliver_report(app: &Arc<App>, report: SubagentOutboxReport) -> anyhow::Result<()> {
    let JobRecovery::Subagent { parent_session_id, child_session_id, .. } = &report.recovery else { return Ok(()) };
    if app.sessions.get(parent_session_id).is_err() {
        app.jobs.complete_background(&report.notification_id);
        return Ok(());
    }
    let delivered = crate::job::deliver_subagent(
        app,
        report.status,
        report.output.clone(),
        report.error.clone(),
        Some(report.notification_id.clone()),
        &report.recovery,
    )
    .await;
    match delivered {
        Ok(()) => {}
        Err(crate::session::SessionError::PromptConflict(_)) => {
            tracing::warn!(
                childSessionID = child_session_id.as_str(),
                parentSessionID = parent_session_id.as_str(),
                notificationID = report.notification_id.as_str(),
                "Subagent report notification ID conflicted in parent inbox"
            );
            return Ok(());
        }
        Err(e) => {
            // The parent may be deleted right after the read above.
            if app.sessions.get(parent_session_id).is_err() {
                app.jobs.complete_background(&report.notification_id);
                return Ok(());
            }
            return Err(e.into());
        }
    }
    if let Ok(parent) = app.sessions.get(parent_session_id) {
        if parent.revert.is_none() {
            if let Some(c) = app.coordinator.get() {
                c.wake(parent_session_id, crate::inbox::Scope::Input);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(c: &Connection, session: &str, id: &str, seq: i64, text: &str, completed: bool, error: bool) {
        let mut data = serde_json::json!({
            "time": {"created": seq},
            "agent": "build",
            "model": {"id": "m", "providerID": "p"},
            "content": [{"type": "text", "text": text}],
        });
        if completed {
            data["time"]["completed"] = serde_json::json!(seq + 1);
        }
        if error {
            data["error"] = serde_json::json!({"type": "unknown", "message": "failed"});
        }
        c.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, 'assistant', ?, 0, 0, ?)",
            rusqlite::params![id, session, seq, data.to_string()],
        )
        .unwrap();
    }

    fn seed(db: &oc_storage::Database, session: &str) {
        db.with(|c| {
            // Only the session row matters here, not its project.
            c.execute_batch("PRAGMA foreign_keys = OFF")?;
            c.execute(
                "INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated) VALUES (?, 'global', 's', '/', '2.3.0', 0, 0)",
                [session],
            )
        })
        .unwrap();
    }

    fn text_of(message: Option<Message>) -> Option<(String, String)> {
        match message? {
            Message::Assistant(a) => Some((a.id.clone(), crate::job::subagent_text(Some(&Message::Assistant(a))))),
            _ => None,
        }
    }

    /// session-store.test.ts "lastAssistantAfter pages past more than twenty incomplete or errored assistant
    /// messages" (2.3.0).
    #[test]
    fn last_assistant_after_pages() {
        let db = oc_storage::Database::memory().unwrap();
        seed(&db, "ses_last_assistant_pages");
        db.with(|c| {
            assistant(c, "ses_last_assistant_pages", "msg_target", 5, "older answer", true, false);
            for i in 0..23 {
                assistant(c, "ses_last_assistant_pages", &format!("msg_noise_{i}"), 10 + i, &format!("noise {i}"), i % 2 == 1, i % 2 == 1);
            }
            Ok(())
        })
        .unwrap();
        let found = db.with(|c| Ok(last_assistant_after(c, "ses_last_assistant_pages", Some(0)).unwrap())).unwrap();
        assert_eq!(text_of(found), Some(("msg_target".into(), "older answer".into())));
    }

    /// "lastAssistantAfter returns undefined without a qualifying assistant after the boundary"
    #[test]
    fn last_assistant_after_boundary() {
        let db = oc_storage::Database::memory().unwrap();
        seed(&db, "ses_last_assistant_missing");
        db.with(|c| {
            assistant(c, "ses_last_assistant_missing", "msg_settled", 2, "before the boundary", true, false);
            assistant(c, "ses_last_assistant_missing", "msg_pending", 8, "still streaming", false, false);
            assistant(c, "ses_last_assistant_missing", "msg_failed", 9, "failed answer", true, true);
            Ok(())
        })
        .unwrap();
        for after in [5, 9] {
            assert!(db.with(|c| Ok(last_assistant_after(c, "ses_last_assistant_missing", Some(after)).unwrap())).unwrap().is_none());
        }
        let all = db.with(|c| Ok(last_assistant_after(c, "ses_last_assistant_missing", None).unwrap())).unwrap();
        assert_eq!(text_of(all).unwrap().0, "msg_settled");
    }
}
