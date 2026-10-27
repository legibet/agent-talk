//! OpenCode history rows and the rules read from them (DESIGN.md §6.3): where a turn ends
//! and what it answered, the turn of a page that starts mid-turn, and the messages `read`
//! shows.
//!
//! A row (`Session.Message.*`) stays the `Value` the service sent and is read field by
//! field: a field of an unexpected type reads as absent and never hides its row (DESIGN.md
//! §3). `read` shows `user`, `synthetic` and `assistant` rows; an `idle` row, written at
//! an execution's end, ends a turn; `system`, `compaction`, `skill`, `shell` and
//! `{agent,model,location}-switched` rows are skipped.

use super::handle;
use super::transport::Service;
use crate::agents::history::RemoteHistory;
use crate::model::{self, Message, Phase, Result, Role, TurnStatus};
use serde_json::Value;

/// How many history pages (newest first) `find_span` scans for a user message. An older
/// message counts as not in history, so `wait` on it ends with `E_PRECONDITION`.
const SPAN_PAGES: usize = 40;

fn id(row: &Value) -> &str {
    row["id"].as_str().unwrap_or_default()
}

/// The error of an assistant step.
fn error(row: &Value) -> Option<&Value> {
    Some(&row["error"]).filter(|e| !e.is_null())
}

/// An assistant step that an interrupt cut off.
fn aborted(row: &Value) -> bool {
    row["type"] == "assistant" && row["error"]["type"] == "aborted"
}

/// The parts of an assistant step: `text`, `tool`, `reasoning`, and anything newer.
fn parts(row: &Value) -> &[Value] {
    row["content"].as_array().map_or(&[], Vec::as_slice)
}

/// The texts of the row's text parts.
fn texts(row: &Value) -> impl Iterator<Item = &str> {
    parts(row)
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
}

/// The index of the part `read` shows as the final reply: the last text part of a step
/// that finished with `stop`.
fn reply_part(row: &Value) -> Option<usize> {
    if row["finish"] != "stop" {
        return None;
    }
    parts(row).iter().rposition(|p| p["type"] == "text")
}

/// The text of `reply_part`.
fn reply(row: &Value) -> Option<&str> {
    let part = &parts(row)[reply_part(row)?];
    Some(part["text"].as_str().unwrap_or_default())
}

/// A tool part's name or status; an empty string is unknown.
fn or_unknown(v: &Value) -> &str {
    v.as_str().filter(|s| !s.is_empty()).unwrap_or("?")
}

/// Everything in history from a user message to the end of its turn.
pub struct Span {
    user: Value,
    /// Rows after the user message, oldest first: up to the `idle` (excluded), or up to the
    /// interrupted step (included).
    after: Vec<Value>,
    end: Option<End>,
}

/// How the turn ended in history (DESIGN.md §6.3).
enum End {
    /// The `idle` row an execution end writes.
    Idle(Value),
    /// An interrupt with reason `shutdown` (message-less permission reject, service going
    /// down) writes no `idle`; the aborted step is the turn's last row, followed by
    /// another execution's input or by nothing while the session is inactive.
    Aborted,
}

