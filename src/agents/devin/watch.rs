//! What one command observes of its prompt on the `devin acp` connection (DESIGN.md §6.7):
//! its acceptance (the `user_message_chunk` echoing agent-talk's client message id), the
//! agent text, the tool calls, the permission requests, and the end of the turn. The
//! authority for the end is the `session/prompt` response (DESIGN.md §7). Devin's session
//! lock keeps every other process out while the child runs, so every event on the
//! connection belongs to this prompt.

use super::handle;
use crate::agents::acp::{Conn, Event, Response};
use crate::agents::history::first_line;
use crate::agents::lifecycle::{ApprovalPolicy, record};
use crate::model::{
    self, Approval, ApprovalOutcome, Error, ErrorCode, Progress, Result, TurnStatus,
};
use crate::store::Store;
use serde_json::{Value, json};

const REQUEST_PERMISSION: &str = "session/request_permission";
/// Where Devin takes and echoes a client's message id (3000.11.92).
const CLIENT_MESSAGE_ID: &str = "cognition.ai/clientMessageId";

pub struct Watch<'s> {
    store: &'s Store,
    session_id: String,
    receipt_id: String,
    /// agent-talk's client message id, which Devin stores with the user node: the turn id.
    turn_id: String,
    /// The `session/prompt` response until it arrives.
    response: Option<Response>,
    policy: ApprovalPolicy,
    accepted: bool,
    /// Agent text since the last tool call.
    text: String,
    pub approvals: Vec<Approval>,
}

