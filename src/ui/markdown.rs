//! Agent markdown to styled, width-wrapped transcript lines.
//!
//! Output carries no outer gutter; the caller prefixes each line. Re-parsed on
//! every frame like the rest of the transcript, so partial (streaming)
//! markdown must degrade gracefully, which pulldown-cmark does.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{wrap_code, ACCENT, CODE_FG, DIM};

type Run = (String, Style);

pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut r = Renderer {
        width: width.max(1),
        out: Vec::new(),
        runs: Vec::new(),
        styles: Vec::new(),
        lists: Vec::new(),
        items: Vec::new(),
        marker: None,
        quote: 0,
        code: None,
        link: None,
        fresh: true,
    };
    for ev in Parser::new_ext(text, opts) {
        r.event(ev);
    }
    r.flush();
    r.out
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
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
    /// Link destination and index of its first run.
    link: Option<(String, usize)>,
    /// Last emitted line was blank (or nothing emitted yet).
    fresh: bool,
}

impl Renderer {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
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

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => match &mut self.code {
                Some(buf) => buf.push_str(&t),
                None => {
                    let s = self.style();
                    self.runs.push((t.into_string(), s));
                }
            },
            Event::Code(t) => {
                let s = self.style().fg(CODE_FG);
                self.runs.push((t.into_string(), s));
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                let s = self.style();
                self.runs.push((t.into_string(), s));
            }
            Event::SoftBreak => {
                let s = self.style();
                self.runs.push((" ".into(), s));
            }
            Event::HardBreak => self.runs.push(("\n".into(), Style::new())),
            Event::TaskListMarker(done) => {
                let m = if done { "[x] " } else { "[ ] " };
                self.runs.push((m.into(), DIM));
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
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.push_style(|s| s.fg(ACCENT).add_modifier(Modifier::UNDERLINED));
                self.link = Some((dest_url.into_string(), self.runs.len()));
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
                if let Some((url, from)) = self.link.take() {
                    let text: String = self.runs[from..].iter().map(|(t, _)| t.as_str()).collect();
                    if !url.is_empty() && text != url {
                        let s = self.style();
                        self.runs.push((" ".into(), s));
                        self.runs.push((format!("({url})"), DIM));
                    }
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
        let avail = self.width.saturating_sub(self.prefix_width()).max(1);
        for (i, spans) in layout(&runs, avail).into_iter().enumerate() {
            let mut line = self.prefix(i == 0);
            line.extend(spans);
            self.out.push(Line::from(line));
        }
        self.marker = None;
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

/// Greedy word wrap over styled runs; words may span several styles.
fn layout(runs: &[Run], avail: usize) -> Vec<Vec<Span<'static>>> {
    let mut l = Layout {
        avail: avail.max(1),
        lines: Vec::new(),
        cur: Vec::new(),
        n: 0,
    };
    let mut word: Vec<Run> = Vec::new();
    for (text, style) in runs {
        if text == "\n" {
            l.place(std::mem::take(&mut word));
            l.newline();
            continue;
        }
        for ch in text.chars() {
            if ch.is_whitespace() {
                l.place(std::mem::take(&mut word));
            } else {
                match word.last_mut() {
                    Some((s, st)) if st == style => s.push(ch),
                    _ => word.push((ch.to_string(), *style)),
                }
            }
        }
    }
    l.place(word);
    if !l.cur.is_empty() || l.lines.is_empty() {
        l.lines.push(l.cur);
    }
    l.lines
}

struct Layout {
    avail: usize,
    lines: Vec<Vec<Span<'static>>>,
    cur: Vec<Span<'static>>,
    n: usize,
}

impl Layout {
    fn newline(&mut self) {
        self.lines.push(std::mem::take(&mut self.cur));
        self.n = 0;
    }

    fn place(&mut self, word: Vec<Run>) {
        let wl: usize = word.iter().map(|(s, _)| s.chars().count()).sum();
        if wl == 0 {
            return;
        }
        if self.n > 0 && self.n + 1 + wl <= self.avail {
            self.cur.push(Span::raw(" "));
            self.n += 1;
        } else if self.n > 0 {
            self.newline();
        }
        if wl <= self.avail - self.n {
            for (s, st) in word {
                self.cur.push(Span::styled(s, st));
            }
            self.n += wl;
            return;
        }
        // Longer than a whole line: hard split.
        for (s, st) in word {
            for ch in s.chars() {
                if self.n == self.avail {
                    self.newline();
                }
                match self.cur.last_mut() {
                    Some(last) if last.style == st => last.content.to_mut().push(ch),
                    _ => self.cur.push(Span::styled(ch.to_string(), st)),
                }
                self.n += 1;
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
    fn link_shows_destination() {
        let out = render("[docs](https://x.y)", 80);
        assert_eq!(text(&out), vec!["docs (https://x.y)"]);
    }

    #[test]
    fn unterminated_input_does_not_panic() {
        let _ = render("**half and `open\n\n```\ncode", 10);
        let _ = render("", 10);
    }
}
