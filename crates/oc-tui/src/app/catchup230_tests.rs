//! The 2.3.0 catch-up of the terminal UI: transcript verbosity low by default (#63207), the footer's live work
//! count and the composer tabs as click targets (#60168), the prompt history keeping the live draft (#63262),
//! undo bringing back a paste (#63265), copying a prompt selection by position (#63275) and session links
//! (#63224, #63247) in assistant text, compaction summaries and the `/btw` answer.

use super::catchup227_tests::{Handler, drive_until, flow};
use super::tests::{app, render, session};
use super::*;
use crate::keymap::Stroke;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::sync::Arc;

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

fn mouse(a: &mut App, kind: MouseEventKind, x: u16, y: u16, modifiers: KeyModifiers) {
    a.on_mouse(MouseEvent { kind, column: x, row: y, modifiers });
}

/// A plain click at (x, y).
fn click(a: &mut App, x: u16, y: u16) {
    mouse(a, MouseEventKind::Down(MouseButton::Left), x, y, KeyModifiers::NONE);
    mouse(a, MouseEventKind::Up(MouseButton::Left), x, y, KeyModifiers::NONE);
}

/// Screen cell of the first occurrence of `needle` (column in cells, not bytes).
fn find(lines: &[String], needle: &str) -> Option<(u16, u16)> {
    lines.iter().enumerate().find_map(|(y, l)| l.find(needle).map(|b| (crate::util::locale::width(&l[..b]) as u16, y as u16)))
}

fn at(lines: &[String], needle: &str) -> (u16, u16) {
    find(lines, needle).unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", joined(lines)))
}

// ---- #63207: transcript verbosity defaults to low ------------------------------------------------------------------

#[test]
fn verbosity_defaults_to_low() {
    let mut a = app();
    assert_eq!(a.config.verbosity(), "low");
    session(&mut a);
    let lines = render(&mut a, 80, 60);
    // Tools and thoughts fold into one activity summary; the answer stays.
    assert!(lines.iter().any(|l| l.contains("+ 1 command, 1 edit, 1 thought, 1 read, 2 tools")), "{}", joined(&lines));
    assert!(lines.iter().any(|l| l.contains("I fixed the bug.")), "{}", joined(&lines));
    assert!(!lines.iter().any(|l| l.contains("$ bun test")), "{}", joined(&lines));
    // The settings dialog lists low as the default (`config-v2.test.tsx`).
    let entry = super::commands::settings().into_iter().find(|s| s.2 == "session.verbosity").unwrap();
    assert_eq!(entry.5, json!("low"));
}

// ---- #60168: the footer's `N subagents · M shells` and the composer tabs are clickable ----------------------------

const CHILD: &str = "ses_child";

fn live_work(a: &mut App, subagent: bool) {
    let sid = a.session_id().unwrap().to_string();
    a.store.remember(SessionInfo { id: CHILD.into(), parent_id: Some(sid.clone()), title: Some("First".into()), ..Default::default() });
    a.store.active.insert(CHILD.into(), subagent);
    a.store.location_mut("/work/proj").shells =
        vec![crate::api::types::ShellInfo { id: "sh_1".into(), status: "running".into(), command: "bun test".into(), metadata: Some(json!({ "sessionID": sid })) }];
}

/// `prompt-footer.test.tsx`: any cell of the live count opens the composer on the subagents tab, or on the shell tab
/// when only shells run; a click opens, it never closes.
#[test]
fn the_footer_live_count_opens_the_right_composer_tab() {
    let mut a = app();
    session(&mut a);
    live_work(&mut a, true);
    let lines = render(&mut a, 100, 40);
    let (x, y) = at(&lines, "1 subagent · 1 shell");
    assert!(lines[y as usize].contains("↓ 1 subagent · 1 shell · "), "{}", joined(&lines));
    let key = x - 2;
    for cx in [key, x + 2, x + 16] {
        a.composer = None;
        click(&mut a, cx, y);
        assert_eq!(a.composer.as_ref().map(|c| c.tab.as_str()), Some("subagents"), "column {cx}");
    }
    // A second click keeps it open.
    a.composer = None;
    let lines = render(&mut a, 100, 40);
    let (x, y) = at(&lines, "1 subagent · 1 shell");
    click(&mut a, x, y);
    assert!(a.composer.is_some());
    // Outside the count nothing happens.
    a.composer = None;
    click(&mut a, x + 30, y);
    assert!(a.composer.is_none());

    a.store.active.insert(CHILD.into(), false);
    let lines = render(&mut a, 100, 40);
    let (x, y) = at(&lines, "↓ 1 shell · ");
    for cx in [x, x + 3] {
        a.composer = None;
        click(&mut a, cx, y);
        assert_eq!(a.composer.as_ref().map(|c| c.tab.as_str()), Some("shell"), "column {cx}");
    }
}

