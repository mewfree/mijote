//! Minimal Agent Client Protocol client (JSON-RPC 2.0, newline-delimited,
//! over a child process's stdio). Protocol: <https://agentclientprotocol.com>.
//!
//! A reader thread forwards every stdout line to a channel; the calling
//! thread drives requests and dispatches agent->client traffic while it
//! waits. We advertise no `fs`/`terminal` capabilities, so agents use
//! their own tools and only talk to us for updates and permissions.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: u64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("failed to spawn agent `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("io error talking to agent: {0}")]
    Io(#[from] std::io::Error),
    #[error("agent closed the connection")]
    Closed,
    #[error("agent returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("unexpected response: {0}")]
    Protocol(String),
}

/// A content block in an ACP `session/prompt` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptBlock {
    Text(String),
    ResourceLink {
        uri: String,
        name: String,
        mime_type: Option<String>,
    },
    Resource {
        uri: String,
        text: String,
        mime_type: Option<String>,
    },
}

impl PromptBlock {
    fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => json!({ "type": "text", "text": text }),
            Self::ResourceLink {
                uri,
                name,
                mime_type,
            } => {
                let mut obj = json!({
                    "type": "resource_link",
                    "uri": uri,
                    "name": name,
                });
                if let Some(m) = mime_type {
                    obj["mimeType"] = json!(m);
                }
                obj
            }
            Self::Resource {
                uri,
                text,
                mime_type,
            } => {
                let mut res = json!({
                    "uri": uri,
                    "text": text,
                });
                if let Some(m) = mime_type {
                    res["mimeType"] = json!(m);
                }
                json!({ "type": "resource", "resource": res })
            }
        }
    }
}

/// Content block attached to a `tool_call` or `tool_call_update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolContent {
    Text(String),
    Diff {
        path: String,
        old_text: Option<String>,
        new_text: String,
    },
}

/// A slash command advertised by the agent via `available_commands_update`
/// (or in the `session/new` response).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableCommand {
    /// Command name without a leading `/` (e.g. `"compact"`, `"review"`).
    pub name: String,
    pub description: String,
    /// Optional argument hint from `input.hint` (e.g. `"query to search for"`).
    pub input_hint: Option<String>,
}

/// Cumulative session cost reported inside an ACP `usage_update`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostInfo {
    pub amount: f64,
    pub currency: String,
}

impl CostInfo {
    fn parse(v: &Value) -> Option<Self> {
        let amount = v
            .get("amount")
            .and_then(Value::as_f64)
            .filter(|a| a.is_finite() && *a >= 0.0)?;
        let currency = v.get("currency").and_then(non_empty_str)?.to_string();
        Some(Self { amount, currency })
    }
}

/// Live context window occupancy and optional cumulative cost from an ACP
/// `usage_update` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageUpdate {
    pub used: u64,
    pub size: u64,
    #[serde(default)]
    pub cost: Option<CostInfo>,
}

impl UsageUpdate {
    fn parse(v: &Value) -> Option<Self> {
        let used = json_u64(v.get("used")?)?;
        let size = json_u64(v.get("size")?)?;
        let cost = v.get("cost").and_then(CostInfo::parse);
        Some(Self { used, size, cost })
    }
}

/// Token breakdown returned on an ACP `session/prompt` response (`usage`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub thought_tokens: Option<u64>,
    #[serde(default)]
    pub cached_read_tokens: Option<u64>,
    #[serde(default)]
    pub cached_write_tokens: Option<u64>,
}

impl TokenUsage {
    fn parse(v: &Value) -> Option<Self> {
        let input = v.get("inputTokens").and_then(json_u64);
        let output = v.get("outputTokens").and_then(json_u64);
        let total = v.get("totalTokens").and_then(json_u64);
        if input.is_none() && output.is_none() && total.is_none() {
            return None;
        }
        let input_tokens = input.unwrap_or(0);
        let output_tokens = output.unwrap_or(0);
        let total_tokens = total.unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
        Some(Self {
            total_tokens,
            input_tokens,
            output_tokens,
            thought_tokens: v.get("thoughtTokens").and_then(json_u64),
            cached_read_tokens: v.get("cachedReadTokens").and_then(json_u64),
            cached_write_tokens: v.get("cachedWriteTokens").and_then(json_u64),
        })
    }
}

/// Prompt-cache TTL tier reported by Claude's raw SDK usage payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTtl {
    FiveMinutes,
    OneHour,
}

impl CacheTtl {
    pub fn duration(self) -> Duration {
        match self {
            Self::FiveMinutes => Duration::from_secs(5 * 60),
            Self::OneHour => Duration::from_secs(60 * 60),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

/// Something the agent told us during a prompt turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    AgentText(String),
    AgentThought(String),
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
        content: Vec<ToolContent>,
    },
    ToolUpdate {
        id: String,
        title: Option<String>,
        status: Option<String>,
        content: Vec<ToolContent>,
    },
    Plan(Vec<String>),
    /// The agent switched models (e.g. legacy `current_model_update`).
    ModelChanged(String),
    /// The agent switched reasoning/effort level.
    EffortChanged(String),
    /// The agent switched session modes (e.g. legacy `current_mode_update`).
    ModeChanged(String),
    /// The agent pushed a refreshed `configOptions` snapshot (may update model,
    /// effort, or session mode).
    ConfigOptionsUpdated {
        models: Option<ModelState>,
        effort: Option<EffortState>,
        modes: Option<SessionModeState>,
    },
    /// The agent advertised or replaced its available slash commands.
    AvailableCommands(Vec<AvailableCommand>),
    /// Live context window occupancy and optional cost (`usage_update`).
    UsageUpdated(UsageUpdate),
    /// Token breakdown reported on `session/prompt` completion (`usage`).
    TurnUsage(TokenUsage),
    /// Main-conversation prompt cache read/write observed (via Claude's
    /// `_claude/sdkMessage` extension). `ttl` is `Some` when the response
    /// explicitly created a 5m or 1h cache entry, or `None` on a pure hit.
    CacheActivity {
        ttl: Option<CacheTtl>,
    },
    /// An update type we don't model yet (kept so nothing is silently lost).
    Other(String),
}

/// One selectable choice inside an ACP form elicitation field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Supported JSON Schema field shapes for ACP form elicitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    SingleSelect {
        options: Vec<QuestionOption>,
        default: Option<String>,
    },
    MultiSelect {
        options: Vec<QuestionOption>,
        default: Vec<String>,
    },
    Text {
        default: Option<String>,
    },
    Bool {
        default: Option<bool>,
    },
    Number {
        integer: bool,
        default: Option<String>,
    },
}

/// One field inside an ACP `elicitation/create` form request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionField {
    pub key: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub kind: FieldKind,
}

/// An ACP `elicitation/create` (form mode) request asking the user questions mid-turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElicitationRequest {
    pub message: String,
    pub fields: Vec<QuestionField>,
}

/// Client reply to an ACP `elicitation/create` request.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationResponse {
    Accept(serde_json::Map<String, Value>),
    Decline,
    Cancel,
}

