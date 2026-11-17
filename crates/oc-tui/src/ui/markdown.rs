//! Markdown → styled lines (stands in for OpenTUI's `<markdown>` renderable).
//! `conceal` hides inline markers (`**`, backticks, `#`) as in `session.markdown = "rendered"`.

use super::highlight::{highlight, token_style};
use super::text::{Click, SLine, Seg, Wrap, wrap};
use crate::theme::{SyntaxStyle, Theme};
use crate::util::session_mention::{parse_session_mention, session_mentions};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd, TextMergeStream};
use ratatui::style::{Modifier, Style};
use std::cell::RefCell;

pub struct MdOpts<'a> {
    pub theme: &'a Theme,
    pub syntax: &'a SyntaxStyle,
    pub conceal: bool,
    /// Base foreground (`markdown.text`, or `text.muted` for thinking).
    pub fg: crate::theme::Rgba,
    /// Force every style's fg to `fg` (thinking blocks).
    pub mono: bool,
    /// Show session IDs as links (`useSessionLinks`).
    pub links: Option<&'a SessionLinks<'a>>,
}

/// `useSessionLinks` (`ui/session-link.ts`): session IDs in prose show as links titled after their
/// sessions (`@<label>` when markers are concealed, the ID itself otherwise) and open them on click.
/// `label` answers `None` for IDs that stay plain (the current session, archived sessions and ones
/// that are not loaded). Every ID a render mentions is recorded with the label it got, so a cache
/// can re-render only what mentions a session whose label changed.
pub struct SessionLinks<'a> {
    label: &'a dyn Fn(&str) -> Option<String>,
    used: RefCell<Vec<(String, Option<String>)>>,
}

impl<'a> SessionLinks<'a> {
    pub fn new(label: &'a dyn Fn(&str) -> Option<String>) -> Self {
        SessionLinks { label, used: RefCell::new(Vec::new()) }
    }

    pub fn label(&self, id: &str) -> Option<String> {
        let label = (self.label)(id);
        let mut used = self.used.borrow_mut();
        if !used.iter().any(|(u, _)| u == id) {
            used.push((id.to_string(), label.clone()));
        }
        label
    }

    /// The IDs mentioned so far, each with the label it showed (`None`: plain), in first-seen order.
    pub fn take(&self) -> Vec<(String, Option<String>)> {
        std::mem::take(&mut *self.used.borrow_mut())
    }
}

struct Ctx<'a> {
    o: &'a MdOpts<'a>,
    out: Vec<SLine>,
    inline: Vec<Seg>,
    styles: Vec<Style>,
    prefix: Vec<Seg>,
    lists: Vec<Option<u64>>,
    width: usize,
    code: Option<(String, String)>,
    table: Option<Table>,
    link: Option<String>,
    pending_item: Option<Vec<Seg>>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Seg>>>,
    cell: Vec<Seg>,
    row: Vec<Vec<Seg>>,
    head_rows: usize,
    in_head: bool,
}

