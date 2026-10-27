//! WebSocket JSON-RPC over the daemon's Unix control socket.
//!
//! One reader task dispatches three kinds of frames separately: responses to our
//! requests (by id), notifications (bounded buffer), and server-to-client requests
//! (their own unbounded buffer, never dropped; their ids overlap with ours). The
//! reader never answers server requests; the adapter decides. Writes go through
//! one mutex-guarded sink.

use super::protocol::{CursorPage, Incoming, classify_error};
use crate::model::{AgentError, Error, ErrorCode, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{WebSocketStream, client_async};

type Sink = SplitSink<WebSocketStream<UnixStream>, WsMessage>;
type Waiter = oneshot::Sender<std::result::Result<Value, AgentError>>;

/// Notifications retained while the command is busy elsewhere (e.g. awaiting an RPC).
const EVENT_BUFFER: usize = 4096;
const RPC_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `close` waits for the WebSocket close handshake.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum Event {
    Notification { method: String, params: Value },
    Request(ServerRequest),
}

/// A server-to-client request awaiting an answer (possibly a replay after resume).
#[derive(Debug)]
pub struct ServerRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

#[derive(Default)]
struct Pending {
    closed: Option<String>,
    waiters: HashMap<u64, Waiter>,
}

pub struct Conn {
    sink: tokio::sync::Mutex<Sink>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    events: mpsc::Receiver<Event>,
    requests: mpsc::UnboundedReceiver<ServerRequest>,
    overflow: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Connect, upgrade to WebSocket and run the initialize handshake.
pub async fn connect(socket: &Path) -> Result<Conn> {
    let no_daemon = || {
        Error::new(
            ErrorCode::NoDaemon,
            format!(
                "Codex daemon socket {} is absent or refusing connections; start it with `codex app-server daemon start`",
                socket.display()
            ),
        )
    };
    // A sandbox that blocks local network access hides the socket from stat and refuses
    // the connect with EPERM; that is not a missing daemon.
    let sandboxed = |e: std::io::Error| {
        Error::new(
            ErrorCode::Transport,
            format!(
                "cannot reach the Codex daemon socket {}: {e}. A sandbox that blocks local network access fails this way; agent-talk needs to reach the daemon's socket",
                socket.display()
            ),
        )
    };
    match std::fs::metadata(socket) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(sandboxed(e)),
        Err(_) => return Err(no_daemon()),
    }
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => no_daemon(),
            std::io::ErrorKind::PermissionDenied => sandboxed(e),
            _ => Error::transport(format!("connect {}", socket.display()), e),
        })?;
    let (ws, _) = client_async("ws://localhost/", stream)
        .await
        .map_err(|e| Error::transport("websocket upgrade", e))?;
    let (sink, stream) = ws.split();
    let sink = tokio::sync::Mutex::new(sink);
    let pending = Arc::new(Mutex::new(Pending::default()));
    let overflow = Arc::new(AtomicBool::new(false));
    let (tx, events) = mpsc::channel(EVENT_BUFFER);
    let (req_tx, requests) = mpsc::unbounded_channel();
    let reader = tokio::spawn(read_loop(
        stream,
        Arc::clone(&pending),
        tx,
        req_tx,
        Arc::clone(&overflow),
    ));
    let conn = Conn {
        sink,
        pending,
        next_id: AtomicU64::new(1),
        events,
        requests,
        overflow,
        reader,
    };
    conn.request(
        "initialize",
        json!({
            "clientInfo": {
                "name": "agent-talk",
                "title": "agent-talk",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"experimentalApi": true},
        }),
    )
    .await?;
    conn.write(json!({"method": "initialized", "params": {}}))
        .await?;
    Ok(conn)
}

