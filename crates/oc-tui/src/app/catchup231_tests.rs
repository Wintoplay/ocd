//! The 2.3.1 catch-up of the terminal UI: a permission prompt wins over the composer, also in a subagent session,
//! which now lists its own permission requests (TUI half of #64343). The footer without the time to first token
//! (#63759) is `tests::footer_leaves_out_time_to_first_token`.

use super::tests::{app, render, session};
use super::*;
use crate::keymap::Stroke;
use serde_json::json;

const CHILD: &str = "ses_child";
const SIBLING: &str = "ses_sibling";

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

fn has(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

fn ask(a: &mut App, sid: &str, id: &str, command: &str) {
    a.store.permissions.insert(
        sid.into(),
        vec![PermissionRequest {
            id: id.into(),
            session_id: sid.into(),
            action: "bash".into(),
            resources: vec![command.into()],
            metadata: Some(json!({ "command": command })),
            ..Default::default()
        }],
    );
}

/// A root session with two subagent sessions.
fn family(a: &mut App) -> String {
    let sid = session(a);
    for (id, title) in [(CHILD, "find config loader"), (SIBLING, "read the docs")] {
        a.store.remember(SessionInfo {
            id: id.into(),
            parent_id: Some(sid.clone()),
            agent: Some("explore".into()),
            title: Some(title.into()),
            ..Default::default()
        });
        a.store.active.insert(id.into(), true);
    }
    sid
}

#[test]
fn a_subagent_session_shows_its_permission_prompt_instead_of_the_composer() {
    let mut a = app();
    family(&mut a);
    a.route = Route::Session(CHILD.into());
    // Without a request the subagent session shows its Subagents composer.
    assert!(a.composer_open());
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "Subagents  Shell"), "{}", joined(&lines));
    assert!(!has(&lines, "Permission required"), "{}", joined(&lines));

    ask(&mut a, CHILD, "per_child", "rm -rf build");
    assert_eq!(a.active_permission().map(|p| p.id), Some("per_child".into()));
    assert!(!a.composer_open());
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "△ Permission required"), "{}", joined(&lines));
    assert!(has(&lines, "$ rm -rf build"), "{}", joined(&lines));
    assert!(!has(&lines, "Subagents  Shell"), "{}", joined(&lines));
    // Keys reach the prompt, not the composer: right moves to "Always allow" and escape does not leave the session.
    a.on_key(Stroke::key("right"));
    assert_eq!(a.permission_ui.as_ref().map(|u| u.id.as_str()), Some("per_child"));
    assert_eq!(a.route, Route::Session(CHILD.into()));

    // Answered: the composer is back.
    a.store.permissions.remove(CHILD);
    a.permission_ui = None;
    assert!(a.composer_open());
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "Subagents  Shell"), "{}", joined(&lines));
}

#[test]
fn a_subagent_session_only_lists_its_own_requests() {
    let mut a = app();
    let root = family(&mut a);
    ask(&mut a, SIBLING, "per_sibling", "rm -rf sibling");
    ask(&mut a, &root, "per_root", "rm -rf root");
    a.route = Route::Session(CHILD.into());
    assert!(a.active_permission().is_none());
    assert!(a.composer_open());
    let lines = render(&mut a, 100, 50);
    assert!(!has(&lines, "Permission required"), "{}", joined(&lines));
    assert!(has(&lines, "Subagents  Shell"), "{}", joined(&lines));

    // The root still gathers its descendants' requests, its own first.
    a.route = Route::Session(root.clone());
    assert_eq!(a.active_permission().map(|p| p.id), Some("per_root".into()));
    a.store.permissions.remove(&root);
    assert_eq!(a.active_permission().map(|p| p.id), Some("per_sibling".into()));

    // Auto-accept prompts nothing anywhere.
    a.route = Route::Session(SIBLING.into());
    assert_eq!(a.active_permission().map(|p| p.id), Some("per_sibling".into()));
    a.autoaccept = true;
    assert!(a.active_permission().is_none());
    assert!(a.composer_open());
}

#[test]
fn an_open_composer_gives_way_to_an_arriving_permission_prompt() {
    let mut a = app();
    let root = family(&mut a);
    a.on_key(Stroke::key("down"));
    assert!(a.composer.is_some());
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "Subagents  Shell"), "{}", joined(&lines));

    // A subagent asks: the root shows its prompt over the composer the user opened.
    ask(&mut a, CHILD, "per_child", "rm -rf build");
    assert!(!a.composer_open());
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "△ Permission required"), "{}", joined(&lines));
    assert!(!has(&lines, "Subagents  Shell"), "{}", joined(&lines));
    // Keys reach the prompt; the composer the user opened stays remembered.
    a.on_key(Stroke::key("right"));
    assert!(a.permission_ui.is_some());
    assert!(a.composer.is_some());

    // Once the request is gone the composer the user opened comes back.
    a.store.permissions.remove(CHILD);
    a.permission_ui = None;
    assert!(a.composer_open());
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "Subagents  Shell"), "{}", joined(&lines));
    assert_eq!(a.route, Route::Session(root));
}

// permission.tsx `source`: the prompt reads the command from the input of the tool call that asked, as the server's
// shell asks carry no command in their metadata (the subagent session capture against 2.3.1 showed `$ echo built`).
#[test]
fn a_shell_ask_shows_the_command_of_its_source_call() {
    let mut a = app();
    family(&mut a);
    a.route = Route::Session(CHILD.into());
    let msgs: Vec<Message> = serde_json::from_value(json!([
        {"id": "msg_child_user", "type": "user", "text": "run the build", "time": {"created": 1000}},
        {"id": "msg_child_tools", "type": "assistant", "agent": "general", "time": {"created": 2000},
         "content": [{"type": "tool", "id": "call_sh", "name": "shell", "time": {"created": 2000},
                      "state": {"status": "running", "input": {"command": "echo built", "description": "Build it"}}}]}
    ]))
    .unwrap();
    a.store.messages.insert(CHILD.into(), msgs);
    a.store.permissions.insert(
        CHILD.into(),
        vec![PermissionRequest {
            id: "per_sh".into(),
            session_id: CHILD.into(),
            action: "shell".into(),
            resources: vec!["echo built".into()],
            source: Some(json!({"type": "tool", "messageID": "msg_child_tools", "id": "call_sh"})),
            ..Default::default()
        }],
    );
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "△ Permission required"), "{}", joined(&lines));
    assert!(has(&lines, "$ echo built"), "{}", joined(&lines));
    assert!(!has(&lines, "- echo built"), "{}", joined(&lines));
    // Without a source call the resources are listed.
    a.store.permissions.get_mut(CHILD).unwrap()[0].source = None;
    let lines = render(&mut a, 100, 50);
    assert!(has(&lines, "- echo built"), "{}", joined(&lines));
}