/// `composer-keymap.test.tsx` "composer tabs switch by mouse and keyboard".
#[test]
fn composer_tabs_switch_by_mouse_and_keyboard() {
    let mut a = app();
    session(&mut a);
    live_work(&mut a, true);
    a.run_command("session.child.first", Some("subagents".into()));
    let lines = render(&mut a, 100, 40);
    let row = lines.iter().position(|l| l.contains("Subagents") && l.contains("Shell")).unwrap_or_else(|| panic!("{}", joined(&lines)));
    let col = |label: &str| crate::util::locale::width(&lines[row][..lines[row].find(label).unwrap()]) as u16;
    let (shell, subagents) = (col("Shell"), col("Subagents"));
    click(&mut a, shell, row as u16);
    assert_eq!(a.composer.as_ref().unwrap().tab, "shell");
    assert!(joined(&render(&mut a, 100, 40)).contains("bun test"));
    click(&mut a, subagents, row as u16);
    assert_eq!(a.composer.as_ref().unwrap().tab, "subagents");
    assert!(!joined(&render(&mut a, 100, 40)).contains("bun test"));
    a.on_key(Stroke::key("right"));
    assert_eq!(a.composer.as_ref().unwrap().tab, "shell");
    // `session.child.first` with a tab switches tabs too, and without one it still toggles.
    a.run_command("session.child.first", Some("subagents".into()));
    assert_eq!(a.composer.as_ref().unwrap().tab, "subagents");
    a.run_command("session.child.first", None);
    assert!(a.composer.is_none());
}

// ---- #63262: prompt history keeps the live draft ---------------------------------------------------------------

fn with_history(entries: &[&str]) -> App {
    let mut a = app();
    for t in entries {
        a.history.append(crate::state::PromptInfo { text: (*t).into(), mode: Some("normal".into()), ..Default::default() });
    }
    a
}

fn typed(a: &mut App, text: &str) {
    for c in text.chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
}

/// `history-composer.test.tsx` "down past the newest entry restores the draft text and shell mode".
#[test]
fn down_past_the_newest_entry_restores_the_draft_and_shell_mode() {
    // Matching the oldest entry let Up recall "previous" even before 2.3.0.
    let mut a = with_history(&["echo pen", "previous"]);
    typed(&mut a, "!echo pen");
    assert!(a.editor.shell && a.editor.text == "echo pen");
    a.on_key(Stroke::key("home"));
    a.on_key(Stroke::key("up"));
    assert_eq!(a.editor.text, "previous");
    assert!(!a.editor.shell);
    a.on_key(Stroke::key("end"));
    a.on_key(Stroke::key("down"));
    assert_eq!(a.editor.text, "echo pen");
    assert!(a.editor.shell);
    assert!(joined(&render(&mut a, 100, 30)).contains("Shell"));
}

/// "clearing a recalled entry drops the draft so down stays empty".
#[test]
fn clearing_a_recalled_entry_drops_the_draft() {
    let mut a = with_history(&["echo pen", "previous"]);
    typed(&mut a, "unsent");
    a.on_key(Stroke::key("home"));
    a.on_key(Stroke::key("up"));
    assert_eq!(a.editor.text, "previous");
    a.on_key(Stroke::ctrl("c"));
    assert_eq!(a.editor.text, "");
    a.on_key(Stroke::key("down"));
    assert_eq!(a.editor.text, "");
}

