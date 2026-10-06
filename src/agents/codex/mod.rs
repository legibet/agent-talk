//! Codex adapter: talks to the shared app-server daemon over its control socket.

pub mod protocol;
pub mod transport;

use self::protocol::*;
use self::transport::{Conn, Event, ServerRequest, decode};
use super::{
    Agent, ApprovalPolicy, Caps, Check, ListFilter, Operation, ReadPage, ReadRange, SendRequest,
    StartRequest, WaitTarget, approval_policy, bounded, cli_version, full_access, record, reject,
    resolve, settle, strip_provenance, wait_receipt,
};
use crate::model::{
    self, Approval, Error, ErrorCode, Message, Observations, Outcome, Page, Result, Session,
};
use crate::store::{NewIntent, Store};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;

const DUMMY_THREAD: &str = "00000000-0000-0000-0000-000000000000";
/// Seconds `subscribe` keeps retrying while a just-started thread's rollout is empty.
const RESUME_RETRIES: u32 = 10;
/// How long `send` waits for a `turn/started` after `thread/queue/add` on an idle thread
/// before it starts the submission itself (`start_if_dormant`).
const DORMANT_WINDOW: Duration = Duration::from_secs(2);

pub struct Codex<'a> {
    store: &'a Store,
    socket: PathBuf,
}

/// What a command is waiting for. A queued submission has no turn id until the
/// daemon consumes it, so it is matched by its client message id first.
enum Target {
    Turn(String),
    Client {
        client_id: String,
        receipt_id: String,
        /// Known to be sitting in the thread's queue.
        queued: bool,
    },
}

impl Target {
    /// Progress reported when the outcome is not known yet.
    fn progress(&self) -> &'static str {
        match self {
            Target::Turn(_) => "running",
            Target::Client { queued: true, .. } => "pending",
            Target::Client { queued: false, .. } => "unknown",
        }
    }
}

enum Lookup {
    Done(model::Turn),
    Running,
    Absent,
}

/// Per-command observation state on one connection.
struct Watch<'t> {
    thread_id: &'t str,
    policy: ApprovalPolicy,
    approvals: Vec<Approval>,
}

impl<'t> Watch<'t> {
    fn new(thread_id: &'t str, policy: ApprovalPolicy) -> Self {
        Watch {
            thread_id,
            policy,
            approvals: Vec::new(),
        }
    }
}

fn handle(thread_id: &str) -> String {
    format!("codex:{thread_id}")
}

/// History of a thread created moments ago is not readable yet. Two transient shapes:
/// `-32603 "… rollout at <path> is empty"` (`thread/resume`, `thread/turns/list`) while
/// the rollout file has no line, and `-32601 "list_turns is not supported yet"` from
/// `thread/turns/list` while the thread has no row in the daemon's state DB (its
/// paginated reads check that row first). Other -32603s are thread-store failures and
/// other -32601s are really unsupported methods.
fn history_not_ready(e: &Error) -> bool {
    e.agent_error.as_ref().is_some_and(|v| {
        (v.code == -32603 && v.message.ends_with(" is empty"))
            || (v.code == -32601 && v.message == "list_turns is not supported yet")
    })
}

fn approval_summary(method: &str, params: &Value) -> String {
    match method {
        "item/commandExecution/requestApproval" | "execCommandApproval" => params["command"]
            .as_str()
            .map(|c| format!("run: {c}"))
            .unwrap_or_else(|| "run a command".into()),
        "item/fileChange/requestApproval" | "applyPatchApproval" => params["reason"]
            .as_str()
            .map(|r| format!("file change: {r}"))
            .unwrap_or_else(|| "file change".into()),
        "item/permissions/requestApproval" => "additional permissions".into(),
        "item/tool/requestUserInput" => "user input requested".into(),
        "mcpServer/elicitation/request" => format!(
            "MCP elicitation from {}",
            params["serverName"].as_str().unwrap_or("?")
        ),
        other => other.to_string(),
    }
}

