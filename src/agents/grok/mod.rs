//! Grok CLI adapter (DESIGN.md §6.4).
//!
//! Every mutation runs one `grok agent … stdio` child speaking ACP. When the leader
//! socket (`$GROK_LEADER_SOCKET`, default `<GROK_HOME>/leader.sock`) answers a connect,
//! the child is `grok agent --leader stdio`, a proxy into the user's shared leader,
//! where sessions behave like Codex threads in the daemon; otherwise it is `grok agent
//! --no-leader stdio`, an in-process agent that holds the session only while the
//! command runs (closing its stdin ends it within about two seconds, and a running
//! turn dies with it). `--leader` is never passed without a live socket: it spawns a
//! persistent leader when none listens.
//!
//! agent-talk's `promptId` (the intent's client message id) becomes the persisted
//! `turn_completed.prompt_id`, so it is the turn id. History, turn ends after the
//! command and listings come from the store `<GROK_HOME>/sessions/<urlencoded
//! cwd>/<id>/` (`updates.jsonl`, `summary.json`) and `<GROK_HOME>/active_sessions.json`.

mod acp;
mod updates;

use self::acp::{Conn, Event, Reply, ServerRequest, Spawn};
use self::updates::{History, first_prompt};
use super::{
    Agent, AgentStatus, ApprovalPolicy, Check, ListFilter, Operation, ReadPage, ReadQuery,
    SendRequest, StartRequest, WaitTarget, approval_policy, bounded, cli_version, cut, first_line,
    full_access, lock, pid_alive, record, reject, settle, strip_provenance, wait_receipt,
};
use crate::model::{
    self, Approval, Error, ErrorCode, Model, Observations, Outcome, Page, ReceiptState, Result,
    Session,
};
use crate::store::{NewIntent, Store};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// Why agent-talk refuses a second writer (DESIGN.md §6.4).
const SECOND_WRITER: &str = "Grok has no lock between processes: a second writer marks the running turn interrupted and the two processes' memories of the session diverge";
/// How long a command whose deadline passed keeps the direct-mode child to end the
/// turn cleanly (wait for it to start, `session/cancel`, wait for its response).
const WIND_DOWN: Duration = Duration::from_secs(10);

pub struct Grok<'a> {
    store: &'a Store,
    /// `$GROK_HOME`, default `~/.grok`.
    home: PathBuf,
    /// `$GROK_LEADER_SOCKET`, default `<home>/leader.sock`.
    socket: PathBuf,
    /// The socket came from `GROK_LEADER_SOCKET`; only then is `--leader-socket` passed.
    socket_override: bool,
}

fn handle(id: &str) -> String {
    format!("grok:{id}")
}

/// Session ids are UUIDs; anything else is refused before it reaches a path.
fn check_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map(|_| ()).map_err(|_| {
        Error::new(
            ErrorCode::Precondition,
            format!("invalid Grok session id {id}; expected a UUID"),
        )
    })
}

fn io_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Transport, format!("{what}: {e}"))
}

#[derive(Debug, Deserialize)]
struct Summary {
    info: SummaryInfo,
    /// Set by `_x.ai/session/rename` (with `title_is_manual: true`) or generated.
    #[serde(default)]
    generated_title: Option<String>,
    /// `headless` for `-p`; null for the TUI and ACP.
    #[serde(default)]
    session_kind: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

impl Summary {
    /// `updated_at`, empty when absent: the `ls` order and cursor.
    fn at(&self) -> &str {
        self.updated_at.as_deref().unwrap_or("")
    }
}

#[derive(Debug, Deserialize)]
struct SummaryInfo {
    id: String,
    #[serde(default)]
    cwd: Option<String>,
}

/// One row of `active_sessions.json`: a live TUI (DESIGN.md §6.4).
#[derive(Debug, Deserialize)]
struct Active {
    session_id: String,
    pid: u32,
}

/// A session as `_x.ai/sessions/list` describes it, through the leader.
struct Listed {
    resident: bool,
    activity: String,
}

/// The leader as seen at the start of this command: its socket answered a connect.
struct Leader {
    /// The pid in `leader.lock` at the probe, to report a leader Grok spawned since.
    pid: Option<u32>,
}

/// What this command observes of one prompt: its own `session/prompt`, or, for a
/// steer message, the turn it joined.
struct TurnWatch {
    id: String,
    receipt_id: String,
    /// agent-talk's promptId; for steer, the turn the interjection joined, once known.
    prompt_id: Option<String>,
    /// Steer: the delivered text, matched against `_x.ai/session/interjection`.
    interjection: Option<String>,
    policy: ApprovalPolicy,
    /// Attached through a leader, where other clients' permission requests arrive too.
    shared: bool,
    accepted: bool,
    running: bool,
    /// The prompt the session runs now (`runningPromptId`).
    running_prompt: Option<String>,
    /// Agent text of the prompt since its last tool call.
    text: String,
    tools: HashSet<String>,
    approvals: Vec<Approval>,
    response: Option<oneshot::Receiver<Reply>>,
    /// `{stopReason, …}`: the `session/prompt` result, or for steer the
    /// `_x.ai/session/prompt_complete` of the joined turn.
    result: Option<Result<Value>>,
}

impl TurnWatch {
    /// This command's own `session/prompt`, submitted with `prompt_id`.
    fn prompt(
        id: &str,
        receipt_id: &str,
        prompt_id: &str,
        policy: ApprovalPolicy,
        shared: bool,
    ) -> Self {
        TurnWatch {
            prompt_id: Some(prompt_id.to_string()),
            ..TurnWatch::new(id, receipt_id, policy, shared)
        }
    }