impl<'s> Watch<'s> {
    pub fn new(
        store: &'s Store,
        session_id: &str,
        receipt_id: &str,
        turn_id: &str,
        response: Response,
        policy: ApprovalPolicy,
    ) -> Self {
        Watch {
            store,
            session_id: session_id.to_string(),
            receipt_id: receipt_id.to_string(),
            turn_id: turn_id.to_string(),
            response: Some(response),
            policy,
            accepted: false,
            text: String::new(),
            approvals: Vec::new(),
        }
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn accepted(&self) -> bool {
        self.accepted
    }

    /// The response arrived: the turn is over.
    pub fn ended(&self) -> bool {
        self.response.is_none()
    }

    /// How far the prompt got, for an observation that ended without a result. Nothing
    /// queues in front of it: the lock admits one process, which runs one prompt.
    pub fn progress(&self) -> Progress {
        if self.accepted {
            Progress::Running
        } else {
            Progress::Unknown
        }
    }

    pub fn final_text(&self) -> Option<String> {
        (!self.text.is_empty()).then(|| self.text.clone())
    }

    fn accept(&mut self) -> Result<()> {
        if !self.accepted {
            let t = Some(self.turn_id.as_str());
            self.store.accept(&self.receipt_id, None, t, None)?;
            self.accepted = true;
        }
        Ok(())
    }

    /// Events until the turn ends: the `session/prompt` result or error.
    pub async fn finish(&mut self, conn: &mut Conn) -> Result<Result<Value>> {
        loop {
            if let Some(end) = self.step(conn).await? {
                return Ok(end);
            }
        }
    }

    /// Events until the prompt is accepted; the end of the turn when it came first.
    pub async fn until_accepted(&mut self, conn: &mut Conn) -> Result<Option<Result<Value>>> {
        while !self.accepted {
            if let Some(end) = self.step(conn).await? {
                return Ok(Some(end));
            }
        }
        Ok(None)
    }

    /// One event, or the prompt's response; `Some` once the turn ended.
    async fn step(&mut self, conn: &mut Conn) -> Result<Option<Result<Value>>> {
        let Some(response) = &mut self.response else {
            return Err(Error::new(
                ErrorCode::Transport,
                "the turn already ended on this connection",
            ));
        };
        tokio::select! {
            biased;
            result = response => {
                self.response = None;
                if result.is_ok() {
                    self.accept()?;
                }
                // The reader pushed the turn's last notifications before it resolved the
                // response, which the biased select takes first. The turn is over: an answer
                // that fails to reach the child now does not change its outcome.
                for ev in conn.drain() {
                    if let Err(e) = self.on_event(conn, ev).await {
                        tracing::warn!("after the turn ended: {e}");
                    }
                }
                Ok(Some(result))
            }
            ev = conn.next_event() => match ev {
                Some(ev) => self.on_event(conn, ev).await.map(|()| None),
                // Not reached while the response is pending: the reader drops the waiters
                // before the event channels close, so the response (or its loss) comes first.
                None => Err(Error::new(
                    ErrorCode::Transport,
                    "devin acp exited while agent-talk was observing; outcome unknown",
                )),
            },
        }
    }

    async fn on_event(&mut self, conn: &Conn, ev: Event) -> Result<()> {
        match ev {
            Event::Request(r) if r.method == REQUEST_PERMISSION => {
                let id = r.id.clone();
                if let Some((a, answer)) = self.on_permission(r.id, r.params) {
                    // Recorded as declined only once the answer is written.
                    if let Some(answer) = answer {
                        conn.respond(&id, answer).await?;
                    }
                    record(self.store, &a);
                    self.approvals.push(a);
                }
            }
            // agent-talk offers no client capabilities.
            Event::Request(r) => {
                conn.respond_error(&r.id, -32601, "not supported by agent-talk")
                    .await?;
            }
            Event::Notification { method, params } => self.on_notification(&method, &params)?,
        }
        Ok(())
    }

    fn on_notification(&mut self, method: &str, params: &Value) -> Result<()> {
        if method != "session/update" || params["sessionId"] != self.session_id.as_str() {
            return Ok(());
        }
        let u = &params["update"];
        match u["sessionUpdate"].as_str().unwrap_or_default() {
            "user_message_chunk" => {
                let echoed = [&u["_meta"], &params["_meta"]]
                    .iter()
                    .any(|m| m[CLIENT_MESSAGE_ID] == self.turn_id.as_str());
                if echoed {
                    self.accept()?;
                }
            }
            "agent_message_chunk" => self
                .text
                .push_str(u["content"]["text"].as_str().unwrap_or_default()),
            "tool_call" => self.text.clear(),
            _ => {}
        }
        Ok(())
    }

    /// A permission request: the approval to record and the answer to send before it, if
    /// any; `None` for a request already recorded. Under `deny` the answer is the
    /// `reject_once` option (Devin offers no reject-always); the tool fails and the turn
    /// goes on.
    fn on_permission(&self, request_id: Value, params: Value) -> Option<(Approval, Option<Value>)> {
        if self.approvals.iter().any(|a| a.request_id == request_id) {
            return None;
        }
        let call = &params["toolCall"];
        let reject = params["options"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| o["kind"] == "reject_once" || o["optionId"] == "reject_once")
            .and_then(|o| o["optionId"].as_str());
        let answer = match (self.policy, reject) {
            (ApprovalPolicy::Deny, Some(option)) => {
                Some(json!({"outcome": {"outcome": "selected", "optionId": option}}))
            }
            _ => None,
        };
        let summary = call["title"]
            .as_str()
            .or(call["rawInput"]["command"].as_str())
            .map_or_else(|| "permission request".into(), |t| first_line(t, 160));
        let a = Approval {
            handle: handle(&self.session_id),
            turn_id: Some(self.turn_id.clone()),
            item_id: call["toolCallId"].as_str().map(String::from),
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

    /// The turn as the `session/prompt` result reports it; `final_text` is the agent text
    /// after the last tool call. `note` is appended to the basis.
    pub fn turn(&self, handle: String, result: &Value, note: Option<&str>) -> model::Turn {
        let (status, error) = match result["stopReason"].as_str().unwrap_or_default() {
            "end_turn" => (TurnStatus::Completed, None),
            "cancelled" => (TurnStatus::Interrupted, Some(result.clone())),
            "" => (TurnStatus::Unknown, Some(result.clone())),
            _ => (TurnStatus::Failed, Some(result.clone())),
        };
        let basis = "session/prompt response";
        model::Turn {
            handle,
            turn_id: self.turn_id.clone(),
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
    use crate::model::{Caller, ReceiptState};
    use crate::store::NewIntent;

    // Frames in the shape Devin 3000.11.92 sends them (.agents/findings/devin.md §5, §8).

    const SESSION: &str = "swanky-towel";
    const CLIENT: &str = "7f0c3c9e-2f4b-4a43-9a43-2b1f1c0d9a11";

    fn store_with_intent() -> Store {
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
                client_msg_id: CLIENT,
                message: &message,
            })
            .unwrap();
        store
    }

    /// A fake `devin acp`: answers `initialize`, then writes `frames` once the prompt is
    /// sent, the prompt's response last, in one go; `answers` lines are read after it.
    async fn agent(frames: &[Value]) -> Conn {
        let mut lines: Vec<String> = frames.iter().map(|f| format!("echo '{f}'")).collect();
        lines.push("true".into());
        let script = format!(
            "read l; echo '{}'; read l; {}; while read l; do echo \"$l\" >&2; done",
            json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
            lines.join("; "),
        );
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &script]);
        crate::agents::acp::spawn(cmd, "test agent").await.unwrap()
    }

    fn update(update: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SESSION, "update": update}})
    }

    async fn watch<'s>(store: &'s Store, conn: &Conn, policy: ApprovalPolicy) -> Watch<'s> {
        let response = conn.submit("session/prompt", json!({})).await.unwrap();
        // Every frame is read before the watcher first polls.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Watch::new(store, SESSION, "r", CLIENT, response, policy)
    }

    /// The echo of the client message id accepts the prompt; the reply is the text after the
    /// last tool call, including the chunk buffered with the response.
    #[tokio::test]
    async fn echo_accepts_and_the_last_text_is_the_reply() {
        let _children = crate::agents::acp::SPAWN_LOCK.lock().await;
        let store = store_with_intent();
        let mut conn = agent(&[
            update(
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "x"}, "_meta": {CLIENT_MESSAGE_ID: CLIENT}}),
            ),
            update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Checking."}}),
            ),
            update(
                json!({"sessionUpdate": "tool_call", "toolCallId": "exec:0#ab", "title": "ls"}),
            ),
            update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "po"}}),
            ),
            update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "ng"}}),
            ),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"stopReason": "end_turn", "_meta": {"cognition.ai/userMessageId": CLIENT}}}),
        ])
        .await;
        let mut w = watch(&store, &conn, ApprovalPolicy::Deny).await;
        let end = w.finish(&mut conn).await.unwrap().unwrap();
        let r = store.receipt("r").unwrap().unwrap();
        assert_eq!(
            (r.state, r.turn_id.as_deref()),
            (ReceiptState::Accepted, Some(CLIENT))
        );
        let t = w.turn("devin:s".into(), &end, None);
        assert_eq!(
            (t.status, t.final_text.as_deref()),
            (TurnStatus::Completed, Some("pong"))
        );
        conn.close().await;
    }

    const PERMISSION: &str = r#"{"jsonrpc":"2.0","id":"5b1f0c52-0d55-4d8e-a0f3-4b1d9e7f5a10","method":"session/request_permission","params":{"sessionId":"swanky-towel","toolCall":{"toolCallId":"exec:1#9f","title":"touch probe.txt","rawInput":{"command":"touch probe.txt"}},"options":[{"optionId":"allow_once","kind":"allow_once","name":"Allow once"},{"optionId":"allow_session","kind":"allow_always","name":"Allow for this session"},{"optionId":"switch_bypass","kind":"allow_always","name":"Switch to bypass"},{"optionId":"reject_once","kind":"reject_once","name":"Reject"}]}}"#;

    /// `deny` answers `reject_once` and the turn goes on to its end; `observe` leaves the
    /// request pending.
    #[test]
    fn permission_requests_get_reject_once_under_deny() {
        let store = store_with_intent();
        let request: Value = serde_json::from_str(PERMISSION).unwrap();
        let mut w = Watch {
            store: &store,
            session_id: SESSION.into(),
            receipt_id: "r".into(),
            turn_id: CLIENT.into(),
            response: None,
            policy: ApprovalPolicy::Deny,
            accepted: false,
            text: String::new(),
            approvals: Vec::new(),
        };
        let (a, answer) = w
            .on_permission(request["id"].clone(), request["params"].clone())
            .unwrap();
        assert_eq!(
            answer,
            Some(json!({"outcome": {"outcome": "selected", "optionId": "reject_once"}}))
        );
        assert_eq!(
            (a.outcome, a.summary.as_str(), a.item_id.as_deref()),
            (
                ApprovalOutcome::Declined,
                "touch probe.txt",
                Some("exec:1#9f")
            )
        );
        w.approvals.push(a);
        assert!(
            w.on_permission(request["id"].clone(), request["params"].clone())
                .is_none(),
            "already answered"
        );
        w.policy = ApprovalPolicy::Observe;
        w.approvals.clear();
        let (a, answer) = w
            .on_permission(request["id"].clone(), request["params"].clone())
            .unwrap();
        assert_eq!((a.outcome, answer), (ApprovalOutcome::Pending, None));
    }

    /// A cancelled turn is interrupted; an error response before the echo leaves the receipt
    /// unaccepted.
    #[tokio::test]
    async fn cancelled_and_refused_prompts() {
        let _children = crate::agents::acp::SPAWN_LOCK.lock().await;
        let store = store_with_intent();
        let mut conn = agent(&[
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32603, "message": "boom"}}),
        ])
        .await;
        let mut w = watch(&store, &conn, ApprovalPolicy::Deny).await;
        assert!(w.finish(&mut conn).await.unwrap().is_err());
        assert!(!w.accepted() && w.ended());
        assert_eq!(
            store.receipt("r").unwrap().unwrap().state,
            ReceiptState::Pending
        );
        let t = w.turn(
            "devin:s".into(),
            &json!({"stopReason": "cancelled"}),
            Some("n"),
        );
        assert_eq!(
            (t.status, t.basis.as_deref()),
            (TurnStatus::Interrupted, Some("session/prompt response; n"))
        );
        conn.close().await;
    }
}
