//! ratatui rendering. Reads `App`, never mutates it.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
};
use ratatui::Frame;

mod markdown;

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

const SIDEBAR_WIDTH: u16 = 30;
/// Sidebar rows above the first session: wordmark and a spacer.
const SIDEBAR_HEAD: u16 = 2;
/// Sidebar rows per session: name line and detail line.
const SESSION_ROWS: u16 = 2;

/// Screen areas: sidebar, header, transcript, prompt box, status bar. Shared
/// by rendering and mouse hit-testing so they cannot drift apart.
fn layout(area: Rect) -> [Rect; 5] {
    let [side, main] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(30)]).areas(area);
    let [header, transcript, input, status] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(main);
    [side, header, transcript, input, status]
}

fn sidebar_block() -> Block<'static> {
    Block::default().borders(Borders::RIGHT).border_style(DIM)
}

/// What is under a mouse position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// A session card of the sidebar (may be past the last session).
    Session(usize),
    Transcript,
    Input,
    Other,
}

/// Map a terminal cell to the region it falls in, for a terminal of `area`.
pub fn locate(area: Rect, col: u16, row: u16) -> Region {
    let [side, _, transcript, input, _] = layout(area);
    let at = |r: Rect| r.contains((col, row).into());
    if at(side) {
        let inner = sidebar_block().inner(side);
        let first = inner.y + SIDEBAR_HEAD;
        if at(inner) && row >= first {
            return Region::Session(((row - first) / SESSION_ROWS) as usize);
        }
        return Region::Other;
    }
    if at(transcript) {
        Region::Transcript
    } else if at(input) {
        Region::Input
    } else {
        Region::Other
    }
}

pub fn render(f: &mut Frame, app: &App) {
    let [side, header, transcript, input, status] = layout(f.area());
    render_sidebar(f, app, side);
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

fn render_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let block = sidebar_block();
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width as usize;

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(" ◆ ", Style::new().fg(ACCENT)),
            Span::styled("mijote", BOLD),
        ]),
        Line::from(""),
    ];
    for (i, s) in app.sessions.iter().enumerate() {
        let selected = i == app.selected;
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
            _ => status_glyph(s.status).1,
        };
        let bar = if selected { "▌" } else { " " };
        let name_style = if selected { BOLD } else { Style::new() };
        let mut name = Line::from(vec![
            Span::styled(bar, Style::new().fg(ACCENT)),
            Span::styled(format!(" {g} "), Style::new().fg(c)),
            Span::styled(fit(s.spec.name, w.saturating_sub(4)), name_style),
        ]);
        let mut sub = Line::from(vec![
            Span::styled(bar, Style::new().fg(ACCENT)),
            Span::raw("   "),
            Span::styled(
                fit(&detail, w.saturating_sub(4)),
                Style::new().fg(detail_color),
            ),
        ]);
        if selected {
            for l in [&mut name, &mut sub] {
                let used: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
                l.spans.push(Span::raw(" ".repeat(w.saturating_sub(used))));
                *l = std::mem::take(l).style(Style::new().bg(SELECTED_BG));
            }
        }
        lines.push(name);
        lines.push(sub);
    }
    if app.sessions.is_empty() {
        lines.push(Line::styled("  no sessions", DIM));
    }
    f.render_widget(Paragraph::new(lines), inner);

    if inner.height > 6 {
        let foot = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled("n", BOLD),
                Span::styled(" new  ", DIM),
                Span::styled("N", BOLD),
                Span::styled(" here  ", DIM),
                Span::styled("d", BOLD),
                Span::styled(" close", DIM),
            ])),
            foot,
        );
    }
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

/// Transcript body for one session, already wrapped to `width` columns, so
/// scrolling is exact and every wrapped line keeps its gutter.
fn transcript_lines(s: &Session, width: usize) -> Vec<Line<'static>> {
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
                ..
            } => {
                let (g, c) = tool_glyph(s, status);
                let kind_txt = format!("  {kind}");
                let room = width.saturating_sub(4 + kind_txt.chars().count());
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{g} "), Style::new().fg(c)),
                    Span::raw(fit(title, room)),
                    Span::styled(kind_txt, DIM),
                ]));
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