/// Up from a typed prompt works whatever it holds, and a stash pop leaves the history.
#[test]
fn up_always_leaves_a_typed_prompt_and_set_resets() {
    let mut a = with_history(&["one", "two"]);
    typed(&mut a, "draft");
    a.on_key(Stroke::key("home"));
    a.on_key(Stroke::key("up"));
    assert_eq!(a.editor.text, "two");
    a.stash.push(crate::state::PromptInfo { text: "stashed".into(), ..crate::state::PromptInfo::empty() });
    a.run_command("prompt.stash.pop", None);
    assert_eq!(a.editor.text, "stashed");
    assert_eq!((a.history.index, a.history.draft.is_none()), (0, true));
}

// ---- #63265: undo brings back a paste ----------------------------------------------------------------------------

/// `prompt-paste.test.tsx` "undo submits the restored whole paste chip". (Its "fragment" case needs a chip cut in
/// half by Ctrl+W; ocd's editor deletes a chip whole, so undo brings the whole chip back there too.)
#[test]
fn undo_restores_a_paste_chip() {
    let pasted = "one\ntwo\nthree\nfour";
    let mut undo = Stroke::key("z");
    undo.super_ = true;
    let mut a = app();
    a.paste(pasted);
    assert_eq!(a.editor.text, "[Pasted ~4 lines] ");
    a.on_key(Stroke::ctrl("u"));
    assert_eq!(a.editor.text, "");
    a.on_key(undo.clone());
    assert_eq!(a.editor.text, "[Pasted ~4 lines] ");
    assert_eq!(a.editor.input_text(), format!("{pasted} "));

    let mut a = app();
    a.paste(pasted);
    a.on_key(Stroke::key("backspace")); // the space after the chip
    a.on_key(Stroke::ctrl("w"));
    assert_eq!(a.editor.text, "");
    a.on_key(undo);
    assert_eq!(a.editor.text, "[Pasted ~4 lines]");
    assert_eq!(a.editor.input_text(), pasted);
}

// ---- #63275: copying a prompt selection expands the pastes inside it by position ----------------------------------

#[test]
fn copying_a_prompt_selection_expands_pastes_by_position() {
    let mut a = app();
    a.paste("first\nfirst\nfirst");
    typed(&mut a, "and ");
    a.paste("second\nsecond\nsecond");
    assert_eq!(a.editor.text, "[Pasted ~3 lines] and [Pasted ~3 lines] ");
    let lines = render(&mut a, 100, 30);
    let (x, y) = at(&lines, "[Pasted ~3 lines] and");
    let copy = |a: &mut App, from: u16, to: u16| {
        a.selection = Some(super::selection::Selection { anchor: (from, y), focus: (to, y), dragged: true, active: false });
        crate::clipboard::TEST_CLIPBOARD.with(|c| *c.borrow_mut() = None);
        assert!(a.copy_selection());
        crate::clipboard::TEST_CLIPBOARD.with(|c| c.borrow().clone()).unwrap()
    };
    // Only the second chip: its own text, though both labels are the same.
    let second = x + 22;
    assert_eq!(copy(&mut a, second, second + 16), "second\nsecond\nsecond");
    // Both chips, in place.
    assert_eq!(copy(&mut a, x, second + 16), "first\nfirst\nfirst and second\nsecond\nsecond");
    // Part of a chip stays its label text.
    assert_eq!(copy(&mut a, x + 5, x + 21), "ed ~3 lines] and ");
    // A selection reaching past the prompt's text falls back to the labels.
    assert_eq!(copy(&mut a, x - 1, second + 16), " first\nfirst\nfirst and second\nsecond\nsecond");
}

// ---- #63224, #63247: session links ----------------------------------------------------------------------------

fn sid(name: &str) -> String {
    format!("ses_{}", name.repeat(26).chars().take(26).collect::<String>())
}

fn info(id: &str, title: &str) -> Value {
    json!({ "id": id, "projectID": "prj_1", "title": title, "location": { "directory": "/work/proj" }, "time": { "created": 1, "updated": 1 } })
}

fn remember(a: &mut App, id: &str, title: &str, archived: bool) {
    let mut s: SessionInfo = serde_json::from_value(info(id, title)).unwrap();
    if archived {
        s.time.archived = Some(2);
    }
    a.store.remember(s);
}

