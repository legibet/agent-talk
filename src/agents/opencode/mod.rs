//! OpenCode adapter: HTTP client of the user's background service (`opencode serve
//! --service`), which every OpenCode client shares. Built against opencode 2.0.22; the
//! verified behaviour is in DESIGN.md §6.3.
//!
//! OpenCode has no turn id. agent-talk's turn id is the user message id, which the client
//! chooses (`session.prompt.id`); a turn ends at the first `idle` message after that user
//! message in history (`outcome` = status), or at its aborted step when an interrupt with
//! reason `shutdown` wrote no `idle` (`End`). Live, `session.execution.{succeeded,failed,
//! interrupted}` says "an execution ended"; history is re-read to confirm, because the SSE
//! stream replays nothing and one execution can serve several user messages.

mod transport;

use self::transport::{Events, Service};
use super::{
    Agent, ApprovalPolicy, Caps, Check, ListFilter, Operation, ReadPage, ReadRange, SendRequest,
    StartRequest, WaitTarget, approval_policy, bounded, record, reject, resolve, settle,
    wait_receipt,
};
use crate::model::{
    self, Approval, Error, ErrorCode, Message, Observations, Outcome, Page, ReceiptState, Result,
    Session,
};
use crate::store::{NewIntent, Store};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::time::Instant;

/// How many history pages (newest first) `span` scans for a user message. An older
/// message counts as not in history, so `wait` on it ends with `E_PRECONDITION`.
const SPAN_PAGES: usize = 40;
const DENY_MESSAGE: &str = "Declined by agent-talk: nobody answers approval requests in a session agent-talk started. Continue without this action, or report that it needs approval.";

pub struct OpenCode<'a> {
    store: &'a Store,
    /// `$XDG_STATE_HOME/opencode/service.json`, written by the service itself.
    registration: PathBuf,
}

enum Lookup {
    Done(model::Turn),
    /// In history with no `idle` after it yet, or still in the session's inbox (queued
    /// behind another execution, or dormant). `Watch::delivered` tells which.
    Unfinished,
    Absent,
}

/// Per-command observation state.
struct Watch<'t> {
    session_id: &'t str,
    msg_id: &'t str,
    policy: ApprovalPolicy,
    /// The service accepted the prompt (always true when waiting on an earlier one).
    submitted: bool,
    /// The message left the inbox and is part of an execution.
    delivered: bool,
    /// A receipt whose submission outcome was lost (`unknown`): accepted once the
    /// inbox or history shows the message.
    unaccepted: Option<String>,
    approvals: Vec<Approval>,
}

impl<'t> Watch<'t> {
    fn new(session_id: &'t str, msg_id: &'t str, policy: ApprovalPolicy, submitted: bool) -> Self {
        Watch {
            session_id,
            msg_id,
            policy,
            submitted,
            delivered: false,
            unaccepted: None,
            approvals: Vec::new(),
        }
    }

    /// Progress reported when the outcome is not known yet.
    fn progress(&self) -> &'static str {
        match (self.submitted, self.delivered) {
            (false, _) => "unknown",
            (true, false) => "pending",
            (true, true) => "running",
        }
    }
}

fn handle(id: &str) -> String {
    format!("opencode:{id}")
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// Moves the `data` array out of a list response (empty when absent).
fn take_data(resp: &mut Value) -> Vec<Value> {
    resp.get_mut("data")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default()
}

fn ts(ms: &Value) -> Option<String> {
    ms.as_i64()
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map(|t| t.to_string())
}

/// A message id in OpenCode's own ascending format (12 hex digits of time, 14 base62
/// characters), so clients that sort by id keep agent-talk's messages in order.
fn new_msg_id() -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    let random = uuid::Uuid::new_v4();
    let bytes = random.as_bytes();
    let counter = u64::from(bytes[14]) << 4 | u64::from(bytes[15] >> 4);
    let time = (now_ms.wrapping_mul(0x1000).wrapping_add(counter)) & 0xffff_ffff_ffff;
    let tail: String = bytes[..14]
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect();
    format!("msg_{time:012x}{tail}")
}

/// `provider/model[#variant]` → `Model.Ref`.
fn model_ref(spec: &str) -> Result<Value> {
    let (provider, rest) = spec.split_once('/').ok_or_else(|| {
        Error::new(
            ErrorCode::Precondition,
            format!("--model for OpenCode is <providerID>/<modelID>[#variant], got {spec}"),
        )
    })?;
    let (model, variant) = match rest.split_once('#') {
        Some((m, v)) => (m, Some(v)),
        None => (rest, None),
    };
    let mut r = json!({"providerID": provider, "id": model});
    if let Some(v) = variant {
        r["variant"] = json!(v);
    }
    Ok(r)
}

