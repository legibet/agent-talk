//! `updates.jsonl`, the conversation log of a Grok session: the ACP session updates as
//! persisted, one JSON object per line, append-only (DESIGN.md §6.4). Streamed
//! chunks are coalesced on disk: one `agent_message_chunk` line per stretch of text.
//!
//! Turns: a `user_message_chunk` with `_meta.promptIndex` opens one, a `turn_completed
//! {prompt_id, stop_reason}` closes it. Lines the agent writes during a turn carry its
//! prompt id in `params._meta.promptId`, and the turn id is that prompt id (the
//! `promptId` agent-talk chose for its turns). A second writer on the same session
//! interleaves turns (DESIGN.md §6.4): the other process closes a running turn as
//! `interrupted` and the first keeps appending to it, so lines are attributed by their
//! prompt id where they carry one and by position otherwise, and the last
//! `turn_completed` of a prompt id is its end.

use crate::agents::first_line;
use crate::model::{self, Message, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub struct Line {
    /// 1-based line number in the file, the item id.
    pub no: usize,
    pub raw: Value,
}

pub fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            serde_json::from_str(l)
                .ok()
                .map(|raw| Line { no: i + 1, raw })
        })
        .collect()
}

/// The lines of `updates.jsonl`; a missing file (a session that has written no update
/// yet) is an empty history.
pub fn load(path: &Path) -> Result<Vec<Line>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(parse(&t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(super::io_err(&path.display().to_string(), e)),
    }
}

fn update(raw: &Value) -> &Value {
    &raw["params"]["update"]
}

fn kind(raw: &Value) -> &str {
    update(raw)["sessionUpdate"].as_str().unwrap_or("")
}

/// The prompt id a line names: `turn_completed.prompt_id`, else `params._meta.promptId`.
fn prompt_of(raw: &Value) -> Option<&str> {
    update(raw)["prompt_id"]
        .as_str()
        .or_else(|| raw["params"]["_meta"]["promptId"].as_str())
}

/// A user prompt that opens a turn; steer messages (`interjection`) join the running one.
fn opens_turn(raw: &Value) -> bool {
    let meta = &update(raw)["_meta"];
    kind(raw) == "user_message_chunk"
        && meta["promptIndex"].is_u64()
        && meta["interjection"].as_bool() != Some(true)
}

pub struct Span {
    /// Index (into the lines) of the opening user line.
    pub start: usize,
    /// The prompt id, once a line of the turn names it.
    pub id: Option<String>,
    /// Index of the turn's last `turn_completed`.
    pub end: Option<usize>,
}

pub struct History {
    pub lines: Vec<Line>,
    pub spans: Vec<Span>,
    /// The span each line belongs to (`None` before the first prompt).
    owner: Vec<Option<usize>>,
}

impl History {
    pub fn new(lines: Vec<Line>) -> History {
        let mut spans: Vec<Span> = Vec::new();
        let mut owner = Vec::with_capacity(lines.len());
        let mut current: Option<usize> = None;
        for (i, l) in lines.iter().enumerate() {
            if opens_turn(&l.raw) {
                spans.push(Span {
                    start: i,
                    id: None,
                    end: None,
                });
                current = Some(spans.len() - 1);
            }
            let prompt = prompt_of(&l.raw);
            let this = match prompt {
                Some(p) => match spans.iter().rposition(|s| s.id.as_deref() == Some(p)) {
                    Some(s) => Some(s),
                    None => match current {
                        Some(c) if spans[c].id.is_none() => {
                            spans[c].id = Some(p.to_string());
                            current
                        }
                        // A prompt cancelled within milliseconds of starting gets a
                        // turn_completed and no user line: a turn of its own.
                        _ if kind(&l.raw) == "turn_completed" => {
                            spans.push(Span {
                                start: i,
                                id: Some(p.to_string()),
                                end: None,
                            });
                            Some(spans.len() - 1)
                        }
                        _ => current,
                    },
                },
                None => current,
            };
            if kind(&l.raw) == "turn_completed"
                && let Some(s) = this
                && spans[s].id.as_deref() == prompt
            {
                spans[s].end = Some(i);
            }
            owner.push(this);
        }
        History {
            lines,
            spans,
            owner,
        }
    }