fn assistant(id: &str, text: &str) -> Value {
    json!({"id": id, "type": "assistant", "agent": "build", "model": {"id": "claude-sonnet", "providerID": "anthropic"},
           "time": {"created": 2000, "completed": 3000}, "finish": "stop", "content": [{"type": "text", "text": text}]})
}

/// The current session `current` showing `messages`.
fn transcript(a: &mut App, current: &str, messages: Vec<Value>) {
    remember(a, current, "Current", false);
    let mut all = vec![json!({"id": "msg_0001", "type": "user", "text": "Which sessions?", "time": {"created": 1000}})];
    all.extend(messages);
    a.store.messages.insert(current.into(), serde_json::from_value(Value::Array(all)).unwrap());
    a.store.loaded.insert(format!("session:{current}"));
    a.store.loaded.insert(format!("session.message:{current}"));
    a.store.loaded.insert(format!("session.pending:{current}"));
    a.route = Route::Session(current.into());
}

/// Texts of the linked runs on screen, with their targets.
fn links(a: &App, sid: &str, width: usize) -> Vec<(String, String)> {
    let (lines, _) = a.render_transcript_lines(sid, width);
    lines
        .iter()
        .flat_map(|l| l.segs.iter())
        .filter_map(|s| match s.link.as_deref() {
            Some(crate::ui::text::Click::SessionLink(id)) => Some((s.text.trim().to_string(), id.clone())),
            _ => None,
        })
        .collect()
}

/// The cache matches the reference render.
fn same_as_reference(a: &mut App, sid: &str, width: usize) {
    a.transcript_sync(sid, width);
    let (lines, _) = a.render_transcript_lines(sid, width);
    assert_eq!(a.tcache.to_vec(), lines);
}

fn session_routes(unknown: String) -> Handler {
    Arc::new(move |method: &str, path: &str| match (method, path.strip_prefix("/api/session/")) {
        ("GET", Some(id)) if id == unknown => (200, json!({ "data": info(&unknown, "Looked up") })),
        ("GET", Some(id)) => (404, json!({ "_tag": "SessionNotFoundError", "sessionID": id, "message": format!("Session not found: {id}") })),
        _ => (404, json!({ "_tag": "NotFound", "message": "no route" })),
    })
}

/// `session-link.test.tsx` "session links in … markdown": loaded sessions link as `@<title>`; the current,
/// archived, missing and still-loading ones stay IDs; each unknown ID is looked up once, a miss marks it missing;
/// a lookup or a rename re-renders the link; a click opens the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_ids_in_messages_link_to_their_sessions() {
    let (current, known, archived, unknown, missing) = (sid("current"), sid("known"), sid("archived"), sid("unknown"), sid("missing"));
    let (mut a, mut rx, log) = flow(session_routes(unknown.clone())).await;
    remember(&mut a, &known, "Fix the flaky reconnect test on Windows", false);
    remember(&mut a, &archived, "Old work", true);
    let text = format!("**中文** see {known} now\n\n- current {current}\n- archived {archived}\n- unknown {unknown}\n- missing {missing}\n\n```\n{known}\n```");
    transcript(&mut a, &current, vec![assistant("msg_0002", &text)]);
    let lines = render(&mut a, 100, 40);
    let screen = joined(&lines);
    assert!(screen.contains("see @Fix the flaky reconnect test on Windows now"), "{screen}");
    for plain in [&current, &archived, &unknown, &missing, &known] {
        assert!(screen.contains(plain.as_str()), "{plain}: {screen}");
    }
    assert_eq!(links(&a, &current, 96), vec![("@Fix the flaky reconnect test on Windows".into(), known.clone())]);
    let lookups = |log: &super::catchup227_tests::Log| {
        let mut v: Vec<String> = log.calls().into_iter().filter(|c| c.starts_with("GET /api/session/")).collect();
        v.sort();
        v
    };
    drive_until(&mut a, &mut rx, 3000, |a| a.store.is_missing(&missing) && a.store.session(&unknown).is_some()).await;
    assert_eq!(lookups(&log), vec![format!("GET /api/session/{missing}"), format!("GET /api/session/{unknown}")]);
    render(&mut a, 100, 40);
    assert_eq!(links(&a, &current, 96), vec![("@Fix the flaky reconnect test on Windows".into(), known.clone()), ("@Looked up".into(), unknown.clone())]);
    same_as_reference(&mut a, &current, 96);
    // A rename follows; lookups are cached, misses included.
    remember(&mut a, &known, "Renamed", false);
    let lines = render(&mut a, 100, 40);
    assert!(joined(&lines).contains("see @Renamed now"), "{}", joined(&lines));
    same_as_reference(&mut a, &current, 96);
    drive_until(&mut a, &mut rx, 200, |_| false).await;
    assert_eq!(lookups(&log).len(), 2);
    // Source mode keeps the ID text, still a link.
    a.config.set("session.markdown", Some(json!("source")));
    assert_eq!(links(&a, &current, 96)[0], (known.clone(), known.clone()));
    a.config.set("session.markdown", Some(json!("rendered")));
    // A modified click is the terminal's; a plain one opens the session.
    let lines = render(&mut a, 100, 40);
    let (x, y) = at(&lines, "@Looked up");
    mouse(&mut a, MouseEventKind::Down(MouseButton::Left), x + 4, y, KeyModifiers::CONTROL);
    mouse(&mut a, MouseEventKind::Up(MouseButton::Left), x + 4, y, KeyModifiers::CONTROL);
    assert_eq!(a.session_id(), Some(current.as_str()));
    click(&mut a, x + 4, y);
    assert_eq!(a.session_id(), Some(unknown.as_str()));
}

