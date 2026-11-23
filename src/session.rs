//! One agent conversation: the transcript model plus the worker thread that
//! owns the ACP client. Never imports from `ui` or `app`.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::agent::acp::{
    AcpClient, AvailableCommand, CacheTtl, CancelHandle, CostInfo, EffortState, ElicitationRequest,
    ElicitationResponse, Event, ModelState, PermissionRequest, SessionModeState, StopReason,
    TokenUsage, ToolContent, UsageUpdate,
};
use crate::agent::{install, AgentSpec};
use crate::store::{self, Record};
use crate::worktree::{Released, State as WorktreeState, Worktree};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Entry {
    User(String),
    Agent(String),
    Thought(String),
    Tool {
        id: String,
        title: String,
        kind: String,
        status: String,
        #[serde(default)]
        detail: Vec<ToolContent>,
    },
    Notice(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Starting,
    Idle,
    Running,
    AwaitingPermission,
    AwaitingQuestion,
    Failed,
}

/// Computed warmth of the session's prompt cache at a point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    Warm {
        ttl: CacheTtl,
        remaining: Duration,
    },
    Expired {
        ttl: CacheTtl,
        by_model_switch: bool,
    },
}

/// Process-local prompt-cache TTL tracker (populated for Claude sessions via
/// `_claude/sdkMessage`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheState {
    pub ttl: Option<CacheTtl>,
    pub warmed_at: Option<Instant>,
    pub invalidated_by_model: bool,
}

impl CacheState {
    pub fn record(&mut self, ttl: Option<CacheTtl>, now: Instant) {
        if let Some(t) = ttl {
            self.ttl = Some(t);
        } else if self.ttl.is_none() {
            self.ttl = Some(CacheTtl::FiveMinutes);
        }
        self.warmed_at = Some(now);
        self.invalidated_by_model = false;
    }

    pub fn invalidate_model_switch(&mut self) {
        if self.ttl.is_some() || self.warmed_at.is_some() {
            self.warmed_at = None;
            self.invalidated_by_model = true;
        }
    }

    pub fn status(&self) -> Option<CacheStatus> {
        self.status_at(Instant::now())
    }

    pub fn status_at(&self, now: Instant) -> Option<CacheStatus> {
        let ttl = self.ttl?;
        if self.invalidated_by_model {
            return Some(CacheStatus::Expired {
                ttl,
                by_model_switch: true,
            });
        }
        let warmed_at = self.warmed_at?;
        let elapsed = now.saturating_duration_since(warmed_at);
        let limit = ttl.duration();
        match limit.checked_sub(elapsed) {
            Some(remaining) if !remaining.is_zero() => Some(CacheStatus::Warm { ttl, remaining }),
            _ => Some(CacheStatus::Expired {
                ttl,
                by_model_switch: false,
            }),
        }
    }
}

/// Aggregated live context-window and token usage for one agent session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Current context window occupancy in tokens (from ACP `usage_update`).
    pub context_used: Option<u64>,
    /// Context window capacity in tokens (from ACP `usage_update`).
    pub context_size: Option<u64>,
    /// Cumulative session cost (from ACP `usage_update.cost`).
    pub cost: Option<CostInfo>,
    /// Token breakdown from the latest `session/prompt` response (`usage`).
    pub tokens: Option<TokenUsage>,
    /// Prompt-cache TTL / expiration state (live, process-local).
    #[serde(skip)]
    pub cache: CacheState,
}

impl SessionUsage {
    pub fn apply_update(&mut self, update: UsageUpdate) {
        self.context_used = Some(update.used);
        if update.size > 0 {
            self.context_size = Some(update.size);
        }
        if update.cost.is_some() {
            self.cost = update.cost;
        }
    }

    pub fn apply_tokens(&mut self, tokens: TokenUsage) {
        self.tokens = Some(tokens);
    }

    pub fn record_cache(&mut self, ttl: Option<CacheTtl>) {
        self.cache.record(ttl, Instant::now());
    }

    pub fn invalidate_cache_for_model_switch(&mut self) {
        self.cache.invalidate_model_switch();
    }

    /// Context occupancy percentage (`0.0..=100.0+`) when both `used` and a
    /// non-zero `size` have been reported.
    pub fn context_pct(&self) -> Option<f64> {
        let used = self.context_used?;
        let size = self.context_size.filter(|&s| s > 0)?;
        Some((used as f64 / size as f64) * 100.0)
    }

    pub fn has_data(&self) -> bool {
        self.context_used.is_some()
            || self.context_size.is_some()
            || self.cost.is_some()
            || self.tokens.is_some()
            || self.cache.ttl.is_some()
    }
}

/// Worker -> UI messages.
#[derive(Debug)]
pub enum UiMsg {
    Ready(
        CancelHandle,
        ModelState,
        EffortState,
        SessionModeState,
        Vec<AvailableCommand>,
        Option<UsageUpdate>,
    ),
    /// `session/set_model` failed; the model is unchanged.
    ModelFailed(String),
    /// Setting the effort level failed; the effort is unchanged.
    EffortFailed(String),
    /// Setting the session mode failed; the mode is unchanged.
    ModeFailed(String),
    Event(Event),
    Permission(PermissionRequest),
    Question(ElicitationRequest),
    TurnDone(StopReason),
    Error(String),
    /// A closed session finished shutting down (agent stopped, worktree
    /// released). Delivered after the session is gone from the UI.
    Closed(String),
}

#[derive(Debug)]
pub struct Msg {
    pub session: usize,
    pub body: UiMsg,
}

enum WorkerCmd {
    Prompt(String),
    SetModel(String),
    SetEffort(String),
    SetMode(String),
    Quit,
}