impl ElicitationResponse {
    fn to_result_json(&self) -> Value {
        match self {
            Self::Accept(content) => json!({
                "action": "accept",
                "content": Value::Object(content.clone()),
            }),
            Self::Decline => json!({ "action": "decline" }),
            Self::Cancel => json!({ "action": "cancel" }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDiff {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PermissionRequest {
    pub tool_call_id: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub command: Option<String>,
    pub locations: Vec<String>,
    pub diffs: Vec<PermissionDiff>,
    pub preview_text: Option<String>,
    pub options: Vec<PermissionOption>,
}

impl PermissionRequest {
    /// Index of the option chosen by the `a` shortcut: first `allow_once`,
    /// else first `allow*`.
    pub fn allow_index(&self) -> Option<usize> {
        self.options
            .iter()
            .position(|o| o.kind == "allow_once")
            .or_else(|| {
                self.options
                    .iter()
                    .position(|o| o.kind.starts_with("allow"))
            })
    }

    /// Index of the option chosen by the `r` / `Esc` shortcut: first
    /// `reject_once`, else first `reject*`.
    pub fn reject_index(&self) -> Option<usize> {
        self.options
            .iter()
            .position(|o| o.kind == "reject_once")
            .or_else(|| {
                self.options
                    .iter()
                    .position(|o| o.kind.starts_with("reject"))
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
}

impl StopReason {
    fn parse(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "max_tokens" => Self::MaxTokens,
            "max_turn_requests" => Self::MaxTurnRequests,
            "refusal" => Self::Refusal,
            "cancelled" => Self::Cancelled,
            other => Self::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

/// Models an agent advertises for a session. Empty `available` means the
/// agent offers no model selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelState {
    pub current: Option<String>,
    pub available: Vec<ModelInfo>,
    /// Set when the model is a session config option (switched with
    /// `session/set_config_option`) rather than the older `models` field.
    pub config_id: Option<String>,
}

impl ModelState {
    fn parse(models: &Value) -> Self {
        Self {
            current: models
                .get("currentModelId")
                .and_then(Value::as_str)
                .map(str::to_string),
            config_id: None,
            available: models
                .get("availableModels")
                .and_then(Value::as_array)
                .map(|ms| {
                    ms.iter()
                        .map(|m| {
                            let id = str_of(m, "modelId");
                            let name = m
                                .get("name")
                                .and_then(Value::as_str)
                                .map_or_else(|| id.clone(), str::to_string);
                            ModelInfo { id, name }
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Pull the model select out of a `configOptions` array, if there is one.
    fn from_config_options(options: &Value) -> Option<Self> {
        let opt = options.as_array()?.iter().find(|o| {
            o.get("category").and_then(Value::as_str) == Some("model")
                || (o.get("category").is_none()
                    && o.get("id").and_then(Value::as_str) == Some("model"))
        })?;
        // Values are flat, or grouped as `{group, options: [...]}`.
        let mut available = Vec::new();
        for v in opt.get("options").and_then(Value::as_array)? {
            let group = v.get("options").and_then(Value::as_array);
            for m in group.map_or(std::slice::from_ref(v), Vec::as_slice) {
                let id = str_of(m, "value");
                let name = m
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or_else(|| id.clone(), str::to_string);
                available.push(ModelInfo { id, name });
            }
        }
        Some(Self {
            current: opt
                .get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_string),
            available,
            config_id: Some(str_of(opt, "id")),
        })
    }

    /// Display name for `id`, falling back to the id itself.
    pub fn display(&self, id: &str) -> String {
        self.available
            .iter()
            .find(|m| m.id == id)
            .map_or_else(|| id.to_string(), |m| m.name.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// Reasoning / effort levels an agent advertises for the current model via the
/// ACP `thought_level` config option. Empty `available` means the agent (or
/// current model) offers no effort selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffortState {
    pub current: Option<String>,
    pub available: Vec<EffortInfo>,
    pub config_id: Option<String>,
}

impl EffortState {
    /// Pull the reasoning/effort select out of a `configOptions` array, if
    /// there is one. Matches standard ACP `category: "thought_level"`, falling
    /// back to options named `thought_level`, `effort`, or `reasoning_effort`.
    fn from_config_options(options: &Value) -> Option<Self> {
        let arr = options.as_array()?;
        let opt = arr
            .iter()
            .find(|o| o.get("category").and_then(Value::as_str) == Some("thought_level"))
            .or_else(|| {
                arr.iter().find(|o| {
                    let cat = o.get("category").and_then(Value::as_str);
                    !matches!(cat, Some("model" | "mode"))
                        && matches!(
                            o.get("id").and_then(Value::as_str),
                            Some("thought_level" | "effort" | "reasoning_effort")
                        )
                })
            })?;
        let config_id = opt.get("id").and_then(non_empty_str)?.to_string();
        let mut available = Vec::new();
        for v in opt.get("options").and_then(Value::as_array)? {
            let group = v.get("options").and_then(Value::as_array);
            for item in group.map_or(std::slice::from_ref(v), Vec::as_slice) {
                let id = str_of(item, "value");
                if id.is_empty() {
                    continue;
                }
                let name = item
                    .get("name")
                    .and_then(non_empty_str)
                    .map_or_else(|| id.clone(), str::to_string);
                let description = item
                    .get("description")
                    .and_then(non_empty_str)
                    .map(str::to_string);
                available.push(EffortInfo {
                    id,
                    name,
                    description,
                });
            }
        }
        if available.is_empty() {
            return None;
        }
        Some(Self {
            current: opt
                .get("currentValue")
                .and_then(non_empty_str)
                .map(str::to_string),
            available,
            config_id: Some(config_id),
        })
    }

    /// Display name for `id`, falling back to the id itself.
    pub fn display(&self, id: &str) -> String {
        self.available
            .iter()
            .find(|e| e.id == id)
            .map_or_else(|| id.to_string(), |e| e.name.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// Execution / permission modes an agent advertises for a session (e.g.
/// `default`, `acceptEdits`, `plan`, `bypassPermissions`, `auto`). Empty
/// `available` means the agent offers no mode selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionModeState {
    pub current: Option<String>,
    pub available: Vec<SessionModeInfo>,
    /// Set when the mode is a session config option (switched with
    /// `session/set_config_option`) rather than the older `modes` field
    /// (switched with `session/set_mode`).
    pub config_id: Option<String>,
}

impl SessionModeState {
    fn parse(modes: &Value) -> Self {
        Self {
            current: modes
                .get("currentModeId")
                .and_then(non_empty_str)
                .map(str::to_string),
            config_id: None,
            available: modes
                .get("availableModes")
                .and_then(Value::as_array)
                .map(|ms| {
                    ms.iter()
                        .filter_map(|m| {
                            let id = m
                                .get("id")
                                .or_else(|| m.get("modeId"))
                                .and_then(non_empty_str)?
                                .to_string();
                            let name = m
                                .get("name")
                                .and_then(non_empty_str)
                                .map_or_else(|| id.clone(), str::to_string);
                            let description = m
                                .get("description")
                                .and_then(non_empty_str)
                                .map(str::to_string);
                            Some(SessionModeInfo {
                                id,
                                name,
                                description,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Pull the session mode select out of a `configOptions` array, if there is
    /// one. Matches standard ACP `category: "mode"`, falling back to an option
    /// with `id: "mode"`.
    fn from_config_options(options: &Value) -> Option<Self> {
        let arr = options.as_array()?;
        let opt = arr
            .iter()
            .find(|o| o.get("category").and_then(Value::as_str) == Some("mode"))
            .or_else(|| {
                arr.iter().find(|o| {
                    let cat = o.get("category").and_then(Value::as_str);
                    !matches!(cat, Some("model" | "thought_level"))
                        && o.get("id").and_then(Value::as_str) == Some("mode")
                })
            })?;
        let config_id = opt.get("id").and_then(non_empty_str)?.to_string();
        let mut available = Vec::new();
        for v in opt.get("options").and_then(Value::as_array)? {
            let group = v.get("options").and_then(Value::as_array);
            for item in group.map_or(std::slice::from_ref(v), Vec::as_slice) {
                let id = str_of(item, "value");
                if id.is_empty() {
                    continue;
                }
                let name = item
                    .get("name")
                    .and_then(non_empty_str)
                    .map_or_else(|| id.clone(), str::to_string);
                let description = item
                    .get("description")
                    .and_then(non_empty_str)
                    .map(str::to_string);
                available.push(SessionModeInfo {
                    id,
                    name,
                    description,
                });
            }
        }
        if available.is_empty() {
            return None;
        }
        Some(Self {
            current: opt
                .get("currentValue")
                .and_then(non_empty_str)
                .map(str::to_string),
            available,
            config_id: Some(config_id),
        })
    }

    /// Display name for `id`, falling back to the id itself.
    pub fn display(&self, id: &str) -> String {
        self.available
            .iter()
            .find(|m| m.id == id)
            .map_or_else(|| id.to_string(), |m| m.name.clone())
    }
}

/// Sends `session/cancel` from any thread, even while a prompt is running.
#[derive(Clone, Debug)]
pub struct CancelHandle {
    stdin: Arc<Mutex<ChildStdin>>,
    session_id: String,
}

impl CancelHandle {
    pub fn cancel(&self) -> Result<(), AcpError> {
        write_line(
            &self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {"sessionId": self.session_id},
            }),
        )
    }
}

fn write_line(stdin: &Mutex<ChildStdin>, msg: &Value) -> Result<(), AcpError> {
    let mut line = serde_json::to_string(msg).map_err(|e| AcpError::Protocol(e.to_string()))?;
    line.push('\n');
    let mut w = stdin.lock().map_err(|_| AcpError::Closed)?;
    w.write_all(line.as_bytes())?;
    w.flush()?;
    Ok(())
}

pub struct AcpClient {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    rx: Receiver<Value>,
    next_id: u64,
    models: ModelState,
    effort: EffortState,
    modes: SessionModeState,
    commands: Vec<AvailableCommand>,
    usage: Option<UsageUpdate>,
    embedded_context: bool,
}

impl AcpClient {
    pub fn spawn(bin: &str, args: &[&str], cwd: &Path) -> Result<Self, AcpError> {
        let mut child = Command::new(bin)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| AcpError::Spawn(bin.to_string(), e))?;
        let stdin = child.stdin.take().ok_or(AcpError::Closed)?;
        let stdout = child.stdout.take().ok_or(AcpError::Closed)?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            rx,
            next_id: 0,
            models: ModelState::default(),
            effort: EffortState::default(),
            modes: SessionModeState::default(),
            commands: Vec::new(),
            usage: None,
            embedded_context: false,
        })
    }

    fn send(&mut self, msg: &Value) -> Result<(), AcpError> {
        write_line(&self.stdin, msg)
    }

    /// A handle that can interrupt a running turn from another thread. The
    /// pending `prompt` then resolves with `StopReason::Cancelled`.
    pub fn cancel_handle(&self, session_id: &str) -> CancelHandle {
        CancelHandle {
            stdin: Arc::clone(&self.stdin),
            session_id: session_id.to_string(),
        }
    }

    fn handle_session_update(&mut self, update: &Value, on_event: &mut dyn FnMut(Event)) {
        let ev = parse_update(update);
        match &ev {
            Event::ModelChanged(id) => self.models.current = Some(id.clone()),
            Event::EffortChanged(id) => self.effort.current = Some(id.clone()),
            Event::ModeChanged(id) => self.modes.current = Some(id.clone()),
            Event::ConfigOptionsUpdated {
                models,
                effort,
                modes,
            } => {
                if let Some(m) = models {
                    self.models = m.clone();
                }
                self.effort = effort.clone().unwrap_or_default();
                if let Some(m) = modes {
                    self.modes = m.clone();
                } else if self.modes.config_id.is_some() {
                    self.modes = SessionModeState::default();
                }
            }
            Event::AvailableCommands(cmds) => {
                self.commands = cmds.clone();
            }
            Event::UsageUpdated(u) => {
                let prev_cost = self.usage.as_ref().and_then(|cur| cur.cost.clone());
                let prev_size = self.usage.as_ref().map_or(0, |cur| cur.size);
                self.usage = Some(UsageUpdate {
                    used: u.used,
                    size: if u.size > 0 { u.size } else { prev_size },
                    cost: u.cost.clone().or(prev_cost),
                });
            }
            _ => {}
        }
        on_event(ev);
    }

    /// Non-blocking drain of unsolicited agent notifications (such as
    /// `available_commands_update` or `config_option_update` emitted between turns).
    pub fn poll_updates(&mut self, mut on_event: impl FnMut(Event)) -> Result<(), AcpError> {
        while let Ok(msg) = self.rx.try_recv() {
            let method = msg.get("method").and_then(Value::as_str);
            match (method, msg.get("id")) {
                (Some("session/update"), None) => {
                    if let Some(update) = msg.pointer("/params/update") {
                        self.handle_session_update(update, &mut on_event);
                    }
                }
                (Some("_claude/sdkMessage"), None) => {
                    if let Some(ev) = parse_claude_sdk_cache(&msg["params"]) {
                        on_event(ev);
                    }
                }
                (Some(_), Some(rid)) => {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": rid,
                        "error": {"code": -32601, "message": "method not supported"}
                    });
                    self.send(&reply)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Send a request and block until its response, dispatching any
    /// agent->client traffic that arrives in the meantime.
    fn call(
        &mut self,
        method: &str,
        params: Value,
        on_event: &mut dyn FnMut(Event),
        on_permission: &mut dyn FnMut(&PermissionRequest) -> Option<String>,
        on_elicitation: &mut dyn FnMut(&ElicitationRequest) -> ElicitationResponse,
    ) -> Result<Value, AcpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let msg = self.rx.recv().map_err(|_| AcpError::Closed)?;
            let method = msg.get("method").and_then(Value::as_str);
            match (method, msg.get("id")) {
                // Response to one of our requests.
                (None, Some(rid)) if rid.as_u64() == Some(id) => {
                    if let Some(err) = msg.get("error") {
                        return Err(AcpError::Rpc {
                            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                            message: err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        });
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                // Notification.
                (Some("session/update"), None) => {
                    if let Some(update) = msg.pointer("/params/update") {
                        self.handle_session_update(update, on_event);
                    }
                }
                (Some("_claude/sdkMessage"), None) => {
                    if let Some(ev) = parse_claude_sdk_cache(&msg["params"]) {
                        on_event(ev);
                    }
                }
                // Agent->client request.
                (Some(m), Some(rid)) => {
                    let rid = rid.clone();
                    let reply = if m == "session/request_permission" {
                        let req = parse_permission(&msg["params"]);
                        let outcome = match on_permission(&req) {
                            Some(option_id) => {
                                json!({"outcome": "selected", "optionId": option_id})
                            }
                            None => json!({"outcome": "cancelled"}),
                        };
                        json!({"jsonrpc": "2.0", "id": rid, "result": {"outcome": outcome}})
                    } else if m == "elicitation/create" {
                        let resp = match parse_elicitation(&msg["params"]) {
                            Some(req) => on_elicitation(&req),
                            None => ElicitationResponse::Decline,
                        };
                        json!({"jsonrpc": "2.0", "id": rid, "result": resp.to_result_json()})
                    } else {
                        json!({"jsonrpc": "2.0", "id": rid,
                               "error": {"code": -32601, "message": "method not supported"}})
                    };
                    self.send(&reply)?;
                }
                _ => {}
            }
        }
    }

    fn call_quiet(&mut self, method: &str, params: Value) -> Result<Value, AcpError> {
        self.call(method, params, &mut |_| {}, &mut |_| None, &mut |_| {
            ElicitationResponse::Cancel
        })
    }

    /// Negotiate version and capabilities. Returns the agent's display name.
    pub fn initialize(&mut self) -> Result<String, AcpError> {
        let result = self.call_quiet(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "elicitation": {
                        "form": {},
                    },
                },
                "clientInfo": {
                    "name": "mijote",
                    "title": "mijote",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        self.embedded_context = result
            .pointer("/agentCapabilities/promptCapabilities/embeddedContext")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(result
            .pointer("/agentInfo/title")
            .or_else(|| result.pointer("/agentInfo/name"))
            .and_then(Value::as_str)
            .unwrap_or("agent")
            .to_string())
    }

    /// Whether the agent advertised `promptCapabilities.embeddedContext`.
    pub fn supports_embedded_context(&self) -> bool {
        self.embedded_context
    }

    pub fn new_session(&mut self, cwd: &Path) -> Result<String, AcpError> {
        self.new_session_with_meta(cwd, None)
    }

    pub fn new_session_with_meta(
        &mut self,
        cwd: &Path,
        meta: Option<Value>,
    ) -> Result<String, AcpError> {
        self.commands.clear();
        self.usage = None;
        let mut params = json!({"cwd": cwd.to_string_lossy(), "mcpServers": []});
        if let Some(m) = meta {
            params["_meta"] = m;
        }
        let result = self.call_quiet("session/new", params)?;
        self.models = ModelState::default();
        self.effort = EffortState::default();
        self.modes = SessionModeState::default();
        if let Some(opts) = result.get("configOptions") {
            if let Some(m) = ModelState::from_config_options(opts) {
                self.models = m;
            } else if let Some(models) = result.get("models") {
                self.models = ModelState::parse(models);
            }
            if let Some(e) = EffortState::from_config_options(opts) {
                self.effort = e;
            }
            if let Some(m) = SessionModeState::from_config_options(opts) {
                self.modes = m;
            } else if let Some(modes) = result.get("modes") {
                self.modes = SessionModeState::parse(modes);
            }
        } else {
            if let Some(models) = result.get("models") {
                self.models = ModelState::parse(models);
            }
            if let Some(modes) = result.get("modes") {
                self.modes = SessionModeState::parse(modes);
            }
        }
        if let Some(cmds) = result.get("availableCommands").filter(|v| v.is_array()) {
            self.commands = parse_available_commands(cmds);
        }
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AcpError::Protocol("session/new: missing sessionId".into()))
    }

    /// Models the agent advertised in `session/new` (empty if none).
    pub fn models(&self) -> &ModelState {
        &self.models
    }

    /// Effort levels the agent advertised for the current model (empty if none).
    pub fn effort(&self) -> &EffortState {
        &self.effort
    }

    /// Session modes the agent advertised in `session/new` (empty if none).
    pub fn modes(&self) -> &SessionModeState {
        &self.modes
    }

    /// Slash commands the agent advertised for the session (empty if none).
    pub fn commands(&self) -> &[AvailableCommand] {
        &self.commands
    }

    /// Latest context usage update reported by the agent, if any.
    pub fn usage(&self) -> Option<&UsageUpdate> {
        self.usage.as_ref()
    }

    fn sync_config_options(
        &mut self,
        opts: &Value,
        fallback_model: Option<&str>,
        fallback_effort: Option<&str>,
        fallback_mode: Option<&str>,
    ) {
        if let Some(m) = ModelState::from_config_options(opts) {
            self.models = m;
        } else if let Some(id) = fallback_model {
            self.models.current = Some(id.to_string());
        }
        if let Some(e) = EffortState::from_config_options(opts) {
            self.effort = e;
        } else if let Some(id) = fallback_effort {
            self.effort.current = Some(id.to_string());
        } else {
            self.effort = EffortState::default();
        }
        if let Some(m) = SessionModeState::from_config_options(opts) {
            self.modes = m;
        } else if let Some(id) = fallback_mode {
            self.modes.current = Some(id.to_string());
        } else if self.modes.config_id.is_some() {
            self.modes = SessionModeState::default();
        }
    }

    /// Switch the session's model. Agents that do not support it answer with
    /// an RPC error, which the caller reports.
    pub fn set_model(&mut self, session_id: &str, model_id: &str) -> Result<(), AcpError> {
        match self.models.config_id.clone() {
            Some(config_id) => {
                let result = self.call_quiet(
                    "session/set_config_option",
                    json!({"sessionId": session_id, "configId": config_id, "value": model_id}),
                )?;
                if let Some(opts) = result.get("configOptions") {
                    self.sync_config_options(opts, Some(model_id), None, None);
                } else {
                    self.models.current = Some(model_id.to_string());
                }
            }
            None => {
                self.call_quiet(
                    "session/set_model",
                    json!({"sessionId": session_id, "modelId": model_id}),
                )?;
                self.models.current = Some(model_id.to_string());
            }
        }
        Ok(())
    }

    /// Switch the session's reasoning/effort level via `session/set_config_option`.
    pub fn set_effort(&mut self, session_id: &str, effort_id: &str) -> Result<(), AcpError> {
        let Some(config_id) = self.effort.config_id.clone() else {
            return Err(AcpError::Protocol(
                "agent does not offer effort selection".into(),
            ));
        };
        let result = self.call_quiet(
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": config_id, "value": effort_id}),
        )?;
        if let Some(opts) = result.get("configOptions") {
            self.sync_config_options(opts, None, Some(effort_id), None);
        } else {
            self.effort.current = Some(effort_id.to_string());
        }
        Ok(())
    }

    /// Switch the session's execution/permission mode via
    /// `session/set_config_option` (when advertised as a config option) or
    /// legacy `session/set_mode`.
    pub fn set_mode(&mut self, session_id: &str, mode_id: &str) -> Result<(), AcpError> {
        match self.modes.config_id.clone() {
            Some(config_id) => {
                let result = self.call_quiet(
                    "session/set_config_option",
                    json!({"sessionId": session_id, "configId": config_id, "value": mode_id}),
                )?;
                if let Some(opts) = result.get("configOptions") {
                    self.sync_config_options(opts, None, None, Some(mode_id));
                } else {
                    self.modes.current = Some(mode_id.to_string());
                }
            }
            None => {
                self.call_quiet(
                    "session/set_mode",
                    json!({"sessionId": session_id, "modeId": mode_id}),
                )?;
                self.modes.current = Some(mode_id.to_string());
            }
        }
        Ok(())
    }

    /// Run one prompt turn to completion with a single text block.
    ///
    /// `on_permission` returns the chosen option id, or `None` to cancel.
    #[cfg(test)]
    pub fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        on_event: impl FnMut(Event),
        on_permission: impl FnMut(&PermissionRequest) -> Option<String>,
    ) -> Result<StopReason, AcpError> {
        self.prompt_blocks(
            session_id,
            &[PromptBlock::Text(text.to_string())],
            on_event,
            on_permission,
            |_| ElicitationResponse::Cancel,
        )
    }

    /// Run one prompt turn to completion with structured ACP content blocks.
    pub fn prompt_blocks(
        &mut self,
        session_id: &str,
        blocks: &[PromptBlock],
        mut on_event: impl FnMut(Event),
        mut on_permission: impl FnMut(&PermissionRequest) -> Option<String>,
        mut on_elicitation: impl FnMut(&ElicitationRequest) -> ElicitationResponse,
    ) -> Result<StopReason, AcpError> {
        let prompt: Vec<Value> = blocks.iter().map(PromptBlock::to_json).collect();
        let result = self.call(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": prompt,
            }),
            &mut on_event,
            &mut on_permission,
            &mut on_elicitation,
        )?;
        let reason = result
            .get("stopReason")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::Protocol("session/prompt: missing stopReason".into()))?;
        if let Some(usage) = result.get("usage").and_then(TokenUsage::parse) {
            on_event(Event::TurnUsage(usage));
        }
        Ok(StopReason::parse(reason))
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn parse_tool_content(v: Option<&Value>) -> Vec<ToolContent> {
    let Some(items) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "content" => {
                if let Some(inner) = item.get("content") {
                    if inner.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = inner.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                out.push(ToolContent::Text(text.to_string()));
                            }
                        }
                    }
                }
            }
            "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        out.push(ToolContent::Text(text.to_string()));
                    }
                }
            }
            "terminal" => {
                if let Some(text) = item
                    .get("output")
                    .or_else(|| item.get("text"))
                    .and_then(Value::as_str)
                {
                    if !text.is_empty() {
                        out.push(ToolContent::Text(text.to_string()));
                    }
                }
            }
            "diff" => {
                out.push(ToolContent::Diff {
                    path: str_of(item, "path"),
                    old_text: item
                        .get("oldText")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    new_text: str_of(item, "newText"),
                });
            }
            _ => {}
        }
    }
    out
}

fn parse_update(u: &Value) -> Event {
    let kind = str_of(u, "sessionUpdate");
    let text = || {
        u.pointer("/content/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match kind.as_str() {
        "agent_message_chunk" => Event::AgentText(text()),
        "agent_thought_chunk" => Event::AgentThought(text()),
        "tool_call" => Event::ToolCall {
            id: str_of(u, "toolCallId"),
            title: str_of(u, "title"),
            kind: u
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("other")
                .to_string(),
            status: u
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending")
                .to_string(),
            content: parse_tool_content(u.get("content")),
        },
        "tool_call_update" => Event::ToolUpdate {
            id: str_of(u, "toolCallId"),
            title: u.get("title").and_then(Value::as_str).map(str::to_string),
            status: u.get("status").and_then(Value::as_str).map(str::to_string),
            content: parse_tool_content(u.get("content")),
        },
        "plan" => Event::Plan(
            u.get("entries")
                .and_then(Value::as_array)
                .map(|es| es.iter().map(|e| str_of(e, "content")).collect())
                .unwrap_or_default(),
        ),
        "config_option_update" => match u.get("configOptions").filter(|v| v.is_array()) {
            Some(opts) => Event::ConfigOptionsUpdated {
                models: ModelState::from_config_options(opts),
                effort: EffortState::from_config_options(opts),
                modes: SessionModeState::from_config_options(opts),
            },
            None => Event::Other(kind),
        },
        "available_commands_update" => match u.get("availableCommands").filter(|v| v.is_array()) {
            Some(cmds) => Event::AvailableCommands(parse_available_commands(cmds)),
            None => Event::Other(kind),
        },
        "current_model_update" => Event::ModelChanged(str_of(u, "currentModelId")),
        "current_mode_update" => match u
            .get("currentModeId")
            .or_else(|| u.get("modeId"))
            .and_then(non_empty_str)
        {
            Some(id) => Event::ModeChanged(id.to_string()),
            None => Event::Other(kind),
        },
        "usage_update" => match UsageUpdate::parse(u) {
            Some(usage) => Event::UsageUpdated(usage),
            None => Event::Other(kind),
        },
        _ => Event::Other(kind),
    }
}

fn parse_available_commands(v: &Value) -> Vec<AvailableCommand> {
    let Some(items) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let Some(raw_name) = item.get("name").and_then(non_empty_str) else {
            continue;
        };
        let name = raw_name.trim_start_matches('/').trim();
        if name.is_empty() || !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        let input_hint = item
            .get("input")
            .and_then(|i| i.get("hint"))
            .and_then(non_empty_str)
            .map(str::to_string);
        out.push(AvailableCommand {
            name: name.to_string(),
            description,
            input_hint,
        });
    }
    out
}

fn non_empty_str(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn json_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0 && *f <= u64::MAX as f64)
            .map(|f| f.round() as u64)
    })
}

/// Extract main-conversation prompt-cache activity and TTL tier (`5m` vs `1h`)
/// from a `claude-agent-acp` `_claude/sdkMessage` extension notification.
/// Subagent messages (those carrying a non-null `parent_tool_use_id`) are
/// ignored so their separate 5m caches never skew the main session timer.
fn parse_claude_sdk_cache(params: &Value) -> Option<Event> {
    let msg = params.get("message")?;
    if msg
        .get("parent_tool_use_id")
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
    {
        return None;
    }
    let usage = match msg.get("type").and_then(Value::as_str)? {
        "assistant" => msg.pointer("/message/usage")?,
        "result" => msg.get("usage")?,
        _ => return None,
    };
    let eph_1h = usage
        .pointer("/cache_creation/ephemeral_1h_input_tokens")
        .and_then(json_u64)
        .unwrap_or(0);
    let eph_5m = usage
        .pointer("/cache_creation/ephemeral_5m_input_tokens")
        .and_then(json_u64)
        .unwrap_or(0);
    if eph_1h > 0 {
        return Some(Event::CacheActivity {
            ttl: Some(CacheTtl::OneHour),
        });
    }
    if eph_5m > 0 {
        return Some(Event::CacheActivity {
            ttl: Some(CacheTtl::FiveMinutes),
        });
    }
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(json_u64)
        .unwrap_or(0);
    let cache_write = usage
        .get("cache_creation_input_tokens")
        .and_then(json_u64)
        .unwrap_or(0);
    if cache_read > 0 || cache_write > 0 {
        return Some(Event::CacheActivity { ttl: None });
    }
    None
}

fn string_array(v: &Value) -> Option<Vec<&str>> {
    let arr = v.as_array()?;
    let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

fn extract_command(raw: &Value) -> Option<String> {
    if let Some(s) = non_empty_str(raw) {
        return Some(s.to_string());
    }
    let obj = raw.as_object()?;
    if let Some(cmd) = obj.get("command").or_else(|| obj.get("cmd")) {
        if let Some(s) = non_empty_str(cmd) {
            if let Some(args) = obj.get("args").and_then(string_array) {
                return Some(format!("{s} {}", args.join(" ")));
            }
            return Some(s.to_string());
        }
        if let Some(parts) = string_array(cmd) {
            return Some(parts.join(" "));
        }
    }
    if let Some(parts) = obj.get("args").and_then(string_array) {
        return Some(parts.join(" "));
    }
    for key in ["path", "file_path", "filePath"] {
        if let Some(s) = obj.get(key).and_then(non_empty_str) {
            return Some(s.to_string());
        }
    }
    None
}

fn format_raw_input(raw: &Value) -> Option<String> {
    match raw {
        Value::Null => None,
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Object(map) if !map.is_empty() => {
            let mut lines = Vec::new();
            for (k, v) in map {
                let val = match v {
                    Value::Null => continue,
                    Value::String(s) if s.trim().is_empty() => continue,
                    Value::String(s) => s.trim().to_string(),
                    other => other.to_string(),
                };
                lines.push(format!("{k}: {val}"));
            }
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        Value::Array(arr) if !arr.is_empty() => Some(raw.to_string()),
        Value::Bool(_) | Value::Number(_) => Some(raw.to_string()),
        _ => None,
    }
}

fn parse_permission(p: &Value) -> PermissionRequest {
    let tc = p.get("toolCall");
    let command = tc.and_then(|t| t.get("rawInput")).and_then(extract_command);
    let locations = tc
        .and_then(|t| t.get("locations"))
        .and_then(Value::as_array)
        .map(|locs| {
            locs.iter()
                .filter_map(|loc| {
                    if let Some(s) = non_empty_str(loc) {
                        return Some(s.to_string());
                    }
                    let path = loc.get("path").and_then(non_empty_str)?;
                    match loc.get("line").and_then(Value::as_u64) {
                        Some(line) => Some(format!("{path}:{line}")),
                        None => Some(path.to_string()),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let mut diffs = Vec::new();
    let mut content_texts = Vec::new();
    if let Some(items) = tc.and_then(|t| t.get("content")).and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("diff") => {
                    let path = str_of(item, "path");
                    let old_text = item
                        .get("oldText")
                        .or_else(|| item.get("old_text"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let new_text = item
                        .get("newText")
                        .or_else(|| item.get("new_text"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    diffs.push(PermissionDiff {
                        path,
                        old_text,
                        new_text,
                    });
                }
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(non_empty_str) {
                        content_texts.push(t.to_string());
                    }
                }
                Some("content") => {
                    if let Some(t) = item.pointer("/content/text").and_then(non_empty_str) {
                        content_texts.push(t.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    let preview_text = if diffs.is_empty() && command.is_none() {
        if !content_texts.is_empty() {
            Some(content_texts.join("\n"))
        } else {
            tc.and_then(|t| t.get("rawInput"))
                .and_then(format_raw_input)
        }
    } else {
        None
    };

    PermissionRequest {
        tool_call_id: tc
            .and_then(|t| t.get("toolCallId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        title: tc
            .and_then(|t| t.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string),
        kind: tc
            .and_then(|t| t.get("kind"))
            .and_then(non_empty_str)
            .map(str::to_string),
        command,
        locations,
        diffs,
        preview_text,
        options: p
            .get("options")
            .and_then(Value::as_array)
            .map(|os| {
                os.iter()
                    .map(|o| PermissionOption {
                        id: str_of(o, "optionId"),
                        name: str_of(o, "name"),
                        kind: str_of(o, "kind"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn parse_elicitation(p: &Value) -> Option<ElicitationRequest> {
    if p.get("mode").and_then(Value::as_str) == Some("url") {
        return None;
    }
    let message = str_of(p, "message");
    let schema = p.get("requestedSchema");
    let required: std::collections::HashSet<&str> = schema
        .and_then(|s| s.get("required"))
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut entries: Vec<(&String, &Value)> = schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
        .map(|props| props.iter().collect())
        .unwrap_or_default();
    entries.sort_by(|(a, _), (b, _)| elicitation_key_order(a).cmp(&elicitation_key_order(b)));

    let fields = entries
        .into_iter()
        .map(|(key, prop)| QuestionField {
            key: key.clone(),
            title: prop
                .get("title")
                .and_then(non_empty_str)
                .map(str::to_string),
            description: prop
                .get("description")
                .and_then(non_empty_str)
                .map(str::to_string),
            required: required.contains(key.as_str()),
            kind: parse_field_kind(prop),
        })
        .collect();

    Some(ElicitationRequest { message, fields })
}

/// Keep indexed `question_<n>` and `question_<n>_custom` keys in numeric order
/// even when JSON object keys are sorted alphabetically.
fn elicitation_key_order(key: &str) -> (u8, usize, u8, &str) {
    if let Some(rest) = key.strip_prefix("question_") {
        if let Some(num_str) = rest.strip_suffix("_custom") {
            if let Ok(n) = num_str.parse::<usize>() {
                return (0, n, 1, key);
            }
        } else if let Ok(n) = rest.parse::<usize>() {
            return (0, n, 0, key);
        }
    }
    (1, 0, 0, key)
}

fn parse_field_kind(prop: &Value) -> FieldKind {
    let ty = prop.get("type").and_then(Value::as_str).unwrap_or("string");
    match ty {
        "array" => {
            let options = prop.get("items").map(parse_options).unwrap_or_default();
            let default = prop
                .get("default")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            FieldKind::MultiSelect { options, default }
        }
        "boolean" => FieldKind::Bool {
            default: prop.get("default").and_then(Value::as_bool),
        },
        "integer" | "number" => FieldKind::Number {
            integer: ty == "integer",
            default: prop.get("default").and_then(|v| match v {
                Value::Number(n) => Some(n.to_string()),
                Value::String(s) => Some(s.clone()),
                _ => None,
            }),
        },
        _ => {
            let options = parse_options(prop);
            let default = prop
                .get("default")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !options.is_empty() {
                FieldKind::SingleSelect { options, default }
            } else {
                FieldKind::Text { default }
            }
        }
    }
}

fn parse_options(v: &Value) -> Vec<QuestionOption> {
    if let Some(arr) = v
        .get("oneOf")
        .or_else(|| v.get("anyOf"))
        .and_then(Value::as_array)
    {
        let mut out = Vec::new();
        for item in arr {
            let Some(val) = item.get("const").and_then(Value::as_str) else {
                continue;
            };
            let label = item
                .get("title")
                .and_then(non_empty_str)
                .unwrap_or(val)
                .to_string();
            let description = item
                .get("description")
                .and_then(non_empty_str)
                .map(str::to_string);
            out.push(QuestionOption {
                value: val.to_string(),
                label,
                description,
            });
        }
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(arr) = v.get("enum").and_then(Value::as_array) {
        let names = v.get("enumNames").and_then(Value::as_array);
        return arr
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let val = item.as_str()?;
                let label = names
                    .and_then(|ns| ns.get(i))
                    .and_then(non_empty_str)
                    .unwrap_or(val)
                    .to_string();
                Some(QuestionOption {
                    value: val.to_string(),
                    label,
                    description: None,
                })
            })
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted fake agent: replies to ids 0 (initialize), 1 (session/new)
    /// and 2 (session/prompt, after a permission round-trip).
    const FAKE: &str = r#"
read l
echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentInfo":{"name":"fake"},"authMethods":[]}}'
read l
echo '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}'
read l
echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}'
echo '{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"c1","title":"rm"},"options":[{"optionId":"ok","name":"Allow","kind":"allow_once"}]}}'
read l
case "$l" in *'"optionId":"ok"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}';; *) echo '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"refusal"}}';; esac
"#;

    #[test]
    fn full_turn_against_fake_agent() {
        let mut c = AcpClient::spawn("sh", &["-c", FAKE], Path::new(".")).unwrap();
        assert_eq!(c.initialize().unwrap(), "fake");
        let sid = c.new_session(Path::new("/tmp")).unwrap();
        assert_eq!(sid, "s1");
        let mut events = Vec::new();
        let mut perms = Vec::new();
        let stop = c
            .prompt(
                &sid,
                "hi",
                |e| events.push(e),
                |p| {
                    perms.push(p.clone());
                    Some(p.options[0].id.clone())
                },
            )
            .unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(events, vec![Event::AgentText("hello".into())]);
        assert_eq!(perms[0].tool_call_id, "c1");
        assert_eq!(perms[0].title.as_deref(), Some("rm"));
    }

    /// Advertises two models, then accepts `session/set_model`.
    const FAKE_MODELS: &str = r#"
read l
echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}'
read l
echo '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1","models":{"currentModelId":"a","availableModels":[{"modelId":"a","name":"Alpha"},{"modelId":"b"}]}}}'
read l
case "$l" in *'"modelId":"b"'*) echo '{"jsonrpc":"2.0","id":2,"result":{}}';; *) echo '{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"bad"}}';; esac
"#;

    #[test]
    fn models_are_advertised_and_settable() {
        let mut c = AcpClient::spawn("sh", &["-c", FAKE_MODELS], Path::new(".")).unwrap();
        c.initialize().unwrap();
        let sid = c.new_session(Path::new("/tmp")).unwrap();
        let m = c.models().clone();
        assert_eq!(m.current.as_deref(), Some("a"));
        assert_eq!(m.available.len(), 2);
        assert_eq!(m.display("a"), "Alpha");
        assert_eq!(m.display("b"), "b");
        c.set_model(&sid, "b").unwrap();
        assert_eq!(c.models().current.as_deref(), Some("b"));
    }

    #[test]
    fn model_as_config_option() {
        let opts = json!([
            {"id": "mode", "category": "mode", "currentValue": "default", "options": []},
            {"id": "model", "category": "model", "type": "select", "currentValue": "opus",
             "options": [
                {"value": "default", "name": "Default"},
                {"group": "g", "name": "G", "options": [{"value": "opus", "name": "Opus"}]}
             ]}
        ]);
        let m = ModelState::from_config_options(&opts).unwrap();
        assert_eq!(m.current.as_deref(), Some("opus"));
        assert_eq!(m.config_id.as_deref(), Some("model"));
        assert_eq!(m.display("opus"), "Opus");
        assert_eq!(m.available.len(), 2);
        let u = json!({"sessionUpdate": "config_option_update", "configOptions": opts});
        assert_eq!(
            parse_update(&u),
            Event::ConfigOptionsUpdated {
                models: Some(m),
                effort: None,
                modes: None,
            }
        );
        assert!(ModelState::from_config_options(&json!([])).is_none());
    }

    #[test]
    fn effort_as_config_option_supports_category_and_id_fallbacks() {
        let claude_opts = json!([
            {"id": "model", "category": "model", "type": "select", "currentValue": "opus",
             "options": [{"value": "opus", "name": "Opus"}]},
            {"id": "effort", "category": "thought_level", "type": "select", "currentValue": "high",
             "options": [
                {"value": "default", "name": "Default"},
                {"value": "low", "name": "Low", "description": "Fast replies"},
                {"value": "high", "name": "High"}
             ]}
        ]);
        let e = EffortState::from_config_options(&claude_opts).unwrap();
        assert_eq!(e.current.as_deref(), Some("high"));
        assert_eq!(e.config_id.as_deref(), Some("effort"));
        assert_eq!(e.display("high"), "High");
        assert_eq!(e.available.len(), 3);
        assert_eq!(e.available[1].description.as_deref(), Some("Fast replies"));

        // Fallback by id when category is omitted (e.g. codex-acp reasoning_effort)
        let codex_opts = json!([
            {"id": "reasoning_effort", "type": "select", "currentValue": "medium",
             "options": [
                {"group": "levels", "options": [
                    {"value": "low", "name": "Low"},
                    {"value": "medium", "name": "Medium"},
                    {"value": "xhigh", "name": "Extra High"}
                ]}
             ]}
        ]);
        let ce = EffortState::from_config_options(&codex_opts).unwrap();
        assert_eq!(ce.config_id.as_deref(), Some("reasoning_effort"));
        assert_eq!(ce.current.as_deref(), Some("medium"));
        assert_eq!(ce.display("xhigh"), "Extra High");

        let u = json!({"sessionUpdate": "config_option_update", "configOptions": claude_opts});
        assert_eq!(
            parse_update(&u),
            Event::ConfigOptionsUpdated {
                models: ModelState::from_config_options(&claude_opts),
                effort: Some(e),
                modes: None,
            }
        );
        assert!(EffortState::from_config_options(&json!([])).is_none());
    }

    #[test]
    fn effort_set_and_model_switch_sync_config_options() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, sys

model = "opus"
effort = "default"

def config_options():
    opts = [
        {"id": "model", "category": "model", "type": "select", "currentValue": model,
         "options": [
             {"value": "opus", "name": "Opus"},
             {"value": "haiku", "name": "Haiku"},
             {"value": "sonnet", "name": "Sonnet"},
         ]}
    ]
    if model == "opus":
        opts.append({
            "id": "effort", "category": "thought_level", "type": "select", "currentValue": effort,
            "options": [
                {"value": "default", "name": "Default"},
                {"value": "high", "name": "High"},
                {"value": "max", "name": "Max"},
            ]
        })
    elif model == "sonnet":
        opts.append({
            "id": "effort", "category": "thought_level", "type": "select",
            "currentValue": "medium" if effort == "max" else effort,
            "options": [
                {"value": "default", "name": "Default"},
                {"value": "medium", "name": "Medium"},
                {"value": "high", "name": "High"},
            ]
        })
    return opts

for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":1,"agentInfo":{"name":"fake"}}}), flush=True)
    elif m == "session/new":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"s1","configOptions":config_options()}}), flush=True)
    elif m == "session/set_config_option":
        cid = msg["params"]["configId"]
        val = msg["params"]["value"]
        if cid == "effort":
            effort = val
        elif cid == "model":
            model = val
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"configOptions":config_options()}}), flush=True)
    elif m == "session/prompt":
        effort = "high"
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{"sessionUpdate":"config_option_update","configOptions":config_options()}
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}), flush=True)
"#;
        let mut c = AcpClient::spawn("python3", &["-c", script], dir.path()).unwrap();
        c.initialize().unwrap();
        let sid = c.new_session(dir.path()).unwrap();
        assert_eq!(c.models().current.as_deref(), Some("opus"));
        assert_eq!(c.effort().current.as_deref(), Some("default"));
        assert_eq!(c.effort().available.len(), 3);

        c.set_effort(&sid, "max").unwrap();
        assert_eq!(c.effort().current.as_deref(), Some("max"));

        // Switching to Haiku (no effort support) clears available effort options.
        c.set_model(&sid, "haiku").unwrap();
        assert_eq!(c.models().current.as_deref(), Some("haiku"));
        assert!(c.effort().available.is_empty());
        assert!(c.set_effort(&sid, "high").is_err());

        // Switching to Sonnet repopulates effort with Sonnet's levels (clamped from max -> medium).
        c.set_model(&sid, "sonnet").unwrap();
        assert_eq!(c.models().current.as_deref(), Some("sonnet"));
        assert_eq!(c.effort().current.as_deref(), Some("medium"));
        assert_eq!(c.effort().available.len(), 3);

        // Mid-turn config_option_update syncs AcpClient and delivers Event::ConfigOptionsUpdated.
        let mut events = Vec::new();
        c.prompt(&sid, "hi", |e| events.push(e), |_| None).unwrap();
        assert_eq!(c.effort().current.as_deref(), Some("high"));
        assert!(matches!(
            events.first(),
            Some(Event::ConfigOptionsUpdated {
                effort: Some(e),
                ..
            }) if e.current.as_deref() == Some("high")
        ));
    }

    #[test]
    fn model_update_notification_is_parsed() {
        let u = json!({"sessionUpdate": "current_model_update", "currentModelId": "b"});
        assert_eq!(parse_update(&u), Event::ModelChanged("b".into()));
    }

    #[test]
    fn session_mode_parses_config_options_and_legacy_modes() {
        let opts = json!([
            {"id": "mode", "category": "mode", "type": "select", "currentValue": "default",
             "options": [
                {"value": "default", "name": "Default", "description": "Ask before edits"},
                {"value": "acceptEdits", "name": "Accept Edits"},
                {"value": "plan", "name": "Plan Mode", "description": "Read-only planning"},
                {"value": "bypassPermissions", "name": "Bypass Permissions"}
             ]}
        ]);
        let m = SessionModeState::from_config_options(&opts).unwrap();
        assert_eq!(m.current.as_deref(), Some("default"));
        assert_eq!(m.config_id.as_deref(), Some("mode"));
        assert_eq!(m.display("plan"), "Plan Mode");
        assert_eq!(m.available.len(), 4);
        assert_eq!(
            m.available[0].description.as_deref(),
            Some("Ask before edits")
        );

        let u = json!({"sessionUpdate": "config_option_update", "configOptions": opts});
        assert_eq!(
            parse_update(&u),
            Event::ConfigOptionsUpdated {
                models: None,
                effort: None,
                modes: Some(m),
            }
        );

        let legacy = json!({
            "currentModeId": "ask",
            "availableModes": [
                {"id": "ask", "name": "Ask", "description": "Read-only"},
                {"id": "code", "name": "Code"}
            ]
        });
        let lm = SessionModeState::parse(&legacy);
        assert_eq!(lm.current.as_deref(), Some("ask"));
        assert_eq!(lm.config_id, None);
        assert_eq!(lm.available.len(), 2);
        assert_eq!(lm.available[0].description.as_deref(), Some("Read-only"));
        assert_eq!(lm.display("code"), "Code");

        let mu = json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"});
        assert_eq!(parse_update(&mu), Event::ModeChanged("code".into()));
    }

    #[test]
    fn session_mode_switches_via_config_option_and_legacy_set_mode() {
        let dir = tempfile::tempdir().unwrap();
        // Config-option mode + mid-turn mode update.
        let config_script = r#"
import json, sys

mode = "default"

def config_options():
    return [{
        "id": "mode", "category": "mode", "type": "select", "currentValue": mode,
        "options": [
            {"value": "default", "name": "Default"},
            {"value": "plan", "name": "Plan Mode"},
            {"value": "bypassPermissions", "name": "Auto"}
        ]
    }]

for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":1,"agentInfo":{"name":"fake"}}}), flush=True)
    elif m == "session/new":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"s1","configOptions":config_options()}}), flush=True)
    elif m == "session/set_config_option":
        mode = msg["params"]["value"]
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"configOptions":config_options()}}), flush=True)
    elif m == "session/prompt":
        mode = "bypassPermissions"
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{"sessionUpdate":"config_option_update","configOptions":config_options()}
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}), flush=True)
"#;
        let mut c = AcpClient::spawn("python3", &["-c", config_script], dir.path()).unwrap();
        c.initialize().unwrap();
        let sid = c.new_session(dir.path()).unwrap();
        assert_eq!(c.modes().current.as_deref(), Some("default"));
        assert_eq!(c.modes().config_id.as_deref(), Some("mode"));
        c.set_mode(&sid, "plan").unwrap();
        assert_eq!(c.modes().current.as_deref(), Some("plan"));

        let mut events = Vec::new();
        c.prompt(&sid, "go", |e| events.push(e), |_| None).unwrap();
        assert_eq!(c.modes().current.as_deref(), Some("bypassPermissions"));
        assert!(matches!(
            events.first(),
            Some(Event::ConfigOptionsUpdated { modes: Some(m), .. })
                if m.current.as_deref() == Some("bypassPermissions")
        ));

        // Legacy `modes` + `session/set_mode` + `current_mode_update`.
        let legacy_script = r#"
import json, sys

for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":1,"agentInfo":{"name":"legacy"}}}), flush=True)
    elif m == "session/new":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "sessionId":"s2",
            "modes":{
                "currentModeId":"ask",
                "availableModes":[
                    {"id":"ask","name":"Ask"},
                    {"id":"code","name":"Code"}
                ]
            }
        }}), flush=True)
    elif m == "session/set_mode":
        if msg["params"]["modeId"] == "code":
            print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{}}), flush=True)
        else:
            print(json.dumps({"jsonrpc":"2.0","id":mid,"error":{"code":-32602,"message":"bad mode"}}), flush=True)
    elif m == "session/prompt":
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s2",
            "update":{"sessionUpdate":"current_mode_update","currentModeId":"ask"}
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}), flush=True)
"#;
        let mut lc = AcpClient::spawn("python3", &["-c", legacy_script], dir.path()).unwrap();
        lc.initialize().unwrap();
        let lsid = lc.new_session(dir.path()).unwrap();
        assert_eq!(lc.modes().current.as_deref(), Some("ask"));
        assert_eq!(lc.modes().config_id, None);
        lc.set_mode(&lsid, "code").unwrap();
        assert_eq!(lc.modes().current.as_deref(), Some("code"));
        assert!(lc.set_mode(&lsid, "nope").is_err());

        let mut levents = Vec::new();
        lc.prompt(&lsid, "go", |e| levents.push(e), |_| None)
            .unwrap();
        assert_eq!(lc.modes().current.as_deref(), Some("ask"));
        assert_eq!(levents, vec![Event::ModeChanged("ask".into())]);
    }

    #[test]
    fn tool_call_and_update_parse_text_and_diff_content() {
        let call = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "c1",
            "title": "Edit main.rs",
            "kind": "edit",
            "status": "in_progress",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "updating file"}},
                {"type": "diff", "path": "src/main.rs", "oldText": "let x = 1;", "newText": "let x = 2;"}
            ]
        });
        assert_eq!(
            parse_update(&call),
            Event::ToolCall {
                id: "c1".into(),
                title: "Edit main.rs".into(),
                kind: "edit".into(),
                status: "in_progress".into(),
                content: vec![
                    ToolContent::Text("updating file".into()),
                    ToolContent::Diff {
                        path: "src/main.rs".into(),
                        old_text: Some("let x = 1;".into()),
                        new_text: "let x = 2;".into(),
                    },
                ],
            }
        );

        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "title": "Edited src/main.rs",
            "status": "completed",
            "content": [
                {"type": "text", "text": "done"},
                {"type": "terminal", "terminalId": "t1", "output": "ok\n"},
                {"type": "diff", "path": "new.rs", "oldText": null, "newText": "fn f() {}"}
            ]
        });
        assert_eq!(
            parse_update(&update),
            Event::ToolUpdate {
                id: "c1".into(),
                title: Some("Edited src/main.rs".into()),
                status: Some("completed".into()),
                content: vec![
                    ToolContent::Text("done".into()),
                    ToolContent::Text("ok\n".into()),
                    ToolContent::Diff {
                        path: "new.rs".into(),
                        old_text: None,
                        new_text: "fn f() {}".into(),
                    },
                ],
            }
        );

        // Missing or empty content blocks produce an empty Vec.
        let minimal = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "status": "failed"
        });
        assert_eq!(
            parse_update(&minimal),
            Event::ToolUpdate {
                id: "c1".into(),
                title: None,
                status: Some("failed".into()),
                content: vec![],
            }
        );
    }

    #[test]
    fn spawn_failure_is_reported() {
        let r = AcpClient::spawn("definitely-not-a-binary-xyz", &[], Path::new("."));
        assert!(matches!(r, Err(AcpError::Spawn(..))));
    }

    #[test]
    fn parse_permission_extracts_command_locations_and_diffs() {
        let exec = parse_permission(&json!({
            "toolCall": {
                "toolCallId": "c1",
                "title": "Run tests",
                "kind": "execute",
                "rawInput": {"command": "cargo", "args": ["test", "--all"]},
                "locations": [{"path": "Cargo.toml", "line": 12}, {"path": "src/lib.rs"}],
            },
            "options": [
                {"optionId": "a1", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "r1", "name": "Reject", "kind": "reject_once"}
            ]
        }));
        assert_eq!(exec.tool_call_id, "c1");
        assert_eq!(exec.title.as_deref(), Some("Run tests"));
        assert_eq!(exec.kind.as_deref(), Some("execute"));
        assert_eq!(exec.command.as_deref(), Some("cargo test --all"));
        assert_eq!(exec.locations, vec!["Cargo.toml:12", "src/lib.rs"]);
        assert!(exec.diffs.is_empty());
        assert!(exec.preview_text.is_none());

        let array_cmd = parse_permission(&json!({
            "toolCall": {
                "toolCallId": "c2",
                "rawInput": {"cmd": ["git", "status", "-s"]}
            }
        }));
        assert_eq!(array_cmd.command.as_deref(), Some("git status -s"));

        let path_fallback = parse_permission(&json!({
            "toolCall": {
                "toolCallId": "c3",
                "kind": "read",
                "rawInput": {"file_path": "src/main.rs"}
            }
        }));
        assert_eq!(path_fallback.command.as_deref(), Some("src/main.rs"));

        let diff_req = parse_permission(&json!({
            "toolCall": {
                "toolCallId": "c4",
                "title": "Edit client",
                "kind": "edit",
                "locations": [{"path": "src/client.rs", "line": 5}],
                "content": [{
                    "type": "diff",
                    "path": "src/client.rs",
                    "oldText": "let x = 1;\n",
                    "newText": "let x = 2;\n"
                }]
            }
        }));
        assert_eq!(diff_req.kind.as_deref(), Some("edit"));
        assert_eq!(
            diff_req.diffs,
            vec![PermissionDiff {
                path: "src/client.rs".into(),
                old_text: Some("let x = 1;\n".into()),
                new_text: "let x = 2;\n".into(),
            }]
        );
        assert!(diff_req.preview_text.is_none());
    }

    #[test]
    fn parse_permission_extracts_preview_text_from_content_or_raw_input() {
        let wrapped = parse_permission(&json!({
            "toolCall": {
                "toolCallId": "c5",
                "content": [
                    {"type": "content", "content": {"type": "text", "text": "first line"}},
                    {"type": "text", "text": "second line"}
                ]
            }
        }));
        assert_eq!(
            wrapped.preview_text.as_deref(),
            Some("first line\nsecond line")
        );

        let raw_summary = parse_permission(&json!({
            "toolCall": {
                "toolCallId": "c6",
                "rawInput": {"query": "retry", "limit": 5}
            }
        }));
        assert_eq!(
            raw_summary.preview_text.as_deref(),
            Some("limit: 5\nquery: retry")
        );
    }

    #[test]
    fn prompt_blocks_reads_embedded_context_capability_and_sends_resources() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":1,
            "agentCapabilities":{"promptCapabilities":{"embeddedContext":True}},
            "agentInfo":{"name":"fake"}
        }}), flush=True)
    elif m == "session/new":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"s1"}}), flush=True)
    elif m == "session/prompt":
        prompt = msg["params"]["prompt"]
        kinds = [b["type"] for b in prompt]
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":",".join(kinds)}}
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}), flush=True)
"#;
        let mut client = AcpClient::spawn("python3", &["-c", script], dir.path()).unwrap();
        client.initialize().unwrap();
        assert!(client.supports_embedded_context());
        let sid = client.new_session(dir.path()).unwrap();
        let blocks = vec![
            PromptBlock::Text("see ".into()),
            PromptBlock::Resource {
                uri: "file:///a.rs".into(),
                text: "fn a() {}".into(),
                mime_type: Some("text/x-rust".into()),
            },
            PromptBlock::ResourceLink {
                uri: "file:///b.bin".into(),
                name: "b.bin".into(),
                mime_type: None,
            },
        ];
        let mut out = String::new();
        let stop = client
            .prompt_blocks(
                &sid,
                &blocks,
                |ev| {
                    if let Event::AgentText(t) = ev {
                        out.push_str(&t);
                    }
                },
                |_| None,
                |_| ElicitationResponse::Cancel,
            )
            .unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(out, "text,resource,resource_link");
    }

    #[test]
    fn elicitation_create_round_trip_against_fake_agent() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        caps = msg["params"]["clientCapabilities"]
        assert "form" in caps.get("elicitation", {}), f"missing elicitation.form: {caps}"
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":1,
            "agentInfo":{"name":"fake"}
        }}), flush=True)
    elif m == "session/new":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"s1"}}), flush=True)
    elif m == "session/prompt":
        print(json.dumps({"jsonrpc":"2.0","id":99,"method":"elicitation/create","params":{
            "mode":"form",
            "sessionId":"s1",
            "message":"Which database?",
            "requestedSchema":{
                "type":"object",
                "properties":{
                    "question_0":{
                        "type":"string",
                        "title":"Database",
                        "oneOf":[
                            {"const":"SQLite","title":"SQLite","description":"Embedded"},
                            {"const":"Postgres","title":"Postgres","description":"Server"}
                        ]
                    },
                    "question_0_custom":{"type":"string","title":"Other"},
                    "question_1":{
                        "type":"array",
                        "title":"Features",
                        "items":{"anyOf":[
                            {"const":"TLS","title":"TLS"},
                            {"const":"Metrics","title":"Metrics"}
                        ]}
                    }
                }
            }
        }}), flush=True)
        reply = json.loads(sys.stdin.readline())
        assert reply["id"] == 99
        res = reply["result"]
        text = f"{res['action']}:{res['content']['question_0']}:{','.join(res['content']['question_1'])}"
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}), flush=True)
"#;
        let mut client = AcpClient::spawn("python3", &["-c", script], dir.path()).unwrap();
        client.initialize().unwrap();
        let sid = client.new_session(dir.path()).unwrap();
        let mut out = String::new();
        let mut seen_fields = Vec::new();
        let stop = client
            .prompt_blocks(
                &sid,
                &[PromptBlock::Text("setup db".into())],
                |ev| {
                    if let Event::AgentText(t) = ev {
                        out.push_str(&t);
                    }
                },
                |_| None,
                |req| {
                    seen_fields = req.fields.iter().map(|f| f.key.clone()).collect();
                    let mut map = serde_json::Map::new();
                    map.insert("question_0".into(), json!("Postgres"));
                    map.insert("question_1".into(), json!(["TLS", "Metrics"]));
                    ElicitationResponse::Accept(map)
                },
            )
            .unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(
            seen_fields,
            vec!["question_0", "question_0_custom", "question_1"]
        );
        assert_eq!(out, "accept:Postgres:TLS,Metrics");
    }

    #[test]
    fn available_commands_update_parses_and_handles_edge_cases() {
        let u = json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {
                    "name": "web",
                    "description": "Search the web",
                    "input": {"hint": "query to search for"}
                },
                {
                    "name": "/compact",
                    "description": "  Compact conversation  ",
                    "input": null
                },
                {
                    "name": "WEB",
                    "description": "Duplicate ignored"
                },
                {
                    "name": "  ",
                    "description": "Empty name skipped"
                }
            ]
        });
        assert_eq!(
            parse_update(&u),
            Event::AvailableCommands(vec![
                AvailableCommand {
                    name: "web".into(),
                    description: "Search the web".into(),
                    input_hint: Some("query to search for".into()),
                },
                AvailableCommand {
                    name: "compact".into(),
                    description: "Compact conversation".into(),
                    input_hint: None,
                },
            ])
        );

        let empty = json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": []
        });
        assert_eq!(parse_update(&empty), Event::AvailableCommands(vec![]));

        let malformed = json!({
            "sessionUpdate": "available_commands_update"
        });
        assert_eq!(
            parse_update(&malformed),
            Event::Other("available_commands_update".into())
        );
    }

    #[test]
    fn available_commands_received_after_new_session_and_mid_turn() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":1,
            "agentInfo":{"name":"fake"}
        }}), flush=True)
    elif m == "session/new":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"s1"}}), flush=True)
        # Matches claude-agent-acp: emit available_commands_update right after session/new.
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{
                "sessionUpdate":"available_commands_update",
                "availableCommands":[
                    {"name":"compact","description":"Compact history"},
                    {"name":"mcp","description":"Manage MCP servers","input":{"hint":"[reconnect]"}}
                ]
            }
        }}), flush=True)
    elif m == "session/prompt":
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{
                "sessionUpdate":"available_commands_update",
                "availableCommands":[
                    {"name":"review","description":"Review changes"}
                ]
            }
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}), flush=True)
"#;
        let mut client = AcpClient::spawn("python3", &["-c", script], dir.path()).unwrap();
        client.initialize().unwrap();
        let sid = client.new_session(dir.path()).unwrap();

        let mut idle_events = Vec::new();
        for _ in 0..50 {
            client.poll_updates(|ev| idle_events.push(ev)).unwrap();
            if !client.commands().is_empty() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            client.commands(),
            &[
                AvailableCommand {
                    name: "compact".into(),
                    description: "Compact history".into(),
                    input_hint: None,
                },
                AvailableCommand {
                    name: "mcp".into(),
                    description: "Manage MCP servers".into(),
                    input_hint: Some("[reconnect]".into()),
                },
            ]
        );
        assert_eq!(idle_events.len(), 1);

        let mut prompt_events = Vec::new();
        let stop = client
            .prompt(&sid, "/compact", |ev| prompt_events.push(ev), |_| None)
            .unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(
            client.commands(),
            &[AvailableCommand {
                name: "review".into(),
                description: "Review changes".into(),
                input_hint: None,
            }]
        );
        assert_eq!(
            prompt_events,
            vec![Event::AvailableCommands(vec![AvailableCommand {
                name: "review".into(),
                description: "Review changes".into(),
                input_hint: None,
            }])]
        );
    }

    #[test]
    fn parse_usage_update_and_token_usage() {
        let u = parse_update(&json!({
            "sessionUpdate": "usage_update",
            "used": 45200,
            "size": 200000,
            "cost": { "amount": 0.14, "currency": "USD" },
        }));
        assert_eq!(
            u,
            Event::UsageUpdated(UsageUpdate {
                used: 45_200,
                size: 200_000,
                cost: Some(CostInfo {
                    amount: 0.14,
                    currency: "USD".into(),
                }),
            })
        );

        // Mid-turn usage_update without cost and with float numbers.
        let mid = parse_update(&json!({
            "sessionUpdate": "usage_update",
            "used": 1200.4,
            "size": 100000.0,
        }));
        assert_eq!(
            mid,
            Event::UsageUpdated(UsageUpdate {
                used: 1200,
                size: 100_000,
                cost: None,
            })
        );

        // Missing required fields falls back to Event::Other.
        assert_eq!(
            parse_update(&json!({"sessionUpdate": "usage_update", "used": 100})),
            Event::Other("usage_update".into())
        );

        // Prompt response usage breakdown.
        let tok = TokenUsage::parse(&json!({
            "totalTokens": 12345,
            "inputTokens": 10000,
            "outputTokens": 2345,
            "thoughtTokens": 500,
            "cachedReadTokens": 8000,
            "cachedWriteTokens": 200,
        }))
        .unwrap();
        assert_eq!(
            tok,
            TokenUsage {
                total_tokens: 12_345,
                input_tokens: 10_000,
                output_tokens: 2_345,
                thought_tokens: Some(500),
                cached_read_tokens: Some(8_000),
                cached_write_tokens: Some(200),
            }
        );
        // Empty usage object is ignored.
        assert_eq!(TokenUsage::parse(&json!({})), None);
    }

    #[test]
    fn usage_update_and_prompt_usage_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":1,
            "agentInfo":{"name":"fake"}
        }}), flush=True)
    elif m == "session/new":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"s1"}}), flush=True)
    elif m == "session/prompt":
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{"sessionUpdate":"usage_update","used":40000,"size":200000}
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s1",
            "update":{
                "sessionUpdate":"usage_update",
                "used":52000,
                "size":200000,
                "cost":{"amount":0.18,"currency":"USD"}
            }
        }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "stopReason":"end_turn",
            "usage":{
                "totalTokens":52000,
                "inputTokens":48000,
                "outputTokens":4000,
                "cachedReadTokens":30000
            }
        }}), flush=True)
