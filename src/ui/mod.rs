//! ratatui rendering. Reads `App`, never mutates it.

use std::io::{self, Write};
use std::path::Path;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CColor, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
};
use ratatui::Frame;

mod markdown;

use crate::agent::acp::{ElicitationRequest, FieldKind, ToolContent};
use crate::agent::install::AgentStatus;
use crate::app::{App, Confirm, FieldDraft, MentionPopup, Mode, QuestionDraft, SlashPopup};
use crate::session::{Entry, Session, Status};

/// A clickable markdown hyperlink on the current screen, in terminal cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenLink {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub url: String,
}

// Palette: ANSI names plus one 256-colour grey, so it follows the user's
// terminal theme and still works on 256-colour terminals.
const ACCENT: Color = Color::Cyan;
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const ERR: Color = Color::Red;
const ASK: Color = Color::Magenta;
const SELECTED_BG: Color = Color::Indexed(236);
const CODE_FG: Color = Color::Indexed(250);

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

const MAX_INPUT_ROWS: u16 = 6;
const MAX_TOOL_DIFF_LINES: usize = 16;
const MAX_TOOL_TEXT_LINES: usize = 10;

/// Screen areas: header, transcript, prompt box, status bar. Shared by
/// rendering and mouse hit-testing so they cannot drift apart.
fn layout(area: Rect, input_lines: usize) -> [Rect; 4] {
    // The prompt box grows with the number of lines typed, up to a cap.
    let rows = (input_lines as u16).min(MAX_INPUT_ROWS);
    Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(rows + 2),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// What is under a mouse position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Transcript,
    Input,
    Other,
}

/// The transcript's screen rectangle.
pub fn transcript_area(area: Rect, input_lines: usize) -> Rect {
    layout(area, input_lines)[1]
}

/// Map a terminal cell to the region it falls in, for a terminal of `area`.
pub fn locate(area: Rect, input_lines: usize, col: u16, row: u16) -> Region {
    let [_, transcript, input, _] = layout(area, input_lines);
    let at = |r: Rect| r.contains((col, row).into());
    if at(transcript) {
        Region::Transcript
    } else if at(input) {
        Region::Input
    } else {
        Region::Other
    }
}

pub fn render(f: &mut Frame, app: &App) {
    app.cursor_pos.set(None);
    let [header, transcript, input, status] = layout(f.area(), app.input.line_count());
    render_header(f, app, header);
    render_transcript(f, app, transcript);
    render_input(f, app, input);
    render_status(f, app, status);

    let mut modal_open = false;
    if app.mode == Mode::Picker {
        render_picker(f, app);
        modal_open = true;
    }
    if app.mode == Mode::Model {
        render_models(f, app);
        modal_open = true;
    }
    if app.mode == Mode::Effort {
        render_effort(f, app);
        modal_open = true;
    }
    if app.mode == Mode::Switcher {
        render_switcher(f, app);
        modal_open = true;
    }
    if app.mode == Mode::Help {
        render_help(f);
        modal_open = true;
    }
    if app.mode == Mode::Rename {
        render_rename(f, app);
        modal_open = true;
    }
    if let Some(s) = app.current() {
        if s.pending.is_some() {
            render_permission(f, s);
            modal_open = true;
        } else if let Some(req) = &s.pending_question {
            render_question(f, app, s, req);
            modal_open = true;
        }
    }
    if let Some(c) = app.confirm {
        render_confirm(f, c);
        modal_open = true;
    }
    if !modal_open {
        if let Some(popup) = &app.slash {
            render_slash_popup(f, popup, input);
            modal_open = true;
        } else if let Some(popup) = &app.mention {
            render_mention_popup(f, popup, input);
            modal_open = true;
        }
    }
    if modal_open {
        app.visible_links.borrow_mut().clear();
    }
}

/// Overlay OSC 8 terminal hyperlinks onto the already-drawn cells for `links`,
/// replaying exact cell styles from `buf` and restoring cursor position.
pub fn write_osc8_links<W: Write>(
    out: &mut W,
    links: &[ScreenLink],
    buf: &Buffer,
    cursor: Option<(u16, u16)>,
) -> io::Result<()> {
    if links.is_empty() {
        return Ok(());
    }
    queue!(out, Hide)?;
    for link in links {
        let url = sanitize_osc8_url(&link.url);
        if url.is_empty() || link.width == 0 || link.y >= buf.area.bottom() {
            continue;
        }
        let end_x = link.x.saturating_add(link.width).min(buf.area.right());
        if link.x >= end_x {
            continue;
        }
        queue!(out, MoveTo(link.x, link.y))?;
        write!(out, "\x1b]8;;{url}\x1b\\")?;
        let mut x = link.x;
        while x < end_x {
            let cell = &buf[(x, link.y)];
            let sym = cell.symbol();
            let step = unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            apply_cell_style(out, cell.fg, cell.bg, cell.modifier)?;
            out.write_all(sym.as_bytes())?;
            x = x.saturating_add(step);
        }
        write!(out, "\x1b]8;;\x1b\\")?;
        queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;
    }
    if let Some((cx, cy)) = cursor {
        queue!(out, MoveTo(cx, cy), Show)?;
    }
    out.flush()
}

fn sanitize_osc8_url(url: &str) -> String {
    url.chars().filter(|&c| !c.is_control()).collect()
}

fn to_crossterm_color(c: Color) -> CColor {
    match c {
        Color::Reset => CColor::Reset,
        Color::Black => CColor::Black,
        Color::Red => CColor::DarkRed,
        Color::Green => CColor::DarkGreen,
        Color::Yellow => CColor::DarkYellow,
        Color::Blue => CColor::DarkBlue,
        Color::Magenta => CColor::DarkMagenta,
        Color::Cyan => CColor::DarkCyan,
        Color::Gray => CColor::Grey,
        Color::DarkGray => CColor::DarkGrey,
        Color::LightRed => CColor::Red,
        Color::LightGreen => CColor::Green,
        Color::LightYellow => CColor::Yellow,
        Color::LightBlue => CColor::Blue,
        Color::LightMagenta => CColor::Magenta,
        Color::LightCyan => CColor::Cyan,
        Color::White => CColor::White,
        Color::Indexed(i) => CColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CColor::Rgb { r, g, b },
    }
}

fn apply_cell_style<W: Write>(
    out: &mut W,
    fg: Color,
    bg: Color,
    modifier: Modifier,
) -> io::Result<()> {
    queue!(
        out,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(to_crossterm_color(fg)),
        SetBackgroundColor(to_crossterm_color(bg))
    )?;
    if modifier.contains(Modifier::BOLD) {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    if modifier.contains(Modifier::DIM) {
        queue!(out, SetAttribute(Attribute::Dim))?;
    }
    if modifier.contains(Modifier::ITALIC) {
        queue!(out, SetAttribute(Attribute::Italic))?;
    }
    if modifier.contains(Modifier::UNDERLINED) {
        queue!(out, SetAttribute(Attribute::Underlined))?;
    }
    if modifier.contains(Modifier::REVERSED) {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        queue!(out, SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Animated frame for a running turn, advancing every 100ms.
fn spinner(s: &Session) -> &'static str {
    let ms = s.turn_started.map_or(0, |t| t.elapsed().as_millis());
    SPINNER[(ms / 100) as usize % SPINNER.len()]
}

fn elapsed(s: &Session) -> String {
    let secs = s.turn_started.map_or(0, |t| t.elapsed().as_secs());
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn status_glyph(s: Status) -> (&'static str, Color) {
    match s {
        Status::Starting => ("◌", WARN),
        Status::Idle => ("●", OK),
        Status::Running => ("●", ACCENT),
        Status::AwaitingPermission | Status::AwaitingQuestion => ("◆", ASK),
        Status::Failed => ("✗", ERR),
    }
}

fn status_label(s: &Session) -> (String, Color) {
    match s.status {
        Status::Starting => ("starting…".into(), WARN),
        Status::Idle => ("idle".into(), OK),
        Status::Running => (format!("working {}", elapsed(s)), ACCENT),
        Status::AwaitingPermission => ("needs permission".into(), ASK),
        Status::AwaitingQuestion => ("needs answer".into(), ASK),
        Status::Failed => ("failed".into(), ERR),
    }
}

/// Truncate to `width` chars, ending in `…` when cut.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Word-wrap prose to `width` chars. Words longer than a line are split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut n = 0;
        for word in raw.split(' ') {
            let wl = word.chars().count();
            if n > 0 && n + 1 + wl <= width {
                cur.push(' ');
                cur.push_str(word);
                n += 1 + wl;
                continue;
            }
            if n > 0 {
                out.push(std::mem::take(&mut cur));
            }
            let chars: Vec<char> = word.chars().collect();
            let mut chunks = chars.chunks(width).map(|c| c.iter().collect::<String>());
            let mut last = chunks.next().unwrap_or_default();
            for next in chunks {
                out.push(std::mem::replace(&mut last, next));
            }
            n = last.chars().count();
            cur = last;
        }
        out.push(cur);
    }
    out
}

/// Hard-wrap preformatted text (code) at `width` chars, keeping spaces.
fn wrap_code(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

fn worktree_state_spans(s: &Session) -> Vec<Span<'static>> {
    let Some(st) = s.worktree_state else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    if st.dirty {
        spans.push(Span::styled(
            " *",
            Style::new().fg(WARN).add_modifier(Modifier::BOLD),
        ));
    }
    if st.commits > 0 {
        spans.push(Span::styled(
            format!(" +{}", st.commits),
            Style::new().fg(OK).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::BOTTOM).border_style(DIM);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(s) = app.current() else { return };

    let mut left = vec![Span::raw("  ")];
    match &s.label {
        Some(label) => {
            left.push(Span::styled(label.as_str(), BOLD));
            left.push(Span::styled(format!(" · {}", s.spec.name), DIM));
        }
        None => {
            left.push(Span::styled(s.spec.name, BOLD));
        }
    }
    match &s.worktree {
        Some(w) => {
            left.push(Span::styled(format!("  ⎇ {}", w.branch), DIM));
            left.extend(worktree_state_spans(s));
        }
        None => {
            left.push(Span::styled("  current directory", DIM));
        }
    }
    if let Some(m) = s.model_name() {
        left.push(Span::styled(format!("  · {m}"), DIM));
    }
    if let Some(e) = s.effort_name() {
        left.push(Span::styled(format!("  · effort: {e}"), DIM));
    }
    f.render_widget(Paragraph::new(Line::from(left)), inner);

    let (label, color) = status_label(s);
    let waiting = app
        .sessions
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            *i != app.selected
                && matches!(
                    s.status,
                    Status::AwaitingPermission | Status::AwaitingQuestion
                )
        })
        .count();
    let done = app
        .sessions
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            *i != app.selected
                && s.attention
                && !matches!(
                    s.status,
                    Status::AwaitingPermission | Status::AwaitingQuestion
                )
        })
        .count();
    let mut right = Vec::new();
    if app.scroll > 0 {
        right.push(Span::styled(
            "↓ scrolled · G follows   ",
            Style::new().fg(WARN),
        ));
    }
    if waiting > 0 {
        let badge = if done == 0 {
            format!(" ◆ {waiting} waiting · s switch ")
        } else {
            format!(" ◆ {waiting} waiting ")
        };
        right.push(Span::styled(
            badge,
            Style::new()
                .fg(Color::Black)
                .bg(ASK)
                .add_modifier(Modifier::BOLD),
        ));
        right.push(Span::raw(if done == 0 { "  " } else { " " }));
    }
    if done > 0 {
        right.push(Span::styled(
            format!(" ★ {done} done · s switch "),
            Style::new()
                .fg(Color::Black)
                .bg(OK)
                .add_modifier(Modifier::BOLD),
        ));
        right.push(Span::raw("  "));
    }
    right.push(Span::styled(label, Style::new().fg(color)));
    right.push(Span::raw("  "));
    f.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        inner,
    );
}

