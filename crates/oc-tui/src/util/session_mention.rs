//! `@opencode/util/session-mention`: session IDs mentioned in text.

use fancy_regex::Regex;
use std::sync::LazyLock;

/// Complete generated IDs only, including while an assistant is streaming one. `\w` is spelled out:
/// JavaScript's is ASCII.
static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?<![A-Za-z0-9_/:@-])(?:@session:)?(ses_[A-Za-z0-9]{26})(?![A-Za-z0-9_/-])").unwrap());

/// One mention: the session ID, the matched text (with any `@session:` prefix) and its byte range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

/// `sessionMentions`.
pub fn session_mentions(text: &str) -> Vec<Mention> {
    if !text.contains("ses_") {
        return Vec::new();
    }
    PATTERN
        .captures_iter(text)
        .filter_map(Result::ok)
        .filter_map(|c| {
            let all = c.get(0)?;
            Some(Mention { id: c.get(1)?.as_str().to_string(), start: all.start(), end: all.end() })
        })
        .collect()
}

/// `parseSessionMention`: the ID when the whole text is one mention.
pub fn parse_session_mention(text: &str) -> Option<String> {
    session_mentions(text).into_iter().next().filter(|m| m.start == 0 && m.end == text.len()).map(|m| m.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "ses_0123456789abcdefghijklmnop";

    #[test]
    fn session_tokens_round_trip() {
        assert_eq!(parse_session_mention(&format!("@session:{ID}")).as_deref(), Some(ID));
        assert_eq!(parse_session_mention(ID).as_deref(), Some(ID));
        let ids: Vec<String> = session_mentions(&format!("See @session:{ID}, then {ID}.")).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, [ID, ID]);
    }

    #[test]
    fn does_not_parse_malformed_or_embedded_ids() {
        for text in ["ses_123".to_string(), format!("@session:{ID}extra"), format!("x{ID}"), format!("{ID}/path"), format!("https://host/{ID}"), format!("@other:{ID}"), format!("{ID}_x")] {
            assert_eq!(parse_session_mention(&text), None, "{text}");
            assert_eq!(session_mentions(&text), vec![], "{text}");
        }
    }

    #[test]
    fn non_ascii_neighbours_do_not_block_a_mention() {
        // JavaScript's `\w` is ASCII, so a CJK character next to an ID is a boundary.
        assert_eq!(session_mentions(&format!("见{ID}。")).len(), 1);
    }

    /// `session-title-fallback.test.ts` "supplies compact display labels".
    #[test]
    fn display_labels() {
        use crate::api::types::SessionInfo;
        let label = |title: Option<&str>, parent: bool| {
            SessionInfo { title: title.map(str::to_string), parent_id: parent.then(|| "ses_parent".to_string()), ..Default::default() }.display_label()
        };
        assert_eq!(label(None, false), "New session");
        assert_eq!(label(None, true), "Child session");
        assert_eq!(label(Some(""), true), "Child session");
        assert_eq!(label(Some("New session - 2026-07-30T18:45:03.662Z"), false), "New session");
        assert_eq!(label(Some("Child session - 2026-07-30T18:45:03.662Z"), false), "Child session");
        assert_eq!(label(Some("Generated title"), false), "Generated title");
    }
}
