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
    /// An update type we don't model yet (kept so nothing is silently lost).
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub tool_call_id: String,
    pub title: Option<String>,
    pub options: Vec<PermissionOption>,
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
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AcpError::Protocol("session/new: missing sessionId".into()))
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
        _ => Event::Other(kind),
    }
}

fn parse_permission(p: &Value) -> PermissionRequest {
    PermissionRequest {
        tool_call_id: p
            .pointer("/toolCall/toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        title: p
            .pointer("/toolCall/title")
            .and_then(Value::as_str)
            .map(str::to_string),
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

    #[test]
    fn spawn_failure_is_reported() {
        let r = AcpClient::spawn("definitely-not-a-binary-xyz", &[], Path::new("."));
        assert!(matches!(r, Err(AcpError::Spawn(..))));
    }
}
