//! `sessions.db`, Devin's session store, read-only (DESIGN.md §6.7): the session rows and
//! the conversation of a session, the `parent_node_id` chain from `sessions.main_chain_id`.
//! agent-talk opens it with `mode=ro` and never writes it.
//!
//! A node's `chat_message` stays the `Value` Devin wrote, read field by field: a field of an
//! unexpected type reads as absent and never drops the node (DESIGN.md §3).
//!
//! Turns: a `user` node with `metadata.is_user_input` opens one; its turn id is the client
//! message id Devin stored for it (`metadata.extensions["chisel/client-message-id"]`, set by
//! agent-talk, `devin -p` and the TUI alike), else its `message_id`. A turn ends at the first
//! later assistant node with `finish_reason: stop` and no tool calls, or at a tool node
//! Devin's cancel marked `Canceled`. A prompt that follows before either leaves the earlier
//! turn `unknown`: the process that took it died, or (a prompt sent while it ran) Devin
//! answered both together, and the chain does not tell which.

use crate::agents::history::first_line;
use crate::model::{self, Message, Phase, Result, Role, TurnStatus};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

/// The store, opened read-only in place. Devin keeps it in WAL mode with its `-wal` and
/// `-shm` files present between runs (3000.11.92), so a read-only connection sees the
/// committed state of every writer.
pub struct Db {
    conn: Connection,
}

/// A `sessions` row.
pub struct Row {
    pub cwd: String,
    /// The permission mode Devin stored for the session (`normal`, `accept-edits`, `bypass`).
    pub agent_mode: String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        // A URI keeps the path literal; `?` and `#` would start its query or fragment.
        let escaped = path
            .to_string_lossy()
            .replace('%', "%25")
            .replace('?', "%3f")
            .replace('#', "%23");
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(format!("file:{escaped}?mode=ro"), flags)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Db { conn })
    }

    pub fn session(&self, id: &str) -> Result<Option<Row>> {
        let row = self
            .conn
            .query_row(
                "SELECT working_directory, agent_mode FROM sessions WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Row {
                        cwd: r.get(0)?,
                        agent_mode: r.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The session's conversation, oldest first: the chain from `main_chain_id` to its root,
    /// read in one statement so that it is one snapshot. A `chat_message` that is not JSON
    /// is kept as a string (a node of no role).
    pub fn chain(&self, id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare_cached(
            "WITH RECURSIVE chain(node_id, parent_node_id, chat_message, depth) AS (
                 SELECT node_id, parent_node_id, chat_message, 0 FROM message_nodes
                  WHERE session_id = ?1
                    AND node_id = (SELECT main_chain_id FROM sessions WHERE id = ?1)
                 UNION ALL
                 SELECT m.node_id, m.parent_node_id, m.chat_message, c.depth + 1
                   FROM message_nodes m JOIN chain c
                     ON m.session_id = ?1 AND m.node_id = c.parent_node_id
             )
             SELECT chat_message FROM chain ORDER BY depth DESC",
        )?;
        let nodes = stmt
            .query_map(params![id], |r| r.get::<_, String>(0))?
            .map(|m| m.map(|m| serde_json::from_str(&m).unwrap_or(Value::String(m))))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(nodes)
    }
}

fn role(n: &Value) -> &str {
    n["role"].as_str().unwrap_or_default()
}

fn content(n: &Value) -> &str {
    n["content"].as_str().unwrap_or_default()
}

fn extension<'a>(n: &'a Value, name: &str) -> &'a Value {
    &n["metadata"]["extensions"][name]
}

fn is_prompt(n: &Value) -> bool {
    role(n) == "user" && n["metadata"]["is_user_input"] == true
}

fn tool_calls(n: &Value) -> &[Value] {
    n["tool_calls"].as_array().map_or(&[], Vec::as_slice)
}

/// An assistant reply that ends a turn: `finish_reason: stop` and no tool calls (`[]` or
/// null on 3000.11.92). A `tool_calls` of another shape is not read as "none": an odd field
/// never ends a turn (DESIGN.md §3).
fn is_final(n: &Value) -> bool {
    let no_calls = match &n["tool_calls"] {
        Value::Null => true,
        Value::Array(calls) => calls.is_empty(),
        _ => false,
    };
    role(n) == "assistant" && n["metadata"]["finish_reason"] == "stop" && no_calls
}

