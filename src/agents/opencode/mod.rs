//! OpenCode adapter: HTTP client of the user's background service (`opencode serve
//! --service`), which every OpenCode client shares. Built against opencode 2.0.22; the
//! verified behaviour is in DESIGN.md §6.3.
//!
//! OpenCode has no turn id. agent-talk's turn id is the user message id, which the client
//! chooses (`session.prompt.id`); a turn ends at the first `idle` message after that user
//! message in history (`outcome` = status), or at its aborted step when an interrupt with
//! reason `shutdown` wrote no `idle` (`history::Span`). Live, `session.execution.{succeeded,
//! failed,interrupted}` says "an execution ended"; history is re-read to confirm, because the
//! SSE stream replays nothing and one execution can serve several user messages.

mod history;
mod transport;

use self::history::{find_span, messages, turn_before};
use self::transport::{Event, Events, ModelRef, Service, SessionInfo};
use crate::agents::history::{ReadPage, ReadQuery, RemoteHistory};
use crate::agents::lifecycle::{
    ApprovalPolicy, approval_policy, bounded, record, reject, resolve, settle, wait_receipt,
};
use crate::agents::{
    Agent, AgentStatus, Check, ListFilter, Operation, Outgoing, SendRequest, StartRequest, Wait,
    WaitTarget,
};
use crate::model::{
    self, Approval, ApprovalOutcome, Error, ErrorCode, History, Loaded, Model, Observations,
    Outcome, Page, Phase, Progress, ReceiptState, Result, Session, SessionState,
};
use crate::store::{NewIntent, Store};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::time::Instant;

const DENY_MESSAGE: &str = "Declined by agent-talk: nobody answers approval requests in a session agent-talk started. Continue without this action, or report that it needs approval.";

pub struct OpenCode<'a> {
    store: &'a Store,
    /// `$XDG_STATE_HOME/opencode/service.json`, written by the service itself.
    registration: PathBuf,
}

enum Lookup {
    Done(model::Turn),
    /// In history with no end after it yet, or still in the session's inbox (queued behind
    /// another execution, or dormant).
    Unfinished,
    Absent,
}

/// How far the observed message got.
#[derive(Clone, Copy, PartialEq)]
enum Delivery {
    /// The service has not accepted the prompt yet.
    Unsent,
    /// Accepted (or submitted by an earlier command) and not seen in an execution yet.
    Queued,
    /// The message left the inbox and is part of an execution.
    Delivered,
}

/// Per-command observation state.
struct Watch<'t> {
    session_id: &'t str,
    msg_id: &'t str,
    policy: ApprovalPolicy,
    delivery: Delivery,
    /// A receipt whose submission outcome was lost (`unknown`): accepted once the
    /// inbox or history shows the message.
    unaccepted: Option<String>,
    approvals: Vec<Approval>,
}

impl<'t> Watch<'t> {
    fn new(
        session_id: &'t str,
        msg_id: &'t str,
        policy: ApprovalPolicy,
        delivery: Delivery,
    ) -> Self {
        Watch {
            session_id,
            msg_id,
            policy,
            delivery,
            unaccepted: None,
            approvals: Vec::new(),
        }
    }

    /// Progress reported when the outcome is not known yet.
    fn progress(&self) -> Progress {
        match self.delivery {
            Delivery::Unsent => Progress::Unknown,
            Delivery::Queued => Progress::Pending,
            Delivery::Delivered => Progress::Running,
        }
    }

    /// The inbox or history shows the message: a receipt left `unknown` is accepted.
    fn found(&mut self, store: &Store) -> Result<()> {
        if let Some(r) = self.unaccepted.take() {
            store.accept(&r, Some(self.msg_id), Some(self.msg_id), None)?;
        }
        Ok(())
    }
}

fn handle(id: &str) -> String {
    format!("opencode:{id}")
}

/// A message id in OpenCode's own ascending format, so clients that sort by id keep
/// agent-talk's messages in order: 12 hex digits of the low 48 bits of
/// `milliseconds * 0x1000 + counter` (the service's own scheme, read in its source on
/// 2.0.22; a random counter here), then 14 base62 characters.
fn new_msg_id() -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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

/// The rules of a session agent-talk started, so that the default agent never asks
/// (DESIGN.md §6.3): its rules from the first `ask` on, with every `ask` turned into `deny`,
/// or into `allow` under full access, then a denied question tool, since nobody answers it.
/// The service evaluates the agent's rules, then the session's, and the last match wins, so
/// every other answer stays, the user's denials included. The rules before the first `ask`
/// decide the same either way and are left out, so sub-agents, which inherit session rules,
/// keep their own there.
fn session_rules(agent_rules: &[Value], full_access: bool) -> Value {
    let answer = if full_access { "allow" } else { "deny" };
    let mut rules: Vec<Value> = agent_rules
        .iter()
        .skip_while(|r| r["effect"] != "ask")
        .map(|r| {
            let mut r = r.clone();
            if r["effect"] == "ask" {
                r["effect"] = json!(answer);
            }
            r
        })
        .collect();
    rules.push(json!({"action": "question", "resource": "*", "effect": "deny"}));
    json!(rules)
}

