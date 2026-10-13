//! Codex adapter: talks to the shared app-server daemon over its control socket.

mod protocol;
mod transport;

use self::protocol::{
    ALL_SOURCE_KINDS, AgentMessage, AgentPhase, CursorPage, DEFAULT_SOURCE_KINDS, Item,
    ItemNotification, ModelInfo, QueuedEntry, Thread, ThreadQueueAddResponse, ThreadStartResponse,
    ThreadStatus, TurnCompletedNotification, TurnResponse, TurnSteerResponse, deny_response,
    history_not_ready, nothing_dormant, resume_error, resume_latest_turn, text_input,
};
use self::transport::{Conn, Event, ServerRequest, decode};
use crate::agents::child::{agent_cmd, cli_version};
use crate::agents::history::{ReadPage, ReadQuery, RemoteHistory};
use crate::agents::lifecycle::{
    ApprovalPolicy, approval_policy, bounded, record, reject, resolve, settle, wait_receipt,
};
use crate::agents::{
    Agent, AgentStatus, Check, ListFilter, Operation, SendRequest, StartRequest, WaitTarget,
    strip_provenance,
};
use crate::model::{
    self, Approval, ApprovalOutcome, Error, ErrorCode, History, Loaded, Message, Model,
    Observations, Outcome, Page, Phase, Progress, Result, Role, Session, SessionState, TurnStatus,
};
use crate::store::{NewIntent, Store};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;