impl<'a> Ctx<'a> {
    fn style(&self) -> Style {
        *self.styles.last().unwrap()
    }
    fn st(&self, c: crate::theme::Rgba) -> Style {
        if self.o.mono { Style::default().fg(self.o.fg.tui()) } else { Style::default().fg(c.tui()) }
    }
    fn push_text(&mut self, text: &str, style: Style) {
        if let Some(t) = self.table.as_mut() {
            t.cell.push(Seg::new(text.to_string(), style));
            return;
        }
        self.inline.push(Seg::new(text.to_string(), style));
    }
    /// Prose text: session IDs become links (`linkChunks`). Tables are not prose blocks.
    fn push_prose(&mut self, text: &str, style: Style) {
        let Some(links) = self.o.links.filter(|_| self.table.is_none()) else {
            return self.push_text(text, style);
        };
        let mut at = 0;
        for m in session_mentions(text) {
            if m.start > at {
                self.push_text(&text[at..m.start], style);
            }
            let raw = &text[m.start..m.end];
            match links.label(&m.id) {
                Some(label) => {
                    let shown = if self.o.conceal { format!("@{label}") } else { raw.to_string() };
                    let link = self.link_style(style);
                    self.inline.push(Seg::new(shown, link).on_click(Click::SessionLink(m.id)));
                }
                None => self.push_text(raw, style),
            }
            at = m.end;
        }
        if at < text.len() {
            self.push_text(&text[at..], style);
        }
    }
    /// The chunk restyled as `markup.link.url`: its foreground and attributes, underlined.
    fn link_style(&self, style: Style) -> Style {
        let url = token_style(self.o.syntax.get("markup.link.url"));
        let mut s = style;
        s.fg = url.fg.or(style.fg);
        s.add_modifier = url.add_modifier | Modifier::UNDERLINED;
        s.sub_modifier = Modifier::empty();
        s
    }
    fn flush(&mut self) {
        if self.inline.is_empty() && self.pending_item.is_none() {
            return;
        }
        let mut first_prefix = self.prefix.clone();
        let mut rest_prefix = self.prefix.clone();
        if let Some(marker) = self.pending_item.take() {
            let mw: usize = marker.iter().map(|s| s.width()).sum();
            first_prefix.extend(marker);
            rest_prefix.push(Seg::new(" ".repeat(mw), Style::default()));
        }
        let pw: usize = first_prefix.iter().map(|s| s.width()).sum();
        let segs = std::mem::take(&mut self.inline);
        let lines = wrap(&segs, self.width.saturating_sub(pw).max(1), Wrap::Word);
        for (i, l) in lines.into_iter().enumerate() {
            let p = if i == 0 { &first_prefix } else { &rest_prefix };
            self.out.push(SLine::new(l).prefixed(p));
        }
    }
    fn blank(&mut self) {
        if !self.out.is_empty() && self.out.last().is_some_and(|l| !l.segs.is_empty() || l.bg.is_some()) {
            if self.prefix.is_empty() {
                self.out.push(SLine::blank());
            } else {
                self.out.push(SLine::new(self.prefix.clone()));
            }
        }
    }
}

/// Bare `http(s)://` URLs in plain text as `detectLinks` (OpenTUI 0.7) finds them: up to whitespace, `<`, `>`, `"`
/// or a backtick, without trailing `.,!?;:]}'` or an unbalanced `)`. Byte ranges.
pub fn bare_urls(text: &str) -> Vec<(usize, usize)> {
    static BARE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#"(?i)https?://[^\s<>"`]+"#).unwrap());
    let mut out = Vec::new();
    for m in BARE.find_iter(text) {
        let mut url = m.as_str();
        let open = url.matches('(').count();
        let mut close = url.matches(')').count();
        while let Some(last) = url.chars().last() {
            if last == ')' && close > open {
                close -= 1;
            } else if !".,!?;:]}'".contains(last) {
                break;
            }
            url = &url[..url.len() - last.len_utf8()];
        }
        if !url.is_empty() {
            out.push((m.start(), m.start() + url.len()));
        }
    }
    out
}