/// A tool node whose call Devin's `session/cancel` ended ("Canceled due to user interrupt").
fn is_canceled(n: &Value) -> bool {
    role(n) == "tool" && extension(n, "chisel/tool_failure")["reason"] == "Canceled"
}

/// The turn id of a prompt node: its client message id, else its `message_id`.
fn turn_id(n: &Value) -> String {
    extension(n, "chisel/client-message-id")
        .as_str()
        .or(n["message_id"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// How a turn ended in the chain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum End {
    /// At this final assistant node.
    Completed(usize),
    /// At this canceled tool node.
    Interrupted(usize),
    /// The next prompt came before either: the process that took this one died, or Devin
    /// answered both prompts together; the chain does not tell which.
    Superseded,
}

pub struct Span {
    /// Index of the prompt node.
    pub start: usize,
    pub id: String,
    pub end: Option<End>,
}

pub struct History {
    pub nodes: Vec<Value>,
    pub spans: Vec<Span>,
}

impl History {
    pub fn new(nodes: Vec<Value>) -> History {
        let starts: Vec<usize> = (0..nodes.len()).filter(|&i| is_prompt(&nodes[i])).collect();
        let spans = starts
            .iter()
            .map(|&start| {
                let end = nodes.iter().enumerate().skip(start + 1).find_map(|(j, n)| {
                    if is_prompt(n) {
                        Some(End::Superseded)
                    } else if is_final(n) {
                        Some(End::Completed(j))
                    } else if is_canceled(n) {
                        Some(End::Interrupted(j))
                    } else {
                        None
                    }
                });
                Span {
                    start,
                    id: turn_id(&nodes[start]),
                    end,
                }
            })
            .collect();
        History { nodes, spans }
    }

    pub fn find(&self, turn_id: &str) -> Option<usize> {
        self.spans.iter().rposition(|s| s.id == turn_id)
    }

    /// The text of the first prompt, for `ls` previews.
    pub fn first_prompt(&self) -> Option<&str> {
        self.spans.first().map(|s| content(&self.nodes[s.start]))
    }

    /// Normalized messages tagged with their node's index, oldest first: prompts, assistant
    /// text (`final` for the node that ends a turn, `commentary` otherwise) and each tool
    /// call as one `other` message whose status comes from its tool node. Item ids are
    /// `<message_id>-<phase>` and the tool call id, both stable when Devin copies the chain
    /// into a new root (DESIGN.md §6.7). The `from` of prompts is left to the caller.
    pub fn messages(&self) -> Vec<(usize, Message)> {
        let results: HashMap<&str, &Value> = self
            .nodes
            .iter()
            .filter(|n| role(n) == "tool")
            .filter_map(|n| Some((n["tool_call_id"].as_str()?, n)))
            .collect();
        let finals: Vec<usize> = self
            .spans
            .iter()
            .filter_map(|s| match s.end {
                Some(End::Completed(j)) => Some(j),
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        let mut turn = String::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if is_prompt(n) {
                turn = turn_id(n);
            }
            let at = n["metadata"]["created_at"].as_str().map(String::from);
            let message = |item_id: String, role, phase, text: String| Message {
                turn_id: turn.clone(),
                item_id,
                role,
                phase,
                text,
                from: None,
                timestamp: at.clone(),
            };
            let id = n["message_id"].as_str().unwrap_or_default();
            match role(n) {
                "user" if is_prompt(n) => {
                    let m = message(
                        format!("{id}-prompt"),
                        Role::User,
                        Phase::Prompt,
                        content(n).into(),
                    );
                    out.push((i, m));
                }
                "assistant" => {
                    if !content(n).is_empty() {
                        let phase = if finals.contains(&i) {
                            Phase::Final
                        } else {
                            Phase::Commentary
                        };
                        let m = message(
                            format!("{id}-{phase}"),
                            Role::Assistant,
                            phase,
                            content(n).into(),
                        );
                        out.push((i, m));
                    }
                    for call in tool_calls(n) {
                        let Some(call_id) = call["id"].as_str() else {
                            continue;
                        };
                        let text = tool_text(call, results.get(call_id).copied());
                        out.push((
                            i,
                            message(call_id.into(), Role::Assistant, Phase::Other, text),
                        ));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The finished turn of a span; `None` while it has no end.
    pub fn turn(&self, handle: String, span: usize) -> Option<model::Turn> {
        let s = &self.spans[span];
        let (status, final_text, error, basis) = match s.end? {
            End::Completed(j) => (
                TurnStatus::Completed,
                Some(content(&self.nodes[j]).to_string()).filter(|t| !t.is_empty()),
                None,
                "sessions.db: the turn's final assistant message",
            ),
            End::Interrupted(j) => (
                TurnStatus::Interrupted,
                None,
                Some(json!({"message": content(&self.nodes[j])})),
                "sessions.db: a tool call canceled by session/cancel",
            ),
            End::Superseded => (
                TurnStatus::Unknown,
                None,
                Some(
                    json!({"message": "the next prompt came before any end of this turn: the process that took it died, or Devin answered both prompts together"}),
                ),
                "sessions.db: the next prompt came before an end",
            ),
        };
        Some(model::Turn {
            handle,
            turn_id: s.id.clone(),
            status,
            error,
            final_text,
            duration_ms: None,
            basis: Some(basis.into()),
        })
    }
}

/// `[name] status input` for a tool call, the status from its tool node (`pending` without
/// one).
fn tool_text(call: &Value, result: Option<&Value>) -> String {
    let name = call["name"].as_str().unwrap_or("tool");
    let status = match result {
        None => "pending",
        Some(r) => {
            let meta = extension(r, "chisel/tool_result_meta");
            if meta["success"] == true {
                "completed"
            } else if is_canceled(r) || meta["failure_reason"] == "Canceled" {
                "cancelled"
            } else {
                "failed"
            }
        }
    };
    // Arguments are an object; a JSON string is read as one too.
    let args = match &call["arguments"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        v => v.clone(),
    };
    let detail = [
        "command",
        "file_path",
        "path",
        "url",
        "pattern",
        "query",
        "skill",
    ]
    .iter()
    .find_map(|k| args[k].as_str())
    .map(|d| format!(" {}", first_line(d, 120)))
    .unwrap_or_default();
    format!("[{name}] {status}{detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Nodes in the shape Devin 3000.11.92 writes them (.agents/findings/devin.md §3, §11),
    // trimmed to the fields read.

    fn prompt(id: &str, client: Option<&str>, text: &str) -> Value {
        let mut n = json!({"message_id": id, "role": "user", "content": text,
            "metadata": {"is_user_input": true, "extensions": {}}});
        if let Some(c) = client {
            n["metadata"]["extensions"]["chisel/client-message-id"] = json!(c);
        }
        n
    }

    fn assistant(id: &str, text: &str, finish: &str, calls: Value) -> Value {
        json!({"message_id": id, "role": "assistant", "content": text, "tool_calls": calls,
            "metadata": {"finish_reason": finish, "created_at": "2026-10-20T08:00:00Z"}})
    }

    fn tool(call: &str, ext: Value) -> Value {
        json!({"message_id": format!("t-{call}"), "role": "tool", "tool_call_id": call,
            "content": "Canceled due to user interrupt", "metadata": {"extensions": ext}})
    }

    fn exec(id: &str, command: &str) -> Value {
        json!([{"id": id, "name": "exec", "arguments": {"command": command}, "kind": "function"}])
    }

    fn system() -> Value {
        json!({"message_id": "s", "role": "system", "content": "You are Devin"})
    }

    /// A completed turn with a tool call, a cancelled one, a killed one, a prompt joined
    /// mid-turn, and an unfinished one.
    fn history() -> History {
        History::new(vec![
            system(),
            prompt("u1", Some("c1"), "run ls"),
            assistant("a1", "Listing.", "tool_calls", exec("exec:0#1", "ls")),
            tool(
                "exec:0#1",
                json!({"chisel/tool_result_meta": {"success": true, "kind": "execute"}}),
            ),
            assistant("a2", "Done.", "stop", json!([])),
            prompt("u2", Some("c2"), "sleep"),
            assistant("a3", "", "tool_calls", exec("exec:1#2", "sleep 30")),
            tool(
                "exec:1#2",
                json!({"chisel/tool_failure": {"reason": "Canceled"}, "chisel/tool_result_meta": {"success": false, "failure_reason": "Canceled"}}),
            ),
            prompt("u3", None, "killed"),
            prompt("u4", Some("c4"), "first"),
            assistant("a4", "", "tool_calls", exec("exec:2#3", "sleep 5")),
            tool(
                "exec:2#3",
                json!({"chisel/tool_result_meta": {"success": true}}),
            ),
            prompt("u5", Some("c5"), "also this"),
            assistant("a5", "Both answered.", "stop", json!(null)),
            prompt("u6", Some("c6"), "still running"),
            assistant("a6", "", "tool_calls", exec("exec:3#4", "make")),
        ])
    }

    #[test]
    fn turns_end_at_a_final_a_cancel_or_the_next_prompt() {
        let h = history();
        let ends: Vec<_> = h.spans.iter().map(|s| (s.id.as_str(), s.end)).collect();
        assert_eq!(
            ends,
            [
                ("c1", Some(End::Completed(4))),
                ("c2", Some(End::Interrupted(7))),
                // No client message id: the message id is the turn id.
                ("u3", Some(End::Superseded)),
                // A prompt joined mid-turn: the chain cannot tell it from a killed one.
                ("c4", Some(End::Superseded)),
                ("c5", Some(End::Completed(13))),
                ("c6", None),
            ]
        );
        let t = h.turn("devin:x".into(), 0).unwrap();
        assert_eq!(
            (t.status, t.final_text.as_deref()),
            (TurnStatus::Completed, Some("Done."))
        );
        assert_eq!(
            h.turn("devin:x".into(), 1).unwrap().status,
            TurnStatus::Interrupted
        );
        assert_eq!(
            h.turn("devin:x".into(), 2).unwrap().status,
            TurnStatus::Unknown
        );
        assert!(h.turn("devin:x".into(), 5).is_none());
        assert_eq!(h.find("c5"), Some(4));
        assert_eq!(h.first_prompt(), Some("run ls"));
    }

    #[test]
    fn messages_normalize_prompts_text_and_tool_calls() {
        let h = history();
        let m: Vec<_> = h
            .messages()
            .into_iter()
            .map(|(i, m)| (i, m.turn_id, m.item_id, m.phase, m.text))
            .collect();
        let row = |i, t: &str, item: &str, phase, text: &str| {
            (i, t.to_string(), item.to_string(), phase, text.to_string())
        };
        assert_eq!(
            m[..4],
            [
                row(1, "c1", "u1-prompt", Phase::Prompt, "run ls"),
                row(2, "c1", "a1-commentary", Phase::Commentary, "Listing."),
                row(2, "c1", "exec:0#1", Phase::Other, "[exec] completed ls"),
                row(4, "c1", "a2-final", Phase::Final, "Done."),
            ]
        );
        assert_eq!(m[5].4, "[exec] cancelled sleep 30");
        assert_eq!(m.last().unwrap().4, "[exec] pending make");
        assert!(m.iter().any(|r| r.2 == "a5-final" && r.1 == "c5"));
    }

    /// Fields that do not fit keep the node and end no turn: an odd `tool_calls` with
    /// `finish_reason: stop` is not a final, a tool call without an id is skipped, and string
    /// arguments are read.
    #[test]
    fn odd_fields_keep_the_node() {
        let h = History::new(vec![
            prompt("u1", Some("c1"), "x"),
            json!({"message_id": 7, "role": "assistant", "content": ["x"], "tool_calls": "no",
                "metadata": {"finish_reason": "stop"}}),
            assistant(
                "a1",
                "",
                "tool_calls",
                json!([{"name": "exec"}, {"id": "e#1", "name": "read", "arguments": "{\"file_path\":\"/a\"}"}]),
            ),
            Value::String("not json".into()),
        ]);
        assert_eq!(h.spans[0].end, None);
        let texts: Vec<_> = h.messages().into_iter().map(|(_, m)| m.text).collect();
        assert_eq!(texts, ["x", "[read] pending /a"]);
    }
}