impl Conn {
    /// Send a request and wait for its response. Agent errors keep code/message/data.
    pub async fn request(&self, method: &str, params: impl Serialize) -> Result<Value> {
        let params = serde_json::to_value(params)
            .map_err(|e| Error::transport(format!("encode {method} params"), e))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().expect("lock poisoned");
            if let Some(reason) = &p.closed {
                return Err(closed(reason));
            }
            p.waiters.insert(id, tx);
        }
        tracing::debug!(id, method, %params, "send");
        self.write(json!({"id": id, "method": method, "params": params}))
            .await?;
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(Error::from_agent(classify_error(&e), e)),
            Ok(Err(_)) => {
                let reason = self.pending.lock().expect("lock poisoned").closed.clone();
                Err(closed(reason.as_deref().unwrap_or("connection closed")))
            }
            Err(_) => {
                self.pending
                    .lock()
                    .expect("lock poisoned")
                    .waiters
                    .remove(&id);
                Err(Error::new(
                    ErrorCode::Timeout,
                    format!("no response to {method} within {}s", RPC_TIMEOUT.as_secs()),
                ))
            }
        }
    }

    /// `request` plus decoding of the result into a narrow struct.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        let v = self.request(method, params).await?;
        decode(method, &v)
    }

    /// A cursor-paged list method, read page by page starting from `params`.
    pub fn pages<'c>(&'c self, method: &'c str, params: Value) -> Pages<'c> {
        Pages {
            conn: self,
            method,
            params: Some(params),
        }
    }

    /// Next buffered event, server requests first; `None` once the connection is
    /// gone and both buffers are drained.
    pub async fn next_event(&mut self) -> Option<Event> {
        tokio::select! {
            biased;
            Some(r) = self.requests.recv() => Some(Event::Request(r)),
            ev = self.events.recv() => ev,
        }
    }

    /// Server requests already buffered, without waiting.
    pub fn drain_requests(&mut self) -> Vec<ServerRequest> {
        let mut out = Vec::new();
        while let Ok(r) = self.requests.try_recv() {
            out.push(r);
        }
        out
    }

    /// Answer a server request.
    pub async fn respond(&self, id: &Value, result: Value) -> Result<()> {
        self.write(json!({"id": id, "result": result})).await
    }

    /// True once if notifications were dropped because the buffer was full.
    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::Relaxed)
    }

    /// Close the WebSocket, waiting at most `CLOSE_TIMEOUT` for the close handshake.
    pub async fn close(self) {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
            let _ = self.sink.lock().await.close().await;
        })
        .await;
    }

    async fn write(&self, msg: Value) -> Result<()> {
        self.sink
            .lock()
            .await
            .send(WsMessage::text(msg.to_string()))
            .await
            .map_err(|e| Error::transport("write", e))
    }
}

/// The pages of a list method (`CursorPage`): each request passes the previous page's
/// `nextCursor` as `cursor`.
pub struct Pages<'c> {
    conn: &'c Conn,
    method: &'c str,
    /// The next request's params; `None` after the last page.
    params: Option<Value>,
}

impl Pages<'_> {
    /// The next page's entries; `None` after the last page.
    pub async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<Vec<T>>> {
        let Some(params) = &mut self.params else {
            return Ok(None);
        };
        let page: CursorPage<T> = self.conn.call(self.method, &*params).await?;
        match page.next_cursor {
            Some(c) => params["cursor"] = json!(c),
            None => self.params = None,
        }
        Ok(Some(page.data))
    }
}

pub fn decode<T: DeserializeOwned>(what: &str, v: &Value) -> Result<T> {
    T::deserialize(v).map_err(|e| Error::transport(format!("unexpected {what} payload"), e))
}

fn closed(reason: &str) -> Error {
    Error::transport("daemon connection lost", reason)
}

async fn read_loop(
    mut stream: SplitStream<WebSocketStream<UnixStream>>,
    pending: Arc<Mutex<Pending>>,
    events: mpsc::Sender<Event>,
    requests: mpsc::UnboundedSender<ServerRequest>,
    overflow: Arc<AtomicBool>,
) {
    let reason = loop {
        let text = match stream.next().await {
            Some(Ok(WsMessage::Text(t))) => t,
            Some(Ok(WsMessage::Close(_))) | None => break "closed by daemon".to_string(),
            Some(Ok(_)) => continue,
            Some(Err(e)) => break e.to_string(),
        };
        let msg: Incoming = match serde_json::from_str(text.as_str()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("undecodable frame: {e}");
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
                let ev = Event::Notification {
                    method,
                    params: msg.params,
                };
                if events.try_send(ev).is_err() {
                    overflow.store(true, Ordering::Relaxed);
                }
            }
            (Some(id), None) => {
                let Some(id) = id.as_u64() else { continue };
                let waiter = pending.lock().expect("lock poisoned").waiters.remove(&id);
                if let Some(w) = waiter {
                    let r = match msg.error {
                        Some(e) => Err(e),
                        None => Ok(msg.result.unwrap_or(Value::Null)),
                    };
                    let _ = w.send(r);
                }
            }
            (None, None) => {}
        }
    };
    tracing::debug!(reason, "reader stopped");
    let mut p = pending.lock().expect("lock poisoned");
    p.closed = Some(reason);
    // Dropping the waiters wakes their requests with a closed-connection error.
    p.waiters.clear();
}
