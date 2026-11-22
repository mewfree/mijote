//! TUI application state and key handling. No rendering here.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::agent::acp::{ElicitationRequest, ElicitationResponse, Event, FieldKind};
use crate::agent::install::AgentStatus;
use crate::agent::{self, AgentSpec};
use crate::editor::Editor;
use crate::files::{self, FileMatch};
use crate::selection::Selection;
use crate::session::{Entry, Msg, Session, Status, UiMsg};
use crate::store::Store;
use crate::ui::{self, Region};
use crate::worktree::Worktree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Picker,
    /// Choosing a model for the current session.
    Model,
    /// Choosing a reasoning/effort level for the current session's model.
    Effort,
    /// Floating session list.
    Switcher,
    /// Key-binding help overlay.
    Help,
    /// Renaming a session's label.
    Rename,
    /// Searching the current session's transcript.
    Search,
}

/// A destructive action waiting for the user to confirm it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    CloseSession,
    Quit,
}

/// In-progress value for one field of an active interactive question modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldDraft {
    Single(Option<usize>),
    Multi(Vec<bool>),
    Text(String),
    Bool(bool),
}

/// Interactive state for the active session's pending `elicitation/create` modal.
#[derive(Debug, Clone)]
pub struct QuestionDraft {
    pub session_id: usize,
    pub field_idx: usize,
    pub option_idx: usize,
    pub values: Vec<FieldDraft>,
    pub error: Option<String>,
}

impl QuestionDraft {
    pub fn from_request(session_id: usize, req: &ElicitationRequest) -> Self {
        let values: Vec<FieldDraft> = req
            .fields
            .iter()
            .map(|f| match &f.kind {
                FieldKind::SingleSelect { options, default } => {
                    let sel = default
                        .as_deref()
                        .and_then(|d| options.iter().position(|o| o.value == d));
                    FieldDraft::Single(sel)
                }
                FieldKind::MultiSelect { options, default } => {
                    let checks = options
                        .iter()
                        .map(|o| default.iter().any(|d| d == &o.value))
                        .collect();
                    FieldDraft::Multi(checks)
                }
                FieldKind::Text { default } | FieldKind::Number { default, .. } => {
                    FieldDraft::Text(default.clone().unwrap_or_default())
                }
                FieldKind::Bool { default } => FieldDraft::Bool(default.unwrap_or(false)),
            })
            .collect();
        let option_idx = match values.first() {
            Some(FieldDraft::Single(Some(i))) => *i,
            _ => 0,
        };
        Self {
            session_id,
            field_idx: 0,
            option_idx,
            values,
            error: None,
        }
    }
}

/// Active `@` file-mention autocomplete state while typing in Insert mode.
#[derive(Debug, Clone)]
pub struct MentionPopup {
    /// Char index of the triggering `@` in `App::input`.
    pub at_start: usize,
    pub query: String,
    pub matches: Vec<FileMatch>,
    pub cursor: usize,
    pub total_files: usize,
}

/// One slash command shown in the `/` autocomplete popup (local built-in or
/// advertised by the agent via ACP `available_commands_update`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandEntry {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
    pub local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMatch {
    pub command: SlashCommandEntry,
    /// Character indices in `command.name` that matched the query (for highlighting).
    pub positions: Vec<usize>,
    pub score: i64,
}

/// Active `/` slash-command autocomplete state while typing in Insert mode.
#[derive(Debug, Clone)]
pub struct SlashPopup {
    /// Char index of the triggering `/` in `App::input`.
    pub slash_start: usize,
    pub query: String,
    pub matches: Vec<SlashMatch>,
    pub cursor: usize,
    pub total_commands: usize,
}

/// If the cursor sits inside a leading `/command` token on the first line,
/// returns `(slash_char_index, query)`.
pub fn active_slash_command(chars: &[char], cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(chars.len());
    let mut slash_start = 0;
    while slash_start < cursor && chars[slash_start] == ' ' {
        slash_start += 1;
    }
    if slash_start >= cursor || chars[slash_start] != '/' {
        return None;
    }
    if chars[slash_start + 1..cursor]
        .iter()
        .any(|c| c.is_whitespace())
    {
        return None;
    }
    let query: String = chars[slash_start + 1..cursor].iter().collect();
    Some((slash_start, query))
}

/// Build the combined slash-command list for `session`: local built-ins
/// (`/model`, `/effort`) alongside agent-advertised commands (deduplicated
/// case-insensitively with local commands taking precedence).
pub fn session_slash_commands(session: &Session) -> Vec<SlashCommandEntry> {
    let mut out = vec![
        SlashCommandEntry {
            name: "model".to_string(),
            description: "Switch session model".to_string(),
            input_hint: Some("[model]".to_string()),
            local: true,
        },
        SlashCommandEntry {
            name: "effort".to_string(),
            description: "Switch reasoning effort level".to_string(),
            input_hint: Some("[level]".to_string()),
            local: true,
        },
    ];
    for cmd in &session.commands {
        if out
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&cmd.name))
        {
            continue;
        }
        out.push(SlashCommandEntry {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            input_hint: cmd.input_hint.clone(),
            local: false,
        });
    }
    out
}

/// Rank and filter slash commands against `query`.
pub fn filter_slash_commands(commands: &[SlashCommandEntry], query: &str) -> Vec<SlashMatch> {
    let query = query.trim();
    if query.is_empty() {
        return commands
            .iter()
            .cloned()
            .map(|command| SlashMatch {
                command,
                positions: Vec::new(),
                score: 0,
            })
            .collect();
    }
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let needle_str: String = needle.iter().collect();
    let mut scored: Vec<SlashMatch> = commands
        .iter()
        .filter_map(|cmd| score_slash_command(cmd, &needle, &needle_str))
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.command.local.cmp(&a.command.local))
            .then_with(|| a.command.name.cmp(&b.command.name))
    });
    scored
}

fn score_slash_command(
    cmd: &SlashCommandEntry,
    needle: &[char],
    needle_str: &str,
) -> Option<SlashMatch> {
    let chars: Vec<char> = cmd.name.chars().collect();
    let lowered: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    if let Some(positions) = find_char_subsequence(&lowered, needle) {
        let mut score: i64 = 1_000;
        if lowered == needle {
            score += 200;
        } else if lowered.starts_with(needle) {
            score += 100;
        }
        let mut prev: Option<usize> = None;
        for &idx in &positions {
            score += 10;
            if idx == 0 {
                score += 25;
            } else if matches!(chars[idx - 1], '-' | '_' | ':' | '.' | '/') {
                score += 15;
            }
            if let Some(p) = prev {
                if idx == p + 1 {
                    score += 12;
                } else {
                    score -= (idx - p - 1).min(10) as i64;
                }
            }
            prev = Some(idx);
        }
        score -= (chars.len() as i64).min(100) / 4;
        return Some(SlashMatch {
            command: cmd.clone(),
            positions,
            score,
        });
    }

    if needle.len() >= 2 {
        let desc_lower = cmd.description.to_lowercase();
        if let Some(byte_pos) = desc_lower.find(needle_str) {
            let char_pos = desc_lower[..byte_pos].chars().count();
            return Some(SlashMatch {
                command: cmd.clone(),
                positions: Vec::new(),
                score: 100 - (char_pos as i64).min(80),
            });
        }
    }

    None
}

fn find_char_subsequence(haystack: &[char], needle: &[char]) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(needle.len());
    let mut ni = 0;
    for (hi, &ch) in haystack.iter().enumerate() {
        if ch == needle[ni] {
            positions.push(hi);
            ni += 1;
            if ni == needle.len() {
                return Some(positions);
            }
        }
    }
    None
}

/// A paste with more lines or characters than this is collapsed to a placeholder.
const PASTE_COLLAPSE_LINES: usize = 3;
const PASTE_COLLAPSE_CHARS: usize = 500;

/// How long a transient status-bar notice stays visible before expiring.
const NOTICE_TTL: Duration = Duration::from_secs(4);

struct Notice {
    text: String,
    expires_at: Instant,
}

struct HistoryDraft {
    text: String,
    cursor: usize,
    pastes: Vec<(String, String)>,
}

struct HistoryNav {
    /// Index into `App::history` currently loaded in `App::input`.
    index: usize,
    /// Prompt state saved before entering history navigation.
    draft: HistoryDraft,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub mode: Mode,
    pub input: Editor,
    /// Previously submitted prompts, oldest first (persisted in `store`).
    history: Vec<String>,
    /// Active prompt-history browsing state (`Up`/`Down` or Normal-mode `k`/`j`).
    history_nav: Option<HistoryNav>,
    /// Active `@` file-mention completion popup in Insert mode.
    pub mention: Option<MentionPopup>,
    /// Cached file list for the currently open `@` completion session.
    mention_files: Option<(PathBuf, Vec<String>)>,
    /// Active `/` slash-command completion popup in Insert mode.
    pub slash: Option<SlashPopup>,
    /// Large pastes shown in `input` as a placeholder: (placeholder, text).
    /// Expanded on submit.
    pastes: Vec<(String, String)>,
    /// The leader key was pressed; the next key is an app-level command.
    pub leader: bool,
    /// Agents offered by the new-session picker.
    pub picker: Vec<&'static AgentSpec>,
    /// Cached local install/login status for each entry in `picker`.
    pub picker_status: Vec<AgentStatus>,
    pub picker_cursor: usize,
    /// Whether the picked session gets its own worktree (`n`) or runs
    /// directly in the current directory/branch (`N`).
    pub picker_isolated: bool,
    pub model_cursor: usize,
    pub effort_cursor: usize,
    pub switcher_cursor: usize,
    /// Buffer edited while in `Mode::Rename`.
    pub rename_buf: String,
    /// Index in `sessions` being renamed in `Mode::Rename`.
    pub rename_target: usize,
    /// Mode to return to once `Mode::Rename` closes.
    rename_return: Mode,
    /// Lines scrolled up from the bottom of the transcript (0 = follow).
    pub scroll: u16,
    /// Largest useful `scroll`, reported by the last render so scrolling
    /// stops at the top instead of piling up invisible overscroll.
    pub max_scroll: Cell<u16>,
    /// Last rendered transcript viewport height, used for half-page scrolling
    /// and centering search matches.
    pub transcript_height: Cell<u16>,
    /// Active transcript search query (case-insensitive substring).
    pub search_query: String,
    /// Line indices in `transcript_text` matching `search_query`.
    pub search_matches: Vec<usize>,
    /// Index into `search_matches` of the currently focused match.
    pub search_current: Option<usize>,
    /// Mouse selection over the transcript, in content coordinates.
    pub selection: Option<Selection>,
    /// Plain text of the transcript lines from the last render, for copying.
    pub transcript_text: RefCell<Vec<String>>,
    /// Clickable markdown links currently visible on screen (for OSC 8).
    pub visible_links: RefCell<Vec<ui::ScreenLink>>,
    /// Rendered transcript line index -> trace block start entry index, for
    /// clicking `▸`/`▾` summary rows to expand or collapse individual blocks.
    pub trace_toggles: RefCell<Vec<(usize, usize)>>,
    /// Cursor position requested during the last render, if visible.
    pub cursor_pos: Cell<Option<(u16, u16)>>,
    /// Text to put on the clipboard; the event loop owns the terminal I/O.
    pub pending_copy: Option<String>,
    /// Set when a background session needs attention; the event loop rings BEL.
    pub pending_bell: bool,
    pub should_quit: bool,
    /// Whether the terminal reports mouse events (`m` toggles; off lets the
    /// terminal select text natively).
    pub mouse: bool,
    /// Whether chain-of-thought and tool-call details are expanded globally
    /// (`Ctrl-o` or `leader + t`). Defaults to `false` (compact summary lines).
    pub show_trace: bool,
    /// Per-block overrides `(session_id, block_start_entry_idx)` inverting
    /// `show_trace`, toggled by clicking a block's `▸`/`▾` line.
    pub trace_overrides: HashSet<(usize, usize)>,
    /// Action awaiting y/n; owns the keyboard while set.
    pub confirm: Option<Confirm>,
    /// In-progress form state for the active session's pending question modal.
    pub question_draft: Option<QuestionDraft>,
    /// One-line notice shown in the status bar until its TTL expires or a newer
    /// notice replaces it.
    notice: Option<Notice>,
    cwd: PathBuf,
    worktrees_root: PathBuf,
    store: Store,
    ui_tx: Sender<Msg>,
    next_id: usize,
}

impl App {
    pub fn new(cwd: PathBuf, worktrees_root: PathBuf, store: Store, ui_tx: Sender<Msg>) -> Self {
        let history = store.load_history();
        Self {
            sessions: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            input: Editor::default(),
            history,
            history_nav: None,
            mention: None,
            mention_files: None,
            slash: None,
            pastes: Vec::new(),
            leader: false,
            picker: Vec::new(),
            picker_status: Vec::new(),
            picker_cursor: 0,
            picker_isolated: true,
            model_cursor: 0,
            effort_cursor: 0,
            switcher_cursor: 0,
            rename_buf: String::new(),
            rename_target: 0,
            rename_return: Mode::Normal,
            scroll: 0,
            max_scroll: Cell::new(u16::MAX),
            transcript_height: Cell::new(20),
            search_query: String::new(),
            search_matches: Vec::new(),
            search_current: None,
            selection: None,
            transcript_text: RefCell::new(Vec::new()),
            visible_links: RefCell::new(Vec::new()),
            trace_toggles: RefCell::new(Vec::new()),
            cursor_pos: Cell::new(None),
            pending_copy: None,
            pending_bell: false,
            should_quit: false,
            mouse: true,
            show_trace: false,
            trace_overrides: HashSet::new(),
            confirm: None,
            question_draft: None,
            notice: None,
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

    /// Whether the trace block starting at `block_start` in `session_id` is
    /// currently expanded.
    pub fn is_trace_expanded(&self, session_id: usize, block_start: usize) -> bool {
        self.show_trace ^ self.trace_overrides.contains(&(session_id, block_start))
    }

    /// Flip global expansion of chain-of-thought and tool-call details, clearing
    /// any per-block overrides so all blocks match the new global state.
    pub fn toggle_trace(&mut self) {
        self.show_trace = !self.show_trace;
        self.trace_overrides.clear();
        self.set_message(if self.show_trace {
            "showing full thoughts & tool details"
        } else {
            "compact view: thoughts & tools collapsed"
        });
    }

    /// Toggle expansion of one specific trace block in the current session.
    fn toggle_trace_block(&mut self, block_start: usize) {
        let Some(sid) = self.current().map(|s| s.id) else {
            return;
        };
        let key = (sid, block_start);
        if !self.trace_overrides.remove(&key) {
            self.trace_overrides.insert(key);
        }
    }

    /// Show a transient notice in the status bar for `NOTICE_TTL`.
    pub fn set_message(&mut self, msg: impl Into<String>) {
        self.notice = Some(Notice {
            text: msg.into(),
            expires_at: Instant::now() + NOTICE_TTL,
        });
    }

    /// Active status-bar notice, if one is set and has not expired yet.
    pub fn message(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|n| Instant::now() < n.expires_at)
            .map(|n| n.text.as_str())
    }

    /// Periodic housekeeping called from the event loop (expires old notices).
    pub fn tick(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|n| Instant::now() >= n.expires_at)
        {
            self.notice = None;
        }
    }

