//! What one command observes of a prompt on its ACP connection (DESIGN.md §6.4): its
//! acceptance, the turn it runs as, the agent text, the permission requests it raises, and
//! the end of the turn. The authority for the end is the `session/prompt` response, or for a
//! steer message the joined turn's `_x.ai/session/prompt_complete` (DESIGN.md §7).

use super::acp::{Conn, Event, Response};
use super::handle;
use super::updates::{self, SessionUpdate, ToolCall};
use crate::agents::decode::{entries, or_default};
use crate::agents::history::first_line;
use crate::agents::lifecycle::{ApprovalPolicy, record};
use crate::model::{self, Approval, ApprovalOutcome, Error, ErrorCode, Progress, Result};
use crate::store::Store;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

const REQUEST_PERMISSION: &str = "session/request_permission";

/// Which prompt is watched.
enum Kind {
    /// This command's own `session/prompt`, submitted with `prompt_id`. `response` is its
    /// response until it arrives.
    Prompt {
        prompt_id: String,
        response: Option<Response>,
        accepted: bool,
        running: bool,
    },
    /// A steer message the agent acknowledged (`_x.ai/interject`): its delivered text,
    /// whether `_x.ai/session/interjection` reported it yet, and the turn it joined once
    /// known.
    Steer {
        text: String,
        delivered: bool,
        joined: Option<String>,
    },
}

pub struct Watch<'s> {
    store: &'s Store,
    session_id: String,
    receipt_id: String,
    kind: Kind,
    policy: ApprovalPolicy,
    /// Attached through a leader, where other clients' permission requests arrive too.
    shared: bool,
    /// The prompt the session runs now (`runningPromptId`).
    running_prompt: Option<String>,
    /// Agent text of the prompt since its last tool call.
    text: String,
    /// The prompt's tool calls.
    tools: HashSet<String>,
    pub approvals: Vec<Approval>,
}

/// How the watched turn ended.
pub struct End {
    pub turn_id: String,
    /// `{stopReason, _meta…}`: the `session/prompt` result, or the joined turn's
    /// `_x.ai/session/prompt_complete`; for a prompt also an error response.
    pub result: Result<Value>,
}

/// `_x.ai/queue/changed`: the queued prompts (entry id = prompt id) and the running one.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueChanged {
    #[serde(default, deserialize_with = "entries")]
    entries: Vec<QueueEntry>,
    #[serde(default, deserialize_with = "or_default")]
    running_prompt_id: Option<String>,
}

#[derive(Deserialize)]
struct QueueEntry {
    #[serde(default, deserialize_with = "or_default")]
    id: Option<String>,
}

/// `session/request_permission`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionRequest {
    #[serde(default, deserialize_with = "or_default")]
    tool_call: ToolCall,
    #[serde(default, deserialize_with = "entries")]
    options: Vec<PermissionOption>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOption {
    #[serde(default, deserialize_with = "or_default")]
    option_id: Option<String>,
    #[serde(default, deserialize_with = "or_default")]
    kind: String,
}

impl<'s> Watch<'s> {
    /// This command's own `session/prompt`, submitted with `prompt_id`; `response` is its
    /// response.
    pub fn prompt(
        store: &'s Store,
        session_id: &str,
        receipt_id: &str,
        prompt_id: &str,
        response: Response,
        policy: ApprovalPolicy,
        shared: bool,
    ) -> Self {
        let kind = Kind::Prompt {
            prompt_id: prompt_id.to_string(),
            response: Some(response),
            accepted: false,
            running: false,
        };
        Watch::new(store, session_id, receipt_id, kind, policy, shared, None)
    }

    /// A steer message `text` the leader acknowledged, sent while the session ran the
    /// prompt `running` (`runningPromptId` at load); the joined turn is learned later.
    pub fn steer(
        store: &'s Store,
        session_id: &str,
        receipt_id: &str,
        text: &str,
        running: Option<String>,
        policy: ApprovalPolicy,
    ) -> Self {
        let kind = Kind::Steer {
            text: text.to_string(),
            delivered: false,
            joined: None,
        };
        Watch::new(store, session_id, receipt_id, kind, policy, true, running)
    }