    pub fn turn_id(&self, span: usize) -> String {
        let s = &self.spans[span];
        s.id.clone()
            .unwrap_or_else(|| format!("line-{}", self.lines[s.start].no))
    }

    pub fn find(&self, turn_id: &str) -> Option<usize> {
        (0..self.spans.len()).rfind(|&s| self.turn_id(s) == turn_id)
    }

    /// Normalized messages, oldest first: user prompts, agent text (a stretch of
    /// text between tool calls is one message; the last one of a finished turn is
    /// `final`, earlier ones `commentary`), and each tool call as one `other` message
    /// carrying its latest status. Thoughts, hooks and bookkeeping are not messages.
    pub fn messages(&self) -> Vec<Msg> {
        let mut out: Vec<Msg> = Vec::new();
        let mut tools: HashMap<&str, (usize, String, String)> = HashMap::new();
        for (i, l) in self.lines.iter().enumerate() {
            let span = self.owner[i];
            let turn_id = span.map(|s| self.turn_id(s)).unwrap_or_default();
            let u = update(&l.raw);
            let at = l.raw["timestamp"]
                .as_i64()
                .and_then(|s| jiff::Timestamp::from_second(s).ok())
                .map(|t| t.to_string());
            let message = |role, phase, text: String| Message {
                turn_id: turn_id.clone(),
                item_id: format!("line-{}", l.no),
                role,
                phase,
                text,
                from: None,
                timestamp: at.clone(),
            };
            match kind(&l.raw) {
                "user_message_chunk" => {
                    let c = &u["content"];
                    let text = c["_meta"]["displayText"]
                        .as_str()
                        .or(c["text"].as_str())
                        .unwrap_or("");
                    out.push(Msg {
                        line: i,
                        span,
                        prompt: opens_turn(&l.raw),
                        message: message("user", "prompt", text.to_string()),
                    });
                }
                "agent_message_chunk" => {
                    let text = u["content"]["text"].as_str().unwrap_or("");
                    let extends = out.last().is_some_and(|m| {
                        m.span == span
                            && m.message.role == "assistant"
                            && m.message.phase == "commentary"
                    });
                    match out.last_mut() {
                        Some(m) if extends => m.message.text.push_str(text),
                        _ => out.push(Msg {
                            line: i,
                            span,
                            prompt: false,
                            message: message("assistant", "commentary", text.to_string()),
                        }),
                    }
                }
                "tool_call" => {
                    let Some(id) = u["toolCallId"].as_str() else {
                        continue;
                    };
                    let name = u["_meta"]["x.ai/tool"]["name"]
                        .as_str()
                        .or(u["title"].as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let input = &u["rawInput"];
                    let detail = [
                        "command",
                        "file_path",
                        "path",
                        "url",
                        "query",
                        "description",
                    ]
                    .iter()
                    .find_map(|k| input[k].as_str())
                    .map(|d| format!(" {}", first_line(d, 120)))
                    .unwrap_or_default();
                    let status = u["status"].as_str().unwrap_or("pending");
                    out.push(Msg {
                        line: i,
                        span,
                        prompt: false,
                        message: message("assistant", "other", tool_text(&name, status, &detail)),
                    });
                    tools.insert(id, (out.len() - 1, name, detail));
                }
                "tool_call_update" => {
                    if let Some((idx, name, detail)) =
                        u["toolCallId"].as_str().and_then(|id| tools.get(id))
                        && let Some(status) = u["status"].as_str()
                    {
                        out[*idx].message.text = tool_text(name, status, detail);
                    }
                }
                _ => {}
            }
        }
        for (s, span) in self.spans.iter().enumerate() {
            if span.end.is_some()
                && let Some(m) = out.iter_mut().rfind(|m| m.span == Some(s))
                && m.message.phase == "commentary"
            {
                m.message.phase = "final";
            }
        }
        out
    }

    /// The finished turn of a span: status from `stop_reason`, the final message as
    /// `final_text`.
    pub fn turn(&self, handle: String, span: usize, messages: &[Msg]) -> Option<model::Turn> {
        let s = &self.spans[span];
        let end = &self.lines[s.end?].raw;
        let u = update(end);
        let (status, error) = status(u["stop_reason"].as_str().unwrap_or(""), u);
        Some(model::Turn {
            handle,
            turn_id: self.turn_id(span),
            status,
            error,
            final_text: messages
                .iter()
                .find(|m| m.span == Some(span) && m.message.phase == "final")
                .map(|m| m.message.text.clone()),
            duration_ms: u["elapsed_ms"].as_i64(),
            basis: Some("updates.jsonl turn_completed".into()),
        })
    }
}

pub struct Msg {
    /// Index of the line it came from.
    pub line: usize,
    pub span: Option<usize>,
    /// The user prompt that opened the turn (not a steer message).
    pub prompt: bool,
    pub message: Message,
}

fn tool_text(name: &str, status: &str, detail: &str) -> String {
    format!("[{name}] {status}{detail}")
}

/// Turn status for an ACP stop reason (`session/prompt` `stopReason`,
/// `turn_completed.stop_reason`); `detail` goes into the error of a turn that did not
/// complete.
pub fn status(stop_reason: &str, detail: &Value) -> (&'static str, Option<Value>) {
    match stop_reason {
        "end_turn" => ("completed", None),
        // cancelled: session/cancel, a rejected permission, Ctrl+C in the TUI;
        // interrupted: written by another process that found the turn dangling.
        "cancelled" | "interrupted" => ("interrupted", Some(detail.clone())),
        "" => ("unknown", Some(detail.clone())),
        _ => ("failed", Some(detail.clone())),
    }
}

/// Text of the first user prompt, for `ls` previews.
pub fn first_prompt(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    BufReader::new(f)
        .lines()
        .map_while(std::result::Result::ok)
        .take(2000)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .find(opens_turn)
        .and_then(|raw| update(&raw)["content"]["text"].as_str().map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal lines from probe sessions (grok 1.0.46: two-writer and step-3 probes); usage
    // objects and long fields trimmed.

    /// Two `-p` processes on one session: A runs a shell loop, B starts meanwhile,
    /// closes A's turn as `interrupted` and runs its own; A keeps appending.
    const TWO_WRITERS: &str = r#"{"timestamp":1791130579,"method":"session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Run this shell command: for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 2; done . Then reply with exactly DONE-A."},"_meta":{"modelId":"grok-4.7","promptIndex":2}},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-5","agentTimestampMs":1791130579932}}}
{"timestamp":1791130583,"method":"session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Running the requested shell loop from 1 to 8."}},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-9","agentTimestampMs":1791130583719,"promptId":"d6b9ca89-36d5-49bf-af24-4c558560ee39","turnStartMs":1791130579962}}}
{"timestamp":1791130584,"method":"session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"I'll run the loop, then reply with exactly DONE-A."}},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-22","agentTimestampMs":1791130583778,"promptId":"d6b9ca89-36d5-49bf-af24-4c558560ee39","turnStartMs":1791130579962}}}
{"timestamp":1791130584,"method":"session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"tool_call","toolCallId":"call-9ea6ba67-c211-45c3-9a85-b7357bfb9be0-0","title":"run_terminal_command","rawInput":{"command":"for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 2; done","description":"Count 1–8 with 2s pauses"},"_meta":{"x.ai/tool":{"version":1,"name":"run_terminal_command","kind":"execute","namespace":"grok_build","label":"Run Command","read_only":false}}},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-24","agentTimestampMs":1791130584248,"promptId":"d6b9ca89-36d5-49bf-af24-4c558560ee39","turnStartMs":1791130579962}}}
{"timestamp":1791130585,"method":"_x.ai/session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"turn_completed","prompt_id":"d6b9ca89-36d5-49bf-af24-4c558560ee39","stop_reason":"interrupted","agent_result":"Grok stopped before this turn finished (the agent process exited or was restarted). Committed tool results were kept."},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-0","agentTimestampMs":1791130585690}}}
{"timestamp":1791130585,"method":"_x.ai/session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"hook_execution","event_name":"session_start","runs":[]},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-2","agentTimestampMs":1791130585827}}}
{"timestamp":1791130585,"method":"session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Reply with exactly DONE-B."},"_meta":{"modelId":"grok-4.7","promptIndex":3}},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-6","agentTimestampMs":1791130585931}}}
{"timestamp":1791130588,"method":"session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"DONE-B"}},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-10","agentTimestampMs":1791130588528,"promptId":"c173acf1-b1ac-4810-a2a2-a22afd57eb78","turnStartMs":1791130585970}}}
{"timestamp":1791130588,"method":"_x.ai/session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"turn_completed","prompt_id":"c173acf1-b1ac-4810-a2a2-a22afd57eb78","stop_reason":"end_turn","elapsed_ms":2791},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-14","agentTimestampMs":1791130588714}}}
{"timestamp":1791130602,"method":"session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"DONE-A"}},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-31","agentTimestampMs":1791130602183,"promptId":"d6b9ca89-36d5-49bf-af24-4c558560ee39","turnStartMs":1791130579962}}}
{"timestamp":1791130602,"method":"_x.ai/session/update","params":{"sessionId":"01a107af-f588-7081-8451-89dbf41db1d5","update":{"sessionUpdate":"turn_completed","prompt_id":"d6b9ca89-36d5-49bf-af24-4c558560ee39","stop_reason":"end_turn","elapsed_ms":22400},"_meta":{"eventId":"01a107af-f588-7081-8451-89dbf41db1d5-35","agentTimestampMs":1791130602319}}}"#;

    /// `session/cancel` right after the prompt started: no agent line names the turn.
    const CANCELLED_EARLY: &str = r#"{"timestamp":1791191274,"method":"session/update","params":{"sessionId":"01a10b51-d31c-71f2-92f5-c7f513e76db6","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Run exactly this shell command and then reply DONE: sleep 20"},"_meta":{"modelId":"grok-4.7","promptIndex":0}},"_meta":{"eventId":"01a10b51-d31c-71f2-92f5-c7f513e76db6-2","agentTimestampMs":1791191274475}}}
{"timestamp":1791191274,"method":"_x.ai/session/update","params":{"sessionId":"01a10b51-d31c-71f2-92f5-c7f513e76db6","update":{"sessionUpdate":"turn_completed","prompt_id":"40d64389-3fc6-4a15-bdb7-fc3957a2ca47","stop_reason":"cancelled","elapsed_ms":9},"_meta":{"eventId":"01a10b51-d31c-71f2-92f5-c7f513e76db6-3","agentTimestampMs":1791191274483,"cancellationCategory":"MidTurnAbort"}}}"#;

    /// `session/cancel` 3 ms after the next prompt started (provenance header and tool
    /// lines of the previous turn dropped): Grok wrote its turn_completed and no user line.
    const CANCELLED_AT_START: &str = r#"{"timestamp":1791192172,"method":"session/update","params":{"sessionId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Run exactly this shell command and then reply DONE: sleep 30"},"_meta":{"modelId":"grok-4.7","promptIndex":2}},"_meta":{"eventId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722-27","agentTimestampMs":1791192172061}}}
{"timestamp":1791192175,"method":"session/update","params":{"sessionId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Running `sleep 30` and replying DONE."}},"_meta":{"eventId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722-30","agentTimestampMs":1791192175370,"promptId":"3df7f532-785a-44ff-95cd-694e97681a73","turnStartMs":1791192172201}}}
{"timestamp":1791192178,"method":"_x.ai/session/update","params":{"sessionId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722","update":{"sessionUpdate":"turn_completed","prompt_id":"3df7f532-785a-44ff-95cd-694e97681a73","stop_reason":"cancelled","elapsed_ms":5868},"_meta":{"eventId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722-34","agentTimestampMs":1791192177936,"cancellationCategory":"MidTurnAbort"}}}
{"timestamp":1791192181,"method":"_x.ai/session/update","params":{"sessionId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722","update":{"sessionUpdate":"background_tasks","tasks":[]},"_meta":{"eventId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722-37","agentTimestampMs":1791192181229}}}
{"timestamp":1791192181,"method":"_x.ai/session/update","params":{"sessionId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722","update":{"sessionUpdate":"turn_completed","prompt_id":"e171c160-7ac7-4e44-9ffa-ac57134b9035","stop_reason":"cancelled","elapsed_ms":3},"_meta":{"eventId":"01a10b5e-e2a0-7773-84a9-c1ded0a37722-39","agentTimestampMs":1791192181246,"cancellationCategory":"MidTurnAbort"}}}"#;

    /// A steer message (`_x.ai/interject`) folded into a running turn after its tool call.
    const INTERJECTION: &str = r#"{"timestamp":1791130857,"method":"session/update","params":{"sessionId":"01a107b7-9e80-7e82-8808-76482c1baa7e","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Run this shell command: for i in 1 2 3 4 5 6; do echo $i; sleep 2; done . Then reply with exactly DONE-1 followed by anything else you were asked."},"_meta":{"modelId":"grok-4.7","promptIndex":2}},"_meta":{"eventId":"01a107b7-9e80-7e82-8808-76482c1baa7e-40","agentTimestampMs":1791130857633}}}
{"timestamp":1791130863,"method":"session/update","params":{"sessionId":"01a107b7-9e80-7e82-8808-76482c1baa7e","update":{"sessionUpdate":"tool_call","toolCallId":"call-bb2f58be-0697-4067-be5a-ca3af755a647-0","title":"run_terminal_command","rawInput":{"command":"for i in 1 2 3 4 5 6; do echo $i; sleep 2; done","description":"Echo 1-6 with 2s pauses"},"_meta":{"x.ai/tool":{"version":1,"name":"run_terminal_command","kind":"execute","namespace":"grok_build","label":"Run Command","read_only":false}}},"_meta":{"eventId":"01a107b7-9e80-7e82-8808-76482c1baa7e-54","agentTimestampMs":1791130863165,"promptId":"67b0e5d7-d14f-4e29-b565-b9e7b9d6c797","turnStartMs":1791130857635}}}
{"timestamp":1791130875,"method":"session/update","params":{"sessionId":"01a107b7-9e80-7e82-8808-76482c1baa7e","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-bb2f58be-0697-4067-be5a-ca3af755a647-0","status":"completed","content":[{"type":"content","content":{"type":"text","text":"1\n2\n3\n4\n5\n6\n"}}]},"_meta":{"eventId":"01a107b7-9e80-7e82-8808-76482c1baa7e-56","agentTimestampMs":1791130875428,"promptId":"67b0e5d7-d14f-4e29-b565-b9e7b9d6c797","turnStartMs":1791130857635}}}
{"timestamp":1791130877,"method":"session/update","params":{"sessionId":"01a107b7-9e80-7e82-8808-76482c1baa7e","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"The user sent a message while you were working:\n<user_query>\nAlso append the word CHERRY to your final reply.\n</user_query>\nIf the user is asking for a response, address the user first. After replying, complete any unfinished tasks from previous turns.","_meta":{"displayText":"Also append the word CHERRY to your final reply."}},"_meta":{"modelId":"grok-4.7","interjection":true}},"_meta":{"eventId":"01a107b7-9e80-7e82-8808-76482c1baa7e-57","agentTimestampMs":1791130875428}}}
{"timestamp":1791130879,"method":"session/update","params":{"sessionId":"01a107b7-9e80-7e82-8808-76482c1baa7e","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"DONE-1 CHERRY"}},"_meta":{"eventId":"01a107b7-9e80-7e82-8808-76482c1baa7e-64","agentTimestampMs":1791130879146,"promptId":"67b0e5d7-d14f-4e29-b565-b9e7b9d6c797","turnStartMs":1791130857635}}}
{"timestamp":1791130879,"method":"_x.ai/session/update","params":{"sessionId":"01a107b7-9e80-7e82-8808-76482c1baa7e","update":{"sessionUpdate":"turn_completed","prompt_id":"67b0e5d7-d14f-4e29-b565-b9e7b9d6c797","stop_reason":"end_turn","elapsed_ms":21750},"_meta":{"eventId":"01a107b7-9e80-7e82-8808-76482c1baa7e-66","agentTimestampMs":1791130879300}}}"#;

    fn texts(h: &History) -> Vec<(String, &'static str, String)> {
        h.messages()
            .into_iter()
            .map(|m| (m.message.turn_id, m.message.phase, m.message.text))
            .collect()
    }

    #[test]
    fn interleaved_writers_attribute_by_prompt_id() {
        let h = History::new(parse(TWO_WRITERS));
        let a = "d6b9ca89-36d5-49bf-af24-4c558560ee39";
        let b = "c173acf1-b1ac-4810-a2a2-a22afd57eb78";
        assert_eq!(h.spans.len(), 2);
        assert_eq!(h.spans[0].id.as_deref(), Some(a));
        assert_eq!(h.spans[1].id.as_deref(), Some(b));
        // A's last turn_completed (end_turn) is its end, not B's `interrupted` marker.
        assert_eq!(h.spans[0].end, Some(10));
        let m = h.messages();
        let t = h.turn("grok:s".into(), 0, &m).unwrap();
        assert_eq!(
            (t.status, t.final_text.as_deref()),
            ("completed", Some("DONE-A"))
        );
        assert_eq!(
            texts(&h),
            [
                (a.into(), "prompt", "Run this shell command: for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 2; done . Then reply with exactly DONE-A.".into()),
                (a.into(), "commentary", "I'll run the loop, then reply with exactly DONE-A.".into()),
                (a.into(), "other", "[run_terminal_command] pending for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 2; done".into()),
                (b.into(), "prompt", "Reply with exactly DONE-B.".into()),
                (b.into(), "final", "DONE-B".into()),
                (a.into(), "final", "DONE-A".into()),
            ]
        );
        assert_eq!(m[0].line, 0);
        assert!(m[0].prompt && !m[1].prompt);
    }

    #[test]
    fn turn_named_by_its_turn_completed_alone() {
        let h = History::new(parse(CANCELLED_EARLY));
        let id = "40d64389-3fc6-4a15-bdb7-fc3957a2ca47";
        let span = h.find(id).unwrap();
        let t = h.turn("grok:s".into(), span, &h.messages()).unwrap();
        assert_eq!((t.status, t.final_text), ("interrupted", None));
        assert_eq!(t.duration_ms, Some(9));
    }

    #[test]
    fn turn_completed_without_a_user_line_is_its_own_turn() {
        let h = History::new(parse(CANCELLED_AT_START));
        let span = h.find("e171c160-7ac7-4e44-9ffa-ac57134b9035").unwrap();
        let t = h.turn("grok:s".into(), span, &h.messages()).unwrap();
        assert_eq!((t.status, t.duration_ms), ("interrupted", Some(3)));
        // The turn before it keeps its own end.
        assert!(h.spans[0].end.is_some() && h.spans.len() == 2);
    }

    #[test]
    fn steer_message_joins_the_running_turn() {
        let h = History::new(parse(INTERJECTION));
        assert_eq!(h.spans.len(), 1);
        let id = "67b0e5d7-d14f-4e29-b565-b9e7b9d6c797".to_string();
        let m = texts(&h);
        assert_eq!(
            m[1],
            (
                id.clone(),
                "other",
                "[run_terminal_command] completed for i in 1 2 3 4 5 6; do echo $i; sleep 2; done"
                    .into()
            )
        );
        assert_eq!(
            m[2],
            (
                id.clone(),
                "prompt",
                "Also append the word CHERRY to your final reply.".into()
            )
        );
        assert_eq!(m[3], (id, "final", "DONE-1 CHERRY".into()));
        assert!(!h.messages()[2].prompt);
    }

    #[test]
    fn running_turn_has_no_end_and_a_positional_id() {
        let lines: Vec<&str> = TWO_WRITERS.lines().take(1).collect();
        let h = History::new(parse(&lines.join("\n")));
        assert_eq!(h.turn_id(0), "line-1");
        assert!(h.turn("grok:s".into(), 0, &h.messages()).is_none());
    }
}