/// "a wrapped title opens its session from either row, and plain text does not".
#[test]
fn a_wrapped_link_opens_from_either_row() {
    let mut a = app();
    let (current, known) = (sid("current"), sid("known"));
    remember(&mut a, &known, "Fix the flaky reconnect test on Windows", false);
    transcript(&mut a, &current, vec![assistant("msg_0002", &format!("**中文** see {known} now"))]);
    let lines = render(&mut a, 34, 30);
    let (hx, hy) = at(&lines, "@Fix");
    let (tx, ty) = at(&lines, "Windows");
    assert!(ty > hy, "{}", joined(&lines));
    let (sx, sy) = at(&lines, "see");
    click(&mut a, sx, sy);
    let (nx, ny) = at(&lines, "now");
    click(&mut a, nx, ny);
    // A drag over the link selects text instead.
    mouse(&mut a, MouseEventKind::Down(MouseButton::Left), hx, hy, KeyModifiers::NONE);
    mouse(&mut a, MouseEventKind::Drag(MouseButton::Left), hx + 3, hy, KeyModifiers::NONE);
    mouse(&mut a, MouseEventKind::Up(MouseButton::Left), hx + 3, hy, KeyModifiers::NONE);
    assert_eq!(a.session_id(), Some(current.as_str()));
    a.selection = None;
    click(&mut a, tx + 2, ty);
    assert_eq!(a.session_id(), Some(known.as_str()));
}

/// "code spans link only when they are exactly an ID".
#[test]
fn code_spans_link_only_when_exactly_an_id() {
    for conceal in [true, false] {
        let mut a = app();
        let (current, known) = (sid("current"), sid("known"));
        remember(&mut a, &known, "Fix the flaky reconnect test on Windows", false);
        a.config.set("session.markdown", Some(json!(if conceal { "rendered" } else { "source" })));
        transcript(&mut a, &current, vec![assistant("msg_0002", &format!("Run `opencode --session {known}` or open `{known}`."))]);
        let found: Vec<String> = links(&a, &current, 200).into_iter().map(|(t, _)| t).collect();
        assert_eq!(found, [if conceal { "@Fix the flaky reconnect test on Windows".to_string() } else { known.clone() }]);
        let (lines, _) = a.render_transcript_lines(&current, 200);
        assert!(lines.iter().any(|l| l.text().contains(&format!("opencode --session {known}"))));
    }
}

