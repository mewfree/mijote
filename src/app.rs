//! TUI application state and key handling. No rendering here.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::agent::{self, AgentSpec};
use crate::session::{Entry, Msg, Session, Status, UiMsg};
use crate::store::Store;
use crate::ui::{self, Region};
use crate::worktree::Worktree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Picker,
}

/// A destructive action waiting for the user to confirm it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    CloseSession,
    Quit,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub mode: Mode,
    pub input: String,
    /// Agents offered by the new-session picker.
    pub picker: Vec<&'static AgentSpec>,
    pub picker_cursor: usize,
    /// Whether the picked session gets its own worktree (`n`) or runs
    /// directly in the current directory/branch (`N`).
    pub picker_isolated: bool,
    /// Lines scrolled up from the bottom of the transcript (0 = follow).
    pub scroll: u16,
    pub should_quit: bool,
    /// Whether the terminal reports mouse events (`m` toggles; off lets the
    /// terminal select text natively).
    pub mouse: bool,
    /// Action awaiting y/n; owns the keyboard while set.
    pub confirm: Option<Confirm>,
    /// One-line notice shown in the status bar until the next key press.
    pub message: Option<String>,
    cwd: PathBuf,
    worktrees_root: PathBuf,
    store: Store,
    ui_tx: Sender<Msg>,
    next_id: usize,
}