fn tool_glyph(s: &Session, status: &str) -> (&'static str, Color) {
    match status {
        "completed" => ("✓", OK),
        "failed" => ("✗", ERR),
        _ if s.status == Status::Running => (spinner(s), WARN),
        _ => ("○", WARN),
    }
}

enum DiffLine<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// Compute a compact line diff between optional `old` and `new`, keeping up to
/// a couple of unchanged context lines around the changed span so large-file
/// edits always show the actual changes before hitting the display cap.
fn diff_lines<'a>(old: Option<&'a str>, new: &'a str) -> Vec<DiffLine<'a>> {
    const CONTEXT: usize = 2;
    const MAX_LCS_CELLS: usize = 20_000;

    let new_lines: Vec<&str> = new.lines().collect();
    let Some(old) = old.filter(|s| !s.is_empty()) else {
        return new_lines.into_iter().map(DiffLine::Added).collect();
    };
    let old_lines: Vec<&str> = old.lines().collect();

    let prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let old_rem = &old_lines[prefix..];
    let new_rem = &new_lines[prefix..];
    let suffix = old_rem
        .iter()
        .rev()
        .zip(new_rem.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let old_mid = &old_rem[..old_rem.len() - suffix];
    let new_mid = &new_rem[..new_rem.len() - suffix];

    if old_mid.is_empty() && new_mid.is_empty() {
        return old_lines.into_iter().map(DiffLine::Context).collect();
    }

    let mut out = Vec::new();
    let ctx_start = prefix.saturating_sub(CONTEXT);
    for &l in &old_lines[ctx_start..prefix] {
        out.push(DiffLine::Context(l));
    }

    let m = old_mid.len();
    let n = new_mid.len();
    if m.saturating_mul(n) <= MAX_LCS_CELLS {
        // Bottom-up LCS table so a forward walk emits removals before additions.
        let cols = n + 1;
        let mut dp = vec![0usize; (m + 1) * cols];
        for i in (0..m).rev() {
            for j in (0..n).rev() {
                dp[i * cols + j] = if old_mid[i] == new_mid[j] {
                    dp[(i + 1) * cols + (j + 1)] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < m || j < n {
            if i < m && j < n && old_mid[i] == new_mid[j] {
                out.push(DiffLine::Context(old_mid[i]));
                i += 1;
                j += 1;
            } else if i < m && (j == n || dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)]) {
                out.push(DiffLine::Removed(old_mid[i]));
                i += 1;
            } else {
                out.push(DiffLine::Added(new_mid[j]));
                j += 1;
            }
        }
    } else {
        out.extend(old_mid.iter().copied().map(DiffLine::Removed));
        out.extend(new_mid.iter().copied().map(DiffLine::Added));
    }

    let suffix_start = old_lines.len() - suffix;
    let ctx_end = (suffix_start + CONTEXT).min(old_lines.len());
    for &l in &old_lines[suffix_start..ctx_end] {
        out.push(DiffLine::Context(l));
    }
    out
}

fn render_tool_detail(detail: &[ToolContent], width: usize, out: &mut Vec<Line<'static>>) {
    let body = width.saturating_sub(6).max(1);
    for item in detail {
        match item {
            ToolContent::Diff {
                path,
                old_text,
                new_text,
            } => {
                if !path.is_empty() {
                    out.push(Line::styled(format!("    ┄ {}", fit(path, body)), DIM));
                }
                let mut rendered: Vec<Line<'static>> = Vec::new();
                for dl in diff_lines(old_text.as_deref(), new_text) {
                    let (first_gutter, style, text) = match dl {
                        DiffLine::Removed(s) => ("    - ", Style::new().fg(ERR), s),
                        DiffLine::Added(s) => ("    + ", Style::new().fg(OK), s),
                        DiffLine::Context(s) => ("      ", DIM, s),
                    };
                    for (idx, chunk) in wrap_code(text, body).into_iter().enumerate() {
                        let gutter = if idx == 0 { first_gutter } else { "      " };
                        rendered.push(Line::styled(format!("{gutter}{chunk}"), style));
                    }
                }
                let total = rendered.len();
                out.extend(rendered.into_iter().take(MAX_TOOL_DIFF_LINES));
                if total > MAX_TOOL_DIFF_LINES {
                    let more = total - MAX_TOOL_DIFF_LINES;
                    out.push(Line::styled(format!("    … (+{more} more lines)"), DIM));
                }
            }
            ToolContent::Text(t) => {
                let wrapped = wrap(t, body);
                let total = wrapped.len();
                for l in wrapped.into_iter().take(MAX_TOOL_TEXT_LINES) {
                    out.push(Line::from(vec![
                        Span::styled("    │ ", DIM),
                        Span::styled(l, Style::new().fg(CODE_FG)),
                    ]));
                }
                if total > MAX_TOOL_TEXT_LINES {
                    let more = total - MAX_TOOL_TEXT_LINES;
                    out.push(Line::styled(format!("    … (+{more} more lines)"), DIM));
                }
            }
        }
    }
}

#[cfg(test)]
fn transcript_lines(s: &Session, width: usize, show_tool_details: bool) -> Vec<Line<'static>> {
    transcript_lines_with_links(s, width, show_tool_details, None).0
}

/// Transcript body for one session, already wrapped to `width` columns, so
/// scrolling is exact and every wrapped line keeps its gutter. Also returns
/// link column ranges in transcript coordinates for OSC 8 hyperlink rendering.
fn transcript_lines_with_links(
    s: &Session,
    width: usize,
    show_tool_details: bool,
    cwd: Option<&Path>,
) -> (Vec<Line<'static>>, Vec<markdown::LinkSpan>) {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<markdown::LinkSpan> = Vec::new();
    let body = width.saturating_sub(4).max(1);
    let prose = |t: &str, gutter: &'static str, gutter_style: Style, style: Style| {
        wrap(t, body)
            .into_iter()
            .map(|l| {
                Line::from(vec![
                    Span::styled(gutter, gutter_style),
                    Span::styled(l, style),
                ])
            })
            .collect::<Vec<_>>()
    };

    for (i, e) in s.entries.iter().enumerate() {
        match e {
            Entry::User(t) => out.extend(prose(t, "  ▎ ", Style::new().fg(ACCENT), BOLD)),
            Entry::Agent(t) => {
                let base_line = out.len();
                let rendered = markdown::render_with_links(t, body, cwd);
                for l in rendered.lines {
                    let mut spans = vec![Span::raw("    ")];
                    spans.extend(l.spans);
                    out.push(Line::from(spans));
                }
                for mut ls in rendered.links {
                    ls.line += base_line;
                    ls.start_col = ls.start_col.saturating_add(4);
                    ls.end_col = ls.end_col.saturating_add(4);
                    links.push(ls);
                }
            }
            Entry::Thought(t) => {
                out.extend(prose(t, "  ┊ ", DIM, DIM.add_modifier(Modifier::ITALIC)))
            }
            Entry::Tool {
                title,
                kind,
                status,
                detail,
                ..
            } => {
                let (g, c) = tool_glyph(s, status);
                let toggle = if detail.is_empty() {
                    ""
                } else if show_tool_details {
                    " ▾"
                } else {
                    " ▸"
                };
                let kind_txt = format!("  {kind}{toggle}");
                let room = width.saturating_sub(4 + kind_txt.chars().count());
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{g} "), Style::new().fg(c)),
                    Span::raw(fit(title, room)),
                    Span::styled(kind_txt, DIM),
                ]));
                if show_tool_details && !detail.is_empty() {
                    render_tool_detail(detail, width, &mut out);
                }
            }
            Entry::Notice(t) => {
                out.extend(prose(t, "  ▎ ", Style::new().fg(ERR), Style::new().fg(ERR)))
            }
        }
        // Breathing room between entries, except inside a run of tool calls.
        let next_is_tool = matches!(s.entries.get(i + 1), Some(Entry::Tool { .. }));
        if !(matches!(e, Entry::Tool { .. }) && next_is_tool) {
            out.push(Line::from(""));
        }
    }
    (out, links)
}

/// Trailing "the agent is busy" line, so a long silent turn never looks hung.
fn activity_line(s: &Session) -> Option<Line<'static>> {
    match s.status {
        Status::Running => Some(Line::from(vec![
            Span::styled(format!("  {} ", spinner(s)), Style::new().fg(ACCENT)),
            Span::styled(format!("working… {}", elapsed(s)), DIM),
        ])),
        Status::AwaitingPermission => Some(Line::styled(
            "  ◆ waiting for your permission",
            Style::new().fg(ASK),
        )),
        Status::AwaitingQuestion => Some(Line::styled(
            "  ◆ waiting for your answer",
            Style::new().fg(ASK),
        )),
        _ => None,
    }
}

fn key_hint(key: &'static str, what: &'static str) -> Vec<Span<'static>> {
    vec![
        Span::styled(key, BOLD),
        Span::styled(format!(" {what}"), DIM),
    ]
}

fn logo_lines() -> Vec<Line<'static>> {
    let pot = Style::new().fg(ACCENT);
    vec![
        Line::styled("   ° ˚ °", DIM),
        Line::styled(" ╭───────╮", pot),
        Line::from(vec![
            Span::styled("━┥ ", pot),
            Span::styled("◕ ‿ ◕", Style::new().fg(WARN)),
            Span::styled(" ┝━", pot),
            Span::styled("  mijote", BOLD),
        ]),
        Line::styled(" ╰───────╯", pot),
    ]
}

fn render_logo(f: &mut Frame, area: Rect) {
    let lines = logo_lines();
    let r = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: (lines.len() as u16).min(area.height),
    };
    f.render_widget(Paragraph::new(lines), r);
}