    pub fn current(&self) -> Option<&Session> {
        self.sessions.get(self.selected)
    }

    fn current_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.selected)
    }

    pub fn handle_msg(&mut self, msg: Msg) {
        if let UiMsg::Closed(text) = msg.body {
            self.set_message(text);
            return;
        }
        let is_ready = matches!(msg.body, UiMsg::Ready(..));
        let is_commands = matches!(msg.body, UiMsg::Event(Event::AvailableCommands(_)));
        let is_turn_done = matches!(msg.body, UiMsg::TurnDone(_));
        let is_permission = matches!(msg.body, UiMsg::Permission(_));
        let is_question = matches!(msg.body, UiMsg::Question(_));
        let settled = is_turn_done || matches!(msg.body, UiMsg::Error(_));
        let Some(idx) = self.sessions.iter().position(|s| s.id == msg.session) else {
            return;
        };
        let background = idx != self.selected;
        let s = &mut self.sessions[idx];
        s.apply(msg.body);
        if is_turn_done && s.worktree.is_some() {
            s.refresh_worktree_state();
        }
        if background && (is_turn_done || is_permission || is_question) {
            if is_turn_done {
                s.attention = true;
            }
            self.pending_bell = true;
        }
        if !background && (is_ready || is_commands) && self.slash.is_some() {
            self.update_completions();
        }
        if !background && is_question {
            self.sync_question_draft();
        } else if !background && settled {
            self.question_draft = None;
        }
        if settled {
            self.persist(msg.session);
        }
    }

    /// Write one session's record. A failure is shown, never fatal.
    fn persist(&mut self, id: usize) {
        let Some(s) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        if s.entries.is_empty() && s.label.is_none() {
            return;
        }
        if let Err(e) = self.store.save(&s.key, &s.record(&self.cwd)) {
            self.set_message(format!("could not save session: {e}"));
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
            let mut s = Session::spawn(
                id,
                spec,
                self.cwd.clone(),
                worktree,
                rec.model,
                rec.effort,
                self.ui_tx.clone(),
            );
            s.key = key;
            s.label = rec.label;
            s.entries = rec.entries;
            s.entries.push(Entry::Notice(note));
            self.sessions.push(s);
        }
        // Records load oldest first; land on the newest, like `open_session`.
        self.selected = self.sessions.len().saturating_sub(1);
        // If no persisted prompt history file exists yet, seed from restored transcripts.
        if self.history.is_empty() {
            for s in &self.sessions {
                for entry in &s.entries {
                    if let Entry::User(text) = entry {
                        let text = text.trim();
                        if !text.is_empty() && self.history.last().map(String::as_str) != Some(text)
                        {
                            self.history.push(text.to_string());
                        }
                    }
                }
            }
            if self.history.len() > crate::store::HISTORY_LIMIT {
                let excess = self.history.len() - crate::store::HISTORY_LIMIT;
                self.history.drain(..excess);
            }
        }
    }

    pub fn open_session(&mut self, spec: &'static AgentSpec, isolated: bool) {
        let id = self.next_id;
        self.next_id += 1;
        let worktree = if isolated {
            match Worktree::create(&self.cwd, &self.worktrees_root, spec.id) {
                Ok(w) => Some(w),
                Err(e) => {
                    self.set_message(format!(
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
            None,
            None,
            self.ui_tx.clone(),
        ));
        self.selected = self.sessions.len() - 1;
        self.scroll = 0;
        self.clear_search();
        self.clear_completions();
        self.mode = Mode::Insert;
    }

    /// Agent to open on startup: the one used in the previous conversation
    /// (if still known), falling back to `agent::default_agent`.
    pub fn default_agent(&self) -> Option<&'static AgentSpec> {
        self.store
            .last_agent()
            .and_then(|id| agent::find(&id))
            .or_else(agent::default_agent)
    }

    /// Open a default session in the current directory (no worktree).
    pub fn open_default_session(&mut self) {
        if let Some(spec) = self.default_agent() {
            self.open_session(spec, false);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.selection = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            match self.current_mut() {
                Some(s)
                    if matches!(
                        s.status,
                        Status::Running | Status::AwaitingPermission | Status::AwaitingQuestion
                    ) =>
                {
                    s.cancel();
                    self.question_draft = None;
                }
                _ => self.request_quit(),
            }
            return;
        }
        let modal_open = self.confirm.is_some()
            || self
                .current()
                .is_some_and(|s| s.pending.is_some() || s.pending_question.is_some());
        if ctrl && key.code == KeyCode::Char('d') {
            let scroll_ctx = !modal_open
                && !self.sessions.is_empty()
                && (self.leader || self.mode == Mode::Normal);
            if !scroll_ctx {
                self.should_quit = true;
                return;
            }
        }
        // A pending permission or question request owns the keyboard.
        if self.current().is_some_and(|s| s.pending.is_some()) {
            self.handle_permission_key(key);
            return;
        }
        if self.current().is_some_and(|s| s.pending_question.is_some()) {
            self.handle_question_key(key);
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
        if self.leader {
            self.leader = false;
            self.handle_command(key);
            if self.mode != Mode::Insert {
                self.clear_completions();
            }
            return;
        }
        if matches!(self.mode, Mode::Normal | Mode::Insert)
            && !self.sessions.is_empty()
            && (ctrl && key.code == KeyCode::Char('a')
                || self.mode == Mode::Normal && key.code == KeyCode::Char(' '))
        {
            self.leader = true;
            return;
        }
        if matches!(self.mode, Mode::Normal | Mode::Insert)
            && !self.sessions.is_empty()
            && ctrl
            && key.code == KeyCode::Char('o')
        {
            self.toggle_trace();
            return;
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Insert => self.handle_insert(key),
            Mode::Picker => self.handle_picker(key),
            Mode::Model => self.handle_model(key),
            Mode::Effort => self.handle_effort(key),
            Mode::Switcher => self.handle_switcher(key),
            Mode::Help => self.handle_help(key),
            Mode::Rename => self.handle_rename(key),
            Mode::Search => self.handle_search(key),
        }
    }

    /// Insert bracketed-paste `text` into the prompt. Pastes longer than
    /// `PASTE_COLLAPSE_LINES` lines or `PASTE_COLLAPSE_CHARS` chars show as
    /// `[Pasted text #1 +12 lines]` and are expanded when the prompt is sent.
    pub fn handle_paste(&mut self, text: &str) {
        if self.current().is_some_and(|s| s.pending_question.is_some()) {
            self.sync_question_draft();
            if let Some(draft) = self.question_draft.as_mut() {
                if let Some(FieldDraft::Text(buf)) = draft.values.get_mut(draft.field_idx) {
                    let clean: String = text
                        .replace("\r\n", " ")
                        .replace(['\r', '\n', '\t'], " ")
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect();
                    buf.push_str(&clean);
                    draft.error = None;
                }
            }
            return;
        }
        if self.mode != Mode::Insert
            || self.sessions.is_empty()
            || self.confirm.is_some()
            || self.current().is_some_and(|s| s.pending.is_some())
        {
            return;
        }
        self.selection = None;
        // Terminals send \r or \r\n for line breaks; tabs would break the
        // cursor column maths, and other control chars can corrupt the screen.
        let text: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
            .chars()
            .filter(|&c| c == '\n' || !c.is_control())
            .collect();
        let lines = text.matches('\n').count() + 1;
        if lines <= PASTE_COLLAPSE_LINES && text.chars().count() <= PASTE_COLLAPSE_CHARS {
            self.input.insert_str(&text);
            self.update_completions();
            return;
        }
        let n = self.pastes.len() + 1;
        let placeholder = format!("[Pasted text #{n} +{lines} lines]");
        self.input.insert_str(&placeholder);
        self.pastes.push((placeholder, text));
        self.update_completions();
    }

    /// `text` with paste placeholders replaced by what was pasted. A
    /// placeholder the user edited or deleted is left alone.
    fn expand_pastes(&self, text: &str) -> String {
        self.pastes
            .iter()
            .fold(text.to_string(), |t, (ph, full)| t.replacen(ph, full, 1))
    }

    /// React to a mouse event on a terminal of `area`. Modals (picker,
    /// permission, confirm, help) own the screen, so the mouse is ignored then.
    pub fn handle_mouse(&mut self, ev: MouseEvent, area: Rect) {
        if matches!(
            self.mode,
            Mode::Picker | Mode::Model | Mode::Effort | Mode::Switcher | Mode::Help | Mode::Rename
        ) || self.confirm.is_some()
            || self
                .current()
                .is_some_and(|s| s.pending.is_some() || s.pending_question.is_some())
        {
            return;
        }
        let region = ui::locate(area, self.input.line_count(), ev.column, ev.row);
        match ev.kind {
            MouseEventKind::ScrollUp if region != Region::Other => self.scroll_up(3),
            MouseEventKind::ScrollDown if region != Region::Other => {
                self.scroll = self.scroll.saturating_sub(3)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                match region {
                    Region::Transcript => {
                        self.selection = self.content_pos(area, ev).map(Selection::new)
                    }
                    Region::Input if !self.sessions.is_empty() => {
                        if self.mode != Mode::Insert {
                            self.input.checkpoint();
                        }
                        self.mode = Mode::Insert;
                    }
                    _ => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selection.is_some() => {
                // Dragging past an edge scrolls, so a selection can grow
                // beyond one screen.
                let view = ui::transcript_area(area, self.input.line_count());
                if ev.row < view.y {
                    self.scroll_up(1);
                } else if ev.row >= view.bottom() {
                    self.scroll = self.scroll.saturating_sub(1);
                }
                if let (Some(pos), Some(sel)) = (self.content_pos(area, ev), &mut self.selection) {
                    sel.head = pos;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = self.selection {
                    if sel.is_empty() {
                        self.selection = None;
                        let clicked_line = sel.anchor.0;
                        let block_start = self
                            .trace_toggles
                            .borrow()
                            .iter()
                            .find_map(|&(line, start)| (line == clicked_line).then_some(start));
                        if let Some(start) = block_start {
                            self.toggle_trace_block(start);
                        }
                    } else {
                        let text = sel.extract(&self.transcript_text.borrow());
                        if !text.is_empty() {
                            self.pending_copy = Some(text);
                            self.set_message("copied selection");
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Transcript content position under the mouse, clamping to the
    /// transcript area so drags past its edges keep extending the selection.
    fn content_pos(&self, area: Rect, ev: MouseEvent) -> Option<(usize, u16)> {
        let view = ui::transcript_area(area, self.input.line_count());
        if view.height == 0 || view.width == 0 {
            return None;
        }
        let row = ev.row.clamp(view.y, view.bottom() - 1) - view.y;
        let col = ev.column.clamp(view.x, view.right() - 1) - view.x;
        let max = self.max_scroll.get();
        let top = max.saturating_sub(self.scroll.min(max));
        Some((top as usize + row as usize, col))
    }

    fn handle_permission_key(&mut self, key: KeyEvent) {
        let Some(s) = self.current_mut() else { return };
        let Some(req) = &s.pending else { return };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let choice = match key.code {
            KeyCode::Char('a') if !ctrl => {
                req.allow_index().map(|i| Some(req.options[i].id.clone()))
            }
            KeyCode::Char('r') if !ctrl => {
                Some(req.reject_index().map(|i| req.options[i].id.clone()))
            }
            KeyCode::Char(c) if !ctrl => c
                .to_digit(10)
                .and_then(|n| (n as usize).checked_sub(1))
                .and_then(|i| req.options.get(i))
                .map(|o| Some(o.id.clone())),
            KeyCode::Esc => Some(req.reject_index().map(|i| req.options[i].id.clone())),
            _ => None,
        };
        match choice {
            Some(Some(id)) => s.answer_permission(Some(id)),
            // No reject option offered: cancel the whole turn.
            Some(None) => s.cancel(),
            None => {}
        }
    }

    /// Ensure `question_draft` matches the currently selected session's pending question.
    pub fn sync_question_draft(&mut self) {
        let Some(s) = self.current() else {
            self.question_draft = None;
            return;
        };
        let Some(req) = &s.pending_question else {
            self.question_draft = None;
            return;
        };
        let matches = self
            .question_draft
            .as_ref()
            .is_some_and(|d| d.session_id == s.id && d.values.len() == req.fields.len());
        if !matches {
            self.question_draft = Some(QuestionDraft::from_request(s.id, req));
        }
    }

    fn handle_question_key(&mut self, key: KeyEvent) {
        self.sync_question_draft();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(req) = self.current().and_then(|s| s.pending_question.clone()) else {
            return;
        };
        if key.code == KeyCode::Esc {
            if let Some(s) = self.current_mut() {
                s.answer_question(ElicitationResponse::Cancel);
            }
            self.question_draft = None;
            return;
        }
        if ctrl && key.code == KeyCode::Char('s') {
            self.submit_question(&req);
            return;
        }
        if req.fields.is_empty() {
            match key.code {
                KeyCode::Enter => self.submit_question(&req),
                KeyCode::Char('d') if !ctrl => {
                    if let Some(s) = self.current_mut() {
                        s.answer_question(ElicitationResponse::Decline);
                    }
                    self.question_draft = None;
                }
                _ => {}
            }
            return;
        }

        let n_fields = req.fields.len();
        let Some(draft) = self.question_draft.as_mut() else {
            return;
        };
        let fi = draft.field_idx.min(n_fields - 1);
        draft.field_idx = fi;
        let kind = &req.fields[fi].kind;

        match key.code {
            KeyCode::Tab => {
                let next = (fi + 1) % n_fields;
                draft.field_idx = next;
                draft.option_idx = match draft.values.get(next) {
                    Some(FieldDraft::Single(Some(i))) => *i,
                    _ => 0,
                };
                draft.error = None;
                return;
            }
            KeyCode::BackTab => {
                let prev = (fi + n_fields - 1) % n_fields;
                draft.field_idx = prev;
                draft.option_idx = match draft.values.get(prev) {
                    Some(FieldDraft::Single(Some(i))) => *i,
                    _ => 0,
                };
                draft.error = None;
                return;
            }
            _ => {}
        }

        match kind {
            FieldKind::SingleSelect { options, .. } => match key.code {
                KeyCode::Down | KeyCode::Char('j') if !ctrl && !options.is_empty() => {
                    draft.option_idx = (draft.option_idx + 1) % options.len();
                }
                KeyCode::Up | KeyCode::Char('k') if !ctrl && !options.is_empty() => {
                    draft.option_idx = (draft.option_idx + options.len() - 1) % options.len();
                }
                KeyCode::Char(' ') if !ctrl && !options.is_empty() => {
                    if let Some(FieldDraft::Single(sel)) = draft.values.get_mut(fi) {
                        *sel = if *sel == Some(draft.option_idx) {
                            None
                        } else {
                            Some(draft.option_idx)
                        };
                        draft.error = None;
                    }
                }
                KeyCode::Char('d') if !ctrl => {
                    if let Some(s) = self.current_mut() {
                        s.answer_question(ElicitationResponse::Decline);
                    }
                    self.question_draft = None;
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(idx) = c
                        .to_digit(10)
                        .and_then(|n| (n as usize).checked_sub(1))
                        .filter(|&i| i < options.len())
                    {
                        draft.option_idx = idx;
                        if let Some(FieldDraft::Single(sel)) = draft.values.get_mut(fi) {
                            *sel = Some(idx);
                            draft.error = None;
                        }
                    }
                }
                KeyCode::Enter => {
                    if !options.is_empty() {
                        if let Some(FieldDraft::Single(sel)) = draft.values.get_mut(fi) {
                            if sel.is_none() {
                                *sel = Some(draft.option_idx.min(options.len() - 1));
                            }
                        }
                    }
                    if let Some(next) = next_primary_field(&req, fi) {
                        draft.field_idx = next;
                        draft.option_idx = match draft.values.get(next) {
                            Some(FieldDraft::Single(Some(i))) => *i,
                            _ => 0,
                        };
                        draft.error = None;
                    } else {
                        self.submit_question(&req);
                    }
                }
                _ => {}
            },
            FieldKind::MultiSelect { options, .. } => match key.code {
                KeyCode::Down | KeyCode::Char('j') if !ctrl && !options.is_empty() => {
                    draft.option_idx = (draft.option_idx + 1) % options.len();
                }
                KeyCode::Up | KeyCode::Char('k') if !ctrl && !options.is_empty() => {
                    draft.option_idx = (draft.option_idx + options.len() - 1) % options.len();
                }
                KeyCode::Char(' ') if !ctrl && !options.is_empty() => {
                    if let Some(FieldDraft::Multi(checks)) = draft.values.get_mut(fi) {
                        if let Some(slot) = checks.get_mut(draft.option_idx) {
                            *slot = !*slot;
                            draft.error = None;
                        }
                    }
                }
                KeyCode::Char('d') if !ctrl => {
                    if let Some(s) = self.current_mut() {
                        s.answer_question(ElicitationResponse::Decline);
                    }
                    self.question_draft = None;
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(idx) = c
                        .to_digit(10)
                        .and_then(|n| (n as usize).checked_sub(1))
                        .filter(|&i| i < options.len())
                    {
                        draft.option_idx = idx;
                        if let Some(FieldDraft::Multi(checks)) = draft.values.get_mut(fi) {
                            if let Some(slot) = checks.get_mut(idx) {
                                *slot = !*slot;
                                draft.error = None;
                            }
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(next) = next_primary_field(&req, fi) {
                        draft.field_idx = next;
                        draft.option_idx = match draft.values.get(next) {
                            Some(FieldDraft::Single(Some(i))) => *i,
                            _ => 0,
                        };
                        draft.error = None;
                    } else {
                        self.submit_question(&req);
                    }
                }
                _ => {}
            },
            FieldKind::Bool { .. } => match key.code {
                KeyCode::Char(' ') if !ctrl => {
                    if let Some(FieldDraft::Bool(b)) = draft.values.get_mut(fi) {
                        *b = !*b;
                        draft.error = None;
                    }
                }
                KeyCode::Char('y') if !ctrl => {
                    if let Some(FieldDraft::Bool(b)) = draft.values.get_mut(fi) {
                        *b = true;
                        draft.error = None;
                    }
                }
                KeyCode::Char('n') if !ctrl => {
                    if let Some(FieldDraft::Bool(b)) = draft.values.get_mut(fi) {
                        *b = false;
                        draft.error = None;
                    }
                }
                KeyCode::Char('d') if !ctrl => {
                    if let Some(s) = self.current_mut() {
                        s.answer_question(ElicitationResponse::Decline);
                    }
                    self.question_draft = None;
                }
                KeyCode::Enter => {
                    if let Some(next) = next_primary_field(&req, fi) {
                        draft.field_idx = next;
                        draft.option_idx = 0;
                        draft.error = None;
                    } else {
                        self.submit_question(&req);
                    }
                }
                _ => {}
            },
            FieldKind::Text { .. } | FieldKind::Number { .. } => match key.code {
                KeyCode::Backspace => {
                    if let Some(FieldDraft::Text(buf)) = draft.values.get_mut(fi) {
                        buf.pop();
                        draft.error = None;
                    }
                }
                KeyCode::Char('u') if ctrl => {
                    if let Some(FieldDraft::Text(buf)) = draft.values.get_mut(fi) {
                        buf.clear();
                        draft.error = None;
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(FieldDraft::Text(buf)) = draft.values.get_mut(fi) {
                        buf.push(c);
                        draft.error = None;
                    }
                }
                KeyCode::Enter => {
                    if let Some(next) = next_primary_field(&req, fi) {
                        draft.field_idx = next;
                        draft.option_idx = match draft.values.get(next) {
                            Some(FieldDraft::Single(Some(i))) => *i,
                            _ => 0,
                        };
                        draft.error = None;
                    } else {
                        self.submit_question(&req);
                    }
                }
                _ => {}
            },
        }
    }

    fn submit_question(&mut self, req: &ElicitationRequest) {
        let Some(draft) = self.question_draft.as_mut() else {
            return;
        };
        let mut content = serde_json::Map::new();
        for (i, field) in req.fields.iter().enumerate() {
            let label = field.title.as_deref().unwrap_or(&field.key);
            match (&field.kind, draft.values.get(i)) {
                (FieldKind::SingleSelect { options, .. }, Some(FieldDraft::Single(sel))) => {
                    if let Some(opt) = sel.and_then(|idx| options.get(idx)) {
                        content.insert(
                            field.key.clone(),
                            serde_json::Value::String(opt.value.clone()),
                        );
                    } else if field.required {
                        draft.field_idx = i;
                        draft.error = Some(format!("{label} is required"));
                        return;
                    }
                }
                (FieldKind::MultiSelect { options, .. }, Some(FieldDraft::Multi(checks))) => {
                    let chosen: Vec<serde_json::Value> = options
                        .iter()
                        .zip(checks.iter())
                        .filter(|(_, &on)| on)
                        .map(|(o, _)| serde_json::Value::String(o.value.clone()))
                        .collect();
                    if !chosen.is_empty() {
                        content.insert(field.key.clone(), serde_json::Value::Array(chosen));
                    } else if field.required {
                        draft.field_idx = i;
                        draft.error = Some(format!("{label} requires at least one choice"));
                        return;
                    }
                }
                (FieldKind::Bool { .. }, Some(FieldDraft::Bool(val))) => {
                    content.insert(field.key.clone(), serde_json::Value::Bool(*val));
                }
                (FieldKind::Text { .. }, Some(FieldDraft::Text(buf))) => {
                    let trimmed = buf.trim();
                    if !trimmed.is_empty() {
                        content.insert(
                            field.key.clone(),
                            serde_json::Value::String(trimmed.to_string()),
                        );
                    } else if field.required {
                        draft.field_idx = i;
                        draft.error = Some(format!("{label} is required"));
                        return;
                    }
                }
                (FieldKind::Number { integer, .. }, Some(FieldDraft::Text(buf))) => {
                    let trimmed = buf.trim();
                    if trimmed.is_empty() {
                        if field.required {
                            draft.field_idx = i;
                            draft.error = Some(format!("{label} is required"));
                            return;
                        }
                    } else if *integer {
                        match trimmed.parse::<i64>() {
                            Ok(n) => {
                                content.insert(field.key.clone(), serde_json::json!(n));
                            }
                            Err(_) => {
                                draft.field_idx = i;
                                draft.error = Some(format!("{label} must be an integer"));
                                return;
                            }
                        }
                    } else {
                        match trimmed.parse::<f64>() {
                            Ok(n) if n.is_finite() => {
                                content.insert(field.key.clone(), serde_json::json!(n));
                            }
                            _ => {
                                draft.field_idx = i;
                                draft.error = Some(format!("{label} must be a number"));
                                return;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(s) = self.current_mut() {
            s.answer_question(ElicitationResponse::Accept(content));
        }
        self.question_draft = None;
    }
}

/// Next field after `current` that should be visited on `Enter`. Skips optional
/// `*_custom` companion boxes (like `claude-agent-acp`'s per-question "Other"
/// field) so single-question forms submit on one `Enter`; users can still reach
/// any companion field with `Tab`.
fn next_primary_field(req: &ElicitationRequest, current: usize) -> Option<usize> {
    ((current + 1)..req.fields.len()).find(|&i| {
        let f = &req.fields[i];
        !f.key.ends_with("_custom") || f.required
    })
}

impl App {
    /// Normal mode edits the prompt vim-style; app-level commands sit behind
    /// the leader key. With no session there is no prompt, so they are direct.
    fn handle_normal(&mut self, key: KeyEvent) {
        if self.sessions.is_empty() {
            self.handle_command(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            match key.code {
                KeyCode::Char('u') => {
                    self.scroll_up(self.half_page());
                    return;
                }
                KeyCode::Char('d') => {
                    self.scroll = self.scroll.saturating_sub(self.half_page());
                    return;
                }
                KeyCode::Char('r') => {
                    self.input.redo();
                    return;
                }
                _ => {}
            }
        }
        if !ctrl && self.input.pending().is_empty() {
            match key.code {
                KeyCode::Char('?') => {
                    self.mode = Mode::Help;
                    return;
                }
                KeyCode::Char('/') => {
                    self.open_search();
                    return;
                }
                KeyCode::Char('n') if !self.search_query.is_empty() => {
                    self.search_step(true);
                    return;
                }
                KeyCode::Char('N') if !self.search_query.is_empty() => {
                    self.search_step(false);
                    return;
                }
                KeyCode::Up | KeyCode::Char('k') if self.input.cursor_pos().0 == 0 => {
                    self.history_prev();
                    return;
                }
                KeyCode::Down | KeyCode::Char('j')
                    if self.input.cursor_pos().0 + 1 >= self.input.line_count() =>
                {
                    self.history_next();
                    return;
                }
                _ => {}
            }
        }
        let motion = match key.code {
            KeyCode::Char(c) if !ctrl => Some(c),
            KeyCode::Left => Some('h'),
            KeyCode::Right => Some('l'),
            KeyCode::Up => Some('k'),
            KeyCode::Down => Some('j'),
            KeyCode::Home => Some('0'),
            KeyCode::End => Some('$'),
            _ => None,
        };
        if let Some(c) = motion {
            if self.input.normal(c) {
                self.mode = Mode::Insert;
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                if !self.input.pending().is_empty() {
                    self.input.cancel_pending();
                } else if !self.search_query.is_empty() {
                    self.clear_search();
                }
            }
            KeyCode::Enter => {
                self.input.checkpoint();
                self.mode = Mode::Insert;
            }
            KeyCode::Tab => self.open_switcher(),
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            _ => {}
        }
    }

    /// App-level commands: the key after the leader, or any key in normal mode
    /// when there is no session.
    fn handle_command(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => self.scroll_up(self.half_page()),
            KeyCode::Char('d') if ctrl => {
                self.scroll = self.scroll.saturating_sub(self.half_page());
            }
            KeyCode::Char('/') if !self.sessions.is_empty() => self.open_search(),
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('n') => self.open_picker(true),
            KeyCode::Char('N') => self.open_picker(false),
            KeyCode::Char('d') => self.request_close(),
            KeyCode::Char('r') => self.open_rename(self.selected, Mode::Normal),
            KeyCode::Char('M') => self.open_models(),
            KeyCode::Char('e') => self.open_effort(),
            KeyCode::Char('s') | KeyCode::Tab => self.open_switcher(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('t') => self.toggle_trace(),
            KeyCode::Char('m') => {
                self.mouse = !self.mouse;
                self.set_message(if self.mouse {
                    "mouse on"
                } else {
                    "mouse off: terminal text selection works"
                });
            }
            KeyCode::Char('j') => self.select(1),
            KeyCode::Char('k') => self.select(-1),
            // Terminals without mouse capture turn the wheel into arrow
            // keys on the alternate screen, so arrows scroll the transcript.
            KeyCode::Up => self.scroll_up(3),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(3),
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = self.max_scroll.get(),
            KeyCode::Char('G') | KeyCode::End => self.scroll = 0,
            KeyCode::Char('i') | KeyCode::Enter => {
                if self.sessions.is_empty() {
                    self.open_picker(false);
                } else {
                    self.input.checkpoint();
                    self.mode = Mode::Insert;
                }
            }
            _ => {}
        }
    }

    fn half_page(&self) -> u16 {
        (self.transcript_height.get() / 2).max(1)
    }

    fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n).min(self.max_scroll.get());
    }

    fn open_search(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.clear_search();
        self.mode = Mode::Search;
    }

    fn clear_search(&mut self) {
        self.search_query.clear();
        self.search_matches.clear();
        self.search_current = None;
    }

    fn handle_search(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.clear_search();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.update_search();
            }
            KeyCode::Char('u') if ctrl => {
                self.search_query.clear();
                self.update_search();
            }
            KeyCode::Char(c) if !ctrl => {
                self.search_query.push(c);
                self.update_search();
            }
            _ => {}
        }
    }

    fn update_search(&mut self) {
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.search_current = None;
            return;
        }
        let needle = self.search_query.to_lowercase();
        self.search_matches = self
            .transcript_text
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.search_matches.is_empty() {
            self.search_current = None;
        } else {
            self.search_current = Some(0);
            self.scroll_to_current_match();
        }
    }

    fn search_step(&mut self, forward: bool) {
        if self.search_query.is_empty() {
            return;
        }
        let needle = self.search_query.to_lowercase();
        let fresh: Vec<usize> = self
            .transcript_text
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if fresh.is_empty() {
            self.search_matches.clear();
            self.search_current = None;
            return;
        }
        let next = if fresh == self.search_matches {
            let n = fresh.len();
            match self.search_current {
                Some(i) if forward => (i + 1) % n,
                Some(i) => (i + n - 1) % n,
                None if forward => 0,
                None => n - 1,
            }
        } else {
            let prev_line = self
                .search_current
                .and_then(|i| self.search_matches.get(i).copied());
            self.search_matches = fresh;
            match prev_line {
                Some(line) if forward => self
                    .search_matches
                    .iter()
                    .position(|&l| l > line)
                    .unwrap_or(0),
                Some(line) => self
                    .search_matches
                    .iter()
                    .rposition(|&l| l < line)
                    .unwrap_or(self.search_matches.len() - 1),
                None if forward => 0,
                None => self.search_matches.len() - 1,
            }
        };
        self.search_current = Some(next);
        self.scroll_to_current_match();
    }

    fn scroll_to_current_match(&mut self) {
        let Some(line) = self
            .search_current
            .and_then(|i| self.search_matches.get(i).copied())
        else {
            return;
        };
        let total = self.transcript_text.borrow().len();
        let height = self.transcript_height.get().max(1) as usize;
        let max_scroll = total.saturating_sub(height);
        let target_top = line.saturating_sub(height / 2).min(max_scroll);
        let target_scroll = max_scroll.saturating_sub(target_top);
        self.scroll = u16::try_from(target_scroll).unwrap_or(u16::MAX);
    }

    /// Quit now, or ask first when an agent is mid-turn.
    fn request_quit(&mut self) {
        let busy = self.sessions.iter().any(|s| {
            matches!(
                s.status,
                Status::Running | Status::AwaitingPermission | Status::AwaitingQuestion
            )
        });
        if busy {
            self.confirm = Some(Confirm::Quit);
        } else {
            self.should_quit = true;
        }
    }

    /// Close the selected session, asking first if it is running or has history.
    fn request_close(&mut self) {
        let Some(s) = self.current() else { return };
        let busy = matches!(
            s.status,
            Status::Running | Status::AwaitingPermission | Status::AwaitingQuestion
        );
        if busy || !s.entries.is_empty() {
            self.confirm = Some(Confirm::CloseSession);
        } else {
            self.close_selected();
        }
    }

    fn clear_mention(&mut self) {
        self.mention = None;
        self.mention_files = None;
    }

    fn clear_slash(&mut self) {
        self.slash = None;
    }

    fn clear_completions(&mut self) {
        self.clear_mention();
        self.clear_slash();
    }

    fn update_completions(&mut self) {
        if self.mode != Mode::Insert || self.current().is_none() {
            self.clear_completions();
            return;
        }
        if let Some((slash_start, query)) =
            active_slash_command(self.input.chars(), self.input.cursor_index())
        {
            self.clear_mention();
            self.update_slash(slash_start, query);
            return;
        }
        self.clear_slash();
        self.update_mention();
    }

    fn update_slash(&mut self, slash_start: usize, query: String) {
        let Some(s) = self.current() else {
            self.clear_slash();
            return;
        };
        let commands = session_slash_commands(s);
        let prev_selected = self
            .slash
            .as_ref()
            .and_then(|p| p.matches.get(p.cursor))
            .map(|m| m.command.name.clone());
        let matches = filter_slash_commands(&commands, &query);
        let cursor = prev_selected
            .and_then(|name| {
                matches
                    .iter()
                    .position(|m| m.command.name.eq_ignore_ascii_case(&name))
            })
            .unwrap_or(0);
        self.slash = Some(SlashPopup {
            slash_start,
            query,
            matches,
            cursor,
            total_commands: commands.len(),
        });
    }

    fn accept_slash_command(&mut self, trailing_space: bool) -> bool {
        let Some(popup) = self.slash.as_ref() else {
            return false;
        };
        let Some(chosen) = popup.matches.get(popup.cursor) else {
            return false;
        };
        let slash_start = popup.slash_start;
        let end = self.input.cursor_index();
        let next_is_space = self
            .input
            .chars()
            .get(end)
            .is_some_and(|c| c.is_whitespace());
        let replacement = if trailing_space && !next_is_space {
            format!("/{} ", chosen.command.name)
        } else {
            format!("/{}", chosen.command.name)
        };
        self.input.replace_range(slash_start, end, &replacement);
        self.clear_slash();
        true
    }

    fn update_mention(&mut self) {
        if self.mode != Mode::Insert {
            self.clear_mention();
            return;
        }
        let Some(dir) = self.current().map(|s| s.dir(&self.cwd).to_path_buf()) else {
            self.clear_mention();
            return;
        };
        let Some((at_start, query)) =
            files::active_mention(self.input.chars(), self.input.cursor_index())
        else {
            self.clear_mention();
            return;
        };
        let needs_load = self
            .mention_files
            .as_ref()
            .is_none_or(|(cached_dir, _)| cached_dir != &dir);
        if needs_load {
            let listed = files::list_files(&dir);
            self.mention_files = Some((dir, listed));
        }
        let Some((_, listed)) = &self.mention_files else {
            return;
        };
        let prev_selected = self
            .mention
            .as_ref()
            .and_then(|m| m.matches.get(m.cursor))
            .map(|m| m.path.clone());
        let matches = files::fuzzy_filter(listed, &query, 50);
        let cursor = prev_selected
            .and_then(|p| matches.iter().position(|m| m.path == p))
            .unwrap_or(0);
        self.mention = Some(MentionPopup {
            at_start,
            query,
            matches,
            cursor,
            total_files: listed.len(),
        });
    }

    fn accept_mention(&mut self) -> bool {
        let Some(popup) = self.mention.as_ref() else {
            return false;
        };
        let Some(chosen) = popup.matches.get(popup.cursor) else {
            return false;
        };
        let at_start = popup.at_start;
        let replacement = files::format_mention(&chosen.path);
        let end = self.input.cursor_index();
        self.input.replace_range(at_start, end, &replacement);
        self.clear_mention();
        true
    }

    fn handle_insert(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(popup) = self.slash.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.clear_slash();
                    return;
                }
                KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                    if !popup.matches.is_empty() {
                        popup.cursor = (popup.cursor + 1) % popup.matches.len();
                    }
                    return;
                }
                KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                    if !popup.matches.is_empty() {
                        popup.cursor =
                            (popup.cursor + popup.matches.len() - 1) % popup.matches.len();
                    }
                    return;
                }
                KeyCode::Tab => {
                    self.accept_slash_command(true);
                    return;
                }
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    let end = self.input.cursor_index();
                    let has_trailing = self.input.chars()[end..].iter().any(|c| !c.is_whitespace());
                    let chosen = popup.matches.get(popup.cursor).map(|m| {
                        (
                            popup.query.eq_ignore_ascii_case(&m.command.name),
                            m.command.local,
                            m.command.input_hint.is_some(),
                        )
                    });
                    match chosen {
                        None => self.clear_slash(),
                        Some(_) if has_trailing => {
                            self.accept_slash_command(false);
                            return;
                        }
                        Some((exact, local, has_hint)) => {
                            if exact || local || !has_hint {
                                self.accept_slash_command(false);
                                // Fall through to submit immediately.
                            } else {
                                self.accept_slash_command(true);
                                return;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(popup) = self.mention.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.clear_mention();
                    return;
                }
                KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                    if !popup.matches.is_empty() {
                        popup.cursor = (popup.cursor + 1) % popup.matches.len();
                    }
                    return;
                }
                KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                    if !popup.matches.is_empty() {
                        popup.cursor =
                            (popup.cursor + popup.matches.len() - 1) % popup.matches.len();
                    }
                    return;
                }
                KeyCode::Tab => {
                    self.accept_mention();
                    return;
                }
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    if self.accept_mention() {
                        return;
                    }
                    self.clear_mention();
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                self.clear_completions();
                self.input.leave_insert();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.input.insert('\n');
                self.update_completions();
            }
            KeyCode::Char('j') if ctrl => {
                self.input.insert('\n');
                self.update_completions();
            }
            KeyCode::Char('w') if ctrl => {
                self.input.delete_word_back();
                self.update_completions();
            }
            KeyCode::Char('u') if ctrl => {
                self.input.kill_line_back();
                self.update_completions();
            }
            KeyCode::Enter => {
                let text = self.input.text().trim().to_string();
                if text.is_empty() {
                    return;
                }
                if self.slash_model(&text) || self.slash_effort(&text) {
                    self.history_nav = None;
                    self.input.clear();
                    self.pastes.clear();
                    self.clear_completions();
                    return;
                }
                let text = self.expand_pastes(&text);
                let mut sent = None;
                if let Some(s) = self.current_mut() {
                    if s.can_prompt() {
                        s.send_prompt(text.clone());
                        sent = Some((s.id, s.spec.id));
                    }
                }
                if let Some((id, agent)) = sent {
                    self.record_history(text);
                    self.history_nav = None;
                    self.input.clear();
                    self.pastes.clear();
                    self.clear_completions();
                    self.scroll = 0;
                    self.persist(id);
                    let _ = self.store.save_last_agent(agent);
                }
            }
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Backspace => {
                self.input.backspace();
                self.update_completions();
            }
            KeyCode::Delete => {
                self.input.delete_forward();
                self.update_completions();
            }
            KeyCode::Left => {
                self.input.left();
                self.update_completions();
            }
            KeyCode::Right => {
                self.input.right();
                self.update_completions();
            }
            KeyCode::Up => {
                if self.input.cursor_pos().0 == 0 {
                    self.history_prev();
                } else {
                    self.input.up();
                    self.update_completions();
                }
            }
            KeyCode::Down => {
                if self.input.cursor_pos().0 + 1 >= self.input.line_count() {
                    self.history_next();
                } else {
                    self.input.down();
                    self.update_completions();
                }
            }
            KeyCode::Home => {
                self.input.home();
                self.update_completions();
            }
            KeyCode::End => {
                self.input.end();
                self.update_completions();
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(c);
                self.update_completions();
            }
            _ => {}
        }
    }

    fn record_history(&mut self, prompt: String) {
        if prompt.trim().is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(prompt.as_str()) {
            self.history.push(prompt);
            if self.history.len() > crate::store::HISTORY_LIMIT {
                let excess = self.history.len() - crate::store::HISTORY_LIMIT;
                self.history.drain(..excess);
            }
            let _ = self.store.save_history(&self.history);
        }
    }

    /// Return the active history index if `self.input` still matches the
    /// unmodified recalled entry; otherwise detach so user edits are preserved.
    fn active_history_index(&mut self) -> Option<usize> {
        let index = self.history_nav.as_ref()?.index;
        let matches = self
            .history
            .get(index)
            .is_some_and(|entry| self.input.text() == *entry);
        if matches {
            Some(index)
        } else {
            self.history_nav = None;
            None
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let normal = self.mode == Mode::Normal;
        let target = match self.active_history_index() {
            Some(0) => return,
            Some(i) => i - 1,
            None => {
                let draft = HistoryDraft {
                    text: self.input.text(),
                    cursor: self.input.cursor_index(),
                    pastes: std::mem::take(&mut self.pastes),
                };
                let last = self.history.len() - 1;
                self.history_nav = Some(HistoryNav { index: last, draft });
                last
            }
        };
        if let Some(nav) = self.history_nav.as_mut() {
            nav.index = target;
        }
        let entry = self.history[target].clone();
        self.pastes.clear();
        self.clear_completions();
        self.input.set_text_first_line_end(&entry, normal);
        if !normal {
            self.input.checkpoint();
        }
    }

    fn history_next(&mut self) {
        let Some(index) = self.active_history_index() else {
            return;
        };
        let normal = self.mode == Mode::Normal;
        if index + 1 < self.history.len() {
            let target = index + 1;
            if let Some(nav) = self.history_nav.as_mut() {
                nav.index = target;
            }
            let entry = self.history[target].clone();
            self.pastes.clear();
            self.clear_completions();
            self.input.set_text_at(&entry, usize::MAX, normal);
        } else if let Some(nav) = self.history_nav.take() {
            self.pastes = nav.draft.pastes;
            self.clear_completions();
            self.input
                .set_text_at(&nav.draft.text, nav.draft.cursor, normal);
        }
        if !normal {
            self.input.checkpoint();
        }
    }

    /// Handle `/model` and `/model <name>` locally; returns whether `text`
    /// was that command. It is a terminal-UI command in Claude Code, so
    /// forwarding it to the agent is not reliable.
    fn slash_model(&mut self, text: &str) -> bool {
        let Some(rest) = text.strip_prefix("/model") else {
            return false;
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return false; // e.g. `/models`
        }
        let want = rest.trim();
        if want.is_empty() {
            self.open_models();
            return true;
        }
        let Some(s) = self.current_mut() else {
            return true;
        };
        let found = s
            .models
            .available
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(want) || m.name.eq_ignore_ascii_case(want))
            .map(|m| m.id.clone());
        let msg = if s.models.available.is_empty() {
            format!("{} does not offer model selection", s.spec.name)
        } else if !s.can_prompt() {
            "change the model between turns".into()
        } else if let Some(id) = found {
            s.set_model(id);
            return true;
        } else {
            let names: Vec<_> = s.models.available.iter().map(|m| m.name.as_str()).collect();
            format!("unknown model `{want}`; try: {}", names.join(", "))
        };
        self.set_message(msg);
        true
    }

    fn open_models(&mut self) {
        let Some(s) = self.current() else { return };
        if s.models.available.is_empty() {
            let msg = if s.status == Status::Starting {
                "agent is still starting".into()
            } else {
                format!("{} does not offer model selection", s.spec.name)
            };
            self.set_message(msg);
        } else if !s.can_prompt() {
            self.set_message("change the model between turns");
        } else {
            self.model_cursor = s
                .model()
                .and_then(|cur| s.models.available.iter().position(|m| m.id == cur))
                .unwrap_or(0);
            self.mode = Mode::Model;
        }
    }

    fn handle_model(&mut self, key: KeyEvent) {
        let n = self.current().map_or(0, |s| s.models.available.len());
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if n > 0 => {
                self.model_cursor = (self.model_cursor + 1) % n
            }
            KeyCode::Char('k') | KeyCode::Up if n > 0 => {
                self.model_cursor = (self.model_cursor + n - 1) % n
            }
            KeyCode::Enter => self.choose_model(self.model_cursor),
            KeyCode::Char(c) => {
                if let Some(i) = c.to_digit(10).and_then(|d| (d as usize).checked_sub(1)) {
                    self.choose_model(i);
                }
            }
            _ => {}
        }
    }

    fn choose_model(&mut self, i: usize) {
        if let Some(s) = self.current_mut() {
            if let Some(id) = s.models.available.get(i).map(|m| m.id.clone()) {
                s.set_model(id);
                self.mode = Mode::Normal;
            }
        }
    }

    /// Handle `/effort` and `/effort <level>` locally; returns whether `text`
    /// was that command.
    fn slash_effort(&mut self, text: &str) -> bool {
        let Some(rest) = text.strip_prefix("/effort") else {
            return false;
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return false; // e.g. `/efforts`
        }
        let want = rest.trim();
        if want.is_empty() {
            self.open_effort();
            return true;
        }
        let Some(s) = self.current_mut() else {
            return true;
        };
        let found = s
            .effort
            .available
            .iter()
            .find(|e| e.id.eq_ignore_ascii_case(want) || e.name.eq_ignore_ascii_case(want))
            .map(|e| e.id.clone());
        let msg = if s.effort.available.is_empty() {
            if s.status == Status::Starting {
                "agent is still starting".into()
            } else {
                format!("{} does not offer effort selection", s.spec.name)
            }
        } else if !s.can_prompt() {
            "change the effort level between turns".into()
        } else if let Some(id) = found {
            s.set_effort(id);
            return true;
        } else {
            let names: Vec<_> = s.effort.available.iter().map(|e| e.name.as_str()).collect();
            format!("unknown effort `{want}`; try: {}", names.join(", "))
        };
        self.set_message(msg);
        true
    }

    fn open_effort(&mut self) {
        let Some(s) = self.current() else { return };
        if s.effort.available.is_empty() {
            let msg = if s.status == Status::Starting {
                "agent is still starting".into()
            } else {
                format!("{} does not offer effort selection", s.spec.name)
            };
            self.set_message(msg);
        } else if !s.can_prompt() {
            self.set_message("change the effort level between turns");
        } else {
            self.effort_cursor = s
                .effort()
                .and_then(|cur| s.effort.available.iter().position(|e| e.id == cur))
                .unwrap_or(0);
            self.mode = Mode::Effort;
        }
    }

    fn handle_effort(&mut self, key: KeyEvent) {
        let n = self.current().map_or(0, |s| s.effort.available.len());
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if n > 0 => {
                self.effort_cursor = (self.effort_cursor + 1) % n
            }
            KeyCode::Char('k') | KeyCode::Up if n > 0 => {
                self.effort_cursor = (self.effort_cursor + n - 1) % n
            }
            KeyCode::Enter => self.choose_effort(self.effort_cursor),
            KeyCode::Char(c) => {
                if let Some(i) = c.to_digit(10).and_then(|d| (d as usize).checked_sub(1)) {
                    self.choose_effort(i);
                }
            }
            _ => {}
        }
    }

    fn choose_effort(&mut self, i: usize) {
        if let Some(s) = self.current_mut() {
            if let Some(id) = s.effort.available.get(i).map(|e| e.id.clone()) {
                s.set_effort(id);
                self.mode = Mode::Normal;
            }
        }
    }

    fn open_switcher(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        for s in &mut self.sessions {
            if s.worktree.is_some() {
                s.refresh_worktree_state();
            }
        }
        self.switcher_cursor = self.selected;
        self.mode = Mode::Switcher;
    }

    fn handle_switcher(&mut self, key: KeyEvent) {
        let n = self.sessions.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') | KeyCode::Tab => {
                self.mode = Mode::Normal
            }
            KeyCode::Char('j') | KeyCode::Down if n > 0 => {
                self.switcher_cursor = (self.switcher_cursor + 1) % n
            }
            KeyCode::Char('k') | KeyCode::Up if n > 0 => {
                self.switcher_cursor = (self.switcher_cursor + n - 1) % n
            }
            KeyCode::Char('r') if n > 0 => self.open_rename(self.switcher_cursor, Mode::Switcher),
            KeyCode::Enter => self.switch_to(self.switcher_cursor),
            KeyCode::Char(c) => {
                if let Some(i) = c.to_digit(10).and_then(|d| (d as usize).checked_sub(1)) {
                    self.switch_to(i);
                }
            }
            _ => {}
        }
    }

    fn switch_to(&mut self, i: usize) {
        if i < self.sessions.len() {
            if i != self.selected {
                self.selected = i;
                self.scroll = 0;
                self.clear_search();
                self.clear_completions();
                self.sync_question_draft();
            }
            self.sessions[i].attention = false;
            if self.sessions[i].worktree.is_some() {
                self.sessions[i].refresh_worktree_state();
            }
            self.mode = Mode::Normal;
        }
    }

    fn handle_help(&mut self, key: KeyEvent) {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
        ) {
            self.mode = Mode::Normal;
        }
    }

    fn open_rename(&mut self, target: usize, return_mode: Mode) {
        let Some(s) = self.sessions.get(target) else {
            return;
        };
        self.rename_target = target;
        self.rename_buf = s.label.clone().unwrap_or_default();
        self.rename_return = return_mode;
        self.mode = Mode::Rename;
    }

    fn handle_rename(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.mode = self.rename_return,
            KeyCode::Enter => self.apply_rename(),
            KeyCode::Backspace => {
                self.rename_buf.pop();
            }
            KeyCode::Char('u') if ctrl => self.rename_buf.clear(),
            KeyCode::Char(c) if !ctrl && !c.is_control() => self.rename_buf.push(c),
            _ => {}
        }
    }

    fn apply_rename(&mut self) {
        let trimmed = self.rename_buf.trim();
        let label = (!trimmed.is_empty()).then(|| trimmed.to_string());
        let mut persist_id = None;
        let mut remove_key = None;
        if let Some(s) = self.sessions.get_mut(self.rename_target) {
            s.label = label;
            if !s.entries.is_empty() || s.label.is_some() {
                persist_id = Some(s.id);
            } else {
                remove_key = Some(s.key.clone());
            }
        }
        if let Some(id) = persist_id {
            self.persist(id);
        }
        if let Some(key) = remove_key {
            self.store.remove(&key);
        }
        self.mode = self.rename_return;
    }
    fn open_picker(&mut self, isolated: bool) {
        let agents = agent::KNOWN
            .iter()
            .filter(|a| {
                !matches!(a.install, agent::Install::Manual(_))
                    || agent::install::locate(a).is_some()
            })
            .collect();
        self.open_picker_with(agents, isolated);
    }

    fn open_picker_with(&mut self, agents: Vec<&'static AgentSpec>, isolated: bool) {
        self.picker_isolated = isolated;
        self.picker = agents;
        self.picker_cursor = 0;
        if self.picker.len() == 1 {
            self.picker_status.clear();
            self.open_session(self.picker[0], isolated);
            return;
        }
        self.picker_status = self
            .picker
            .iter()
            .map(|a| agent::install::status(a))
            .collect();
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
        self.trace_overrides.retain(|(sid, _)| *sid != s.id);
        self.trace_toggles.borrow_mut().clear();
        self.store.remove(&s.key);
        self.selected = self
            .selected
            .saturating_sub(1)
            .min(self.sessions.len().saturating_sub(1));
        if let Some(cur) = self.sessions.get_mut(self.selected) {
            cur.attention = false;
        }
        self.scroll = 0;
        self.clear_search();
        self.clear_completions();
        self.sync_question_draft();
        if self.sessions.is_empty() {
            self.mode = Mode::Normal;
        }
        self.set_message(format!("closing {}…", s.display_title()));
        s.close(self.ui_tx.clone());
    }

    fn select(&mut self, delta: isize) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let next = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        let changed = next != self.selected;
        self.selected = next;
        self.sessions[self.selected].attention = false;
        if self.sessions[self.selected].worktree.is_some() {
            self.sessions[self.selected].refresh_worktree_state();
        }
        if changed {
            self.clear_search();
            self.clear_completions();
            self.sync_question_draft();
        }
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
    fn shift_enter_inserts_newline() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.input.text(), "a\nb");
        assert_eq!(app.sessions[0].status, Status::Idle);
    }

    #[test]
    fn small_paste_is_inserted_verbatim() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_paste("a\r\nb\tc");
        assert_eq!(app.input.text(), "a\nb    c");
        assert_eq!(app.sessions[0].status, Status::Idle);
    }

    #[test]
    fn large_paste_collapses_and_expands_on_send() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('x')));
        let big = "1\n2\n3\n4\n5";
        app.handle_paste(big);
        assert_eq!(app.input.text(), "x[Pasted text #1 +5 lines]");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.input.is_empty());
        assert_eq!(app.sessions[0].status, Status::Running);
        let sent = format!("x{big}");
        assert!(app.sessions[0]
            .entries
            .iter()
            .any(|e| matches!(e, Entry::User(t) if *t == sent)));
    }

    #[test]
    fn paste_ignored_outside_insert_mode() {
        let (mut app, _) = app_with_session();
        app.handle_paste("hello");
        assert!(app.input.is_empty());
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
                ..Default::default()
            }),
        });
        // 'q' must not quit while a permission is pending.
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("no".into()));
    }

    #[test]
    fn permission_shortcuts_a_r_digits_and_cancel() {
        let (mut app, perm_rx) = app_with_session();
        let req_all = || PermissionRequest {
            tool_call_id: "c".into(),
            title: Some("Edit file".into()),
            options: vec![
                PermissionOption {
                    id: "always".into(),
                    name: "Always allow".into(),
                    kind: "allow_always".into(),
                },
                PermissionOption {
                    id: "once".into(),
                    name: "Allow once".into(),
                    kind: "allow_once".into(),
                },
                PermissionOption {
                    id: "rej_always".into(),
                    name: "Always reject".into(),
                    kind: "reject_always".into(),
                },
                PermissionOption {
                    id: "rej_once".into(),
                    name: "Reject once".into(),
                    kind: "reject_once".into(),
                },
            ],
            ..Default::default()
        };

        // `a` prefers allow_once over allow_always.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(req_all()),
        });
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("once".into()));

        // `r` prefers reject_once over reject_always.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(req_all()),
        });
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("rej_once".into()));

        // `1-9` still chooses by index.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(req_all()),
        });
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("always".into()));

        // `a` falls back to allow_always when allow_once is absent.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(PermissionRequest {
                tool_call_id: "c2".into(),
                options: vec![PermissionOption {
                    id: "always".into(),
                    name: "Always allow".into(),
                    kind: "allow_always".into(),
                }],
                ..Default::default()
            }),
        });
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("always".into()));

        // `r` cancels the turn (None) when no reject option is offered.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(PermissionRequest {
                tool_call_id: "c3".into(),
                options: vec![PermissionOption {
                    id: "once".into(),
                    name: "Allow once".into(),
                    kind: "allow_once".into(),
                }],
                ..Default::default()
            }),
        });
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(perm_rx.try_recv().unwrap(), None);
    }

    #[test]
    fn closed_notice_sets_status_message() {
        let (mut app, _) = app_with_session();
        app.handle_msg(Msg {
            session: 7,
            body: UiMsg::Closed("done".into()),
        });
        assert_eq!(app.message(), Some("done"));
    }

    #[test]
    fn q_quits_from_normal_mode() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn q_asks_before_quitting_a_running_turn() {
        let (mut app, _) = app_with_session();
        app.sessions[0].status = Status::Running;
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert_eq!(app.confirm, Some(Confirm::Quit));
        app.handle_key(key(KeyCode::Char('n')));
        assert!(!app.should_quit && app.confirm.is_none());
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('q')));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.should_quit);
    }

    #[test]
    fn d_confirms_when_session_has_history() {
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.sessions.len(), 0);

        let (mut app, _) = app_with_session();
        app.sessions[0]
            .entries
            .push(crate::session::Entry::Agent("x".into()));
        app.handle_key(key(KeyCode::Char(' ')));
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

        app.handle_key(key(KeyCode::Char(' ')));
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
    fn dragging_over_the_transcript_selects_and_queues_a_copy() {
        let (mut app, _) = app_with_session();
        *app.transcript_text.borrow_mut() = vec!["hello world".into(), "second".into()];
        app.max_scroll.set(0);
        let area = Rect::new(0, 0, 80, 30);
        let mouse = |kind, col, row| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // The transcript starts below the 2-row header.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 6, 2), area);
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2, 3), area);
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, 3), area);
        assert_eq!(app.pending_copy.as_deref(), Some("world\nsec"));
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.selection.is_none());
    }

    #[test]
    fn switcher_picks_a_session_and_mouse_scrolls() {
        let area = Rect::new(0, 0, 100, 30);
        let (mut app, _) = app_with_session();
        let (s, _) = Session::detached(1, agent::find("antigravity").unwrap());
        app.sessions.push(s);
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.mode, Mode::Switcher);
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!((app.mode, app.selected), (Mode::Normal, 1));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!((app.mode, app.selected), (Mode::Normal, 1));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.selected, 0);

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
    fn model_picker_selects_between_turns() {
        use crate::agent::acp::{ModelInfo, ModelState};
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('M')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.message().is_some());

        app.sessions[0].models = ModelState {
            current: Some("b".into()),
            available: vec![
                ModelInfo {
                    id: "a".into(),
                    name: "A".into(),
                },
                ModelInfo {
                    id: "b".into(),
                    name: "B".into(),
                },
            ],
            ..Default::default()
        };
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('M')));
        assert_eq!(app.mode, Mode::Model);
        assert_eq!(app.model_cursor, 1, "starts on the current model");
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn slash_model_is_handled_locally() {
        use crate::agent::acp::{ModelInfo, ModelState};
        let (mut app, _) = app_with_session();
        app.sessions[0].models = ModelState {
            current: Some("a".into()),
            available: vec![
                ModelInfo {
                    id: "a".into(),
                    name: "Alpha".into(),
                },
                ModelInfo {
                    id: "b".into(),
                    name: "Beta".into(),
                },
            ],
            ..Default::default()
        };
        let say = |app: &mut App, t: &str| {
            app.mode = Mode::Insert;
            app.input.set_text(t);
            app.handle_key(key(KeyCode::Enter));
        };
        say(&mut app, "/model");
        assert_eq!(app.mode, Mode::Model);
        assert!(app.input.is_empty());
        assert!(app.sessions[0].entries.is_empty(), "not sent as a prompt");

        say(&mut app, "/model nope");
        assert!(app.message().unwrap().contains("Alpha, Beta"));
        assert_eq!(app.sessions[0].status, Status::Idle);

        say(&mut app, "/model beta");
        assert!(app.sessions[0].entries.is_empty());

        // Not the command: goes to the agent as a normal prompt.
        say(&mut app, "/models please");
        assert_eq!(app.sessions[0].status, Status::Running);
    }

    #[test]
    fn effort_picker_selects_between_turns() {
        use crate::agent::acp::{EffortInfo, EffortState};
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app
            .message()
            .unwrap()
            .contains("does not offer effort selection"));

        app.sessions[0].effort = EffortState {
            current: Some("high".into()),
            available: vec![
                EffortInfo {
                    id: "default".into(),
                    name: "Default".into(),
                    description: None,
                },
                EffortInfo {
                    id: "medium".into(),
                    name: "Medium".into(),
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
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.mode, Mode::Effort);
        assert_eq!(app.effort_cursor, 2, "starts on the current effort level");
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.effort_cursor, 1);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);

        // Mid-turn effort change is refused with a notice.
        app.sessions[0].status = Status::Running;
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.message(), Some("change the effort level between turns"));
    }

    #[test]
    fn slash_effort_is_handled_locally() {
        use crate::agent::acp::{EffortInfo, EffortState};
        let (mut app, _) = app_with_session();
        app.sessions[0].effort = EffortState {
            current: Some("default".into()),
            available: vec![
                EffortInfo {
                    id: "default".into(),
                    name: "Default".into(),
                    description: None,
                },
                EffortInfo {
                    id: "low".into(),
                    name: "Low".into(),
                    description: None,
                },
                EffortInfo {
                    id: "high".into(),
                    name: "High".into(),
                    description: None,
                },
            ],
            config_id: Some("effort".into()),
        };
        let say = |app: &mut App, t: &str| {
            app.mode = Mode::Insert;
            app.input.set_text(t);
            app.handle_key(key(KeyCode::Enter));
        };
        say(&mut app, "/effort");
        assert_eq!(app.mode, Mode::Effort);
        assert!(app.input.is_empty());
        assert!(app.sessions[0].entries.is_empty(), "not sent as a prompt");

        say(&mut app, "/effort nope");
        assert!(app.message().unwrap().contains("Default, Low, High"));
        assert_eq!(app.sessions[0].status, Status::Idle);

        say(&mut app, "/effort HIGH");
        assert!(app.sessions[0].entries.is_empty());

        // Not the command: goes to the agent as a normal prompt.
        say(&mut app, "/efforts please");
        assert_eq!(app.sessions[0].status, Status::Running);
    }

    #[test]
    fn m_toggles_mouse_capture() {
        let (mut app, _) = app_with_session();
        assert!(app.mouse);
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('m')));
        assert!(!app.mouse);
    }

    #[test]
    fn ctrl_o_and_leader_t_toggle_trace_and_clear_overrides() {
        let (mut app, _) = app_with_session();
        // Compact by default.
        assert!(!app.show_trace);
        assert!(!app.is_trace_expanded(0, 1));

        // Per-block override flips just that block.
        app.trace_overrides.insert((0, 1));
        assert!(app.is_trace_expanded(0, 1));
        assert!(!app.is_trace_expanded(0, 5));

        // leader + t expands globally and clears per-block overrides.
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('t')));
        assert!(app.show_trace);
        assert!(app.trace_overrides.is_empty());
        assert!(app.is_trace_expanded(0, 1));
        assert!(app.is_trace_expanded(0, 5));
        assert_eq!(app.message(), Some("showing full thoughts & tool details"));

        // Ctrl-o toggles directly from Insert mode too.
        app.mode = Mode::Insert;
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(!app.show_trace);
        assert_eq!(
            app.message(),
            Some("compact view: thoughts & tools collapsed")
        );
        assert_eq!(app.mode, Mode::Insert);

        // Ctrl-a + t also works from Insert mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('t')));
        assert!(app.show_trace);
        assert_eq!(app.mode, Mode::Insert);

        // Ctrl-o is ignored while a modal is open.
        app.mode = Mode::Help;
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(app.show_trace);
    }

    #[test]
    fn clicking_trace_toggle_row_flips_only_that_block() {
        let (mut app, _) = app_with_session();
        let area = Rect::new(0, 0, 80, 20);
        // Simulate a rendered transcript with a toggleable trace block on line 2 (entry 1).
        app.max_scroll.set(0);
        *app.transcript_text.borrow_mut() = vec![
            "  ▎ fix it".into(),
            "".into(),
            "  ▸ ✓ thought · 2 tools (read, edit)".into(),
            "".into(),
            "    Done.".into(),
        ];
        *app.trace_toggles.borrow_mut() = vec![(2, 1)];

        // Transcript starts at row 2 (header is 2 rows), so content line 2 is terminal row 4.
        let click = |kind| MouseEvent {
            kind,
            column: 6,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)), area);
        app.handle_mouse(click(MouseEventKind::Up(MouseButton::Left)), area);
        assert!(app.is_trace_expanded(0, 1));
        assert!(!app.is_trace_expanded(0, 5));
        assert!(app.pending_copy.is_none());

        // Clicking the same row again collapses it back.
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)), area);
        app.handle_mouse(click(MouseEventKind::Up(MouseButton::Left)), area);
        assert!(!app.is_trace_expanded(0, 1));

        // Dragging across the row still selects & copies instead of toggling.
        app.handle_mouse(click(MouseEventKind::Down(MouseButton::Left)), area);
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 18,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 18,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(!app.is_trace_expanded(0, 1));
        assert!(app.pending_copy.is_some());
    }

    #[test]
    fn esc_in_normal_mode_does_not_cancel_the_turn() {
        let (mut app, _) = app_with_session();
        app.sessions[0].status = Status::Running;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.sessions[0].status, Status::Running);
    }

    fn type_keys(app: &mut App, ks: &str) {
        for c in ks.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn vim_keys_edit_the_prompt_instead_of_closing_the_session() {
        let (mut app, _) = app_with_session();
        app.input.set_text("one\ntwo");
        app.sessions[0]
            .entries
            .push(crate::session::Entry::Agent("x".into()));
        type_keys(&mut app, "ddq");
        assert_eq!(app.input.text(), "one");
        assert!(app.confirm.is_none() && !app.should_quit);
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn leader_runs_app_commands_from_both_modes() {
        let (mut app, _) = app_with_session();
        app.sessions[0]
            .entries
            .push(crate::session::Entry::Agent("x".into()));
        type_keys(&mut app, " d");
        assert_eq!(app.confirm, Some(Confirm::CloseSession));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.sessions.len(), 1);

        // From insert mode via Ctrl-A; the mode survives a command that
        // does not change it, and the next key types normally again.
        type_keys(&mut app, "i");
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(app.leader);
        type_keys(&mut app, "m");
        assert!(!app.mouse && !app.leader);
        assert_eq!(app.mode, Mode::Insert);
        type_keys(&mut app, "d");
        assert_eq!(app.input.text(), "d");
    }

    #[test]
    fn normal_mode_edits_then_returns_to_insert() {
        let (mut app, _) = app_with_session();
        type_keys(&mut app, "ihello world");
        app.handle_key(key(KeyCode::Esc));
        type_keys(&mut app, "bcwthere");
        assert_eq!(app.mode, Mode::Insert);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.input.text(), "hello there");
        type_keys(&mut app, "u");
        assert_eq!(app.input.text(), "hello world");
    }

    #[test]
    fn app_keys_are_direct_without_sessions() {
        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut app = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_d_quits_from_any_mode() {
        let (mut app, _) = app_with_session();
        app.mode = Mode::Insert;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_u_and_ctrl_d_scroll_half_page_in_normal_mode() {
        let (mut app, _) = app_with_session();
        app.max_scroll.set(30);
        // Default transcript_height is 20 -> half-page step is 10.
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        app.handle_key(ctrl('u'));
        assert_eq!(app.scroll, 10);
        assert!(!app.should_quit);
        app.handle_key(ctrl('u'));
        assert_eq!(app.scroll, 20);
        app.handle_key(ctrl('d'));
        assert_eq!(app.scroll, 10);
        assert!(!app.should_quit);
        app.handle_key(ctrl('d'));
        assert_eq!(app.scroll, 0);
        assert!(!app.should_quit);

        // Custom viewport height scales the step and floors at 1.
        app.transcript_height.set(12);
        app.handle_key(ctrl('u'));
        assert_eq!(app.scroll, 6);
        app.transcript_height.set(1);
        app.handle_key(ctrl('u'));
        assert_eq!(app.scroll, 7);

        // Leader + Ctrl-u / Ctrl-d also scroll half-page from Insert mode.
        app.transcript_height.set(10);
        app.mode = Mode::Insert;
        app.handle_key(ctrl('a'));
        app.handle_key(ctrl('u'));
        assert_eq!(app.scroll, 12);
        assert_eq!(app.mode, Mode::Insert);
        app.handle_key(ctrl('a'));
        app.handle_key(ctrl('d'));
        assert_eq!(app.scroll, 7);
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_d_quits_when_not_scrolling_transcript() {
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);

        // No sessions open.
        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut empty = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
        empty.handle_key(ctrl_d);
        assert!(empty.should_quit);

        // Confirm modal open.
        let (mut app, _) = app_with_session();
        app.confirm = Some(Confirm::Quit);
        app.handle_key(ctrl_d);
        assert!(app.should_quit);

        // Permission modal open.
        let (mut app, _) = app_with_session();
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(PermissionRequest {
                tool_call_id: "c".into(),
                title: Some("run".into()),
                options: vec![],
                ..Default::default()
            }),
        });
        app.handle_key(ctrl_d);
        assert!(app.should_quit);

        // Search mode.
        let (mut app, _) = app_with_session();
        app.mode = Mode::Search;
        app.handle_key(ctrl_d);
        assert!(app.should_quit);
    }

    #[test]
    fn search_finds_case_insensitive_matches_cycles_and_clears() {
        let (mut app, _) = app_with_session();
        let mut lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        lines[2] = "  ▎ Fix the BUG".into();
        lines[14] = "    bug again".into();
        lines[27] = "    another Bug".into();
        *app.transcript_text.borrow_mut() = lines;
        app.transcript_height.set(10);
        app.max_scroll.set(20);

        // `/` enters search mode and searches live.
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Search);
        type_keys(&mut app, "bug");
        assert_eq!(app.search_query, "bug");
        assert_eq!(app.search_matches, vec![2, 14, 27]);
        assert_eq!(app.search_current, Some(0));
        // Line 2 is visible: from_top = 20 - scroll <= 2 < from_top + 10.
        let from_top = 20 - app.scroll as usize;
        assert!((from_top..from_top + 10).contains(&2));

        // Backspace & Ctrl-u edit the query live.
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.search_query, "bu");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.search_query.is_empty());
        assert!(app.search_matches.is_empty());
        assert_eq!(app.search_current, None);

        // Enter confirms and returns to Normal while keeping highlights active.
        type_keys(&mut app, "BUG");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.search_query, "BUG");
        assert_eq!(app.search_current, Some(0));

        // n / N step forward and backward with wrap-around and scroll into view.
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.search_current, Some(1));
        let from_top = 20 - app.scroll as usize;
        assert!((from_top..from_top + 10).contains(&14));

        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.search_current, Some(2));
        let from_top = 20 - app.scroll as usize;
        assert!((from_top..from_top + 10).contains(&27));

        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.search_current, Some(0));

        app.handle_key(key(KeyCode::Char('N')));
        assert_eq!(app.search_current, Some(2));

        // Pending operator + Esc cancels the operator without clearing search.
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.input.pending(), "d");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.input.pending().is_empty());
        assert_eq!(app.search_query, "BUG");

        // Esc with no pending operator clears the active search.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.search_query.is_empty());
        assert!(app.search_matches.is_empty());
        assert_eq!(app.search_current, None);
    }

    #[test]
    fn search_esc_clears_leader_opens_and_session_switch_clears() {
        let (mut app, _) = app_with_session();
        let (s1, _) = Session::detached(1, agent::find("antigravity").unwrap());
        app.sessions.push(s1);
        *app.transcript_text.borrow_mut() = vec!["alpha".into(), "beta".into()];

        // Leader + / opens search from Insert mode as well.
        app.mode = Mode::Insert;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Search);
        type_keys(&mut app, "alpha");
        assert_eq!(app.search_matches, vec![0]);

        // Esc inside Search mode clears and returns to Normal.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.search_query.is_empty());
        assert!(app.search_matches.is_empty());

        // Switching sessions clears an active search.
        app.handle_key(key(KeyCode::Char('/')));
        type_keys(&mut app, "beta");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.search_query, "beta");
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);
        assert!(app.search_query.is_empty());
        assert!(app.search_matches.is_empty());
        assert_eq!(app.search_current, None);
    }

    #[test]
    fn default_agent_uses_previous_convo_and_survives_close() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("/repo-a"),
            PathBuf::from("."),
            store.clone(),
            tx,
        );

        // No prior conversation: falls back to agent::default_agent().
        assert_eq!(
            app.default_agent().map(|s| s.id),
            agent::default_agent().map(|s| s.id)
        );

        // Sending a prompt in an Antigravity session records it as the default.
        let (mut s0, _) = Session::detached(0, agent::find("claude").unwrap());
        s0.status = Status::Idle;
        let (mut s1, _) = Session::detached(1, agent::find("antigravity").unwrap());
        s1.status = Status::Idle;
        app.sessions.push(s0);
        app.sessions.push(s1);
        app.selected = 1;
        app.mode = Mode::Insert;
        app.input.set_text("hello");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.default_agent().map(|s| s.id), Some("antigravity"));

        // Closing the session deletes its transcript, but keeps the last agent.
        app.mode = Mode::Normal;
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.default_agent().map(|s| s.id), Some("antigravity"));

        // A fresh App in another directory still defaults to Antigravity.
        let (tx2, _rx2) = mpsc::channel();
        let app2 = App::new(
            PathBuf::from("/repo-b"),
            PathBuf::from("."),
            store.clone(),
            tx2,
        );
        assert_eq!(app2.default_agent().map(|s| s.id), Some("antigravity"));

        // Unknown / removed agent ids fall back to agent::default_agent().
        store.save_last_agent("removed-agent").unwrap();
        assert_eq!(
            app2.default_agent().map(|s| s.id),
            agent::default_agent().map(|s| s.id)
        );
    }

    #[test]
    fn default_agent_picks_up_existing_session_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        store
            .save(
                "100-codex",
                &crate::store::Record {
                    agent: "codex".into(),
                    cwd: PathBuf::from("/earlier"),
                    worktree: None,
                    entries: vec![Entry::User("hi".into())],
                    model: None,
                    effort: None,
                    label: None,
                },
            )
            .unwrap();
        let (tx, _rx) = mpsc::channel();
        let app = App::new(PathBuf::from("/now"), PathBuf::from("."), store, tx);
        assert_eq!(app.default_agent().map(|s| s.id), Some("codex"));
    }

    #[test]
    fn i_and_enter_open_picker_when_no_sessions() {
        for k in [KeyCode::Char('i'), KeyCode::Enter] {
            let (tx, _rx) = mpsc::channel();
            let store = Store::new(tempfile::tempdir().unwrap().keep());
            let mut app = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
            app.handle_key(key(k));
            assert_eq!(app.mode, Mode::Picker, "key {k:?} should open picker");
            assert!(!app.picker_isolated);
            assert!(!app.picker.is_empty());
        }
    }

    #[test]
    fn single_agent_picker_spawns_directly_without_modal() {
        static ONLY: agent::AgentSpec = agent::AgentSpec {
            id: "fake",
            name: "Fake",
            bin: "nonexistent-mijote-test-agent",
            args: &[],
            install: agent::Install::None,
            note: "test only",
        };
        static OTHER: agent::AgentSpec = agent::AgentSpec {
            id: "fake2",
            name: "Fake 2",
            bin: "nonexistent-mijote-test-agent-2",
            args: &[],
            install: agent::Install::None,
            note: "test only",
        };
        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut app = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
        app.open_picker_with(vec![&ONLY], false);
        assert_eq!(app.mode, Mode::Insert);
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].spec.id, "fake");
        assert!(app.sessions[0].worktree.is_none());

        let (tx2, _rx2) = mpsc::channel();
        let store2 = Store::new(tempfile::tempdir().unwrap().keep());
        let mut app2 = App::new(PathBuf::from("."), PathBuf::from("."), store2, tx2);
        app2.open_picker_with(vec![&ONLY, &OTHER], true);
        assert_eq!(app2.mode, Mode::Picker);
        assert!(app2.picker_isolated);
        assert!(app2.sessions.is_empty());
        assert_eq!(app2.picker_status.len(), 2);
    }

    #[test]
    fn closing_last_session_from_insert_allows_reopening_picker() {
        let (mut app, _) = app_with_session();
        app.mode = Mode::Insert;
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('d')));
        assert!(app.sessions.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.mode, Mode::Picker);
    }

    #[test]
    fn notices_persist_across_keys_until_ttl_expires() {
        let (mut app, _) = app_with_session();
        app.set_message("mouse off");
        assert_eq!(app.message(), Some("mouse off"));

        // Subsequent key presses and pastes must not clear an active notice.
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_paste("bc");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.message(), Some("mouse off"));

        // A newer notice replaces the current one.
        app.set_message("copied selection");
        assert_eq!(app.message(), Some("copied selection"));

        // Once expired, message() returns None and tick() clears storage.
        app.notice.as_mut().unwrap().expires_at = Instant::now();
        assert_eq!(app.message(), None);
        app.tick();
        assert!(app.notice.is_none());
    }

    #[test]
    fn help_overlay_opens_and_closes() {
        // Empty sessions: `?` directly opens help.
        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut empty = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
        empty.handle_key(key(KeyCode::Char('?')));
        assert_eq!(empty.mode, Mode::Help);
        empty.handle_key(key(KeyCode::Esc));
        assert_eq!(empty.mode, Mode::Normal);

        // With an open session: `?` in normal mode opens help, `q` closes it.
        let (mut app, _) = app_with_session();
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Help);
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.mode, Mode::Normal);

        // Leader + `?` from normal mode opens help, `?` closes it.
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Help);
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Normal);

        // Leader (Ctrl-A) + `?` from insert mode opens help.
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.mode, Mode::Insert);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Help);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);

        // Pending vim replace (`r?`) still edits the prompt instead of opening help.
        app.input.set_text("a");
        app.input.normal('0');
        app.handle_key(key(KeyCode::Char('r')));
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.input.text(), "?");
    }

    #[test]
    fn leader_r_renames_session_persists_and_clears_when_empty() {
        let (mut app, _) = app_with_session();
        app.sessions[0].key = "k-rename".into();
        app.sessions[0].entries.push(Entry::User("hi".into()));

        // Open rename via leader + r, type a label, fix a typo, save.
        type_keys(&mut app, " r");
        assert_eq!(app.mode, Mode::Rename);
        assert_eq!(app.rename_target, 0);
        type_keys(&mut app, "fix-retryx");
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.sessions[0].label.as_deref(), Some("fix-retry"));
        let saved = app.store.load(app.cwd());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].1.label.as_deref(), Some("fix-retry"));

        // Opening again pre-fills the existing label; Esc cancels changes.
        type_keys(&mut app, " r");
        assert_eq!(app.rename_buf, "fix-retry");
        type_keys(&mut app, "-extra");
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.sessions[0].label.as_deref(), Some("fix-retry"));

        // Ctrl-u clears the buffer; Enter with empty string clears the label.
        type_keys(&mut app, " r");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.rename_buf.is_empty());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.sessions[0].label, None);
        let saved = app.store.load(app.cwd());
        assert_eq!(saved[0].1.label, None);
    }

    #[test]
    fn switcher_r_renames_highlighted_background_session() {
        let (mut app, _) = app_with_session();
        let (s1, _) = Session::detached(1, agent::find("claude").unwrap());
        app.sessions.push(s1);
        assert_eq!(app.selected, 0);

        // Open switcher, highlight session 1, press r, rename it.
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.mode, Mode::Switcher);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.switcher_cursor, 1);
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(app.mode, Mode::Rename);
        assert_eq!(app.rename_target, 1);
        type_keys(&mut app, "  bg-task  ");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.mode, Mode::Switcher);
        assert_eq!(app.selected, 0);
        assert_eq!(app.sessions[0].label, None);
        assert_eq!(app.sessions[1].label.as_deref(), Some("bg-task"));
    }

    #[test]
    fn background_turn_done_and_permission_set_attention_and_bell() {
        let (mut app, _) = app_with_session();
        let (mut s1, _) = Session::detached(1, agent::find("claude").unwrap());
        s1.status = Status::Running;
        app.sessions.push(s1);
        app.selected = 0;

        // Foreground TurnDone does not set attention or ring the bell.
        app.sessions[0].status = Status::Running;
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::TurnDone(crate::agent::acp::StopReason::EndTurn),
        });
        assert!(!app.sessions[0].attention);
        assert!(!app.pending_bell);

        // Background TurnDone sets attention on session 1 and pending_bell.
        app.handle_msg(Msg {
            session: 1,
            body: UiMsg::TurnDone(crate::agent::acp::StopReason::EndTurn),
        });
        assert!(app.sessions[1].attention);
        assert!(app.pending_bell);
        app.pending_bell = false;

        // Switching to session 1 via switcher clears its attention.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.selected, 1);
        assert!(!app.sessions[1].attention);

        // Background Permission on session 0 sets AwaitingPermission and rings bell.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Permission(PermissionRequest {
                tool_call_id: "c1".into(),
                title: Some("run tests".into()),
                options: vec![PermissionOption {
                    id: "ok".into(),
                    name: "Allow".into(),
                    kind: "allow_once".into(),
                }],
                ..Default::default()
            }),
        });
        assert_eq!(app.sessions[0].status, Status::AwaitingPermission);
        assert!(app.pending_bell);

        // Background TurnDone on session 0 cleared when switching via leader+j.
        app.pending_bell = false;
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::TurnDone(crate::agent::acp::StopReason::EndTurn),
        });
        assert!(app.sessions[0].attention);
        assert!(app.pending_bell);
        type_keys(&mut app, " j");
        assert_eq!(app.selected, 0);
        assert!(!app.sessions[0].attention);
    }

    #[test]
    fn opening_picker_caches_status_for_every_agent() {
        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut app = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::Picker);
        assert!(!app.picker.is_empty());
        assert_eq!(app.picker_status.len(), app.picker.len());
    }

    #[test]
    fn at_mention_popup_filters_selects_accepts_and_dismisses() {
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("src")).unwrap();
        std::fs::write(work.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(work.path().join("src/app.rs"), "").unwrap();
        std::fs::write(work.path().join("Cargo.toml"), "").unwrap();

        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut app = App::new(
            work.path().to_path_buf(),
            work.path().to_path_buf(),
            store,
            tx,
        );
        let (mut s, _) = Session::detached(0, agent::find("claude").unwrap());
        s.status = Status::Idle;
        app.sessions.push(s);
        app.mode = Mode::Insert;

        type_keys(&mut app, "check @ma");
        let popup = app.mention.as_ref().expect("mention popup should be open");
        assert_eq!(popup.query, "ma");
        assert_eq!(popup.matches[0].path, "src/main.rs");

        // Tab accepts the selected match and inserts a trailing space.
        app.handle_key(key(KeyCode::Tab));
        assert!(app.mention.is_none());
        assert_eq!(app.input.text(), "check @src/main.rs ");

        // Typing `@` again opens the popup; Down moves selection; Esc closes popup
        // while staying in Insert mode, and a second Esc returns to Normal mode.
        type_keys(&mut app, "@");
        let popup = app.mention.as_ref().unwrap();
        assert!(popup.matches.len() >= 2);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.mention.as_ref().unwrap().cursor, 1);

        app.handle_key(key(KeyCode::Esc));
        assert!(app.mention.is_none());
        assert_eq!(app.mode, Mode::Insert);

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn question_modal_handles_select_multi_custom_validation_decline_and_cancel() {
        use crate::agent::acp::{QuestionField, QuestionOption};

        let (tx, _rx) = mpsc::channel();
        let store = Store::new(tempfile::tempdir().unwrap().keep());
        let mut app = App::new(PathBuf::from("."), PathBuf::from("."), store, tx);
        let (s, _, q_rx) = Session::detached_with_questions(0, agent::find("claude").unwrap());
        app.sessions.push(s);

        let req = ElicitationRequest {
            message: "Configure backend".into(),
            fields: vec![
                QuestionField {
                    key: "question_0".into(),
                    title: Some("Database".into()),
                    description: None,
                    required: false,
                    kind: FieldKind::SingleSelect {
                        options: vec![
                            QuestionOption {
                                value: "SQLite".into(),
                                label: "SQLite".into(),
                                description: Some("Local".into()),
                            },
                            QuestionOption {
                                value: "Postgres".into(),
                                label: "Postgres".into(),
                                description: Some("Server".into()),
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
                QuestionField {
                    key: "question_1".into(),
                    title: Some("Features".into()),
                    description: None,
                    required: false,
                    kind: FieldKind::MultiSelect {
                        options: vec![
                            QuestionOption {
                                value: "TLS".into(),
                                label: "TLS".into(),
                                description: None,
                            },
                            QuestionOption {
                                value: "Metrics".into(),
                                label: "Metrics".into(),
                                description: None,
                            },
                        ],
                        default: vec![],
                    },
                },
            ],
        };

        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Question(req.clone()),
        });
        assert_eq!(app.sessions[0].status, Status::AwaitingQuestion);

        // Move to Postgres, Tab to Other and add a note, Tab to Features, toggle both, Enter submits.
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::Tab));
        type_keys(&mut app, "v16");
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Enter));

        match q_rx.try_recv().unwrap() {
            ElicitationResponse::Accept(map) => {
                assert_eq!(map.get("question_0"), Some(&serde_json::json!("Postgres")));
                assert_eq!(
                    map.get("question_0_custom"),
                    Some(&serde_json::json!("v16"))
                );
                assert_eq!(
                    map.get("question_1"),
                    Some(&serde_json::json!(["TLS", "Metrics"]))
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
        assert_eq!(app.sessions[0].status, Status::Running);

        // Required integer field rejects invalid input and stays open until valid.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Question(ElicitationRequest {
                message: "Port".into(),
                fields: vec![QuestionField {
                    key: "port".into(),
                    title: Some("Port".into()),
                    description: None,
                    required: true,
                    kind: FieldKind::Number {
                        integer: true,
                        default: None,
                    },
                }],
            }),
        });
        type_keys(&mut app, "abc");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.sessions[0].pending_question.is_some());
        assert!(app.question_draft.as_ref().unwrap().error.is_some());
        assert!(q_rx.try_recv().is_err());

        // Clear with Ctrl-u, enter valid integer, submit.
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_keys(&mut app, "8080");
        app.handle_key(key(KeyCode::Enter));
        match q_rx.try_recv().unwrap() {
            ElicitationResponse::Accept(map) => {
                assert_eq!(map.get("port"), Some(&serde_json::json!(8080)));
            }
            other => panic!("expected Accept, got {other:?}"),
        }

        // Esc cancels and `d` declines.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Question(req.clone()),
        });
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(q_rx.try_recv().unwrap(), ElicitationResponse::Decline);

        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Question(req),
        });
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(q_rx.try_recv().unwrap(), ElicitationResponse::Cancel);
    }

    #[test]
    fn active_slash_command_detects_leading_token_only() {
        let check = |s: &str, cursor: usize| {
            let chars: Vec<char> = s.chars().collect();
            active_slash_command(&chars, cursor)
        };
        assert_eq!(check("", 0), None);
        assert_eq!(check("/", 1), Some((0, String::new())));
        assert_eq!(check("/mo", 3), Some((0, "mo".into())));
        assert_eq!(check("  /rev", 6), Some((2, "rev".into())));
        assert_eq!(check("/model ", 7), None);
        assert_eq!(check("hello /model", 12), None);
        assert_eq!(check("/foo\n/bar", 9), None);
        assert_eq!(check("/review @src/main.rs", 20), None);
    }

    #[test]
    fn slash_command_popup_lists_filters_completes_runs_and_refreshes_live() {
        use crate::agent::acp::{AvailableCommand, ModelInfo, ModelState};

        let (mut app, _) = app_with_session();
        app.sessions[0].models = ModelState {
            current: Some("opus".into()),
            available: vec![ModelInfo {
                id: "opus".into(),
                name: "Opus".into(),
            }],
            ..Default::default()
        };
        app.mode = Mode::Insert;

        // Typing `/` opens popup with local commands even before agent advertises any.
        app.handle_key(key(KeyCode::Char('/')));
        let popup = app.slash.as_ref().expect("slash popup should open");
        assert_eq!(
            popup
                .matches
                .iter()
                .map(|m| m.command.name.as_str())
                .collect::<Vec<_>>(),
            vec!["model", "effort"]
        );

        // Live `available_commands_update` refreshes the open popup and deduplicates `/model`.
        app.handle_msg(Msg {
            session: 0,
            body: UiMsg::Event(Event::AvailableCommands(vec![
                AvailableCommand {
                    name: "model".into(),
                    description: "Agent model".into(),
                    input_hint: None,
                },
                AvailableCommand {
                    name: "review".into(),
                    description: "Review recent changes".into(),
                    input_hint: None,
                },
                AvailableCommand {
                    name: "web".into(),
                    description: "Search the web".into(),
                    input_hint: Some("query".into()),
                },
            ])),
        });
        let popup = app.slash.as_ref().unwrap();
        assert_eq!(
            popup
                .matches
                .iter()
                .map(|m| (m.command.name.as_str(), m.command.local))
                .collect::<Vec<_>>(),
            vec![
                ("model", true),
                ("effort", true),
                ("review", false),
                ("web", false),
            ]
        );

        // Esc dismisses popup while staying in Insert mode.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.slash.is_none());
        assert_eq!(app.mode, Mode::Insert);

        // Filtering by description substring finds `/web` when typing `search`.
        type_keys(&mut app, "search");
        let popup = app.slash.as_ref().unwrap();
        assert_eq!(popup.matches.len(), 1);
        assert_eq!(popup.matches[0].command.name, "web");

        // Enter on a prefix match for a command with `input_hint` inserts `/web ` for args.
        app.handle_key(key(KeyCode::Enter));
        assert!(app.slash.is_none());
        assert_eq!(app.input.text(), "/web ");
        assert_eq!(app.sessions[0].status, Status::Idle);

        // Clear and test `Tab` completion on `/mo` -> `/model `.
        app.input.clear();
        type_keys(&mut app, "/mo");
        assert_eq!(app.slash.as_ref().unwrap().matches[0].command.name, "model");
        app.handle_key(key(KeyCode::Tab));
        assert!(app.slash.is_none());
        assert_eq!(app.input.text(), "/model ");

        // Clear and test `Enter` on `/mo` -> opens local model picker immediately.
        app.input.clear();
        type_keys(&mut app, "/mo");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.slash.is_none());
        assert_eq!(app.mode, Mode::Model);
        assert!(app.input.is_empty());

        // Return to insert, test `Enter` on `/rev` (no input_hint) -> completes & submits `/review`.
        app.mode = Mode::Insert;
        type_keys(&mut app, "/rev");
        assert_eq!(
            app.slash.as_ref().unwrap().matches[0].command.name,
            "review"
        );
        app.handle_key(key(KeyCode::Enter));
        assert!(app.slash.is_none());
        assert!(app.input.is_empty());
        assert_eq!(app.sessions[0].status, Status::Running);
        assert_eq!(app.sessions[0].entries, vec![Entry::User("/review".into())]);

        // Exact match on a command WITH `input_hint` (`/web`) + Enter submits directly.
        app.sessions[0].status = Status::Idle;
        app.sessions[0].entries.clear();
        type_keys(&mut app, "/web");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.slash.is_none());
        assert_eq!(app.sessions[0].entries, vec![Entry::User("/web".into())]);

        // Typing `/review @` closes slash popup and opens `@` file-mention popup.
        app.sessions[0].status = Status::Idle;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();
        app.cwd = dir.path().to_path_buf();
        type_keys(&mut app, "/review @");
        assert!(app.slash.is_none());
        assert!(app.mention.is_some());
    }

    fn send_prompt(app: &mut App, text: &str) {
        app.sessions[app.selected].status = Status::Idle;
        app.mode = Mode::Insert;
        for c in text.chars() {
            if c == '\n' {
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
            } else {
                app.handle_key(key(KeyCode::Char(c)));
            }
        }
        app.handle_key(key(KeyCode::Enter));
        app.sessions[app.selected].status = Status::Idle;
    }

    #[test]
    fn prompt_history_up_down_in_insert_mode_and_draft_restoration() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("sessions");
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(
            PathBuf::from("."),
            PathBuf::from("."),
            Store::new(store_dir.clone()),
            tx,
        );
        let (mut s, _) = Session::detached(0, agent::find("claude").unwrap());
        s.status = Status::Idle;
        app.sessions.push(s);

        // Sending prompts records history and skips consecutive duplicates.
        send_prompt(&mut app, "first prompt");
        send_prompt(&mut app, "second prompt");
        send_prompt(&mut app, "second prompt");

        // Type an unsent draft with a collapsed large paste.
        app.mode = Mode::Insert;
        for c in "wip ".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let big_paste = "a\nb\nc\nd\ne";
        app.handle_paste(big_paste);
        let draft_text = app.input.text();
        let draft_cursor = app.input.cursor_index();
        assert!(draft_text.starts_with("wip [Pasted text #1"));

        // Up steps backward through history; stops cleanly at the oldest entry.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "second prompt");
        assert_eq!(app.input.cursor_pos(), (0, "second prompt".len()));

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "first prompt");

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "first prompt");

        // Down steps forward and restores the exact draft (including collapsed paste).
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "second prompt");

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), draft_text);
        assert_eq!(app.input.cursor_index(), draft_cursor);

        // Submitting the restored draft still expands its collapsed paste.
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.sessions[0].entries.last(),
            Some(&Entry::User(format!("wip {big_paste}")))
        );

        // History persists on disk for a fresh App sharing the same Store.
        let (tx2, _rx2) = mpsc::channel();
        let mut app2 = App::new(
            PathBuf::from("."),
            PathBuf::from("."),
            Store::new(store_dir),
            tx2,
        );
        let (mut s2, _) = Session::detached(0, agent::find("claude").unwrap());
        s2.status = Status::Idle;
        app2.sessions.push(s2);
        app2.mode = Mode::Insert;
        app2.handle_key(key(KeyCode::Up));
        assert_eq!(app2.input.text(), format!("wip {big_paste}"));
        app2.handle_key(key(KeyCode::Up));
        assert_eq!(app2.input.text(), "second prompt");
        app2.handle_key(key(KeyCode::Up));
        assert_eq!(app2.input.text(), "first prompt");
    }

    #[test]
    fn prompt_history_j_k_and_arrows_in_normal_mode_respect_vim_operators() {
        let (mut app, _) = app_with_session();
        let (mut s2, _) = Session::detached(1, agent::find("antigravity").unwrap());
        s2.status = Status::Idle;
        app.sessions.push(s2);

        send_prompt(&mut app, "alpha");
        send_prompt(&mut app, "beta");

        app.mode = Mode::Normal;
        app.input.set_text_at("draft", 2, true);

        // k / j navigate history in Normal mode and clamp the cursor onto a char.
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.input.text(), "beta");
        assert_eq!(app.input.cursor_pos(), (0, 3));

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "alpha");
        assert_eq!(app.input.cursor_pos(), (0, 4));

        // Horizontal cursor motion does not detach history browsing.
        app.handle_key(key(KeyCode::Char('0')));
        assert_eq!(app.input.cursor_pos(), (0, 0));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.input.text(), "beta");

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "draft");
        assert_eq!(app.input.cursor_pos(), (0, 2));

        // Vim replace `rk` / `rj` and linewise operators `dk` / `dj` still work.
        app.input.set_text_at("ab\ncd", 0, true);
        app.handle_key(key(KeyCode::Char('r')));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.input.text(), "kb\ncd");

        app.handle_key(key(KeyCode::Char('d')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.input.text(), "");

        // Leader + j/k still switches sessions rather than browsing prompt history.
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn prompt_history_multiline_prompts_and_edits_preserve_user_text() {
        let (mut app, _) = app_with_session();
        send_prompt(&mut app, "oldest");
        send_prompt(&mut app, "line1\nline2\nline3");
        send_prompt(&mut app, "newest");

        // Multi-line draft: Up moves within the buffer first, then recalls history at row 0.
        app.mode = Mode::Insert;
        app.input.set_text("d1\nd2");
        assert_eq!(app.input.cursor_pos(), (1, 2));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "d1\nd2");
        assert_eq!(app.input.cursor_pos(), (0, 2));

        // At row 0, repeated Up steps cleanly through multi-line history entries in 1 press each.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "newest");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "line1\nline2\nline3");
        assert_eq!(app.input.cursor_pos().0, 0);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "oldest");

        // Repeated Down steps cleanly forward through multi-line entries in 1 press each.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "line1\nline2\nline3");
        assert_eq!(app.input.cursor_pos().0, 2);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "newest");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "d1\nd2");

        // Reversing direction inside a multi-line recalled entry lets you move across its lines.
        app.handle_key(key(KeyCode::Up)); // newest
        app.handle_key(key(KeyCode::Up)); // line1\nline2\nline3 (row 0)
        app.handle_key(key(KeyCode::Down)); // row 1 inside multi-line prompt
        assert_eq!(app.input.text(), "line1\nline2\nline3");
        assert_eq!(app.input.cursor_pos().0, 1);

        // Editing a recalled prompt detaches history so Down never clobbers the edit.
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.input.text(), "line1\nline2!\nline3");
        app.handle_key(key(KeyCode::Down)); // row 2
        app.handle_key(key(KeyCode::Down)); // bottom line: must NOT clobber edit
        assert_eq!(app.input.text(), "line1\nline2!\nline3");

        // Pressing Esc + u undoes the insert-mode edit back to the recalled prompt.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        app.handle_key(key(KeyCode::Char('u')));
        assert_eq!(app.input.text(), "line1\nline2\nline3");
    }

    #[test]
    fn prompt_history_does_not_conflict_with_popups_and_seeds_on_restore() {
        let (mut app, _) = app_with_session();
        send_prompt(&mut app, "plain prompt");
        send_prompt(&mut app, "/review");

        // Recalling a slash command must NOT open the slash popup and hijack the next Up.
        app.mode = Mode::Insert;
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "/review");
        assert!(app.slash.is_none());
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "plain prompt");

        // When the slash popup IS open, Up/Down still navigate the popup rather than history.
        app.input.clear();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.slash.is_some());
        assert_eq!(app.slash.as_ref().unwrap().cursor, 0);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "/");
        assert_eq!(app.slash.as_ref().unwrap().cursor, 1);

        // Restoring saved sessions seeds history when no prompt-history file exists yet.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        let cwd = PathBuf::from("/proj");
        store
            .save(
                "1-claude",
                &crate::store::Record {
                    agent: "claude".into(),
                    cwd: cwd.clone(),
                    worktree: None,
                    entries: vec![
                        Entry::User("restored one".into()),
                        Entry::Agent("reply".into()),
                        Entry::User("restored two".into()),
                    ],
                    model: None,
                    effort: None,
                    label: None,
                },
            )
            .unwrap();
        let (tx, _rx) = mpsc::channel();
        let mut restored = App::new(cwd, dir.path().join("wt"), store, tx);
        restored.restore();
        restored.mode = Mode::Normal;
        restored.handle_key(key(KeyCode::Char('k')));
        assert_eq!(restored.input.text(), "restored two");
        restored.handle_key(key(KeyCode::Char('k')));
        assert_eq!(restored.input.text(), "restored one");
        let _ = restored.close_all();
    }
}