/// Render markdown text to lines of at most `width` cells.
pub fn render(text: &str, width: usize, o: &MdOpts) -> Vec<SLine> {
    let width = width.max(4);
    let base = Style::default().fg(o.fg.tui());
    let mut c = Ctx {
        o,
        out: Vec::new(),
        inline: Vec::new(),
        styles: vec![base],
        prefix: Vec::new(),
        lists: Vec::new(),
        width,
        code: None,
        table: None,
        link: None,
        pending_item: None,
    };
    let md = &o.theme.markdown;
    // Adjacent text events are one run, as OpenTUI's chunks are: a session ID or URL is not split at `_`.
    let parser = TextMergeStream::new(Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS));
    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    c.blank_if_needed();
                    let mut s = c.st(md.heading).add_modifier(Modifier::BOLD);
                    if level == HeadingLevel::H1 {
                        s = s.add_modifier(Modifier::UNDERLINED);
                    }
                    c.styles.push(s);
                    if !o.conceal {
                        let n = match level {
                            HeadingLevel::H1 => 1,
                            HeadingLevel::H2 => 2,
                            HeadingLevel::H3 => 3,
                            HeadingLevel::H4 => 4,
                            HeadingLevel::H5 => 5,
                            HeadingLevel::H6 => 6,
                        };
                        c.push_text(&format!("{} ", "#".repeat(n)), s);
                    }
                }
                Tag::BlockQuote(_) => {
                    c.flush();
                    let s = c.st(md.block_quote);
                    c.prefix.push(Seg::new("│ ", s));
                    c.styles.push(c.st(md.block_quote).add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    c.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    if !o.conceal {
                        let p = c.prefix.clone();
                        c.out.push(SLine::new(vec![Seg::new(format!("```{lang}"), c.st(md.code))]).prefixed(&p));
                    }
                    c.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    c.flush();
                    c.lists.push(start);
                }
                Tag::Item => {
                    c.flush();
                    let depth = c.lists.len().saturating_sub(1);
                    let indent = "  ".repeat(depth);
                    let marker = match c.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{indent}{n}. ");
                            *n += 1;
                            vec![Seg::new(m, c.st(md.list_enumeration))]
                        }
                        _ => vec![Seg::new(format!("{indent}- "), c.st(md.list_item))],
                    };
                    c.pending_item = Some(marker);
                }
                Tag::Emphasis => {
                    let s = c.style().add_modifier(Modifier::ITALIC);
                    let s = if o.mono { s } else { s.fg(md.emphasis.tui()) };
                    if !o.conceal {
                        c.push_text("*", s);
                    }
                    c.styles.push(s);
                }
                Tag::Strong => {
                    let s = c.style().add_modifier(Modifier::BOLD);
                    let s = if o.mono { s } else { s.fg(md.strong.tui()) };
                    if !o.conceal {
                        c.push_text("**", s);
                    }
                    c.styles.push(s);
                }
                Tag::Strikethrough => {
                    let s = c.style().add_modifier(Modifier::CROSSED_OUT);
                    c.styles.push(s);
                }
                Tag::Link { dest_url, .. } => {
                    let s = c.st(md.link_text).add_modifier(Modifier::UNDERLINED);
                    if !o.conceal {
                        c.push_text("[", c.style());
                    }
                    c.styles.push(s);
                    c.link = Some(dest_url.to_string());
                }
                Tag::Image { dest_url, .. } => {
                    let s = c.st(md.image_text);
                    c.styles.push(s);
                    c.link = Some(dest_url.to_string());
                }
                Tag::Table(_) => {
                    c.flush();
                    c.table = Some(Table::default());
                }
                Tag::TableHead => {
                    if let Some(t) = c.table.as_mut() {
                        t.in_head = true;
                    }
                    c.styles.push(c.style().add_modifier(Modifier::BOLD));
                }
                Tag::TableRow => {}
                Tag::TableCell => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    c.flush();
                    c.blank();
                }
                TagEnd::Heading(_) => {
                    c.styles.pop();
                    c.flush();
                    c.blank();
                }
                TagEnd::BlockQuote(_) => {
                    c.flush();
                    c.prefix.pop();
                    c.styles.pop();
                    c.blank();
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, code)) = c.code.take() {
                        let code = code.strip_suffix('\n').unwrap_or(&code).to_string();
                        let lines = if o.mono {
                            code.split('\n').map(|l| vec![Seg::new(l.to_string(), base)]).collect()
                        } else {
                            highlight(&code, &lang, o.syntax)
                        };
                        let p = c.prefix.clone();
                        let pw: usize = p.iter().map(|s| s.width()).sum();
                        for l in lines {
                            for wl in wrap(&l, width.saturating_sub(pw).max(1), Wrap::Char) {
                                c.out.push(SLine::new(wl).prefixed(&p));
                            }
                        }
                        if !o.conceal {
                            c.out.push(SLine::new(vec![Seg::new("```", c.st(md.code))]).prefixed(&p));
                        }
                    }
                    c.blank();
                }
                TagEnd::List(_) => {
                    c.flush();
                    c.lists.pop();
                    if c.lists.is_empty() {
                        c.blank();
                    }
                }
                TagEnd::Item => c.flush(),
                TagEnd::Emphasis => {
                    let s = c.styles.pop().unwrap_or(base);
                    if !o.conceal {
                        c.push_text("*", s);
                    }
                }
                TagEnd::Strong => {
                    let s = c.styles.pop().unwrap_or(base);
                    if !o.conceal {
                        c.push_text("**", s);
                    }
                }
                TagEnd::Strikethrough => {
                    c.styles.pop();
                }
                TagEnd::Link => {
                    c.styles.pop();
                    if let Some(url) = c.link.take() {
                        let ls = c.st(md.link).add_modifier(Modifier::UNDERLINED);
                        if o.conceal {
                            c.push_text(&format!(" ({url})"), ls);
                        } else {
                            c.push_text("](", c.style());
                            c.push_text(&url, ls);
                            c.push_text(")", c.style());
                        }
                    }
                }
                TagEnd::Image => {
                    c.styles.pop();
                    if let Some(url) = c.link.take() {
                        c.push_text(&format!(" ({url})"), c.st(md.image));
                    }
                }
                TagEnd::TableCell => {
                    if let Some(t) = c.table.as_mut() {
                        let cell = std::mem::take(&mut t.cell);
                        t.row.push(cell);
                    }
                }
                TagEnd::TableHead => {
                    c.styles.pop();
                    if let Some(t) = c.table.as_mut() {
                        let row = std::mem::take(&mut t.row);
                        t.rows.push(row);
                        t.head_rows = t.rows.len();
                        t.in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = c.table.as_mut() {
                        let row = std::mem::take(&mut t.row);
                        t.rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = c.table.take() {
                        let border = c.st(o.theme.border);
                        let p = c.prefix.clone();
                        for l in render_table(&t, width.saturating_sub(p.iter().map(|s| s.width()).sum()), border) {
                            c.out.push(l.prefixed(&p));
                        }
                    }
                    c.blank();
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some((_, code)) = c.code.as_mut() {
                    code.push_str(&t);
                } else {
                    let s = c.style();
                    if c.link.is_some() {
                        c.push_text(&t, s);
                    } else {
                        // OpenTUI 0.7's `detectLinks` gives bare URLs the link label's look.
                        let label = c.st(md.link_text);
                        let bare = s.patch(Style { fg: label.fg, ..Style::default() }).add_modifier(Modifier::UNDERLINED);
                        let mut at = 0;
                        for (a, b) in bare_urls(&t) {
                            if a > at {
                                c.push_prose(&t[at..a], s);
                            }
                            c.push_text(&t[a..b], bare);
                            at = b;
                        }
                        if at < t.len() {
                            c.push_prose(&t[at..], s);
                        }
                    }
                }
            }
            Event::Code(t) => {
                let style = if o.mono { base } else { token_style(o.syntax.get("markup.raw.inline")) };
                if !o.conceal {
                    c.push_text("`", style);
                }
                // A code span links only when it is exactly a session ID, so a command such as
                // `opencode --session ses_…` stays copyable.
                if c.link.is_none() && parse_session_mention(&t).is_some() {
                    c.push_prose(&t, style);
                } else {
                    c.push_text(&t, style);
                }
                if !o.conceal {
                    c.push_text("`", style);
                }
            }
            Event::SoftBreak => c.push_text(" ", base),
            Event::HardBreak => c.push_text("\n", base),
            Event::Rule => {
                c.flush();
                let s = c.st(md.horizontal_rule);
                c.out.push(SLine::new(vec![Seg::new("─".repeat(width), s)]));
                c.blank();
            }
            Event::TaskListMarker(done) => {
                let marker = if done { "[x] " } else { "[ ] " };
                c.push_text(marker, c.st(md.list_item));
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                let s = c.style();
                c.push_text(h.trim_end_matches('\n'), s);
            }
            _ => {}
        }
    }
    c.flush();
    while c.out.last().is_some_and(|l| l.segs.iter().all(|s| s.text.trim().is_empty() || s.text == "│ ")) {
        c.out.pop();
    }
    c.out
}

