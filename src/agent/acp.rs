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
    },
    ToolUpdate {
        id: String,
        status: Option<String>,
    },
    Plan(Vec<String>),
    /// The agent switched models (e.g. `/model` inside Claude Code).
    ModelChanged(String),
    /// An update type we don't model yet (kept so nothing is silently lost).
    Other(String),
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

    /// Send a request and block until its response, dispatching any
    /// agent->client traffic that arrives in the meantime.
    fn call(
        &mut self,
        method: &str,
        params: Value,
        on_event: &mut dyn FnMut(Event),
        on_permission: &mut dyn FnMut(&PermissionRequest) -> Option<String>,
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
                        on_event(parse_update(update));
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
        self.call(method, params, &mut |_| {}, &mut |_| None)
    }

    /// Negotiate version and capabilities. Returns the agent's display name.
    pub fn initialize(&mut self) -> Result<String, AcpError> {
        let result = self.call_quiet(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": "mijote",
                    "title": "mijote",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        Ok(result
            .pointer("/agentInfo/title")
            .or_else(|| result.pointer("/agentInfo/name"))
            .and_then(Value::as_str)
            .unwrap_or("agent")
            .to_string())
    }

    pub fn new_session(&mut self, cwd: &Path) -> Result<String, AcpError> {
        let result = self.call_quiet(
            "session/new",
            json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
        )?;
        if let Some(m) = result
            .get("configOptions")
            .and_then(ModelState::from_config_options)
        {
            self.models = m;
        } else if let Some(models) = result.get("models") {
            self.models = ModelState::parse(models);
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

    /// Switch the session's model. Agents that do not support it answer with
    /// an RPC error, which the caller reports.
    pub fn set_model(&mut self, session_id: &str, model_id: &str) -> Result<(), AcpError> {
        match self.models.config_id.clone() {
            Some(config_id) => self.call_quiet(
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": config_id, "value": model_id}),
            )?,
            None => self.call_quiet(
                "session/set_model",
                json!({"sessionId": session_id, "modelId": model_id}),
            )?,
        };
        self.models.current = Some(model_id.to_string());
        Ok(())
    }

    /// Run one prompt turn to completion.
    ///
    /// `on_permission` returns the chosen option id, or `None` to cancel.
    pub fn prompt(
        &mut self,
        session_id: &str,
        text: &str,
        mut on_event: impl FnMut(Event),
        mut on_permission: impl FnMut(&PermissionRequest) -> Option<String>,
    ) -> Result<StopReason, AcpError> {
        let result = self.call(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}],
            }),
            &mut on_event,
            &mut on_permission,
        )?;
        let reason = result
            .get("stopReason")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::Protocol("session/prompt: missing stopReason".into()))?;
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
        },
        "tool_call_update" => Event::ToolUpdate {
            id: str_of(u, "toolCallId"),
            status: u.get("status").and_then(Value::as_str).map(str::to_string),
        },
        "plan" => Event::Plan(
            u.get("entries")
                .and_then(Value::as_array)
                .map(|es| es.iter().map(|e| str_of(e, "content")).collect())
                .unwrap_or_default(),
        ),
        "config_option_update" => match u
            .get("configOptions")
            .and_then(ModelState::from_config_options)
            .and_then(|m| m.current)
        {
            Some(id) => Event::ModelChanged(id),
            None => Event::Other(kind),
        },
        "current_model_update" => Event::ModelChanged(str_of(u, "currentModelId")),
        _ => Event::Other(kind),
    }
}

fn non_empty_str(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
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
        assert_eq!(parse_update(&u), Event::ModelChanged("opus".into()));
        assert!(ModelState::from_config_options(&json!([])).is_none());
    }

    #[test]
    fn model_update_notification_is_parsed() {
        let u = json!({"sessionUpdate": "current_model_update", "currentModelId": "b"});
        assert_eq!(parse_update(&u), Event::ModelChanged("b".into()));
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
}