fn render_welcome(f: &mut Frame, area: Rect) {
    let rows = [
        ("n", "new isolated session (git worktree)"),
        ("N", "new session in the current directory (i / Enter)"),
        ("?", "key help"),
        ("q", "quit"),
    ];
    render_logo(f, area);
    let mut lines = vec![
        Line::styled("the coding agents you already pay for, in one place", DIM),
        Line::from(""),
    ];
    for (k, what) in rows {
        let mut spans = vec![Span::styled(
            format!("{k:>3}  "),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::raw(what));
        lines.push(Line::from(spans));
    }
    let r = centered(area, 56, lines.len() as u16);
    f.render_widget(Paragraph::new(lines), r);
}

/// Display column ranges `[from, to)` of every case-insensitive occurrence of
/// `query` in `line`.
fn find_match_cols(line: &str, query: &str) -> Vec<(u16, u16)> {
    use unicode_width::UnicodeWidthChar;

    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut lowered: Vec<(char, u16, u16)> = Vec::new();
    let mut col = 0u16;
    for ch in line.chars() {
        let w = ch.width().unwrap_or(0) as u16;
        let end = col.saturating_add(w);
        for lc in ch.to_lowercase() {
            lowered.push((lc, col, end));
        }
        col = end;
    }
    let mut out = Vec::new();
    let n = needle.len();
    let mut i = 0;
    while i + n <= lowered.len() {
        if (0..n).all(|k| lowered[i + k].0 == needle[k]) {
            let from = lowered[i].1;
            let to = lowered[i + n - 1].2;
            if from < to {
                out.push((from, to));
            }
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

fn render_transcript(f: &mut Frame, app: &App, area: Rect) {
    let inner = area;
    app.transcript_height.set(inner.height);
    let Some(s) = app.current() else {
        app.max_scroll.set(0);
        app.transcript_text.borrow_mut().clear();
        app.visible_links.borrow_mut().clear();
        render_welcome(f, inner);
        return;
    };
    if s.entries.is_empty() {
        app.max_scroll.set(0);
        app.transcript_text.borrow_mut().clear();
        app.visible_links.borrow_mut().clear();
        let msg = match s.status {
            Status::Starting => "starting agent (first run may install the adapter)…",
            Status::Failed => "agent failed to start",
            _ if app.mode == Mode::Insert => "type a prompt, Enter to send, Esc for normal mode",
            _ => "press i to write a prompt, Enter to send",
        };
        render_logo(f, inner);
        let r = centered(inner, 52, 1);
        f.render_widget(Paragraph::new(Line::styled(msg, DIM)), r);
        return;
    }

    let cwd = s
        .worktree
        .as_ref()
        .map(|w| w.path.as_path())
        .unwrap_or_else(|| app.cwd());
    let (mut lines, link_spans) =
        transcript_lines_with_links(s, inner.width as usize, app.show_tool_details, Some(cwd));
    lines.extend(activity_line(s));
    let total = lines.len().min(u16::MAX as usize) as u16;
    let max_scroll = total.saturating_sub(inner.height);
    app.max_scroll.set(max_scroll);
    let from_top = max_scroll.saturating_sub(app.scroll.min(max_scroll));
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    f.render_widget(Paragraph::new(lines).scroll((from_top, 0)), inner);

    let first_line = from_top as usize;
    let last_line = first_line.saturating_add(inner.height as usize);
    let visible_links: Vec<ScreenLink> = link_spans
        .into_iter()
        .filter(|ls| ls.line >= first_line && ls.line < last_line)
        .filter_map(|ls| {
            let start = ls.start_col.min(inner.width);
            let end = ls.end_col.min(inner.width);
            if start >= end {
                return None;
            }
            Some(ScreenLink {
                x: inner.x.saturating_add(start),
                y: inner.y.saturating_add((ls.line - first_line) as u16),
                width: end - start,
                url: ls.url,
            })
        })
        .collect();
    *app.visible_links.borrow_mut() = visible_links;

    if !app.search_query.is_empty() {
        let current_line = app
            .search_current
            .and_then(|i| app.search_matches.get(i).copied());
        let match_style = Style::new().fg(Color::Black).bg(WARN);
        let current_style = Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD);
        for y in 0..inner.height {
            let line = from_top as usize + y as usize;
            let Some(t) = text.get(line) else {
                break;
            };
            let style = if current_line == Some(line) {
                current_style
            } else {
                match_style
            };
            for (from, to) in find_match_cols(t, &app.search_query) {
                for x in from..to.min(inner.width) {
                    if let Some(cell) = f.buffer_mut().cell_mut((inner.x + x, inner.y + y)) {
                        cell.set_style(style);
                    }
                }
            }
        }
    }

    if let Some(sel) = app.selection {
        for y in 0..inner.height {
            let line = from_top as usize + y as usize;
            let Some(t) = text.get(line) else {
                break;
            };
            if let Some((from, to)) = sel.content_cols_on(line, t) {
                for x in from..to.min(inner.width) {
                    if let Some(cell) = f.buffer_mut().cell_mut((inner.x + x, inner.y + y)) {
                        cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
                    }
                }
            }
        }
    }
    *app.transcript_text.borrow_mut() = text;
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let active = app.mode == Mode::Insert;
    let border = if active { ACCENT } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let prompt = |style: Style| Span::styled("› ", style);
    let lines: Vec<Line> = if app.input.is_empty() && !active {
        let hint = if app.sessions.is_empty() {
            "press i or Enter to start a session"
        } else {
            "press i to write a prompt"
        };
        vec![Line::from(vec![prompt(DIM), Span::styled(hint, DIM)])]
    } else {
        // Scroll sideways and vertically just enough to keep the cursor in view.
        let room = (inner.width as usize).saturating_sub(3).max(1);
        let (crow, ccol) = app.input.cursor_pos();
        let off = (ccol + 1).saturating_sub(room);
        let rows = app.input.line_count().min(MAX_INPUT_ROWS as usize);
        let first = (crow + 1).saturating_sub(rows);
        // Insert mode uses the terminal cursor; normal mode paints a block.
        let paint_block = app.mode == Mode::Normal && !app.sessions.is_empty();
        if active {
            let x = inner.x + 2 + (ccol - off) as u16;
            if x < inner.right() {
                let pos = (x, inner.y + (crow - first) as u16);
                f.set_cursor_position(pos);
                app.cursor_pos.set(Some(pos));
            }
        }
        app.input
            .text()
            .split('\n')
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(i, p)| {
                let lead = if i == 0 {
                    prompt(if active { Style::new().fg(ACCENT) } else { DIM })
                } else {
                    Span::raw("  ")
                };
                let chars: Vec<char> = p.chars().skip(off).collect();
                let mut spans = vec![lead];
                if paint_block && i == crow {
                    let at = (ccol - off).min(chars.len());
                    let under = chars.get(at).copied().unwrap_or(' ');
                    spans.push(Span::raw(chars[..at].iter().collect::<String>()));
                    spans.push(Span::styled(
                        under.to_string(),
                        Style::new().add_modifier(Modifier::REVERSED),
                    ));
                    spans.push(Span::raw(chars.iter().skip(at + 1).collect::<String>()));
                } else {
                    spans.push(Span::raw(chars.into_iter().collect::<String>()));
                }
                Line::from(spans)
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn search_count_span(app: &App) -> Span<'static> {
    if app.search_matches.is_empty() {
        Span::styled("no matches", Style::new().fg(ERR))
    } else {
        let cur = app.search_current.map_or(0, |i| i + 1);
        Span::styled(format!("[{cur}/{}]", app.search_matches.len()), DIM)
    }
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let (badge, badge_bg) = match app.mode {
        _ if app.leader => (" LEADER ", ASK),
        Mode::Normal => (" NORMAL ", ACCENT),
        Mode::Insert => (" INSERT ", OK),
        Mode::Picker => (" PICK ", ASK),
        Mode::Model => (" MODEL ", ASK),
        Mode::Effort => (" EFFORT ", ASK),
        Mode::Switcher => (" SESSIONS ", ASK),
        Mode::Help => (" HELP ", ASK),
        Mode::Rename => (" RENAME ", ASK),
        Mode::Search => (" SEARCH ", WARN),
    };
    let mut spans = vec![
        Span::styled(
            badge,
            Style::new()
                .fg(Color::Black)
                .bg(badge_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    let has_pending_perm = app.current().is_some_and(|s| s.pending.is_some());
    let has_pending_question = app.current().is_some_and(|s| s.pending_question.is_some());
    if app.mode == Mode::Search && !has_pending_perm && !has_pending_question {
        let prefix_w: u16 = spans
            .iter()
            .map(|s| crate::selection::text_width(s.content.as_ref()))
            .sum();
        let query_w = crate::selection::text_width(&app.search_query);
        spans.push(Span::styled("/", BOLD));
        spans.push(Span::raw(app.search_query.clone()));
        let cursor_x = area
            .x
            .saturating_add(prefix_w)
            .saturating_add(1)
            .saturating_add(query_w);
        if cursor_x < area.right() {
            let pos = (cursor_x, area.y);
            f.set_cursor_position(pos);
            app.cursor_pos.set(Some(pos));
        }
        spans.push(Span::raw("  "));
        if !app.search_query.is_empty() {
            spans.push(search_count_span(app));
            spans.push(Span::raw("  "));
        }
        spans.extend(key_hint("Enter", "jump"));
        spans.push(Span::styled(" · ", DIM));
        spans.extend(key_hint("Esc", "clear"));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let pending = app.input.pending();
    if !pending.is_empty() {
        spans.push(Span::styled(format!("{pending}  "), Style::new().fg(WARN)));
    }
    if !app.leader && app.confirm.is_none() && !has_pending_perm && !has_pending_question {
        if let Some(m) = app.message() {
            spans.push(Span::styled(m, Style::new().fg(WARN)));
            f.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
    }

    if !app.search_query.is_empty()
        && !app.leader
        && app.confirm.is_none()
        && !has_pending_perm
        && !has_pending_question
        && app.mode == Mode::Normal
    {
        spans.push(Span::styled(
            format!("/{} ", fit(&app.search_query, 24)),
            BOLD,
        ));
        spans.push(search_count_span(app));
        spans.push(Span::raw("  "));
        spans.extend(key_hint("n/N", "next/prev"));
        spans.push(Span::styled(" · ", DIM));
        spans.extend(key_hint("Esc", "clear"));
    } else {
        if !app.search_query.is_empty()
            && !app.leader
            && app.confirm.is_none()
            && !has_pending_perm
            && !has_pending_question
            && app.mode == Mode::Insert
        {
            spans.push(Span::styled(
                format!("/{} ", fit(&app.search_query, 20)),
                BOLD,
            ));
            spans.push(search_count_span(app));
            spans.push(Span::raw("  "));
        }
        let busy = app.current().is_some_and(|s| {
            matches!(
                s.status,
                Status::Running | Status::AwaitingPermission | Status::AwaitingQuestion
            )
        });
        let hints: &[(&'static str, &'static str)] = match app.mode {
            _ if has_pending_perm => &[
                ("a", "allow"),
                ("r", "reject"),
                ("1-9", "choose"),
                ("Esc", "cancel"),
            ],
            _ if has_pending_question => &[
                ("↑/↓", "option"),
                ("Space", "toggle"),
                ("Tab", "field"),
                ("Enter", "next/submit"),
                ("Esc", "cancel"),
            ],
            _ if app.confirm.is_some() => &[("y", "confirm"), ("any other key", "cancel")],
            _ if app.leader => &[
                ("n", "new"),
                ("N", "new here"),
                ("d", "close"),
                ("r", "rename"),
                ("M", "model"),
                ("e", "effort"),
                ("s", "sessions"),
                ("t", "tools"),
                ("j/k", "select"),
                ("g/G", "top/bottom"),
                ("/", "search"),
                ("m", "mouse"),
                ("?", "help"),
                ("q", "quit"),
                ("Esc", "cancel"),
            ],
            Mode::Normal if app.sessions.is_empty() => &[
                ("n", "new"),
                ("N/Enter", "new here"),
                ("m", "mouse"),
                ("?", "help"),
                ("q", "quit"),
            ],
            Mode::Normal if app.scroll > 0 => &[
                ("␣G", "follow"),
                ("^U/^D", "half-page"),
                ("/", "search"),
                ("i", "insert"),
                ("␣", "commands"),
                ("?", "help"),
            ],
            Mode::Normal if busy => &[
                ("^C", "cancel turn"),
                ("␣", "commands"),
                ("^U/^D", "half-page"),
                ("/", "search"),
                ("i", "insert"),
                ("?", "help"),
            ],
            Mode::Normal => &[
                ("i", "insert"),
                ("␣", "commands"),
                ("^U/^D", "half-page"),
                ("/", "search"),
                ("?", "help"),
                ("^C", "quit"),
            ],
            Mode::Insert if app.slash.is_some() => &[
                ("Tab", "complete"),
                ("Enter", "run"),
                ("↑/↓", "select"),
                ("Esc", "close"),
            ],
            Mode::Insert if app.mention.is_some() => &[
                ("Tab/Enter", "insert file"),
                ("↑/↓", "select"),
                ("Esc", "close"),
            ],
            Mode::Insert if busy => &[
                ("Enter", "send when idle"),
                ("^C", "cancel turn"),
                ("^A", "commands"),
                ("Esc", "normal"),
            ],
            Mode::Insert => &[
                ("Enter", "send"),
                ("S-Enter", "newline"),
                ("/", "slash"),
                ("@", "files"),
                ("^A", "commands"),
                ("Esc", "normal"),
            ],
            Mode::Picker | Mode::Model | Mode::Effort => {
                &[("j/k", "move"), ("Enter/1-9", "choose"), ("Esc", "close")]
            }
            Mode::Switcher => &[
                ("j/k", "move"),
                ("Enter/1-9", "switch"),
                ("r", "rename"),
                ("Esc", "close"),
            ],
            Mode::Help => &[("Esc/q/?", "close")],
            Mode::Rename => &[("Enter", "save"), ("^U", "clear"), ("Esc", "cancel")],
            Mode::Search => &[],
        };
        for (i, (k, what)) in hints.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.extend(key_hint(k, what));
        }
    }

    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let cwd = app.cwd().display().to_string();
    // Only show the cwd when the hints leave room for it.
    let show_cwd = used + cwd.chars().count() + 3 <= area.width as usize;
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    if show_cwd {
        f.render_widget(
            Paragraph::new(Line::styled(format!("{cwd} "), DIM)).alignment(Alignment::Right),
            area,
        );
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [_, v, _] = Layout::new(
        Direction::Vertical,
        [
            Constraint::Fill(1),
            Constraint::Length(h.min(area.height)),
            Constraint::Fill(1),
        ],
    )
    .areas(area);
    let [_, c, _] = Layout::new(
        Direction::Horizontal,
        [
            Constraint::Fill(1),
            Constraint::Length(w.min(area.width)),
            Constraint::Fill(1),
        ],
    )
    .areas(v);
    c
}

/// Rounded modal frame with a coloured border and a hint line on the bottom edge.
fn modal(title: &'static str, color: Color, hint: Line<'static>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title, Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
        .title_bottom(hint.right_aligned())
        .padding(Padding::new(2, 2, 1, 1))
}

fn hint_line(pairs: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, what)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", DIM));
        }
        spans.extend(key_hint(k, what));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn picker_status_badge(status: AgentStatus) -> (&'static str, Color) {
    match status {
        AgentStatus::Ready => ("● installed", OK),
        AgentStatus::NeedsLogin => ("○ needs login", WARN),
        AgentStatus::NeedsInstall => ("↓ will install", ACCENT),
        AgentStatus::Missing => ("✗ not installed", ERR),
    }
}

fn render_picker(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 58, app.picker.len() as u16 + 4);
    let inner_w = area.width.saturating_sub(8) as usize;
    let title = if app.picker_isolated {
        "new session · worktree"
    } else {
        "new session · current directory"
    };
    let items: Vec<ListItem> = app
        .picker
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let idx = format!("{} ", i + 1);
            let mut spans = vec![Span::styled(idx.clone(), DIM)];
            if let Some(status) = app.picker_status.get(i).copied() {
                let (badge, color) = picker_status_badge(status);
                let idx_len = idx.chars().count();
                let badge_len = badge.chars().count();
                let name_room = inner_w.saturating_sub(idx_len + badge_len + 2);
                let name = if name_room > 0 {
                    fit(a.name, name_room)
                } else {
                    a.name.to_string()
                };
                let gap = inner_w
                    .saturating_sub(idx_len + name.chars().count() + badge_len)
                    .max(2);
                spans.push(Span::raw(name));
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(Span::styled(badge, Style::new().fg(color)));
            } else {
                spans.push(Span::raw(a.name));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.picker_cursor));
    let list = List::new(items)
        .block(modal(
            title,
            ACCENT,
            hint_line(&[("Enter", "choose"), ("Esc", "close")]),
        ))
        .highlight_symbol("▌ ")
        .highlight_style(BOLD.fg(ACCENT).bg(SELECTED_BG));
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
}

fn render_switcher(f: &mut Frame, app: &App) {
    let rows = app.sessions.len() as u16;
    let area = centered(f.area(), 74, rows + 4);
    // 2 borders + 4 horizontal padding + 2 highlight symbol ("▌ ").
    let inner_w = area.width.saturating_sub(8) as usize;
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (g, c) = status_glyph(s.status);
            let (glyph, glyph_style) = if matches!(
                s.status,
                Status::AwaitingPermission | Status::AwaitingQuestion
            ) {
                ("◆", Style::new().fg(ASK).add_modifier(Modifier::BOLD))
            } else if s.attention {
                ("★", Style::new().fg(OK).add_modifier(Modifier::BOLD))
            } else if s.status == Status::Running {
                (spinner(s), Style::new().fg(c))
            } else {
                (g, Style::new().fg(c))
            };

            let wt_spans = worktree_state_spans(s);
            let wt_w: usize = wt_spans.iter().map(|sp| sp.content.chars().count()).sum();

            let trailing: Option<(String, Style)> = if s.status == Status::AwaitingPermission {
                Some((
                    "◆ needs permission".into(),
                    Style::new().fg(ASK).add_modifier(Modifier::BOLD),
                ))
            } else if s.status == Status::AwaitingQuestion {
                Some((
                    "◆ needs answer".into(),
                    Style::new().fg(ASK).add_modifier(Modifier::BOLD),
                ))
            } else if s.attention {
                Some((
                    "★ done".into(),
                    Style::new().fg(OK).add_modifier(Modifier::BOLD),
                ))
            } else if s.status != Status::Idle {
                Some((status_label(s).0, Style::new().fg(c)))
            } else {
                None
            };
            let trailing_w = trailing.as_ref().map_or(0, |(t, _)| 2 + t.chars().count());

            let idx_str = format!("{} ", i + 1);
            let avail = inner_w.saturating_sub(idx_str.chars().count() + 2 + wt_w + trailing_w);

            let title = s.display_title();
            let place = match &s.worktree {
                Some(wt) => format!("⎇ {}", wt.branch),
                None => "current directory".to_string(),
            };
            let title_w = title.chars().count();
            let place_w = place.chars().count();
            let (title_room, place_room) = if title_w + 2 + place_w <= avail {
                (title_w, place_w)
            } else {
                let tr = avail
                    .saturating_sub(place_w.min(avail / 2) + 2)
                    .max(8)
                    .min(avail);
                let shown_t = title_w.min(tr);
                let pr = avail.saturating_sub(shown_t + 2);
                (tr, pr)
            };

            let mut spans = vec![
                Span::styled(idx_str, DIM),
                Span::styled(format!("{glyph} "), glyph_style),
            ];
            if title_w <= title_room {
                match &s.label {
                    Some(lbl) => {
                        spans.push(Span::raw(lbl.clone()));
                        spans.push(Span::styled(format!(" · {}", s.spec.name), DIM));
                    }
                    None => spans.push(Span::raw(s.spec.name)),
                }
            } else {
                spans.push(Span::raw(fit(&title, title_room)));
            }
            if place_room >= 3 {
                let place_style = if s.label.is_none() && s.worktree.is_some() {
                    Style::new().fg(CODE_FG)
                } else {
                    DIM
                };
                spans.push(Span::styled(
                    format!("  {}", fit(&place, place_room)),
                    place_style,
                ));
            }
            spans.extend(wt_spans);
            if let Some((text, style)) = trailing {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(text, style));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.switcher_cursor));
    let list = List::new(items)
        .block(modal(
            "sessions",
            ACCENT,
            hint_line(&[("Enter", "switch"), ("r", "rename"), ("Esc", "close")]),
        ))
        .highlight_symbol("▌ ")
        .highlight_style(BOLD.fg(ACCENT).bg(SELECTED_BG));
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
}

fn render_rename(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 52, 5);
    let block = modal(
        "rename session",
        ACCENT,
        hint_line(&[("Enter", "save"), ("Esc", "cancel")]),
    );
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let room = (inner.width as usize).saturating_sub(3).max(1);
    let len = app.rename_buf.chars().count();
    let off = (len + 1).saturating_sub(room);
    let visible: String = app.rename_buf.chars().skip(off).collect();
    let line = if app.rename_buf.is_empty() {
        Line::from(vec![
            Span::styled("› ", Style::new().fg(ACCENT)),
            Span::styled("session label (empty clears)", DIM),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ", Style::new().fg(ACCENT)),
            Span::raw(visible),
        ])
    };
    f.render_widget(Paragraph::new(line), inner);
    let x = inner.x + 2 + (len - off) as u16;
    if x < inner.right() && inner.height > 0 {
        let pos = (x, inner.y);
        f.set_cursor_position(pos);
        app.cursor_pos.set(Some(pos));
    }
}

fn render_models(f: &mut Frame, app: &App) {
    let Some(s) = app.current() else { return };
    let current = s.model();
    let area = centered(f.area(), 52, s.models.available.len() as u16 + 4);
    let items: Vec<ListItem> = s
        .models
        .available
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mark = if current == Some(m.id.as_str()) {
                " ✓"
            } else {
                ""
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", i + 1), DIM),
                Span::raw(format!("{}{mark}", m.name)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.model_cursor));
    let list = List::new(items)
        .block(modal(
            "model",
            ACCENT,
            hint_line(&[("Enter", "choose"), ("Esc", "close")]),
        ))
        .highlight_symbol("▌ ")
        .highlight_style(BOLD.fg(ACCENT).bg(SELECTED_BG));
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
}

fn render_effort(f: &mut Frame, app: &App) {
    let Some(s) = app.current() else { return };
    let current = s.effort();
    let has_desc = s.effort.available.iter().any(|e| e.description.is_some());
    let width = if has_desc { 62 } else { 44 };
    let area = centered(f.area(), width, s.effort.available.len() as u16 + 4);
    let inner_w = area.width.saturating_sub(8) as usize;
    let items: Vec<ListItem> = s
        .effort
        .available
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mark = if current == Some(e.id.as_str()) {
                " ✓"
            } else {
                ""
            };
            let idx = format!("{} ", i + 1);
            let label = format!("{}{mark}", e.name);
            let mut spans = vec![Span::styled(idx.clone(), DIM), Span::raw(label.clone())];
            if let Some(desc) = &e.description {
                let used = idx.chars().count() + label.chars().count() + 2;
                let room = inner_w.saturating_sub(used);
                if room >= 4 {
                    spans.push(Span::styled(format!("  {}", fit(desc, room)), DIM));
                }
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.effort_cursor));
    let list = List::new(items)
        .block(modal(
            "effort",
            ACCENT,
            hint_line(&[("Enter", "choose"), ("Esc", "close")]),
        ))
        .highlight_symbol("▌ ")
        .highlight_style(BOLD.fg(ACCENT).bg(SELECTED_BG));
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
}

fn render_help(f: &mut Frame) {
    let heading = |title: &'static str| {
        Line::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
    };
    let row = |key: &'static str, what: &'static str| {
        Line::from(vec![
            Span::styled(format!("  {key:<14} "), BOLD),
            Span::raw(what),
        ])
    };
    let pair = |k1: &'static str, d1: &'static str, k2: &'static str, d2: &'static str| {
        Line::from(vec![
            Span::styled(format!("  {k1:<9} "), BOLD),
            Span::raw(format!("{d1:<20}")),
            Span::styled(format!("{k2:<7} "), BOLD),
            Span::raw(d2),
        ])
    };

    let lines = vec![
        heading("prompt (vim)"),
        row("i / Enter", "insert mode, Esc for normal"),
        row("S-Enter / C-j", "insert newline (also Alt-Enter)"),
        row("/  @", "slash commands & workspace file mentions"),
        row("d c y u C-r", "operators, undo/redo & vim motions"),
        Line::from(""),
        heading("commands (Space in normal · C-a anywhere)"),
        pair("n", "new worktree", "N", "new here"),
        pair("s / Tab", "switcher", "j / k", "next / prev"),
        pair("d", "close session", "r", "rename session"),
        pair("M", "model (/model)", "e", "effort (/effort)"),
        pair("t", "tool details", "m", "toggle mouse"),
        pair("g / G", "top / bottom", "/", "search"),
        pair("?", "help", "q", "quit"),
        Line::from(""),
        heading("session & scroll"),
        row("PgUp/Dn C-u/d", "scroll transcript (half-page in normal)"),
        row("/  n / N", "search transcript, next / prev match"),
        row("drag", "select and copy transcript text"),
        row("C-c / C-d", "cancel turn or quit / quit (insert/modal)"),
    ];

    let area = centered(f.area(), 68, lines.len() as u16 + 4);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(modal("help", ACCENT, hint_line(&[("Esc/q/?", "close")]))),
        area,
    );
}

fn render_slash_popup(f: &mut Frame, popup: &SlashPopup, input: Rect) {
    const MAX_ROWS: usize = 10;
    let max_avail_h = input.y.saturating_sub(2);
    if max_avail_h < 3 || input.width < 24 {
        return;
    }
    let rows = popup.matches.len().clamp(1, MAX_ROWS) as u16;
    let h = (rows + 2).min(max_avail_h);
    let visible_rows = h.saturating_sub(2) as usize;
    let w = input.width.clamp(48, 88).min(input.width);
    let area = Rect {
        x: input.x,
        y: input.y.saturating_sub(h),
        width: w,
        height: h,
    };

    let title = if popup.matches.is_empty() {
        format!(" /{} · no matches ", fit(&popup.query, 24))
    } else if popup.query.is_empty() {
        format!(" / commands ({}) ", popup.total_commands)
    } else {
        format!(
            " /{} ({}/{}) ",
            fit(&popup.query, 24),
            popup.matches.len(),
            popup.total_commands
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(Span::styled(
            title,
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            hint_line(&[
                ("Tab", "complete"),
                ("Enter", "run"),
                ("↑/↓", "move"),
                ("Esc", "close"),
            ])
            .right_aligned(),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);

    let items: Vec<Line<'static>> = if popup.matches.is_empty() {
        vec![Line::styled("  no matching commands", DIM)]
    } else {
        let start = if popup.cursor >= visible_rows {
            popup.cursor + 1 - visible_rows
        } else {
            0
        };
        let room = inner.width.saturating_sub(2) as usize;
        let visible: Vec<_> = popup
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible_rows)
            .collect();
        let max_name_w = visible
            .iter()
            .map(|(_, m)| 1 + m.command.name.chars().count())
            .max()
            .unwrap_or(1);
        let name_col = max_name_w.min((room / 3).max(10)).min(room);

        visible
            .into_iter()
            .map(|(idx, m)| {
                let selected = idx == popup.cursor;
                let base_style = if selected {
                    Style::new().bg(SELECTED_BG)
                } else {
                    Style::new()
                };
                let match_style = if selected {
                    BOLD.fg(ACCENT).bg(SELECTED_BG)
                } else {
                    BOLD.fg(ACCENT)
                };
                let dim_style = if selected { DIM.bg(SELECTED_BG) } else { DIM };
                let local_style = if selected {
                    Style::new().fg(ACCENT).bg(SELECTED_BG)
                } else {
                    Style::new().fg(ACCENT)
                };

                let mut spans = vec![Span::styled(
                    if selected { "▌ " } else { "  " },
                    if selected {
                        Style::new().fg(ACCENT).bg(SELECTED_BG)
                    } else {
                        base_style
                    },
                )];

                // Leading `/` + highlighted command name.
                let mut used = 0;
                if room > 0 {
                    spans.push(Span::styled("/", base_style));
                    used += 1;
                }
                let name_budget = name_col.saturating_sub(1);
                let chars: Vec<char> = m.command.name.chars().collect();
                let show_len = chars.len().min(name_budget);
                let mut run = String::new();
                let mut run_hl = false;
                for (ci, &ch) in chars.iter().take(show_len).enumerate() {
                    let hl = m.positions.binary_search(&ci).is_ok();
                    if !run.is_empty() && hl != run_hl {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            if run_hl { match_style } else { base_style },
                        ));
                    }
                    run_hl = hl;
                    run.push(ch);
                }
                if !run.is_empty() {
                    spans.push(Span::styled(
                        run,
                        if run_hl { match_style } else { base_style },
                    ));
                }
                used += show_len;

                if used < name_col {
                    let pad = name_col - used;
                    spans.push(Span::styled(" ".repeat(pad), base_style));
                    used += pad;
                }

                // Description, optional input hint, and `local` tag.
                let rem = room.saturating_sub(used);
                if rem >= 4 {
                    spans.push(Span::styled("  ", base_style));
                    used += 2;
                    let mut avail = room.saturating_sub(used);
                    let show_local = m.command.local && avail >= 14;
                    let local_reserve = if show_local { 8 } else { 0 };
                    avail = avail.saturating_sub(local_reserve);

                    let hint_fmt = m.command.input_hint.as_deref().map(|h| {
                        if h.starts_with('[') || h.starts_with('<') {
                            h.to_string()
                        } else {
                            format!("<{h}>")
                        }
                    });

                    if !m.command.description.is_empty() && avail > 0 {
                        let shown = fit(&m.command.description, avail);
                        let w = shown.chars().count();
                        spans.push(Span::styled(shown, dim_style));
                        used += w;
                        avail = avail.saturating_sub(w);
                    }

                    if let Some(hint) = hint_fmt {
                        let sep = if !m.command.description.is_empty() {
                            "  "
                        } else {
                            ""
                        };
                        let needed = sep.len() + 4;
                        if avail >= needed {
                            if !sep.is_empty() {
                                spans.push(Span::styled(sep, base_style));
                                used += sep.len();
                                avail = avail.saturating_sub(sep.len());
                            }
                            let shown = fit(&hint, avail);
                            let w = shown.chars().count();
                            spans.push(Span::styled(shown, dim_style));
                            used += w;
                        }
                    }

                    if show_local {
                        spans.push(Span::styled(" · ", dim_style));
                        spans.push(Span::styled("local", local_style));
                        used += 8;
                    }
                }

                if selected && used < room {
                    spans.push(Span::styled(" ".repeat(room - used), base_style));
                }
                Line::from(spans)
            })
            .collect()
    };

    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(items).block(block), area);
}

