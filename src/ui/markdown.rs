//! Agent markdown to styled, width-wrapped transcript lines.
//!
//! Output carries no outer gutter; the caller prefixes each line. Re-parsed on
//! every frame like the rest of the transcript, so partial (streaming)
//! markdown must degrade gracefully, which pulldown-cmark does.

use std::path::Path;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::{wrap_code, ACCENT, CODE_FG, DIM};

/// A clickable link region on one rendered markdown line (0-based line index
/// and display-column range `[start_col, end_col)` within that line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub line: usize,
    pub start_col: u16,
    pub end_col: u16,
    pub url: String,
}

#[derive(Debug, Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<LinkSpan>,
}

#[derive(Clone)]
struct Run {
    text: String,
    style: Style,
    url: Option<String>,
}

struct ActiveLink {
    url: String,
    is_image: bool,
    start_run: usize,
}

#[cfg(test)]
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    render_with_links(text, width, None).lines
}

pub fn render_with_links(text: &str, width: usize, cwd: Option<&Path>) -> Rendered {
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut r = Renderer {
        width: width.max(1),
        cwd,
        out: Vec::new(),
        link_spans: Vec::new(),
        runs: Vec::new(),
        styles: Vec::new(),
        lists: Vec::new(),
        items: Vec::new(),
        marker: None,
        quote: 0,
        code: None,
        links: Vec::new(),
        refs: Vec::new(),
        fresh: true,
    };
    for ev in Parser::new_ext(text, opts) {
        r.event(ev);
    }
    r.flush();
    r.emit_refs();
    Rendered {
        lines: r.out,
        links: r.link_spans,
    }
}

