//! Generic output bounding (`core/src/tool-output.ts`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use oc_schema::ToolContent;
use serde_json::Value;

use crate::tool::ToolResult;

pub const MAX_LINES: usize = 2_000;
pub const MAX_BYTES: usize = 50 * 1024;
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const DIRECTORY: &str = "tool-output";

pub fn file_name() -> String {
    format!("tool_{}", oc_schema::id::create(false, None))
}

pub fn directory(data: &Path) -> PathBuf {
    data.join(DIRECTORY)
}

/// `ToolOutput.save`: writes bytes the model cannot receive inline to a retained file and returns its path.
pub fn save(data: &Path, bytes: &[u8], extension: &str) -> std::io::Result<PathBuf> {
    let dir = directory(data);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}{extension}", file_name()));
    std::fs::write(&file, bytes)?;
    Ok(file)
}

/// `ToolOutput.truncate`. Applied by the session runner after every local tool execution. When the full output
/// can't be saved, the preview is kept and the marker says why; `outputPath` is then left out.
pub fn truncate(result: ToolResult, max_lines: usize, max_bytes: usize, data: &Path) -> ToolResult {
    if result.metadata.as_ref().is_some_and(|m| m.contains_key("truncated")) {
        return result;
    }
    // A line break joins text parts unless the text before already ends in one.
    let mut text = String::new();
    for item in &result.content {
        let ToolContent::Text { text: part } = item else { continue };
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(part);
    }
    let total_lines = count_lines(&text);
    if total_lines <= max_lines && text.len() <= max_bytes {
        let mut metadata = result.metadata.clone().unwrap_or_default();
        metadata.insert("truncated".into(), Value::Bool(false));
        return ToolResult { metadata: Some(metadata), ..result };
    }

    // Cut once, at the line or byte limit, whichever comes first, so a line too long for the space left shows its
    // start. The preview never ends on a line break.
    let end = after_lines(&text, max_lines).min(byte_boundary(&text, max_bytes));
    let shown_end = if text[..end].ends_with("\r\n") {
        end - 2
    } else if text[..end].ends_with('\n') {
        end - 1
    } else {
        end
    };
    let dir = directory(data);
    let file = dir.join(file_name());
    let failure = match std::fs::create_dir_all(&dir) {
        Err(error) => Some(node_error(&error, "mkdir", &dir)),
        Ok(()) => std::fs::write(&file, &text).err().map(|error| node_error(&error, "open", &file)),
    };
    if let Some(failure) = &failure {
        tracing::warn!(file = %file.display(), cause = %failure, "failed to save full tool output");
    }
    // Counted before the final line break is stripped, so a blank last line still counts.
    let shown_lines = count_lines(&text[..end]);
    let rest = &text[shown_end..];
    let last_line_cut = shown_lines > 0 && shown_end < text.len() && !rest.starts_with('\n') && !rest.starts_with("\r\n");
    let range = if shown_lines == 0 { "0 lines".to_string() } else { format!("lines 1-{shown_lines}") };
    let saved = match &failure {
        None => format!("full output saved to {}", file.to_string_lossy()),
        Some(failure) => format!("full output could not be saved: {failure}"),
    };
    let marker =
        format!("[showing {range} of {total_lines}{}; {saved}]", if last_line_cut { " (last line cut)" } else { "" });

    let mut bounded: Vec<ToolContent> = Vec::new();
    let mut remaining = shown_end;
    let mut separator = false;
    let mut marked = false;
    for item in &result.content {
        let ToolContent::Text { text } = item else {
            bounded.push(item.clone());
            continue;
        };
        if separator && remaining > 0 {
            remaining -= 1;
        }
        separator = !text.is_empty() && !text.ends_with('\n');
        if !marked && remaining >= text.len() {
            bounded.push(item.clone());
            remaining -= text.len();
            continue;
        }
        if remaining > 0 {
            bounded.push(ToolContent::Text { text: text[..remaining].to_string() });
        }
        if !marked {
            bounded.push(ToolContent::Text { text: marker.clone() });
        }
        remaining = 0;
        marked = true;
    }
    if !marked {
        bounded.push(ToolContent::Text { text: marker });
    }
    let mut metadata = result.metadata.clone().unwrap_or_default();
    metadata.insert("truncated".into(), Value::Bool(true));
    if failure.is_none() {
        metadata.insert("outputPath".into(), Value::String(file.to_string_lossy().into_owned()));
    }
    ToolResult { output: result.output, content: bounded, metadata: Some(metadata) }
}