"#;
        let mut client = AcpClient::spawn("python3", &["-c", script], dir.path()).unwrap();
        client.initialize().unwrap();
        let sid = client.new_session(dir.path()).unwrap();
        assert_eq!(client.usage(), None);

        let mut events = Vec::new();
        let stop = client
            .prompt(&sid, "hi", |ev| events.push(ev), |_| None)
            .unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(
            events,
            vec![
                Event::UsageUpdated(UsageUpdate {
                    used: 40_000,
                    size: 200_000,
                    cost: None,
                }),
                Event::UsageUpdated(UsageUpdate {
                    used: 52_000,
                    size: 200_000,
                    cost: Some(CostInfo {
                        amount: 0.18,
                        currency: "USD".into(),
                    }),
                }),
                Event::TurnUsage(TokenUsage {
                    total_tokens: 52_000,
                    input_tokens: 48_000,
                    output_tokens: 4_000,
                    thought_tokens: None,
                    cached_read_tokens: Some(30_000),
                    cached_write_tokens: None,
                }),
            ]
        );
        assert_eq!(
            client.usage(),
            Some(&UsageUpdate {
                used: 52_000,
                size: 200_000,
                cost: Some(CostInfo {
                    amount: 0.18,
                    currency: "USD".into(),
                }),
            })
        );
    }

    #[test]
    fn parse_claude_sdk_cache_detects_ttl_and_skips_subagents() {
        // Explicit 1h tier on assistant message.
        let ev_1h = parse_claude_sdk_cache(&json!({
            "sessionId": "s1",
            "message": {
                "type": "assistant",
                "parent_tool_use_id": null,
                "message": {
                    "usage": {
                        "cache_creation_input_tokens": 12000,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": 0,
                            "ephemeral_1h_input_tokens": 12000
                        }
                    }
                }
            }
        }));
        assert_eq!(
            ev_1h,
            Some(Event::CacheActivity {
                ttl: Some(CacheTtl::OneHour)
            })
        );

        // Explicit 5m tier on result message.
        let ev_5m = parse_claude_sdk_cache(&json!({
            "sessionId": "s1",
            "message": {
                "type": "result",
                "usage": {
                    "cache_creation_input_tokens": 8000,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 8000,
                        "ephemeral_1h_input_tokens": 0
                    }
                }
            }
        }));
        assert_eq!(
            ev_5m,
            Some(Event::CacheActivity {
                ttl: Some(CacheTtl::FiveMinutes)
            })
        );

        // Pure cache read refreshes timer without overriding TTL tier.
        let ev_hit = parse_claude_sdk_cache(&json!({
            "sessionId": "s1",
            "message": {
                "type": "assistant",
                "message": {
                    "usage": {
                        "cache_read_input_tokens": 45000,
                        "cache_creation_input_tokens": 0
                    }
                }
            }
        }));
        assert_eq!(ev_hit, Some(Event::CacheActivity { ttl: None }));

        // Subagent message with parent_tool_use_id is ignored.
        let sub = parse_claude_sdk_cache(&json!({
            "sessionId": "s1",
            "message": {
                "type": "assistant",
                "parent_tool_use_id": "toolu_123",
                "message": {
                    "usage": {
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": 5000,
                            "ephemeral_1h_input_tokens": 0
                        }
                    }
                }
            }
        }));
        assert_eq!(sub, None);

        // Unrelated message type or zero cache tokens is ignored.
        assert_eq!(
            parse_claude_sdk_cache(&json!({
                "sessionId": "s1",
                "message": {
                    "type": "assistant",
                    "message": { "usage": { "input_tokens": 50, "output_tokens": 10 } }
                }
            })),
            None
        );
    }

    #[test]
    fn new_session_with_meta_forwards_meta_and_receives_claude_sdk_cache_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, sys