    /// A steer message `text` through the leader, sent while the session runs the
    /// prompt `running` (`runningPromptId` at load); the joined turn is learned later.
    fn steer(
        id: &str,
        receipt_id: &str,
        running: Option<String>,
        text: &str,
        policy: ApprovalPolicy,
    ) -> Self {
        TurnWatch {
            interjection: Some(text.to_string()),
            running_prompt: running,
            ..TurnWatch::new(id, receipt_id, policy, true)
        }
    }

    /// What both kinds start from: nothing seen yet.
    fn new(id: &str, receipt_id: &str, policy: ApprovalPolicy, shared: bool) -> Self {
        TurnWatch {
            id: id.to_string(),
            receipt_id: receipt_id.to_string(),
            prompt_id: None,
            interjection: None,
            policy,
            shared,
            accepted: false,
            running: false,
            running_prompt: None,
            text: String::new(),
            tools: HashSet::new(),
            approvals: Vec::new(),
            response: None,
            result: None,
        }
    }

    fn accept(&mut self, store: &Store) -> Result<()> {
        if !self.accepted {
            let p = self.prompt_id.as_deref();
            store.accept(&self.receipt_id, p, p, None)?;
            self.accepted = true;
        }
        Ok(())
    }

    /// Events until the turn's result arrives (or, with `until_running`, until the
    /// prompt runs).
    async fn observe(&mut self, store: &Store, conn: &mut Conn, until_running: bool) -> Result<()> {
        loop {
            if self.result.is_some() || (until_running && self.running) {
                return Ok(());
            }
            let ev = match self.response.as_mut() {
                Some(rx) => tokio::select! {
                    biased;
                    r = rx => Err(r),
                    ev = conn.next_event() => Ok(ev),
                },
                None => Ok(conn.next_event().await),
            };
            match ev {
                Err(r) => {
                    self.response = None;
                    let r = conn.reply(r);
                    if r.is_ok() {
                        self.accept(store)?;
                    }
                    self.result = Some(r);
                }
                Ok(Some(Event::Request(r))) => self.on_request(store, conn, r).await?,
                Ok(Some(Event::Notification { method, params })) => {
                    self.on_notification(store, &method, &params)?
                }
                // Never reached with a response pending: the reader drops the waiters
                // before the event channels close, so the biased select above has
                // already taken the response (or its loss).
                Ok(None) => {
                    return Err(Error::new(
                        ErrorCode::Transport,
                        "grok agent exited while agent-talk was observing; outcome unknown",
                    ));
                }
            }
        }
    }

