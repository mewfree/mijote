//! ratatui rendering. Reads `App`, never mutates it.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Mode};
use crate::session::{Entry, Session, Status};

const DIM: Style = Style::new().fg(Color::DarkGray);

pub fn render(f: &mut Frame, app: &App) {
    let [side, main] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(30)]).areas(f.area());
    render_sidebar(f, app, side);

    let [transcript, input, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(main);
    render_transcript(f, app, transcript);
    render_input(f, app, input);
    render_status(f, app, status);

    if app.mode == Mode::Picker {
        render_picker(f, app);
    }
    if let Some(s) = app.current() {
        if s.pending.is_some() {
            render_permission(f, s);
        }
    }
}

fn status_glyph(s: Status) -> (&'static str, Color) {
    match s {
        Status::Starting => ("…", Color::Yellow),
        Status::Idle => ("●", Color::Green),
        Status::Running => ("▶", Color::Cyan),
        Status::AwaitingPermission => ("!", Color::Magenta),
        Status::Failed => ("✗", Color::Red),
    }
}

fn render_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let (g, c) = status_glyph(s.status);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{g} "), Style::new().fg(c)),
                Span::raw(s.spec.name),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if !app.sessions.is_empty() {
        state.select(Some(app.selected));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::RIGHT).title(" sessions "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut state);
}

fn entry_lines(e: &Entry) -> Vec<Line<'static>> {
    let text_lines = |t: &str, style: Style, prefix: &str| -> Vec<Line<'static>> {
        t.lines()
            .enumerate()
            .map(|(i, l)| {
                let p = if i == 0 { prefix } else { "  " };
                Line::from(vec![Span::styled(format!("{p}{l}"), style)])
            })
            .collect()
    };
    match e {
        Entry::User(t) => text_lines(
            t,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            "› ",
        ),
        Entry::Agent(t) => text_lines(t, Style::new(), ""),
        Entry::Thought(t) => text_lines(t, DIM.add_modifier(Modifier::ITALIC), "· "),
        Entry::Tool {
            title,
            kind,
            status,
            ..
        } => {
            let color = match status.as_str() {
                "completed" => Color::Green,
                "failed" => Color::Red,
                _ => Color::Yellow,
            };
            vec![Line::from(vec![
                Span::styled(format!("⚙ [{kind}] "), Style::new().fg(color)),
                Span::raw(title.clone()),
                Span::styled(format!("  {status}"), DIM),
            ])]
        }
        Entry::Notice(t) => text_lines(t, Style::new().fg(Color::Red), "! "),
    }
}

fn render_transcript(f: &mut Frame, app: &App, area: Rect) {
    let title = match app.current() {
        Some(s) => match &s.worktree {
            Some(w) => format!(" {} · {} ", s.spec.name, w.branch),
            None => format!(" {} · (current directory) ", s.spec.name),
        },
        None => " mijote ".to_string(),
    };
    let block = Block::default().borders(Borders::BOTTOM).title(title);
    let inner = block.inner(area);

    let lines: Vec<Line> = match app.current() {
        None => vec![
            Line::from(""),
            Line::from("No sessions yet. Press n to start one."),
        ],
        Some(s) if s.entries.is_empty() => {
            let msg = match s.status {
                Status::Starting => "starting agent (first run may install the adapter)…",
                Status::Failed => "agent failed to start",
                _ => "type a prompt (i), then Enter",
            };
            vec![Line::styled(msg, DIM)]
        }
        Some(s) => s
            .entries
            .iter()
            .flat_map(|e| {
                let mut l = entry_lines(e);
                l.push(Line::from(""));
                l
            })
            .collect(),
    };

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = para.line_count(inner.width) as u16;
    let max_scroll = total.saturating_sub(inner.height);
    let from_top = max_scroll.saturating_sub(app.scroll.min(max_scroll));
    f.render_widget(block, area);
    f.render_widget(para.scroll((from_top, 0)), inner);
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let active = app.mode == Mode::Insert;
    let style = if active {
        Style::new().fg(Color::Cyan)
    } else {
        DIM
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(" prompt ");
    let text = if active {
        format!("{}▏", app.input)
    } else {
        app.input.clone()
    };
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    if let Some(m) = &app.message {
        f.render_widget(
            Paragraph::new(format!(" {m}")).style(Style::new().fg(Color::Yellow)),
            area,
        );
        return;
    }
    let hint = match app.mode {
        Mode::Normal => {
            "n new  d close  j/k switch  i prompt  Esc/^C cancel  PgUp/PgDn scroll  q quit"
        }
        Mode::Insert => "Enter send  Esc normal  ^C cancel turn",
        Mode::Picker => "j/k move  Enter/1-9 choose  Esc close",
    };
    let cwd = app.cwd().display().to_string();
    f.render_widget(Paragraph::new(format!(" {hint}   {cwd}")).style(DIM), area);
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

fn render_picker(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 50, app.picker.len() as u16 + 4);
    let items: Vec<ListItem> = app
        .picker
        .iter()
        .enumerate()
        .map(|(i, a)| ListItem::new(format!("{}. {}", i + 1, a.name)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.picker_cursor));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" new session "),
        )
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut state);
}

fn render_permission(f: &mut Frame, s: &Session) {
    let Some(req) = &s.pending else { return };
    let mut lines = vec![
        Line::styled(
            req.title
                .clone()
                .unwrap_or_else(|| req.tool_call_id.clone()),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
    ];
    for (i, o) in req.options.iter().enumerate() {
        lines.push(Line::from(format!("{}. {}  [{}]", i + 1, o.name, o.kind)));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("Esc rejects", DIM));
    let area = centered(f.area(), 64, lines.len() as u16 + 2);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Magenta))
        .title(" permission requested ")
        .title_alignment(Alignment::Left);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
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
        let mut app = App::new(PathBuf::from("/work"), PathBuf::from("/wt"), tx);
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();

        term.draw(|f| render(f, &app)).unwrap();
        assert!(dump(&term).contains("Press n to start one"));

        let (mut s, _) = Session::detached(0, find("claude").unwrap());
        s.status = Status::Idle;
        s.entries.push(Entry::User("fix the bug".into()));
        s.entries.push(Entry::Agent("on it".into()));
        app.sessions.push(s);
        term.draw(|f| render(f, &app)).unwrap();
        let screen = dump(&term);
        assert!(screen.contains("› fix the bug"), "{screen}");
        assert!(screen.contains("on it"));
        assert!(screen.contains("Claude Code"));
    }
}