fn render_mention_popup(f: &mut Frame, popup: &MentionPopup, input: Rect) {
    const MAX_ROWS: usize = 8;
    let max_avail_h = input.y.saturating_sub(2);
    if max_avail_h < 3 || input.width < 20 {
        return;
    }
    let rows = popup.matches.len().clamp(1, MAX_ROWS) as u16;
    let h = (rows + 2).min(max_avail_h);
    let visible_rows = h.saturating_sub(2) as usize;
    let w = input.width.clamp(36, 76).min(input.width);
    let area = Rect {
        x: input.x,
        y: input.y.saturating_sub(h),
        width: w,
        height: h,
    };

    let title = if popup.matches.is_empty() {
        format!(" @{} · no matches ", fit(&popup.query, 24))
    } else if popup.query.is_empty() {
        format!(" @ files ({}) ", popup.total_files)
    } else {
        format!(
            " @{} ({}/{}) ",
            fit(&popup.query, 24),
            popup.matches.len(),
            popup.total_files
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(Span::styled(
            title,
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            hint_line(&[("Tab/Enter", "insert"), ("↑/↓", "move"), ("Esc", "close")])
                .right_aligned(),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);

    let items: Vec<Line<'static>> = if popup.matches.is_empty() {
        vec![Line::styled("  no matching files in workspace", DIM)]
    } else {
        let start = if popup.cursor >= visible_rows {
            popup.cursor + 1 - visible_rows
        } else {
            0
        };
        let room = inner.width.saturating_sub(2) as usize;
        popup
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible_rows)
            .map(|(idx, m)| {
                let selected = idx == popup.cursor;
                let base_style = if selected {
                    Style::new().bg(SELECTED_BG)
                } else {
                    Style::new()
                };
                let match_style = if selected {
                    BOLD.fg(ACCENT).bg(SELECTED_BG)
                } else {
                    BOLD.fg(ACCENT)
                };
                let mut spans = vec![Span::styled(
                    if selected { "▌ " } else { "  " },
                    if selected {
                        Style::new().fg(ACCENT).bg(SELECTED_BG)
                    } else {
                        base_style
                    },
                )];
                let chars: Vec<char> = m.path.chars().collect();
                let show_len = chars.len().min(room);
                let mut run = String::new();
                let mut run_hl = false;
                for (ci, &ch) in chars.iter().take(show_len).enumerate() {
                    let hl = m.positions.binary_search(&ci).is_ok();
                    if !run.is_empty() && hl != run_hl {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            if run_hl { match_style } else { base_style },
                        ));
                    }
                    run_hl = hl;
                    run.push(ch);
                }
                if !run.is_empty() {
                    spans.push(Span::styled(
                        run,
                        if run_hl { match_style } else { base_style },
                    ));
                }
                if selected && show_len < room {
                    spans.push(Span::styled(" ".repeat(room - show_len), base_style));
                }
                Line::from(spans)
            })
            .collect()
    };

    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(items).block(block), area);
}