struct Renderer<'a> {
    width: usize,
    cwd: Option<&'a Path>,
    out: Vec<Line<'static>>,
    link_spans: Vec<LinkSpan>,
    /// Inline runs of the block being built; "\n" is a hard break.
    runs: Vec<Run>,
    styles: Vec<Style>,
    /// Open lists: next number for ordered, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// Marker width of every open item, for continuation indent.
    items: Vec<usize>,
    /// (indent, marker) waiting for the first line of an item.
    marker: Option<(usize, String)>,
    quote: usize,
    code: Option<String>,
    /// Stack of open links / images.
    links: Vec<ActiveLink>,
    /// Deduplicated reference footnotes: `(display_target, href_url)`.
    refs: Vec<(String, String)>,
    /// Last emitted line was blank (or nothing emitted yet).
    fresh: bool,
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn active_href(&self) -> Option<String> {
        self.links
            .iter()
            .rev()
            .find(|l| !l.is_image && !l.url.is_empty())
            .map(|l| l.url.clone())
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
    }

    fn push_run(&mut self, text: impl Into<String>, style: Style, url: Option<String>) {
        self.runs.push(Run {
            text: text.into(),
            style,
            url,
        });
    }

    fn quote_span(&self) -> Span<'static> {
        Span::styled("▎ ".repeat(self.quote), DIM)
    }

    fn push_blank(&mut self) {
        self.out.push(Line::from(vec![self.quote_span()]));
        self.fresh = true;
    }

    /// Blank line between top-level blocks; compact inside list items.
    fn separate(&mut self) {
        if self.items.is_empty() && !self.fresh {
            self.push_blank();
        }
    }

    fn register_ref(&mut self, display: String, href: String) -> usize {
        if let Some(i) = self
            .refs
            .iter()
            .position(|(d, u)| d == &display && u == &href)
        {
            return i + 1;
        }
        self.refs.push((display, href));
        self.refs.len()
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => match &mut self.code {
                Some(buf) => buf.push_str(&t),
                None => {
                    let s = self.style();
                    let url = self.active_href();
                    self.push_run(t.into_string(), s, url);
                }
            },
            Event::Code(t) => {
                let url = self.active_href();
                // Keep link colour when inline code sits inside a link so it
                // still reads as clickable.
                let s = if url.is_some() {
                    self.style()
                } else {
                    self.style().fg(CODE_FG)
                };
                self.push_run(t.into_string(), s, url);
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                let s = self.style();
                let url = self.active_href();
                self.push_run(t.into_string(), s, url);
            }
            Event::SoftBreak => {
                let s = self.style();
                let url = self.active_href();
                self.push_run(" ", s, url);
            }
            Event::HardBreak => self.push_run("\n", Style::new(), None),
            Event::TaskListMarker(done) => {
                let m = if done { "[x] " } else { "[ ] " };
                self.push_run(m, DIM, None);
            }
            Event::Rule => {
                self.flush();
                self.separate();
                let n = self.width.saturating_sub(self.quote * 2).min(40);
                self.out.push(Line::from(vec![
                    self.quote_span(),
                    Span::styled("─".repeat(n), DIM),
                ]));
                self.fresh = false;
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.separate(),
            Tag::Heading { level, .. } => {
                self.separate();
                let accent = level == HeadingLevel::H1;
                self.push_style(|s| {
                    let s = s.add_modifier(Modifier::BOLD);
                    if accent {
                        s.fg(ACCENT)
                    } else {
                        s
                    }
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.separate();
                self.quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush();
                self.separate();
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                self.separate();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let indent: usize = self.items.iter().sum();
                let m = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.items.push(m.chars().count());
                self.marker = Some((indent, m));
            }
            Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.push_style(|s| s.fg(ACCENT).add_modifier(Modifier::UNDERLINED));
                self.links.push(ActiveLink {
                    url: dest_url.into_string(),
                    is_image: false,
                    start_run: self.runs.len(),
                });
            }
            Tag::Image { dest_url, .. } => {
                self.push_style(|s| s.fg(Color::DarkGray));
                self.links.push(ActiveLink {
                    url: dest_url.into_string(),
                    is_image: true,
                    start_run: self.runs.len(),
                });
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => self.code_block(),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
                self.marker = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                self.styles.pop();
                let Some(active) = self.links.pop() else {
                    return;
                };
                let label: String = self.runs[active.start_run..]
                    .iter()
                    .map(|r| r.text.as_str())
                    .collect();

                if active.is_image {
                    // Replace nested image alt runs with a compact marker.
                    let in_link = self.active_href();
                    let style = if in_link.is_some() { self.style() } else { DIM };
                    self.runs.truncate(active.start_run);
                    let marker = if label.trim().is_empty() {
                        "[image]".to_string()
                    } else {
                        format!("[image: {}]", label.trim())
                    };
                    self.push_run(marker, style, in_link);
                    return;
                }

                if active.url.is_empty() {
                    return;
                }

                // Empty link text: display a clean representation of the target.
                if label.trim().is_empty() {
                    let link_style = self.style().fg(ACCENT).add_modifier(Modifier::UNDERLINED);
                    let shown = parse_file_uri(&active.url, self.cwd)
                        .map(|f| f.display)
                        .unwrap_or_else(|| active.url.clone());
                    self.push_run(shown, link_style, Some(active.url));
                    return;
                }

                // File links (`file:///...`): keep prose clean and attach the
                // line range if omitted from the label; only add a reference
                // when the label does not identify the file at all.
                if let Some(file) = parse_file_uri(&active.url, self.cwd) {
                    if label_identifies_file(&label, &file.compact_path) {
                        if let Some(lines) = &file.lines {
                            if !label_has_line_suffix(&label, lines) {
                                self.push_run(format!(":{lines}"), DIM, Some(active.url));
                            }
                        }
                    } else {
                        let n = self.register_ref(file.display, active.url.clone());
                        self.push_run(format!("[{n}]"), DIM, Some(active.url));
                    }
                    return;
                }

                // Web / other links: omit redundant references when the label
                // already shows the URL; otherwise append a compact `[n]` ref.
                if !label_matches_url(&label, &active.url) {
                    let n = self.register_ref(active.url.clone(), active.url.clone());
                    self.push_run(format!("[{n}]"), DIM, Some(active.url));
                }
            }
            _ => {}
        }
    }

    /// Spans that start every output line of the current block.
    fn prefix(&self, first: bool) -> Vec<Span<'static>> {
        let indent: usize = self.items.iter().sum();
        let mut p = vec![self.quote_span()];
        match (first, &self.marker) {
            (true, Some((mi, m))) => {
                p.push(Span::raw(" ".repeat(*mi)));
                p.push(Span::styled(m.clone(), Style::new().fg(ACCENT)));
            }
            _ => p.push(Span::raw(" ".repeat(indent))),
        }
        p
    }

    fn prefix_width(&self) -> usize {
        self.quote * 2 + self.items.iter().sum::<usize>()
    }

    fn flush(&mut self) {
        if self.runs.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.runs);
        let pwidth = self.prefix_width();
        let avail = self.width.saturating_sub(pwidth).max(1);
        let laid = layout(&runs, avail);
        let base_line = self.out.len();
        for (i, spans) in laid.lines.into_iter().enumerate() {
            let mut line = self.prefix(i == 0);
            line.extend(spans);
            self.out.push(Line::from(line));
        }
        let pwidth_u16 = u16::try_from(pwidth).unwrap_or(u16::MAX);
        for mut ls in laid.links {
            ls.line += base_line;
            ls.start_col = ls.start_col.saturating_add(pwidth_u16);
            ls.end_col = ls.end_col.saturating_add(pwidth_u16);
            self.link_spans.push(ls);
        }
        self.marker = None;
        self.fresh = false;
    }

    fn emit_refs(&mut self) {
        if self.refs.is_empty() {
            return;
        }
        if !self.fresh {
            self.push_blank();
        }
        let refs = std::mem::take(&mut self.refs);
        for (i, (display, href)) in refs.into_iter().enumerate() {
            let tag = format!("[{}] ", i + 1);
            let tag_w = display_width(&tag);
            let avail = self.width.saturating_sub(tag_w).max(1);
            let url_style = DIM.add_modifier(Modifier::UNDERLINED);
            for (row, chunk) in wrap_code(&display, avail).into_iter().enumerate() {
                let line_idx = self.out.len();
                let lead = if row == 0 {
                    Span::styled(tag.clone(), DIM)
                } else {
                    Span::raw(" ".repeat(tag_w))
                };
                let chunk_w = display_width(&chunk);
                self.out
                    .push(Line::from(vec![lead, Span::styled(chunk, url_style)]));
                if chunk_w > 0 {
                    let start = u16::try_from(tag_w).unwrap_or(u16::MAX);
                    let end = start.saturating_add(u16::try_from(chunk_w).unwrap_or(u16::MAX));
                    self.link_spans.push(LinkSpan {
                        line: line_idx,
                        start_col: start,
                        end_col: end,
                        url: href.clone(),
                    });
                }
            }
        }
        self.fresh = false;
    }

    fn code_block(&mut self) {
        let Some(buf) = self.code.take() else { return };
        let avail = self.width.saturating_sub(self.prefix_width() + 2).max(1);
        for raw in buf.trim_end_matches('\n').lines() {
            for l in wrap_code(raw, avail) {
                let mut line = self.prefix(false);
                line.push(Span::styled("▏ ", DIM));
                line.push(Span::styled(l, Style::new().fg(CODE_FG)));
                self.out.push(Line::from(line));
            }
        }
        self.fresh = false;
    }
}