impl<'a> Codex<'a> {
    pub fn new(store: &'a Store) -> Self {
        let socket = std::env::home_dir()
            .unwrap_or_default()
            .join(".codex/app-server-control/app-server-control.sock");
        Codex { store, socket }
    }

    async fn connect(&self) -> Result<Conn> {
        transport::connect(&self.socket).await
    }

    /// Subscribe to the thread's events (`thread/resume`, which also loads a not-loaded
    /// thread). For about a second after `new` the thread's history is not readable yet
    /// and the daemon cannot resume it; that is retried once a second, at most
    /// `RESUME_RETRIES` times, within the caller's deadline. A thread another app-server
    /// process holds is `E_FOREIGN_LIVE` (`resume_error`).
    async fn subscribe(&self, conn: &Conn, thread_id: &str) -> Result<ThreadResumeResponse> {
        let mut params = resume_latest_turn(thread_id);
        if full_access(self.store, &handle(thread_id))? {
            // A thread that unloaded comes back with the user's sandbox; the approval
            // policy survives (observed on codex 0.160.1, DESIGN.md §6.1).
            params["sandbox"] = json!("danger-full-access");
        }
        for _ in 0..RESUME_RETRIES {
            match conn.call("thread/resume", params.clone()).await {
                Err(e) if history_not_ready(&e) => tokio::time::sleep(Duration::from_secs(1)).await,
                r => return r.map_err(resume_error),
            }
        }
        conn.call("thread/resume", params)
            .await
            .map_err(resume_error)
    }

