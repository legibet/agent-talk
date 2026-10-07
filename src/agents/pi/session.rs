//! pi session files, `<agent dir>/sessions/--<cwd>--/<timestamp>_<id>.jsonl`: decoding, the
//! live branch, turn boundaries and normalized messages (DESIGN.md §6.6). Pure. The records
//! are narrow: every field defaults when absent, unknown entry types, roles, content shapes
//! and block types are tolerated, and content of an unknown shape never ends a turn.

use super::io_err;
use crate::agents::{first_line, strip_provenance};
use crate::model::{self, Caller, Error, ErrorCode, Message, Result};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// `message` of a `message` entry (pi's `AgentMessage`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Body {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    content: Content,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Unix milliseconds; the one value shared by a run-log event and the persisted entry.
    #[serde(default)]
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
    /// A shape this version does not know; it may hold anything, tool calls included.
    Other(IgnoredAny),
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Block {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolCall {
        #[serde(default)]
        name: String,
        #[serde(default)]
        arguments: Value,
    },
    /// thinking, image, …
    #[serde(other)]
    Other,
}

impl Body {
    /// The text blocks joined; a string content as is.
    pub fn text(&self) -> String {
        match &self.content {
            Content::Text(s) => s.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Content::Other(_) => String::new(),
        }
    }

    fn tool_calls(&self) -> Vec<String> {
        match &self.content {
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    Block::ToolCall { name, arguments } => Some(tool_summary(name, arguments)),
                    _ => None,
                })
                .collect(),
            Content::Text(_) | Content::Other(_) => Vec::new(),
        }
    }

    /// An assistant message that ends a run: no tool calls to execute (DESIGN.md §6.6).
    /// Content of an unknown shape is not known to be free of them.
    pub fn is_reply(&self) -> bool {
        self.role == "assistant"
            && match &self.content {
                Content::Text(_) => true,
                Content::Blocks(b) => !b.iter().any(|b| matches!(b, Block::ToolCall { .. })),
                Content::Other(_) => false,
            }
    }
}

/// The decoded part of one entry.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// ISO 8601.
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub message: Option<Body>,
    /// `session` header: the working directory pi runs the session in.
    #[serde(default)]
    pub cwd: Option<String>,
    /// `session_info`: the display name.
    #[serde(default)]
    pub name: Option<String>,
    /// `context_edit`: the entry whose content later model context sees edited, or, with a
    /// `null` replacement, not at all (an auto-retried failure).
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub replacement: Value,
    /// `custom_message`: text an extension injected into the context.
    #[serde(default)]
    pub content: Option<Value>,
}

impl Line {
    pub fn is_prompt(&self) -> bool {
        self.kind == "message" && self.message.as_ref().is_some_and(|m| m.role == "user")
    }
}

/// An entry kept both raw (for `--raw`) and decoded (best-effort).
pub struct Entry {
    pub raw: Value,
    pub line: Line,
}

pub fn load(path: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(path).map_err(|e| io_err(&path.display().to_string(), e))?;
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|raw| Entry {
            line: Line::deserialize(&raw).unwrap_or_default(),
            raw,
        })
        .collect())
}

/// Indices of the live branch: the newest entry (the leaf pi takes on load) and its
/// `parentId` ancestry. A second writer leaves other branches in the file (DESIGN.md §6.6);
/// they are excluded.
pub fn live_branch(entries: &[Entry]) -> HashSet<usize> {
    let by_id: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.line.id.as_deref().map(|id| (id, i)))
        .collect();
    let mut branch = HashSet::new();
    let mut next = entries.iter().rposition(|e| e.line.id.is_some());
    while let Some(i) = next {
        if !branch.insert(i) {
            break;
        }
        next = entries[i]
            .line
            .parent_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied());
    }
    branch
}

/// What ended a turn in the file.
pub enum TurnEnd {
    /// An assistant message without tool calls.
    Reply(usize),
    /// A later user message; the reply, if any, was not recorded on this branch.
    LaterPrompt(usize),
}