saw_meta = False
for line in sys.stdin:
    msg = json.loads(line)
    m, mid = msg.get("method"), msg.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":1,
            "agentInfo":{"name":"fake-claude"}
        }}), flush=True)
    elif m == "session/new":
        meta = msg.get("params", {}).get("_meta", {})
        saw_meta = bool(meta.get("claudeCode", {}).get("emitRawSDKMessages"))
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"s1"}}), flush=True)
    elif m == "session/prompt":
        if saw_meta:
            print(json.dumps({"jsonrpc":"2.0","method":"_claude/sdkMessage","params":{
                "sessionId":"s1",
                "message":{
                    "type":"assistant",
                    "parent_tool_use_id":None,
                    "message":{
                        "usage":{
                            "cache_creation_input_tokens":9000,
                            "cache_creation":{
                                "ephemeral_5m_input_tokens":0,
                                "ephemeral_1h_input_tokens":9000
                            }
                        }
                    }
                }
            }}), flush=True)
        print(json.dumps({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}}), flush=True)
"#;
        let mut client = AcpClient::spawn("python3", &["-c", script], dir.path()).unwrap();
        client.initialize().unwrap();
        let sid = client
            .new_session_with_meta(
                dir.path(),
                Some(json!({
                    "claudeCode": {
                        "emitRawSDKMessages": [
                            { "type": "assistant" },
                            { "type": "result" }
                        ]
                    }
                })),
            )
            .unwrap();
        let mut events = Vec::new();
        let stop = client
            .prompt(&sid, "hi", |ev| events.push(ev), |_| None)
            .unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(
            events,
            vec![Event::CacheActivity {
                ttl: Some(CacheTtl::OneHour)
            }]
        );
    }
}