const DUMMY_THREAD: &str = "00000000-0000-0000-0000-000000000000";
/// How often `subscribe` retries `thread/resume`, a second apart, while a just-started
/// thread's history is not readable yet.
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
    fn progress(&self) -> Progress {
        match self {
            Target::Turn(_) => Progress::Running,
            Target::Client { queued: true, .. } => Progress::Pending,
            Target::Client { queued: false, .. } => Progress::Unknown,
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
    /// thread) and return its newest turn, without items. For about a second after `new`
    /// the thread's history is not readable yet and the daemon cannot resume it; that is
    /// retried once a second, at most `RESUME_RETRIES` times, within the caller's deadline.
    /// A thread another app-server process holds is `E_FOREIGN_LIVE` (`resume_error`).
    async fn subscribe(&self, conn: &Conn, thread_id: &str) -> Result<Option<protocol::Turn>> {
        let mut params = resume_latest_turn(thread_id);
        let settings = self.store.settings(&handle(thread_id))?;
        if settings.is_some_and(|s| s.full_access) {
            // A thread that unloaded comes back with the user's sandbox; the approval
            // policy survives (observed on codex 0.160.1, DESIGN.md §6.1).
            params["sandbox"] = json!("danger-full-access");
        }
        let mut retries = 0;
        let resumed: protocol::ThreadResumeResponse = loop {
            match conn.call("thread/resume", &params).await {
                Err(e) if history_not_ready(&e) && retries < RESUME_RETRIES => {
                    retries += 1;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                r => break r.map_err(resume_error)?,
            }
        };
        Ok(resumed
            .initial_turns_page
            .and_then(|p| p.data.into_iter().next()))
    }

    /// `turn/start`, accepted with the turn it started.
    async fn start_turn(&self, conn: &Conn, receipt_id: &str, params: Value) -> Result<Target> {
        let started: TurnResponse = conn
            .call("turn/start", params)
            .await
            .map_err(|e| reject(self.store, receipt_id, e))?;
        self.store
            .accept(receipt_id, None, Some(&started.turn.id), None)?;
        Ok(Target::Turn(started.turn.id))
    }

    /// `turn/steer` into `expected_turn`, accepted with that turn.
    async fn steer(
        &self,
        conn: &Conn,
        thread_id: &str,
        intent: &NewIntent<'_>,
        expected_turn: &str,
    ) -> Result<Target> {
        let params = json!({
            "threadId": thread_id,
            "expectedTurnId": expected_turn,
            "input": text_input(intent.message.delivered),
            "clientUserMessageId": intent.client_msg_id,
        });
        let steered: TurnSteerResponse = conn
            .call("turn/steer", params)
            .await
            .map_err(|e| reject(self.store, intent.receipt_id, e))?;
        self.store
            .accept(intent.receipt_id, None, Some(&steered.turn_id), None)?;
        Ok(Target::Turn(steered.turn_id))
    }

    /// `thread/queue/add`, accepted with the queue id. On an `idle` thread this also runs
    /// `start_if_dormant`, without --wait too, so a plain send to an interrupted thread
    /// runs: the command stays connected up to `DORMANT_WINDOW` longer for it.
    async fn queue(
        &self,
        conn: &mut Conn,
        w: &mut Watch<'_>,
        intent: &NewIntent<'_>,
        idle: bool,
    ) -> Result<Target> {
        let params = json!({
            "threadId": w.thread_id,
            "clientUserMessageId": intent.client_msg_id,
            "input": text_input(intent.message.delivered),
        });
        let added: ThreadQueueAddResponse = conn
            .call("thread/queue/add", params)
            .await
            .map_err(|e| reject(self.store, intent.receipt_id, e))?;
        let queue_id = added.queued_submission.id;
        self.store
            .accept(intent.receipt_id, Some(&queue_id), None, None)?;
        if idle {
            self.start_if_dormant(conn, w, &queue_id, intent.receipt_id)
                .await?;
        }
        Ok(Target::Client {
            client_id: intent.client_msg_id.into(),
            receipt_id: intent.receipt_id.into(),
            queued: true,
        })
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
            .call::<TurnResponse>("thread/queue/start", params)
            .await
        {
            Ok(started) => self
                .store
                .accept(receipt_id, None, Some(&started.turn.id), None),
            Err(e) if nothing_dormant(&e) => Ok(()),
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
                ApprovalOutcome::Declined
            }
            _ => ApprovalOutcome::Pending,
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
        progress: Progress,
    ) -> Result<Outcome> {
        for r in conn.drain_requests() {
            if let Err(e) = self.on_request(&conn, &mut w, r).await {
                tracing::warn!("{e}");
            }
        }
        conn.close().await;
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
        let mut pages = conn.pages(
            "thread/turns/list",
            json!({
                "threadId": thread_id,
                "limit": 50,
                "sortDirection": "asc",
                "itemsView": "full",
            }),
        );
        loop {
            let turns = match pages.next::<protocol::Turn>().await {
                Ok(Some(turns)) => turns,
                Ok(None) => return Ok(Lookup::Absent),
                Err(e) if history_not_ready(&e) => return Ok(Lookup::Absent),
                Err(e) => return Err(e),
            };
            for turn in turns {
                let found = match &*target {
                    Target::Turn(id) => *id == turn.id,
                    Target::Client {
                        client_id,
                        receipt_id,
                        ..
                    } => {
                        let item = turn.items.iter().find_map(|i| match i {
                            Item::User(u) if u.client_id.as_deref() == Some(client_id) => {
                                Some(&u.id)
                            }
                            _ => None,
                        });
                        if let Some(item_id) = item {
                            self.store
                                .accept(receipt_id, None, Some(&turn.id), Some(item_id))?;
                        }
                        item.is_some()
                    }
                };
                if found {
                    *target = Target::Turn(turn.id.clone());
                    return Ok(match turn.status {
                        protocol::TurnStatus::InProgress => Lookup::Running,
                        _ => Lookup::Done(to_turn(thread_id, turn)),
                    });
                }
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
                    if let Item::User(u) = n.item
                        && u.client_id.as_deref() == Some(client_id)
                    {
                        self.store
                            .accept(receipt_id, None, Some(&n.turn_id), Some(&u.id))?;
                        *target = Target::Turn(n.turn_id);
                    }
                }
                // Other turns' completions are skipped undecoded.
                ("turn/completed", Target::Turn(turn_id))
                    if params["turn"]["id"] == turn_id.as_str() =>
                {
                    let n: TurnCompletedNotification = decode(&method, &params)?;
                    return Ok(to_turn(&n.thread_id, n.turn));
                }
                _ => {}
            }
        }
    }

    /// Normalized messages of Codex turns (full items), each with the index of its turn.
    fn messages(&self, turns: &[Value]) -> Result<Vec<(usize, Message)>> {
        let mut messages = Vec::new();
        for (i, turn) in turns.iter().enumerate() {
            let turn: protocol::Turn = decode("turn", turn)?;
            let started = timestamp(turn.started_at);
            let completed = timestamp(turn.completed_at).or_else(|| started.clone());
            for item in turn.items {
                let message = match item {
                    Item::User(u) => Message {
                        turn_id: turn.id.clone(),
                        role: Role::User,
                        phase: Phase::Prompt,
                        text: u.text(),
                        from: match &u.client_id {
                            Some(c) => self.store.sender_of(c)?,
                            None => None,
                        },
                        timestamp: started.clone(),
                        item_id: u.id,
                    },
                    Item::Agent(a) => Message {
                        turn_id: turn.id.clone(),
                        item_id: a.id,
                        role: Role::Assistant,
                        phase: match a.phase {
                            Some(AgentPhase::FinalAnswer) => Phase::Final,
                            Some(AgentPhase::Commentary) => Phase::Commentary,
                            _ => Phase::Other,
                        },
                        text: a.text,
                        from: None,
                        timestamp: completed.clone(),
                    },
                    Item::Other => continue,
                };
                messages.push((i, message));
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

/// The queue id of a client message still waiting in the thread's queue.
async fn queued(conn: &Conn, thread_id: &str, client_id: &str) -> Result<Option<String>> {
    let mut pages = conn.pages("thread/queue/list", json!({"threadId": thread_id}));
    while let Some(entries) = pages.next::<QueuedEntry>().await? {
        if let Some(e) = entries
            .into_iter()
            .find(|e| e.client_user_message_id.as_deref() == Some(client_id))
        {
            return Ok(Some(e.id));
        }
    }
    Ok(None)
}

/// Unix seconds as an RFC 3339 timestamp.
fn timestamp(secs: Option<i64>) -> Option<String> {
    secs.and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map(|t| t.to_string())
}

/// Build the normalized turn from a decoded Codex turn (summary or full items). The
/// final text is the last `final_answer` agent message, else the last agent message.
fn to_turn(thread_id: &str, t: protocol::Turn) -> model::Turn {
    let replies: Vec<AgentMessage> = t
        .items
        .into_iter()
        .filter_map(|i| match i {
            Item::Agent(a) => Some(a),
            _ => None,
        })
        .collect();
    let final_text = replies
        .iter()
        .rev()
        .find(|a| a.phase == Some(AgentPhase::FinalAnswer))
        .or(replies.last())
        .map(|a| a.text.clone());
    model::Turn {
        handle: handle(thread_id),
        turn_id: t.id,
        status: match t.status {
            protocol::TurnStatus::Completed => TurnStatus::Completed,
            protocol::TurnStatus::Failed => TurnStatus::Failed,
            protocol::TurnStatus::Interrupted => TurnStatus::Interrupted,
            protocol::TurnStatus::InProgress | protocol::TurnStatus::Unknown => TurnStatus::Unknown,
        },
        error: t.error,
        final_text,
        duration_ms: t.duration_ms,
        basis: None,
    }
}

fn session_state(s: &ThreadStatus) -> SessionState {
    match s {
        ThreadStatus::Idle => SessionState::Idle,
        ThreadStatus::Active { active_flags } if !active_flags.is_empty() => SessionState::Waiting,
        ThreadStatus::Active { .. } => SessionState::Running,
        _ => SessionState::Unknown,
    }
}

impl Agent for Codex<'_> {
    async fn models(&self) -> Result<Vec<Model>> {
        let conn = self.connect().await?;
        let mut models = Vec::new();
        let mut pages = conn.pages("model/list", json!({"cursor": null, "limit": 100}));
        while let Some(page) = pages.next::<ModelInfo>().await? {
            models.extend(page.into_iter().map(|m| {
                Model {
                    id: m.model,
                    efforts: m
                        .supported_reasoning_efforts
                        .into_iter()
                        .map(|e| e.reasoning_effort)
                        .collect(),
                }
            }));
        }
        conn.close().await;
        Ok(models)
    }

    async fn status(&self) -> AgentStatus {
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
                let server = agent_cmd("codex")
                    .args(["app-server", "daemon", "version"])
                    .output()
                    .await
                    .ok()
                    .filter(|out| out.status.success())
                    .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
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
        AgentStatus {
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
        let threads: CursorPage<Thread> = conn.call("thread/list", params).await?;
        let mut loaded = HashSet::new();
        let mut pages = conn.pages("thread/loaded/list", json!({"limit": 200}));
        while let Some(ids) = pages.next::<String>().await? {
            loaded.extend(ids);
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
                    history: History::Visible,
                    loaded: if loaded.contains(&t.id) {
                        Loaded::Yes
                    } else {
                        Loaded::No
                    },
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
        if let Some(m) = &req.settings.model {
            params["model"] = json!(m);
        }
        if req.settings.full_access {
            params["sandbox"] = json!("danger-full-access");
        }
        let started: ThreadStartResponse = conn.call("thread/start", params).await?;
        let thread_id = started.thread.id;
        let handle = handle(&thread_id);
        self.store.insert_owned(&handle, req.cwd, req.settings)?;
        if let Some(name) = req.name {
            // Before turn/start: right after it the rollout file is still empty and the
            // daemon cannot update thread metadata (DESIGN.md §6.1).
            conn.call::<Value>(
                "thread/name/set",
                json!({"threadId": thread_id, "name": name}),
            )
            .await?;
        }
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
                message: &req.message,
            })?;
            let params = json!({
                "threadId": thread_id,
                "input": text_input(req.message.delivered),
                "clientUserMessageId": client_msg_id,
                // thread/start takes no effort; this one applies to the thread's later
                // turns too.
                "effort": req.settings.effort,
            });
            let target = target.insert(self.start_turn(&conn, &receipt_id, params).await?);
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
        let progress = target.as_ref().map_or(Progress::Unknown, Target::progress);
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
        let intent = NewIntent {
            receipt_id: &receipt_id,
            handle: &handle,
            client_msg_id: &client_msg_id,
            message: &req.message,
        };
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
            let newest = self.subscribe(&conn, thread_id).await?;
            let idle = newest
                .as_ref()
                .is_none_or(|t| t.status != protocol::TurnStatus::InProgress);
            let steer_into = match (req.steer, newest) {
                (false, _) => None,
                (true, Some(t)) => Some(t.id),
                (true, None) => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        "thread has no turn to steer",
                    ));
                }
            };
            // A settings change needs a turn of its own: only turn/start carries it, and
            // thread/queue/add ignores those fields (DESIGN.md §6.1).
            let change = !req.change.is_empty();
            if change && !idle {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    "thread is running a turn and a queued message cannot carry model, effort \
                     or full access; send the change when the thread is idle",
                ));
            }
            self.store.insert_intent(&intent)?;
            let submitted = match steer_into {
                Some(turn) => self.steer(&conn, thread_id, &intent, &turn).await?,
                None if change => {
                    // The thread keeps these for its later turns; the sandbox only until
                    // it unloads, so subscribe passes it again (DESIGN.md §6.1).
                    let mut params = json!({
                        "threadId": thread_id,
                        "input": text_input(req.message.delivered),
                        "clientUserMessageId": client_msg_id,
                    });
                    if let Some(m) = &req.change.model {
                        params["model"] = json!(m);
                    }
                    if let Some(e) = &req.change.effort {
                        params["effort"] = json!(e);
                    }
                    if req.change.full_access {
                        params["sandboxPolicy"] = json!({"type": "dangerFullAccess"});
                    }
                    self.start_turn(&conn, &receipt_id, params).await?
                }
                None => self.queue(&mut conn, &mut w, &intent, idle).await?,
            };
            let target = target.insert(submitted);
            if deadline.is_none() {
                return Ok(None);
            }
            self.observe(&mut conn, &mut w, target).await.map(Some)
        };
        let res = match deadline {
            Some(d) => bounded(Some(d), work).await,
            None => work.await,
        };
        let progress = target.as_ref().map_or(Progress::Unknown, Target::progress);
        self.finish(conn, res, Some(&receipt_id), w, progress).await
    }

    async fn read(&self, thread_id: &str, q: &ReadQuery) -> Result<ReadPage> {
        let conn = self.connect().await?;
        let fetch = async |cursor: Option<&str>| {
            let mut params = json!({
                "threadId": thread_id,
                "limit": 20,
                "sortDirection": "desc",
                "itemsView": "full",
            });
            if let Some(c) = cursor {
                params["cursor"] = json!(c);
            }
            let page: CursorPage<Value> = conn.call("thread/turns/list", params).await?;
            Ok((page.data, page.next_cursor))
        };
        let normalize = |turns: &[Value]| self.messages(turns);
        let history = RemoteHistory::fetch(q, fetch, normalize).await?;
        conn.close().await;
        let messages = normalize(history.records())?;
        history.page(q, messages)
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
                        receipt_id: rec.receipt_id.clone(),
                        queued: false,
                    },
                };
                (target, Some(rec.receipt_id))
            }
        };
        let mut w = Watch::new(thread_id, approval_policy(self.store, &handle)?);
        let mut conn = self.connect().await?;
        let work = async {
            // History first: the turn may already be complete.
            if let Lookup::Done(t) = self.lookup(&conn, thread_id, &mut target).await? {
                return Ok(Some(t));
            }
            // A client message not in history may still sit in the queue, which proves the
            // daemon took it. Never resubmit.
            if let Target::Client {
                client_id,
                receipt_id,
                queued: in_queue,
            } = &mut target
                && let Some(queue_id) = queued(&conn, thread_id, client_id).await?
            {
                self.store.accept(receipt_id, Some(&queue_id), None, None)?;
                *in_queue = true;
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
