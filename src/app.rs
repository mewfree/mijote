//! TUI application state and key handling. No rendering here.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::agent::{self, AgentSpec};
use crate::session::{Msg, Session, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Picker,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub mode: Mode,
    pub input: String,
    /// Agents offered by the new-session picker.
    pub picker: Vec<&'static AgentSpec>,
    pub picker_cursor: usize,
    /// Lines scrolled up from the bottom of the transcript (0 = follow).
    pub scroll: u16,
    pub should_quit: bool,
    cwd: PathBuf,
    ui_tx: Sender<Msg>,
    next_id: usize,
}

impl App {
    pub fn new(cwd: PathBuf, ui_tx: Sender<Msg>) -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            picker: Vec::new(),
            picker_cursor: 0,
            scroll: 0,
            should_quit: false,
            cwd,
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
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == msg.session) {
            s.apply(msg.body);
        }
    }

    pub fn open_session(&mut self, spec: &'static AgentSpec) {
        let id = self.next_id;
        self.next_id += 1;
        self.sessions.push(Session::spawn(
            id,
            spec,
            self.cwd.clone(),
            self.ui_tx.clone(),
        ));
        self.selected = self.sessions.len() - 1;
        self.scroll = 0;
        self.mode = Mode::Insert;
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            match self.current_mut() {
                Some(s) if matches!(s.status, Status::Running | Status::AwaitingPermission) => {
                    s.cancel()
                }
                _ => self.should_quit = true,
            }
            return;
        }
        // A pending permission request owns the keyboard.
        if self.current().is_some_and(|s| s.pending.is_some()) {
            self.handle_permission_key(key);
            return;
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Picker => self.handle_picker(key),
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
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('n') => self.open_picker(),
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
            KeyCode::Esc => {
                if let Some(s) = self.current_mut() {
                    s.cancel();
                }
            }
            _ => {}
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
                if let Some(s) = self.current_mut() {
                    if s.can_prompt() {
                        s.send_prompt(text);
                        self.input.clear();
                        self.scroll = 0;
                    }
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn open_picker(&mut self) {
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
            self.open_session(spec);
        }
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
        let mut app = App::new(PathBuf::from("."), tx);
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
    fn q_quits_from_normal_mode() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }
}
