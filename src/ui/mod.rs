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

use crate::agent::acp::{CostInfo, ElicitationRequest, FieldKind, ToolContent};
use crate::agent::install::AgentStatus;
use crate::app::{App, Confirm, FieldDraft, MentionPopup, Mode, QuestionDraft, SlashPopup};
use crate::session::{CacheStatus, Entry, Session, SessionUsage, Status};

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
    if app.mode == Mode::SessionModes {
        render_session_mode(f, app);
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
    if app.mode == Mode::Usage {
        render_usage(f, app);
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

fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

fn str_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Word-wrap prose to `width` display columns. Words longer than a line are split.
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
            let wl = str_width(word);
            if n > 0 && n + 1 + wl <= width {
                cur.push(' ');
                cur.push_str(word);
                n += 1 + wl;
                continue;
            }
            if n > 0 {
                out.push(std::mem::take(&mut cur));
                n = 0;
            }
            if wl <= width {
                cur.push_str(word);
                n = wl;
            } else {
                for ch in word.chars() {
                    let cw = char_width(ch);
                    if n > 0 && n + cw > width {
                        out.push(std::mem::take(&mut cur));
                        n = 0;
                    }
                    cur.push(ch);
                    n += cw;
                }
            }
        }
        out.push(cur);
    }
    out
}

/// Wrap editable single-line text to `width` display columns while preserving all
/// spaces so a cursor at the end maps cleanly to the last wrapped row.
fn wrap_input(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    let mut last_space_byte: Option<usize> = None;

    for ch in text.chars() {
        let cw = char_width(ch);
        if cur_w > 0 && cur_w + cw > width {
            if let Some(split_at) = last_space_byte.take() {
                let tail = cur[split_at..].to_string();
                cur.truncate(split_at);
                out.push(cur);
                cur = tail;
                cur_w = str_width(&cur);
            } else {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if cur_w > 0 && cur_w + cw > width {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
        }
        cur.push(ch);
        cur_w += cw;
        if ch == ' ' {
            last_space_byte = Some(cur.len());
        }
    }
    out.push(cur);
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

/// Format a token count compactly (`450`, `1.2k`, `45k`, `200k`, `1.2M`).
pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 9_950 {
        let tenths = (n + 50) / 100;
        let whole = tenths / 10;
        let frac = tenths % 10;
        if frac == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{frac}k")
        }
    } else if n < 999_500 {
        format!("{}k", (n + 500) / 1_000)
    } else {
        let tenths = (n + 50_000) / 100_000;
        let whole = tenths / 10;
        let frac = tenths % 10;
        if frac == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{frac}M")
        }
    }
}