use ratatui::style::Color;

struct ParsedFileUri {
    compact_path: String,
    lines: Option<String>,
    display: String,
}

fn parse_file_uri(url: &str, cwd: Option<&Path>) -> Option<ParsedFileUri> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let (raw_path, fragment) = match rest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (rest, None),
    };
    let decoded = percent_decode(raw_path);
    let compact_path = match cwd {
        Some(base) => {
            let base_str = base.to_string_lossy();
            let prefix = base_str.trim_end_matches('/');
            decoded
                .strip_prefix(prefix)
                .and_then(|s| s.strip_prefix('/'))
                .map(str::to_string)
                .unwrap_or_else(|| decoded.clone())
        }
        None => decoded,
    };
    let lines = fragment.and_then(parse_line_fragment);
    let display = match &lines {
        Some(l) => format!("{compact_path}:{l}"),
        None => compact_path.clone(),
    };
    Some(ParsedFileUri {
        compact_path,
        lines,
        display,
    })
}

/// Convert `#L10`, `#L10-L20`, `#L10-20`, or `#10` into `"10"` / `"10-20"`.
fn parse_line_fragment(frag: &str) -> Option<String> {
    let f = frag
        .strip_prefix('L')
        .or_else(|| frag.strip_prefix('l'))
        .unwrap_or(frag);
    if let Some((a, b)) = f.split_once('-') {
        let b = b
            .strip_prefix('L')
            .or_else(|| b.strip_prefix('l'))
            .unwrap_or(b);
        if !a.is_empty()
            && !b.is_empty()
            && a.chars().all(|c| c.is_ascii_digit())
            && b.chars().all(|c| c.is_ascii_digit())
        {
            return Some(format!("{a}-{b}"));
        }
        return None;
    }
    if !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()) {
        return Some(f.to_string());
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Whether `label` already names the file referenced by `compact_path`
/// (e.g. `markdown.rs`, `src/ui/markdown.rs`, or `markdown.rs:10-20`).
fn label_identifies_file(label: &str, compact_path: &str) -> bool {
    let clean = label.trim().trim_matches('`');
    let without_line = clean
        .split_once(':')
        .filter(|(_, tail)| {
            !tail.is_empty()
                && tail
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '-' || c == 'L' || c == 'l')
        })
        .map_or(clean, |(head, _)| head);
    let without_line = without_line.trim_start_matches("./");
    if without_line.is_empty() {
        return false;
    }
    let path = compact_path.trim_start_matches("./");
    if path == without_line {
        return true;
    }
    path.ends_with(&format!("/{without_line}"))
}