    /// Start a submission `thread/queue/add` left dormant. On an idle thread the daemon
    /// starts the queued turn inside the add request (`turn/started` follows within
    /// milliseconds), except when the thread's last turn was aborted by an interrupt (or a
    /// budget limit): that thread stays `Interrupted`, also across a reload, and neither the
    /// add nor the daemon's 10 s queue watcher starts anything until some turn starts
    /// (codex-rs ext/queue `wake_if_loaded`, `on_thread_idle`). The rest, including the two
    /// `thread/queue/start` refusals that mean nothing is dormant, is in DESIGN.md §6.1.
    async fn start_if_dormant(
        &self,
        conn: &mut Conn,
        w: &mut Watch<'_>,
        queue_id: &str,
        receipt_id: &str,
    ) -> Result<()> {
        let end = Instant::now() + DORMANT_WINDOW;
        loop {
            match tokio::time::timeout_at(end, conn.next_event()).await {
                Err(_) => break,
                Ok(None) => {
                    return Err(Error::new(
                        ErrorCode::Transport,
                        "daemon connection lost after queueing; outcome unknown",
                    ));
                }
                Ok(Some(Event::Request(r))) => self.on_request(conn, w, r).await?,
                Ok(Some(Event::Notification { method, params })) => {
                    if method == "turn/started" && params["threadId"] == w.thread_id {
                        return Ok(());
                    }
                }
            }
        }
        let params = json!({"threadId": w.thread_id, "queuedSubmissionId": queue_id});
        match conn
            .call::<ThreadQueueStartResponse>("thread/queue/start", params)
            .await
        {
            Ok(started) => self
                .store
                .accept(receipt_id, None, Some(&started.turn.id), None),
            Err(e)
                if e.agent_error.as_ref().is_some_and(|v| {
                    v.code == -32600
                        && (v.message.starts_with("queued submission not found: ")
                            || v.message == "thread already has an active or pending turn")
                }) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// A server request reached this connection (live or replayed after resume; a
    /// replay of one already seen is ignored). It is recorded as an approval: under
    /// `deny`, approval requests get `deny_response` and become `declined`; everything
    /// else stays unanswered and `pending`.
    async fn on_request(&self, conn: &Conn, w: &mut Watch<'_>, r: ServerRequest) -> Result<()> {
        let ServerRequest { id, method, params } = r;
        let item_id = params["itemId"].as_str().map(String::from);
        if w.approvals
            .iter()
            .any(|a| a.request_id == id && a.item_id == item_id)
        {
            return Ok(());
        }
        let outcome = match (w.policy, deny_response(&method)) {
            (ApprovalPolicy::Deny, Some(body)) => {
                conn.respond(&id, body).await?;
                "declined"
            }
            _ => "pending",
        };
        let a = Approval {
            handle: handle(params["threadId"].as_str().unwrap_or(w.thread_id)),
            turn_id: params["turnId"].as_str().map(String::from),
            item_id,
            request_id: id,
            summary: approval_summary(&method, &params),
            kind: method,
            outcome,
            raw: params,
        };
        record(self.store, &a);
        w.approvals.push(a);
        Ok(())
    }

    /// Handle server requests still buffered on the socket (`on_request`), close the
    /// connection, then settle the receipt with `progress` as the unknown-outcome state.
    async fn finish(
        &self,
        mut conn: Conn,
        res: Result<Option<model::Turn>>,
        receipt_id: Option<&str>,
        mut w: Watch<'_>,
        progress: &'static str,
    ) -> Result<Outcome> {
        for r in conn.drain_requests() {
            if let Err(e) = self.on_request(&conn, &mut w, r).await {
                tracing::warn!("{e}");
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.close()).await;
        settle(
            self.store,
            handle(w.thread_id),
            res,
            receipt_id,
            w.approvals,
            progress,
        )
    }

    /// Search persisted history (ascending, full items) for the target. A client
    /// id found in history is resolved to its turn.
    async fn lookup(&self, conn: &Conn, thread_id: &str, target: &mut Target) -> Result<Lookup> {
        let mut cursor: Option<String> = None;
        loop {
            let mut params = json!({
                "threadId": thread_id,
                "limit": 50,
                "sortDirection": "asc",
                "itemsView": "full",
            });
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            let page: TurnsPage = match conn.call("thread/turns/list", params).await {
                Ok(p) => p,
                Err(e) if history_not_ready(&e) => return Ok(Lookup::Absent),
                Err(e) => return Err(e),
            };
            for raw in page.data {
                let turn: protocol::Turn = decode("turn", &raw)?;
                let found = match &*target {
                    Target::Turn(id) => *id == turn.id,
                    Target::Client {
                        client_id,
                        receipt_id,
                        ..
                    } => {
                        let item = turn.items.iter().find_map(|i| match Item::parse(i) {
                            Item::User(u) if u.client_id.as_deref() == Some(client_id) => {
                                Some(u.id)
                            }
                            _ => None,
                        });
                        match item {
                            Some(item_id) => {
                                self.store.accept(
                                    receipt_id,
                                    None,
                                    Some(&turn.id),
                                    Some(&item_id),
                                )?;
                                true
                            }
                            None => false,
                        }
                    }
                };
                if found {
                    *target = Target::Turn(turn.id.clone());
                    return Ok(if turn.status == "inProgress" {
                        Lookup::Running
                    } else {
                        Lookup::Done(to_turn(thread_id, turn))
                    });
                }
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return Ok(Lookup::Absent),
            }
        }
    }

    /// The queue id of a client message still waiting in the thread's queue.
    async fn queued(
        &self,
        conn: &Conn,
        thread_id: &str,
        client_id: &str,
    ) -> Result<Option<String>> {
        let mut cursor: Option<String> = None;
        loop {
            let mut params = json!({"threadId": thread_id});
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            let page: ThreadQueueListResponse = conn.call("thread/queue/list", params).await?;
            if let Some(e) = page
                .data
                .into_iter()
                .find(|e| e.client_user_message_id.as_deref() == Some(client_id))
            {
                return Ok(Some(e.id));
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return Ok(None),
            }
        }
    }

    /// Consume events until the target turn completes. The connection must
    /// already be subscribed to the thread.
    async fn observe(
        &self,
        conn: &mut Conn,
        w: &mut Watch<'_>,
        target: &mut Target,
    ) -> Result<model::Turn> {
        let thread_id = w.thread_id;
        loop {
            if conn.take_overflow() {
                tracing::warn!("event buffer overflowed; reconciling from history");
                if let Lookup::Done(t) = self.lookup(conn, thread_id, target).await? {
                    return Ok(t);
                }
            }
            let Some(ev) = conn.next_event().await else {
                return Err(Error::new(
                    ErrorCode::Transport,
                    "daemon connection lost while waiting; outcome unknown",
                ));
            };
            let (method, params) = match ev {
                Event::Request(r) => {
                    self.on_request(conn, w, r).await?;
                    continue;
                }
                Event::Notification { method, params } => (method, params),
            };
            if params["threadId"] != thread_id {
                continue;
            }
            if method == "serverRequest/resolved" {
                // Someone answered; a pending approval is no longer pending.
                resolve(self.store, &mut w.approvals, &params["requestId"]);
                continue;
            }
            match (method.as_str(), &*target) {
                (
                    "item/started" | "item/completed",
                    Target::Client {
                        client_id,
                        receipt_id,
                        ..
                    },
                ) => {
                    let n: ItemNotification = decode(&method, &params)?;
                    if let Item::User(u) = Item::parse(&n.item)
                        && u.client_id.as_deref() == Some(client_id)
                    {
                        self.store
                            .accept(receipt_id, None, Some(&n.turn_id), Some(&u.id))?;
                        *target = Target::Turn(n.turn_id);
                    }
                }
                ("turn/completed", Target::Turn(turn_id)) => {
                    let n: TurnCompletedNotification = decode(&method, &params)?;
                    if n.turn["id"] == turn_id.as_str() {
                        let turn = decode("turn", &n.turn)?;
                        return Ok(to_turn(&n.thread_id, turn));
                    }
                }
                _ => {}
            }
        }
    }

    /// Normalized messages of Codex turns (full items), in the given order.
    fn messages(&self, turns: &[Value]) -> Result<Vec<Message>> {
        let at = |turn: &Value, key: &str| {
            turn[key]
                .as_i64()
                .and_then(|s| jiff::Timestamp::from_second(s).ok())
                .map(|t| t.to_string())
        };
        let mut messages = Vec::new();
        for turn in turns {
            let turn_id = turn["id"].as_str().unwrap_or_default();
            for item in turn["items"].as_array().into_iter().flatten() {
                let message = match Item::parse(item) {
                    Item::User(u) => Message {
                        turn_id: turn_id.into(),
                        role: "user",
                        phase: "other",
                        text: u.text(),
                        from: match &u.client_id {
                            Some(c) => self.store.sender_of(c)?,
                            None => None,
                        },
                        timestamp: at(turn, "startedAt"),
                        item_id: u.id,
                    },
                    Item::Agent(a) => Message {
                        turn_id: turn_id.into(),
                        item_id: a.id,
                        role: "assistant",
                        phase: match a.phase.as_deref() {
                            Some("final_answer") => "final",
                            Some("commentary") => "commentary",
                            _ => "other",
                        },
                        text: a.text,
                        from: None,
                        timestamp: at(turn, "completedAt").or_else(|| at(turn, "startedAt")),
                    },
                    Item::Other => continue,
                };
                messages.push(message);
            }
        }
        Ok(messages)
    }

    /// Whether the daemon knows `method`, probed on a dummy thread: only a missing
    /// capability makes it unavailable; any other refusal means the method exists.
    async fn probe(&self, conn: &Conn, method: &str, params: Value) -> Check {
        match conn.request(method, params).await {
            Err(e) if matches!(e.code, ErrorCode::CapMissing | ErrorCode::Unsupported) => {
                Err(e.message)
            }
            _ => Ok(()),
        }
    }
}

/// Build the normalized turn from a decoded Codex turn (summary or full items).
fn to_turn(thread_id: &str, t: protocol::Turn) -> model::Turn {
    let agents: Vec<AgentMessage> = t
        .items
        .iter()
        .filter_map(|i| match Item::parse(i) {
            Item::Agent(a) => Some(a),
            _ => None,
        })
        .collect();
    let final_text = agents
        .iter()
        .rev()
        .find(|a| a.phase.as_deref() == Some("final_answer"))
        .or(agents.last())
        .map(|a| a.text.clone());
    model::Turn {
        handle: handle(thread_id),
        turn_id: t.id,
        status: match t.status.as_str() {
            "completed" => "completed",
            "failed" => "failed",
            "interrupted" => "interrupted",
            _ => "unknown",
        },
        error: t.error.filter(|e| !e.is_null()),
        final_text,
        duration_ms: t.duration_ms,
        basis: None,
    }
}

async fn run(program: &str, args: &[&str]) -> std::result::Result<String, String> {
    match Command::new(program).args(args).output().await {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).trim().into()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().into()),
        Err(e) => Err(e.to_string()),
    }
}

fn session_state(s: &ThreadStatus) -> &'static str {
    match s {
        ThreadStatus::Idle => "idle",
        ThreadStatus::Active { active_flags } if !active_flags.is_empty() => "waiting",
        ThreadStatus::Active { .. } => "running",
        _ => "unknown",
    }
}

