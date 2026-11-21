//! ratatui rendering. Reads `App`, never mutates it.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
};
use ratatui::Frame;

mod markdown;

use crate::agent::acp::ToolContent;
use crate::app::{App, Confirm, Mode};
use crate::session::{Entry, Session, Status};

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
    let [header, transcript, input, status] = layout(f.area(), app.input.line_count());
    render_header(f, app, header);
    render_transcript(f, app, transcript);
    render_input(f, app, input);
    render_status(f, app, status);

    if app.mode == Mode::Picker {
        render_picker(f, app);
    }
    if app.mode == Mode::Model {
        render_models(f, app);
    }
    if app.mode == Mode::Switcher {
        render_switcher(f, app);
    }
    if app.mode == Mode::Help {
        render_help(f);
    }
    if let Some(s) = app.current() {
        if s.pending.is_some() {
            render_permission(f, s);
        }
    }
    if let Some(c) = app.confirm {
        render_confirm(f, c);
    }
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
        Status::AwaitingPermission => ("◆", ASK),
        Status::Failed => ("✗", ERR),
    }
}

fn status_label(s: &Session) -> (String, Color) {
    match s.status {
        Status::Starting => ("starting…".into(), WARN),
        Status::Idle => ("idle".into(), OK),
        Status::Running => (format!("working {}", elapsed(s)), ACCENT),
        Status::AwaitingPermission => ("needs permission".into(), ASK),
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

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::BOTTOM).border_style(DIM);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(s) = app.current() else { return };

    let place = match &s.worktree {
        Some(w) => format!("⎇ {}", w.branch),
        None => "current directory".to_string(),
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(s.spec.name, BOLD),
            Span::styled(format!("  {place}"), DIM),
            Span::styled(
                s.model_name().map_or(String::new(), |m| format!("  · {m}")),
                DIM,
            ),
        ])),
        inner,
    );

    let (label, color) = status_label(s);
    let mut right = vec![Span::styled(label, Style::new().fg(color))];
    let waiting = app
        .sessions
        .iter()
        .enumerate()
        .filter(|(i, s)| *i != app.selected && s.status == Status::AwaitingPermission)
        .count();
    if waiting > 0 {
        right.insert(
            0,
            Span::styled(
                format!("◆ {waiting} waiting · s switch   "),
                Style::new().fg(ASK),
            ),
        );
    }
    if app.scroll > 0 {
        right.insert(
            0,
            Span::styled("↓ scrolled · G follows   ", Style::new().fg(WARN)),
        );
    }
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