    fn new(
        store: &'s Store,
        session_id: &str,
        receipt_id: &str,
        kind: Kind,
        policy: ApprovalPolicy,
        shared: bool,
        running_prompt: Option<String>,
    ) -> Self {
        Watch {
            store,
            session_id: session_id.to_string(),
            receipt_id: receipt_id.to_string(),
            kind,
            policy,
            shared,
            running_prompt,
            text: String::new(),
            tools: HashSet::new(),
            approvals: Vec::new(),
        }
    }

    /// The watched prompt's id: a prompt's own, or the turn a steer message joined.
    fn prompt_id(&self) -> Option<&str> {
        match &self.kind {
            Kind::Prompt { prompt_id, .. } => Some(prompt_id),
            Kind::Steer { joined, .. } => joined.as_deref(),
        }
    }

    fn running(&self) -> bool {
        match &self.kind {
            Kind::Prompt { running, .. } => *running,
            Kind::Steer { joined, .. } => joined.is_some(),
        }
    }

    /// The agent took the message: always for a steer message, whose ack is the acceptance.
    pub fn accepted(&self) -> bool {
        match &self.kind {
            Kind::Prompt { accepted, .. } => *accepted,
            Kind::Steer { .. } => true,
        }
    }

    /// How far the prompt got, for an observation that ended without a result.
    pub fn progress(&self) -> Progress {
        if self.running() {
            Progress::Running
        } else if self.accepted() {
            Progress::Pending
        } else {
            Progress::Unknown
        }
    }

    pub fn final_text(&self) -> Option<String> {
        (!self.text.is_empty()).then(|| self.text.clone())
    }

    /// Record the acceptance of a prompt, once.
    fn accept(&mut self) -> Result<()> {
        if let Kind::Prompt {
            prompt_id,
            accepted: accepted @ false,
            ..
        } = &mut self.kind
        {
            let p = Some(prompt_id.as_str());
            self.store.accept(&self.receipt_id, p, p, None)?;
            *accepted = true;
        }
        Ok(())
    }

    /// A steer message joined the turn `prompt_id`.
    fn join(&mut self, prompt_id: String) -> Result<()> {
        self.store
            .accept(&self.receipt_id, None, Some(&prompt_id), None)?;
        if let Kind::Steer { joined, .. } = &mut self.kind {
            *joined = Some(prompt_id);
        }
        Ok(())
    }

    /// Events until the turn ends.
    pub async fn finish(&mut self, conn: &mut Conn) -> Result<End> {
        loop {
            if let Some(end) = self.step(conn).await? {
                return Ok(end);
            }
        }
    }