/// Everything in history from the user message `msg_id` (inclusive) to the end of its turn.
struct Span {
    user: Value,
    /// Messages after the user message, oldest first: up to the `idle` (excluded), or up
    /// to the interrupted step (included).
    after: Vec<Value>,
    end: Option<End>,
}

/// How the turn ended in history (DESIGN.md §6.3).
enum End {
    /// The `idle` message an execution end writes.
    Idle(Value),
    /// An interrupt with reason `shutdown` (message-less permission reject, service going
    /// down) writes no `idle`; the aborted step is the turn's last row, followed by
    /// another execution's input or by nothing while the session is inactive.
    Aborted,
}

fn aborted(m: &Value) -> bool {
    m["type"] == "assistant" && m["error"]["type"] == "aborted"
}

impl Span {
    /// `after`: what follows the user message in history, oldest first.
    fn new(user: Value, mut after: Vec<Value>) -> Span {
        let end_at = after.iter().enumerate().find_map(|(i, m)| {
            if m["type"] == "idle" {
                return Some(i);
            }
            if !aborted(m) {
                return None;
            }
            // A user message right after the aborted step is another execution's input
            // (DESIGN.md §6.3); anything else continues this one: its own
            // `idle` (user interrupt), a restart notice, or further steps after the
            // location closed during a permission prompt (OpenCode source `step.ts`,
            // `needsContinuation`). Nothing after it yet: undecided.
            (after.get(i + 1)?["type"] == "user").then_some(i)
        });
        let end = end_at.map(|i| {
            if after[i]["type"] == "idle" {
                End::Idle(after.drain(i..).next().expect("the idle row"))
            } else {
                after.truncate(i + 1);
                End::Aborted
            }
        });
        Span { user, after, end }
    }

    /// The last row is an aborted step and nothing tells yet whether the execution went on.
    fn undecided_abort(&self) -> bool {
        self.end.is_none() && self.after.last().is_some_and(aborted)
    }

