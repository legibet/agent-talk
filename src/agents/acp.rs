//! An agent child speaking ACP (`grok agent … stdio`, `devin acp`): line-delimited
//! JSON-RPC 2.0 on its stdin/stdout.
//!
//! One reader task dispatches three kinds of frames separately, as in the Codex
//! transport: responses to our requests (by id), notifications (unbounded: one child
//! serves one command, so the buffer holds at most one session's replay and turn), and
//! server-to-client requests such as `session/request_permission` (their ids overlap
//! with ours). The reader never answers server requests; the adapter decides.

use crate::model::{AgentError, Error, ErrorCode, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const RPC_TIMEOUT: Duration = Duration::from_secs(120);
/// Bytes of the child's stderr kept for error messages.
const STDERR_TAIL: usize = 4096;

/// A response as the reader delivers it: the result, or the agent's error object.
type Reply = std::result::Result<Value, AgentError>;

#[derive(Debug)]
pub enum Event {
    Notification { method: String, params: Value },
    Request(ServerRequest),
}

/// A request from the agent awaiting an answer (permission requests).
#[derive(Debug)]
pub struct ServerRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Deserialize)]
struct Incoming {
    id: Option<Value>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    /// Kept raw: an error object that does not fit JSON-RPC's shape still answers its
    /// request (`agent_error`).
    error: Option<Value>,
}

/// The agent's error object; one without an integer code or a message keeps what it has,
/// with the whole object as data.
fn agent_error(e: Value) -> AgentError {
    AgentError::deserialize(&e).unwrap_or_else(|_| AgentError {
        code: e["code"].as_i64().unwrap_or_default(),
        message: e["message"]
            .as_str()
            .map_or_else(|| e.to_string(), String::from),
        data: Some(e),
    })
}

#[derive(Default)]
struct Pending {
    closed: Option<String>,
    waiters: HashMap<u64, oneshot::Sender<Reply>>,
}

/// What the connection, the reader and the stderr task share.
struct Shared {
    /// The agent's name in error messages (`grok agent`, `devin acp`).
    agent: &'static str,
    pending: Mutex<Pending>,
    /// The tail of the child's stderr, for error messages.
    stderr: Mutex<String>,
}

impl Shared {
    /// The error for a connection the reader saw end.
    fn closed(&self) -> Error {
        let reason = self.pending.lock().unwrap().closed.clone();
        let stderr = self.stderr.lock().unwrap().trim().to_string();
        let mut msg = format!(
            "{} connection lost: {}",
            self.agent,
            reason.as_deref().unwrap_or("connection closed")
        );
        if !stderr.is_empty() {
            let _ = write!(msg, "; stderr: {stderr}");
        }
        Error::new(ErrorCode::Transport, msg)
    }
}

/// The response to a request `Conn::submit` wrote. Agent errors keep code/message/data; a
/// connection that ends first is a transport error. Dropping it forgets the request, so a
/// response that comes later is discarded.
pub struct Response {
    id: u64,
    rx: oneshot::Receiver<Reply>,
    shared: Arc<Shared>,
}

impl Drop for Response {
    fn drop(&mut self) {
        self.shared.pending.lock().unwrap().waiters.remove(&self.id);
    }
}

impl Future for Response {
    type Output = Result<Value>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<Value>> {
        Poll::Ready(match ready!(Pin::new(&mut self.rx).poll(cx)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(Error::from_agent(
                if e.code == -32601 {
                    ErrorCode::Unsupported
                } else {
                    ErrorCode::Precondition
                },
                e,
            )),
            // The reader dropped the waiter: the connection ended.
            Err(_) => Err(self.shared.closed()),
        })
    }
}

pub struct Conn {
    /// The `initialize` response (Grok: `_meta.modelState` lists the account's models).
    pub init: Value,
    child: Child,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    events: mpsc::UnboundedReceiver<Event>,
    requests: mpsc::UnboundedReceiver<ServerRequest>,
    reader: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Spawn `cmd`, the agent's command prepared by the adapter (program, arguments, cwd,
/// environment, from `child::agent_cmd`), and run the ACP `initialize` handshake. `agent`
/// names it in errors.
pub async fn spawn(mut cmd: Command, agent: &'static str) -> Result<Conn> {
    // Its own process group, so that Ctrl-C reaches agent-talk, which then ends the
    // turn itself; kill_on_drop for a command that unwinds without closing.
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::transport(format!("spawn {agent}"), e))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut err_pipe = child.stderr.take().expect("stderr is piped");
    let shared = Arc::new(Shared {
        agent,
        pending: Mutex::default(),
        stderr: Mutex::default(),
    });
    let tail = shared.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        while let Ok(n) = err_pipe.read(&mut buf).await
            && n > 0
        {
            let mut t = tail.stderr.lock().unwrap();
            t.push_str(&String::from_utf8_lossy(&buf[..n]));
            if t.len() > STDERR_TAIL {
                let cut = t.len() - STDERR_TAIL;
                let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                t.drain(..cut);
            }
        }
    });
    let (tx, events) = mpsc::unbounded_channel();
    let (req_tx, requests) = mpsc::unbounded_channel();
    let reader = tokio::spawn(read_loop(stdout, shared.clone(), tx, req_tx));
    let mut conn = Conn {
        init: Value::Null,
        stdin: tokio::sync::Mutex::new(child.stdin.take()),
        child,
        shared,
        next_id: AtomicU64::new(1),
        events,
        requests,
        reader,
    };
    conn.init = conn
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "agent-talk", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
    Ok(conn)
}

