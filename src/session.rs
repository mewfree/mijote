//! One agent conversation: the transcript model plus the worker thread that
//! owns the ACP client. Never imports from `ui` or `app`.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::agent::acp::{AcpClient, CancelHandle, Event, PermissionRequest, StopReason};
use crate::agent::{install, AgentSpec};

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Agent(String),
    Thought(String),
    Tool {
        id: String,
        title: String,
        kind: String,
        status: String,
    },
    Notice(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Starting,
    Idle,
    Running,
    AwaitingPermission,
    Failed,
}

/// Worker -> UI messages.
#[derive(Debug)]
pub enum UiMsg {
    Ready(CancelHandle),
    Event(Event),
    Permission(PermissionRequest),
    TurnDone(StopReason),
    Error(String),
}

#[derive(Debug)]
pub struct Msg {
    pub session: usize,
    pub body: UiMsg,
}

enum WorkerCmd {
    Prompt(String),
    Quit,
}

pub struct Session {
    pub id: usize,
    pub spec: &'static AgentSpec,
    pub entries: Vec<Entry>,
    pub status: Status,
    pub pending: Option<PermissionRequest>,
    cancel: Option<CancelHandle>,
    cmd_tx: Sender<WorkerCmd>,
    perm_tx: Sender<Option<String>>,
}

impl Session {
    /// Start a session; the agent is installed (if needed) and launched on a
    /// worker thread, reporting back through `ui_tx`.
    pub fn spawn(id: usize, spec: &'static AgentSpec, cwd: PathBuf, ui_tx: Sender<Msg>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (perm_tx, perm_rx) = mpsc::channel();
        thread::spawn(move || worker(id, spec, cwd, ui_tx, cmd_rx, perm_rx));
        Self::new(id, spec, cmd_tx, perm_tx)
    }

    fn new(
        id: usize,
        spec: &'static AgentSpec,
        cmd_tx: Sender<WorkerCmd>,
        perm_tx: Sender<Option<String>>,
    ) -> Self {
        Self {
            id,
            spec,
            entries: Vec::new(),
            status: Status::Starting,
            pending: None,
            cancel: None,
            cmd_tx,
            perm_tx,
        }
    }

    /// A session with no worker, for tests.
    #[cfg(test)]
    pub fn detached(id: usize, spec: &'static AgentSpec) -> (Self, Receiver<Option<String>>) {
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (perm_tx, perm_rx) = mpsc::channel();
        (Self::new(id, spec, cmd_tx, perm_tx), perm_rx)
    }

    pub fn can_prompt(&self) -> bool {
        self.status == Status::Idle
    }

    pub fn send_prompt(&mut self, text: String) {
        if !self.can_prompt() {
            return;
        }
        self.entries.push(Entry::User(text.clone()));
        self.status = Status::Running;
        let _ = self.cmd_tx.send(WorkerCmd::Prompt(text));
    }

    /// Answer a pending permission request (`None` cancels the turn).
    pub fn answer_permission(&mut self, option_id: Option<String>) {
        if self.pending.take().is_some() {
            self.status = Status::Running;
            let _ = self.perm_tx.send(option_id);
        }
    }

    /// Interrupt the running turn.
    pub fn cancel(&mut self) {
        if self.status != Status::Running && self.status != Status::AwaitingPermission {
            return;
        }
        // A pending permission must be answered `cancelled` first.
        self.answer_permission(None);
        if let Some(h) = &self.cancel {
            let _ = h.cancel();
        }
    }

    pub fn apply(&mut self, body: UiMsg) {
        match body {
            UiMsg::Ready(handle) => {
                self.cancel = Some(handle);
                self.status = Status::Idle;
            }
            UiMsg::Event(e) => self.apply_event(e),
            UiMsg::Permission(req) => {
                self.pending = Some(req);
                self.status = Status::AwaitingPermission;
            }
            UiMsg::TurnDone(stop) => {
                self.status = Status::Idle;
                self.pending = None;
                if stop != StopReason::EndTurn {
                    self.entries
                        .push(Entry::Notice(format!("stopped: {stop:?}")));
                }
            }
            UiMsg::Error(e) => {
                self.status = Status::Failed;
                self.pending = None;
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
            } => self.entries.push(Entry::Tool {
                id,
                title,
                kind,
                status,
            }),
            Event::ToolUpdate { id, status } => {
                if let Some(new) = status {
                    for entry in self.entries.iter_mut().rev() {
                        if let Entry::Tool {
                            id: tid, status, ..
                        } = entry
                        {
                            if *tid == id {
                                *status = new;
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
            Event::Other(_) => {}
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCmd::Quit);
    }
}

fn worker(
    id: usize,
    spec: &'static AgentSpec,
    cwd: PathBuf,
    ui_tx: Sender<Msg>,
    cmd_rx: Receiver<WorkerCmd>,
    perm_rx: Receiver<Option<String>>,
) {
    let send = |body: UiMsg| {
        let _ = ui_tx.send(Msg { session: id, body });
    };
    let run = || -> Result<(), String> {
        let program = install::ensure(spec).map_err(|e| e.to_string())?;
        let mut client = AcpClient::spawn(&program.to_string_lossy(), spec.args, &cwd)
            .map_err(|e| e.to_string())?;
        client.initialize().map_err(|e| e.to_string())?;
        let sid = client.new_session(&cwd).map_err(|e| e.to_string())?;
        send(UiMsg::Ready(client.cancel_handle(&sid)));
        while let Ok(WorkerCmd::Prompt(text)) = cmd_rx.recv() {
            let stop = client
                .prompt(
                    &sid,
                    &text,
                    |e| send(UiMsg::Event(e)),
                    |req| {
                        send(UiMsg::Permission(req.clone()));
                        perm_rx.recv().ok().flatten()
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
        }));
        s.apply(UiMsg::Event(Event::ToolUpdate {
            id: "t1".into(),
            status: Some("completed".into()),
        }));
        assert_eq!(s.entries[0], Entry::Agent("hello".into()));
        assert!(matches!(&s.entries[1], Entry::Tool { status, .. } if status == "completed"));
        s.apply(UiMsg::TurnDone(StopReason::EndTurn));
        assert_eq!(s.status, Status::Idle);
    }

    #[test]
    fn permission_round_trip_and_cancel() {
        let (mut s, perm_rx) = session();
        s.apply(UiMsg::Permission(PermissionRequest {
            tool_call_id: "c".into(),
            title: None,
            options: vec![PermissionOption {
                id: "ok".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            }],
        }));
        assert_eq!(s.status, Status::AwaitingPermission);
        s.answer_permission(Some("ok".into()));
        assert_eq!(perm_rx.try_recv().unwrap(), Some("ok".into()));
        assert_eq!(s.status, Status::Running);

        s.apply(UiMsg::Permission(PermissionRequest {
            tool_call_id: "c2".into(),
            title: None,
            options: vec![],
        }));
        s.cancel();
        assert_eq!(perm_rx.try_recv().unwrap(), None);
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
}
