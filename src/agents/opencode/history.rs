//! OpenCode history rows and the rules read from them (DESIGN.md §6.3): where a turn ends
//! and what it answered, the turn of a page that starts mid-turn, and the messages `read`
//! shows.

use super::handle;
use super::transport::{Service, text};
use crate::agents::history::RemoteHistory;
use crate::model::{self, Message, Phase, Result, Role, TurnStatus};
use serde::Deserialize;
use serde_json::Value;

/// How many history pages (newest first) `find_span` scans for a user message. An older
/// message counts as not in history, so `wait` on it ends with `E_PRECONDITION`.
const SPAN_PAGES: usize = 40;

/// A history row (`Session.Message.*`), as far as agent-talk reads it; callers keep the
/// raw row for `--raw`. A row that does not decode reads as one of an unknown type.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Row {
    id: String,
    #[serde(rename = "type")]
    kind: Kind,
    time: Time,
    /// User and synthetic rows.
    text: Option<String>,
    /// Assistant rows.
    content: Vec<Part>,
    finish: Option<Finish>,
    error: Option<Value>,
    /// Idle rows.
    outcome: Option<IdleOutcome>,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    User,
    Synthetic,
    Assistant,
    /// Written at an execution's end.
    Idle,
    /// `system`, `compaction`, `skill`, `shell`, `{agent,model,location}-switched`.
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct Time {
    created: Option<i64>,
    completed: Option<i64>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Part {
    Text {
        text: Option<String>,
    },
    Tool {
        #[serde(default, deserialize_with = "text")]
        name: Option<String>,
        #[serde(default)]
        state: ToolState,
    },
    /// `reasoning`, and anything newer.
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct ToolState {
    #[serde(default, deserialize_with = "text")]
    status: Option<String>,
    #[serde(default)]
    input: Value,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Finish {
    Stop,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum IdleOutcome {
    Succeeded,
    Failed,
    Interrupted,
    #[serde(other)]
    Other,
}

impl Row {
    pub fn decode(raw: &Value) -> Row {
        Row::deserialize(raw).unwrap_or_default()
    }

    /// An assistant step that an interrupt cut off.
    fn aborted(&self) -> bool {
        self.kind == Kind::Assistant && self.error.as_ref().is_some_and(|e| e["type"] == "aborted")
    }

    /// The texts of the row's text parts.
    fn texts(&self) -> impl Iterator<Item = &str> {
        self.content.iter().filter_map(|p| match p {
            Part::Text { text } => text.as_deref(),
            _ => None,
        })
    }

    /// The index of the part `read` shows as the final reply: the last text part of a step
    /// that finished with `stop`.
    fn reply_part(&self) -> Option<usize> {
        if self.finish != Some(Finish::Stop) {
            return None;
        }
        self.content
            .iter()
            .rposition(|p| matches!(p, Part::Text { .. }))
    }

    /// The text of `reply_part`.
    fn reply(&self) -> Option<&str> {
        match &self.content[self.reply_part()?] {
            Part::Text { text } => Some(text.as_deref().unwrap_or_default()),
            _ => None,
        }
    }
}

/// Everything in history from a user message to the end of its turn.
pub struct Span {
    user: Row,
    /// Rows after the user message, oldest first: up to the `idle` (excluded), or up to the
    /// interrupted step (included).
    after: Vec<Row>,
    end: Option<End>,
}

/// How the turn ended in history (DESIGN.md §6.3).
enum End {
    /// The `idle` row an execution end writes.
    Idle(Row),
    /// An interrupt with reason `shutdown` (message-less permission reject, service going
    /// down) writes no `idle`; the aborted step is the turn's last row, followed by
    /// another execution's input or by nothing while the session is inactive.
    Aborted,
}

impl Span {
    /// `after`: what follows the user message in history, oldest first.
    fn new(user: Row, mut after: Vec<Row>) -> Span {
        let end_at = after.iter().enumerate().find_map(|(i, r)| {
            if r.kind == Kind::Idle {
                return Some(i);
            }
            if !r.aborted() {
                return None;
            }
            // A user message right after the aborted step is another execution's input
            // (DESIGN.md §6.3); anything else continues this one: its own
            // `idle` (user interrupt), a restart notice, or further steps after the
            // location closed during a permission prompt (OpenCode source `step.ts`,
            // `needsContinuation`). Nothing after it yet: undecided.
            (after.get(i + 1)?.kind == Kind::User).then_some(i)
        });
        let end = match end_at {
            Some(i) if after[i].kind == Kind::Idle => {
                let idle = after.remove(i);
                after.truncate(i);
                Some(End::Idle(idle))
            }
            Some(i) => {
                after.truncate(i + 1);
                Some(End::Aborted)
            }
            None => None,
        };
        Span { user, after, end }
    }

    /// The last row is an aborted step and nothing tells yet whether the execution went on.
    pub fn undecided_abort(&self) -> bool {
        self.end.is_none() && self.after.last().is_some_and(Row::aborted)
    }

    /// End the turn at its trailing aborted step: nothing runs in the session any more.
    pub fn end_at_abort(&mut self) {
        if self.undecided_abort() {
            self.end = Some(End::Aborted);
        }
    }

    /// The turn, once it ended.
    pub fn turn(&self, session_id: &str) -> Option<model::Turn> {
        let (status, ended_at, basis) = match self.end.as_ref()? {
            End::Idle(idle) => (
                match idle.outcome {
                    Some(IdleOutcome::Succeeded) => TurnStatus::Completed,
                    Some(IdleOutcome::Failed) => TurnStatus::Failed,
                    Some(IdleOutcome::Interrupted) => TurnStatus::Interrupted,
                    Some(IdleOutcome::Other) | None => TurnStatus::Unknown,
                },
                idle.time.created,
                "history: the first idle row after the message; one execution may answer several messages",
            ),
            End::Aborted => (
                TurnStatus::Interrupted,
                self.after.last().and_then(|r| r.time.completed),
                "history: an interrupted step with no idle row after it",
            ),
        };
        let assistants: Vec<&Row> = self
            .after
            .iter()
            .filter(|r| r.kind == Kind::Assistant)
            .collect();
        // The message `read` marks final; a turn without one (interrupted, failed) answers
        // with its last text.
        let final_text = assistants
            .iter()
            .rev()
            .find_map(|r| r.reply())
            .or_else(|| assistants.iter().rev().find_map(|r| r.texts().last()))
            .map(String::from);
        let error = assistants.iter().rev().find_map(|r| r.error.clone());
        Some(model::Turn {
            handle: handle(session_id),
            turn_id: self.user.id.clone(),
            status,
            error,
            final_text,
            duration_ms: match (self.user.time.created, ended_at) {
                (Some(s), Some(e)) => Some(e - s),
                _ => None,
            },
            basis: Some(basis.into()),
        })
    }
}

/// Find the user message `msg_id` in history (newest pages first) and what followed it.
pub async fn find_span(svc: &Service, session_id: &str, msg_id: &str) -> Result<Option<Span>> {
    let mut cursor: Option<String> = None;
    let mut newer: Vec<Row> = Vec::new(); // newest first
    for _ in 0..SPAN_PAGES {
        let (rows, next) = svc.history(session_id, cursor.as_deref()).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        for row in rows.iter().map(Row::decode) {
            if row.id == msg_id {
                return Ok(Some(Span::new(row, newer.into_iter().rev().collect())));
            }
            newer.push(row);
        }
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// The user message in force before the fetched rows: none when they start with a user
/// message or are all of history, otherwise the newest user row behind them.
pub async fn turn_before(
    svc: &Service,
    session_id: &str,
    history: &RemoteHistory,
) -> Result<Option<String>> {
    match (history.records().first(), history.older()) {
        (Some(oldest), Some(older)) if Row::decode(oldest).kind != Kind::User => {
            svc.user_before(session_id, older).await
        }
        _ => Ok(None),
    }
}

fn timestamp(ms: Option<i64>) -> Option<String> {
    ms.and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map(|t| t.to_string())
}

/// The messages of history rows, oldest first, each with the index of its row; `turn` is
/// the user message in force before the first row. `from` is left to the caller.
pub fn messages(rows: &[Value], mut turn: Option<String>) -> Vec<(usize, Message)> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().map(Row::decode).enumerate() {
        if row.kind == Kind::User {
            turn = Some(row.id.clone());
        }
        let turn_id = match row.kind {
            // A synthetic row before any user message starts a turn of its own.
            Kind::Synthetic => turn.clone().unwrap_or_else(|| row.id.clone()),
            _ => turn.clone().unwrap_or_default(),
        };
        let mut push = |item_id: String, role, phase, text: String, timestamp| {
            let turn_id = turn_id.clone();
            out.push((
                i,
                Message {
                    turn_id,
                    item_id,
                    role,
                    phase,
                    text,
                    from: None,
                    timestamp,
                },
            ));
        };
        let created = timestamp(row.time.created);
        match row.kind {
            Kind::User => push(
                row.id.clone(),
                Role::User,
                Phase::Prompt,
                row.text.unwrap_or_default(),
                created,
            ),
            // Written by the service itself under its own id: no sender.
            Kind::Synthetic => push(
                row.id.clone(),
                Role::User,
                Phase::Other,
                format!("[synthetic] {}", row.text.unwrap_or_default()),
                created,
            ),
            Kind::Assistant => {
                let at = timestamp(row.time.completed).or(created);
                let reply = row.reply_part();
                for (p, part) in row.content.iter().enumerate() {
                    let (phase, text) = match part {
                        Part::Text { text } if Some(p) == reply => {
                            (Phase::Final, text.clone().unwrap_or_default())
                        }
                        Part::Text { text } => {
                            (Phase::Commentary, text.clone().unwrap_or_default())
                        }
                        Part::Tool { name, state } => (
                            Phase::Other,
                            format!(
                                "[tool {}] {} {}",
                                name.as_deref().unwrap_or("?"),
                                state.status.as_deref().unwrap_or("?"),
                                state.input
                            ),
                        ),
                        Part::Other => continue,
                    };
                    push(
                        format!("{}#{p}", row.id),
                        Role::Assistant,
                        phase,
                        text,
                        at.clone(),
                    );
                }
                if let Some(e) = &row.error {
                    push(
                        format!("{}#error", row.id),
                        Role::Assistant,
                        Phase::Other,
                        format!("[error] {e}"),
                        at,
                    );
                }
            }
            Kind::Idle | Kind::Other => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(values: &[Value]) -> Vec<Row> {
        values.iter().map(Row::decode).collect()
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
        let span = Span::new(
            Row::decode(&user),
            rows(&[assistant.clone(), idle, later.clone()]),
        );
        let t = span.turn("ses_x").unwrap();
        assert_eq!(t.status, TurnStatus::Completed);
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
        let span = Span::new(
            Row::decode(&user),
            rows(&[interrupted.clone(), idle_interrupted]),
        );
        let t = span.turn("ses_x").unwrap();
        assert_eq!(t.status, TurnStatus::Interrupted);
        assert_eq!(t.final_text, None);
        assert_eq!(t.error.unwrap()["type"], "aborted");
        assert_eq!(t.duration_ms, Some(2000));

        // A shutdown-reason interrupt (DESIGN.md §6.3): no idle; the next
        // execution's rows must not become this turn's reply.
        let idle_later =
            json!({"id":"msg_k","time":{"created":6000},"type":"idle","outcome":"succeeded"});
        let span = Span::new(
            Row::decode(&user),
            rows(&[interrupted.clone(), later, assistant, idle_later]),
        );
        let t = span.turn("ses_x").unwrap();
        assert_eq!(t.status, TurnStatus::Interrupted);
        assert_eq!(t.final_text, None);
        assert_eq!(t.duration_ms, Some(900));
        assert_eq!(span.after.len(), 1);

        // Resumed after a service restart: the turn goes on.
        let restart = json!({"id":"msg_s","type":"synthetic","text":"The server restarted…","metadata":{"notice":"restart"}});
        let span = Span::new(Row::decode(&user), rows(&[interrupted.clone(), restart]));
        assert!(!span.undecided_abort() && span.turn("ses_x").is_none());

        // Nothing after the aborted step yet: undecided until nothing runs any more.
        let mut span = Span::new(Row::decode(&user), rows(&[interrupted]));
        assert!(span.undecided_abort());
        span.end_at_abort();
        assert_eq!(span.turn("ses_x").unwrap().status, TurnStatus::Interrupted);
    }

    /// `wait` reports as the final reply what `read` marks `final`: the last text part of
    /// the last step that stopped, not every text of that step.
    #[test]
    fn wait_and_read_agree_on_the_reply() {
        let page = [
            json!({"id":"msg_u","time":{"created":1},"text":"two","type":"user"}),
            json!({"id":"msg_a1","time":{"created":2,"completed":3},"type":"assistant","finish":"stop",
                "content":[{"type":"text","text":"working"},{"type":"tool","name":"shell","state":{"status":"completed"}},{"type":"text","text":"two"}]}),
            json!({"id":"msg_a2","time":{"created":4,"completed":5},"type":"assistant","finish":"stop",
                "content":[{"type":"reasoning","text":"done"}]}),
            json!({"id":"msg_i","time":{"created":6},"type":"idle","outcome":"succeeded"}),
        ];
        let finals: Vec<String> = messages(&page, None)
            .into_iter()
            .filter(|(_, m)| m.phase == Phase::Final)
            .map(|(_, m)| m.text)
            .collect();
        let span = Span::new(Row::decode(&page[0]), rows(&page[1..]));
        let t = span.turn("ses_x").unwrap();
        assert_eq!(finals, ["two"]);
        assert_eq!(t.final_text.as_deref(), Some("two"));

        // Interrupted before any step stopped: no final message, the last text instead.
        let cut = json!({"id":"msg_b","time":{"created":2,"completed":3},"type":"assistant","finish":"error",
            "error":{"type":"aborted"},"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]});
        let idle = json!({"id":"msg_j","time":{"created":4},"type":"idle","outcome":"interrupted"});
        let span = Span::new(Row::decode(&page[0]), rows(&[cut, idle]));
        assert_eq!(
            span.turn("ses_x").unwrap().final_text.as_deref(),
            Some("second")
        );
    }

    #[test]
    fn messages_group_by_user_message_and_phase() {
        let page = [
            json!({"id":"msg_u1","time":{"created":1},"text":"slow","type":"user"}),
            json!({"id":"msg_a1","time":{"created":2,"completed":3},"type":"assistant","agent":"build","model":{"id":"m","providerID":"p"},
                "finish":"tool-calls","content":[{"type":"reasoning","text":"hmm"},{"type":"tool","name":"shell","state":{"status":"completed","input":{"command":"sleep 1"}}}]}),
            json!({"id":"msg_u2","time":{"created":4},"text":"two","type":"user"}),
            json!({"id":"msg_a2","time":{"created":5,"completed":6},"type":"assistant","agent":"build","model":{"id":"m","providerID":"p"},
                "finish":"stop","content":[{"type":"text","text":"working"},{"type":"text","text":"two"}]}),
            json!({"id":"msg_i","time":{"created":7},"type":"idle","outcome":"succeeded"}),
        ];
        let m = messages(&page, None);
        let brief: Vec<(&str, &str, &str, &str)> = m
            .iter()
            .map(|(_, m)| {
                (
                    m.turn_id.as_str(),
                    m.role.as_str(),
                    m.phase.as_str(),
                    m.text.as_str(),
                )
            })
            .collect();
        assert_eq!(
            brief,
            vec![
                ("msg_u1", "user", "prompt", "slow"),
                (
                    "msg_u1",
                    "assistant",
                    "other",
                    "[tool shell] completed {\"command\":\"sleep 1\"}"
                ),
                ("msg_u2", "user", "prompt", "two"),
                ("msg_u2", "assistant", "commentary", "working"),
                ("msg_u2", "assistant", "final", "two"),
            ]
        );
        assert_eq!(m[1].1.item_id, "msg_a1#1");
        // A page starting mid-turn takes the turn id handed in.
        let m = messages(&page[1..2], Some("msg_u0".into()));
        assert_eq!(m[0].1.turn_id, "msg_u0");
        // An empty tool name or status is unknown.
        let tool = json!({"id":"msg_a","type":"assistant","content":[{"type":"tool","name":"","state":{"status":""}}]});
        assert_eq!(messages(&[tool], None)[0].1.text, "[tool ?] ? null");
    }
}
