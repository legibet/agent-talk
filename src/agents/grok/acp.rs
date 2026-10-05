//! A `grok agent … stdio` child speaking ACP: line-delimited JSON-RPC 2.0 on its
//! stdin/stdout.
//!
//! One reader task dispatches three kinds of frames separately, as in the Codex
//! transport: responses to our requests (by id), notifications (unbounded: one child
//! serves one command, so the buffer holds at most one session's replay and turn), and
//! server-to-client requests such as `session/request_permission` (their ids overlap
//! with ours). The reader never answers server requests; the adapter decides.

use crate::agents::agent_cmd;
use crate::model::{AgentError, Error, ErrorCode, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const RPC_TIMEOUT: Duration = Duration::from_secs(120);
/// Bytes of the child's stderr kept for error messages.
const STDERR_TAIL: usize = 4096;

/// A response as the reader delivers it: the result, or the agent's error object.
pub type Reply = std::result::Result<Value, AgentError>;

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
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<AgentError>,
}

#[derive(Default)]
struct Pending {
    closed: Option<String>,
    waiters: HashMap<u64, oneshot::Sender<Reply>>,
}

/// How the child is started.
pub struct Spawn<'a> {
    /// `--leader` (a proxy into the live leader) instead of `--no-leader`.
    pub leader: bool,
    /// `--leader-socket`, passed only when `GROK_LEADER_SOCKET` is set.
    pub leader_socket: Option<&'a Path>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
}

pub struct Conn {
    child: Child,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Arc<Mutex<Pending>>,
    stderr: Arc<Mutex<String>>,
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

/// Spawn the child in `cwd` and run the ACP `initialize` handshake.
pub async fn spawn(cwd: &str, s: &Spawn<'_>) -> Result<Conn> {
    // GROK_HOME and GROK_LEADER_SOCKET are honoured by the child as by agent-talk.
    let mut cmd = agent_cmd("grok");
    // `grok agent` has no --no-auto-update (only `grok agent leader` does); the
    // binary's GROK_DISABLE_AUTOUPDATER is set instead (its effect on a stdio agent
    // was not observed).
    cmd.env("GROK_DISABLE_AUTOUPDATER", "1").arg("agent");
    cmd.arg(if s.leader { "--leader" } else { "--no-leader" });
    if let Some(p) = s.leader_socket {
        cmd.arg("--leader-socket").arg(p);
    }
    if let Some(m) = s.model {
        cmd.args(["-m", m]);
    }
    if let Some(e) = s.effort {
        cmd.args(["--reasoning-effort", e]);
    }
    // Its own process group, so that Ctrl-C reaches agent-talk, which then ends the
    // turn itself; kill_on_drop for a command that unwinds without closing.
    let mut child = cmd
        .arg("stdio")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::new(ErrorCode::Transport, format!("spawn grok: {e}")))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut err_pipe = child.stderr.take().expect("stderr is piped");
    let stderr = Arc::new(Mutex::new(String::new()));
    let tail = stderr.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        while let Ok(n) = err_pipe.read(&mut buf).await
            && n > 0
        {
            let mut t = tail.lock().unwrap();
            t.push_str(&String::from_utf8_lossy(&buf[..n]));
            if t.len() > STDERR_TAIL {
                let cut = t.len() - STDERR_TAIL;
                let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                t.drain(..cut);
            }
        }
    });
    let pending = Arc::new(Mutex::new(Pending::default()));
    let (tx, events) = mpsc::unbounded_channel();
    let (req_tx, requests) = mpsc::unbounded_channel();
    let reader = tokio::spawn(read_loop(stdout, pending.clone(), tx, req_tx));
    let conn = Conn {
        stdin: tokio::sync::Mutex::new(child.stdin.take()),
        child,
        pending,
        stderr,
        next_id: AtomicU64::new(1),
        events,
        requests,
        reader,
    };
    conn.request(
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

    /// Write a request; the receiver yields its response. For requests that resolve
    /// late (`session/prompt` answers at the end of the turn).
    pub async fn start(&self, method: &str, params: Value) -> Result<oneshot::Receiver<Reply>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap();
            if let Some(reason) = &p.closed {
                return Err(self.closed(reason));
            }
            p.waiters.insert(id, tx);
        }
        tracing::debug!(id, method, %params, "send");
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        Ok(rx)
    }

    /// Send a request and wait for its response. Agent errors keep code/message/data.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let rx = self.start(method, params).await?;
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(r) => self.reply(r),
            Err(_) => Err(Error::new(
                ErrorCode::Timeout,
                format!("no response to {method} within {}s", RPC_TIMEOUT.as_secs()),
            )),
        }
    }

    /// A response as delivered to the receiver of `start`.
    pub fn reply(&self, r: std::result::Result<Reply, oneshot::error::RecvError>) -> Result<Value> {
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(Error::from_agent(
                if e.code == -32601 {
                    ErrorCode::Unsupported
                } else {
                    ErrorCode::Precondition
                },
                e,
            )),
            Err(_) => {
                let reason = self.pending.lock().unwrap().closed.clone();
                Err(self.closed(reason.as_deref().unwrap_or("connection closed")))
            }
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

    /// Close stdin, which ends the child (in direct mode it exits within about two
    /// seconds and a running turn dies with it), and reap it; killed after 10 s.
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
            return Err(Error::new(ErrorCode::Transport, "grok agent stdin closed"));
        };
        let mut line = msg.to_string();
        line.push('\n');
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| Error::new(ErrorCode::Transport, format!("write to grok agent: {e}")))
    }

    fn closed(&self, reason: &str) -> Error {
        let stderr = self.stderr.lock().unwrap().trim().to_string();
        let mut msg = format!("grok agent connection lost: {reason}");
        if !stderr.is_empty() {
            msg.push_str(&format!("; stderr: {stderr}"));
        }
        Error::new(ErrorCode::Transport, msg)
    }
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<Pending>>,
    events: mpsc::UnboundedSender<Event>,
    requests: mpsc::UnboundedSender<ServerRequest>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let reason = loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break "grok agent exited".to_string(),
            Err(e) => break e.to_string(),
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Incoming = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("undecodable line from grok agent: {e}");
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
                let waiter = pending.lock().unwrap().waiters.remove(&id);
                if let Some(w) = waiter {
                    let _ = w.send(match msg.error {
                        Some(e) => Err(e),
                        None => Ok(msg.result.unwrap_or(Value::Null)),
                    });
                }
            }
            (None, None) => {}
        }
    };
    tracing::debug!(reason, "reader stopped");
    let mut p = pending.lock().unwrap();
    p.closed = Some(reason);
    // Dropping the waiters wakes their requests with a closed-connection error.
    p.waiters.clear();
}