/// Transcript body for one session, already wrapped to `width` columns, so
/// scrolling is exact and every wrapped line keeps its gutter.
fn transcript_lines(s: &Session, width: usize, show_tool_details: bool) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
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
                for l in markdown::render(t, body) {
                    let mut spans = vec![Span::raw("    ")];
                    spans.extend(l.spans);
                    out.push(Line::from(spans));
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
    out
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

fn render_transcript(f: &mut Frame, app: &App, area: Rect) {
    let inner = area;
    let Some(s) = app.current() else {
        render_welcome(f, inner);
        return;
    };
    if s.entries.is_empty() {
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

    let mut lines = transcript_lines(s, inner.width as usize, app.show_tool_details);
    lines.extend(activity_line(s));
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner.height);
    app.max_scroll.set(max_scroll);
    let from_top = max_scroll.saturating_sub(app.scroll.min(max_scroll));
    let text: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    f.render_widget(Paragraph::new(lines).scroll((from_top, 0)), inner);

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
                f.set_cursor_position((x, inner.y + (crow - first) as u16));
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

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let (badge, badge_bg) = match app.mode {
        _ if app.leader => (" LEADER ", ASK),
        Mode::Normal => (" NORMAL ", ACCENT),
        Mode::Insert => (" INSERT ", OK),
        Mode::Picker => (" PICK ", ASK),
        Mode::Model => (" MODEL ", ASK),
        Mode::Switcher => (" SESSIONS ", ASK),
        Mode::Help => (" HELP ", ASK),
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
    let pending = app.input.pending();
    if !pending.is_empty() {
        spans.push(Span::styled(format!("{pending}  "), Style::new().fg(WARN)));
    }
    if !app.leader && app.confirm.is_none() && app.current().is_none_or(|s| s.pending.is_none()) {
        if let Some(m) = app.message() {
            spans.push(Span::styled(m, Style::new().fg(WARN)));
            f.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
    }

    let busy = app
        .current()
        .is_some_and(|s| matches!(s.status, Status::Running | Status::AwaitingPermission));
    let hints: &[(&'static str, &'static str)] = match app.mode {
        _ if app.current().is_some_and(|s| s.pending.is_some()) => &[
            ("a", "allow"),
            ("r", "reject"),
            ("1-9", "choose"),
            ("Esc", "cancel"),
        ],
        _ if app.confirm.is_some() => &[("y", "confirm"), ("any other key", "cancel")],
        _ if app.leader => &[
            ("n", "new"),
            ("N", "new here"),
            ("d", "close"),
            ("M", "model"),
            ("s", "sessions"),
            ("t", "tools"),
            ("j/k", "select"),
            ("g/G", "top/bottom"),
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
            ("PgUp/PgDn", "scroll"),
            ("i", "insert"),
            ("␣", "commands"),
            ("?", "help"),
        ],
        Mode::Normal if busy => &[
            ("^C", "cancel turn"),
            ("␣", "commands"),
            ("PgUp/PgDn", "scroll"),
            ("i", "insert"),
            ("?", "help"),
        ],
        Mode::Normal => &[
            ("i", "insert"),
            ("␣", "commands"),
            ("PgUp/PgDn", "scroll"),
            ("?", "help"),
            ("^C", "quit"),
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
            ("^A", "commands"),
            ("Esc", "normal"),
        ],
        Mode::Picker | Mode::Model | Mode::Switcher => {
            &[("j/k", "move"), ("Enter/1-9", "choose"), ("Esc", "close")]
        }
        Mode::Help => &[("Esc/q/?", "close")],
    };
    for (i, (k, what)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(key_hint(k, what));
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

fn render_picker(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 52, app.picker.len() as u16 + 4);
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
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", i + 1), DIM),
                Span::raw(a.name),
            ]))
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
    let area = centered(f.area(), 56, rows + 4);
    let w = area.width.saturating_sub(10) as usize;
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (g, c) = status_glyph(s.status);
            let g = if s.status == Status::Running {
                spinner(s)
            } else {
                g
            };
            let detail = match s.status {
                Status::Idle => match &s.worktree {
                    Some(wt) => format!("⎇ {}", wt.branch),
                    None => "current directory".to_string(),
                },
                _ => status_label(s).0,
            };
            let detail_color = match s.status {
                Status::Idle | Status::Starting => Color::DarkGray,
                _ => c,
            };
            let name = s.spec.name;
            let room = w.saturating_sub(name.chars().count() + 2);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", i + 1), DIM),
                Span::styled(format!("{g} "), Style::new().fg(c)),
                Span::raw(name),
                Span::styled(
                    format!("  {}", fit(&detail, room)),
                    Style::new().fg(detail_color),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.switcher_cursor));
    let list = List::new(items)
        .block(modal(
            "sessions",
            ACCENT,
            hint_line(&[("Enter", "switch"), ("Esc", "close")]),
        ))
        .highlight_symbol("▌ ")
        .highlight_style(BOLD.fg(ACCENT).bg(SELECTED_BG));
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
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
        row("d c y u C-r", "operators, undo/redo & vim motions"),
        Line::from(""),
        heading("commands (Space in normal · C-a anywhere)"),
        pair("n", "new worktree", "N", "new here"),
        pair("s / Tab", "switcher", "j / k", "next / prev"),
        pair("d", "close session", "M", "model (/model)"),
        pair("g / G", "top / bottom", "t", "tool details"),
        pair("m", "toggle mouse", "? / q", "help / quit"),
        Line::from(""),
        heading("session & scroll"),
        row("PgUp / PgDn", "scroll transcript (or mouse wheel)"),
        row("drag", "select and copy transcript text"),
        row("C-c / C-d", "cancel turn or quit / quit now"),
    ];

    let area = centered(f.area(), 68, lines.len() as u16 + 4);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(modal("help", ACCENT, hint_line(&[("Esc/q/?", "close")]))),
        area,
    );
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
}