    /// Events until the prompt runs; whether the turn ended first.
    pub async fn until_running(&mut self, conn: &mut Conn) -> Result<bool> {
        while !self.running() {
            if self.step(conn).await?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// One event, or the prompt's response; `Some` once the turn ended.
    async fn step(&mut self, conn: &mut Conn) -> Result<Option<End>> {
        enum Next {
            Response(End),
            Event(Option<Event>),
        }
        let next = match &mut self.kind {
            Kind::Prompt {
                prompt_id,
                response: Some(response),
                ..
            } => tokio::select! {
                biased;
                result = response => Next::Response(End { turn_id: prompt_id.clone(), result }),
                ev = conn.next_event() => Next::Event(ev),
            },
            _ => Next::Event(conn.next_event().await),
        };
        match next {
            Next::Response(end) => {
                if let Kind::Prompt { response, .. } = &mut self.kind {
                    *response = None;
                }
                if end.result.is_ok() {
                    self.accept()?;
                }
                Ok(Some(end))
            }
            Next::Event(Some(Event::Request(r))) if r.method == REQUEST_PERMISSION => {
                let id = r.id.clone();
                if let Some((a, answer)) = self.on_permission(r.id, r.params) {
                    // Recorded as declined only once the answer is written.
                    if let Some(answer) = answer {
                        conn.respond(&id, answer).await?;
                    }
                    self.add_approval(a);
                }
                Ok(None)
            }
            // agent-talk offers no client capabilities.
            Next::Event(Some(Event::Request(r))) => {
                conn.respond_error(&r.id, -32601, "not supported by agent-talk")
                    .await?;
                Ok(None)
            }
            Next::Event(Some(Event::Notification { method, params })) => {
                self.on_notification(&method, &params)
            }
            // Never reached with a response pending: the reader drops the waiters before
            // the event channels close, so the biased select above has already taken the
            // response (or its loss).
            Next::Event(None) => Err(Error::new(
                ErrorCode::Transport,
                "grok agent exited while agent-talk was observing; outcome unknown",
            )),
        }
    }

    /// A notification; `Some` when it ends a steer message's turn.
    fn on_notification(&mut self, method: &str, params: &Value) -> Result<Option<End>> {
        if params["sessionId"] != self.session_id.as_str() {
            return Ok(None);
        }
        match method {
            // The queue lists the prompt (its id is the promptId) and then names it
            // running; the first is the earliest proof of acceptance (direct mode:
            // about 20 ms after the request).
            "_x.ai/queue/changed" => {
                let Ok(q) = QueueChanged::deserialize(params) else {
                    return Ok(None);
                };
                if let Kind::Prompt { prompt_id, .. } = &self.kind {
                    let listed = q.entries.iter().any(|e| e.id.as_ref() == Some(prompt_id));
                    let runs = q.running_prompt_id.as_ref() == Some(prompt_id);
                    if listed || runs {
                        self.accept()?;
                    }
                    if runs && let Kind::Prompt { running, .. } = &mut self.kind {
                        *running = true;
                    }
                }
                self.running_prompt = q.running_prompt_id;
            }
            "_x.ai/session/interjection" => {
                if let Kind::Steer {
                    text,
                    delivered: delivered @ false,
                    ..
                } = &mut self.kind
                    && params["text"].as_str() == Some(text.as_str())
                {
                    *delivered = true;
                    // Joined the running turn; with none running, Grok starts a fallback
                    // turn, named by its first update.
                    if let Some(p) = self.running_prompt.clone() {
                        self.join(p)?;
                    }
                }
            }
            "session/update" => {
                let p = params["_meta"]["promptId"].as_str();
                if let Kind::Steer { joined: None, .. } = self.kind
                    && let Some(p) = p.filter(|p| p.starts_with("interject-fallback-"))
                {
                    self.join(p.to_string())?;
                }
                if p.is_none() || p != self.prompt_id() {
                    return Ok(None);
                }
                if let Kind::Prompt { running, .. } = &mut self.kind {
                    *running = true;
                }
                self.accept()?;
                match SessionUpdate::of(params) {
                    SessionUpdate::AgentMessageChunk { content } => {
                        self.text.push_str(content.text.as_deref().unwrap_or(""))
                    }
                    SessionUpdate::ToolCall(call) => {
                        self.text.clear();
                        if let Some(id) = call.tool_call_id {
                            self.tools.insert(id);
                        }
                    }
                    _ => {}
                }
            }
            "_x.ai/session/prompt_complete" => {
                if let Kind::Steer {
                    joined: Some(joined),
                    ..
                } = &self.kind
                    && params["promptId"].as_str() == Some(joined.as_str())
                {
                    return Ok(Some(End {
                        turn_id: joined.clone(),
                        result: Ok(params.clone()),
                    }));
                }
            }
            "_x.ai/session_notification" => {
                // Someone answered (or agent-talk's own answer took effect).
                if let SessionUpdate::InteractionResolved { tool_call_id } =
                    SessionUpdate::of(params)
                {
                    for a in self.approvals.iter_mut().filter(|a| {
                        a.outcome == ApprovalOutcome::Pending
                            && a.item_id.as_ref() == Some(&tool_call_id)
                    }) {
                        a.outcome = ApprovalOutcome::Resolved;
                        record(self.store, a);
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// A permission request: the approval to record and the answer to send before it, if
    /// any; `None` for a request already recorded. Under `deny` the ones for this prompt's
    /// tool calls (matched by `toolCallId`; through a leader, other clients' turns raise
    /// them too) get the `reject_once` option.
    fn on_permission(&self, request_id: Value, params: Value) -> Option<(Approval, Option<Value>)> {
        let req = PermissionRequest::deserialize(&params).unwrap_or_default();
        let call = req.tool_call;
        // A request already recorded is not answered again. Unverified whether Grok
        // ever sends the same request twice to one client.
        if self
            .approvals
            .iter()
            .any(|a| a.request_id == request_id && a.item_id == call.tool_call_id)
        {
            return None;
        }
        let ours = !self.shared
            || call
                .tool_call_id
                .as_ref()
                .is_some_and(|c| self.tools.contains(c));
        let reject = req
            .options
            .iter()
            .find(|o| o.kind == "reject_once")
            .and_then(|o| o.option_id.as_deref());
        let answer = match (self.policy, ours, reject) {
            (ApprovalPolicy::Deny, true, Some(option)) => {
                Some(json!({"outcome": {"outcome": "selected", "optionId": option}}))
            }
            _ => None,
        };
        // As `read` shows the tool call: name and main argument.
        let summary = match call.name() {
            Some(name) => first_line(&format!("{name}{}", call.detail()), 160),
            None => "permission request".into(),
        };
        let a = Approval {
            handle: handle(&self.session_id),
            turn_id: if ours {
                self.prompt_id().map(String::from)
            } else {
                None
            },
            item_id: call.tool_call_id,
            request_id,
            kind: REQUEST_PERMISSION.into(),
            summary,
            outcome: if answer.is_some() {
                ApprovalOutcome::Declined
            } else {
                ApprovalOutcome::Pending
            },
            raw: params,
        };
        Some((a, answer))
    }

    fn add_approval(&mut self, a: Approval) {
        record(self.store, &a);
        self.approvals.push(a);
    }

    /// The turn as `result` reports it; `final_text` is the agent text after the last
    /// tool call, from the live updates. `note` is appended to the basis.
    pub fn turn(
        &self,
        handle: String,
        turn_id: String,
        result: &Value,
        basis: &str,
        note: Option<&str>,
    ) -> model::Turn {
        let (status, error) = updates::status(
            result["stopReason"].as_str().unwrap_or(""),
            &result["_meta"],
        );
        model::Turn {
            handle,
            turn_id,
            status,
            error,
            final_text: self.final_text(),
            duration_ms: None,
            basis: Some(match note {
                Some(n) => format!("{basis}; {n}"),
                None => basis.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Outgoing;
    use crate::model::{Caller, ReceiptState, TurnStatus};
    use crate::store::NewIntent;

    // Frames as captured on grok 1.0.46 (.agents/findings/grok.md §5, §8, §10), with the
    // session id filled in and elided fields dropped.

    const SESSION: &str = "01a107e9-eede-7e82-8808-76482c1baa7e";

    fn store_with_intent(client_msg_id: &str) -> Store {
        let store = Store::memory();
        let message = Outgoing {
            text: "x",
            delivered: "x",
            from: &Caller::UNKNOWN,
            depth: 0,
        };
        store
            .insert_intent(&NewIntent {
                receipt_id: "r",
                handle: &handle(SESSION),
                client_msg_id,
                message: &message,
            })
            .unwrap();
        store
    }

    /// A prompt watch whose `session/prompt` response has not arrived.
    fn prompt<'s>(
        store: &'s Store,
        prompt_id: &str,
        policy: ApprovalPolicy,
        shared: bool,
    ) -> Watch<'s> {
        let kind = Kind::Prompt {
            prompt_id: prompt_id.into(),
            response: None,
            accepted: false,
            running: false,
        };
        Watch::new(store, SESSION, "r", kind, policy, shared, None)
    }

    fn frame(w: &mut Watch, method: &str, params: &str) -> Option<End> {
        let params: Value = serde_json::from_str(&params.replace("$S", SESSION)).unwrap();
        w.on_notification(method, &params).unwrap()
    }

    fn receipt(store: &Store) -> crate::model::Receipt {
        store.receipt("r").unwrap().unwrap()
    }

    /// A queued prompt is accepted when the queue lists it, and runs when the queue names
    /// it running.
    #[test]
    fn queue_changed_accepts_then_runs_the_prompt() {
        let store = store_with_intent("l1-y-queued");
        let mut w = prompt(&store, "l1-y-queued", ApprovalPolicy::Deny, true);
        frame(
            &mut w,
            "_x.ai/queue/changed",
            r#"{"sessionId":"01a10000-0000-7000-8000-000000000000","entries":[{"id":"l1-y-queued","version":0,"kind":"prompt","text":"Remember the codeword BETA. Reply with exactly OK-BETA.","position":0}]}"#,
        );
        assert_eq!(
            receipt(&store).state,
            ReceiptState::Pending,
            "another session"
        );
        frame(
            &mut w,
            "_x.ai/queue/changed",
            r#"{"sessionId":"$S","entries":[{"id":"l1-y-queued","version":0,"kind":"prompt","text":"Remember the codeword BETA. Reply with exactly OK-BETA.","position":0}],"runningPromptId":"l1-long-x","runningText":"Run exactly this shell command …","runningKind":"prompt"}"#,
        );
        let r = receipt(&store);
        assert_eq!(r.state, ReceiptState::Accepted);
        assert_eq!(r.turn_id.as_deref(), Some("l1-y-queued"));
        assert_eq!(w.progress(), Progress::Pending);
        frame(
            &mut w,
            "_x.ai/queue/changed",
            r#"{"sessionId":"$S","entries":[],"runningPromptId":"l1-y-queued","runningText":"Remember the codeword BETA. Reply with exactly OK-BETA.","runningKind":"prompt"}"#,
        );
        assert_eq!(w.progress(), Progress::Running);
    }

    /// A steer message joins the turn running when the leader reports the interjection,
    /// and its turn ends with that turn's `prompt_complete`.
    #[test]
    fn steer_message_joins_the_running_turn() {
        let store = store_with_intent("c");
        store.accept("r", None, None, None).unwrap();
        let text = "Also remember the codeword GAMMA and mention it in your final reply.";
        let mut w = Watch::steer(
            &store,
            SESSION,
            "r",
            text,
            Some("l1-long-x".into()),
            ApprovalPolicy::Observe,
        );
        frame(
            &mut w,
            "_x.ai/session/interjection",
            r#"{"sessionId":"$S","text":"Another client's message."}"#,
        );
        assert_eq!(receipt(&store).turn_id, None);
        frame(
            &mut w,
            "_x.ai/session/interjection",
            r#"{"sessionId":"$S","text":"Also remember the codeword GAMMA and mention it in your final reply."}"#,
        );
        assert_eq!(receipt(&store).turn_id.as_deref(), Some("l1-long-x"));
        frame(
            &mut w,
            "session/update",
            r#"{"sessionId":"$S","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"DONE. Codeword GAMMA."}},"_meta":{"promptId":"l1-long-x"}}"#,
        );
        let end = frame(
            &mut w,
            "_x.ai/session/prompt_complete",
            r#"{"sessionId":"$S","promptId":"l1-long-x","stopReason":"end_turn","agentResult":null}"#,
        )
        .expect("the joined turn ended");
        let t = w.turn(
            "grok:s".into(),
            end.turn_id,
            &end.result.unwrap(),
            "b",
            None,
        );
        assert_eq!(
            (t.turn_id.as_str(), t.status, t.final_text.as_deref()),
            (
                "l1-long-x",
                TurnStatus::Completed,
                Some("DONE. Codeword GAMMA.")
            )
        );
    }

    const PERMISSION: &str = r#"{"sessionId":"$S","toolCall":{"toolCallId":"call-f3cb…-0","kind":"execute","title":"Execute `env | grep -iE 'grok' | sort`","rawInput":{"variant":"Bash","command":"env | grep -iE 'grok' | sort"},"_meta":{"x.ai/tool":{"name":"run_terminal_command","read_only":false}}},"options":[{"optionId":"always-allow","kind":"allow_always","name":"Yes, and don't ask again for bash commands"},{"optionId":"allow-once","kind":"allow_once","name":"Yes, proceed"},{"optionId":"reject-once","kind":"reject_once","name":"No, and tell Grok what to do differently"},{"optionId":"reject-always","kind":"reject_always","name":"No, and don't ask again for this command"}]}"#;

    fn permission(w: &mut Watch, id: u64, params: &str) -> Option<Value> {
        let params = serde_json::from_str(&params.replace("$S", SESSION)).unwrap();
        let (a, answer) = w.on_permission(json!(id), params)?;
        w.add_approval(a);
        answer
    }

    /// In direct mode every request is this prompt's: `deny` answers `reject_once`, once;
    /// `observe` leaves it pending.
    #[test]
    fn deny_answers_reject_once() {
        let store = store_with_intent("p");
        let mut w = prompt(&store, "p", ApprovalPolicy::Deny, false);
        assert_eq!(
            permission(&mut w, 0, PERMISSION),
            Some(json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}}))
        );
        assert_eq!(permission(&mut w, 0, PERMISSION), None, "already answered");
        let a = &w.approvals[..];
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].outcome, ApprovalOutcome::Declined);
        assert_eq!(
            (a[0].turn_id.as_deref(), a[0].item_id.as_deref()),
            (Some("p"), Some("call-f3cb…-0"))
        );
        assert_eq!(
            a[0].summary,
            "run_terminal_command env | grep -iE 'grok' | sort"
        );

        let mut w = prompt(&store, "p", ApprovalPolicy::Observe, false);
        assert_eq!(permission(&mut w, 0, PERMISSION), None);
        assert_eq!(w.approvals[0].outcome, ApprovalOutcome::Pending);
    }

    /// Through a leader only requests for this prompt's tool calls are answered; the
    /// others stay pending until `interaction_resolved` reports someone answered them.
    #[test]
    fn leader_requests_match_our_tool_calls() {
        let store = store_with_intent("p");
        let mut w = prompt(&store, "p", ApprovalPolicy::Deny, true);
        assert_eq!(
            permission(&mut w, 0, PERMISSION),
            None,
            "another client's turn"
        );
        assert_eq!(w.approvals[0].turn_id, None);
        frame(
            &mut w,
            "session/update",
            r#"{"sessionId":"$S","update":{"sessionUpdate":"tool_call","toolCallId":"call-3b49b159-0","title":"run_terminal_command","rawInput":{"command":"env | grep GROK_SESSION_ID"},"_meta":{"x.ai/tool":{"version":1,"name":"run_terminal_command","kind":"execute","namespace":"grok_build","label":"Run Command","read_only":false}}},"_meta":{"promptId":"p"}}"#,
        );
        let ours = PERMISSION.replace("call-f3cb…-0", "call-3b49b159-0");
        assert!(permission(&mut w, 1, &ours).is_some());
        assert_eq!(w.approvals[1].outcome, ApprovalOutcome::Declined);

        frame(
            &mut w,
            "_x.ai/session_notification",
            r#"{"sessionId":"$S","update":{"sessionUpdate":"interaction_resolved","tool_call_id":"call-f3cb…-0"}}"#,
        );
        let outcomes: Vec<_> = w.approvals.iter().map(|a| a.outcome).collect();
        assert_eq!(
            outcomes,
            [ApprovalOutcome::Resolved, ApprovalOutcome::Declined]
        );
    }

    /// Fields of a permission request that do not fit leave the rest usable: `deny` still
    /// finds `reject_once` and the tool call.
    #[test]
    fn odd_permission_fields_still_decline() {
        let store = store_with_intent("p");
        let mut w = prompt(&store, "p", ApprovalPolicy::Deny, false);
        let odd = PERMISSION
            .replace(r#""kind":"allow_always""#, r#""kind":null"#)
            .replace(r#""title":"Execute"#, r#""title":7,"x":"Execute"#);
        assert_eq!(
            permission(&mut w, 0, &odd),
            Some(json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}}))
        );
        assert_eq!(w.approvals[0].item_id.as_deref(), Some("call-f3cb…-0"));
    }
}