    fn on_notification(&mut self, store: &Store, method: &str, params: &Value) -> Result<()> {
        if params["sessionId"] != self.id.as_str() {
            return Ok(());
        }
        let mine = |p: &Value, id: &Option<String>| id.is_some() && p.as_str() == id.as_deref();
        match method {
            // The queue lists the prompt (its id is the promptId) and then names it
            // running; the first is the earliest proof of acceptance (direct mode:
            // about 20 ms after the request).
            "_x.ai/queue/changed" => {
                let listed = params["entries"]
                    .as_array()
                    .is_some_and(|es| es.iter().any(|e| mine(&e["id"], &self.prompt_id)));
                let running = mine(&params["runningPromptId"], &self.prompt_id);
                self.running_prompt = params["runningPromptId"].as_str().map(String::from);
                if listed || running {
                    self.accept(store)?;
                }
                if running {
                    self.running = true;
                }
            }
            "_x.ai/session/interjection" => {
                if self.interjection.is_some()
                    && params["text"].as_str() == self.interjection.as_deref()
                {
                    self.interjection = None;
                    // Joined the running turn; with none running, Grok starts a
                    // fallback turn, named by its first update.
                    if let Some(p) = self.running_prompt.clone() {
                        self.prompt_id = Some(p);
                        self.running = true;
                        store.accept(&self.receipt_id, None, self.prompt_id.as_deref(), None)?;
                    }
                }
            }
            "session/update" => {
                let p = &params["_meta"]["promptId"];
                if self.prompt_id.is_none()
                    && self.accepted
                    && p.as_str()
                        .is_some_and(|p| p.starts_with("interject-fallback-"))
                {
                    self.prompt_id = p.as_str().map(String::from);
                    store.accept(&self.receipt_id, None, self.prompt_id.as_deref(), None)?;
                }
                if !mine(p, &self.prompt_id) {
                    return Ok(());
                }
                self.running = true;
                self.accept(store)?;
                let u = &params["update"];
                match u["sessionUpdate"].as_str() {
                    Some("agent_message_chunk") => self
                        .text
                        .push_str(u["content"]["text"].as_str().unwrap_or("")),
                    Some("tool_call") => {
                        self.text.clear();
                        if let Some(id) = u["toolCallId"].as_str() {
                            self.tools.insert(id.to_string());
                        }
                    }
                    _ => {}
                }
            }
            "_x.ai/session/prompt_complete"
                if self.response.is_none() && mine(&params["promptId"], &self.prompt_id) =>
            {
                self.result = Some(Ok(params.clone()));
            }
            "_x.ai/session_notification" => {
                let u = &params["update"];
                if u["sessionUpdate"] == "interaction_resolved" {
                    // Someone answered (or agent-talk's own answer took effect).
                    for a in self.approvals.iter_mut().filter(|a| {
                        a.outcome == "pending" && a.item_id.as_deref() == u["tool_call_id"].as_str()
                    }) {
                        a.outcome = "resolved";
                        record(store, a);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A request from the agent. Permission requests are recorded; under `deny` the
    /// ones for this prompt's tool calls (matched by `toolCallId`; through a leader,
    /// other clients' turns raise them too) get the `reject_once` option. Anything
    /// else is answered "method not found": agent-talk offers no client capabilities.
    async fn on_request(&mut self, store: &Store, conn: &Conn, r: ServerRequest) -> Result<()> {
        let ServerRequest { id, method, params } = r;
        if method != "session/request_permission" {
            return conn
                .respond_error(&id, -32601, "not supported by agent-talk")
                .await;
        }
        let call = &params["toolCall"];
        let call_id = call["toolCallId"].as_str().map(String::from);
        // A request already recorded is not answered again. Unverified whether Grok
        // ever sends the same request twice to one client.
        if self
            .approvals
            .iter()
            .any(|a| a.request_id == id && a.item_id == call_id)
        {
            return Ok(());
        }
        let ours = !self.shared || call_id.as_ref().is_some_and(|c| self.tools.contains(c));
        let reject = params["options"]
            .as_array()
            .and_then(|os| os.iter().find(|o| o["kind"] == "reject_once"))
            .and_then(|o| o["optionId"].as_str());
        let outcome = match (self.policy, ours, reject) {
            (ApprovalPolicy::Deny, true, Some(option)) => {
                conn.respond(
                    &id,
                    json!({"outcome": {"outcome": "selected", "optionId": option}}),
                )
                .await?;
                "declined"
            }
            _ => "pending",
        };
        let summary = call["title"]
            .as_str()
            .or(call["_meta"]["x.ai/tool"]["name"].as_str())
            .map(|t| first_line(t, 160))
            .unwrap_or_else(|| "permission request".into());
        let a = Approval {
            handle: handle(&self.id),
            turn_id: if ours { self.prompt_id.clone() } else { None },
            item_id: call_id,
            request_id: id,
            kind: method,
            summary,
            outcome,
            raw: params,
        };
        record(store, &a);
        self.approvals.push(a);
        Ok(())
    }

    /// The turn as its result reports it; `final_text` is the agent text after the
    /// last tool call, from the live updates. `note` is appended to the basis.
    fn turn(&self, handle: String, result: &Value, basis: &str, note: Option<&str>) -> model::Turn {
        let (status, error) = updates::status(
            result["stopReason"].as_str().unwrap_or(""),
            &result["_meta"],
        );
        model::Turn {
            handle,
            // A result exists only for a known prompt: a prompt carries its id from the
            // start, and a steer result is the prompt_complete matched by that id.
            turn_id: self
                .prompt_id
                .clone()
                .expect("a turn result names its prompt"),
            status,
            error,
            final_text: (!self.text.is_empty()).then(|| self.text.clone()),
            duration_ms: None,
            basis: Some(match note {
                Some(n) => format!("{basis}; {n}"),
                None => basis.into(),
            }),
        }
    }
}

impl<'a> Grok<'a> {
    pub fn new(store: &'a Store) -> Self {
        let home_dir = std::env::home_dir().unwrap_or_default();
        let home = std::env::var_os("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".grok"));
        let (socket, socket_override) = match std::env::var_os("GROK_LEADER_SOCKET") {
            Some(s) => (PathBuf::from(s), true),
            None => (home.join("leader.sock"), false),
        };
        Grok {
            store,
            home,
            socket,
            socket_override,
        }
    }

    fn sessions(&self) -> PathBuf {
        self.home.join("sessions")
    }

    /// The session directory, found by id across the cwd groups.
    fn find(&self, id: &str) -> Option<PathBuf> {
        std::fs::read_dir(self.sessions())
            .ok()?
            .flatten()
            .map(|d| d.path().join(id))
            .find(|p| p.is_dir())
    }

    fn require(&self, id: &str) -> Result<PathBuf> {
        self.find(id).ok_or_else(|| {
            Error::new(
                ErrorCode::Precondition,
                format!("no Grok session {id} under {}", self.sessions().display()),
            )
        })
    }

    /// The pid in the leader's lock file, which sits next to its socket (`leader.sock`,
    /// `leader.lock`; a custom socket `x.sock` has `x.lock`).
    fn leader_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.socket.with_extension("lock"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Connect to the leader socket. Only a missing socket, a refused connection (a
    /// stale socket file, e.g. after the leader died) or a path too long for a Unix
    /// socket (macOS: 104 bytes; Grok cannot listen there either, DESIGN.md §6.4)
    /// means "no leader".
    fn leader(&self) -> Result<Option<Leader>> {
        let pid = self.leader_pid();
        match std::os::unix::net::UnixStream::connect(&self.socket) {
            Ok(_) => Ok(Some(Leader { pid })),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::InvalidInput
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(Error::new(
                ErrorCode::Transport,
                format!(
                    "cannot probe the Grok leader socket {}: {e}. A sandbox that blocks local network access fails this way; agent-talk needs to reach the socket to tell whether a leader runs",
                    self.socket.display()
                ),
            )),
        }
    }

    /// A note when `leader.lock` names another pid than at the probe: the leader that
    /// answered exited in between and the `--leader` child spawned a new one.
    fn leader_change(&self, leader: &Option<Leader>) -> Option<String> {
        let before = leader.as_ref()?.pid;
        let after = self.leader_pid();
        (before != after).then(|| {
            let note = format!(
                "the Grok leader pid changed during this command ({before:?} -> {after:?}): the leader that answered the probe exited and grok agent --leader spawned a new one, which keeps running"
            );
            tracing::warn!("{note}");
            note
        })
    }

    /// A `grok agent` child in `cwd`, initialized: `--leader` (only after `leader()`
    /// found a live socket) or `--no-leader`.
    async fn connect(&self, cwd: &str, leader: bool, model: Option<&str>) -> Result<Conn> {
        let opts = Spawn {
            leader,
            leader_socket: self.socket_override.then_some(self.socket.as_path()),
            model,
        };
        acp::spawn(cwd, &opts).await
    }

    /// `_x.ai/sessions/list` through the leader: the entry for `id` if it is listed,
    /// or with `None` every listed session.
    async fn listed(&self, conn: &Conn, id: Option<&str>) -> Result<Vec<(String, Listed)>> {
        let v = conn.request("_x.ai/sessions/list", json!({})).await?;
        // The sessions sit under a nested `result` (verified in direct and leader mode).
        Ok(v["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let sid = s["sessionId"].as_str()?;
                (id.is_none() || id == Some(sid)).then(|| {
                    (
                        sid.to_string(),
                        Listed {
                            resident: s["resident"].as_bool() == Some(true),
                            activity: s["activity"].as_str().unwrap_or("unknown").to_string(),
                        },
                    )
                })
            })
            .collect())
    }

    /// Rows of `active_sessions.json` (absent: none).
    fn active(&self) -> Result<Vec<Active>> {
        let path = self.home.join("active_sessions.json");
        match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| {
                Error::new(
                    ErrorCode::CapMissing,
                    format!(
                        "cannot check whether a Grok TUI holds the session: {}: {e}",
                        path.display()
                    ),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_err(&path.display().to_string(), e)),
        }
    }

    /// A recorded child of the session that is still alive and whose turn has
    /// no `turn_completed` yet in the session directory `dir` (a pid alone may have been
    /// reused).
    fn own_run(&self, id: &str, dir: &Path) -> Result<Option<u32>> {
        let alive: Vec<_> = self
            .store
            .processes(&handle(id))?
            .into_iter()
            .filter(|(_, pid)| pid_alive(*pid))
            .collect();
        if alive.is_empty() {
            return Ok(None);
        }
        let history = History::new(updates::load(&dir.join("updates.jsonl"))?);
        for (receipt_id, pid) in alive {
            let ended = self.store.receipt(&receipt_id)?.is_some_and(|r| {
                history
                    .find(&r.client_msg_id)
                    .is_some_and(|s| history.spans[s].end.is_some())
            });
            if !ended {
                return Ok(Some(pid));
            }
        }
        Ok(None)
    }

    /// Refuse a direct-mode write while another process holds the session: a TUI
    /// listed in `active_sessions.json` (no leader lists the session resident, or the
    /// leader path would have been taken), or a grok agent another agent-talk command
    /// started.
    fn refuse_live(&self, id: &str, dir: &Path) -> Result<()> {
        if let Some(a) = self
            .active()?
            .into_iter()
            .find(|a| a.session_id == id && pid_alive(a.pid))
        {
            return Err(Error::new(
                ErrorCode::ForeignLive,
                format!(
                    "{} is open in a Grok TUI (pid {}, active_sessions.json) that runs it in process, outside any leader; {SECOND_WRITER}, so agent-talk only reads this session. Exit the TUI, or run it with --leader so agent-talk can join it through the leader",
                    handle(id),
                    a.pid
                ),
            ));
        }
        if let Some(pid) = self.own_run(id, dir)? {
            return Err(Error::new(
                ErrorCode::Locked,
                format!(
                    "a grok agent started by another agent-talk command (pid {pid}) is still running {}; {SECOND_WRITER}",
                    handle(id)
                ),
            ));
        }
        Ok(())
    }

    /// Record the intent, submit its text as a prompt on the loaded session and observe
    /// it (until the deadline, or to the end of the turn without one), then settle the
    /// receipt. The caller closes `conn`.
    async fn run(
        &self,
        conn: &mut Conn,
        id: &str,
        intent: &NewIntent<'_>,
        policy: ApprovalPolicy,
        deadline: Option<Instant>,
        leader: &Option<Leader>,
    ) -> Result<Outcome> {
        self.store.insert_intent(intent)?;
        // Direct mode: the child is the session's writer until it exits; later commands
        // tell it from a foreign writer by its pid.
        if leader.is_none()
            && let Some(pid) = conn.pid()
        {
            self.store
                .insert_process(intent.receipt_id, intent.handle, pid)?;
        }
        let mut w = TurnWatch::prompt(
            id,
            intent.receipt_id,
            intent.client_msg_id,
            policy,
            leader.is_some(),
        );
        let text = intent.delivered_text.unwrap_or(intent.text);
        let rx = conn
            .start(
                "session/prompt",
                json!({
                    "sessionId": id,
                    "prompt": [{"type": "text", "text": text}],
                    "_meta": {"promptId": intent.client_msg_id},
                }),
            )
            .await;
        match rx {
            Ok(rx) => w.response = Some(rx),
            Err(e) => return Err(reject(self.store, &w.receipt_id, e)),
        }
        // Without --wait the turn still runs to its end: no process outlives the
        // command in direct mode. Only Ctrl-C stops observing early.
        let mut res = bounded(deadline, w.observe(self.store, conn, false)).await;
        if let Err(e) = &mut res
            && matches!(e.code, ErrorCode::Timeout | ErrorCode::Interrupted)
        {
            if leader.is_some() {
                e.message
                    .push_str("; the turn continues in the Grok leader");
            } else {
                // The child dies with this command and would take the turn down
                // mid-step; end it Grok's way instead, so updates.jsonl records
                // turn_completed (stop_reason cancelled). A cancel sent before the
                // prompt runs is ignored, hence the wait for it to start.
                let _ = tokio::time::timeout(WIND_DOWN, w.observe(self.store, conn, true)).await;
                if w.result.is_none() {
                    let _ = conn
                        .notify("session/cancel", json!({"sessionId": id}))
                        .await;
                    let _ =
                        tokio::time::timeout(WIND_DOWN, w.observe(self.store, conn, false)).await;
                }
                e.message.push_str("; in direct mode no process outlives the command, so agent-talk ended the turn with session/cancel unless it finished first; wait --receipt reads how it ended from updates.jsonl");
            }
        }
        let h = handle(id);
        let note = self.leader_change(leader);
        let res = res.and_then(|()| {
            // observe(false) returns Ok only once the result is in.
            match w.result.take().expect("observe ended without a result") {
                Ok(v) => Ok(w.turn(h.clone(), &v, "session/prompt response", note.as_deref())),
                // A refusal of the prompt itself; an error after acceptance is a failed turn.
                Err(e) if !w.accepted => Err(reject(self.store, &w.receipt_id, e)),
                Err(e) => Ok(model::Turn {
                    handle: h.clone(),
                    turn_id: intent.client_msg_id.to_string(),
                    status: "failed",
                    error: Some(json!({"message": e.message, "agent_error": e.agent_error})),
                    final_text: (!w.text.is_empty()).then(|| w.text.clone()),
                    duration_ms: None,
                    basis: Some("session/prompt error response".into()),
                }),
            }
        });
        let progress = if w.running {
            "running"
        } else if w.accepted {
            "pending"
        } else {
            "unknown"
        };
        settle(
            self.store,
            h,
            res.map(|t| deadline.is_some().then_some(t)),
            Some(&w.receipt_id),
            w.approvals,
            progress,
        )
    }

    /// `session/load` (replays the history as notifications before its response;
    /// they are discarded) and the load result.
    async fn session_load(
        &self,
        conn: &mut Conn,
        id: &str,
        cwd: &str,
        full_access: bool,
    ) -> Result<Value> {
        let mut params = json!({"sessionId": id, "cwd": cwd, "mcpServers": []});
        if full_access {
            // As on session/new: `_meta.yoloMode` on session/load sets yolo for this
            // process's session (DESIGN.md §6.4).
            params["_meta"] = json!({"yoloMode": true});
        }
        let r = conn.request("session/load", params).await?;
        conn.discard_notifications();
        Ok(r)
    }

    /// `send --steer`: `_x.ai/interject` on a session working in the leader.
    async fn steer(
        &self,
        id: &str,
        cwd: &str,
        req: &SendRequest<'_>,
        policy: ApprovalPolicy,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let h = handle(id);
        let Some(leader) = self.leader()? else {
            return Err(Error::new(
                ErrorCode::NoSteer,
                format!(
                    "Grok steers only through a live leader ({} does not answer), and in direct mode no process of an earlier command survives to steer; send without --steer",
                    self.socket.display()
                ),
            ));
        };
        let mut conn = self.connect(cwd, true, None).await?;
        let res = async {
            let listed = self.listed(&conn, Some(id)).await?;
            match listed.first() {
                Some((_, l)) if l.resident && l.activity == "working" => {}
                Some((_, l)) if l.resident => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!(
                            "{h} is {} in the leader; steer needs a running turn (Grok would start a fallback turn instead); send without --steer",
                            l.activity
                        ),
                    ));
                }
                _ => {
                    return Err(Error::new(
                        ErrorCode::NoSteer,
                        format!(
                            "{h} is not resident in the Grok leader, so no running turn can be joined; send without --steer"
                        ),
                    ));
                }
            }
            let loaded = self.session_load(&mut conn, id, cwd, false).await?;
            let running = loaded["_meta"]["x.ai/runningPromptId"]
                .as_str()
                .map(String::from);
            let receipt_id = uuid::Uuid::new_v4().to_string();
            let client_msg_id = uuid::Uuid::new_v4().to_string();
            self.store.insert_intent(&NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &client_msg_id,
                text: req.text,
                delivered_text: (req.delivered != req.text).then_some(req.delivered),
                from: req.from,
                depth: req.depth,
            })?;
            let mut w = TurnWatch::steer(id, &receipt_id, running, req.delivered, policy);
            let ack = conn
                .request(
                    "_x.ai/interject",
                    json!({"sessionId": id, "text": req.delivered}),
                )
                .await;
            match ack {
                // The ack (`status: queued`) is the acceptance; the turn it joined comes
                // with `_x.ai/session/interjection`.
                Ok(_) => w.accept(self.store)?,
                Err(e) => return Err(reject(self.store, &w.receipt_id, e)),
            }
            let res = match deadline {
                Some(d) => bounded(Some(d), w.observe(self.store, &mut conn, false)).await,
                None => {
                    // Without --wait: only until the joined turn is known (`running` is
                    // set exactly when the watch learns its prompt id).
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        w.observe(self.store, &mut conn, true),
                    )
                    .await;
                    Ok(())
                }
            };
            let note = self.leader_change(&Some(leader));
            // Steer sets no `session/prompt` response, so a result is the joined turn's
            // `_x.ai/session/prompt_complete`, never an error.
            let res = res.map(|()| {
                w.result.take().and_then(Result::ok).map(|v| {
                    w.turn(
                        h.clone(),
                        &v,
                        "_x.ai/session/prompt_complete of the joined turn",
                        note.as_deref(),
                    )
                })
            });
            settle(
                self.store,
                h.clone(),
                res,
                Some(&w.receipt_id),
                w.approvals,
                "running",
            )
        }
        .await;
        conn.close().await;
        res
    }
}

impl Agent for Grok<'_> {
    async fn models(&self) -> Result<Vec<Model>> {
        // A direct child's `initialize` response lists the account's models with their
        // effort choices; no session is opened (DESIGN.md §6.4).
        let conn = self
            .connect(&self.home.to_string_lossy(), false, None)
            .await?;
        let models = conn.init["_meta"]["modelState"]["availableModels"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| Model {
                id: m["modelId"].as_str().unwrap_or_default().into(),
                efforts: m["_meta"]["reasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["value"].as_str())
                    .map(String::from)
                    .collect(),
            })
            .collect();
        // Dropping kills the child: a clean exit takes about 4 s and there is nothing to
        // flush.
        drop(conn);
        Ok(models)
    }

    async fn status(&self) -> AgentStatus {
        let version = cli_version("grok").await;
        let cli: Check = version.as_ref().map(|_| ()).map_err(String::clone);
        let (shared, leader): (&str, Check) = match self.leader() {
            Ok(Some(_)) => ("leader running", Ok(())),
            Ok(None) => (
                "no leader running",
                Err("needs a running Grok leader".into()),
            ),
            Err(e) => ("leader not reachable", Err(e.message)),
        };
        let dir = self.sessions();
        let store: Check = if dir.is_dir() {
            Ok(())
        } else {
            Err(format!("{} does not exist", dir.display()))
        };
        AgentStatus {
            agent: "grok",
            version: version.ok(),
            shared: Some(shared.into()),
            operations: vec![
                Operation::new("ls", &store),
                Operation::new("new", &cli),
                Operation::new("send", &cli),
                Operation::new("read", &store),
                Operation::new("wait", &Ok(())),
                Operation::new("steer", &cli.clone().and(leader)),
                Operation::new("name", &cli),
            ],
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        // (dir, summary), newest first, ties by id.
        let mut rows: Vec<(PathBuf, Summary)> = Vec::new();
        for group in std::fs::read_dir(self.sessions())
            .into_iter()
            .flatten()
            .flatten()
        {
            for dir in std::fs::read_dir(group.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = dir.path().join("summary.json");
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(s) = serde_json::from_str::<Summary>(&text) else {
                    tracing::warn!("undecodable {}", path.display());
                    continue;
                };
                if filter.cwd.is_some() && s.info.cwd.as_deref() != filter.cwd {
                    continue;
                }
                rows.push((dir.path(), s));
            }
        }
        rows.sort_by(|(_, a), (_, b)| b.at().cmp(a.at()).then(a.info.id.cmp(&b.info.id)));
        let skip = match cursor {
            Some(c) => {
                let (at, id) = c.split_once('|').ok_or_else(|| {
                    Error::new(ErrorCode::Precondition, format!("invalid cursor {c}"))
                })?;
                rows.iter()
                    .position(|(_, s)| s.at() < at || (s.at() == at && s.info.id.as_str() > id))
                    .unwrap_or(rows.len())
            }
            None => 0,
        };
        let end = (skip + limit as usize).min(rows.len());
        let next_cursor = (end < rows.len() && end > 0).then(|| {
            let s = &rows[end - 1].1;
            format!("{}|{}", s.at(), s.info.id)
        });
        let page: Vec<_> = rows.into_iter().take(end).skip(skip).collect();
        if page.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                next_cursor,
            });
        }
        let active = self.active().unwrap_or_else(|e| {
            tracing::warn!("{e}");
            Vec::new()
        });
        // The leader's own view, when one runs.
        let mut listed = Vec::new();
        match self.leader() {
            Ok(Some(_)) => {
                // The client process needs some cwd; `_x.ai/sessions/list` covers all.
                let cwd = std::env::temp_dir().to_string_lossy().into_owned();
                match self.connect(&cwd, true, None).await {
                    Ok(conn) => {
                        match self.listed(&conn, None).await {
                            Ok(l) => listed = l,
                            Err(e) => tracing::warn!("leader sessions/list: {e}"),
                        }
                        conn.close().await;
                    }
                    Err(e) => tracing::warn!("leader client: {e}"),
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("{e}"),
        }
        let mut items = Vec::new();
        for (dir, s) in page {
            let id = s.info.id.as_str();
            let h = handle(id);
            let owned = self.store.is_owned(&h)?;
            let tui = active
                .iter()
                .any(|a| a.session_id == id && pid_alive(a.pid));
            let lead = listed.iter().find(|(sid, _)| sid == id).map(|(_, l)| l);
            let updates_path = dir.join("updates.jsonl");
            let own_pid = self.own_run(id, &dir)?;
            let state = match (own_pid, lead) {
                (Some(_), _) => "running",
                (None, Some(l)) if l.resident && l.activity == "working" => "running",
                (None, Some(l)) if l.resident && l.activity == "idle" => "idle",
                _ => "unknown",
            };
            let loaded = own_pid.is_some() || tui || lead.is_some_and(|l| l.resident);
            items.push(Session {
                handle: h,
                agent: "grok",
                cwd: s.info.cwd.clone(),
                name: s.generated_title.clone(),
                preview: first_prompt(&updates_path).map(|p| first_line(strip_provenance(&p), 200)),
                observations: Observations {
                    history: if updates_path.is_file() {
                        "visible"
                    } else {
                        "none"
                    },
                    loaded: if loaded { "yes" } else { "no" },
                    origin: if owned {
                        "agent-talk".into()
                    } else if s.session_kind.as_deref() == Some("headless") {
                        "headless".into()
                    } else {
                        "grok".into()
                    },
                },
                state,
                owned,
                id: id.to_string(),
            });
        }
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        let policy = ApprovalPolicy::Deny;
        let leader = self.leader()?;
        let mut conn = self.connect(req.cwd, leader.is_some(), req.model).await?;
        let res = async {
            let mut params = json!({"cwd": req.cwd, "mcpServers": []});
            if req.full_access {
                params["_meta"] = json!({"yoloMode": true});
            }
            let created = conn.request("session/new", params).await?;
            let id = created["sessionId"].as_str().ok_or_else(|| {
                Error::new(
                    ErrorCode::Transport,
                    format!("session/new returned no sessionId: {created}"),
                )
            })?;
            // Set on the session, not as `--reasoning-effort`, which a leader proxy
            // ignores (DESIGN.md §6.4). A refusal leaves the new session empty and
            // unrecorded; agent-talk deletes nothing, so the error names it.
            if let Some(e) = req.effort {
                let params = json!({"sessionId": id, "configId": "reasoning_effort", "value": e});
                if let Err(mut err) = conn.request("session/set_config_option", params).await {
                    err.message = format!(
                        "reasoning effort {e} refused: {}; the new session {id} has no messages and is not used (`grok sessions delete {id}` removes it)",
                        err.message
                    );
                    return Err(err);
                }
            }
            let h = handle(id);
            // Direct mode only: clients of one leader do not exclude each other.
            let _lock = match leader {
                None => Some(lock(&h, SECOND_WRITER)?),
                Some(_) => None,
            };
            let stored_args = json!({
                "model": req.model,
                "effort": req.effort,
                "full_access": req.full_access,
                "leader": leader.is_some(),
            });
            self.store.insert_owned(&h, req.cwd, &stored_args)?;
            if let Some(n) = req.name {
                conn.request("_x.ai/session/rename", json!({"sessionId": id, "title": n}))
                    .await?;
            }
            let receipt_id = uuid::Uuid::new_v4().to_string();
            // The promptId: becomes turn_completed.prompt_id, i.e. the turn id.
            let prompt_id = uuid::Uuid::new_v4().to_string();
            let intent = NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &prompt_id,
                text: req.prompt,
                delivered_text: (req.delivered != req.prompt).then_some(req.delivered),
                from: req.from,
                depth: req.depth,
            };
            self.run(&mut conn, id, &intent, policy, deadline, &leader)
                .await
        }
        .await;
        conn.close().await;
        res
    }

    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        check_id(id)?;
        let h = handle(id);
        let dir = self.require(id)?;
        let path = dir.join("summary.json");
        let summary: Summary = std::fs::read_to_string(&path)
            .map_err(|e| io_err(&path.display().to_string(), e))
            .and_then(|t| {
                serde_json::from_str(&t).map_err(|e| {
                    Error::new(ErrorCode::Precondition, format!("{}: {e}", path.display()))
                })
            })?;
        let cwd = match summary.info.cwd {
            Some(c) if Path::new(&c).is_dir() => c,
            Some(c) => {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    format!(
                        "the session's directory {c} no longer exists; session/load would run it elsewhere"
                    ),
                ));
            }
            None => {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    "summary.json records no working directory for this session",
                ));
            }
        };
        let policy = approval_policy(self.store, &h)?;
        if req.steer {
            return self.steer(id, &cwd, req, policy, deadline).await;
        }
        // Through the leader only when the session is resident there: a listed but
        // dormant session may be held in process by a TUI.
        let resident = match self.leader()? {
            Some(leader) => {
                let conn = self.connect(&cwd, true, None).await?;
                match self.listed(&conn, Some(id)).await {
                    Ok(l) if l.first().is_some_and(|(_, l)| l.resident) => Some((leader, conn)),
                    Ok(_) => {
                        conn.close().await;
                        None
                    }
                    Err(e) => {
                        conn.close().await;
                        return Err(e);
                    }
                }
            }
            None => None,
        };
        let (leader, mut conn, _lock) = match resident {
            Some((leader, conn)) => (Some(leader), conn, None),
            None => {
                let lock = lock(&h, SECOND_WRITER)?;
                self.refuse_live(id, &dir)?;
                let conn = self.connect(&cwd, false, None).await?;
                (None, conn, Some(lock))
            }
        };
        let res = async {
            // Not through the leader: on a resident session yoloMode has no effect
            // (verified: `yolo` stayed unchanged for every client after such a load).
            let yolo = leader.is_none() && full_access(self.store, &h)?;
            self.session_load(&mut conn, id, &cwd, yolo).await?;
            let receipt_id = uuid::Uuid::new_v4().to_string();
            let prompt_id = uuid::Uuid::new_v4().to_string();
            let intent = NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &prompt_id,
                text: req.text,
                delivered_text: (req.delivered != req.text).then_some(req.delivered),
                from: req.from,
                depth: req.depth,
            };
            self.run(&mut conn, id, &intent, policy, deadline, &leader)
                .await
        }
        .await;
        conn.close().await;
        res
    }

    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage> {
        check_id(id)?;
        let dir = self.require(id)?;
        let history = History::new(updates::load(&dir.join("updates.jsonl"))?);
        let mut messages = history.messages();
        for m in &mut messages {
            if m.prompt
                && let Some(s) = m.span
                && let Some(p) = &history.spans[s].id
            {
                m.message.from = self.store.sender_of(p)?;
            }
        }
        let messages = messages.into_iter().map(|m| (m.line, m.message)).collect();
        let cut = cut(q, messages, history.lines.len(), false)?;
        Ok(ReadPage {
            messages: Page {
                next_cursor: cut.cursor_item().map(|(_, m)| m.item_id.clone()),
                items: cut.messages.into_iter().map(|(_, m)| m).collect(),
            },
            // Every line in the span (thoughts, tool updates, hooks included).
            raw: history.lines[cut.records]
                .iter()
                .map(|l| l.raw.clone())
                .collect(),
        })
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        check_id(id)?;
        let h = handle(id);
        let (turn_id, receipt) = match target {
            WaitTarget::Turn(t) => (t.clone(), self.store.receipt_by_turn(&h, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &h, r)?;
                // The promptId, recorded as the client message id before submission.
                (
                    rec.turn_id.clone().unwrap_or(rec.client_msg_id.clone()),
                    Some(rec),
                )
            }
        };
        let dir = self.require(id)?;
        let path = dir.join("updates.jsonl");
        let mut unaccepted = receipt
            .as_ref()
            .is_some_and(|r| matches!(r.state, ReceiptState::Pending | ReceiptState::Unknown));
        // The direct-mode child that ran the receipt's prompt; once it is gone nothing
        // will write the turn's end.
        let child = match &receipt {
            Some(r) => self.store.process(&r.receipt_id)?,
            None => None,
        };
        let work = async {
            loop {
                let history = History::new(updates::load(&path)?);
                let span = history.find(&turn_id);
                if let (Some(_), Some(r)) = (span, &receipt)
                    && unaccepted
                {
                    self.store
                        .accept(&r.receipt_id, Some(&turn_id), Some(&turn_id), None)?;
                    unaccepted = false;
                }
                if let Some(s) = span
                    && let Some(t) = history.turn(h.clone(), s, &history.messages())
                {
                    return Ok(t);
                }
                if span.is_none() && receipt.is_none() {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!("turn {turn_id} is not in {}", path.display()),
                    ));
                }
                if let Some(pid) = child
                    && !pid_alive(pid)
                {
                    let (status, message) = match span {
                        Some(_) => (
                            "interrupted",
                            "the turn started but has no turn_completed; Grok marks it interrupted when the session is next loaded",
                        ),
                        None => ("unknown", "updates.jsonl names no turn for this prompt"),
                    };
                    return Ok(model::Turn {
                        handle: h.clone(),
                        turn_id: turn_id.clone(),
                        status,
                        error: Some(json!({"message": message})),
                        final_text: None,
                        duration_ms: None,
                        basis: Some(format!(
                            "updates.jsonl; the grok agent agent-talk started for this prompt (pid {pid}) has exited, and in direct mode nothing else continues it"
                        )),
                    });
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        let res = bounded(Some(deadline), work).await.map(Some).map_err(|mut e| {
            if matches!(e.code, ErrorCode::Timeout | ErrorCode::Interrupted) {
                e.message.push_str(&format!(
                    "; waiting for turn_completed of {turn_id} in updates.jsonl (polled every second)"
                ));
            }
            e
        });
        let receipt_id = receipt.map(|r| r.receipt_id);
        settle(
            self.store,
            h,
            res,
            receipt_id.as_deref(),
            Vec::new(),
            "running",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn grok<'a>(store: &'a Store, socket: PathBuf) -> Grok<'a> {
        Grok {
            store,
            home: socket.parent().unwrap().to_path_buf(),
            socket,
            socket_override: true,
        }
    }

    /// Direct mode only on a missing socket or a refused connect (a stale socket file
    /// such as one a dead leader left); a socket agent-talk may not reach is an error,
    /// never "no leader" (that would start a second writer beside a live leader).
    #[test]
    fn leader_probe_classification() {
        let store = Store::memory();
        let dir = std::env::temp_dir().join(format!("bgl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("l.sock");
        assert!(
            grok(&store, sock.clone()).leader().unwrap().is_none(),
            "ENOENT"
        );
        let listener = UnixListener::bind(&sock).unwrap();
        assert!(
            grok(&store, sock.clone()).leader().unwrap().is_some(),
            "answering"
        );
        drop(listener);
        assert!(sock.exists());
        assert!(
            grok(&store, sock.clone()).leader().unwrap().is_none(),
            "ECONNREFUSED"
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = grok(&store, sock.clone()).leader();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            denied.err().map(|e| e.code),
            Some(ErrorCode::Transport),
            "EACCES"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