fn render_confirm(f: &mut Frame, c: Confirm) {
    let text = match c {
        Confirm::CloseSession => "Close this session? Its transcript will be lost.",
        Confirm::Quit => "An agent is mid-turn. Quit and stop it?",
    };
    let area = centered(f.area(), 56, 7);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(modal(
                "confirm",
                WARN,
                hint_line(&[("y/Enter", "yes"), ("any other key", "no")]),
            )),
        area,
    );
}

const MAX_PERMISSION_WIDTH: u16 = 78;
const MAX_PERMISSION_DETAIL_LINES: usize = 12;

fn permission_kind_label(kind: &str) -> Option<&str> {
    match kind {
        "allow_once" => Some("allow once"),
        "allow_always" => Some("always allow"),
        "reject_once" => Some("reject once"),
        "reject_always" => Some("always reject"),
        "" => None,
        other => Some(other),
    }
}

fn render_permission(f: &mut Frame, s: &Session) {
    let Some(req) = &s.pending else { return };
    let term = f.area();
    let max_w = if term.width > 4 {
        (term.width - 4).min(MAX_PERMISSION_WIDTH)
    } else {
        term.width
    };
    let max_h = term.height.saturating_sub(4);
    let inner_w = (max_w.saturating_sub(6) as usize).max(1);
    let inner_max_h = max_h.saturating_sub(4) as usize;

    let title = req
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or(&req.tool_call_id);
    let title_line = if let Some(kind) = &req.kind {
        let badge = format!("[{kind}]");
        let room = inner_w.saturating_sub(badge.chars().count() + 2).max(1);
        Line::from(vec![
            Span::styled(fit(title, room), BOLD),
            Span::raw("  "),
            Span::styled(badge, Style::new().fg(WARN)),
        ])
    } else {
        Line::styled(fit(title, inner_w), BOLD)
    };

    let mut detail_lines: Vec<Line<'static>> = Vec::new();
    for loc in &req.locations {
        detail_lines.push(Line::styled(fit(loc, inner_w), Style::new().fg(CODE_FG)));
    }
    if let Some(cmd) = &req.command {
        let already_in_locations = req
            .locations
            .iter()
            .any(|l| l == cmd || l.starts_with(&format!("{cmd}:")));
        if !already_in_locations {
            let is_file_tool = matches!(
                req.kind.as_deref(),
                Some("edit" | "read" | "delete" | "move")
            );
            if is_file_tool {
                detail_lines.push(Line::styled(fit(cmd, inner_w), Style::new().fg(CODE_FG)));
            } else {
                let cmd_w = inner_w.saturating_sub(2).max(1);
                let mut first = true;
                for raw in cmd.lines() {
                    for chunk in wrap_code(raw, cmd_w) {
                        let prefix = if first { "$ " } else { "  " };
                        first = false;
                        detail_lines.push(Line::styled(
                            format!("{prefix}{chunk}"),
                            Style::new().fg(ACCENT),
                        ));
                    }
                }
            }
        }
    }
    for diff in &req.diffs {
        let show_path = !diff.path.is_empty()
            && (req.diffs.len() > 1
                || !req
                    .locations
                    .iter()
                    .any(|l| l == &diff.path || l.starts_with(&format!("{}:", diff.path))));
        if show_path {
            detail_lines.push(Line::styled(fit(&diff.path, inner_w), DIM));
        }
        for dl in diff_lines(diff.old_text.as_deref(), &diff.new_text) {
            let (text, style) = match dl {
                DiffLine::Context(l) => (format!("  {l}"), DIM),
                DiffLine::Removed(l) => (format!("- {l}"), Style::new().fg(ERR)),
                DiffLine::Added(l) => (format!("+ {l}"), Style::new().fg(OK)),
            };
            detail_lines.push(Line::styled(fit(&text, inner_w), style));
        }
    }
    if req.diffs.is_empty() {
        if let Some(preview) = &req.preview_text {
            for raw in preview.lines() {
                for chunk in wrap_code(raw, inner_w) {
                    detail_lines.push(Line::styled(chunk, Style::new().fg(CODE_FG)));
                }
            }
        }
    }

    let mut lines = vec![title_line];
    let fixed_rows = 1 + 1 + req.options.len();
    if !detail_lines.is_empty() {
        let avail = inner_max_h.saturating_sub(fixed_rows);
        if avail >= 2 {
            let cap = (avail - 1).min(MAX_PERMISSION_DETAIL_LINES);
            if detail_lines.len() > cap {
                let keep = cap.saturating_sub(1);
                let remaining = detail_lines.len() - keep;
                let noun = if remaining == 1 { "line" } else { "lines" };
                detail_lines.truncate(keep);
                detail_lines.push(Line::styled(format!("… ({remaining} more {noun})"), DIM));
            }
            lines.push(Line::from(""));
            lines.extend(detail_lines);
        }
    }
    lines.push(Line::from(""));

    let allow_idx = req.allow_index();
    let reject_idx = req.reject_index();
    let has_shortcut = allow_idx.is_some() || reject_idx.is_some();
    for (i, o) in req.options.iter().enumerate() {
        let mut spans = vec![Span::styled(
            format!("{} ", i + 1),
            Style::new().fg(ASK).add_modifier(Modifier::BOLD),
        )];
        if Some(i) == allow_idx {
            spans.push(Span::styled(
                "[a] ",
                Style::new().fg(OK).add_modifier(Modifier::BOLD),
            ));
        } else if Some(i) == reject_idx {
            spans.push(Span::styled(
                "[r] ",
                Style::new().fg(ERR).add_modifier(Modifier::BOLD),
            ));
        } else if has_shortcut {
            spans.push(Span::raw("    "));
        }
        spans.push(Span::raw(o.name.clone()));
        if let Some(label) = permission_kind_label(&o.kind) {
            let name_norm = o.name.trim();
            let raw_spaced = o.kind.replace('_', " ");
            if !name_norm.eq_ignore_ascii_case(label)
                && !name_norm.eq_ignore_ascii_case(&raw_spaced)
            {
                spans.push(Span::styled(format!("  {label}"), DIM));
            }
        }
        lines.push(Line::from(spans));
    }

    let hint = hint_line(&[
        ("a", "allow"),
        ("r", "reject"),
        ("1-9", "choose"),
        ("Esc", "cancel"),
    ]);
    let min_w = (hint.width() as u16 + 4).max(56).min(max_w);
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (content_w + 6).clamp(min_w, max_w);
    let height = (lines.len() as u16 + 4).min(max_h);
    let area = centered(term, width, height);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(modal("permission requested", ASK, hint)),
        area,
    );
}