/// The location of an existing session.
fn location(session: &SessionInfo, id: &str) -> Result<String> {
    session.directory().map(String::from).ok_or_else(|| {
        Error::new(
            ErrorCode::Transport,
            format!("session.get returned no location for {id}"),
        )
    })
}

/// The user's default model for `cwd`, which an effort without a model applies to.
async fn default_model(svc: &Service, cwd: &str) -> Result<ModelRef> {
    svc.default_model(cwd).await?.ok_or_else(|| {
        Error::new(
            ErrorCode::Precondition,
            format!("OpenCode has no default model for {cwd}; pass --model"),
        )
    })
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

    /// A permission request (`Permission.Request`) reached us (live event or
    /// `GET …/permission`). Under `observe` it stays unanswered; under `deny` it gets
    /// `reject` with a message (a message-less reject interrupts the execution and writes no
    /// idle message; DESIGN.md §6.3). Never `once` or `always`.
    async fn on_permission(&self, svc: &Service, w: &mut Watch<'_>, req: Value) -> Result<()> {
        // The reply names the request: one without an id cannot be answered.
        let Some(request_id) = req["id"].as_str().map(String::from) else {
            return Ok(());
        };
        if w.approvals
            .iter()
            .any(|a| a.request_id == request_id.as_str())
        {
            return Ok(());
        }
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
        let action = text(&req["action"]).unwrap_or_else(|| "unknown".into());
        let resources: Vec<&str> = req["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let delivered = w.delivery == Delivery::Delivered;
        let mut a = Approval {
            handle: handle(w.session_id),
            turn_id: delivered.then(|| w.msg_id.to_string()),
            // The tool call that asks (`call_…`).
            item_id: text(&req["source"]["id"]),
            request_id: json!(request_id),
            summary: format!("{action} {}", resources.join(" ")),
            kind: action,
            outcome: ApprovalOutcome::Pending,
            raw: req,
        };
        // Deny only requests of the turn being observed: while the message is still
        // queued, whatever asks belongs to the execution before it.
        // A reject that fails leaves the request pending, recorded as such (DESIGN.md §4).
        let mut failed = None;
        if w.policy == ApprovalPolicy::Deny && delivered {
            match svc.reject(w.session_id, &request_id, DENY_MESSAGE).await {
                Ok(()) => a.outcome = ApprovalOutcome::Declined,
                // Gone already: one reject answers every pending request of the session
                // (DESIGN.md §6.3), or someone else replied first.
                Err(e) if e.agent_error.as_ref().is_some_and(|v| v.code == 404) => {
                    a.outcome = ApprovalOutcome::Resolved;
                }
                Err(e) => failed = Some(e),
            }
        }
        record(self.store, &a);
        w.approvals.push(a);
        failed.map_or(Ok(()), Err)
    }

    /// Inbox first, then history: a message only moves inbox → history, so reading in
    /// this order cannot miss one that moves between the two reads.
    async fn lookup(&self, svc: &Service, w: &mut Watch<'_>) -> Result<Lookup> {
        if svc
            .inbox(w.session_id)
            .await?
            .iter()
            .any(|id| id == w.msg_id)
        {
            w.found(self.store)?;
            return Ok(Lookup::Unfinished);
        }
        let Some(mut span) = find_span(svc, w.session_id, w.msg_id).await? else {
            return Ok(Lookup::Absent);
        };
        w.found(self.store)?;
        w.delivery = Delivery::Delivered;
        // An aborted step with nothing after it ended the turn if nothing runs any more;
        // a running execution may still write its `idle` or a restart notice. History is
        // re-read after the activity check so an execution that ended in between is seen.
        if span.undecided_abort() && !svc.active().await?.contains(w.session_id) {
            if let Some(again) = find_span(svc, w.session_id, w.msg_id).await? {
                span = again;
            }
            span.end_at_abort();
        }
        Ok(span
            .turn(w.session_id)
            .map_or(Lookup::Unfinished, Lookup::Done))
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
        let session_id = w.session_id;
        loop {
            let Some(ev) = events.next().await? else {
                return match self.lookup(svc, w).await? {
                    Lookup::Done(t) => Ok(t),
                    _ => Err(Error::new(
                        ErrorCode::Transport,
                        "event stream closed while waiting; outcome unknown",
                    )),
                };
            };
            match ev {
                Event::PermissionAsked { data } if data["sessionID"] == session_id => {
                    self.on_permission(svc, w, data).await?;
                }
                Event::PermissionReplied { data } if data.session_id == session_id => {
                    let id = json!(data.request_id);
                    resolve(self.store, &mut w.approvals, |a| a.request_id == id);
                }
                Event::InboxDelivered { data }
                    if data.session_id == session_id && data.inbox_id == w.msg_id =>
                {
                    w.delivery = Delivery::Delivered;
                }
                Event::ExecutionEnded { data } if data.session_id == session_id => {
                    if let Lookup::Done(t) = self.lookup(svc, w).await? {
                        return Ok(t);
                    }
                }
                _ => {}
            }
        }
    }

    /// Record the intent, submit it (into the running execution with `steer`), and
    /// optionally observe the resulting turn.
    async fn submit(
        &self,
        svc: &Service,
        session_id: &str,
        message: &Outgoing<'_>,
        steer: bool,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let handle = handle(session_id);
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let msg_id = new_msg_id();
        let mut w = Watch::new(
            session_id,
            &msg_id,
            approval_policy(self.store, &handle)?,
            Delivery::Unsent,
        );
        let work = async {
            if steer && !svc.active().await?.contains(session_id) {
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
                message,
            })?;
            let body = json!({
                "id": msg_id,
                "text": message.delivered,
                // Always explicit: the service defaults to steer.
                "delivery": if steer { "steer" } else { "queue" },
                "metadata": {"agent-talk": {
                    "receipt": receipt_id,
                    "from": message.from,
                    "depth": message.depth,
                }},
            });
            svc.prompt(session_id, &body)
                .await
                .map_err(|e| reject(self.store, &receipt_id, e))?;
            w.delivery = Delivery::Queued;
            self.store
                .accept(&receipt_id, Some(&msg_id), Some(&msg_id), None)?;
            match &mut events {
                None => Ok(None),
                Some(events) => self.observe(svc, events, &mut w).await.map(Some),
            }
        };
        // Bounded even without a deadline, so that Ctrl-C or SIGTERM during the submission
        // settles the receipt `unknown` (DESIGN.md §4).
        let res = bounded(deadline, work).await;
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
}

impl Agent for OpenCode<'_> {
    async fn models(&self) -> Result<Vec<Model>> {
        // Every model of every configured provider; a `variant` is the model's effort.
        let models = self.connect().await?.models().await?;
        Ok(models
            .into_iter()
            .map(|m| Model {
                id: format!("{}/{}", m.provider, m.id),
                efforts: m.variants.into_iter().map(|v| v.id).collect(),
            })
            .collect())
    }

    async fn status(&self) -> AgentStatus {
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
        AgentStatus {
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
        let ((sessions, next_cursor), active) = tokio::try_join!(
            svc.sessions(limit, cursor, filter.cwd, !filter.all),
            svc.active()
        )?;
        let mut items = Vec::new();
        for s in sessions {
            let handle = handle(&s.id);
            let owned = self.store.is_owned(&handle)?;
            let title = s.title.clone();
            items.push(Session {
                handle,
                agent: "opencode",
                cwd: s.directory().map(String::from),
                name: title.clone(),
                preview: title,
                observations: Observations {
                    history: History::Visible,
                    loaded: Loaded::Yes,
                    origin: match (owned, &s.parent_id, &s.agent) {
                        (true, _, _) => "agent-talk".into(),
                        (false, Some(p), _) => format!("opencode child of {p}"),
                        (false, None, Some(agent)) => format!("opencode agent {agent}"),
                        (false, None, None) => "opencode".into(),
                    },
                },
                state: if active.contains(&s.id) {
                    SessionState::Running
                } else {
                    SessionState::Idle
                },
                owned,
                id: s.id,
            });
        }
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, wait: Wait) -> Result<Outcome> {
        let deadline = wait.deadline();
        let svc = self.connect().await?;
        // The service answers a location's first requests before its configuration is
        // applied: no default model, agents without the user's rules (opencode 2.0.23).
        // `integration.list` waits for plugin activation, which applies it.
        svc.load_location(req.cwd).await?;
        let mut body = json!({"location": {"directory": req.cwd}});
        let mut model = req
            .settings
            .model
            .as_deref()
            .map(ModelRef::parse)
            .transpose()?;
        if let Some(e) = &req.settings.effort {
            // The effort is the model's variant, so it needs a model: the user's default.
            let m = match model {
                Some(m) => m,
                None => default_model(&svc, req.cwd).await?,
            };
            model = Some(ModelRef {
                variant: Some(e.clone()),
                ..m
            });
        }
        if let Some(m) = model {
            body["model"] = json!(m);
        }
        if let Some(n) = req.name {
            body["title"] = json!(n);
        }
        // Session rules, so that the service itself never waits for an answer here
        // (DESIGN.md §4, §6.3).
        let rules = svc.default_agent_rules(req.cwd).await?;
        body["permissions"] = session_rules(&rules, req.settings.full_access);
        let session_id = svc.create_session(&body).await?;
        self.store
            .insert_owned(&handle(&session_id), req.cwd, req.settings)?;
        self.submit(&svc, &session_id, &req.message, false, deadline)
            .await
    }

    async fn send(&self, session_id: &str, req: &SendRequest<'_>, wait: Wait) -> Result<Outcome> {
        let deadline = wait.deadline();
        let svc = self.connect().await?;
        // The session stores its model, variant and rules, and the service applies a change
        // from the next step on, so only the change is sent, before the message is recorded:
        // a refused change is a plain error (DESIGN.md §4, §6.3). The prompt has no model
        // field.
        let change = req.change;
        if change.model.is_some() || change.effort.is_some() {
            let model = if let Some(m) = &change.model {
                ModelRef::parse(m)?
            } else {
                // An effort alone applies to the session's model, the user's default when the
                // session has none.
                let session = svc.session(session_id).await?;
                if let Some(m) = session.model {
                    m
                } else {
                    // The default model as the location's configuration sets it, read once
                    // that is applied, as in `start`.
                    let cwd = location(&session, session_id)?;
                    svc.load_location(&cwd).await?;
                    default_model(&svc, &cwd).await?
                }
            };
            // The session's effort (the change included), kept across a model change; without
            // one the service selects the model's default variant.
            let model = ModelRef {
                variant: req.settings.and_then(|s| s.effort.clone()),
                ..model
            };
            svc.set_model(session_id, &model).await?;
        }
        if change.full_access {
            // The default agent's rules as they are now, read once the location's
            // configuration is applied, as in `start`.
            let cwd = location(&svc.session(session_id).await?, session_id)?;
            svc.load_location(&cwd).await?;
            let rules = svc.default_agent_rules(&cwd).await?;
            svc.set_permissions(session_id, &session_rules(&rules, true))
                .await?;
        }
        self.submit(&svc, session_id, &req.message, req.steer, deadline)
            .await
    }

    async fn read(&self, session_id: &str, q: &ReadQuery) -> Result<ReadPage> {
        let svc = self.connect().await?;
        let fetch = async |cursor: Option<&str>| svc.history(session_id, cursor).await;
        // Which messages the page shows does not depend on their turn.
        let history = RemoteHistory::fetch(q, fetch, |rows| Ok(messages(rows, None))).await?;
        let turn = turn_before(&svc, session_id, &history).await?;
        let messages = messages(history.records(), turn);
        let mut page = history.page(q, messages)?;
        for m in &mut page.messages.items {
            if m.phase == Phase::Prompt {
                m.from = self.store.sender_of(&m.item_id)?;
            }
        }
        Ok(page)
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
            Delivery::Queued,
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
            json!(ModelRef::parse("newapi2/deepseek-flash").unwrap()),
            json!({"providerID": "newapi2", "id": "deepseek-flash"})
        );
        // OpenRouter model ids contain a slash themselves.
        assert_eq!(
            json!(ModelRef::parse("openrouter/deepseek/deepseek-v4-flash").unwrap()),
            json!({"providerID": "openrouter", "id": "deepseek/deepseek-v4-flash"})
        );
        assert!(ModelRef::parse("sonnet").is_err());
    }

    /// What asks is denied, or allowed under full access; everything else keeps its answer,
    /// including a narrower allow or deny after an ask; rules before the first ask are left
    /// to the agent (shapes from `GET /api/agent`, opencode 2.2.0).
    #[test]
    fn session_rules_answer_what_would_ask() {
        let agent = vec![
            json!({"action": "*", "resource": "*", "effect": "allow"}),
            json!({"action": "edit", "resource": "*", "effect": "ask"}),
            json!({"action": "edit", "resource": "src/*", "effect": "allow"}),
            json!({"action": "edit", "resource": "*.lock", "effect": "deny"}),
            json!({"action": "browser", "resource": "*", "effect": "deny"}),
        ];
        let question = json!({"action": "question", "resource": "*", "effect": "deny"});
        let rules = |asked: &str| {
            json!([
                {"action": "edit", "resource": "*", "effect": asked},
                agent[2],
                agent[3],
                agent[4],
                question,
            ])
        };
        assert_eq!(session_rules(&agent, false), rules("deny"));
        assert_eq!(session_rules(&agent, true), rules("allow"));
        assert_eq!(session_rules(&agent[..1], true), json!([question]));
    }
}