impl Conn {
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Write a request and return its response without waiting for it, for requests
    /// that resolve late (`session/prompt` answers at the end of the turn).
    pub async fn submit(&self, method: &str, params: Value) -> Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.shared.pending.lock().unwrap();
            if p.closed.is_some() {
                drop(p);
                return Err(self.shared.closed());
            }
            p.waiters.insert(id, tx);
        }
        // The guard owns the waiter from here: a failed or cancelled write removes it.
        let response = Response {
            id,
            rx,
            shared: self.shared.clone(),
        };
        tracing::debug!(id, method, %params, "send");
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        Ok(response)
    }

    /// Send a request and wait for its response, at most `RPC_TIMEOUT`; after that the
    /// request is forgotten.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let response = self.submit(method, params).await?;
        match tokio::time::timeout(RPC_TIMEOUT, response).await {
            Ok(r) => r,
            Err(_) => Err(Error::new(
                ErrorCode::Timeout,
                format!("no response to {method} within {}s", RPC_TIMEOUT.as_secs()),
            )),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    pub async fn respond(&self, id: &Value, result: Value) -> Result<()> {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
    }

    pub async fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        self.write(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
            .await
    }

    /// Next event, server requests first; `None` once the child is gone and both
    /// buffers are drained.
    pub async fn next_event(&mut self) -> Option<Event> {
        tokio::select! {
            biased;
            Some(r) = self.requests.recv() => Some(Event::Request(r)),
            ev = self.events.recv() => ev,
        }
    }

    /// Drop the notifications buffered so far (the history replay of `session/load`,
    /// which arrives before its response).
    pub fn discard_notifications(&mut self) {
        while self.events.try_recv().is_ok() {}
    }

    /// The events buffered right now, server requests first, as `next_event` orders them.
    /// The reader pushes a turn's last notifications before it resolves the `session/prompt`
    /// response, so a watcher that took the response drains them before it ends the turn.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(r) = self.requests.try_recv() {
            out.push(Event::Request(r));
        }
        while let Ok(ev) = self.events.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Close stdin, which ends the child (Grok in direct mode exits within about two
    /// seconds, Devin within 0.1 s, and a running turn dies with it), and reap it; killed
    /// after 10 s.
    pub async fn close(mut self) {
        drop(self.stdin.lock().await.take());
        if tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }

    async fn write(&self, msg: Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let Some(w) = stdin.as_mut() else {
            let agent = self.shared.agent;
            return Err(Error::new(
                ErrorCode::Transport,
                format!("{agent} stdin closed"),
            ));
        };
        let mut line = msg.to_string();
        line.push('\n');
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| Error::transport(format!("write to {}", self.shared.agent), e))
    }
}

/// Held by unit tests that spawn a fake agent, and by the test that drops a listening
/// socket and expects it refused. macOS has no atomic `SOCK_CLOEXEC`: std creates the
/// socket and then sets `FD_CLOEXEC`, and a child forked by another test thread in between
/// inherits the listener, which then survives the drop (seen about one run in five).
#[cfg(test)]
pub static SPAWN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<Event>,
    requests: mpsc::UnboundedSender<ServerRequest>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let reason = loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break format!("{} exited", shared.agent),
            Err(e) => break e.to_string(),
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Incoming = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("undecodable line from {}: {e}", shared.agent);
                continue;
            }
        };
        match (msg.id, msg.method) {
            (Some(id), Some(method)) => {
                tracing::info!(%id, method, "server request");
                let _ = requests.send(ServerRequest {
                    id,
                    method,
                    params: msg.params,
                });
            }
            (None, Some(method)) => {
                tracing::trace!(method, "notification");
                let _ = events.send(Event::Notification {
                    method,
                    params: msg.params,
                });
            }
            (Some(id), None) => {
                let Some(id) = id.as_u64() else { continue };
                let waiter = shared.pending.lock().unwrap().waiters.remove(&id);
                if let Some(w) = waiter {
                    let _ = w.send(match msg.error {
                        Some(e) => Err(agent_error(e)),
                        None => Ok(msg.result.unwrap_or(Value::Null)),
                    });
                }
            }
            (None, None) => {}
        }
    };
    tracing::debug!(reason, "reader stopped");
    let mut p = shared.pending.lock().unwrap();
    p.closed = Some(reason);
    // Dropping the waiters wakes their requests with a closed-connection error.
    p.waiters.clear();
}