fn render_question(f: &mut Frame, app: &App, s: &Session, req: &ElicitationRequest) {
    let fallback;
    let draft = match &app.question_draft {
        Some(d) if d.session_id == s.id && d.values.len() == req.fields.len() => d,
        _ => {
            fallback = QuestionDraft::from_request(s.id, req);
            &fallback
        }
    };

    let max_w = f.area().width.saturating_sub(4).clamp(44, 80);
    let inner_w = max_w.saturating_sub(6) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focus_line: usize = 0;
    let mut text_cursor: Option<(usize, u16)> = None;

    if !req.message.is_empty() {
        for l in wrap(&req.message, inner_w) {
            lines.push(Line::styled(l, BOLD));
        }
    }

    for (fi, field) in req.fields.iter().enumerate() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let active = fi == draft.field_idx;
        let gutter = if active {
            Span::styled("▎ ", Style::new().fg(ACCENT))
        } else {
            Span::raw("  ")
        };
        let body_w = inner_w.saturating_sub(2).max(1);

        if let Some(title) = &field.title {
            let mut spans = vec![gutter.clone()];
            spans.push(Span::styled(
                fit(title, body_w.saturating_sub(2)),
                if active { BOLD.fg(ACCENT) } else { BOLD },
            ));
            if field.required {
                spans.push(Span::styled(" *", Style::new().fg(ERR)));
            }
            lines.push(Line::from(spans));
        }
        if let Some(desc) = &field.description {
            for l in wrap(desc, body_w) {
                lines.push(Line::from(vec![gutter.clone(), Span::styled(l, DIM)]));
            }
        }

        match (&field.kind, draft.values.get(fi)) {
            (FieldKind::SingleSelect { options, .. }, Some(FieldDraft::Single(sel))) => {
                for (oi, opt) in options.iter().enumerate() {
                    let focused = active && oi == draft.option_idx;
                    if focused {
                        focus_line = lines.len();
                    }
                    let picked = *sel == Some(oi);
                    let mark = if picked { "◉ " } else { "○ " };
                    let row_bg = if focused {
                        Style::new().bg(SELECTED_BG)
                    } else {
                        Style::new()
                    };
                    let mark_style = if picked {
                        Style::new().fg(OK).add_modifier(Modifier::BOLD)
                    } else if focused {
                        Style::new().fg(ACCENT)
                    } else {
                        DIM
                    };
                    let mut spans = vec![
                        gutter.clone(),
                        Span::styled(mark, mark_style.patch(row_bg)),
                        Span::styled(format!("{}. ", oi + 1), DIM.patch(row_bg)),
                        Span::styled(
                            opt.label.clone(),
                            if focused { BOLD.patch(row_bg) } else { row_bg },
                        ),
                    ];
                    if let Some(desc) = &opt.description {
                        let used =
                            2 + 2 + format!("{}. ", oi + 1).len() + opt.label.chars().count();
                        let rem = inner_w.saturating_sub(used + 3);
                        if rem >= 8 {
                            spans.push(Span::styled(
                                format!(" — {}", fit(desc, rem)),
                                DIM.patch(row_bg),
                            ));
                        }
                    }
                    lines.push(Line::from(spans));
                }
            }
            (FieldKind::MultiSelect { options, .. }, Some(FieldDraft::Multi(checks))) => {
                for (oi, opt) in options.iter().enumerate() {
                    let focused = active && oi == draft.option_idx;
                    if focused {
                        focus_line = lines.len();
                    }
                    let checked = checks.get(oi).copied().unwrap_or(false);
                    let mark = if checked { "[x] " } else { "[ ] " };
                    let row_bg = if focused {
                        Style::new().bg(SELECTED_BG)
                    } else {
                        Style::new()
                    };
                    let mark_style = if checked {
                        Style::new().fg(OK).add_modifier(Modifier::BOLD)
                    } else if focused {
                        Style::new().fg(ACCENT)
                    } else {
                        DIM
                    };
                    let mut spans = vec![
                        gutter.clone(),
                        Span::styled(mark, mark_style.patch(row_bg)),
                        Span::styled(format!("{}. ", oi + 1), DIM.patch(row_bg)),
                        Span::styled(
                            opt.label.clone(),
                            if focused { BOLD.patch(row_bg) } else { row_bg },
                        ),
                    ];
                    if let Some(desc) = &opt.description {
                        let used =
                            2 + 4 + format!("{}. ", oi + 1).len() + opt.label.chars().count();
                        let rem = inner_w.saturating_sub(used + 3);
                        if rem >= 8 {
                            spans.push(Span::styled(
                                format!(" — {}", fit(desc, rem)),
                                DIM.patch(row_bg),
                            ));
                        }
                    }
                    lines.push(Line::from(spans));
                }
            }
            (FieldKind::Bool { .. }, Some(FieldDraft::Bool(val))) => {
                if active {
                    focus_line = lines.len();
                }
                let row_bg = if active {
                    Style::new().bg(SELECTED_BG)
                } else {
                    Style::new()
                };
                let (mark, label, color) = if *val {
                    ("[x] ", "yes", OK)
                } else {
                    ("[ ] ", "no", Color::DarkGray)
                };
                lines.push(Line::from(vec![
                    gutter.clone(),
                    Span::styled(
                        mark,
                        Style::new()
                            .fg(color)
                            .add_modifier(Modifier::BOLD)
                            .patch(row_bg),
                    ),
                    Span::styled(label, if active { BOLD.patch(row_bg) } else { row_bg }),
                ]));
            }
            (FieldKind::Text { .. } | FieldKind::Number { .. }, Some(FieldDraft::Text(buf))) => {
                let line_idx = lines.len();
                if active {
                    focus_line = line_idx;
                }
                let room = body_w.saturating_sub(3).max(1);
                let len = buf.chars().count();
                let off = (len + 1).saturating_sub(room);
                let visible: String = buf.chars().skip(off).collect();
                let prompt_style = if active { Style::new().fg(ACCENT) } else { DIM };
                if buf.is_empty() && !active {
                    let ph = match field.kind {
                        FieldKind::Number { integer: true, .. } => "integer (optional)",
                        FieldKind::Number { integer: false, .. } => "number (optional)",
                        _ if field.required => "type answer…",
                        _ => "type custom answer (optional)…",
                    };
                    lines.push(Line::from(vec![
                        gutter.clone(),
                        Span::styled("› ", prompt_style),
                        Span::styled(ph, DIM),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        gutter.clone(),
                        Span::styled("› ", prompt_style),
                        Span::raw(visible),
                    ]));
                }
                if active {
                    let col = 4 + (len - off) as u16;
                    text_cursor = Some((line_idx, col));
                }
            }
            _ => {}
        }
    }

    if let Some(err) = &draft.error {
        lines.push(Line::from(""));
        lines.push(Line::styled(format!("  {err}"), Style::new().fg(ERR)));
    }

    let hint = hint_line(&[
        ("↑/↓", "move"),
        ("Space", "toggle"),
        ("Tab", "field"),
        ("Enter", "submit"),
        ("Esc", "cancel"),
    ]);
    let min_w = (hint.width() as u16 + 4).max(56).min(max_w);
    let content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let w = (content_w + 6).clamp(min_w, max_w);
    let max_h = f.area().height.saturating_sub(4).max(7);
    let h = (lines.len() as u16 + 4).min(max_h);
    let area = centered(f.area(), w, h);
    let block = modal("question", ASK, hint);
    let inner = block.inner(area);

    let scroll_y = if focus_line >= inner.height as usize {
        (focus_line + 1 - inner.height as usize) as u16
    } else {
        0
    };

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).scroll((scroll_y, 0)).block(block),
        area,
    );

    if let Some((line_idx, col)) = text_cursor {
        let rel_y = line_idx.saturating_sub(scroll_y as usize) as u16;
        if rel_y < inner.height {
            let cx = inner.x.saturating_add(col);
            if cx < inner.right() {
                let pos = (cx, inner.y.saturating_add(rel_y));
                f.set_cursor_position(pos);
                app.cursor_pos.set(Some(pos));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::find;
    use crate::session::Session;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn dump(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_empty_and_populated_states() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();

        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("new isolated session"));

        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.entries.push(Entry::User("fix the bug".into()));
        s.entries.push(Entry::Agent("on it".into()));
        app.sessions.push(s);
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("▎ fix the bug"), "{screen}");
        assert!(screen.contains("on it"));
        assert!(screen.contains("Claude Code"));

        app.sessions[0].status = Status::Running;
        app.sessions[0].turn_started = Some(std::time::Instant::now());
        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("working… 0s"));

        app.mode = Mode::Switcher;
        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("sessions"));
        app.mode = Mode::Normal;

        app.confirm = Some(Confirm::Quit);
        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("Quit and stop it?"));
    }

    #[test]
    fn renders_help_overlay_and_persistent_notice() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();

        app.set_message("copied selection");
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("copied selection"), "{screen}");

        app.mode = Mode::Help;
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains(" HELP "), "{screen}");
        assert!(screen.contains("prompt (vim)"), "{screen}");
        assert!(
            screen.contains("commands (Space in normal · C-a anywhere)"),
            "{screen}"
        );
        assert!(screen.contains("session & scroll"), "{screen}");
        assert!(screen.contains("Esc/q/? close"), "{screen}");
    }

    #[test]
    fn prompt_shows_a_block_cursor_in_normal_mode() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        app.sessions.push(s);
        app.input.set_text("ab\ncd");
        app.input.normal('0');
        app.input.normal('l');
        let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let reversed: Vec<&str> = buf
            .content()
            .iter()
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .map(|c| c.symbol())
            .collect();
        assert_eq!(reversed, vec!["d"], "{}", dump(&term));
    }

    #[test]
    fn picker_renders_install_and_login_status_badges() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        app.mode = Mode::Picker;
        app.picker = vec![
            find("claude").unwrap(),
            find("codex").unwrap(),
            find("antigravity").unwrap(),
        ];
        app.picker_status = vec![
            AgentStatus::Ready,
            AgentStatus::NeedsLogin,
            AgentStatus::NeedsInstall,
        ];
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("● installed"), "{screen}");
        assert!(screen.contains("○ needs login"), "{screen}");
        assert!(screen.contains("↓ will install"), "{screen}");

        app.picker_status = vec![
            AgentStatus::Missing,
            AgentStatus::Ready,
            AgentStatus::NeedsLogin,
        ];
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("✗ not installed"), "{screen}");
    }

    #[test]
    fn wrap_breaks_words_and_long_tokens() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("a\n\nb", 5), vec!["a", "", "b"]);
        assert_eq!(wrap_code("  x = 1", 4), vec!["  x ", "= 1"]);
        assert_eq!(fit("abcdef", 4), "abc…");
    }

    #[test]
    fn render_permission_shows_command_human_labels_and_hints() {
        use crate::agent::acp::{PermissionOption, PermissionRequest};

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::AwaitingPermission;
        s.pending = Some(PermissionRequest {
            tool_call_id: "c1".into(),
            title: Some("Run cargo test".into()),
            kind: Some("execute".into()),
            command: Some("cargo test --all".into()),
            options: vec![
                PermissionOption {
                    id: "a1".into(),
                    name: "Allow once".into(),
                    kind: "allow_once".into(),
                },
                PermissionOption {
                    id: "a2".into(),
                    name: "Allow".into(),
                    kind: "allow_always".into(),
                },
                PermissionOption {
                    id: "r1".into(),
                    name: "Reject".into(),
                    kind: "reject_once".into(),
                },
            ],
            ..Default::default()
        });
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);

        assert!(screen.contains("Run cargo test"), "{screen}");
        assert!(screen.contains("[execute]"), "{screen}");
        assert!(screen.contains("$ cargo test --all"), "{screen}");
        assert!(screen.contains("1 [a] Allow once"), "{screen}");
        assert!(screen.contains("2     Allow  always allow"), "{screen}");
        assert!(screen.contains("3 [r] Reject  reject once"), "{screen}");
        // No raw ACP kind strings and no duplicate `Allow once  allow once`.
        assert!(!screen.contains("allow_once"), "{screen}");
        assert!(!screen.contains("allow_always"), "{screen}");
        assert!(!screen.contains("reject_once"), "{screen}");
        assert!(!screen.contains("Allow once  allow once"), "{screen}");
        assert!(
            screen.contains("a allow · r reject · 1-9 choose · Esc cancel"),
            "{screen}"
        );
    }

    #[test]
    fn render_permission_shows_diff_lines_and_truncates_long_diffs() {
        use crate::agent::acp::{PermissionDiff, PermissionOption, PermissionRequest};

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::AwaitingPermission;
        let many_new = (1..=20)
            .map(|i| format!("let line_{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        s.pending = Some(PermissionRequest {
            tool_call_id: "c2".into(),
            title: Some("Edit src/client.rs".into()),
            kind: Some("edit".into()),
            locations: vec!["src/client.rs:10".into()],
            diffs: vec![PermissionDiff {
                path: "src/client.rs".into(),
                old_text: Some("let old_val = 0;\n".into()),
                new_text: many_new,
            }],
            options: vec![
                PermissionOption {
                    id: "ok".into(),
                    name: "Allow once".into(),
                    kind: "allow_once".into(),
                },
                PermissionOption {
                    id: "no".into(),
                    name: "Reject once".into(),
                    kind: "reject_once".into(),
                },
            ],
            ..Default::default()
        });
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(90, 22)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);

        assert!(screen.contains("[edit]"), "{screen}");
        assert!(screen.contains("src/client.rs:10"), "{screen}");
        assert!(screen.contains("- let old_val = 0;"), "{screen}");
        assert!(screen.contains("+ let line_1 = 1;"), "{screen}");
        assert!(screen.contains("more lines)"), "{screen}");
        // Options remain visible even when diff is truncated.
        assert!(screen.contains("1 [a] Allow once"), "{screen}");
        assert!(screen.contains("2 [r] Reject once"), "{screen}");

        // Verify diff colors: removed line is Red, added line is Green.
        let buf = term.backend().buffer();
        let has_red_minus = buf
            .content()
            .iter()
            .any(|c| c.symbol() == "-" && c.fg == ERR);
        let has_green_plus = buf
            .content()
            .iter()
            .any(|c| c.symbol() == "+" && c.fg == OK);
        assert!(has_red_minus, "expected red '-' in diff:\n{screen}");
        assert!(has_green_plus, "expected green '+' in diff:\n{screen}");
    }

    #[test]
    fn tool_call_detail_renders_expanded_and_collapsed() {
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.entries.push(Entry::Tool {
            id: "t0".into(),
            title: "Read Cargo.toml".into(),
            kind: "read".into(),
            status: "completed".into(),
            detail: vec![],
        });
        s.entries.push(Entry::Tool {
            id: "t1".into(),
            title: "Edit src/main.rs".into(),
            kind: "edit".into(),
            status: "completed".into(),
            detail: vec![ToolContent::Diff {
                path: "src/main.rs".into(),
                old_text: Some("fn main() {\n    old();\n}".into()),
                new_text: "fn main() {\n    new();\n}".into(),
            }],
        });
        s.entries.push(Entry::Tool {
            id: "t2".into(),
            title: "Run tests".into(),
            kind: "execute".into(),
            status: "completed".into(),
            detail: vec![ToolContent::Text("test ok\npassed".into())],
        });

        let to_text = |lines: &[Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
                .collect()
        };

        let expanded = transcript_lines(&s, 80, true);
        let exp_text = to_text(&expanded);
        assert!(exp_text[0].contains("Read Cargo.toml") && !exp_text[0].contains('▾'));
        assert!(exp_text.iter().any(|l| l.contains("edit ▾")));
        assert!(exp_text.iter().any(|l| l == "    ┄ src/main.rs"));
        assert!(exp_text.iter().any(|l| l == "      fn main() {"));
        assert!(exp_text.iter().any(|l| l == "    -     old();"));
        assert!(exp_text.iter().any(|l| l == "    +     new();"));
        assert!(exp_text.iter().any(|l| l == "      }"));
        assert!(exp_text.iter().any(|l| l.contains("execute ▾")));
        assert!(exp_text.iter().any(|l| l == "    │ test ok"));
        assert!(exp_text.iter().any(|l| l == "    │ passed"));

        // Verify red/green styles on diff lines.
        let removed = expanded
            .iter()
            .find(|l| l.spans.iter().any(|sp| sp.content.contains("-     old();")))
            .unwrap();
        assert_eq!(removed.style.fg, Some(ERR));
        let added = expanded
            .iter()
            .find(|l| l.spans.iter().any(|sp| sp.content.contains("+     new();")))
            .unwrap();
        assert_eq!(added.style.fg, Some(OK));

        let collapsed = transcript_lines(&s, 80, false);
        let col_text = to_text(&collapsed);
        assert!(col_text.iter().any(|l| l.contains("edit ▸")));
        assert!(col_text.iter().any(|l| l.contains("execute ▸")));
        assert!(!col_text.iter().any(|l| l.contains("┄ src/main.rs")));
        assert!(!col_text.iter().any(|l| l.contains("    - ")));
        assert!(!col_text.iter().any(|l| l.contains("    │ ")));
    }

    #[test]
    fn tool_call_detail_caps_large_diffs_and_text() {
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        let many_added: String = (1..=25)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let many_text: String = (1..=15)
            .map(|i| format!("out {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        s.entries.push(Entry::Tool {
            id: "t1".into(),
            title: "Create big.rs".into(),
            kind: "edit".into(),
            status: "completed".into(),
            detail: vec![ToolContent::Diff {
                path: "big.rs".into(),
                old_text: None,
                new_text: many_added,
            }],
        });
        s.entries.push(Entry::Tool {
            id: "t2".into(),
            title: "Verbose output".into(),
            kind: "execute".into(),
            status: "completed".into(),
            detail: vec![ToolContent::Text(many_text)],
        });

        let lines: Vec<String> = transcript_lines(&s, 80, true)
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        assert!(lines.iter().any(|l| l == "    … (+9 more lines)"));
        assert!(lines.iter().any(|l| l == "    … (+5 more lines)"));
        assert!(!lines.iter().any(|l| l.contains("line 25")));
        assert!(!lines.iter().any(|l| l.contains("out 15")));
    }

    #[test]
    fn large_file_diff_shows_changed_hunk_instead_of_file_top() {
        let mut old: Vec<String> = (1..=40).map(|i| format!("keep {i}")).collect();
        let mut new = old.clone();
        old[25] = "before".into();
        new[25] = "after".into();
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.entries.push(Entry::Tool {
            id: "t1".into(),
            title: "Edit large.rs".into(),
            kind: "edit".into(),
            status: "completed".into(),
            detail: vec![ToolContent::Diff {
                path: "large.rs".into(),
                old_text: Some(old.join("\n")),
                new_text: new.join("\n"),
            }],
        });
        let lines: Vec<String> = transcript_lines(&s, 80, true)
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        assert!(lines.iter().any(|l| l == "    - before"));
        assert!(lines.iter().any(|l| l == "    + after"));
        assert!(!lines.iter().any(|l| l == "      keep 1"));
    }

    fn fake_worktree(branch: &str) -> crate::worktree::Worktree {
        serde_json::from_value(serde_json::json!({
            "path": "/tmp/wt",
            "branch": branch,
            "repo": "/tmp/repo",
            "base": "deadbeef"
        }))
        .unwrap()
    }

    #[test]
    fn header_and_switcher_render_labels_worktree_state_and_attention() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );

        let (mut s0, _) = Session::detached(0, find("claude").unwrap());
        s0.status = Status::Idle;
        s0.label = Some("fix-retry".into());
        s0.worktree = Some(fake_worktree("mijote/claude-1"));
        s0.worktree_state = Some(crate::worktree::State {
            dirty: true,
            commits: 2,
        });

        let (mut s1, _) = Session::detached(1, find("claude").unwrap());
        s1.status = Status::AwaitingPermission;
        s1.worktree = Some(fake_worktree("mijote/claude-2"));
        s1.worktree_state = Some(crate::worktree::State {
            dirty: true,
            commits: 0,
        });

        let (mut s2, _) = Session::detached(2, find("codex").unwrap());
        s2.status = Status::Idle;
        s2.attention = true;
        s2.worktree = Some(fake_worktree("mijote/codex-1"));
        s2.worktree_state = Some(crate::worktree::State {
            dirty: false,
            commits: 3,
        });

        app.sessions.push(s0);
        app.sessions.push(s1);
        app.sessions.push(s2);
        app.selected = 0;

        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("fix-retry · Claude Code"), "{screen}");
        assert!(screen.contains("⎇ mijote/claude-1 * +2"), "{screen}");
        assert!(screen.contains("◆ 1 waiting"), "{screen}");
        assert!(screen.contains("★ 1 done · s switch"), "{screen}");

        // Switcher shows all labels/branches, worktree badges, attention states, and rename hint.
        app.mode = Mode::Switcher;
        term.draw(|f| render(f, &app)).unwrap();
        let switcher = dump(&term);
        assert!(switcher.contains("fix-retry · Claude Code"), "{switcher}");
        assert!(switcher.contains("⎇ mijote/claude-1 * +2"), "{switcher}");
        assert!(switcher.contains("⎇ mijote/claude-2 *"), "{switcher}");
        assert!(switcher.contains("◆ needs permission"), "{switcher}");
        assert!(switcher.contains("⎇ mijote/codex-1 +3"), "{switcher}");
        assert!(switcher.contains("★ done"), "{switcher}");
        assert!(switcher.contains("r rename"), "{switcher}");

        // Rename modal renders buffer and hints.
        app.mode = Mode::Rename;
        app.rename_buf = "api-cleanup".into();
        term.draw(|f| render(f, &app)).unwrap();
        let rename = dump(&term);
        assert!(rename.contains("rename session"), "{rename}");
        assert!(rename.contains("api-cleanup"), "{rename}");
        assert!(rename.contains("Enter save"), "{rename}");
        assert!(rename.contains("Esc cancel"), "{rename}");
    }

    #[test]
    fn find_match_cols_handles_case_and_wide_chars() {
        assert_eq!(
            find_match_cols("  ▎ Fix BUG and bug", "bug"),
            vec![(8, 11), (16, 19)]
        );
        assert_eq!(find_match_cols("日本語abc", "語A"), vec![(4, 7)]);
        assert!(find_match_cols("hello", "").is_empty());
        assert!(find_match_cols("hello", "xyz").is_empty());
    }

    #[test]
    fn render_highlights_search_matches_and_shows_counts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.entries.push(Entry::User("first bug here".into()));
        s.entries.push(Entry::Agent("second BUG there".into()));
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(80, 16)).unwrap();
        // Initial draw populates transcript_text and transcript_height.
        term.draw(|f| render(f, &app)).unwrap();
        assert_eq!(app.transcript_height.get(), 10);

        app.handle_key(key(KeyCode::Char('/')));
        for c in "bug".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("SEARCH"), "{screen}");
        assert!(screen.contains("/bug"), "{screen}");
        assert!(screen.contains("[1/2]"), "{screen}");
        assert!(screen.contains("Enter jump"), "{screen}");

        let transcript_highlights = |term: &Terminal<TestBackend>, app: &App| {
            let buf = term.backend().buffer();
            let view = transcript_area(buf.area, app.input.line_count());
            let mut cur_hl = String::new();
            let mut other_hl = String::new();
            for y in view.y..view.bottom() {
                for x in view.x..view.right() {
                    let c = &buf[(x, y)];
                    if c.bg == ACCENT && c.fg == Color::Black && c.modifier.contains(Modifier::BOLD)
                    {
                        cur_hl.push_str(c.symbol());
                    } else if c.bg == WARN && c.fg == Color::Black {
                        other_hl.push_str(c.symbol());
                    }
                }
            }
            (cur_hl, other_hl)
        };

        // Current match cells use ACCENT + BOLD; other match cells use WARN.
        assert_eq!(
            transcript_highlights(&term, &app),
            ("bug".to_string(), "BUG".to_string())
        );

        // Enter returns to Normal mode with compact counter and hints.
        app.handle_key(key(KeyCode::Enter));
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("NORMAL"), "{screen}");
        assert!(screen.contains("/bug [1/2]"), "{screen}");
        assert!(screen.contains("n/N next/prev"), "{screen}");

        // Stepping to the next match swaps current highlight to the second line.
        app.handle_key(key(KeyCode::Char('n')));
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("[2/2]"), "{screen}");
        assert_eq!(
            transcript_highlights(&term, &app),
            ("BUG".to_string(), "bug".to_string())
        );

        // Non-matching query displays "no matches".
        app.handle_key(key(KeyCode::Char('/')));
        for c in "zzzz".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("no matches"));
    }

    #[test]
    fn render_collects_visible_links_clears_on_modal_and_emits_osc8() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.entries.push(Entry::Agent(
            "Read [ACP docs](https://agentclientprotocol.com) now.".into(),
        ));
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let buf = term.draw(|f| render(f, &app)).unwrap().buffer.clone();
        let screen = dump(&term);
        assert!(screen.contains("ACP docs[1]"), "{screen}");
        assert!(
            screen.contains("[1] https://agentclientprotocol.com"),
            "{screen}"
        );

        let links = app.visible_links.borrow().clone();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].url, "https://agentclientprotocol.com");

        let mut bytes = Vec::new();
        write_osc8_links(&mut bytes, &links, &buf, Some((5, 18))).unwrap();
        let out = String::from_utf8(bytes).unwrap();
        assert!(out.contains("\x1b]8;;https://agentclientprotocol.com\x1b\\"));
        assert!(out.contains("\x1b]8;;\x1b\\"));

        // Control characters in a URL must be stripped before writing OSC 8.
        let dirty = vec![ScreenLink {
            url: "https://x.y/\x1b[31m\x07evil".into(),
            ..links[0].clone()
        }];
        let mut bytes = Vec::new();
        write_osc8_links(&mut bytes, &dirty, &buf, None).unwrap();
        let out = String::from_utf8(bytes).unwrap();
        assert!(out.contains("\x1b]8;;https://x.y/[31mevil\x1b\\"));

        // Opening a modal suppresses visible links so OSC 8 never paints over it.
        app.mode = Mode::Help;
        term.draw(|f| render(f, &app)).unwrap();
        assert!(app.visible_links.borrow().is_empty());
    }

    #[test]
    fn render_shows_at_mention_popup_above_prompt() {
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("src")).unwrap();
        std::fs::write(work.path().join("src/main.rs"), "").unwrap();
        std::fs::write(work.path().join("Cargo.toml"), "").unwrap();

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            work.path().to_path_buf(),
            work.path().to_path_buf(),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        app.sessions.push(s);
        app.mode = Mode::Insert;

        for c in "edit @ma".chars() {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }

        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("@ma"), "{screen}");
        assert!(screen.contains("src/main.rs"), "{screen}");
        assert!(screen.contains("Tab/Enter insert"), "{screen}");
    }

    #[test]
    fn render_shows_question_modal_options_and_background_badge() {
        use crate::agent::acp::{QuestionField, QuestionOption};
        use crate::session::{Msg, UiMsg};

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (s0, _, _) = Session::detached_with_questions(0, find("claude").unwrap());
        let (s1, _, _) = Session::detached_with_questions(1, find("codex").unwrap());
        app.sessions.push(s0);
        app.sessions.push(s1);

        let req = ElicitationRequest {
            message: "Which transport?".into(),
            fields: vec![
                QuestionField {
                    key: "question_0".into(),
                    title: Some("Transport".into()),
                    description: None,
                    required: false,
                    kind: FieldKind::SingleSelect {
                        options: vec![
                            QuestionOption {
                                value: "ACP".into(),
                                label: "ACP".into(),
                                description: Some("Standard stdio".into()),
                            },
                            QuestionOption {
                                value: "JSON".into(),
                                label: "JSON".into(),
                                description: None,
                            },
                        ],
                        default: None,
                    },
                },
                QuestionField {
                    key: "question_0_custom".into(),
                    title: Some("Other".into()),
                    description: None,
                    required: false,
                    kind: FieldKind::Text { default: None },
                },
            ],
        };
        // Session 1 (background) asks a question: header badge + bell.
        app.handle_msg(Msg {
            session: 1,
            body: UiMsg::Question(req.clone()),
        });
        assert!(app.pending_bell);
        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("1 waiting"));

        // Switch to session 1: question modal renders cleanly.
        app.selected = 1;
        app.sync_question_draft();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("question"), "{screen}");
        assert!(screen.contains("Which transport?"), "{screen}");
        assert!(screen.contains("1. ACP"), "{screen}");
        assert!(screen.contains("Standard stdio"), "{screen}");
        assert!(screen.contains("Other"), "{screen}");
    }

    #[test]
    fn render_shows_effort_in_header_and_picker_modal() {
        use crate::agent::acp::{EffortInfo, EffortState, ModelInfo, ModelState};

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.models = ModelState {
            current: Some("opus".into()),
            available: vec![ModelInfo {
                id: "opus".into(),
                name: "Opus".into(),
            }],
            config_id: Some("model".into()),
        };
        s.effort = EffortState {
            current: Some("high".into()),
            available: vec![
                EffortInfo {
                    id: "default".into(),
                    name: "Default".into(),
                    description: None,
                },
                EffortInfo {
                    id: "high".into(),
                    name: "High".into(),
                    description: Some("Deeper reasoning".into()),
                },
            ],
            config_id: Some("effort".into()),
        };
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(90, 20)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("· Opus"), "{screen}");
        assert!(screen.contains("· effort: High"), "{screen}");

        app.mode = Mode::Effort;
        app.effort_cursor = 1;
        term.draw(|f| render(f, &app)).unwrap();
        let modal_screen = dump(&term);
        assert!(modal_screen.contains("EFFORT"), "{modal_screen}");
        assert!(modal_screen.contains("effort"), "{modal_screen}");
        assert!(modal_screen.contains("High ✓"), "{modal_screen}");
        assert!(modal_screen.contains("Deeper reasoning"), "{modal_screen}");
    }

    #[test]
    fn render_shows_slash_command_popup_above_prompt() {
        use crate::agent::acp::AvailableCommand;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.commands = vec![
            AvailableCommand {
                name: "compact".into(),
                description: "Compact conversation history".into(),
                input_hint: None,
            },
            AvailableCommand {
                name: "web".into(),
                description: "Search the web".into(),
                input_hint: Some("query".into()),
            },
        ];
        app.sessions.push(s);
        app.mode = Mode::Insert;

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        let mut term = Terminal::new(TestBackend::new(90, 22)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("/ commands (4)"), "{screen}");
        assert!(screen.contains("/model"), "{screen}");
        assert!(screen.contains("local"), "{screen}");
        assert!(screen.contains("/effort"), "{screen}");
        assert!(screen.contains("/compact"), "{screen}");
        assert!(screen.contains("Compact conversation history"), "{screen}");
        assert!(screen.contains("/web"), "{screen}");
        assert!(screen.contains("<query>"), "{screen}");
        assert!(screen.contains("Tab"), "{screen}");

        // Narrow query with no matches renders empty-state message.
        for ch in "zzz".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        term.draw(|f| render(f, &app)).unwrap();
        let empty_screen = dump(&term);
        assert!(empty_screen.contains("/zzz · no matches"), "{empty_screen}");
        assert!(
            empty_screen.contains("no matching commands"),
            "{empty_screen}"
        );
    }
}