/// The first entry on the live branch after the prompt at `start` that ends its turn. A
/// failed message that pi retried is hidden by a later `context_edit` with a `null`
/// replacement and does not count; an edit with content keeps the entry.
pub fn turn_end(entries: &[Entry], branch: &HashSet<usize>, start: usize) -> Option<TurnEnd> {
    let hidden: HashSet<&str> = entries
        .iter()
        .enumerate()
        .filter(|(i, e)| {
            branch.contains(i) && e.line.kind == "context_edit" && e.line.replacement.is_null()
        })
        .filter_map(|(_, e)| e.line.target_id.as_deref())
        .collect();
    entries
        .iter()
        .enumerate()
        .skip(start + 1)
        .filter(|(i, _)| branch.contains(i))
        .find_map(|(i, e)| {
            if e.line.is_prompt() {
                return Some(TurnEnd::LaterPrompt(i));
            }
            let hidden = e.line.id.as_deref().is_some_and(|id| hidden.contains(id));
            match &e.line.message {
                Some(m) if m.is_reply() && !hidden => Some(TurnEnd::Reply(i)),
                _ => None,
            }
        })
}

/// Turn status pi's `stopReason` means.
pub fn status(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("stop" | "length") => "completed",
        Some("error") => "failed",
        Some("aborted") => "interrupted",
        _ => "unknown",
    }
}

/// Turn derived from the live branch, from the prompt at `start` to `end`. No duration:
/// pi stamps an assistant message when its request starts (DESIGN.md §6.6).
pub fn transcript_turn(
    handle: String,
    entries: &[Entry],
    start: usize,
    end: TurnEnd,
) -> model::Turn {
    let turn_id = entries[start].line.id.clone().unwrap_or_default();
    match end {
        TurnEnd::Reply(i) => {
            let body = entries[i].line.message.as_ref();
            let stop = body.and_then(|m| m.stop_reason.as_deref());
            let status = status(stop);
            model::Turn {
                handle,
                turn_id,
                status,
                error: (status != "completed").then(|| {
                    serde_json::json!({
                        "stop_reason": stop,
                        "message": body.and_then(|m| m.error_message.clone()),
                    })
                }),
                final_text: body.map(Body::text).filter(|t| !t.is_empty()),
                duration_ms: None,
                basis: Some(format!(
                    "session file: assistant message {} without tool calls, stopReason {} (best effort; a pi process may still be writing)",
                    entries[i].line.id.as_deref().unwrap_or("?"),
                    stop.unwrap_or("absent")
                )),
            }
        }
        TurnEnd::LaterPrompt(i) => model::Turn {
            handle,
            turn_id,
            status: "unknown",
            error: Some(serde_json::json!({
                "message": "a later user message follows the prompt on the live branch and no reply without tool calls was recorded between them",
            })),
            final_text: None,
            duration_ms: None,
            basis: Some(format!(
                "session file: later user message {} (best effort)",
                entries[i].line.id.as_deref().unwrap_or("?")
            )),
        },
    }
}

/// Messages on the live branch with their entry index, oldest first. A turn's id is the id
/// of its user message; steer and follow-up messages are user messages too and start a turn
/// of their own here (the file does not mark them, DESIGN.md §6.6).
pub fn messages(entries: &[Entry], branch: &HashSet<usize>) -> Vec<(usize, Message)> {
    let mut out = Vec::new();
    let mut turn_id = "";
    for (i, e) in entries.iter().enumerate() {
        let Some(id) = e.line.id.as_deref() else {
            continue;
        };
        if !branch.contains(&i) {
            continue;
        }
        if e.line.is_prompt() {
            turn_id = id;
        }
        if let Some(m) = to_message(&e.line, turn_id, id) {
            out.push((i, m));
        }
    }
    out
}