    fn turn(&self, session_id: &str) -> model::Turn {
        let assistants: Vec<&Value> = self
            .after
            .iter()
            .filter(|m| m["type"] == "assistant")
            .collect();
        let text_of = |m: &Value| -> Option<String> {
            let parts: Vec<&str> = m["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        };
        let final_text = assistants
            .iter()
            .rev()
            .filter(|m| m["finish"] == "stop")
            .find_map(|m| text_of(m))
            .or_else(|| assistants.iter().rev().find_map(|m| text_of(m)));
        let error = assistants
            .iter()
            .rev()
            .map(|m| &m["error"])
            .find(|e| !e.is_null())
            .cloned();
        let (status, ended_at) = match &self.end {
            Some(End::Idle(i)) => (
                match i["outcome"].as_str() {
                    Some("succeeded") => "completed",
                    Some("failed") => "failed",
                    Some("interrupted") => "interrupted",
                    _ => "unknown",
                },
                i["time"]["created"].as_i64(),
            ),
            Some(End::Aborted) => (
                "interrupted",
                self.after
                    .last()
                    .and_then(|m| m["time"]["completed"].as_i64()),
            ),
            None => ("unknown", None),
        };
        let started = self.user["time"]["created"].as_i64();
        model::Turn {
            handle: handle(session_id),
            turn_id: text(&self.user["id"]).unwrap_or_default(),
            status,
            error,
            final_text,
            duration_ms: match (started, ended_at) {
                (Some(s), Some(e)) => Some(e - s),
                _ => None,
            },
            basis: Some(
                "history: the first idle message after the user message, or its interrupted step when a new user message followed without one or nothing runs any more; one execution may answer several user messages".into(),
            ),
        }
    }
}

/// Find the user message `msg_id` in history (newest pages first) and what followed it.
async fn find_span(svc: &Service, session_id: &str, msg_id: &str) -> Result<Option<Span>> {
    let mut cursor: Option<String> = None;
    let mut newer: Vec<Value> = Vec::new(); // newest first
    for _ in 0..SPAN_PAGES {
        let (rows, next) = svc.history(session_id, cursor.as_deref()).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        for m in rows {
            if m["id"] == msg_id {
                return Ok(Some(Span::new(m, newer.into_iter().rev().collect())));
            }
            newer.push(m);
        }
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// The user message in force before a page that starts mid-turn: none when the page
/// is empty or starts with a user message, otherwise the newest user row behind
/// `older`, the page's cursor toward older rows (a cursor is an anchor row plus a
/// direction; `type` filters any cursor read; observed on opencode 2.0.22).
async fn turn_before(
    svc: &Service,
    session_id: &str,
    oldest: Option<&Value>,
    older: Option<&str>,
) -> Result<Option<String>> {
    if oldest.is_none_or(|m| m["type"] == "user") {
        return Ok(None);
    }
    let Some(older) = older else {
        return Ok(None);
    };
    let mut page = svc
        .get(
            &format!("/api/session/{session_id}/message"),
            &[
                ("cursor", older.into()),
                ("type", "user".into()),
                ("limit", "1".into()),
            ],
        )
        .await?;
    Ok(take_data(&mut page).first().and_then(|u| text(&u["id"])))
}

impl<'a> OpenCode<'a> {
    pub fn new(store: &'a Store) -> Self {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| {
                std::env::home_dir()
                    .unwrap_or_default()
                    .join(".local/state")
            });
        OpenCode {
            store,
            registration: state.join("opencode/service.json"),
        }
    }

    async fn connect(&self) -> Result<Service> {
        transport::connect(&self.registration).await
    }

    /// A `Permission.Request` reached us (live event or `GET …/permission`). Under
    /// `observe` it stays unanswered; under `deny` it gets `reject` with a message (a
    /// message-less reject interrupts the execution and writes no idle message;
    /// DESIGN.md §6.3). Never `once` or `always`.
    async fn on_permission(&self, svc: &Service, w: &mut Watch<'_>, req: Value) -> Result<()> {
        let Some(request_id) = text(&req["id"]) else {
            return Ok(());
        };
        if w.approvals.iter().any(|a| a.request_id == request_id) {
            return Ok(());
        }
        let action = text(&req["action"]).unwrap_or_else(|| "unknown".into());
        let resources: Vec<&str> = req["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut a = Approval {
            handle: handle(w.session_id),
            turn_id: w.delivered.then(|| w.msg_id.to_string()),
            item_id: text(&req["source"]["id"]),
            request_id: json!(request_id),
            summary: format!("{action} {}", resources.join(" ")),
            kind: action,
            outcome: "pending",
            raw: req,
        };
        // Deny only requests of the turn being observed: while the message is still
        // queued, whatever asks belongs to the execution before it.
        if w.policy == ApprovalPolicy::Deny && w.delivered {
            let reply = svc
                .post(
                    &format!(
                        "/api/session/{}/permission/{request_id}/reply",
                        w.session_id
                    ),
                    &json!({"decision": "reject", "message": DENY_MESSAGE}),
                )
                .await;
            a.outcome = match reply {
                Ok(_) => "declined",
                // Gone already: one reject answers every pending request of the session
                // (DESIGN.md §6.3), or someone else replied first.
                Err(e) if e.agent_error.as_ref().is_some_and(|v| v.code == 404) => "resolved",
                Err(e) => return Err(e),
            };
        }
        record(self.store, &a);
        w.approvals.push(a);
        Ok(())
    }

    /// Inbox first, then history: a message only moves inbox → history, so reading in
    /// this order cannot miss one that moves between the two reads.
    async fn lookup(&self, svc: &Service, w: &mut Watch<'_>) -> Result<Lookup> {
        let inbox = svc
            .get(&format!("/api/session/{}/inbox", w.session_id), &[])
            .await?;
        let queued = inbox["data"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|i| i["id"] == w.msg_id);
        let span = if queued {
            None
        } else {
            match find_span(svc, w.session_id, w.msg_id).await? {
                None => return Ok(Lookup::Absent),
                s => s,
            }
        };
        if let Some(r) = w.unaccepted.take() {
            self.store
                .accept(&r, Some(w.msg_id), Some(w.msg_id), None)?;
        }
        let Some(mut span) = span else {
            return Ok(Lookup::Unfinished);
        };
        w.delivered = true;
        // An aborted step with nothing after it ended the turn if nothing runs any more;
        // a running execution may still write its `idle` or a restart notice. History is
        // re-read after the activity check so an execution that ended in between is seen.
        if span.undecided_abort() && !svc.active(w.session_id).await? {
            if let Some(again) = find_span(svc, w.session_id, w.msg_id).await? {
                span = again;
            }
            if span.undecided_abort() {
                span.end = Some(End::Aborted);
            }
        }
        Ok(match span.end {
            Some(_) => Lookup::Done(span.turn(w.session_id)),
            None => Lookup::Unfinished,
        })
    }

    /// Follow the event stream until the turn ends. History is consulted at the start
    /// (the gap before the subscription) and after every execution end for this session.
    /// Pending permission requests are read before the target is looked up and answered
    /// only if the target is still unfinished afterwards: for a finished or unknown
    /// target they belong to someone else's turn.
    async fn observe(
        &self,
        svc: &Service,
        events: &mut Events,
        w: &mut Watch<'_>,
    ) -> Result<model::Turn> {
        let pending = svc.permissions(w.session_id).await?;
        match self.lookup(svc, w).await? {
            Lookup::Done(t) => return Ok(t),
            Lookup::Absent => {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    "message not found in this session's history or inbox",
                ));
            }
            Lookup::Unfinished => {}
        }
        for req in pending {
            self.on_permission(svc, w, req).await?;
        }
        loop {
            let Some(mut ev) = events.next().await? else {
                return match self.lookup(svc, w).await? {
                    Lookup::Done(t) => Ok(t),
                    _ => Err(Error::new(
                        ErrorCode::Transport,
                        "event stream closed while waiting; outcome unknown",
                    )),
                };
            };
            if ev["data"]["sessionID"] != w.session_id {
                continue;
            }
            match ev["type"].as_str().unwrap_or_default() {
                "permission.asked" => self.on_permission(svc, w, ev["data"].take()).await?,
                "permission.replied" => {
                    resolve(self.store, &mut w.approvals, &ev["data"]["requestID"]);
                }
                "session.inbox.delivered" if ev["data"]["inboxID"] == w.msg_id => {
                    w.delivered = true;
                }
                "session.execution.succeeded"
                | "session.execution.failed"
                | "session.execution.interrupted" => {
                    if let Lookup::Done(t) = self.lookup(svc, w).await? {
                        return Ok(t);
                    }
                }
                _ => {}
            }
        }
    }