fn format_tokens_exact(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Format context window utilization as a compact percentage (`0%`, `<1%`, `42%`).
pub fn format_pct(used: u64, size: u64) -> String {
    if size == 0 || used == 0 {
        return "0%".to_string();
    }
    let pct = (used as f64 / size as f64) * 100.0;
    if pct < 0.5 {
        "<1%".to_string()
    } else {
        format!("{:.0}%", pct.round())
    }
}

/// Format a session cost (`$0.14`, `$0.004`, `0.14 EUR`).
pub fn format_cost(cost: &CostInfo) -> String {
    if cost.currency.eq_ignore_ascii_case("USD") {
        if cost.amount > 0.0 && cost.amount < 0.01 {
            format!("${:.3}", cost.amount)
        } else {
            format!("${:.2}", cost.amount)
        }
    } else {
        format!("{:.2} {}", cost.amount, cost.currency)
    }
}

/// One-line usage summary suitable for CLI stderr output.
pub fn format_usage_summary(usage: &SessionUsage) -> Option<String> {
    if let (Some(used), Some(size)) = (usage.context_used, usage.context_size.filter(|&s| s > 0)) {
        let mut s = format!(
            "{} ({}/{})",
            format_pct(used, size),
            format_tokens(used),
            format_tokens(size)
        );
        if let Some(cost) = &usage.cost {
            s.push_str(&format!(" · {}", format_cost(cost)));
        }
        return Some(s);
    }
    let mut parts = Vec::new();
    if let Some(tok) = &usage.tokens {
        parts.push(format!("{} tok", format_tokens(tok.total_tokens)));
    }
    if let Some(cost) = &usage.cost {
        parts.push(format_cost(cost));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn context_color(pct: f64) -> Color {
    if pct >= 80.0 {
        ERR
    } else if pct >= 60.0 {
        WARN
    } else {
        OK
    }
}

fn context_bar_spans(pct: f64, width: usize) -> Vec<Span<'static>> {
    let clamped = pct.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let color = context_color(pct);
    let mut spans = Vec::with_capacity(2);
    if filled > 0 {
        spans.push(Span::styled("█".repeat(filled), Style::new().fg(color)));
    }
    if empty > 0 {
        spans.push(Span::styled("░".repeat(empty), DIM));
    }
    spans
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

fn format_cache_remaining(rem: std::time::Duration) -> String {
    let secs = rem.as_secs().max(1);
    if secs >= 10 * 60 {
        format!("{}m", (secs + 30) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn cache_border_badge(status: CacheStatus) -> (String, Style) {
    match status {
        CacheStatus::Warm { remaining, .. } => {
            let style = if remaining.as_secs() < 60 {
                Style::new().fg(WARN)
            } else {
                DIM
            };
            (
                format!("cache {}", format_cache_remaining(remaining)),
                style,
            )
        }
        CacheStatus::Expired {
            by_model_switch: true,
            ..
        } => ("cache cold".to_string(), Style::new().fg(WARN)),
        CacheStatus::Expired {
            by_model_switch: false,
            ..
        } => ("cache expired".to_string(), Style::new().fg(WARN)),
    }
}

/// Right-aligned context/usage gauge for the prompt box border, degrading
/// gracefully as terminal width shrinks.
fn prompt_usage_title(usage: &SessionUsage, max_width: usize) -> Option<Line<'static>> {
    if max_width < 8 {
        return None;
    }
    let cache_badge = usage.cache.status().map(cache_border_badge);
    if let (Some(used), Some(size)) = (usage.context_used, usage.context_size.filter(|&s| s > 0)) {
        let pct = (used as f64 / size as f64) * 100.0;
        let color = context_color(pct);
        let pct_style = if pct >= 80.0 {
            Style::new().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(color)
        };
        let pct_str = format_pct(used, size);
        let tok_str = format!(" · {}/{}", format_tokens(used), format_tokens(size));
        let cost_str = usage
            .cost
            .as_ref()
            .map(|c| format!(" · {}", format_cost(c)));

        let build = |with_bar: bool,
                     with_tok: bool,
                     with_cost: bool,
                     with_cache: bool|
         -> Vec<Span<'static>> {
            let mut spans = vec![Span::styled(" ctx ", DIM)];
            if with_bar {
                spans.extend(context_bar_spans(pct, 8));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(pct_str.clone(), pct_style));
            if with_tok {
                spans.push(Span::styled(tok_str.clone(), DIM));
            }
            if with_cost {
                if let Some(c) = &cost_str {
                    spans.push(Span::styled(c.clone(), DIM));
                }
            }
            if with_cache {
                if let Some((text, style)) = &cache_badge {
                    spans.push(Span::styled(" · ", DIM));
                    spans.push(Span::styled(text.clone(), *style));
                }
            }
            spans.push(Span::raw(" "));
            spans
        };

        let has_cost = cost_str.is_some();
        let has_cache = cache_badge.is_some();
        let candidates = [
            (true, true, has_cost, has_cache),
            (true, true, false, has_cache),
            (true, true, has_cost, false),
            (true, true, false, false),
            (true, false, false, has_cache),
            (true, false, false, false),
            (false, false, false, false),
        ];
        for (bar, tok, cost, cache) in candidates {
            let spans = build(bar, tok, cost, cache);
            if spans_width(&spans) <= max_width {
                return Some(Line::from(spans));
            }
        }
        return None;
    }

    let tok_part = usage
        .tokens
        .as_ref()
        .map(|t| format!("tok {}", format_tokens(t.total_tokens)));
    let cost_part = usage.cost.as_ref().map(format_cost);
    let build_fallback =
        |with_tok: bool, with_cost: bool, with_cache: bool| -> Vec<Span<'static>> {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut push_part = |text: String, style: Style| {
                if spans.is_empty() {
                    spans.push(Span::raw(" "));
                } else {
                    spans.push(Span::styled(" · ", DIM));
                }
                spans.push(Span::styled(text, style));
            };
            if with_tok {
                if let Some(t) = &tok_part {
                    push_part(t.clone(), DIM);
                }
            }
            if with_cost {
                if let Some(c) = &cost_part {
                    push_part(c.clone(), DIM);
                }
            }
            if with_cache {
                if let Some((text, style)) = &cache_badge {
                    push_part(text.clone(), *style);
                }
            }
            if !spans.is_empty() {
                spans.push(Span::raw(" "));
            }
            spans
        };
    let candidates = [
        (
            tok_part.is_some(),
            cost_part.is_some(),
            cache_badge.is_some(),
        ),
        (tok_part.is_some(), false, cache_badge.is_some()),
        (tok_part.is_some(), cost_part.is_some(), false),
        (tok_part.is_some(), false, false),
        (false, false, cache_badge.is_some()),
        (false, cost_part.is_some(), false),
    ];
    for (tok, cost, cache) in candidates {
        let spans = build_fallback(tok, cost, cache);
        if !spans.is_empty() && spans_width(&spans) <= max_width {
            return Some(Line::from(spans));
        }
    }
    None
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
    if let (Some(mode_id), Some(mode_name)) = (s.session_mode(), s.session_mode_name()) {
        left.push(Span::styled(
            format!("  · mode: {mode_name}"),
            session_mode_style(mode_id, &mode_name),
        ));
    }
    if app.show_trace {
        left.push(Span::styled("  · full trace", Style::new().fg(ACCENT)));
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

struct TranscriptOutput {
    lines: Vec<Line<'static>>,
    links: Vec<markdown::LinkSpan>,
    toggles: Vec<(usize, usize)>,
}

fn is_trace_entry(e: &Entry) -> bool {
    matches!(e, Entry::Thought(_) | Entry::Tool { .. })
}

fn is_meaningful_trace_entry(e: &Entry) -> bool {
    match e {
        Entry::Thought(t) => !t.trim().is_empty(),
        Entry::Tool { .. } => true,
        _ => false,
    }
}

struct TraceStats<'a> {
    thought_entries: usize,
    thought_lines: usize,
    tool_count: usize,
    failed_tools: usize,
    active_tools: usize,
    any_tool_detail: bool,
    single_tool: Option<(&'a str, &'a str, &'a str, &'a [ToolContent])>,
    kinds: Vec<(&'a str, usize)>,
}

impl TraceStats<'_> {
    fn is_empty(&self) -> bool {
        self.thought_entries == 0 && self.tool_count == 0
    }

    fn is_collapsible(&self) -> bool {
        self.thought_entries > 0
            || self.tool_count > 1
            || (self.tool_count == 1 && self.any_tool_detail)
    }
}

fn collect_trace_stats(entries: &[Entry]) -> TraceStats<'_> {
    let mut stats = TraceStats {
        thought_entries: 0,
        thought_lines: 0,
        tool_count: 0,
        failed_tools: 0,
        active_tools: 0,
        any_tool_detail: false,
        single_tool: None,
        kinds: Vec::new(),
    };
    for e in entries {
        match e {
            Entry::Thought(t) => {
                let trimmed = t.trim();
                if !trimmed.is_empty() {
                    stats.thought_entries += 1;
                    stats.thought_lines += trimmed.lines().count().max(1);
                }
            }
            Entry::Tool {
                title,
                kind,
                status,
                detail,
                ..
            } => {
                stats.tool_count += 1;
                if status == "failed" {
                    stats.failed_tools += 1;
                } else if status != "completed" {
                    stats.active_tools += 1;
                }
                if !detail.is_empty() {
                    stats.any_tool_detail = true;
                }
                if stats.tool_count == 1 {
                    stats.single_tool = Some((title, kind, status, detail));
                } else {
                    stats.single_tool = None;
                }
                if !kind.is_empty() {
                    if let Some((_, count)) = stats.kinds.iter_mut().find(|(k, _)| *k == kind) {
                        *count += 1;
                    } else {
                        stats.kinds.push((kind, 1));
                    }
                }
            }
            _ => {}
        }
    }
    stats
}

fn render_tool_row(
    s: &Session,
    title: &str,
    kind: &str,
    status: &str,
    arrow: Option<&'static str>,
    width: usize,
) -> Line<'static> {
    let (g, c) = tool_glyph(s, status);
    let kind_txt = if kind.is_empty() {
        String::new()
    } else {
        format!("  {kind}")
    };
    let lead_w = if arrow.is_some() { 6 } else { 4 };
    let room = width
        .saturating_sub(lead_w + kind_txt.chars().count())
        .max(1);
    let mut spans = Vec::with_capacity(4);
    if let Some(a) = arrow {
        spans.push(Span::styled(format!("  {a} "), DIM));
    } else {
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(format!("{g} "), Style::new().fg(c)));
    spans.push(Span::raw(fit(title, room)));
    if !kind_txt.is_empty() {
        spans.push(Span::styled(kind_txt, DIM));
    }
    Line::from(spans)
}

fn render_trace_summary(
    s: &Session,
    stats: &TraceStats<'_>,
    width: usize,
    expanded: bool,
) -> Line<'static> {
    let arrow = if expanded { "▾" } else { "▸" };
    if stats.thought_entries == 0 {
        if let Some((title, kind, status, _)) = stats.single_tool {
            return render_tool_row(s, title, kind, status, Some(arrow), width);
        }
    }

    let (g, c) = if stats.failed_tools > 0 {
        ("✗", ERR)
    } else if stats.active_tools > 0 {
        if s.status == Status::Running {
            (spinner(s), WARN)
        } else {
            ("○", WARN)
        }
    } else if stats.tool_count > 0 {
        ("✓", OK)
    } else {
        ("┊", Color::DarkGray)
    };

    if stats.tool_count == 0 {
        let label = if stats.thought_lines == 1 {
            "thought (1 line)".to_string()
        } else {
            format!("thought ({} lines)", stats.thought_lines)
        };
        let room = width.saturating_sub(6).max(1);
        return Line::from(vec![
            Span::styled(format!("  {arrow} "), DIM),
            Span::styled(format!("{g} "), Style::new().fg(c)),
            Span::styled(fit(&label, room), DIM.add_modifier(Modifier::ITALIC)),
        ]);
    }

    if let Some((title, kind, _, _)) = stats.single_tool {
        let thought_prefix = "thought · ";
        let kind_txt = if kind.is_empty() {
            String::new()
        } else {
            format!("  {kind}")
        };
        let room = width
            .saturating_sub(6 + thought_prefix.chars().count() + kind_txt.chars().count())
            .max(1);
        let mut spans = vec![
            Span::styled(format!("  {arrow} "), DIM),
            Span::styled(format!("{g} "), Style::new().fg(c)),
            Span::styled(thought_prefix, DIM),
            Span::raw(fit(title, room)),
        ];
        if !kind_txt.is_empty() {
            spans.push(Span::styled(kind_txt, DIM));
        }
        return Line::from(spans);
    }

    let kind_part = match stats.kinds.as_slice() {
        [] => String::new(),
        [(k, _)] => format!(" ({k})"),
        many => {
            let list = many
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" ({list})")
        }
    };
    let prefix = if stats.thought_entries > 0 {
        "thought · "
    } else {
        ""
    };
    let body = format!("{prefix}{} tools{kind_part}", stats.tool_count);
    let fail_suffix = if stats.failed_tools > 0 {
        format!(" · {} failed", stats.failed_tools)
    } else {
        String::new()
    };
    let room = width.saturating_sub(6 + fail_suffix.chars().count()).max(1);
    let mut spans = vec![
        Span::styled(format!("  {arrow} "), DIM),
        Span::styled(format!("{g} "), Style::new().fg(c)),
        Span::styled(fit(&body, room), DIM),
    ];
    if !fail_suffix.is_empty() {
        spans.push(Span::styled(fail_suffix, Style::new().fg(ERR)));
    }
    Line::from(spans)
}

#[cfg(test)]
fn transcript_lines(s: &Session, width: usize, show_trace: bool) -> Vec<Line<'static>> {
    transcript_lines_with_links(s, width, |_| show_trace, None).lines
}