impl Span {
    /// `after`: what follows the user message in history, oldest first.
    fn new(user: Value, mut after: Vec<Value>) -> Span {
        let end_at = after.iter().enumerate().find_map(|(i, r)| {
            if r["type"] == "idle" {
                return Some(i);
            }
            if !aborted(r) {
                return None;
            }
            // A user message right after the aborted step is another execution's input
            // (DESIGN.md §6.3); anything else continues this one: its own
            // `idle` (user interrupt), a restart notice, or further steps after the
            // location closed during a permission prompt (OpenCode source `step.ts`,
            // `needsContinuation`). Nothing after it yet: undecided.
            (after.get(i + 1)?["type"] == "user").then_some(i)
        });
        let end = match end_at {
            Some(i) if after[i]["type"] == "idle" => {
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
        self.end.is_none() && self.after.last().is_some_and(aborted)
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
                match idle["outcome"].as_str() {
                    Some("succeeded") => TurnStatus::Completed,
                    Some("failed") => TurnStatus::Failed,
                    Some("interrupted") => TurnStatus::Interrupted,
                    _ => TurnStatus::Unknown,
                },
                idle["time"]["created"].as_i64(),
                "history: the first idle row after the message; one execution may answer several messages",
            ),
            End::Aborted => (
                TurnStatus::Interrupted,
                self.after
                    .last()
                    .and_then(|r| r["time"]["completed"].as_i64()),
                "history: an interrupted step with no idle row after it",
            ),
        };
        let assistants: Vec<&Value> = self
            .after
            .iter()
            .filter(|r| r["type"] == "assistant")
            .collect();
        // The message `read` marks final; a turn without one (interrupted, failed) answers
        // with its last text.
        let final_text = assistants
            .iter()
            .rev()
            .find_map(|r| reply(r))
            .or_else(|| assistants.iter().rev().find_map(|r| texts(r).last()))
            .map(String::from);
        let error = assistants.iter().rev().find_map(|r| error(r)).cloned();
        Some(model::Turn {
            handle: handle(session_id),
            turn_id: id(&self.user).into(),
            status,
            error,
            final_text,
            duration_ms: match (self.user["time"]["created"].as_i64(), ended_at) {
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
    let mut newer: Vec<Value> = Vec::new(); // newest first
    for _ in 0..SPAN_PAGES {
        let (rows, next) = svc.history(session_id, cursor.as_deref()).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        for row in rows {
            if id(&row) == msg_id {
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
        (Some(oldest), Some(older)) if oldest["type"] != "user" => {
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
    for (i, row) in rows.iter().enumerate() {
        let kind = row["type"].as_str().unwrap_or_default();
        let id = id(row);
        if kind == "user" {
            turn = Some(id.into());
        }
        let turn_id = match kind {
            // A synthetic row before any user message starts a turn of its own.
            "synthetic" => turn.clone().unwrap_or_else(|| id.into()),
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
        let text = row["text"].as_str().unwrap_or_default();
        let created = timestamp(row["time"]["created"].as_i64());
        match kind {
            "user" => push(id.into(), Role::User, Phase::Prompt, text.into(), created),
            // Written by the service itself under its own id: no sender.
            "synthetic" => push(
                id.into(),
                Role::User,
                Phase::Other,
                format!("[synthetic] {text}"),
                created,
            ),
            "assistant" => {
                let at = timestamp(row["time"]["completed"].as_i64()).or(created);
                let reply = reply_part(row);
                for (p, part) in parts(row).iter().enumerate() {
                    let (phase, text) = match part["type"].as_str() {
                        Some("text") => (
                            if Some(p) == reply {
                                Phase::Final
                            } else {
                                Phase::Commentary
                            },
                            part["text"].as_str().unwrap_or_default().into(),
                        ),
                        Some("tool") => (
                            Phase::Other,
                            format!(
                                "[tool {}] {} {}",
                                or_unknown(&part["name"]),
                                or_unknown(&part["state"]["status"]),
                                part["state"]["input"]
                            ),
                        ),
                        _ => continue,
                    };
                    push(
                        format!("{id}#{p}"),
                        Role::Assistant,
                        phase,
                        text,
                        at.clone(),
                    );
                }
                if let Some(e) = error(row) {
                    push(
                        format!("{id}#error"),
                        Role::Assistant,
                        Phase::Other,
                        format!("[error] {e}"),
                        at,
                    );
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let span = Span::new(user.clone(), vec![interrupted.clone(), idle_interrupted]);
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
            user.clone(),
            vec![interrupted.clone(), later, assistant, idle_later],
        );
        let t = span.turn("ses_x").unwrap();
        assert_eq!(t.status, TurnStatus::Interrupted);
        assert_eq!(t.final_text, None);
        assert_eq!(t.duration_ms, Some(900));
        assert_eq!(span.after.len(), 1);

        // Resumed after a service restart: the turn goes on.
        let restart = json!({"id":"msg_s","type":"synthetic","text":"The server restarted…","metadata":{"notice":"restart"}});
        let span = Span::new(user.clone(), vec![interrupted.clone(), restart]);
        assert!(!span.undecided_abort() && span.turn("ses_x").is_none());

        // Nothing after the aborted step yet: undecided until nothing runs any more.
        let mut span = Span::new(user, vec![interrupted]);
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
        let span = Span::new(page[0].clone(), page[1..].to_vec());
        let t = span.turn("ses_x").unwrap();
        assert_eq!(finals, ["two"]);
        assert_eq!(t.final_text.as_deref(), Some("two"));

        // Interrupted before any step stopped: no final message, the last text instead.
        let cut = json!({"id":"msg_b","time":{"created":2,"completed":3},"type":"assistant","finish":"error",
            "error":{"type":"aborted"},"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]});
        let idle = json!({"id":"msg_j","time":{"created":4},"type":"idle","outcome":"interrupted"});
        let span = Span::new(page[0].clone(), vec![cut, idle]);
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

    /// A field of an unexpected type reads as absent; it does not hide its row, so the
    /// user message is still found and its reply still read.
    #[test]
    fn odd_fields_keep_their_rows() {
        let page = [
            json!({"id":"msg_u","time":{"created":1.5},"text":"go","type":"user"}),
            json!({"id":"msg_a1","time":{"created":2},"type":"assistant","finish":5,
                "content":[{"type":"text","text":7},{"type":"tool","name":"shell","state":"odd"}]}),
            json!({"id":"msg_a2","time":{"created":3,"completed":"4"},"type":"assistant","finish":"stop",
                "content":[{"type":"text","text":"done"}]}),
            json!({"id":"msg_i","time":{"created":5},"type":"idle","outcome":"succeeded"}),
        ];
        let brief: Vec<(String, &str, String)> = messages(&page, None)
            .into_iter()
            .map(|(_, m)| (m.item_id, m.phase.as_str(), m.text))
            .collect();
        assert_eq!(
            brief,
            [
                ("msg_u".into(), "prompt", "go".into()),
                ("msg_a1#0".into(), "commentary", String::new()),
                ("msg_a1#1".into(), "other", "[tool shell] ? null".into()),
                ("msg_a2#0".into(), "final", "done".into()),
            ]
        );
        let t = Span::new(page[0].clone(), page[1..].to_vec())
            .turn("ses_x")
            .unwrap();
        assert_eq!(t.turn_id, "msg_u");
        assert_eq!(t.status, TurnStatus::Completed);
        assert_eq!(t.final_text.as_deref(), Some("done"));
        assert_eq!(t.duration_ms, None);
    }
}