/// Normalize one entry. `message` entries with a user, assistant or tool-result role carry
/// content, and `custom_message` entries are text an extension put before the model
/// (`from: runtime`); system messages, extension state, usage and the rest stay in `--raw`.
/// Thinking-only messages yield nothing.
fn to_message(line: &Line, turn_id: &str, item_id: &str) -> Option<Message> {
    let (role, phase, text, from) = match line.kind.as_str() {
        "message" => {
            let body = line.message.as_ref()?;
            match body.role.as_str() {
                "user" => ("user", "prompt", body.text(), None),
                "toolResult" => {
                    let tag = if body.is_error == Some(true) {
                        "[tool_result error]"
                    } else {
                        "[tool_result]"
                    };
                    let name = body.tool_name.as_deref().unwrap_or("");
                    (
                        "user",
                        "other",
                        format!("{tag} {name} {}", first_line(&body.text(), 120))
                            .trim()
                            .to_string(),
                        None,
                    )
                }
                "assistant" => {
                    let text = body.text();
                    let tools = body.tool_calls();
                    if !text.is_empty() {
                        let phase = if tools.is_empty() {
                            "final"
                        } else {
                            "commentary"
                        };
                        ("assistant", phase, text, None)
                    } else if !tools.is_empty() {
                        ("assistant", "other", tools.join("\n"), None)
                    } else if let Some(err) = &body.error_message {
                        ("assistant", "other", format!("[error] {err}"), None)
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        "custom_message" => {
            let text = match line.content.as_ref()? {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|i| i["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return None,
            };
            ("user", "other", text, Some(Caller::RUNTIME))
        }
        _ => return None,
    };
    Some(Message {
        turn_id: turn_id.into(),
        item_id: item_id.into(),
        role,
        phase,
        text,
        from,
        timestamp: line.timestamp.clone(),
    })
}

pub fn tool_summary(name: &str, arguments: &Value) -> String {
    let detail = ["command", "path", "file_path", "pattern", "url", "query"]
        .iter()
        .find_map(|k| arguments[k].as_str())
        .map(|d| format!(" {}", first_line(d, 120)))
        .unwrap_or_default();
    format!("[tool_call {name}]{detail}")
}

/// cwd, name and first prompt of a session file. The name is the newest `session_info`
/// (`--name` and `/name` append one at any point).
pub fn head(path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let (mut cwd, mut name, mut preview) = (None, None, None);
    let Ok(f) = File::open(path) else {
        return (cwd, name, preview);
    };
    for l in BufReader::new(f).lines().map_while(std::result::Result::ok) {
        let Ok(line) = serde_json::from_str::<Line>(&l) else {
            continue;
        };
        match line.kind.as_str() {
            "session" => cwd = cwd.or(line.cwd.clone()),
            "session_info" => name = line.name.clone(),
            _ => {}
        }
        if preview.is_none() && line.is_prompt() {
            preview = line
                .message
                .as_ref()
                .map(|m| first_line(strip_provenance(&m.text()), 200));
        }
    }
    (cwd, name, preview)
}

/// The directory pi keeps a cwd's sessions in: the leading separator removed and `/`, `\`
/// and `:` replaced by `-`, between `--` (pi 1.0.3 `session-manager.js`).
pub fn dir_name(cwd: &str) -> String {
    let body: String = cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(cwd)
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{body}--")
}

/// Session ids pi accepts for `--session-id`: alphanumerics, `.`, `_` and `-`, starting and
/// ending alphanumeric. Anything else is refused before it reaches a path.
pub fn check_id(id: &str) -> Result<()> {
    let ok = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.ends_with(|c: char| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::Precondition,
            format!(
                "invalid pi session id {id}; pi accepts alphanumerics, '.', '_' and '-', starting and ending alphanumeric"
            ),
        ))
    }
}

/// The session file `*_<id>.jsonl` under any cwd directory of `sessions`. Ids are unique
/// per cwd only; a second match is an error, not a guess.
pub fn find(sessions: &Path, id: &str) -> Result<Option<PathBuf>> {
    let suffix = format!("_{id}.jsonl");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(sessions)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|d| d.path().is_dir())
        .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
        .map(|f| f.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
        })
        .collect();
    hits.sort();
    match hits.len() {
        0 => Ok(None),
        1 => Ok(hits.pop()),
        _ => Err(Error::new(
            ErrorCode::Precondition,
            format!(
                "session id {id} exists in more than one pi project directory: {}",
                hits.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal entries from the probes of 2026-10-07 (pi 1.0.3; system prompt and usage trimmed).
    const FILE: &str = r#"{"type":"session","version":3,"id":"5384ce3e-1111-4222-8333-444444444444","timestamp":"2026-10-07T12:32:33.000Z","cwd":"/tmp/work"}
{"type":"session_info","id":"deca4f66","parentId":null,"timestamp":"2026-10-07T12:32:33.001Z","name":"probe1"}
{"type":"model_change","id":"71617668","parentId":"deca4f66","timestamp":"2026-10-07T12:32:33.200Z","provider":"deepseek","modelId":"deepseek-flash"}
{"type":"message","id":"22eab53a","parentId":"71617668","timestamp":"2026-10-07T12:32:33.300Z","message":{"role":"system","content":"","sections":{"preamble":"You are..."},"timestamp":1791376353300}}
{"type":"message","id":"4924e440","parentId":"22eab53a","timestamp":"2026-10-07T12:32:33.301Z","message":{"role":"user","content":[{"type":"text","text":"[from codex:abc via agent-talk; answer in your final response]\n\nRun sleep then reply done"}],"timestamp":1791376353300}}
{"type":"custom","id":"c0ffee01","parentId":"4924e440","timestamp":"2026-10-07T12:32:33.400Z","customType":"lovely-dev-tools.llm-call-constraints","data":{}}
{"type":"message","id":"11195ee5","parentId":"c0ffee01","timestamp":"2026-10-07T12:32:34.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Running it."},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"sleep 25"}}],"provider":"deepseek","model":"deepseek-flash","stopReason":"toolUse","timestamp":1791376354000}}
{"type":"message","id":"be0ff72d","parentId":"11195ee5","timestamp":"2026-10-07T12:32:59.000Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":""}],"isError":false,"timestamp":1791376379000}}
{"type":"message","id":"81a95062","parentId":"be0ff72d","timestamp":"2026-10-07T12:32:59.100Z","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"529 overloaded","timestamp":1791376379100}}
{"type":"context_edit","id":"ce000001","parentId":"81a95062","timestamp":"2026-10-07T12:32:59.200Z","targetId":"81a95062","replacement":null}
{"type":"message","id":"3cdeb9ba","parentId":"ce000001","timestamp":"2026-10-07T12:33:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"provider":"deepseek","model":"deepseek-flash","stopReason":"stop","timestamp":1791376381000}}
{"type":"message","id":"0e04f0f2","parentId":"3cdeb9ba","timestamp":"2026-10-07T12:34:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly B"}],"timestamp":1791376440000}}
{"type":"message","id":"7664d1ae","parentId":"0e04f0f2","timestamp":"2026-10-07T12:34:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"B"}],"stopReason":"stop","timestamp":1791376441000}}
{"type":"context_edit","id":"ce000002","parentId":"3cdeb9ba","timestamp":"2026-10-07T12:34:05.000Z","targetId":"3cdeb9ba","replacement":{"content":[{"type":"text","text":"done (edited)"}]}}
{"type":"message","id":"87eef124","parentId":"ce000002","timestamp":"2026-10-07T12:34:10.000Z","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly A"}],"timestamp":1791376450000}}
"#;

    fn entries() -> Vec<Entry> {
        FILE.lines()
            .map(|l| {
                let raw: Value = serde_json::from_str(l).unwrap();
                Entry {
                    line: Line::deserialize(&raw).unwrap(),
                    raw,
                }
            })
            .collect()
    }

    fn idx(entries: &[Entry], id: &str) -> usize {
        entries
            .iter()
            .position(|e| e.line.id.as_deref() == Some(id))
            .unwrap()
    }

    /// The live branch is the newest entry's ancestry: the second writer's branch (B) is
    /// left out, the turn with a retried failure ends at the retry, and a `context_edit`
    /// that replaces the final's content does not hide it.
    #[test]
    fn live_branch_turns_and_messages() {
        let e = entries();
        let branch = live_branch(&e);
        assert!(branch.contains(&idx(&e, "87eef124")));
        assert!(branch.contains(&idx(&e, "3cdeb9ba")));
        assert!(!branch.contains(&idx(&e, "0e04f0f2")));
        assert!(!branch.contains(&idx(&e, "7664d1ae")));

        let start = idx(&e, "4924e440");
        let Some(TurnEnd::Reply(i)) = turn_end(&e, &branch, start) else {
            panic!("expected a reply")
        };
        assert_eq!(e[i].line.id.as_deref(), Some("3cdeb9ba"));
        let t = transcript_turn("pi:s".into(), &e, start, TurnEnd::Reply(i));
        assert_eq!(
            (t.status, t.final_text.as_deref()),
            ("completed", Some("done"))
        );
        assert_eq!(t.turn_id, "4924e440");
        // The last prompt has no reply yet.
        assert!(turn_end(&e, &branch, idx(&e, "87eef124")).is_none());

        let msgs = messages(&e, &branch);
        let view: Vec<(&str, &str, &str)> = msgs
            .iter()
            .map(|(_, m)| (m.role, m.phase, m.item_id.as_str()))
            .collect();
        assert_eq!(
            view,
            [
                ("user", "prompt", "4924e440"),
                ("assistant", "commentary", "11195ee5"),
                ("user", "other", "be0ff72d"),
                ("assistant", "other", "81a95062"),
                ("assistant", "final", "3cdeb9ba"),
                ("user", "prompt", "87eef124"),
            ]
        );
        assert!(
            msgs.iter()
                .all(|(_, m)| m.turn_id == "4924e440" || m.turn_id == "87eef124")
        );
        assert_eq!(msgs[2].1.text, "[tool_result] bash");
        assert_eq!(msgs[3].1.text, "[error] 529 overloaded");
        assert_eq!(
            msgs[0].1.timestamp.as_deref(),
            Some("2026-10-07T12:32:33.301Z")
        );
    }

    /// Content of a shape this version does not know is tolerated but never a reply.
    #[test]
    fn unknown_content_is_not_a_reply() {
        let odd: Body = serde_json::from_str(
            r#"{"role":"assistant","content":{"kind":"future"},"stopReason":"stop"}"#,
        )
        .unwrap();
        assert!(!odd.is_reply() && odd.text().is_empty());
        let bare: Body =
            serde_json::from_str(r#"{"role":"assistant","content":[{"type":"text"}]}"#).unwrap();
        assert!(bare.is_reply() && bare.text().is_empty());
    }

    #[test]
    fn paths_and_ids() {
        assert_eq!(dir_name("/Users/me/proj"), "--Users-me-proj--");
        assert_eq!(dir_name("C:\\work\\x"), "--C--work-x--");
        assert!(check_id("5384ce3e-1111-4222-8333-444444444444").is_ok());
        assert!(check_id("a.b_c-d").is_ok());
        assert!(check_id("-abc").is_err());
        assert!(check_id("bad/id").is_err());
        assert!(check_id("").is_err());
        assert_eq!(status(Some("stop")), "completed");
        assert_eq!(status(Some("error")), "failed");
        assert_eq!(status(Some("aborted")), "interrupted");
        assert_eq!(status(None), "unknown");
    }
}