impl Ctx<'_> {
    fn blank_if_needed(&mut self) {
        self.flush();
    }
}

fn render_table(t: &Table, width: usize, border: Style) -> Vec<SLine> {
    let cols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return vec![];
    }
    let mut widths = vec![1usize; cols];
    for r in &t.rows {
        for (i, cell) in r.iter().enumerate() {
            let w: usize = cell.iter().map(|s| s.width()).sum();
            widths[i] = widths[i].max(w);
        }
    }
    // Shrink columns to fit: each column has padding 1 on both sides plus borders.
    let overhead = cols * 3 + 1;
    while widths.iter().sum::<usize>() + overhead > width && widths.iter().any(|w| *w > 3) {
        let (i, _) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
        widths[i] -= 1;
    }
    let rule = |l: &str, m: &str, r: &str| -> SLine {
        let mut s = l.to_string();
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if i + 1 == cols { r } else { m });
        }
        SLine::new(vec![Seg::new(s, border)])
    };
    let mut out = vec![rule("┌", "┬", "┐")];
    for (ri, r) in t.rows.iter().enumerate() {
        let wrapped: Vec<Vec<Vec<Seg>>> = (0..cols)
            .map(|i| {
                let cell = r.get(i).cloned().unwrap_or_default();
                wrap(&cell, widths[i], Wrap::Word)
            })
            .collect();
        let h = wrapped.iter().map(|c| c.len()).max().unwrap_or(1);
        for li in 0..h {
            let mut segs = vec![Seg::new("│", border)];
            for (i, w) in widths.iter().enumerate() {
                segs.push(Seg::new(" ", Style::default()));
                let line = wrapped[i].get(li).cloned().unwrap_or_default();
                let lw: usize = line.iter().map(|s| s.width()).sum();
                segs.extend(line);
                segs.push(Seg::new(" ".repeat(w.saturating_sub(lw) + 1), Style::default()));
                segs.push(Seg::new("│", border));
            }
            out.push(SLine::new(segs));
        }
        if ri + 1 < t.rows.len() {
            out.push(rule("├", "┼", "┤"));
        }
    }
    out.push(rule("└", "┴", "┘"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Mode, OPENCODE_V2, resolve};

    #[test]
    fn finds_bare_urls_like_detect_links() {
        let t = "see https://bare.url/path. and (https://x.y/a_(b)) or HTTP://Q.r/s?x=1, `https://no` <https://z>";
        let found: Vec<&str> = bare_urls(t).into_iter().map(|(a, b)| &t[a..b]).collect();
        assert_eq!(found, ["https://bare.url/path", "https://x.y/a_(b)", "HTTP://Q.r/s?x=1", "https://no", "https://z"]);
    }

    fn lines(text: &str, conceal: bool) -> Vec<String> {
        let doc: serde_json::Value = serde_json::from_str(OPENCODE_V2).unwrap();
        let t = resolve::resolve_document(&doc, Mode::Dark).unwrap();
        let st = SyntaxStyle::from_theme(&t);
        let o = MdOpts { theme: &t, syntax: &st, conceal, fg: t.markdown.text, mono: false, links: None };
        render(text, 40, &o).into_iter().map(|l| l.text()).collect()
    }

    const KNOWN: &str = "ses_knownknownknownknownknownk";
    const OTHER: &str = "ses_otherotherotherotherothero";

    type Linked = (Vec<String>, Vec<(String, Click)>, Vec<(String, Option<String>)>);

    /// Renders with links for `KNOWN` (titled) only: (rows, linked texts with their targets, mentioned IDs).
    fn linked(text: &str, conceal: bool, width: usize) -> Linked {
        let doc: serde_json::Value = serde_json::from_str(OPENCODE_V2).unwrap();
        let t = resolve::resolve_document(&doc, Mode::Dark).unwrap();
        let st = SyntaxStyle::from_theme(&t);
        let label = |id: &str| (id == KNOWN).then(|| "Fix the flaky reconnect test on Windows".to_string());
        let links = SessionLinks::new(&label);
        let o = MdOpts { theme: &t, syntax: &st, conceal, fg: t.markdown.text, mono: false, links: Some(&links) };
        let out = render(text, width, &o);
        let found = out.iter().flat_map(|l| l.segs.iter()).filter_map(|s| s.link.as_ref().map(|c| (s.text.clone(), (**c).clone()))).collect();
        for s in out.iter().flat_map(|l| l.segs.iter()).filter(|s| s.link.is_some()) {
            assert!(s.style.add_modifier.contains(Modifier::UNDERLINED));
            assert_eq!(s.style.fg, Some(t.markdown.link.tui()));
        }
        (out.iter().map(|l| l.text()).collect(), found, links.take())
    }

    /// `session-link.test.tsx` "session links in … markdown": only loaded sessions link; a long
    /// title wraps and both rows open it; fenced code stays plain.
    #[test]
    fn session_ids_link_in_prose() {
        let text = format!("**中文** see {KNOWN} now\n\n- other {OTHER}\n\n```\n{KNOWN}\n```");
        let (rows, found, used) = linked(&text, true, 30);
        let link = Click::SessionLink(KNOWN.into());
        assert_eq!(found, vec![("@Fix the flaky".to_string(), link.clone()), ("reconnect test on Windows".to_string(), link)]);
        assert_eq!(rows[0], "中文 see @Fix the flaky");
        assert_eq!(rows[1], "reconnect test on Windows now");
        assert!(rows.iter().any(|r| r.starts_with("- other ses_other")), "{rows:?}");
        assert_eq!(rows.last().unwrap(), KNOWN);
        assert_eq!(used, vec![(KNOWN.to_string(), Some("Fix the flaky reconnect test on Windows".to_string())), (OTHER.to_string(), None)]);
        // Source mode keeps the ID text, still a link; `@session:` mentions link too.
        let (rows, found, _) = linked(&format!("see @session:{KNOWN} now"), false, 80);
        assert_eq!(rows, [format!("see @session:{KNOWN} now")]);
        assert_eq!(found, vec![(format!("@session:{KNOWN}"), Click::SessionLink(KNOWN.into()))]);
    }

    /// "code spans link only when they are exactly an ID".
    #[test]
    fn code_spans_link_only_when_exactly_an_id() {
        let text = format!("Run `opencode --session {KNOWN}` or open `{KNOWN}`.");
        let (rows, found, _) = linked(&text, true, 200);
        assert_eq!(found.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["@Fix the flaky reconnect test on Windows"]);
        assert!(rows[0].contains(&format!("opencode --session {KNOWN}")));
        let (rows, found, _) = linked(&text, false, 200);
        assert_eq!(found.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), [KNOWN]);
        assert!(rows[0].contains(&format!("`opencode --session {KNOWN}`")));
    }

    #[test]
    fn urls_and_markdown_links_keep_their_ids() {
        let text = format!("https://host/{KNOWN} and [x](https://h/{KNOWN}) and [{KNOWN}](https://h)");
        let (_, found, used) = linked(&text, true, 200);
        assert!(found.is_empty() && used.is_empty(), "{found:?}");
    }

    #[test]
    fn basics() {
        assert_eq!(lines("# Title\n\nSome **bold** text", true), vec!["Title", "", "Some bold text"]);
        assert_eq!(lines("Some **bold**", false), vec!["Some **bold**"]);
        assert_eq!(lines("- a\n- b", true), vec!["- a", "- b"]);
        assert_eq!(lines("1. a\n2. b", true), vec!["1. a", "2. b"]);
        assert_eq!(lines("```rust\nfn x() {}\n```", true), vec!["fn x() {}"]);
        let t = lines("| a | b |\n|---|---|\n| 1 | 2 |", true);
        assert_eq!(t[0], "┌───┬───┐");
        assert_eq!(t[1], "│ a │ b │");
        assert_eq!(t.len(), 5);
    }
}