pub struct Session {
    pub id: usize,
    /// Key of this session's on-disk record (see `store`).
    pub key: String,
    pub spec: &'static AgentSpec,
    /// Optional user-assigned label for distinguishing parallel sessions.
    pub label: Option<String>,
    pub entries: Vec<Entry>,
    pub status: Status,
    /// Set when a turn completes while this session is in the background;
    /// cleared once the user switches to it.
    pub attention: bool,
    pub pending: Option<PermissionRequest>,
    pub pending_question: Option<ElicitationRequest>,
    pub worktree: Option<Worktree>,
    /// Cached git dirty/ahead counts for `worktree`.
    pub worktree_state: Option<WorktreeState>,
    /// When the current turn started; `None` while not running.
    pub turn_started: Option<Instant>,
    /// Models the agent advertised; empty until it is ready.
    pub models: ModelState,
    /// Model to apply once the agent is up (restored from the record).
    preferred_model: Option<String>,
    /// Effort levels the agent advertised for the current model; empty if unsupported.
    pub effort: EffortState,
    /// Effort level to apply once the agent is up (restored from the record).
    preferred_effort: Option<String>,
    /// Execution/permission modes the agent advertised; empty if unsupported.
    pub modes: SessionModeState,
    /// Session mode to apply once the agent is up (from `--mode` or restored record).
    preferred_mode: Option<String>,
    /// Slash commands advertised by the agent (`available_commands_update`).
    pub commands: Vec<AvailableCommand>,
    /// Live context window, cost, and token usage reported by the agent.
    pub usage: SessionUsage,
    cancel: Option<CancelHandle>,
    cmd_tx: Sender<WorkerCmd>,
    perm_tx: Sender<Option<String>>,
    question_tx: Sender<ElicitationResponse>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Start a session; the agent is installed (if needed) and launched on a
    /// worker thread, reporting back through `ui_tx`. The agent runs in the
    /// worktree if there is one, else in `cwd`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: usize,
        spec: &'static AgentSpec,
        cwd: PathBuf,
        worktree: Option<Worktree>,
        model: Option<String>,
        effort: Option<String>,
        mode: Option<String>,
        ui_tx: Sender<Msg>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (perm_tx, perm_rx) = mpsc::channel();
        let (question_tx, question_rx) = mpsc::channel();
        let dir = worktree.as_ref().map_or(cwd, |w| w.path.clone());
        let preferred_model = model.clone();
        let preferred_effort = effort.clone();
        let preferred_mode = mode.clone();
        let handle = thread::spawn(move || {
            worker(
                id,
                spec,
                dir,
                preferred_model,
                preferred_effort,
                preferred_mode,
                ui_tx,
                cmd_rx,
                perm_rx,
                question_rx,
            )
        });
        let mut s = Self::new(id, spec, cmd_tx, perm_tx, question_tx);
        s.worktree = worktree;
        s.refresh_worktree_state();
        s.preferred_model = model;
        s.preferred_effort = effort;
        s.preferred_mode = mode;
        s.thread = Some(handle);
        s
    }

    fn new(
        id: usize,
        spec: &'static AgentSpec,
        cmd_tx: Sender<WorkerCmd>,
        perm_tx: Sender<Option<String>>,
        question_tx: Sender<ElicitationResponse>,
    ) -> Self {
        Self {
            id,
            key: store::new_key(spec.id),
            spec,
            label: None,
            entries: Vec::new(),
            status: Status::Starting,
            attention: false,
            pending: None,
            pending_question: None,
            worktree: None,
            worktree_state: None,
            turn_started: None,
            models: ModelState::default(),
            preferred_model: None,
            effort: EffortState::default(),
            preferred_effort: None,
            modes: SessionModeState::default(),
            preferred_mode: None,
            commands: Vec::new(),
            usage: SessionUsage::default(),
            cancel: None,
            cmd_tx,
            perm_tx,
            question_tx,
            thread: None,
        }
    }

    /// A session with no worker, for tests.
    #[cfg(test)]
    pub fn detached(id: usize, spec: &'static AgentSpec) -> (Self, Receiver<Option<String>>) {
        let (s, perm_rx, _question_rx) = Self::detached_with_questions(id, spec);
        (s, perm_rx)
    }

    /// A session with no worker exposing both permission and question channels, for tests.
    #[cfg(test)]
    pub fn detached_with_questions(
        id: usize,
        spec: &'static AgentSpec,
    ) -> (
        Self,
        Receiver<Option<String>>,
        Receiver<ElicitationResponse>,
    ) {
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (perm_tx, perm_rx) = mpsc::channel();
        let (question_tx, question_rx) = mpsc::channel();
        (
            Self::new(id, spec, cmd_tx, perm_tx, question_tx),
            perm_rx,
            question_rx,
        )
    }

    /// Directory this session operates in (its worktree if isolated, else `fallback`).
    pub fn dir<'a>(&'a self, fallback: &'a std::path::Path) -> &'a std::path::Path {
        self.worktree
            .as_ref()
            .map_or(fallback, |w| w.path.as_path())
    }

    /// Human-friendly title: `<label> · <agent>` when renamed, else `<agent>`.
    pub fn display_title(&self) -> String {
        match &self.label {
            Some(label) => format!("{label} · {}", self.spec.name),
            None => self.spec.name.to_string(),
        }
    }

    /// Refresh cached dirty/ahead counts from the session's git worktree.
    pub fn refresh_worktree_state(&mut self) {
        self.worktree_state = self.worktree.as_ref().and_then(|w| w.state().ok());
    }

    /// What to persist for this session.
    pub fn record(&self, cwd: &std::path::Path) -> Record {
        Record {
            agent: self.spec.id.to_string(),
            cwd: cwd.to_path_buf(),
            worktree: self.worktree.clone(),
            entries: self.entries.clone(),
            model: self
                .models
                .current
                .clone()
                .or_else(|| self.preferred_model.clone()),
            effort: self
                .effort
                .current
                .clone()
                .or_else(|| self.preferred_effort.clone()),
            mode: self
                .modes
                .current
                .clone()
                .or_else(|| self.preferred_mode.clone()),
            label: self.label.clone(),
        }
    }

    pub fn can_prompt(&self) -> bool {
        self.status == Status::Idle
    }

    /// Id of the model in use, if the agent reported one.
    pub fn model(&self) -> Option<&str> {
        self.models.current.as_deref()
    }

    /// Human name of the model in use.
    pub fn model_name(&self) -> Option<String> {
        self.model().map(|id| self.models.display(id))
    }

    /// Ask the agent to switch models (only between turns).
    pub fn set_model(&mut self, id: String) {
        if self.can_prompt() && self.models.available.iter().any(|m| m.id == id) {
            let _ = self.cmd_tx.send(WorkerCmd::SetModel(id));
        }
    }

    fn note_model(&mut self, id: String) {
        if self.models.current.as_deref() != Some(&id) {
            let name = self.models.display(&id);
            self.entries.push(Entry::Notice(format!("model: {name}")));
            self.models.current = Some(id);
            self.usage.invalidate_cache_for_model_switch();
        }
    }

    /// Id of the effort level in use, if the agent reported one.
    pub fn effort(&self) -> Option<&str> {
        self.effort.current.as_deref()
    }

    /// Human name of the effort level in use.
    pub fn effort_name(&self) -> Option<String> {
        self.effort().map(|id| self.effort.display(id))
    }

    /// Ask the agent to switch effort levels (only between turns).
    pub fn set_effort(&mut self, id: String) {
        if self.can_prompt() && self.effort.available.iter().any(|e| e.id == id) {
            let _ = self.cmd_tx.send(WorkerCmd::SetEffort(id));
        }
    }

    fn note_effort(&mut self, id: String) {
        if self.effort.current.as_deref() != Some(&id) {
            let name = self.effort.display(&id);
            self.entries.push(Entry::Notice(format!("effort: {name}")));
            self.effort.current = Some(id);
        }
    }

    /// Id of the session mode in use, if the agent reported one.
    pub fn session_mode(&self) -> Option<&str> {
        self.modes.current.as_deref()
    }

    /// Human name of the session mode in use.
    pub fn session_mode_name(&self) -> Option<String> {
        self.session_mode().map(|id| self.modes.display(id))
    }

    /// Ask the agent to switch session modes (only between turns).
    pub fn set_session_mode(&mut self, id: String) {
        if self.can_prompt() && self.modes.available.iter().any(|m| m.id == id) {
            let _ = self.cmd_tx.send(WorkerCmd::SetMode(id));
        }
    }

    fn note_mode(&mut self, id: String) {
        if self.modes.current.as_deref() != Some(&id) {
            let name = self.modes.display(&id);
            self.entries.push(Entry::Notice(format!("mode: {name}")));
            self.modes.current = Some(id);
        }
    }

    pub fn send_prompt(&mut self, text: String) {
        if !self.can_prompt() {
            return;
        }
        self.entries.push(Entry::User(text.clone()));
        self.status = Status::Running;
        self.attention = false;
        self.turn_started = Some(Instant::now());
        let _ = self.cmd_tx.send(WorkerCmd::Prompt(text));
    }

    /// Answer a pending permission request (`None` cancels the turn).
    pub fn answer_permission(&mut self, option_id: Option<String>) {
        if self.pending.take().is_some() {
            self.status = Status::Running;
            let _ = self.perm_tx.send(option_id);
        }
    }

    /// Answer a pending interactive question (`elicitation/create`).
    pub fn answer_question(&mut self, response: ElicitationResponse) {
        if self.pending_question.take().is_some() {
            self.status = Status::Running;
            let _ = self.question_tx.send(response);
        }
    }

    /// Interrupt the running turn.
    pub fn cancel(&mut self) {
        if !matches!(
            self.status,
            Status::Running | Status::AwaitingPermission | Status::AwaitingQuestion
        ) {
            return;
        }
        // Any pending permission or question must be answered cancelled first.
        self.answer_permission(None);
        self.answer_question(ElicitationResponse::Cancel);
        if let Some(h) = &self.cancel {
            let _ = h.cancel();
        }
    }

    /// Stop the agent, then release the worktree (on a background thread, so
    /// the UI never waits). Reports back with `UiMsg::Closed`.
    pub fn close(mut self, ui_tx: Sender<Msg>) {
        self.cancel();
        let _ = self.cmd_tx.send(WorkerCmd::Quit);
        let id = self.id;
        let thread = self.thread.take();
        let worktree = self.worktree.take();
        let name = self.display_title();
        thread::spawn(move || {
            if let Some(t) = thread {
                let _ = t.join();
            }
            let msg = match worktree.map(|w| w.release()) {
                None => format!("{name} session closed"),
                Some(Ok(Released::Removed)) => format!("{name} session closed (no changes)"),
                Some(Ok(Released::Kept { path, branch })) => {
                    format!("{name} session closed; kept {branch} at {}", path.display())
                }
                Some(Err(e)) => format!("{name} session closed; worktree cleanup failed: {e}"),
            };
            let _ = ui_tx.send(Msg {
                session: id,
                body: UiMsg::Closed(msg),
            });
        });
    }

    pub fn apply(&mut self, body: UiMsg) {
        match body {
            UiMsg::Closed(_) => {}
            UiMsg::Ready(handle, models, effort, modes, commands, usage) => {
                self.cancel = Some(handle);
                self.status = Status::Idle;
                self.models = models;
                self.preferred_model = None;
                self.effort = effort;
                self.preferred_effort = None;
                self.modes = modes;
                self.preferred_mode = None;
                self.commands = commands;
                if let Some(u) = usage {
                    self.usage.apply_update(u);
                }
            }
            UiMsg::ModelFailed(e) => self
                .entries
                .push(Entry::Notice(format!("could not change model: {e}"))),
            UiMsg::EffortFailed(e) => self
                .entries
                .push(Entry::Notice(format!("could not change effort: {e}"))),
            UiMsg::ModeFailed(e) => self
                .entries
                .push(Entry::Notice(format!("could not change mode: {e}"))),
            UiMsg::Event(e) => self.apply_event(e),
            UiMsg::Permission(req) => {
                self.pending = Some(req);
                self.status = Status::AwaitingPermission;
            }
            UiMsg::Question(req) => {
                self.pending_question = Some(req);
                self.status = Status::AwaitingQuestion;
            }
            UiMsg::TurnDone(stop) => {
                self.status = Status::Idle;
                self.turn_started = None;
                self.pending = None;
                self.pending_question = None;
                if stop != StopReason::EndTurn {
                    self.entries
                        .push(Entry::Notice(format!("stopped: {stop:?}")));
                }
            }
            UiMsg::Error(e) => {
                self.status = Status::Failed;
                self.turn_started = None;
                self.pending = None;
                self.pending_question = None;
                self.entries.push(Entry::Notice(format!("error: {e}")));
            }
        }
    }

    fn apply_event(&mut self, e: Event) {
        match e {
            Event::AgentText(t) => match self.entries.last_mut() {
                Some(Entry::Agent(s)) => s.push_str(&t),
                _ => self.entries.push(Entry::Agent(t)),
            },
            Event::AgentThought(t) => match self.entries.last_mut() {
                Some(Entry::Thought(s)) => s.push_str(&t),
                _ => self.entries.push(Entry::Thought(t)),
            },
            Event::ToolCall {
                id,
                title,
                kind,
                status,
                content,
            } => self.entries.push(Entry::Tool {
                id,
                title,
                kind,
                status,
                detail: content,
            }),
            Event::ToolUpdate {
                id,
                title,
                status,
                content,
            } => {
                if status.is_some() || title.is_some() || !content.is_empty() {
                    for entry in self.entries.iter_mut().rev() {
                        if let Entry::Tool {
                            id: tid,
                            title: cur_title,
                            status: cur_status,
                            detail,
                            ..
                        } = entry
                        {
                            if *tid == id {
                                if let Some(new_status) = status {
                                    *cur_status = new_status;
                                }
                                if let Some(new_title) = title {
                                    if !new_title.is_empty() {
                                        *cur_title = new_title;
                                    }
                                }
                                if !content.is_empty() {
                                    *detail = content;
                                }
                                break;
                            }
                        }
                    }
                }
            }
            Event::Plan(items) => {
                let list = items
                    .iter()
                    .map(|i| format!("- {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.entries.push(Entry::Notice(format!("plan:\n{list}")));
            }
            Event::ModelChanged(id) => self.note_model(id),
            Event::EffortChanged(id) => self.note_effort(id),
            Event::ModeChanged(id) => self.note_mode(id),
            Event::ConfigOptionsUpdated {
                models,
                effort,
                modes,
            } => {
                let model_updated = models.is_some();
                if let Some(m) = models {
                    let prev = self.models.current.clone();
                    let next = m.current.clone();
                    self.models = m;
                    if let Some(id) = next {
                        if prev.as_deref() != Some(id.as_str()) {
                            let name = self.models.display(&id);
                            self.entries.push(Entry::Notice(format!("model: {name}")));
                            self.usage.invalidate_cache_for_model_switch();
                        }
                    }
                }
                let prev_effort = self.effort.current.clone();
                let next_effort = effort.unwrap_or_default();
                let next_effort_id = next_effort.current.clone();
                self.effort = next_effort;
                if let Some(id) = next_effort_id {
                    if prev_effort.as_deref() != Some(id.as_str())
                        && (!model_updated || prev_effort.is_some() || id != "default")
                    {
                        let name = self.effort.display(&id);
                        self.entries.push(Entry::Notice(format!("effort: {name}")));
                    }
                }
                if let Some(m) = modes {
                    let prev_mode = self.modes.current.clone();
                    let next_mode_id = m.current.clone();
                    self.modes = m;
                    if let Some(id) = next_mode_id {
                        if prev_mode.as_deref() != Some(id.as_str()) {
                            let name = self.modes.display(&id);
                            self.entries.push(Entry::Notice(format!("mode: {name}")));
                        }
                    }
                } else if self.modes.config_id.is_some() {
                    self.modes = SessionModeState::default();
                }
            }
            Event::AvailableCommands(cmds) => {
                self.commands = cmds;
            }
            Event::UsageUpdated(u) => {
                self.usage.apply_update(u);
            }
            Event::TurnUsage(t) => {
                self.usage.apply_tokens(t);
            }
            Event::CacheActivity { ttl } => {
                self.usage.record_cache(ttl);
            }
            Event::Other(_) => {}
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCmd::Quit);
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    id: usize,
    spec: &'static AgentSpec,
    cwd: PathBuf,
    preferred_model: Option<String>,
    preferred_effort: Option<String>,
    preferred_mode: Option<String>,
    ui_tx: Sender<Msg>,
    cmd_rx: Receiver<WorkerCmd>,
    perm_rx: Receiver<Option<String>>,
    question_rx: Receiver<ElicitationResponse>,
) {
    let send = |body: UiMsg| {
        let _ = ui_tx.send(Msg { session: id, body });
    };
    let run = || -> Result<(), String> {
        let program = install::ensure(spec).map_err(|e| e.to_string())?;
        let mut client = AcpClient::spawn(&program.to_string_lossy(), spec.args, &cwd)
            .map_err(|e| e.to_string())?;
        client.initialize().map_err(|e| e.to_string())?;
        let session_meta = (spec.id == "claude").then(|| {
            serde_json::json!({
                "claudeCode": {
                    "emitRawSDKMessages": [
                        { "type": "assistant" },
                        { "type": "result" },
                    ],
                },
            })
        });
        let sid = client
            .new_session_with_meta(&cwd, session_meta)
            .map_err(|e| e.to_string())?;
        // Re-apply a restored model first (changing models may update available
        // effort levels), then re-apply any restored effort level and mode.
        if let Some(want) = preferred_model {
            let m = client.models();
            if m.current.as_deref() != Some(&want) && m.available.iter().any(|a| a.id == want) {
                let _ = client.set_model(&sid, &want);
            }
        }
        if let Some(want) = preferred_effort {
            let e = client.effort();
            if e.current.as_deref() != Some(&want) && e.available.iter().any(|a| a.id == want) {
                let _ = client.set_effort(&sid, &want);
            }
        }
        if let Some(want) = preferred_mode {
            let m = client.modes();
            if let Some(chosen) = m
                .available
                .iter()
                .find(|a| a.id.eq_ignore_ascii_case(&want) || a.name.eq_ignore_ascii_case(&want))
                .map(|a| a.id.clone())
            {
                if m.current.as_deref() != Some(chosen.as_str()) {
                    let _ = client.set_mode(&sid, &chosen);
                }
            }
        }
        send(UiMsg::Ready(
            client.cancel_handle(&sid),
            client.models().clone(),
            client.effort().clone(),
            client.modes().clone(),
            client.commands().to_vec(),
            client.usage().cloned(),
        ));
        loop {
            let _ = client.poll_updates(|e| send(UiMsg::Event(e)));
            let cmd = match cmd_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let text = match cmd {
                WorkerCmd::Prompt(text) => text,
                WorkerCmd::SetModel(id) => {
                    let prev_cmds = client.commands().to_vec();
                    let prev_usage = client.usage().cloned();
                    match client.set_model(&sid, &id) {
                        Ok(()) => {
                            let effort = (!client.effort().available.is_empty())
                                .then(|| client.effort().clone());
                            let modes = (!client.modes().available.is_empty()
                                && client.modes().config_id.is_some())
                            .then(|| client.modes().clone());
                            send(UiMsg::Event(Event::ConfigOptionsUpdated {
                                models: Some(client.models().clone()),
                                effort,
                                modes,
                            }));
                            if client.commands() != prev_cmds.as_slice() {
                                send(UiMsg::Event(Event::AvailableCommands(
                                    client.commands().to_vec(),
                                )));
                            }
                            if client.usage() != prev_usage.as_ref() {
                                if let Some(u) = client.usage().cloned() {
                                    send(UiMsg::Event(Event::UsageUpdated(u)));
                                }
                            }
                        }
                        Err(e) => send(UiMsg::ModelFailed(e.to_string())),
                    }
                    continue;
                }
                WorkerCmd::SetEffort(id) => {
                    let prev_cmds = client.commands().to_vec();
                    let prev_usage = client.usage().cloned();
                    match client.set_effort(&sid, &id) {
                        Ok(()) => {
                            if client.effort().available.is_empty() {
                                send(UiMsg::Event(Event::EffortChanged(id)));
                            } else {
                                let modes = (!client.modes().available.is_empty()
                                    && client.modes().config_id.is_some())
                                .then(|| client.modes().clone());
                                send(UiMsg::Event(Event::ConfigOptionsUpdated {
                                    models: None,
                                    effort: Some(client.effort().clone()),
                                    modes,
                                }));
                            }
                            if client.commands() != prev_cmds.as_slice() {
                                send(UiMsg::Event(Event::AvailableCommands(
                                    client.commands().to_vec(),
                                )));
                            }
                            if client.usage() != prev_usage.as_ref() {
                                if let Some(u) = client.usage().cloned() {
                                    send(UiMsg::Event(Event::UsageUpdated(u)));
                                }
                            }
                        }
                        Err(e) => send(UiMsg::EffortFailed(e.to_string())),
                    }
                    continue;
                }
                WorkerCmd::SetMode(id) => {
                    let prev_cmds = client.commands().to_vec();
                    let prev_usage = client.usage().cloned();
                    match client.set_mode(&sid, &id) {
                        Ok(()) => {
                            if client.modes().config_id.is_some()
                                && !client.modes().available.is_empty()
                            {
                                let effort = (!client.effort().available.is_empty())
                                    .then(|| client.effort().clone());
                                send(UiMsg::Event(Event::ConfigOptionsUpdated {
                                    models: None,
                                    effort,
                                    modes: Some(client.modes().clone()),
                                }));
                            } else {
                                send(UiMsg::Event(Event::ModeChanged(id)));
                            }
                            if client.commands() != prev_cmds.as_slice() {
                                send(UiMsg::Event(Event::AvailableCommands(
                                    client.commands().to_vec(),
                                )));
                            }
                            if client.usage() != prev_usage.as_ref() {
                                if let Some(u) = client.usage().cloned() {
                                    send(UiMsg::Event(Event::UsageUpdated(u)));
                                }
                            }
                        }
                        Err(e) => send(UiMsg::ModeFailed(e.to_string())),
                    }
                    continue;
                }
                WorkerCmd::Quit => break,
            };
            let blocks =
                crate::files::expand_mentions(&text, &cwd, client.supports_embedded_context());
            let stop = client
                .prompt_blocks(
                    &sid,
                    &blocks,
                    |e| send(UiMsg::Event(e)),
                    |req| {
                        send(UiMsg::Permission(req.clone()));
                        perm_rx.recv().ok().flatten()
                    },
                    |req| {
                        send(UiMsg::Question(req.clone()));
                        question_rx.recv().unwrap_or(ElicitationResponse::Cancel)
                    },
                )
                .map_err(|e| e.to_string())?;
            send(UiMsg::TurnDone(stop));
        }
        Ok(())
    };
    if let Err(e) = run() {
        send(UiMsg::Error(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::acp::PermissionOption;
    use crate::agent::find;

    fn session() -> (Session, Receiver<Option<String>>) {
        Session::detached(0, find("claude").unwrap())
    }

    #[test]
    fn text_chunks_merge_and_tool_updates_apply() {
        let (mut s, _) = session();
        s.status = Status::Running;
        s.apply(UiMsg::Event(Event::AgentText("hel".into())));
        s.apply(UiMsg::Event(Event::AgentText("lo".into())));
        s.apply(UiMsg::Event(Event::ToolCall {
            id: "t1".into(),
            title: "ls".into(),
            kind: "execute".into(),
            status: "pending".into(),
            content: vec![ToolContent::Text("running...".into())],
        }));
        s.apply(UiMsg::Event(Event::ToolUpdate {
            id: "t1".into(),
            title: Some("ls -la".into()),
            status: None,
            content: vec![ToolContent::Text("Cargo.toml\nsrc".into())],
        }));
        // A status-only update must keep the existing detail intact.
        s.apply(UiMsg::Event(Event::ToolUpdate {
            id: "t1".into(),
            title: None,
            status: Some("completed".into()),
            content: vec![],
        }));
        assert_eq!(s.entries[0], Entry::Agent("hello".into()));
        assert_eq!(
            s.entries[1],
            Entry::Tool {
                id: "t1".into(),
                title: "ls -la".into(),
                kind: "execute".into(),
                status: "completed".into(),
                detail: vec![ToolContent::Text("Cargo.toml\nsrc".into())],
            }
        );
        s.apply(UiMsg::TurnDone(StopReason::EndTurn));
        assert_eq!(s.status, Status::Idle);
    }

    #[test]
    fn old_tool_entry_without_detail_deserializes() {
        let legacy = r#"{"Tool":{"id":"t1","title":"ls","kind":"execute","status":"completed"}}"#;
        let entry: Entry = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            entry,
            Entry::Tool {
                id: "t1".into(),
                title: "ls".into(),
                kind: "execute".into(),
                status: "completed".into(),
                detail: vec![],
            }
        );
    }

    #[test]
    fn permission_round_trip_and_cancel() {
        let (mut s, perm_rx) = session();
        s.apply(UiMsg::Permission(PermissionRequest {
            tool_call_id: "c".into(),
            options: vec![PermissionOption {
                id: "ok".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            }],
            ..Default::default()
        }));
        assert_eq!(s.status, Status::AwaitingPermission);
        s.answer_permission(Some("ok".into()));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("ok".into()));
        assert_eq!(s.status, Status::Running);

        s.apply(UiMsg::Permission(PermissionRequest {
            tool_call_id: "c2".into(),
            ..Default::default()
        }));
        s.cancel();
        assert_eq!(perm_rx.try_recv().unwrap(), None);
    }

    #[test]
    fn question_round_trip_and_cancel() {
        let (mut s, _, question_rx) = Session::detached_with_questions(0, find("claude").unwrap());
        s.apply(UiMsg::Question(ElicitationRequest {
            message: "Pick one".into(),
            fields: vec![],
        }));
        assert_eq!(s.status, Status::AwaitingQuestion);
        let mut content = serde_json::Map::new();
        content.insert("k".into(), serde_json::json!("v"));
        s.answer_question(ElicitationResponse::Accept(content.clone()));
        assert_eq!(
            question_rx.try_recv().unwrap(),
            ElicitationResponse::Accept(content)
        );
        assert_eq!(s.status, Status::Running);

        s.apply(UiMsg::Question(ElicitationRequest {
            message: "Another".into(),
            fields: vec![],
        }));
        s.cancel();
        assert_eq!(question_rx.try_recv().unwrap(), ElicitationResponse::Cancel);
    }

    #[test]
    fn model_changes_are_logged_once() {
        use crate::agent::acp::ModelInfo;
        let (mut s, _) = session();
        s.models = ModelState {
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
        s.apply(UiMsg::Event(Event::ModelChanged("a".into())));
        assert!(s.entries.is_empty());
        s.apply(UiMsg::Event(Event::ModelChanged("b".into())));
        assert_eq!(s.entries, vec![Entry::Notice("model: Beta".into())]);
        assert_eq!(s.model_name().as_deref(), Some("Beta"));
        assert_eq!(
            s.record(std::path::Path::new(".")).model.as_deref(),
            Some("b")
        );
    }

    #[test]
    fn effort_changes_and_config_sync_update_state_and_record() {
        use crate::agent::acp::{EffortInfo, ModelInfo};
        let (mut s, _) = session();
        s.models = ModelState {
            current: Some("opus".into()),
            available: vec![
                ModelInfo {
                    id: "opus".into(),
                    name: "Opus".into(),
                },
                ModelInfo {
                    id: "haiku".into(),
                    name: "Haiku".into(),
                },
            ],
            config_id: Some("model".into()),
        };
        s.effort = EffortState {
            current: Some("default".into()),
            available: vec![
                EffortInfo {
                    id: "default".into(),
                    name: "Default".into(),
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

        // Re-applying current effort is a no-op.
        s.apply(UiMsg::Event(Event::EffortChanged("default".into())));
        assert!(s.entries.is_empty());

        // Switching to high logs once and updates record.
        s.apply(UiMsg::Event(Event::EffortChanged("high".into())));
        assert_eq!(s.entries, vec![Entry::Notice("effort: High".into())]);
        assert_eq!(s.effort_name().as_deref(), Some("High"));
        assert_eq!(
            s.record(std::path::Path::new(".")).effort.as_deref(),
            Some("high")
        );

        // Switching to a model with no effort clears effort state and logs model.
        s.apply(UiMsg::Event(Event::ConfigOptionsUpdated {
            models: Some(ModelState {
                current: Some("haiku".into()),
                available: s.models.available.clone(),
                config_id: Some("model".into()),
            }),
            effort: None,
            modes: None,
        }));
        assert_eq!(s.model_name().as_deref(), Some("Haiku"));
        assert!(s.effort.available.is_empty());
        assert_eq!(s.effort(), None);
        assert_eq!(
            s.entries.last(),
            Some(&Entry::Notice("model: Haiku".into()))
        );

        // Failure notice is logged cleanly.
        s.apply(UiMsg::EffortFailed("rpc error".into()));
        assert_eq!(
            s.entries.last(),
            Some(&Entry::Notice("could not change effort: rpc error".into()))
        );
    }

    #[test]
    fn mode_updates_log_persist_and_preserve_legacy_modes() {
        use crate::agent::acp::SessionModeInfo;

        let (mut s, _) = session();
        s.modes = SessionModeState {
            current: Some("default".into()),
            available: vec![
                SessionModeInfo {
                    id: "default".into(),
                    name: "Default".into(),
                    description: None,
                },
                SessionModeInfo {
                    id: "plan".into(),
                    name: "Plan Mode".into(),
                    description: Some("Read-only planning".into()),
                },
                SessionModeInfo {
                    id: "bypassPermissions".into(),
                    name: "Auto".into(),
                    description: None,
                },
            ],
            config_id: None, // legacy modes
        };

        // Re-applying current mode is a no-op.
        s.apply(UiMsg::Event(Event::ModeChanged("default".into())));
        assert!(s.entries.is_empty());

        // Switching logs once and updates record.
        s.apply(UiMsg::Event(Event::ModeChanged("bypassPermissions".into())));
        assert_eq!(s.entries, vec![Entry::Notice("mode: Auto".into())]);
        assert_eq!(s.session_mode_name().as_deref(), Some("Auto"));
        assert_eq!(
            s.record(std::path::Path::new(".")).mode.as_deref(),
            Some("bypassPermissions")
        );

        // A config_option_update with `modes: None` does not clobber legacy modes.
        s.apply(UiMsg::Event(Event::ConfigOptionsUpdated {
            models: None,
            effort: None,
            modes: None,
        }));
        assert_eq!(s.session_mode(), Some("bypassPermissions"));

        // A config_option_update with `modes: Some(...)` updates mode state & logs.
        let mut cfg_modes = s.modes.clone();
        cfg_modes.config_id = Some("mode".into());
        cfg_modes.current = Some("plan".into());
        s.apply(UiMsg::Event(Event::ConfigOptionsUpdated {
            models: None,
            effort: None,
            modes: Some(cfg_modes),
        }));
        assert_eq!(s.session_mode_name().as_deref(), Some("Plan Mode"));
        assert_eq!(
            s.entries.last(),
            Some(&Entry::Notice("mode: Plan Mode".into()))
        );

        // Failure notice is logged cleanly.
        s.apply(UiMsg::ModeFailed("unsupported".into()));
        assert_eq!(
            s.entries.last(),
            Some(&Entry::Notice("could not change mode: unsupported".into()))
        );
    }

    #[test]
    fn prompt_only_when_idle() {
        let (mut s, _) = session();
        s.send_prompt("nope".into());
        assert!(s.entries.is_empty());
        s.status = Status::Idle;
        s.send_prompt("yes".into());
        assert_eq!(s.entries, vec![Entry::User("yes".into())]);
        assert_eq!(s.status, Status::Running);
    }

    #[test]
    fn display_title_and_record_include_custom_label() {
        let (mut s, _) = session();
        assert_eq!(s.display_title(), "Claude Code");
        assert_eq!(s.record(std::path::Path::new(".")).label, None);

        s.label = Some("fix-retry".into());
        assert_eq!(s.display_title(), "fix-retry · Claude Code");
        assert_eq!(
            s.record(std::path::Path::new(".")).label.as_deref(),
            Some("fix-retry")
        );
    }

    #[test]
    fn refresh_worktree_state_tracks_dirty_and_ahead_commits() {
        use std::process::Command;

        let run = |dir: &std::path::Path, args: &[&str]| {
            let st = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };

        let repo = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        run(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("a.txt"), "a").unwrap();
        run(repo.path(), &["add", "."]);
        run(repo.path(), &["commit", "-q", "-m", "init"]);

        let wt = Worktree::create(repo.path(), root.path(), "claude").unwrap();
        let (mut s, _) = session();
        s.worktree = Some(wt);
        s.refresh_worktree_state();
        assert_eq!(
            s.worktree_state,
            Some(WorktreeState {
                dirty: false,
                commits: 0
            })
        );

        let wt_path = s.worktree.as_ref().unwrap().path.clone();
        std::fs::write(wt_path.join("b.txt"), "b").unwrap();
        s.refresh_worktree_state();
        assert_eq!(
            s.worktree_state,
            Some(WorktreeState {
                dirty: true,
                commits: 0
            })
        );

        run(&wt_path, &["add", "."]);
        run(&wt_path, &["commit", "-q", "-m", "work"]);
        s.refresh_worktree_state();
        assert_eq!(
            s.worktree_state,
            Some(WorktreeState {
                dirty: false,
                commits: 1
            })
        );
    }

    #[test]
    fn available_commands_update_replaces_session_commands() {
        let (mut s, _) = session();
        assert!(s.commands.is_empty());
        let first = vec![
            AvailableCommand {
                name: "compact".into(),
                description: "Compact history".into(),
                input_hint: None,
            },
            AvailableCommand {
                name: "web".into(),
                description: "Search the web".into(),
                input_hint: Some("query".into()),
            },
        ];
        s.apply(UiMsg::Event(Event::AvailableCommands(first.clone())));
        assert_eq!(s.commands, first);

        // Replacement clears previous commands.
        s.apply(UiMsg::Event(Event::AvailableCommands(vec![])));
        assert!(s.commands.is_empty());
    }

    #[test]
    fn usage_events_update_context_preserve_cost_and_handle_compaction() {
        let (mut s, _) = session();
        assert!(!s.usage.has_data());
        assert_eq!(s.usage.context_pct(), None);

        // Turn 1 end-of-turn update includes cost.
        s.apply(UiMsg::Event(Event::UsageUpdated(UsageUpdate {
            used: 80_000,
            size: 200_000,
            cost: Some(CostInfo {
                amount: 0.12,
                currency: "USD".into(),
            }),
        })));
        assert!(s.usage.has_data());
        assert_eq!(s.usage.context_used, Some(80_000));
        assert_eq!(s.usage.context_size, Some(200_000));
        assert!((s.usage.context_pct().unwrap() - 40.0).abs() < 1e-6);
        assert_eq!(
            s.usage.cost,
            Some(CostInfo {
                amount: 0.12,
                currency: "USD".into(),
            })
        );

        // Mid-turn update on Turn 2 omits cost: previous cost is preserved.
        s.apply(UiMsg::Event(Event::UsageUpdated(UsageUpdate {
            used: 120_000,
            size: 200_000,
            cost: None,
        })));
        assert_eq!(s.usage.context_used, Some(120_000));
        assert!((s.usage.context_pct().unwrap() - 60.0).abs() < 1e-6);
        assert_eq!(
            s.usage.cost,
            Some(CostInfo {
                amount: 0.12,
                currency: "USD".into(),
            })
        );

        // Compaction drops used tokens while keeping window size and cost.
        s.apply(UiMsg::Event(Event::UsageUpdated(UsageUpdate {
            used: 15_000,
            size: 200_000,
            cost: None,
        })));
        assert_eq!(s.usage.context_used, Some(15_000));
        assert!((s.usage.context_pct().unwrap() - 7.5).abs() < 1e-6);
        assert!(s.usage.cost.is_some());

        // TurnUsage stores token breakdown.
        let tok = TokenUsage {
            total_tokens: 15_000,
            input_tokens: 12_000,
            output_tokens: 3_000,
            thought_tokens: Some(400),
            cached_read_tokens: Some(8_000),
            cached_write_tokens: Some(500),
        };
        s.apply(UiMsg::Event(Event::TurnUsage(tok.clone())));
        assert_eq!(s.usage.tokens, Some(tok));
    }

    #[test]
    fn cache_state_tracks_ttl_expiration_refresh_and_model_switch() {
        let mut cache = CacheState::default();
        assert_eq!(cache.status(), None);

        let t0 = Instant::now();
        // First turn writes a 1h cache.
        cache.record(Some(CacheTtl::OneHour), t0);
        assert_eq!(
            cache.status_at(t0 + Duration::from_secs(600)),
            Some(CacheStatus::Warm {
                ttl: CacheTtl::OneHour,
                remaining: Duration::from_secs(3000),
            })
        );

        // Pure cache read refreshes the timer and preserves the 1h tier.
        let t1 = t0 + Duration::from_secs(1200);
        cache.record(None, t1);
        assert_eq!(
            cache.status_at(t1 + Duration::from_secs(600)),
            Some(CacheStatus::Warm {
                ttl: CacheTtl::OneHour,
                remaining: Duration::from_secs(3000),
            })
        );

        // Passing the 1h mark transitions to Expired.
        assert_eq!(
            cache.status_at(t1 + Duration::from_secs(3600)),
            Some(CacheStatus::Expired {
                ttl: CacheTtl::OneHour,
                by_model_switch: false,
            })
        );

        // Downgrading to 5m tier (e.g. overage credits) updates TTL.
        let t2 = t1 + Duration::from_secs(4000);
        cache.record(Some(CacheTtl::FiveMinutes), t2);
        assert_eq!(
            cache.status_at(t2 + Duration::from_secs(300)),
            Some(CacheStatus::Expired {
                ttl: CacheTtl::FiveMinutes,
                by_model_switch: false,
            })
        );

        // Switching models invalidates the prompt cache immediately.
        let (mut s, _) = session();
        s.models.current = Some("sonnet".into());
        s.apply(UiMsg::Event(Event::CacheActivity {
            ttl: Some(CacheTtl::OneHour),
        }));
        assert!(matches!(
            s.usage.cache.status(),
            Some(CacheStatus::Warm { .. })
        ));
        s.apply(UiMsg::Event(Event::ModelChanged("opus".into())));
        assert_eq!(
            s.usage.cache.status(),
            Some(CacheStatus::Expired {
                ttl: CacheTtl::OneHour,
                by_model_switch: true,
            })
        );
    }
}