/// How many lines `text` has, not counting an empty one after a final line break.
fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let breaks = text.matches('\n').count();
    1 + breaks - usize::from(text.ends_with('\n'))
}

/// Where the line after the first `count` lines starts, or the end of `text`.
fn after_lines(text: &str, count: usize) -> usize {
    let mut index = 0;
    for _ in 0..count {
        match text[index..].find('\n') {
            None => return text.len(),
            Some(next) => index += next + 1,
        }
    }
    index
}

/// The longest start of `text` that fits in `budget` UTF-8 bytes without splitting a character, emoji included.
fn byte_boundary(text: &str, budget: usize) -> usize {
    if budget >= text.len() {
        return text.len();
    }
    let mut index = budget;
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    if index == 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    let (before, after) = (bytes[index - 1], bytes[index]);
    // Two ASCII characters always have a boundary between them, except the two halves of \r\n.
    if before < 0x80 && after < 0x80 {
        return if before == b'\r' && after == b'\n' { index - 1 } else { index };
    }
    // Otherwise back off to the start of the grapheme the cut is in. A line break always ends one, so segmenting from
    // the start of the line keeps flag pairs and long emoji sequences exact; one code point past the cut gives the
    // context.
    use unicode_segmentation::UnicodeSegmentation;
    let start = text[..index].rfind('\n').map_or(0, |i| i + 1);
    let next = text[index..].chars().next().map_or(0, char::len_utf8);
    let mut cluster = index;
    for (offset, _) in text[start..index + next].grapheme_indices(true) {
        if start + offset > index {
            break;
        }
        cluster = start + offset;
    }
    cluster
}

/// Node's text for a failed `fs` call: `<CODE>: <description>, <syscall> '<path>'`.
fn node_error(error: &std::io::Error, syscall: &str, path: &Path) -> String {
    #[cfg(unix)]
    let known = match error.raw_os_error() {
        Some(libc::EEXIST) => Some(("EEXIST", "file already exists")),
        Some(libc::ENOTDIR) => Some(("ENOTDIR", "not a directory")),
        Some(libc::ENOENT) => Some(("ENOENT", "no such file or directory")),
        Some(libc::EACCES) => Some(("EACCES", "permission denied")),
        Some(libc::EPERM) => Some(("EPERM", "operation not permitted")),
        Some(libc::ENOSPC) => Some(("ENOSPC", "no space left on device")),
        Some(libc::EDQUOT) => Some(("EDQUOT", "disk quota exceeded")),
        Some(libc::EROFS) => Some(("EROFS", "read-only file system")),
        Some(libc::EISDIR) => Some(("EISDIR", "illegal operation on a directory")),
        Some(libc::ENAMETOOLONG) => Some(("ENAMETOOLONG", "name too long")),
        Some(libc::EMFILE) => Some(("EMFILE", "too many open files")),
        _ => None,
    };
    #[cfg(not(unix))]
    let known: Option<(&str, &str)> = None;
    let (code, description) = known.unwrap_or(match error.kind() {
        std::io::ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        std::io::ErrorKind::PermissionDenied => ("EPERM", "operation not permitted"),
        std::io::ErrorKind::AlreadyExists => ("EEXIST", "file already exists"),
        _ => ("EIO", "i/o error"),
    });
    format!("{code}: {description}, {syscall} '{}'", path.display())
}

/// `FileRetention.cleanup` over files matching `pattern`.
pub fn retention_cleanup(files: &[PathBuf], retention: Duration) {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(retention) else { return };
    for file in files {
        let Ok(meta) = std::fs::metadata(file) else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime >= cutoff {
            continue;
        }
        let _ = std::fs::remove_file(file);
    }
}