    /// Record the intent, submit it, and optionally observe the resulting turn.
    async fn submit(
        &self,
        svc: &Service,
        session_id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let handle = handle(session_id);
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let msg_id = new_msg_id();
        let mut w = Watch::new(
            session_id,
            &msg_id,
            approval_policy(self.store, &handle)?,
            false,
        );
        let work = async {
            if req.steer && !svc.active(session_id).await? {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    "no running execution to steer (the service would silently start a new one); send without --steer",
                ));
            }
            // Subscribe before submitting: the stream replays nothing.
            let mut events = match deadline {
                Some(_) => Some(svc.events().await?),
                None => None,
            };
            self.store.insert_intent(&NewIntent {
                receipt_id: &receipt_id,
                handle: &handle,
                client_msg_id: &msg_id,
                text: req.text,
                delivered_text: (req.delivered != req.text).then_some(req.delivered),
                from: req.from,
                depth: req.depth,
            })?;
            let body = json!({
                "id": msg_id,
                "text": req.delivered,
                // Always explicit: the service defaults to steer.
                "delivery": if req.steer { "steer" } else { "queue" },
                "metadata": {"agent-talk": {
                    "receipt": receipt_id,
                    "from": req.from,
                    "depth": req.depth,
                }},
            });
            svc.post(&format!("/api/session/{session_id}/prompt"), &body)
                .await
                .map_err(|e| reject(self.store, &receipt_id, e))?;
            w.submitted = true;
            self.store
                .accept(&receipt_id, Some(&msg_id), Some(&msg_id), None)?;
            match &mut events {
                None => Ok(None),
                Some(events) => self.observe(svc, events, &mut w).await.map(Some),
            }
        };
        let res = match deadline {
            Some(d) => bounded(Some(d), work).await,
            None => work.await,
        };
        let progress = w.progress();
        settle(
            self.store,
            handle,
            res,
            Some(&receipt_id),
            w.approvals,
            progress,
        )
    }

    /// Normalized messages of one OpenCode page, in the given (chronological) order.
    /// `turn` is the user message id in force before the first message of the page.
    fn messages(&self, page: &[Value], mut turn: Option<String>) -> Result<Vec<Message>> {
        let mut out = Vec::new();
        for m in page {
            let id = text(&m["id"]).unwrap_or_default();
            let timestamp = ts(&m["time"]["created"]);
            match m["type"].as_str().unwrap_or_default() {
                "user" => {
                    turn = Some(id.clone());
                    out.push(Message {
                        turn_id: id.clone(),
                        item_id: id.clone(),
                        role: "user",
                        phase: "other",
                        text: text(&m["text"]).unwrap_or_default(),
                        from: self.store.sender_of(&id)?,
                        timestamp,
                    });
                }
                "synthetic" => out.push(Message {
                    turn_id: turn.clone().unwrap_or_else(|| id.clone()),
                    item_id: id.clone(),
                    role: "user",
                    phase: "other",
                    text: format!("[synthetic] {}", text(&m["text"]).unwrap_or_default()),
                    // Written by the service itself under its own id: no sender.
                    from: None,
                    timestamp,
                }),
                "assistant" => {
                    let turn_id = turn.clone().unwrap_or_default();
                    let parts = m["content"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    let last_text = parts.iter().rposition(|p| p["type"] == "text");
                    let completed = ts(&m["time"]["completed"]).or(timestamp.clone());
                    for (i, p) in parts.iter().enumerate() {
                        let (phase, body) = match p["type"].as_str().unwrap_or_default() {
                            "text" => (
                                if m["finish"] == "stop" && Some(i) == last_text {
                                    "final"
                                } else {
                                    "commentary"
                                },
                                text(&p["text"]).unwrap_or_default(),
                            ),
                            "tool" => (
                                "other",
                                format!(
                                    "[tool {}] {} {}",
                                    p["name"].as_str().unwrap_or("?"),
                                    p["state"]["status"].as_str().unwrap_or("?"),
                                    p["state"]["input"]
                                ),
                            ),
                            _ => continue,
                        };
                        out.push(Message {
                            turn_id: turn_id.clone(),
                            item_id: format!("{id}#{i}"),
                            role: "assistant",
                            phase,
                            text: body,
                            from: None,
                            timestamp: completed.clone(),
                        });
                    }
                    if !m["error"].is_null() {
                        out.push(Message {
                            turn_id: turn_id.clone(),
                            item_id: format!("{id}#error"),
                            role: "assistant",
                            phase: "other",
                            text: format!("[error] {}", m["error"]),
                            from: None,
                            timestamp: completed,
                        });
                    }
                }
                // Hidden: idle, system, compaction, skill, shell, {agent,model,location}-switched.
                _ => {}
            }
        }
        Ok(out)
    }

    /// `read --tail n`: `desc` holds the newest OpenCode rows, newest first. They are
    /// over-fetched because idle markers are hidden and one assistant row can expand into
    /// several messages, so the trim to the last `n` visible messages happens after
    /// normalization. Returns the rows oldest first and the trimmed messages.
    fn tail(
        &self,
        mut desc: Vec<Value>,
        turn: Option<String>,
        n: usize,
    ) -> Result<(Vec<Value>, Vec<Message>)> {
        desc.reverse();
        let mut messages = self.messages(&desc, turn)?;
        messages.drain(..messages.len().saturating_sub(n));
        // Raw rows cover the same span: from the row of the first kept message on
        // (part messages are `<row id>#<index>`).
        let start = messages.first().map_or(desc.len(), |m| {
            let row = m.item_id.split('#').next().unwrap_or_default();
            desc.iter().position(|r| r["id"] == row).unwrap_or(0)
        });
        Ok((desc.split_off(start), messages))
    }
}