impl Agent for Codex<'_> {
    async fn caps(&self) -> Caps {
        let version = cli_version("codex").await.ok();
        let (shared, daemon, queue, steer) = match self.connect().await {
            Ok(conn) => {
                let queue = self
                    .probe(
                        &conn,
                        "thread/queue/list",
                        json!({"threadId": DUMMY_THREAD}),
                    )
                    .await;
                let steer = self
                    .probe(
                        &conn,
                        "turn/steer",
                        json!({
                            "threadId": DUMMY_THREAD,
                            "expectedTurnId": DUMMY_THREAD,
                            "input": text_input(""),
                            "clientUserMessageId": DUMMY_THREAD,
                        }),
                    )
                    .await;
                conn.close().await;
                // The daemon can be older than the CLI until it is restarted.
                let server = run("codex", &["app-server", "daemon", "version"])
                    .await
                    .ok()
                    .and_then(|out| serde_json::from_str::<Value>(&out).ok())
                    .and_then(|v| v["appServerVersion"].as_str().map(String::from));
                let shared = match server {
                    Some(v) => format!("daemon running (app-server {v})"),
                    None => "daemon running".into(),
                };
                (shared, Ok(()), queue, steer)
            }
            Err(e) => {
                let shared = match e.code {
                    ErrorCode::NoDaemon => "daemon not running",
                    _ => "daemon not reachable",
                };
                let daemon: Check = Err(e.message);
                (shared.into(), daemon.clone(), daemon.clone(), daemon)
            }
        };
        Caps {
            agent: "codex",
            version,
            shared: Some(shared),
            operations: vec![
                Operation::new("ls", &daemon),
                Operation::new("new", &daemon),
                Operation::new("send", &queue),
                Operation::new("read", &daemon),
                Operation::new("wait", &daemon),
                Operation::new("steer", &steer),
                Operation::new("name", &daemon),
            ],
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        let conn = self.connect().await?;
        let mut params = json!({
            "limit": limit,
            "sourceKinds": if filter.all { ALL_SOURCE_KINDS } else { DEFAULT_SOURCE_KINDS },
        });
        if let Some(c) = cursor {
            params["cursor"] = json!(c);
        }
        if let Some(cwd) = filter.cwd {
            params["cwd"] = json!(cwd);
        }
        let threads: ThreadListResponse = conn.call("thread/list", params).await?;
        let mut loaded = HashSet::new();
        let mut loaded_cursor: Option<String> = None;
        loop {
            let mut params = json!({"limit": 200});
            if let Some(c) = &loaded_cursor {
                params["cursor"] = json!(c);
            }
            let page: ThreadLoadedListResponse = conn.call("thread/loaded/list", params).await?;
            loaded.extend(page.data);
            match page.next_cursor {
                Some(c) => loaded_cursor = Some(c),
                None => break,
            }
        }
        conn.close().await;
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for t in threads.data {
            // The default listing scans rollout files and lists a thread once per file
            // (a thread resumed into a new file has several); the first row is the newest.
            if !seen.insert(t.id.clone()) {
                continue;
            }
            let h = handle(&t.id);
            let owned = self.store.is_owned(&h)?;
            let origin = if owned {
                "agent-talk".to_string()
            } else {
                t.originator.clone().unwrap_or_else(|| match &t.source {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
            };
            items.push(Session {
                handle: h,
                agent: "codex",
                cwd: t.cwd,
                name: t.name,
                preview: t
                    .preview
                    .as_deref()
                    .map(|p| strip_provenance(p).to_string()),
                observations: Observations {
                    history: "visible",
                    loaded: if loaded.contains(&t.id) { "yes" } else { "no" },
                    origin,
                },
                state: session_state(&t.status),
                owned,
                id: t.id,
            });
        }
        Ok(Page {
            items,
            next_cursor: threads.next_cursor,
        })
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        let mut conn = self.connect().await?;
        // Nobody answers approvals on a thread agent-talk started: what needs one fails
        // and the model is told. The sandbox and model fall back to the daemon's defaults.
        let mut params = json!({"cwd": req.cwd, "approvalPolicy": "never"});
        if let Some(m) = req.model {
            params["model"] = json!(m);
        }
        if req.full_access {
            params["sandbox"] = json!("danger-full-access");
        }
        let started: ThreadStartResponse = conn.call("thread/start", params).await?;
        let thread_id = started.thread.id;
        if let Some(name) = req.name {
            // Before turn/start: right after it the rollout file is still empty and the
            // daemon cannot update thread metadata (DESIGN.md §6.1).
            conn.call::<Value>(
                "thread/name/set",
                json!({"threadId": thread_id, "name": name}),
            )
            .await?;
        }
        let handle = handle(&thread_id);
        let args = json!({
            "model": req.model,
            "effort": req.effort,
            "full_access": req.full_access,
        });
        self.store.insert_owned(&handle, req.cwd, &args)?;
        let mut w = Watch::new(&thread_id, ApprovalPolicy::Deny);
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let client_msg_id = uuid::Uuid::new_v4().to_string();
        // Set once the agent accepted the submission.
        let mut target = None;
        let work = async {
            self.store.insert_intent(&NewIntent {
                receipt_id: &receipt_id,
                handle: &handle,
                client_msg_id: &client_msg_id,
                text: req.prompt,
                delivered_text: (req.delivered != req.prompt).then_some(req.delivered),
                from: req.from,
                depth: req.depth,
            })?;
            let started: TurnStartResponse = conn
                .call(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": text_input(req.delivered),
                        "clientUserMessageId": client_msg_id,
                        // thread/start takes no effort; this one applies to the thread's
                        // later turns too.
                        "effort": req.effort,
                    }),
                )
                .await
                .map_err(|e| reject(self.store, &receipt_id, e))?;
            self.store
                .accept(&receipt_id, None, Some(&started.turn.id), None)?;
            let target = target.insert(Target::Turn(started.turn.id));
            if deadline.is_none() {
                return Ok(None);
            }
            // thread/start already subscribed this connection.
            self.observe(&mut conn, &mut w, target).await.map(Some)
        };
        let res = match deadline {
            Some(d) => bounded(Some(d), work).await,
            None => work.await,
        };
        let progress = target.as_ref().map_or("unknown", Target::progress);
        self.finish(conn, res, Some(&receipt_id), w, progress).await
    }

    async fn send(
        &self,
        thread_id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let handle = handle(thread_id);
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let client_msg_id = uuid::Uuid::new_v4().to_string();
        let mut w = Watch::new(thread_id, approval_policy(self.store, &handle)?);
        let mut conn = self.connect().await?;
        // Set once the agent accepted the submission.
        let mut target = None;
        let work = async {
            // Always resume (subscribe) before submitting, on every path:
            // - on a not-loaded thread, thread/queue/add is accepted but stays dormant
            //   (no load, no turn) until some client resumes (DESIGN.md §6.1);
            // - with --wait, no event of the resulting turn may be missed;
            // - the response carries the newest turn: whether the thread is idle (queue
            //   then checks for a dormant submission) and which turn steer targets.
            let resumed = self.subscribe(&conn, thread_id).await?;
            let newest: Option<protocol::Turn> = resumed
                .initial_turns_page
                .and_then(|p| p.data.into_iter().next())
                .map(|t| decode("turn", &t))
                .transpose()?;
            let idle = newest.as_ref().is_none_or(|t| t.status != "inProgress");
            let expected_turn = match (req.steer, newest) {
                (false, _) => None,
                (true, Some(t)) => Some(t.id),
                (true, None) => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        "thread has no turn to steer",
                    ));
                }
            };
            self.store.insert_intent(&NewIntent {
                receipt_id: &receipt_id,
                handle: &handle,
                client_msg_id: &client_msg_id,
                text: req.text,
                delivered_text: (req.delivered != req.text).then_some(req.delivered),
                from: req.from,
                depth: req.depth,
            })?;
            let target = target.insert(match expected_turn {
                None => {
                    let added: ThreadQueueAddResponse = conn
                        .call(
                            "thread/queue/add",
                            json!({
                                "threadId": thread_id,
                                "clientUserMessageId": client_msg_id,
                                "input": text_input(req.delivered),
                            }),
                        )
                        .await
                        .map_err(|e| reject(self.store, &receipt_id, e))?;
                    let queue_id = added.queued_submission.id;
                    self.store
                        .accept(&receipt_id, Some(&queue_id), None, None)?;
                    // Also without --wait, so a plain send to an interrupted thread runs:
                    // the command stays connected up to DORMANT_WINDOW longer for it.
                    if idle {
                        self.start_if_dormant(&mut conn, &mut w, &queue_id, &receipt_id)
                            .await?;
                    }
                    Target::Client {
                        client_id: client_msg_id.clone(),
                        receipt_id: receipt_id.clone(),
                        queued: true,
                    }
                }
                Some(expected) => {
                    let steered: TurnSteerResponse = conn
                        .call(
                            "turn/steer",
                            json!({
                                "threadId": thread_id,
                                "expectedTurnId": expected,
                                "input": text_input(req.delivered),
                                "clientUserMessageId": client_msg_id,
                            }),
                        )
                        .await
                        .map_err(|e| reject(self.store, &receipt_id, e))?;
                    self.store
                        .accept(&receipt_id, None, Some(&steered.turn_id), None)?;
                    Target::Turn(steered.turn_id)
                }
            });
            if deadline.is_none() {
                return Ok(None);
            }
            self.observe(&mut conn, &mut w, target).await.map(Some)
        };
        let res = match deadline {
            Some(d) => bounded(Some(d), work).await,
            None => work.await,
        };
        let progress = target.as_ref().map_or("unknown", Target::progress);
        self.finish(conn, res, Some(&receipt_id), w, progress).await
    }

    async fn read(&self, thread_id: &str, range: ReadRange) -> Result<ReadPage> {
        let conn = self.connect().await?;
        let call = |cursor: Option<String>, limit: u32, sort_direction: &'static str| {
            let conn = &conn;
            async move {
                let mut params = json!({
                    "threadId": thread_id,
                    "limit": limit,
                    "sortDirection": sort_direction,
                    "itemsView": "full",
                });
                if let Some(c) = cursor {
                    params["cursor"] = json!(c);
                }
                conn.call::<TurnsPage>("thread/turns/list", params).await
            }
        };
        let (turns, messages, next_cursor) = match range {
            ReadRange::Forward { since, limit } => {
                let page = call(since, limit, "asc").await?;
                let messages = self.messages(&page.data)?;
                (page.data, messages, page.next_cursor)
            }
            ReadRange::Tail(n) => {
                // Newest turns first until they hold n messages, then back to oldest first.
                let mut turns = Vec::new();
                let mut count = 0;
                let mut cursor = None;
                loop {
                    let page = call(cursor, 20, "desc").await?;
                    count += self.messages(&page.data)?.len();
                    turns.extend(page.data);
                    match page.next_cursor {
                        Some(c) if count < n as usize => cursor = Some(c),
                        _ => break,
                    }
                }
                turns.reverse();
                let mut messages = self.messages(&turns)?;
                let skip = messages.len().saturating_sub(n as usize);
                messages.drain(..skip);
                let first_turn = messages.first().map(|m| m.turn_id.clone());
                let start = turns
                    .iter()
                    .position(|t| t["id"].as_str() == first_turn.as_deref())
                    .unwrap_or(turns.len());
                (turns.split_off(start), messages, None)
            }
        };
        conn.close().await;
        Ok(ReadPage {
            messages: Page {
                items: messages,
                next_cursor,
            },
            raw: turns,
        })
    }

    async fn wait(
        &self,
        thread_id: &str,
        target: &WaitTarget,
        deadline: Instant,
    ) -> Result<Outcome> {
        let handle = handle(thread_id);
        let (mut target, receipt_id) = match target {
            WaitTarget::Turn(t) => (
                Target::Turn(t.clone()),
                self.store
                    .receipt_by_turn(&handle, t)?
                    .map(|r| r.receipt_id),
            ),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &handle, r)?;
                let target = match rec.turn_id {
                    Some(t) => Target::Turn(t),
                    None => Target::Client {
                        client_id: rec.client_msg_id,
                        receipt_id: r.clone(),
                        queued: false,
                    },
                };
                (target, Some(r.clone()))
            }
        };
        let mut w = Watch::new(thread_id, approval_policy(self.store, &handle)?);
        let mut conn = self.connect().await?;
        let work = async {
            // History first: the turn may already be complete.
            let found = self.lookup(&conn, thread_id, &mut target).await?;
            if let Lookup::Done(t) = found {
                return Ok(Some(t));
            }
            // Not in history: the submission may still sit in the queue, which proves the
            // daemon took it. Never resubmit.
            if let (
                Lookup::Absent,
                Target::Client {
                    client_id,
                    receipt_id,
                    queued,
                },
            ) = (&found, &mut target)
                && let Some(queue_id) = self.queued(&conn, thread_id, client_id).await?
            {
                self.store.accept(receipt_id, Some(&queue_id), None, None)?;
                *queued = true;
            }
            // Subscribe. This loads a not-loaded thread, whose queue the daemon then
            // drains unless its last turn was interrupted, and replays pending approvals;
            // both are buffered and handled by `observe`.
            self.subscribe(&conn, thread_id).await?;
            // Check again now that events are buffered, closing the gap before the subscription.
            // A turn id nobody vouches for is refused; one an accepted receipt names is
            // observed even while its rollout is still empty.
            match self.lookup(&conn, thread_id, &mut target).await? {
                Lookup::Done(t) => return Ok(Some(t)),
                Lookup::Absent if matches!(target, Target::Turn(_)) && receipt_id.is_none() => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        "turn not found in this thread",
                    ));
                }
                _ => {}
            }
            self.observe(&mut conn, &mut w, &mut target).await.map(Some)
        };
        let res = bounded(Some(deadline), work).await;
        let progress = target.progress();
        self.finish(conn, res, receipt_id.as_deref(), w, progress)
            .await
    }
}