/// "a label change re-highlights only the blocks that mention that session": in ocd, only the transcript rows
/// that mention it render again, and the cache still matches the reference render.
#[test]
fn a_label_change_renders_only_the_rows_that_mention_it() {
    let mut a = app();
    let (current, known, other) = (sid("current"), sid("known"), sid("other"));
    remember(&mut a, &known, "Fix the flaky reconnect test on Windows", false);
    remember(&mut a, &other, "Other work", false);
    let filler: Vec<Value> = (0..20).map(|i| assistant(&format!("msg_1{i:03}"), &format!("Filler answer {i}"))).collect();
    let mut messages = vec![assistant("msg_0002", &format!("First {known}")), assistant("msg_0003", &format!("Second {other}"))];
    messages.extend(filler);
    transcript(&mut a, &current, messages);
    same_as_reference(&mut a, &current, 96);
    let rendered = a.tcache.stats.rendered;
    // Nothing changed: nothing renders.
    a.transcript_sync(&current, 96);
    assert_eq!(a.tcache.stats.rendered, rendered);
    remember(&mut a, &known, "Renamed", false);
    a.transcript_sync(&current, 96);
    assert_eq!(a.tcache.stats.rendered - rendered, 1, "only the row mentioning the renamed session");
    assert!(a.tcache.to_vec().iter().any(|l| l.text().contains("First @Renamed")));
    same_as_reference(&mut a, &current, 96);
    // Archiving unlinks it; the current session never links.
    let mut s = a.store.session(&other).cloned().unwrap();
    s.time.archived = Some(5);
    a.store.remember(s);
    same_as_reference(&mut a, &current, 96);
    assert_eq!(links(&a, &current, 96), vec![("@Renamed".into(), known.clone())]);
}

/// Compaction summaries link too; thinking does not.
#[test]
fn compaction_summaries_link_and_thoughts_do_not() {
    let mut a = app();
    let (current, known) = (sid("current"), sid("known"));
    remember(&mut a, &known, "Earlier work", false);
    a.config.set("session.thinking", Some(json!("show")));
    a.config.set("session.verbosity", Some(json!("medium")));
    transcript(
        &mut a,
        &current,
        vec![
            json!({"id": "msg_0002", "type": "compaction", "status": "completed", "summary": format!("Continued from {known}."), "time": {"created": 1500}}),
            json!({"id": "msg_0003", "type": "assistant", "agent": "build", "model": {"id": "claude-sonnet", "providerID": "anthropic"},
                   "time": {"created": 2000, "completed": 3000}, "finish": "stop",
                   "content": [{"type": "reasoning", "text": format!("Thinking about {known}"), "time": {"created": 2000, "completed": 2500}}, {"type": "text", "text": "Done."}]}),
        ],
    );
    assert_eq!(links(&a, &current, 96), vec![("@Earlier work".into(), known.clone())]);
    let lines = render(&mut a, 100, 40);
    assert!(joined(&lines).contains("Continued from @Earlier work."), "{}", joined(&lines));
    assert!(joined(&lines).contains(&format!("Thinking about {known}")), "{}", joined(&lines));
    same_as_reference(&mut a, &current, 96);
}

/// "clicking a session link in a dialog closes it and opens the session": the `/btw` answer.
#[test]
fn a_link_in_the_btw_answer_closes_the_dialog_and_opens_the_session() {
    let mut a = app();
    let (current, known) = (sid("current"), sid("known"));
    remember(&mut a, &known, "Fix the flaky reconnect test on Windows", false);
    transcript(&mut a, &current, vec![]);
    let d = super::commands::btw_answer(&a, "which?", &format!("See {known}"), false);
    a.open_dialog(d);
    let lines = render(&mut a, 100, 30);
    let (x, y) = at(&lines, "@Fix");
    // The answer follows a rename.
    remember(&mut a, &known, "Renamed", false);
    let lines = render(&mut a, 100, 30);
    assert!(joined(&lines).contains("See @Renamed"), "{}", joined(&lines));
    assert_eq!(find(&lines, "@Renamed"), Some((x, y)));
    click(&mut a, x + 2, y);
    assert!(a.dialogs.is_empty());
    assert_eq!(a.session_id(), Some(known.as_str()));
}