impl Agent for OpenCode<'_> {
    async fn caps(&self) -> Caps {
        let (version, shared, service): (Option<String>, String, Check) = match self.connect().await
        {
            Ok(s) => (
                Some(s.version),
                format!("service running at {} (pid {})", s.url, s.pid),
                Ok(()),
            ),
            Err(e) => {
                let shared = match e.code {
                    ErrorCode::NoDaemon => "service not running",
                    _ => "service not reachable",
                };
                (None, shared.into(), Err(e.message))
            }
        };
        Caps {
            agent: "opencode",
            version,
            shared: Some(shared),
            operations: ["ls", "new", "send", "read", "wait", "steer", "name"]
                .into_iter()
                .map(|name| Operation::new(name, &service))
                .collect(),
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        let svc = self.connect().await?;
        let mut q: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(c) = cursor {
            q.push(("cursor", c.to_string()));
        } else {
            q.push(("order", "desc".into()));
        }
        if let Some(cwd) = filter.cwd {
            q.push(("directory", cwd.to_string()));
        }
        if !filter.all {
            q.push(("parentID", "null".into()));
        }
        let (page, active) = tokio::try_join!(
            svc.get("/api/session", &q),
            svc.get("/api/session/active", &[])
        )?;
        let mut items = Vec::new();
        for s in page["data"].as_array().into_iter().flatten() {
            let id = text(&s["id"]).unwrap_or_default();
            let running = active["data"].get(&id).is_some();
            let handle = handle(&id);
            let owned = self.store.is_owned(&handle)?;
            items.push(Session {
                handle,
                agent: "opencode",
                id: id.clone(),
                cwd: text(&s["location"]["directory"]),
                name: text(&s["title"]),
                preview: text(&s["title"]),
                observations: Observations {
                    history: "visible",
                    loaded: "yes",
                    origin: match (owned, s["parentID"].as_str(), s["agent"].as_str()) {
                        (true, _, _) => "agent-talk".into(),
                        (false, Some(p), _) => format!("opencode child of {p}"),
                        (false, None, Some(agent)) => format!("opencode agent {agent}"),
                        (false, None, None) => "opencode".into(),
                    },
                },
                state: if running { "running" } else { "idle" },
                owned,
            });
        }
        Ok(Page {
            items,
            next_cursor: text(&page["cursor"]["next"]),
        })
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        if req.effort.is_some() {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "OpenCode takes effort as the model variant: --model provider/model#variant",
            ));
        }
        let svc = self.connect().await?;
        let mut body = json!({"location": {"directory": req.cwd}});
        if let Some(m) = req.model {
            body["model"] = model_ref(m)?;
        }
        if let Some(n) = req.name {
            body["title"] = json!(n);
        }
        if req.full_access {
            // Session rules are evaluated after the agent's, so this overrides the user's
            // configured rules (OpenCode 2.0.23, DESIGN.md §6.3).
            body["permissions"] = json!([{"action": "*", "resource": "*", "effect": "allow"}]);
        }
        let created = svc.post("/api/session", &body).await?;
        let session_id = text(&created["data"]["id"]).ok_or_else(|| {
            Error::new(
                ErrorCode::Transport,
                format!("session.create returned no id: {created}"),
            )
        })?;
        self.store
            .insert_owned(&handle(&session_id), req.cwd, &body)?;
        let send = SendRequest {
            text: req.prompt,
            delivered: req.delivered,
            steer: false,
            from: req.from,
            depth: req.depth,
        };
        self.submit(&svc, &session_id, &send, deadline).await
    }

    async fn send(
        &self,
        session_id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let svc = self.connect().await?;
        self.submit(&svc, session_id, req, deadline).await
    }

    async fn read(&self, session_id: &str, range: ReadRange) -> Result<ReadPage> {
        let svc = self.connect().await?;
        let path = format!("/api/session/{session_id}/message");
        let (raw, items, next_cursor) = match range {
            ReadRange::Forward { since, limit } => {
                let mut q = vec![("limit", limit.to_string())];
                match &since {
                    Some(c) => q.push(("cursor", c.clone())),
                    None => q.push(("order", "asc".into())),
                }
                let mut page = svc.get(&path, &q).await?;
                let rows = take_data(&mut page);
                // An empty page carries null cursors; keep the caller's so polling can continue.
                let next = if rows.is_empty() {
                    since
                } else {
                    text(&page["cursor"]["next"])
                };
                let older = text(&page["cursor"]["previous"]);
                let turn = turn_before(&svc, session_id, rows.first(), older.as_deref()).await?;
                let items = self.messages(&rows, turn)?;
                (rows, items, next)
            }
            ReadRange::Tail(n) => {
                // Newest first, page by page, until the rows hold n visible messages.
                let mut desc = Vec::new();
                let mut count = 0;
                let mut cursor: Option<String> = None;
                // Cursor toward the rows older than everything fetched.
                let mut older: Option<String> = None;
                loop {
                    let (rows, next) = svc.history(session_id, cursor.as_deref()).await?;
                    if rows.is_empty() {
                        break;
                    }
                    // Visibility does not depend on order or turn.
                    count += self.messages(&rows, None)?.len();
                    desc.extend(rows);
                    older = next;
                    match &older {
                        Some(c) if count < n as usize => cursor = Some(c.clone()),
                        _ => break,
                    }
                }
                let turn = turn_before(&svc, session_id, desc.last(), older.as_deref()).await?;
                let (rows, items) = self.tail(desc, turn, n as usize)?;
                (rows, items, None)
            }
        };
        Ok(ReadPage {
            messages: Page { items, next_cursor },
            raw,
        })
    }

    async fn wait(
        &self,
        session_id: &str,
        target: &WaitTarget,
        deadline: Instant,
    ) -> Result<Outcome> {
        let handle = handle(session_id);
        // `submit` records the message id as both client message id and turn id, so a
        // turn id is the message id.
        let (msg_id, receipt) = match target {
            WaitTarget::Turn(t) => (t.clone(), self.store.receipt_by_turn(&handle, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &handle, r)?;
                (rec.client_msg_id.clone(), Some(rec))
            }
        };
        let mut w = Watch::new(
            session_id,
            &msg_id,
            approval_policy(self.store, &handle)?,
            true,
        );
        w.unaccepted = receipt
            .as_ref()
            .filter(|r| r.state != ReceiptState::Accepted)
            .map(|r| r.receipt_id.clone());
        let receipt_id = receipt.map(|r| r.receipt_id);
        let svc = self.connect().await?;
        let work = async {
            let mut events = svc.events().await?;
            self.observe(&svc, &mut events, &mut w).await.map(Some)
        };
        let res = bounded(Some(deadline), work).await;
        let progress = w.progress();
        settle(
            self.store,
            handle,
            res,
            receipt_id.as_deref(),
            w.approvals,
            progress,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn msg_ids_are_ordered_and_well_formed() {
        let a = new_msg_id();
        std::thread::sleep(Duration::from_millis(2));
        let b = new_msg_id();
        assert!(a.starts_with("msg_"));
        assert!(a[4..16].chars().all(|c| c.is_ascii_hexdigit()));
        assert!(a[4..16] < b[4..16], "{a} {b}");
    }

    #[test]
    fn model_spec_parses() {
        assert_eq!(
            model_ref("newapi2/deepseek-flash").unwrap(),
            json!({"providerID": "newapi2", "id": "deepseek-flash"})
        );
        assert_eq!(
            model_ref("openai/gpt-6.1-sol#high").unwrap(),
            json!({"providerID": "openai", "id": "gpt-6.1-sol", "variant": "high"})
        );
        assert!(model_ref("sonnet").is_err());
    }

    /// Shapes observed on opencode 2.0.22.
    #[test]
    fn span_turn_from_history() {
        let user = json!({"id":"msg_u","time":{"created":1000},"text":"reply one","type":"user"});
        let assistant = json!({"id":"msg_a","time":{"created":1200,"completed":1900},"type":"assistant",
            "agent":"build","model":{"id":"m","providerID":"p"},"content":[{"type":"text","text":"one"}],"finish":"stop"});
        let idle =
            json!({"id":"msg_i","time":{"created":2000},"type":"idle","outcome":"succeeded"});
        // Rows of a later execution are not part of the turn.
        let later = json!({"id":"msg_u2","time":{"created":5000},"text":"two","type":"user"});
        let span = Span::new(user.clone(), vec![assistant.clone(), idle, later.clone()]);
        let t = span.turn("ses_x");
        assert_eq!(t.status, "completed");
        assert_eq!(t.final_text.as_deref(), Some("one"));
        assert_eq!(t.duration_ms, Some(1000));
        assert_eq!(t.handle, "opencode:ses_x");
        assert_eq!(t.turn_id, "msg_u");

        // A user interrupt: aborted step, then its own idle.
        let interrupted = json!({"id":"msg_b","time":{"created":1200,"completed":1900},"type":"assistant","agent":"build",
            "model":{"id":"m","providerID":"p"},"finish":"error","error":{"type":"aborted","message":"Step interrupted"},
            "content":[{"type":"tool","id":"call_1","name":"shell","state":{"status":"error","input":{"command":"sleep 25"}}}]});
        let idle_interrupted =
            json!({"id":"msg_j","time":{"created":3000},"type":"idle","outcome":"interrupted"});
        let span = Span::new(user.clone(), vec![interrupted.clone(), idle_interrupted]);
        let t = span.turn("ses_x");
        assert_eq!(t.status, "interrupted");
        assert_eq!(t.final_text, None);
        assert_eq!(t.error.unwrap()["type"], "aborted");
        assert_eq!(t.duration_ms, Some(2000));

        // A shutdown-reason interrupt (DESIGN.md §6.3): no idle; the next
        // execution's rows must not become this turn's reply.
        let span = Span::new(
            user.clone(),
            vec![interrupted.clone(), later, assistant, idle_succeeded(6000)],
        );
        let t = span.turn("ses_x");
        assert_eq!(t.status, "interrupted");
        assert_eq!(t.final_text, None);
        assert_eq!(t.duration_ms, Some(900));
        assert_eq!(span.after.len(), 1);

        // Resumed after a service restart: the turn goes on.
        let restart = json!({"id":"msg_s","type":"synthetic","text":"The server restarted…","metadata":{"notice":"restart"}});
        let span = Span::new(user.clone(), vec![interrupted.clone(), restart]);
        assert!(!span.undecided_abort() && span.end.is_none());

        // Nothing after the aborted step yet: undecided.
        let span = Span::new(user, vec![interrupted]);
        assert!(span.undecided_abort());
    }

    fn idle_succeeded(at: i64) -> Value {
        json!({"id":"msg_k","time":{"created":at},"type":"idle","outcome":"succeeded"})
    }

    #[test]
    fn messages_group_by_user_message_and_phase() {
        let store = Store::memory();
        let oc = OpenCode::new(&store);
        let page = vec![
            json!({"id":"msg_u1","time":{"created":1},"text":"slow","type":"user"}),
            json!({"id":"msg_a1","time":{"created":2,"completed":3},"type":"assistant","agent":"build","model":{"id":"m","providerID":"p"},
                "finish":"tool-calls","content":[{"type":"reasoning","text":"hmm"},{"type":"tool","name":"shell","state":{"status":"completed","input":{"command":"sleep 1"}}}]}),
            json!({"id":"msg_u2","time":{"created":4},"text":"two","type":"user"}),
            json!({"id":"msg_a2","time":{"created":5,"completed":6},"type":"assistant","agent":"build","model":{"id":"m","providerID":"p"},
                "finish":"stop","content":[{"type":"text","text":"working"},{"type":"text","text":"two"}]}),
            json!({"id":"msg_i","time":{"created":7},"type":"idle","outcome":"succeeded"}),
        ];
        let m = oc.messages(&page, None).unwrap();
        let brief: Vec<(&str, &str, &str, &str)> = m
            .iter()
            .map(|m| (m.turn_id.as_str(), m.role, m.phase, m.text.as_str()))
            .collect();
        assert_eq!(
            brief,
            vec![
                ("msg_u1", "user", "other", "slow"),
                (
                    "msg_u1",
                    "assistant",
                    "other",
                    "[tool shell] completed {\"command\":\"sleep 1\"}"
                ),
                ("msg_u2", "user", "other", "two"),
                ("msg_u2", "assistant", "commentary", "working"),
                ("msg_u2", "assistant", "final", "two"),
            ]
        );
        // A page starting mid-turn takes the turn id handed in.
        let m = oc.messages(&page[1..2], Some("msg_u0".into())).unwrap();
        assert_eq!(m[0].turn_id, "msg_u0");
    }

    /// `--tail` counts visible messages, not OpenCode rows (the hidden
    /// idle marker was counted, so `--tail 2` printed only the assistant line).
    #[test]
    fn tail_counts_visible_messages() {
        let store = Store::memory();
        let oc = OpenCode::new(&store);
        // Newest first, as `GET …/message?order=desc` returns it.
        let desc = vec![
            json!({"id":"msg_i2","time":{"created":7},"type":"idle","outcome":"succeeded"}),
            json!({"id":"msg_a2","time":{"created":5,"completed":6},"type":"assistant","finish":"stop","content":[{"type":"text","text":"two"}]}),
            json!({"id":"msg_u2","time":{"created":4},"text":"second","type":"user"}),
            json!({"id":"msg_i1","time":{"created":3},"type":"idle","outcome":"succeeded"}),
            json!({"id":"msg_a1","time":{"created":2,"completed":2},"type":"assistant","finish":"stop","content":[{"type":"text","text":"one"}]}),
            json!({"id":"msg_u1","time":{"created":1},"text":"first","type":"user"}),
        ];
        let (rows, m) = oc.tail(desc, None, 2).unwrap();
        // Raw rows start where the kept messages start.
        assert_eq!(rows[0]["id"], "msg_u2");
        let brief: Vec<(&str, &str)> = m.iter().map(|m| (m.role, m.text.as_str())).collect();
        assert_eq!(brief, vec![("user", "second"), ("assistant", "two")]);
    }
}