fn label_has_line_suffix(label: &str, lines: &str) -> bool {
    let clean = label.trim().trim_matches('`');
    clean.ends_with(&format!(":{lines}")) || clean.contains(&format!("#L{lines}"))
}

fn label_matches_url(label: &str, url: &str) -> bool {
    fn norm(s: &str) -> &str {
        let s = s.trim();
        let s = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .or_else(|| s.strip_prefix("mailto:"))
            .unwrap_or(s);
        s.trim_end_matches('/')
    }
    norm(label) == norm(url)
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

struct LaidOut {
    lines: Vec<Vec<Span<'static>>>,
    links: Vec<LinkSpan>,
}

/// Greedy word wrap over styled runs; words may span several styles and link URLs.
fn layout(runs: &[Run], avail: usize) -> LaidOut {
    let mut l = Layout {
        avail: avail.max(1),
        lines: Vec::new(),
        links: Vec::new(),
        cur: Vec::new(),
        n: 0,
    };
    let mut word: Vec<Run> = Vec::new();
    for run in runs {
        if run.text == "\n" {
            l.place(std::mem::take(&mut word));
            l.newline();
            continue;
        }
        for ch in run.text.chars() {
            if ch.is_whitespace() {
                l.place(std::mem::take(&mut word));
            } else {
                match word.last_mut() {
                    Some(last) if last.style == run.style && last.url == run.url => {
                        last.text.push(ch);
                    }
                    _ => word.push(Run {
                        text: ch.to_string(),
                        style: run.style,
                        url: run.url.clone(),
                    }),
                }
            }
        }
    }
    l.place(word);
    if !l.cur.is_empty() || l.lines.is_empty() {
        l.lines.push(l.cur);
    }
    LaidOut {
        lines: l.lines,
        links: l.links,
    }
}

struct Layout {
    avail: usize,
    lines: Vec<Vec<Span<'static>>>,
    links: Vec<LinkSpan>,
    cur: Vec<Span<'static>>,
    n: usize,
}

impl Layout {
    fn newline(&mut self) {
        self.lines.push(std::mem::take(&mut self.cur));
        self.n = 0;
    }

    fn record_link(&mut self, start_col: usize, end_col: usize, url: &str) {
        if start_col >= end_col {
            return;
        }
        let line = self.lines.len();
        let start = u16::try_from(start_col).unwrap_or(u16::MAX);
        let end = u16::try_from(end_col).unwrap_or(u16::MAX);
        if let Some(last) = self.links.last_mut() {
            if last.line == line && last.url == url && last.end_col == start {
                last.end_col = end;
                return;
            }
        }
        self.links.push(LinkSpan {
            line,
            start_col: start,
            end_col: end,
            url: url.to_string(),
        });
    }

    fn place(&mut self, word: Vec<Run>) {
        let wl: usize = word.iter().map(|r| display_width(&r.text)).sum();
        if wl == 0 {
            return;
        }
        if self.n > 0 && self.n + 1 + wl <= self.avail {
            // If this word continues the same link on the same line, extend
            // the link across the separating space too.
            let space_url = word.first().and_then(|r| r.url.as_deref()).and_then(|u| {
                let last = self.links.last()?;
                (last.line == self.lines.len()
                    && last.url == u
                    && usize::from(last.end_col) == self.n)
                    .then_some(u)
            });
            let start = self.n;
            let space_style = if space_url.is_some() {
                word[0].style
            } else {
                Style::new()
            };
            self.cur.push(Span::styled(" ", space_style));
            self.n += 1;
            if let Some(u) = space_url {
                self.record_link(start, self.n, u);
            }
        } else if self.n > 0 {
            self.newline();
        }
        if wl <= self.avail - self.n {
            for r in word {
                let start = self.n;
                let w = display_width(&r.text);
                self.cur.push(Span::styled(r.text, r.style));
                self.n += w;
                if let Some(url) = &r.url {
                    self.record_link(start, self.n, url);
                }
            }
            return;
        }
        // Longer than a whole line: hard split.
        for r in word {
            for ch in r.text.chars() {
                let cw = ch.width().unwrap_or(0);
                if self.n > 0 && self.n + cw > self.avail {
                    self.newline();
                }
                let start = self.n;
                match self.cur.last_mut() {
                    Some(last) if last.style == r.style => last.content.to_mut().push(ch),
                    _ => self.cur.push(Span::styled(ch.to_string(), r.style)),
                }
                self.n += cw;
                if let Some(url) = &r.url {
                    self.record_link(start, self.n, url);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn inline_styles_and_wrap() {
        let out = render("hello **bold** and `code`", 80);
        assert_eq!(text(&out), vec!["hello bold and code"]);
        let bold = out[0].spans.iter().find(|s| s.content == "bold").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));

        let out = render("aaa bbb ccc", 7);
        assert_eq!(text(&out), vec!["aaa bbb", "ccc"]);
    }

    #[test]
    fn blocks() {
        let md =
            "# Title\n\npara\n\n- one\n- two\n\n1. a\n2. b\n\n> quoted\n\n```rs\nlet x = 1;\n```\n";
        assert_eq!(
            text(&render(md, 40)),
            vec![
                "Title",
                "",
                "para",
                "",
                "• one",
                "• two",
                "",
                "1. a",
                "2. b",
                "",
                "▎ quoted",
                "",
                "▏ let x = 1;",
            ]
        );
    }

    #[test]
    fn list_continuation_is_indented() {
        let out = render("- aaa bbb ccc", 7);
        assert_eq!(text(&out), vec!["• aaa", "  bbb", "  ccc"]);
    }

    #[test]
    fn web_links_use_deduplicated_footnotes_and_record_spans() {
        let rendered = render_with_links(
            "See [ACP docs](https://x.y/acp), [again](https://x.y/acp), and [home](https://example.com).",
            80,
            None,
        );
        assert_eq!(
            text(&rendered.lines),
            vec![
                "See ACP docs[1], again[1], and home[2].",
                "",
                "[1] https://x.y/acp",
                "[2] https://example.com",
            ]
        );
        assert!(rendered.links.iter().any(|l| l.line == 0
            && l.start_col == 4
            && l.end_col == 15
            && l.url == "https://x.y/acp"));
        assert!(rendered
            .links
            .iter()
            .any(|l| l.line == 2 && l.start_col == 4 && l.url == "https://x.y/acp"));
    }

    #[test]
    fn redundant_url_labels_and_autolinks_do_not_emit_footnotes() {
        let rendered = render_with_links("Visit <https://x.y> or [x.y](https://x.y/).", 80, None);
        assert_eq!(text(&rendered.lines), vec!["Visit https://x.y or x.y."]);
        assert_eq!(rendered.links.len(), 2);
        assert_eq!(rendered.links[0].url, "https://x.y");
        assert_eq!(rendered.links[1].url, "https://x.y/");
    }

    #[test]
    fn file_links_avoid_raw_uri_clutter_and_format_line_ranges() {
        let cwd = Path::new("/repo");
        let rendered = render_with_links(
            "Check [markdown.rs](file:///repo/src/ui/markdown.rs#L10-L20), [src/app.rs:42](file:///repo/src/app.rs#L42), and [the entry point](file:///repo/src/main.rs#L1).",
            120,
            Some(cwd),
        );
        assert_eq!(
            text(&rendered.lines),
            vec![
                "Check markdown.rs:10-20, src/app.rs:42, and the entry point[1].",
                "",
                "[1] src/main.rs:1",
            ]
        );
        assert!(rendered
            .links
            .iter()
            .any(|l| l.url == "file:///repo/src/ui/markdown.rs#L10-L20"));
        assert!(rendered
            .links
            .iter()
            .any(|l| l.url == "file:///repo/src/main.rs#L1"));
    }

    #[test]
    fn empty_link_label_and_nested_image_link_render_cleanly() {
        let rendered = render_with_links(
            "[](https://x.y) and [![logo](https://x.y/a.png)](https://x.y)",
            80,
            None,
        );
        assert_eq!(
            text(&rendered.lines),
            vec!["https://x.y and [image: logo][1]", "", "[1] https://x.y"]
        );
    }

    #[test]
    fn wrapped_link_records_spans_on_each_line() {
        let rendered = render_with_links("[alpha beta gamma](https://x.y)", 10, None);
        let line0: Vec<_> = rendered.links.iter().filter(|l| l.line == 0).collect();
        let line1: Vec<_> = rendered.links.iter().filter(|l| l.line == 1).collect();
        assert!(!line0.is_empty());
        assert!(!line1.is_empty());
        assert_eq!(line0[0].url, "https://x.y");
        assert_eq!(line1[0].url, "https://x.y");
    }

    #[test]
    fn unterminated_input_does_not_panic() {
        let _ = render("**half and `open\n\n```\ncode", 10);
        let _ = render("", 10);
    }
}