/// Transcript body for one session, already wrapped to `width` columns, so
/// scrolling is exact and every wrapped line keeps its gutter. Contiguous
/// chain-of-thought and tool-call entries are grouped into collapsible trace
/// blocks controlled by `is_expanded(block_start_entry_idx)`.
fn transcript_lines_with_links(
    s: &Session,
    width: usize,
    is_expanded: impl Fn(usize) -> bool,
    cwd: Option<&Path>,
) -> TranscriptOutput {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<markdown::LinkSpan> = Vec::new();
    let mut toggles: Vec<(usize, usize)> = Vec::new();
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

    let mut i = 0;
    while i < s.entries.len() {
        match &s.entries[i] {
            Entry::User(t) => {
                out.extend(prose(t, "  ▎ ", Style::new().fg(ACCENT), BOLD));
                out.push(Line::from(""));
                i += 1;
            }
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
                out.push(Line::from(""));
                i += 1;
            }
            Entry::Notice(t) => {
                out.extend(prose(t, "  ▎ ", Style::new().fg(ERR), Style::new().fg(ERR)));
                out.push(Line::from(""));
                i += 1;
            }
            e if is_trace_entry(e) => {
                let start = i;
                while i < s.entries.len() && is_trace_entry(&s.entries[i]) {
                    i += 1;
                }
                let slice = &s.entries[start..i];
                let stats = collect_trace_stats(slice);
                if stats.is_empty() {
                    continue;
                }

                let is_live_tail = i == s.entries.len()
                    && matches!(
                        s.status,
                        Status::Running | Status::AwaitingPermission | Status::AwaitingQuestion
                    );

                if !stats.is_collapsible() {
                    if let Some((title, kind, status, _)) = stats.single_tool {
                        out.push(render_tool_row(s, title, kind, status, None, width));
                    }
                    out.push(Line::from(""));
                    continue;
                }

                if is_expanded(start) {
                    if stats.thought_entries == 0 {
                        if let Some((title, kind, status, detail)) = stats.single_tool {
                            toggles.push((out.len(), start));
                            out.push(render_tool_row(s, title, kind, status, Some("▾"), width));
                            if !detail.is_empty() {
                                render_tool_detail(detail, width, &mut out);
                            }
                            out.push(Line::from(""));
                            continue;
                        }
                    }

                    toggles.push((out.len(), start));
                    out.push(render_trace_summary(s, &stats, width, true));
                    let meaningful: Vec<&Entry> = slice
                        .iter()
                        .filter(|entry| is_meaningful_trace_entry(entry))
                        .collect();
                    for (j, entry) in meaningful.iter().enumerate() {
                        match entry {
                            Entry::Thought(t) => {
                                out.extend(prose(
                                    t,
                                    "  ┊ ",
                                    DIM,
                                    DIM.add_modifier(Modifier::ITALIC),
                                ));
                            }
                            Entry::Tool {
                                title,
                                kind,
                                status,
                                detail,
                                ..
                            } => {
                                out.push(render_tool_row(s, title, kind, status, None, width));
                                if !detail.is_empty() {
                                    render_tool_detail(detail, width, &mut out);
                                }
                            }
                            _ => {}
                        }
                        if let Some(next) = meaningful.get(j + 1) {
                            let both_tools = matches!(entry, Entry::Tool { .. })
                                && matches!(next, Entry::Tool { .. });
                            if !both_tools {
                                out.push(Line::from(""));
                            }
                        }
                    }
                    out.push(Line::from(""));
                    continue;
                }

                // Collapsed view: show a live pulse when actively working at the tail.
                if is_live_tail {
                    if let Some(last_rel) = slice.iter().rposition(is_meaningful_trace_entry) {
                        match &slice[last_rel] {
                            Entry::Tool {
                                title,
                                kind,
                                status,
                                ..
                            } if status != "completed" && status != "failed" => {
                                let earlier = collect_trace_stats(&slice[..last_rel]);
                                if !earlier.is_empty() {
                                    toggles.push((out.len(), start));
                                    out.push(render_trace_summary(s, &earlier, width, false));
                                    out.push(render_tool_row(s, title, kind, status, None, width));
                                } else {
                                    toggles.push((out.len(), start));
                                    out.push(render_tool_row(
                                        s,
                                        title,
                                        kind,
                                        status,
                                        Some("▸"),
                                        width,
                                    ));
                                }
                                out.push(Line::from(""));
                                continue;
                            }
                            Entry::Thought(t) if s.status == Status::Running => {
                                let latest = t
                                    .lines()
                                    .rev()
                                    .map(str::trim)
                                    .find(|l| !l.is_empty())
                                    .unwrap_or("");
                                let earlier = collect_trace_stats(&slice[..last_rel]);
                                if !earlier.is_empty() {
                                    toggles.push((out.len(), start));
                                    out.push(render_trace_summary(s, &earlier, width, false));
                                    out.push(Line::from(vec![
                                        Span::styled("  ┊ ", DIM),
                                        Span::styled(
                                            fit(latest, body),
                                            DIM.add_modifier(Modifier::ITALIC),
                                        ),
                                    ]));
                                } else {
                                    toggles.push((out.len(), start));
                                    let prefix = "thinking · ";
                                    let room =
                                        width.saturating_sub(6 + prefix.chars().count()).max(1);
                                    out.push(Line::from(vec![
                                        Span::styled("  ▸ ", DIM),
                                        Span::styled("┊ ", DIM),
                                        Span::styled(
                                            format!("{prefix}{}", fit(latest, room)),
                                            DIM.add_modifier(Modifier::ITALIC),
                                        ),
                                    ]));
                                }
                                out.push(Line::from(""));
                                continue;
                            }
                            _ => {}
                        }
                    }
                }

                toggles.push((out.len(), start));
                out.push(render_trace_summary(s, &stats, width, false));
                out.push(Line::from(""));
            }
            _ => {
                i += 1;
            }
        }
    }
    TranscriptOutput {
        lines: out,
        links,
        toggles,
    }
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
        app.trace_toggles.borrow_mut().clear();
        render_welcome(f, inner);
        return;
    };
    if s.entries.is_empty() {
        app.max_scroll.set(0);
        app.transcript_text.borrow_mut().clear();
        app.visible_links.borrow_mut().clear();
        app.trace_toggles.borrow_mut().clear();
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
    let TranscriptOutput {
        mut lines,
        links: link_spans,
        toggles,
    } = transcript_lines_with_links(
        s,
        inner.width as usize,
        |block_start| app.is_trace_expanded(s.id, block_start),
        Some(cwd),
    );
    *app.trace_toggles.borrow_mut() = toggles;
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
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .padding(Padding::horizontal(1));
    if let Some(s) = app.current() {
        let max_title_w = area.width.saturating_sub(4) as usize;
        if let Some(title) = prompt_usage_title(&s.usage, max_title_w) {
            block = block.title_top(title.right_aligned());
        }
    }
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
        Mode::SessionModes => (" MODE ", ASK),
        Mode::Switcher => (" SESSIONS ", ASK),
        Mode::Help => (" HELP ", ASK),
        Mode::Rename => (" RENAME ", ASK),
        Mode::Search => (" SEARCH ", WARN),
        Mode::Usage => (" USAGE ", ASK),
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
        let trace_label = if app.show_trace { "compact" } else { "details" };
        let hints: Vec<(&'static str, &'static str)> = match app.mode {
            _ if has_pending_perm => vec![
                ("a", "allow"),
                ("r", "reject"),
                ("1-9", "choose"),
                ("Esc", "cancel"),
            ],
            _ if has_pending_question => vec![
                ("↑/↓", "option"),
                ("Space", "toggle"),
                ("Tab", "field"),
                ("Enter", "next/submit"),
                ("Esc", "cancel"),
            ],
            _ if app.confirm.is_some() => vec![("y", "confirm"), ("any other key", "cancel")],
            _ if app.leader => vec![
                ("n", "new"),
                ("N", "new here"),
                ("d", "close"),
                ("r", "rename"),
                ("M", "model"),
                ("e", "effort"),
                ("o", "mode"),
                ("u", "usage"),
                ("s", "sessions"),
                ("t", trace_label),
                ("j/k", "select"),
                ("g/G", "top/bottom"),
                ("/", "search"),
                ("m", "mouse"),
                ("?", "help"),
                ("q", "quit"),
                ("Esc", "cancel"),
            ],
            Mode::Normal if app.sessions.is_empty() => vec![
                ("n", "new"),
                ("N/Enter", "new here"),
                ("m", "mouse"),
                ("?", "help"),
                ("q", "quit"),
            ],
            Mode::Normal if app.scroll > 0 => vec![
                ("␣G", "follow"),
                ("^O", trace_label),
                ("^U/^D", "half-page"),
                ("/", "search"),
                ("i", "insert"),
                ("␣", "commands"),
                ("?", "help"),
            ],
            Mode::Normal if busy => vec![
                ("^C", "cancel turn"),
                ("^O", trace_label),
                ("␣", "commands"),
                ("^U/^D", "half-page"),
                ("/", "search"),
                ("i", "insert"),
                ("?", "help"),
            ],
            Mode::Normal => vec![
                ("i", "insert"),
                ("␣", "commands"),
                ("^O", trace_label),
                ("^U/^D", "half-page"),
                ("/", "search"),
                ("?", "help"),
                ("^C", "quit"),
            ],
            Mode::Insert if app.slash.is_some() => vec![
                ("Tab", "complete"),
                ("Enter", "run"),
                ("↑/↓", "select"),
                ("Esc", "close"),
            ],
            Mode::Insert if app.mention.is_some() => vec![
                ("Tab/Enter", "insert file"),
                ("↑/↓", "select"),
                ("Esc", "close"),
            ],
            Mode::Insert if busy => vec![
                ("Enter", "send when idle"),
                ("^C", "cancel turn"),
                ("^O", trace_label),
                ("^A", "commands"),
                ("Esc", "normal"),
            ],
            Mode::Insert => vec![
                ("Enter", "send"),
                ("S-Enter", "newline"),
                ("/", "slash"),
                ("@", "files"),
                ("^O", trace_label),
                ("^A", "commands"),
                ("Esc", "normal"),
            ],
            Mode::Picker | Mode::Model | Mode::Effort | Mode::SessionModes => {
                vec![("j/k", "move"), ("Enter/1-9", "choose"), ("Esc", "close")]
            }
            Mode::Switcher => vec![
                ("j/k", "move"),
                ("Enter/1-9", "switch"),
                ("r", "rename"),
                ("Esc", "close"),
            ],
            Mode::Help => vec![("Esc/q/?", "close")],
            Mode::Usage => vec![("Esc/q/u", "close")],
            Mode::Rename => vec![("Enter", "save"), ("^U", "clear"), ("Esc", "cancel")],
            Mode::Search => vec![],
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

            let ctx_badge: Option<(String, Style)> = s.usage.context_pct().map(|pct| {
                let used = s.usage.context_used.unwrap_or(0);
                let size = s.usage.context_size.unwrap_or(1);
                let label = format!("ctx {}", format_pct(used, size));
                let style = if pct >= 80.0 {
                    Style::new().fg(ERR).add_modifier(Modifier::BOLD)
                } else if pct >= 60.0 {
                    Style::new().fg(WARN)
                } else {
                    DIM
                };
                (label, style)
            });
            let ctx_w = ctx_badge.as_ref().map_or(0, |(t, _)| 2 + t.chars().count());

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
            let avail =
                inner_w.saturating_sub(idx_str.chars().count() + 2 + wt_w + ctx_w + trailing_w);

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
            if let Some((text, style)) = ctx_badge {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(text, style));
            }
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

fn session_mode_style(id: &str, name: &str) -> Style {
    let lower_id = id.to_ascii_lowercase();
    let lower_name = name.to_ascii_lowercase();
    if lower_id.contains("bypass")
        || lower_id.contains("yolo")
        || lower_id.contains("dontask")
        || lower_id == "auto"
        || lower_name.contains("bypass")
        || lower_name.contains("yolo")
        || lower_name == "auto"
    {
        Style::new().fg(WARN)
    } else if lower_id.contains("plan")
        || lower_id.contains("architect")
        || lower_id == "ask"
        || lower_name.contains("plan")
    {
        Style::new().fg(ACCENT)
    } else if lower_id.contains("accept") || lower_name.contains("accept") {
        Style::new().fg(OK)
    } else {
        DIM
    }
}

fn render_session_mode(f: &mut Frame, app: &App) {
    let Some(s) = app.current() else { return };
    let current = s.session_mode();
    let has_desc = s.modes.available.iter().any(|m| m.description.is_some());
    let width = if has_desc { 66 } else { 44 };
    let area = centered(f.area(), width, s.modes.available.len() as u16 + 4);
    let inner_w = area.width.saturating_sub(8) as usize;
    let items: Vec<ListItem> = s
        .modes
        .available
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mark = if current == Some(m.id.as_str()) {
                " ✓"
            } else {
                ""
            };
            let idx = format!("{} ", i + 1);
            let label = format!("{}{mark}", m.name);
            let mut spans = vec![Span::styled(idx.clone(), DIM), Span::raw(label.clone())];
            if let Some(desc) = &m.description {
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
    state.select(Some(app.session_mode_cursor));
    let list = List::new(items)
        .block(modal(
            "mode",
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
        row("↑/↓ (k/j)", "prompt history (at first/last line)"),
        row("/  @", "slash commands & workspace file mentions"),
        row("d c y u C-r", "operators, undo/redo & vim motions"),
        Line::from(""),
        heading("commands (Space in normal · C-a anywhere)"),
        pair("n", "new worktree", "N", "new here"),
        pair("s / Tab", "switcher", "j / k", "next / prev"),
        pair("d", "close session", "r", "rename session"),
        pair("M", "model (/model)", "e", "effort (/effort)"),
        pair("o", "mode (/mode)", "u", "usage & context"),
        pair("t", "thoughts & tools", "m", "toggle mouse"),
        pair("/", "search", "g / G", "top / bottom"),
        pair("?", "help", "q", "quit"),
        Line::from(""),
        heading("session & scroll"),
        row("S-Tab", "cycle session mode"),
        row("C-o / click", "toggle thoughts & tool details"),
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

fn render_usage(f: &mut Frame, app: &App) {
    let Some(s) = app.current() else { return };
    let heading = |title: &'static str| {
        Line::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
    };
    let kv = |label: &'static str, value: String, style: Style| {
        Line::from(vec![
            Span::styled(format!("  {label:<12} "), DIM),
            Span::styled(value, style),
        ])
    };

    let mut header = vec![Span::styled(s.display_title(), BOLD)];
    if let Some(m) = s.model_name() {
        header.push(Span::styled(format!("  · {m}"), DIM));
    }
    if let Some(e) = s.effort_name() {
        header.push(Span::styled(format!("  · effort: {e}"), DIM));
    }
    if let (Some(mode_id), Some(mode_name)) = (s.session_mode(), s.session_mode_name()) {
        header.push(Span::styled(
            format!("  · mode: {mode_name}"),
            session_mode_style(mode_id, &mode_name),
        ));
    }
    let mut lines: Vec<Line<'static>> = vec![Line::from(header), Line::from("")];

    if !s.usage.has_data() {
        lines.push(Line::styled("No context or token usage reported yet.", DIM));
        lines.push(Line::styled(
            "Updates appear once the agent reports ACP usage_update or turn usage.",
            DIM,
        ));
    } else {
        if let (Some(used), Some(size)) = (
            s.usage.context_used,
            s.usage.context_size.filter(|&sz| sz > 0),
        ) {
            let pct = (used as f64 / size as f64) * 100.0;
            let color = context_color(pct);
            lines.push(heading("context window"));
            let mut bar = vec![Span::raw("  ")];
            bar.extend(context_bar_spans(pct, 28));
            bar.push(Span::styled(
                format!("  {pct:.1}%"),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::from(bar));
            lines.push(kv(
                "used",
                format!(
                    "{} / {} tokens",
                    format_tokens_exact(used),
                    format_tokens_exact(size)
                ),
                Style::new(),
            ));
            if size >= used {
                let rem = size - used;
                let rem_pct = (rem as f64 / size as f64) * 100.0;
                lines.push(kv(
                    "remaining",
                    format!("{} tokens ({rem_pct:.1}%)", format_tokens_exact(rem)),
                    DIM,
                ));
            } else {
                let over = used - size;
                lines.push(kv(
                    "over window",
                    format!("+{} tokens", format_tokens_exact(over)),
                    Style::new().fg(ERR).add_modifier(Modifier::BOLD),
                ));
            }
        }

        if let Some(cost) = &s.usage.cost {
            let formatted = if cost.currency.eq_ignore_ascii_case("USD") {
                format!("{} USD", format_cost(cost))
            } else {
                format_cost(cost)
            };
            lines.push(kv("session cost", formatted, Style::new()));
        }

        if let Some(status) = s.usage.cache.status() {
            let (val, style) = match status {
                CacheStatus::Warm { ttl, remaining } => {
                    let style = if remaining.as_secs() < 60 {
                        Style::new().fg(WARN)
                    } else {
                        Style::new().fg(OK)
                    };
                    (
                        format!(
                            "warm · {} left ({} TTL)",
                            format_cache_remaining(remaining),
                            ttl.label()
                        ),
                        style,
                    )
                }
                CacheStatus::Expired {
                    by_model_switch: true,
                    ..
                } => (
                    "cold (invalidated by model switch)".to_string(),
                    Style::new().fg(WARN),
                ),
                CacheStatus::Expired {
                    ttl,
                    by_model_switch: false,
                } => (
                    format!("expired ({} TTL elapsed)", ttl.label()),
                    Style::new().fg(WARN),
                ),
            };
            lines.push(kv("prompt cache", val, style));
        }

        if let Some(tok) = &s.usage.tokens {
            if lines.len() > 2 {
                lines.push(Line::from(""));
            }
            lines.push(heading("token usage"));
            lines.push(kv("total", format_tokens_exact(tok.total_tokens), BOLD));
            lines.push(kv(
                "input",
                format_tokens_exact(tok.input_tokens),
                Style::new(),
            ));
            lines.push(kv(
                "output",
                format_tokens_exact(tok.output_tokens),
                Style::new(),
            ));
            if let Some(cr) = tok.cached_read_tokens.filter(|&v| v > 0) {
                let denom = tok.input_tokens.saturating_add(cr);
                let val = if denom > 0 {
                    let hit = (cr as f64 / denom as f64) * 100.0;
                    format!("{} ({hit:.0}% cache hit)", format_tokens_exact(cr))
                } else {
                    format_tokens_exact(cr)
                };
                lines.push(kv("cache read", val, Style::new().fg(OK)));
            }
            if let Some(cw) = tok.cached_write_tokens.filter(|&v| v > 0) {
                lines.push(kv("cache write", format_tokens_exact(cw), DIM));
            }
            if let Some(th) = tok.thought_tokens.filter(|&v| v > 0) {
                lines.push(kv("thought", format_tokens_exact(th), DIM));
            }
        }
    }

    let area = centered(f.area(), 62, lines.len() as u16 + 4);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(modal(
            "usage & context",
            ACCENT,
            hint_line(&[("Esc/q/u", "close")]),
        )),
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

#[allow(clippy::too_many_arguments)]
fn push_question_option_lines(
    lines: &mut Vec<Line<'static>>,
    gutter: &Span<'static>,
    mark: &'static str,
    mark_style: Style,
    num: &str,
    label: &str,
    description: Option<&str>,
    focused: bool,
    body_w: usize,
) {
    let row_bg = if focused {
        Style::new().bg(SELECTED_BG)
    } else {
        Style::new()
    };
    let label_style = if focused { BOLD.patch(row_bg) } else { row_bg };
    let desc_style = DIM.patch(row_bg);
    let prefix_w = str_width(mark) + str_width(num);
    let opt_w = body_w.saturating_sub(prefix_w).max(1);
    let indent = " ".repeat(prefix_w);

    // Compact one-line form when label + " — " + description all fit on one line.
    if let Some(desc) = description {
        let single_line = !label.contains('\n')
            && !desc.contains('\n')
            && str_width(label) + 3 + str_width(desc) <= opt_w;
        if single_line {
            lines.push(Line::from(vec![
                gutter.clone(),
                Span::styled(mark, mark_style.patch(row_bg)),
                Span::styled(num.to_string(), DIM.patch(row_bg)),
                Span::styled(label.to_string(), label_style),
                Span::styled(format!(" — {desc}"), desc_style),
            ]));
            return;
        }
    }

    let mut label_lines = wrap(label, opt_w);
    if label_lines.is_empty() {
        label_lines.push(String::new());
    }
    for (i, part) in label_lines.into_iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                gutter.clone(),
                Span::styled(mark, mark_style.patch(row_bg)),
                Span::styled(num.to_string(), DIM.patch(row_bg)),
                Span::styled(part, label_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                gutter.clone(),
                Span::styled(indent.clone(), row_bg),
                Span::styled(part, label_style),
            ]));
        }
    }

    if let Some(desc) = description {
        for part in wrap(desc, opt_w) {
            lines.push(Line::from(vec![
                gutter.clone(),
                Span::styled(indent.clone(), row_bg),
                Span::styled(part, desc_style),
            ]));
        }
    }
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

    let term = f.area();
    let max_w = if term.width > 4 {
        (term.width - 4).min(80)
    } else {
        term.width
    };
    let inner_w = (max_w.saturating_sub(6) as usize).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut focus_start: usize = 0;
    let mut focus_end: usize = 0;
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
        let field_start = lines.len();
        let active = fi == draft.field_idx;
        let gutter = if active {
            Span::styled("▎ ", Style::new().fg(ACCENT))
        } else {
            Span::raw("  ")
        };
        let body_w = inner_w.saturating_sub(2).max(1);

        if let Some(title) = &field.title {
            let title_style = if active { BOLD.fg(ACCENT) } else { BOLD };
            let mut wrapped = wrap(title, body_w);
            if wrapped.is_empty() {
                wrapped.push(String::new());
            }
            let last_idx = wrapped.len() - 1;
            for (i, part) in wrapped.into_iter().enumerate() {
                let is_last = i == last_idx;
                let needs_new_line_for_star =
                    is_last && field.required && str_width(&part) + 2 > body_w;
                let mut spans = vec![gutter.clone(), Span::styled(part, title_style)];
                if is_last && field.required && !needs_new_line_for_star {
                    spans.push(Span::styled(" *", Style::new().fg(ERR)));
                }
                lines.push(Line::from(spans));
                if needs_new_line_for_star {
                    lines.push(Line::from(vec![
                        gutter.clone(),
                        Span::styled("*", Style::new().fg(ERR)),
                    ]));
                }
            }
        }
        if let Some(desc) = &field.description {
            for l in wrap(desc, body_w) {
                lines.push(Line::from(vec![gutter.clone(), Span::styled(l, DIM)]));
            }
        }
        if active {
            focus_start = field_start;
            focus_end = lines.len().saturating_sub(1);
        }

        match (&field.kind, draft.values.get(fi)) {
            (FieldKind::SingleSelect { options, .. }, Some(FieldDraft::Single(sel))) => {
                for (oi, opt) in options.iter().enumerate() {
                    let focused = active && oi == draft.option_idx;
                    let start = lines.len();
                    let picked = *sel == Some(oi);
                    let mark = if picked { "◉ " } else { "○ " };
                    let mark_style = if picked {
                        Style::new().fg(OK).add_modifier(Modifier::BOLD)
                    } else if focused {
                        Style::new().fg(ACCENT)
                    } else {
                        DIM
                    };
                    let num = format!("{}. ", oi + 1);
                    push_question_option_lines(
                        &mut lines,
                        &gutter,
                        mark,
                        mark_style,
                        &num,
                        &opt.label,
                        opt.description.as_deref(),
                        focused,
                        body_w,
                    );
                    if focused {
                        focus_start = start;
                        focus_end = lines.len().saturating_sub(1);
                    }
                }
            }
            (FieldKind::MultiSelect { options, .. }, Some(FieldDraft::Multi(checks))) => {
                for (oi, opt) in options.iter().enumerate() {
                    let focused = active && oi == draft.option_idx;
                    let start = lines.len();
                    let checked = checks.get(oi).copied().unwrap_or(false);
                    let mark = if checked { "[x] " } else { "[ ] " };
                    let mark_style = if checked {
                        Style::new().fg(OK).add_modifier(Modifier::BOLD)
                    } else if focused {
                        Style::new().fg(ACCENT)
                    } else {
                        DIM
                    };
                    let num = format!("{}. ", oi + 1);
                    push_question_option_lines(
                        &mut lines,
                        &gutter,
                        mark,
                        mark_style,
                        &num,
                        &opt.label,
                        opt.description.as_deref(),
                        focused,
                        body_w,
                    );
                    if focused {
                        focus_start = start;
                        focus_end = lines.len().saturating_sub(1);
                    }
                }
            }
            (FieldKind::Bool { .. }, Some(FieldDraft::Bool(val))) => {
                let start = lines.len();
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
                if active {
                    focus_start = start;
                    focus_end = lines.len().saturating_sub(1);
                }
            }
            (FieldKind::Text { .. } | FieldKind::Number { .. }, Some(FieldDraft::Text(buf))) => {
                let start = lines.len();
                let text_w = body_w.saturating_sub(2).max(1);
                let prompt_style = if active { Style::new().fg(ACCENT) } else { DIM };
                if buf.is_empty() && !active {
                    let ph = match field.kind {
                        FieldKind::Number { integer: true, .. } => "integer (optional)",
                        FieldKind::Number { integer: false, .. } => "number (optional)",
                        _ if field.required => "type answer…",
                        _ => "type custom answer (optional)…",
                    };
                    for (i, part) in wrap(ph, text_w).into_iter().enumerate() {
                        let lead = if i == 0 {
                            Span::styled("› ", prompt_style)
                        } else {
                            Span::raw("  ")
                        };
                        lines.push(Line::from(vec![
                            gutter.clone(),
                            lead,
                            Span::styled(part, DIM),
                        ]));
                    }
                } else {
                    let wrapped = wrap_input(buf, text_w);
                    let mut last_w = 0;
                    for (i, part) in wrapped.into_iter().enumerate() {
                        last_w = str_width(&part);
                        let lead = if i == 0 {
                            Span::styled("› ", prompt_style)
                        } else {
                            Span::raw("  ")
                        };
                        lines.push(Line::from(vec![gutter.clone(), lead, Span::raw(part)]));
                    }
                    if active {
                        if last_w >= text_w {
                            lines.push(Line::from(vec![gutter.clone(), Span::raw("  ")]));
                            text_cursor = Some((lines.len() - 1, 4));
                        } else {
                            text_cursor = Some((lines.len() - 1, 4 + last_w as u16));
                        }
                    }
                }
                if active {
                    focus_start = start;
                    focus_end = lines.len().saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    if let Some(err) = &draft.error {
        lines.push(Line::from(""));
        for l in wrap(err, inner_w.saturating_sub(2).max(1)) {
            lines.push(Line::styled(format!("  {l}"), Style::new().fg(ERR)));
        }
    }

    let hint = hint_line(&[
        ("↑/↓", "move"),
        ("Space", "toggle"),
        ("Tab", "field"),
        ("Enter", "submit"),
        ("Esc", "cancel"),
    ]);
    let min_w = (hint.width() as u16 + 4).max(56).min(max_w);
    let mut content_w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    if let Some((_, col)) = text_cursor {
        content_w = content_w.max(col.saturating_add(1));
    }
    let w = (content_w + 6).clamp(min_w, max_w);
    let max_h = term.height.saturating_sub(4).max(7);
    let h = (lines.len() as u16 + 4).min(max_h);
    let area = centered(term, w, h);
    let block = modal("question", ASK, hint);
    let inner = block.inner(area);

    let scroll_y = if focus_end >= inner.height as usize {
        let need = focus_end + 1 - inner.height as usize;
        let clamped = if text_cursor.is_some() {
            need
        } else {
            need.min(focus_start)
        };
        clamped as u16
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
        assert!(exp_text[0].contains("▾ ✓ 3 tools (1 read, 1 edit, 1 execute)"));
        assert!(exp_text.iter().any(|l| l.contains("Read Cargo.toml")));
        assert!(exp_text.iter().any(|l| l.contains("Edit src/main.rs")));
        assert!(exp_text.iter().any(|l| l == "    ┄ src/main.rs"));
        assert!(exp_text.iter().any(|l| l == "      fn main() {"));
        assert!(exp_text.iter().any(|l| l == "    -     old();"));
        assert!(exp_text.iter().any(|l| l == "    +     new();"));
        assert!(exp_text.iter().any(|l| l == "      }"));
        assert!(exp_text.iter().any(|l| l.contains("Run tests")));
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
        assert!(col_text
            .iter()
            .any(|l| l.contains("▸ ✓ 3 tools (1 read, 1 edit, 1 execute)")));
        assert!(!col_text.iter().any(|l| l.contains("┄ src/main.rs")));
        assert!(!col_text.iter().any(|l| l.contains("    - ")));
        assert!(!col_text.iter().any(|l| l.contains("    │ ")));
    }

    #[test]
    fn trace_blocks_collapse_thoughts_tools_and_live_tail_smartly() {
        let to_text = |lines: &[Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
                .collect()
        };

        // 1. Completed mixed trace (thoughts + tools + one failure) folds to 1 line.
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.entries.push(Entry::User("fix tests".into()));
        s.entries.push(Entry::Thought(
            "First line of reasoning\nSecond line".into(),
        ));
        s.entries.push(Entry::Tool {
            id: "t1".into(),
            title: "Read src/lib.rs".into(),
            kind: "read".into(),
            status: "completed".into(),
            detail: vec![ToolContent::Text("fn foo() {}".into())],
        });
        s.entries.push(Entry::Tool {
            id: "t2".into(),
            title: "Run cargo test".into(),
            kind: "execute".into(),
            status: "failed".into(),
            detail: vec![ToolContent::Text("FAILED".into())],
        });
        s.entries.push(Entry::Agent("Found the issue.".into()));

        let compact = to_text(&transcript_lines(&s, 90, false));
        assert!(
            compact
                .iter()
                .any(|l| l.contains("▸ ✗ thought · 2 tools (1 read, 1 execute) · 1 failed")),
            "{compact:?}"
        );
        assert!(!compact
            .iter()
            .any(|l| l.contains("First line of reasoning")));
        assert!(!compact.iter().any(|l| l.contains("FAILED")));
        assert!(compact.iter().any(|l| l.contains("Found the issue.")));

        let full = to_text(&transcript_lines(&s, 90, true));
        assert!(
            full.iter()
                .any(|l| l.contains("▾ ✗ thought · 2 tools (1 read, 1 execute) · 1 failed")),
            "{full:?}"
        );
        assert!(full.iter().any(|l| l.contains("┊ First line of reasoning")));
        assert!(full.iter().any(|l| l.contains("│ FAILED")));

        // 2. Thoughts-only block collapses to line count.
        let (mut s2, _) = Session::detached(1, find("claude").unwrap());
        s2.status = Status::Idle;
        s2.entries
            .push(Entry::Thought("Line A\nLine B\nLine C".into()));
        s2.entries.push(Entry::Agent("Answer.".into()));
        let compact2 = to_text(&transcript_lines(&s2, 80, false));
        assert!(
            compact2.iter().any(|l| l.contains("▸ ┊ thought (3 lines)")),
            "{compact2:?}"
        );
        assert!(!compact2.iter().any(|l| l.contains("Line A")));

        // 3. Single tool with no detail renders without a misleading toggle arrow;
        //    single tool with detail toggles directly on its row.
        let (mut s3, _) = Session::detached(2, find("claude").unwrap());
        s3.status = Status::Idle;
        s3.entries.push(Entry::Tool {
            id: "a".into(),
            title: "Read Cargo.toml".into(),
            kind: "read".into(),
            status: "completed".into(),
            detail: vec![],
        });
        s3.entries.push(Entry::Agent("mid".into()));
        s3.entries.push(Entry::Tool {
            id: "b".into(),
            title: "Edit src/lib.rs".into(),
            kind: "edit".into(),
            status: "completed".into(),
            detail: vec![ToolContent::Text("diff here".into())],
        });
        let compact3 = to_text(&transcript_lines(&s3, 80, false));
        assert!(
            compact3
                .iter()
                .any(|l| l.contains("✓ Read Cargo.toml") && !l.contains('▸')),
            "{compact3:?}"
        );
        assert!(
            compact3.iter().any(|l| l.contains("▸ ✓ Edit src/lib.rs")),
            "{compact3:?}"
        );
        assert!(!compact3.iter().any(|l| l.contains("diff here")));

        let full3 = to_text(&transcript_lines(&s3, 80, true));
        assert!(
            full3.iter().any(|l| l.contains("▾ ✓ Edit src/lib.rs")),
            "{full3:?}"
        );
        assert!(full3.iter().any(|l| l.contains("│ diff here")));

        // 4. Live tail while running shows a 1-line pulse for streaming thoughts and active tools.
        let (mut s4, _) = Session::detached(3, find("claude").unwrap());
        s4.status = Status::Running;
        s4.entries.push(Entry::Thought(
            "Earlier reasoning\nChecking parser.rs now".into(),
        ));
        let live_thought = to_text(&transcript_lines(&s4, 80, false));
        assert!(
            live_thought
                .iter()
                .any(|l| l.contains("▸ ┊ thinking · Checking parser.rs now")),
            "{live_thought:?}"
        );
        assert!(!live_thought.iter().any(|l| l.contains("Earlier reasoning")));

        s4.entries.push(Entry::Tool {
            id: "run1".into(),
            title: "Run cargo test".into(),
            kind: "execute".into(),
            status: "in_progress".into(),
            detail: vec![],
        });
        let live_tool = to_text(&transcript_lines(&s4, 80, false));
        assert!(
            live_tool
                .iter()
                .any(|l| l.contains("▸ ┊ thought (2 lines)")),
            "{live_tool:?}"
        );
        assert!(
            live_tool.iter().any(|l| l.contains("Run cargo test")),
            "{live_tool:?}"
        );
    }

    #[test]
    fn render_shows_full_trace_indicator_and_per_block_toggle() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.entries.push(Entry::User("hi".into()));
        s.entries.push(Entry::Thought("hidden reasoning".into()));
        s.entries.push(Entry::Agent("first reply".into()));
        s.entries.push(Entry::Thought("second reasoning".into()));
        s.entries.push(Entry::Agent("second reply".into()));
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();

        // Compact by default: both thoughts collapsed, status shows ^O details.
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(!screen.contains("full trace"), "{screen}");
        assert!(screen.contains("^O details"), "{screen}");
        assert!(!screen.contains("hidden reasoning"), "{screen}");
        assert!(!screen.contains("second reasoning"), "{screen}");
        assert_eq!(app.trace_toggles.borrow().len(), 2);

        // Expanding only the first block leaves the second collapsed.
        let first_block = app.trace_toggles.borrow()[0].1;
        app.trace_overrides.insert((0, first_block));
        term.draw(|f| render(f, &app)).unwrap();
        let partial = dump(&term);
        assert!(partial.contains("hidden reasoning"), "{partial}");
        assert!(!partial.contains("second reasoning"), "{partial}");

        // Global toggle expands everything and shows header badge + ^O compact.
        app.toggle_trace();
        term.draw(|f| render(f, &app)).unwrap();
        let expanded = dump(&term);
        assert!(expanded.contains("· full trace"), "{expanded}");
        assert!(expanded.contains("hidden reasoning"), "{expanded}");
        assert!(expanded.contains("second reasoning"), "{expanded}");
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
    fn question_modal_wraps_long_titles_options_descriptions_text_and_errors() {
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
        app.sessions.push(s0);

        let req = ElicitationRequest {
            message: "Choose how the migration should proceed when dealing with long identifiers and schemas END_OF_MESSAGE".into(),
            fields: vec![
                QuestionField {
                    key: "question_0".into(),
                    title: Some(
                        "Which database rollout strategy should we adopt across every production region END_OF_TITLE"
                            .into(),
                    ),
                    description: None,
                    required: true,
                    kind: FieldKind::SingleSelect {
                        options: vec![
                            QuestionOption {
                                value: "blue_green".into(),
                                label: "Blue-green deployment with automated dual-write verification across all shards END_OF_SINGLE_LABEL".into(),
                                description: Some(
                                    "Keeps the previous cluster warm so we can cut back instantaneously if latency spikes END_OF_SINGLE_DESC"
                                        .into(),
                                ),
                            },
                            QuestionOption {
                                value: "fast".into(),
                                label: "Fast".into(),
                                description: Some("Short inline desc".into()),
                            },
                        ],
                        default: None,
                    },
                },
                QuestionField {
                    key: "question_1".into(),
                    title: Some("Checks".into()),
                    description: None,
                    required: false,
                    kind: FieldKind::MultiSelect {
                        options: vec![QuestionOption {
                            value: "audit".into(),
                            label: "Run exhaustive schema compatibility checks against every historical snapshot END_OF_MULTI_LABEL".into(),
                            description: Some(
                                "Validates both forward and backward deserialization before approving END_OF_MULTI_DESC"
                                    .into(),
                            ),
                        }],
                        default: Vec::new(),
                    },
                },
                QuestionField {
                    key: "question_0_custom".into(),
                    title: Some("Other".into()),
                    description: None,
                    required: false,
                    kind: FieldKind::Text {
                        default: Some(
                            "A very long custom answer typed by the user that definitely exceeds a single modal line END_OF_TEXT_INPUT"
                                .into(),
                        ),
                    },
                },
            ],
        };
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Question(req),
        });
        app.sync_question_draft();
        if let Some(draft) = app.question_draft.as_mut() {
            draft.error = Some(
                "Validation failed because the chosen configuration needs confirmation before proceeding END_OF_ERROR"
                    .into(),
            );
        }

        let mut term = Terminal::new(TestBackend::new(84, 38)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("END_OF_MESSAGE"), "{screen}");
        assert!(screen.contains("END_OF_TITLE *"), "{screen}");
        assert!(screen.contains("END_OF_SINGLE_LABEL"), "{screen}");
        assert!(screen.contains("END_OF_SINGLE_DESC"), "{screen}");
        // Short option label + description stay compact on one line.
        assert!(screen.contains("2. Fast — Short inline desc"), "{screen}");
        assert!(screen.contains("END_OF_MULTI_LABEL"), "{screen}");
        assert!(screen.contains("END_OF_MULTI_DESC"), "{screen}");
        assert!(screen.contains("END_OF_TEXT_INPUT"), "{screen}");
        assert!(screen.contains("END_OF_ERROR"), "{screen}");

        // Narrow terminal (< 48 cols) still wraps cleanly inside borders without clipping.
        let mut narrow = Terminal::new(TestBackend::new(40, 58)).unwrap();
        narrow.draw(|f| render(f, &app)).unwrap();
        let narrow_screen = dump(&narrow);
        assert!(narrow_screen.contains("END_OF_MESSAGE"), "{narrow_screen}");
        assert!(narrow_screen.contains("END_OF_TITLE *"), "{narrow_screen}");
        assert!(
            narrow_screen.contains("END_OF_SINGLE_LABEL"),
            "{narrow_screen}"
        );
        assert!(
            narrow_screen.contains("END_OF_SINGLE_DESC"),
            "{narrow_screen}"
        );
        assert!(
            narrow_screen.contains("END_OF_MULTI_LABEL"),
            "{narrow_screen}"
        );
        assert!(
            narrow_screen.contains("END_OF_MULTI_DESC"),
            "{narrow_screen}"
        );
        assert!(
            narrow_screen.contains("END_OF_TEXT_INPUT"),
            "{narrow_screen}"
        );
        assert!(narrow_screen.contains("END_OF_ERROR"), "{narrow_screen}");
    }

    #[test]
    fn question_modal_scrolls_wrapped_focused_option_and_places_wrapped_text_cursor() {
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
        app.sessions.push(s0);

        let req = ElicitationRequest {
            message: "Pick an option".into(),
            fields: vec![
                QuestionField {
                    key: "question_0".into(),
                    title: Some("Choices".into()),
                    description: None,
                    required: false,
                    kind: FieldKind::SingleSelect {
                        options: vec![
                            QuestionOption {
                                value: "a".into(),
                                label: "First option".into(),
                                description: Some("Line one description that wraps across multiple rows in a short viewport".into()),
                            },
                            QuestionOption {
                                value: "b".into(),
                                label: "Second option".into(),
                                description: Some("Line two description that wraps across multiple rows in a short viewport".into()),
                            },
                            QuestionOption {
                                value: "c".into(),
                                label: "Third option with a long label that wraps cleanly onto a second line".into(),
                                description: Some("Detailed final option description ending with BOTTOM_MARKER".into()),
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
                    kind: FieldKind::Text {
                        default: Some(
                            "Typed a long custom answer that wraps onto multiple lines CURSOR_END"
                                .into(),
                        ),
                    },
                },
            ],
        };
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Question(req),
        });
        app.sync_question_draft();
        // Focus the last option in a short terminal: its wrapped tail must scroll into view.
        if let Some(draft) = app.question_draft.as_mut() {
            draft.field_idx = 0;
            draft.option_idx = 2;
        }
        let mut short = Terminal::new(TestBackend::new(60, 12)).unwrap();
        short.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&short);
        assert!(screen.contains("BOTTOM_MARKER"), "{screen}");

        // Focus the wrapped text field: tail and cursor stay visible inside the modal.
        if let Some(draft) = app.question_draft.as_mut() {
            draft.field_idx = 1;
            draft.option_idx = 0;
        }
        app.cursor_pos.set(None);
        short.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&short);
        assert!(screen.contains("CURSOR_END"), "{screen}");
        assert!(app.cursor_pos.get().is_some(), "expected text cursor");
    }

    #[test]
    fn wrap_input_preserves_spaces_and_breaks_words() {
        assert_eq!(wrap_input("", 10), vec![""]);
        assert_eq!(wrap_input("hello world", 7), vec!["hello ", "world"]);
        assert_eq!(wrap_input("hello ", 6), vec!["hello "]);
        assert_eq!(wrap_input("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
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
    fn render_shows_session_mode_in_header_and_picker_modal() {
        use crate::agent::acp::{SessionModeInfo, SessionModeState};

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.modes = SessionModeState {
            current: Some("bypassPermissions".into()),
            available: vec![
                SessionModeInfo {
                    id: "default".into(),
                    name: "Default".into(),
                    description: Some("Ask before edits".into()),
                },
                SessionModeInfo {
                    id: "bypassPermissions".into(),
                    name: "Auto".into(),
                    description: Some("Approve edits automatically".into()),
                },
            ],
            config_id: Some("mode".into()),
        };
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(90, 20)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("· mode: Auto"), "{screen}");

        app.mode = Mode::SessionModes;
        app.session_mode_cursor = 1;
        term.draw(|f| render(f, &app)).unwrap();
        let modal_screen = dump(&term);
        assert!(modal_screen.contains("MODE"), "{modal_screen}");
        assert!(modal_screen.contains("Auto ✓"), "{modal_screen}");
        assert!(
            modal_screen.contains("Approve edits automatically"),
            "{modal_screen}"
        );
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
        assert!(screen.contains("/ commands (5)"), "{screen}");
        assert!(screen.contains("/model"), "{screen}");
        assert!(screen.contains("local"), "{screen}");
        assert!(screen.contains("/effort"), "{screen}");
        assert!(screen.contains("/mode"), "{screen}");
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

    #[test]
    fn format_tokens_pct_and_cost_handle_boundaries() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(1_240), "1.2k");
        assert_eq!(format_tokens(9_949), "9.9k");
        assert_eq!(format_tokens(9_950), "10k");
        assert_eq!(format_tokens(45_200), "45k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(999_500), "1M");
        assert_eq!(format_tokens(1_250_000), "1.3M");

        assert_eq!(format_tokens_exact(0), "0");
        assert_eq!(format_tokens_exact(84_210), "84,210");
        assert_eq!(format_tokens_exact(1_000_000), "1,000,000");

        assert_eq!(format_pct(0, 200_000), "0%");
        assert_eq!(format_pct(500, 200_000), "<1%");
        assert_eq!(format_pct(84_000, 200_000), "42%");
        assert_eq!(format_pct(200_000, 200_000), "100%");

        assert_eq!(
            format_cost(&CostInfo {
                amount: 0.004,
                currency: "USD".into()
            }),
            "$0.004"
        );
        assert_eq!(
            format_cost(&CostInfo {
                amount: 0.14,
                currency: "USD".into()
            }),
            "$0.14"
        );
        assert_eq!(
            format_cost(&CostInfo {
                amount: 1.5,
                currency: "EUR".into()
            }),
            "1.50 EUR"
        );
    }

    #[test]
    fn render_shows_context_gauge_on_prompt_switcher_and_usage_modal() {
        use crate::agent::acp::{TokenUsage, UsageUpdate};

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

        // Before any usage arrives, no context gauge is drawn on the prompt box.
        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let before = dump(&term);
        assert!(!before.contains("ctx "), "{before}");

        // Empty Usage modal explains that no usage was reported yet.
        app.mode = Mode::Usage;
        term.draw(|f| render(f, &app)).unwrap();
        let empty_modal = dump(&term);
        assert!(empty_modal.contains("USAGE"), "{empty_modal}");
        assert!(empty_modal.contains("usage & context"), "{empty_modal}");
        assert!(
            empty_modal.contains("No context or token usage reported yet."),
            "{empty_modal}"
        );
        app.mode = Mode::Normal;

        // Populate context usage + cost + token breakdown.
        app.sessions[0].usage.apply_update(UsageUpdate {
            used: 84_000,
            size: 200_000,
            cost: Some(CostInfo {
                amount: 0.14,
                currency: "USD".into(),
            }),
        });
        app.sessions[0].usage.apply_tokens(TokenUsage {
            total_tokens: 96_000,
            input_tokens: 12_000,
            output_tokens: 4_000,
            thought_tokens: Some(1_800),
            cached_read_tokens: Some(78_000),
            cached_write_tokens: Some(2_000),
        });

        term.draw(|f| render(f, &app)).unwrap();
        let main_screen = dump(&term);
        assert!(
            main_screen.contains("ctx ███░░░░░ 42% · 84k/200k · $0.14"),
            "{main_screen}"
        );

        // Narrow terminal degrades gracefully without panicking or dropping %.
        let mut narrow = Terminal::new(TestBackend::new(26, 16)).unwrap();
        narrow.draw(|f| render(f, &app)).unwrap();
        let narrow_screen = dump(&narrow);
        assert!(narrow_screen.contains("ctx"), "{narrow_screen}");
        assert!(narrow_screen.contains("42%"), "{narrow_screen}");

        // Switcher shows compact `ctx 42%` badge.
        app.mode = Mode::Switcher;
        term.draw(|f| render(f, &app)).unwrap();
        let switcher = dump(&term);
        assert!(switcher.contains("ctx 42%"), "{switcher}");

        // Usage modal shows full breakdown.
        app.mode = Mode::Usage;
        term.draw(|f| render(f, &app)).unwrap();
        let modal_screen = dump(&term);
        assert!(modal_screen.contains("context window"), "{modal_screen}");
        assert!(modal_screen.contains("42.0%"), "{modal_screen}");
        assert!(
            modal_screen.contains("84,000 / 200,000 tokens"),
            "{modal_screen}"
        );
        assert!(
            modal_screen.contains("116,000 tokens (58.0%)"),
            "{modal_screen}"
        );
        assert!(modal_screen.contains("$0.14 USD"), "{modal_screen}");
        assert!(modal_screen.contains("token usage"), "{modal_screen}");
        assert!(modal_screen.contains("96,000"), "{modal_screen}");
        assert!(
            modal_screen.contains("78,000 (87% cache hit)"),
            "{modal_screen}"
        );
        assert!(modal_screen.contains("1,800"), "{modal_screen}");

        // High context (>= 80%) colors the percentage cell red (ERR).
        app.mode = Mode::Normal;
        app.sessions[0].usage.apply_update(UsageUpdate {
            used: 170_000,
            size: 200_000,
            cost: None,
        });
        term.draw(|f| render(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let high_screen = dump(&term);
        assert!(high_screen.contains("85%"), "{high_screen}");
        let mut found_err_cell = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let c = &buf[(x, y)];
                if c.symbol() == "█" && c.fg == ERR {
                    found_err_cell = true;
                }
            }
        }
        assert!(found_err_cell, "expected red filled bar at 85% context");
    }

    #[test]
    fn prompt_usage_title_falls_back_to_tokens_when_window_unknown() {
        use crate::agent::acp::TokenUsage;

        let mut usage = SessionUsage::default();
        usage.apply_tokens(TokenUsage {
            total_tokens: 12_400,
            input_tokens: 10_000,
            output_tokens: 2_400,
            thought_tokens: None,
            cached_read_tokens: None,
            cached_write_tokens: None,
        });
        let line = prompt_usage_title(&usage, 40).expect("should show token fallback");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " tok 12k ");
    }

    #[test]
    fn render_shows_prompt_cache_countdown_expired_and_model_switch_cold() {
        use crate::agent::acp::{CacheTtl, UsageUpdate};
        use std::time::{Duration, Instant};

        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/work"),
            PathBuf::from("/wt"),
            crate::store::Store::new(tempfile::tempdir().unwrap().keep()),
            tx,
        );
        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.usage.apply_update(UsageUpdate {
            used: 84_000,
            size: 200_000,
            cost: Some(CostInfo {
                amount: 0.14,
                currency: "USD".into(),
            }),
        });
        // Warm 5m cache (just warmed).
        s.usage
            .cache
            .record(Some(CacheTtl::FiveMinutes), Instant::now());
        app.sessions.push(s);

        let mut term = Terminal::new(TestBackend::new(96, 24)).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let warm_screen = dump(&term);
        assert!(
            warm_screen.contains("cache 4m") || warm_screen.contains("cache 5m"),
            "{warm_screen}"
        );

        // Usage modal shows warm + TTL tier.
        app.mode = Mode::Usage;
        term.draw(|f| render(f, &app)).unwrap();
        let warm_modal = dump(&term);
        assert!(warm_modal.contains("prompt cache"), "{warm_modal}");
        assert!(warm_modal.contains("warm ·"), "{warm_modal}");
        assert!(warm_modal.contains("(5m TTL)"), "{warm_modal}");

        // Expired cache (warmed 6 minutes ago with 5m TTL).
        app.mode = Mode::Normal;
        app.sessions[0].usage.cache.warmed_at =
            Instant::now().checked_sub(Duration::from_secs(360));
        term.draw(|f| render(f, &app)).unwrap();
        let exp_screen = dump(&term);
        assert!(exp_screen.contains("cache expired"), "{exp_screen}");

        app.mode = Mode::Usage;
        term.draw(|f| render(f, &app)).unwrap();
        let exp_modal = dump(&term);
        assert!(
            exp_modal.contains("expired (5m TTL elapsed)"),
            "{exp_modal}"
        );

        // Model-switch invalidation shows `cache cold`.
        app.mode = Mode::Normal;
        app.sessions[0].usage.invalidate_cache_for_model_switch();
        term.draw(|f| render(f, &app)).unwrap();
        let cold_screen = dump(&term);
        assert!(cold_screen.contains("cache cold"), "{cold_screen}");

        app.mode = Mode::Usage;
        term.draw(|f| render(f, &app)).unwrap();
        let cold_modal = dump(&term);
        assert!(
            cold_modal.contains("cold (invalidated by model switch)"),
            "{cold_modal}"
        );
    }
}