/// Hourly cleanup body: delete `tool_[0-9a-f]{12}*` files older than 7 days.
pub fn cleanup(data: &Path) {
    let dir = directory(data);
    let re = regex::Regex::new(r"^tool_[0-9a-f]{12}").unwrap();
    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| re.is_match(&e.file_name().to_string_lossy()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    retention_cleanup(&files, RETENTION);
}

/// Spawn the hourly cleanup loop (runs immediately, then every hour).
pub fn spawn_cleanup(data: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let dir = data.clone();
            let _ = tokio::task::spawn_blocking(move || cleanup(&dir)).await;
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::meta;

    fn text(t: &str) -> ToolContent {
        ToolContent::Text { text: t.to_string() }
    }

    fn texts(parts: &[&str]) -> ToolResult {
        ToolResult { output: None, content: parts.iter().map(|t| text(t)).collect(), metadata: None }
    }

    fn marker(result: &ToolResult) -> String {
        match result.content.last() {
            Some(ToolContent::Text { text }) => text.clone(),
            other => panic!("no marker: {other:?}"),
        }
    }

    fn path_of(result: &ToolResult) -> String {
        result.metadata.as_ref().unwrap().get("outputPath").unwrap().as_str().unwrap().to_string()
    }

    #[test]
    fn writes_oversized_text_and_returns_a_bounded_preview() {
        let dir = tempfile::tempdir().unwrap();
        let output = serde_json::json!({ "items": [1, 2, 3] });
        let input = ToolResult { output: Some(output.clone()), ..ToolResult::text("one\ntwo\nthree") };
        let result = truncate(input, 2, 1_000, dir.path());
        assert_eq!(result.output, Some(output));
        let path = path_of(&result);
        assert_eq!(result.metadata.as_ref().unwrap().get("truncated"), Some(&Value::Bool(true)));
        assert!(Path::new(&path).file_name().unwrap().to_string_lossy().starts_with("tool_"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\nthree");
        assert_eq!(
            result.content,
            vec![text("one\ntwo"), text(&format!("[showing lines 1-2 of 3; full output saved to {path}]"))]
        );
    }

    #[test]
    fn keeps_the_preview_when_the_full_output_cannot_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DIRECTORY), "not a directory").unwrap();
        let result = truncate(ToolResult::text("one\ntwo\nthree"), 2, 1_000, dir.path());
        assert_eq!(result.metadata, Some(meta(vec![("truncated", Value::Bool(true))])));
        assert_eq!(result.content[0], text("one\ntwo"));
        let re = regex::Regex::new(r"^\[showing lines 1-2 of 3; full output could not be saved: E[A-Z]+: [^\n]+\]$").unwrap();
        assert!(re.is_match(&marker(&result)), "{}", marker(&result));
        assert_eq!(
            marker(&result),
            format!(
                "[showing lines 1-2 of 3; full output could not be saved: EEXIST: file already exists, mkdir '{}']",
                dir.path().join(DIRECTORY).display()
            )
        );
    }

    #[test]
    fn cuts_at_the_byte_limit_inside_a_line_when_it_falls_there() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("one\ntwo"), 100, 5, dir.path());
        assert_eq!(result.content[0], text("one\nt"));
        assert!(marker(&result).starts_with("[showing lines 1-2 of 2 (last line cut); full output saved to "));
    }

    #[test]
    fn shows_the_start_of_a_single_line_longer_than_the_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::json!({ "items": (0..50).collect::<Vec<_>>() }).to_string();
        let result = truncate(ToolResult::text(line.clone()), 100, 20, dir.path());
        assert_eq!(result.content.len(), 2);
        assert_eq!(result.content[0], text(&line[..20]));
        assert!(marker(&result).starts_with("[showing lines 1-1 of 1 (last line cut); full output saved to "));
    }

    #[test]
    fn never_splits_a_character_or_an_emoji_at_the_cut() {
        let dir = tempfile::tempdir().unwrap();
        let family = "👨‍👩‍👧‍👦";
        let cases = [
            ("ab✓✓✓".to_string(), "ab✓"),
            (format!("ab{family}cd"), "ab"),
            ("ab👍🏽cd".to_string(), "ab"),
            ("ab🇯🇵cd".to_string(), "ab"),
            ("ab𝒳𝒳".to_string(), "ab𝒳"),
        ];
        for (input, shown) in cases {
            let result = truncate(ToolResult::text(input), 100, 7, dir.path());
            assert_eq!(result.content[0], text(shown));
        }
    }

    #[test]
    fn keeps_flags_and_long_clusters_whole_however_long_the_text_before_them() {
        let dir = tempfile::tempdir().unwrap();
        let flags = truncate(ToolResult::text(format!("a{}", "🇯🇵".repeat(40))), 100, 165, dir.path());
        assert_eq!(flags.content[0], text(&format!("a{}", "🇯🇵".repeat(20))));
        let cluster = format!("👍{}", "🏽\u{0301}".repeat(60));
        let long = truncate(ToolResult::text(format!("ab{cluster}z")), 100, 165, dir.path());
        assert_eq!(long.content[0], text("ab"));
    }

    #[test]
    fn cuts_inside_a_later_text_part_and_leaves_out_the_parts_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let inside = truncate(texts(&["", "abc", "defgh", ""]), 100, 6, dir.path());
        assert_eq!(&inside.content[..3], &[text(""), text("abc"), text("de")]);
        assert_eq!(inside.content.len(), 4);
        assert!(marker(&inside).starts_with("[showing lines 1-2 of 2 (last line cut);"));
        let between = truncate(texts(&["abcde", "fgh"]), 100, 6, dir.path());
        assert_eq!(between.content[0], text("abcde"));
        assert_eq!(between.content.len(), 2);
        assert!(marker(&between).starts_with("[showing lines 1-1 of 2;"));
    }

    #[test]
    fn ends_a_line_cut_before_its_line_break() {
        let dir = tempfile::tempdir().unwrap();
        let crlf = truncate(ToolResult::text("one\r\ntwo\r\nthree"), 1, 1_000, dir.path());
        assert_eq!(crlf.content[0], text("one"));
        assert_eq!(crlf.content.len(), 2);
        assert!(marker(&crlf).starts_with("[showing lines 1-1 of 3;"));
        let blank = truncate(ToolResult::text("\nfoo\nbar"), 1, 1_000, dir.path());
        assert_eq!(blank.content.len(), 1);
        assert!(marker(&blank).starts_with("[showing lines 1-1 of 3;"));
    }

    #[test]
    fn counts_a_blank_last_line_of_the_preview() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("a\n\n\nb"), 2, 1_000, dir.path());
        assert_eq!(result.content[0], text("a\n"));
        assert!(marker(&result).starts_with("[showing lines 1-2 of 4;"));
    }

    #[test]
    fn shows_nothing_under_a_zero_line_limit() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("secret"), 0, 1_000, dir.path());
        assert_eq!(result.content.len(), 1);
        assert!(marker(&result).starts_with("[showing 0 lines of 1;"));
    }

    #[test]
    fn joins_text_parts_with_a_line_break_only_where_one_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(texts(&["one\n", "two", "three"]), 2, 1_000, dir.path());
        assert_eq!(std::fs::read_to_string(path_of(&result)).unwrap(), "one\ntwo\nthree");
        assert_eq!(&result.content[..2], &[text("one\n"), text("two")]);
        assert_eq!(result.content.len(), 3);
        assert!(marker(&result).starts_with("[showing lines 1-2 of 3;"));
    }

    #[test]
    fn preserves_mixed_content_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let file = ToolContent::File { uri: "file:///image.png".into(), mime: "image/png".into(), name: None };
        let input = ToolResult { output: None, content: vec![text("before"), file.clone(), text("after\nomitted")], metadata: None };
        let result = truncate(input, 2, 1_000, dir.path());
        assert_eq!(&result.content[..3], &[text("before"), file, text("after")]);
        assert!(marker(&result).starts_with("[showing lines 1-2 of 3; full output saved to "));
    }

    #[test]
    fn skips_results_that_report_a_truncation_state() {
        let dir = tempfile::tempdir().unwrap();
        for state in [true, false] {
            let input = ToolResult::text("one\ntwo")
                .with_metadata(meta(vec![("truncated", Value::Bool(state)), ("source", Value::String("tool".into()))]));
            assert_eq!(truncate(input.clone(), 2_000, 51_200, dir.path()), input);
        }
    }

    #[test]
    fn marks_results_that_fit_without_changing_their_content() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("small"), MAX_LINES, MAX_BYTES, dir.path());
        assert_eq!(result.metadata, Some(meta(vec![("truncated", Value::Bool(false))])));
        assert_eq!(result.content, vec![text("small")]);
    }

    #[test]
    fn does_not_count_a_trailing_newline_as_another_line() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("one\ntwo\n"), 2, 1_000, dir.path());
        assert_eq!(result.metadata, Some(meta(vec![("truncated", Value::Bool(false))])));
        assert_eq!(result.content, vec![text("one\ntwo\n")]);
    }

    #[test]
    fn reports_a_trailing_newline_omitted_by_the_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let result = truncate(ToolResult::text("one\n"), 2, 3, dir.path());
        assert_eq!(result.content[0], text("one"));
        assert!(marker(&result).starts_with("[showing lines 1-1 of 1; full output saved to "));
    }

    #[test]
    fn cleanup_removes_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let out = directory(dir.path());
        std::fs::create_dir_all(&out).unwrap();
        let old = out.join("tool_0123456789abXYZ");
        let other = out.join("keep.txt");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        let past = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(past).unwrap();
        std::fs::File::options().write(true).open(&other).unwrap().set_modified(past).unwrap();
        cleanup(dir.path());
        assert!(!old.exists());
        assert!(other.exists());
    }
}