fn render_welcome(f: &mut Frame, area: Rect) {
    let rows = [
        ("n", "new isolated session (git worktree)"),
        ("N", "new session in the current directory"),
        ("q", "quit"),
    ];
    let mut lines = vec![
        Line::from(vec![
            Span::styled("◆ ", Style::new().fg(ACCENT)),
            Span::styled("mijote", BOLD),
        ]),
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
    let r = centered(area, 52, lines.len() as u16);
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
            _ => "press i to write a prompt, Enter to send",
        };
        f.render_widget(Paragraph::new(Line::styled(format!("  {msg}"), DIM)), inner);
        return;
    }

    let mut lines = transcript_lines(s, inner.width as usize);
    lines.extend(activity_line(s));
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner.height);
    app.max_scroll.set(max_scroll);
    let from_top = max_scroll.saturating_sub(app.scroll.min(max_scroll));
    f.render_widget(Paragraph::new(lines).scroll((from_top, 0)), inner);
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

    let room = (inner.width as usize).saturating_sub(3);
    let tail = |t: &str| {
        let n = t.chars().count();
        t.chars().skip(n.saturating_sub(room)).collect::<String>()
    };
    let line = if app.input.is_empty() && !active {
        Line::from(vec![
            Span::styled("› ", DIM),
            Span::styled("press i to write a prompt", DIM),
        ])
    } else if active {
        Line::from(vec![
            Span::styled("› ", Style::new().fg(ACCENT)),
            Span::raw(tail(&app.input)),
            Span::styled("▏", Style::new().fg(ACCENT)),
        ])
    } else {
        Line::from(vec![Span::styled("› ", DIM), Span::raw(tail(&app.input))])
    };
    f.render_widget(Paragraph::new(line), inner);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let (badge, badge_bg) = match app.mode {
        Mode::Normal => (" NORMAL ", ACCENT),
        Mode::Insert => (" INSERT ", OK),
        Mode::Picker => (" PICK ", ASK),
        Mode::Model => (" MODEL ", ASK),
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
    if let Some(m) = &app.message {
        spans.push(Span::styled(m.clone(), Style::new().fg(WARN)));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let busy = app
        .current()
        .is_some_and(|s| matches!(s.status, Status::Running | Status::AwaitingPermission));
    let hints: &[(&'static str, &'static str)] = match app.mode {
        _ if app.confirm.is_some() => &[("y", "confirm"), ("any other key", "cancel")],
        Mode::Normal if app.scroll > 0 => &[
            ("G", "follow"),
            ("PgUp/PgDn", "scroll"),
            ("i", "prompt"),
            ("n", "new"),
            ("q", "quit"),
        ],
        Mode::Normal if busy => &[
            ("^C", "cancel turn"),
            ("j/k", "switch"),
            ("PgUp/PgDn", "scroll"),
            ("n", "new"),
            ("q", "quit"),
        ],
        Mode::Normal => &[
            ("i", "prompt"),
            ("n", "new"),
            ("N", "new here"),
            ("d", "close"),
            ("M", "model"),
            ("j/k", "switch"),
            ("PgUp/PgDn", "scroll"),
            ("m", "mouse"),
            ("q", "quit"),
        ],
        Mode::Insert if busy => &[
            ("Enter", "send when idle"),
            ("^C", "cancel turn"),
            ("Esc", "normal"),
        ],
        Mode::Insert => &[("Enter", "send"), ("Esc", "normal"), ("^C", "quit")],
        Mode::Picker | Mode::Model => &[("j/k", "move"), ("Enter/1-9", "choose"), ("Esc", "close")],
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

fn render_permission(f: &mut Frame, s: &Session) {
    let Some(req) = &s.pending else { return };
    let mut lines = vec![
        Line::styled(
            req.title
                .clone()
                .unwrap_or_else(|| req.tool_call_id.clone()),
            BOLD,
        ),
        Line::from(""),
    ];
    for (i, o) in req.options.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", i + 1),
                Style::new().fg(ASK).add_modifier(Modifier::BOLD),
            ),
            Span::raw(o.name.clone()),
            Span::styled(format!("  {}", o.kind), DIM),
        ]));
    }
    let area = centered(f.area(), 68, lines.len() as u16 + 4);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(modal(
                "permission requested",
                ASK,
                hint_line(&[("1-9", "choose"), ("Esc", "reject")]),
            )),
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

        app.confirm = Some(Confirm::Quit);
        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("Quit and stop it?"));
    }

    #[test]
    fn wrap_breaks_words_and_long_tokens() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("a\n\nb", 5), vec!["a", "", "b"]);
        assert_eq!(wrap_code("  x = 1", 4), vec!["  x ", "= 1"]);
        assert_eq!(fit("abcdef", 4), "abc…");
    }
}