impl App {
    pub fn new(cwd: PathBuf, worktrees_root: PathBuf, store: Store, ui_tx: Sender<Msg>) -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            picker: Vec::new(),
            picker_cursor: 0,
            picker_isolated: true,
            scroll: 0,
            should_quit: false,
            mouse: true,
            confirm: None,
            message: None,
            cwd,
            worktrees_root,
            store,
            ui_tx,
            next_id: 0,
        }
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn current(&self) -> Option<&Session> {
        self.sessions.get(self.selected)
    }

    fn current_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.selected)
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        if let UiMsg::Closed(text) = msg.body {
            self.message = Some(text);
            return;
        }
        let settled = matches!(msg.body, UiMsg::TurnDone(_) | UiMsg::Error(_));
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == msg.session) {
            s.apply(msg.body);
            if settled {
                self.persist(msg.session);
            }
        }
    }

    /// Write one session's record. A failure is shown, never fatal.
    fn persist(&mut self, id: usize) {
        let Some(s) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        if s.entries.is_empty() {
            return;
        }
        if let Err(e) = self.store.save(&s.key, &s.record(&self.cwd)) {
            self.message = Some(format!("could not save session: {e}"));
        }
    }

    /// Reopen sessions saved for this directory. Each gets a fresh agent
    /// behind its old transcript; the agent does not remember it.
    pub fn restore(&mut self) {
        for (key, rec) in self.store.load(&self.cwd) {
            let Some(spec) = agent::find(&rec.agent) else {
                continue;
            };
            let mut note =
                "restored from a previous run; the agent does not remember it".to_string();
            let worktree = match rec.worktree {
                Some(w) if w.path.is_dir() => Some(w),
                Some(w) => {
                    note.push_str(&format!(
                        "; worktree {} is gone, running in the current directory",
                        w.path.display()
                    ));
                    None
                }
                None => None,
            };
            let id = self.next_id;
            self.next_id += 1;
            let mut s = Session::spawn(id, spec, self.cwd.clone(), worktree, self.ui_tx.clone());
            s.key = key;
            s.entries = rec.entries;
            s.entries.push(Entry::Notice(note));
            self.sessions.push(s);
        }
    }

    pub fn open_session(&mut self, spec: &'static AgentSpec, isolated: bool) {
        let id = self.next_id;
        self.next_id += 1;
        let worktree = if isolated {
            match Worktree::create(&self.cwd, &self.worktrees_root, spec.id) {
                Ok(w) => Some(w),
                Err(e) => {
                    self.message = Some(format!(
                        "no worktree ({e}); running in the current directory"
                    ));
                    None
                }
            }
        } else {
            None
        };
        self.sessions.push(Session::spawn(
            id,
            spec,
            self.cwd.clone(),
            worktree,
            self.ui_tx.clone(),
        ));
        self.selected = self.sessions.len() - 1;
        self.scroll = 0;
        self.mode = Mode::Insert;
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.message = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            match self.current_mut() {
                Some(s) if matches!(s.status, Status::Running | Status::AwaitingPermission) => {
                    s.cancel()
                }
                _ => self.request_quit(),
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            self.should_quit = true;
            return;
        }
        // A pending permission request owns the keyboard.
        if self.current().is_some_and(|s| s.pending.is_some()) {
            self.handle_permission_key(key);
            return;
        }
        if let Some(action) = self.confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                match action {
                    Confirm::CloseSession => self.close_selected(),
                    Confirm::Quit => self.should_quit = true,
                }
            }
            return;
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Picker => self.handle_picker(key),
        }
    }

    /// React to a mouse event on a terminal of `area`. Modals (picker,
    /// permission, confirm) own the screen, so the mouse is ignored then.
    pub fn handle_mouse(&mut self, ev: MouseEvent, area: Rect) {
        if self.mode == Mode::Picker
            || self.confirm.is_some()
            || self.current().is_some_and(|s| s.pending.is_some())
        {
            return;
        }
        let region = ui::locate(area, ev.column, ev.row);
        match ev.kind {
            MouseEventKind::ScrollUp if region != Region::Other => {
                self.scroll = self.scroll.saturating_add(3)
            }
            MouseEventKind::ScrollDown if region != Region::Other => {
                self.scroll = self.scroll.saturating_sub(3)
            }
            MouseEventKind::Down(MouseButton::Left) => match region {
                Region::Session(i) if i < self.sessions.len() => {
                    if i != self.selected {
                        self.selected = i;
                        self.scroll = 0;
                    }
                }
                Region::Input if !self.sessions.is_empty() => self.mode = Mode::Insert,
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_permission_key(&mut self, key: KeyEvent) {
        let Some(s) = self.current_mut() else { return };
        let Some(req) = &s.pending else { return };
        let choice = match key.code {
            KeyCode::Char(c) => c
                .to_digit(10)
                .and_then(|n| (n as usize).checked_sub(1))
                .and_then(|i| req.options.get(i))
                .map(|o| Some(o.id.clone())),
            KeyCode::Esc => Some(
                req.options
                    .iter()
                    .find(|o| o.kind.starts_with("reject"))
                    .map(|o| o.id.clone()),
            ),
            _ => None,
        };
        match choice {
            Some(Some(id)) => s.answer_permission(Some(id)),
            // No reject option offered: cancel the whole turn.
            Some(None) => s.cancel(),
            None => {}
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('n') => self.open_picker(true),
            KeyCode::Char('N') => self.open_picker(false),
            KeyCode::Char('d') => self.request_close(),
            KeyCode::Char('m') => {
                self.mouse = !self.mouse;
                self.message = Some(
                    if self.mouse {
                        "mouse on"
                    } else {
                        "mouse off: terminal text selection works"
                    }
                    .into(),
                );
            }
            KeyCode::Char('i') | KeyCode::Enter => {
                if !self.sessions.is_empty() {
                    self.mode = Mode::Insert;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.select(1),
            KeyCode::Char('k') | KeyCode::Up => self.select(-1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('G') | KeyCode::End => self.scroll = 0,
            _ => {}
        }
    }

    /// Quit now, or ask first when an agent is mid-turn.
    fn request_quit(&mut self) {
        let busy = self
            .sessions
            .iter()
            .any(|s| matches!(s.status, Status::Running | Status::AwaitingPermission));
        if busy {
            self.confirm = Some(Confirm::Quit);
        } else {
            self.should_quit = true;
        }
    }

    /// Close the selected session, asking first if it is running or has history.
    fn request_close(&mut self) {
        let Some(s) = self.current() else { return };
        let busy = matches!(s.status, Status::Running | Status::AwaitingPermission);
        if busy || !s.entries.is_empty() {
            self.confirm = Some(Confirm::CloseSession);
        } else {
            self.close_selected();
        }
    }

    fn handle_insert(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return;
                }
                let mut sent = None;
                if let Some(s) = self.current_mut() {
                    if s.can_prompt() {
                        s.send_prompt(text);
                        sent = Some(s.id);
                    }
                }
                if let Some(id) = sent {
                    self.input.clear();
                    self.scroll = 0;
                    self.persist(id);
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn open_picker(&mut self, isolated: bool) {
        self.picker_isolated = isolated;
        self.picker = agent::KNOWN
            .iter()
            .filter(|a| {
                !matches!(a.install, agent::Install::Manual(_))
                    || agent::install::locate(a).is_some()
            })
            .collect();
        self.picker_cursor = 0;
        self.mode = Mode::Picker;
    }

    fn handle_picker(&mut self, key: KeyEvent) {
        let n = self.picker.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if n > 0 => {
                self.picker_cursor = (self.picker_cursor + 1) % n
            }
            KeyCode::Char('k') | KeyCode::Up if n > 0 => {
                self.picker_cursor = (self.picker_cursor + n - 1) % n
            }
            KeyCode::Enter => self.pick(self.picker_cursor),
            KeyCode::Char(c) => {
                if let Some(i) = c.to_digit(10).and_then(|d| (d as usize).checked_sub(1)) {
                    self.pick(i);
                }
            }
            _ => {}
        }
    }

    fn pick(&mut self, i: usize) {
        if let Some(spec) = self.picker.get(i).copied() {
            self.open_session(spec, self.picker_isolated);
        }
    }

    /// Close every session (used on quit). Returns how many `Closed`
    /// notices to expect on the message channel.
    pub fn close_all(&mut self) -> usize {
        let n = self.sessions.len();
        // Save everything first: records are what `restore` reopens.
        for i in 0..n {
            self.persist(self.sessions[i].id);
        }
        for s in self.sessions.drain(..) {
            s.close(self.ui_tx.clone());
        }
        n
    }

    fn close_selected(&mut self) {
        if self.selected >= self.sessions.len() {
            return;
        }
        let s = self.sessions.remove(self.selected);
        self.store.remove(&s.key);
        self.selected = self
            .selected
            .saturating_sub(1)
            .min(self.sessions.len().saturating_sub(1));
        self.scroll = 0;
        self.message = Some(format!("closing {}…", s.spec.name));
        s.close(self.ui_tx.clone());
    }

    fn select(&mut self, delta: isize) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        self.scroll = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::acp::{PermissionOption, PermissionRequest};
    use crate::session::UiMsg;
    use std::sync::mpsc;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn app_with_session() -> (App, std::sync::mpsc::Receiver<Option<String>>) {
        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut app = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
        let (s, perm_rx) = Session::detached(0, agent::find("claude").unwrap());
        app.sessions.push(s);
        app.sessions[0].status = Status::Idle;
        (app, perm_rx)
    }

    #[test]
    fn typing_and_sending_a_prompt() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.mode, Mode::Insert);
        for c in "hi".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.input.is_empty());
        assert_eq!(app.sessions[0].status, Status::Running);
    }

    #[test]
    fn permission_takes_over_keyboard() {
        let (mut app, perm_rx) = app_with_session();
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(PermissionRequest {
                tool_call_id: "c".into(),
                title: Some("rm -rf".into()),
                options: vec![
                    PermissionOption {
                        id: "ok".into(),
                        name: "Allow".into(),
                        kind: "allow_once".into(),
                    },
                    PermissionOption {
                        id: "no".into(),
                        name: "Reject".into(),
                        kind: "reject_once".into(),
                    },
                ],
            }),
        });
        // 'q' must not quit while a permission is pending.
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("no".into()));
    }

    #[test]
    fn closed_notice_sets_status_message() {
        let (mut app, _) = app_with_session();
        app.handle_msg(Msg {
            session: 7,
            body: UiMsg::Closed("done".into()),
        });
        assert_eq!(app.message.as_deref(), Some("done"));
    }

    #[test]
    fn q_quits_from_normal_mode() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn q_asks_before_quitting_a_running_turn() {
        let (mut app, _) = app_with_session();
        app.sessions[0].status = Status::Running;
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert_eq!(app.confirm, Some(Confirm::Quit));
        app.handle_key(key(KeyCode::Char('n')));
        assert!(!app.should_quit && app.confirm.is_none());
        app.handle_key(key(KeyCode::Char('q')));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.should_quit);
    }

    #[test]
    fn d_confirms_when_session_has_history() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.sessions.len(), 0);

        let (mut app, _) = app_with_session();
        app.sessions[0]
            .entries
            .push(crate::session::Entry::Agent("x".into()));
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.confirm, Some(Confirm::CloseSession));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.sessions.len(), 0);
    }

    #[test]
    fn transcripts_persist_and_closing_forgets_them() {
        let (mut app, _) = app_with_session();
        app.sessions[0].key = "k".into();
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Event(crate::agent::acp::Event::AgentText("hi".into())),
        });
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::TurnDone(crate::agent::acp::StopReason::EndTurn),
        });
        let saved = app.store.load(app.cwd());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].1.entries, vec![Entry::Agent("hi".into())]);

        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.store.load(app.cwd()).is_empty());
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_selects_sessions_and_scrolls() {
        let area = Rect::new(0, 0, 100, 30);
        let (mut app, _) = app_with_session();
        let (s, _) = Session::detached(1, agent::find("gemini").unwrap());
        app.sessions.push(s);
        // Sidebar rows start under the " sessions " title row.
        app.handle_mouse(click(2, 2), area);
        assert_eq!(app.selected, 1);
        app.handle_mouse(click(2, 20), area); // empty row: no change
        assert_eq!(app.selected, 1);

        let wheel = |kind| MouseEvent {
            kind,
            column: 50,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(wheel(MouseEventKind::ScrollUp), area);
        assert_eq!(app.scroll, 3);
        app.handle_mouse(wheel(MouseEventKind::ScrollDown), area);
        assert_eq!(app.scroll, 0);

        app.handle_mouse(click(50, 28), area); // prompt box
        assert_eq!(app.mode, Mode::Insert);
    }

    #[test]
    fn m_toggles_mouse_capture() {
        let (mut app, _) = app_with_session();
        assert!(app.mouse);
        app.handle_key(key(KeyCode::Char('m')));
        assert!(!app.mouse);
    }

    #[test]
    fn esc_in_normal_mode_does_not_cancel_the_turn() {
        let (mut app, _) = app_with_session();
        app.sessions[0].status = Status::Running;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.sessions[0].status, Status::Running);
    }

    #[test]
    fn ctrl_d_quits_from_any_mode() {
        let (mut app, _) = app_with_session();
        app.mode = Mode::Insert;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }
}
