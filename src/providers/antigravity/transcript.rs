//! Conversation transcripts, `brain/<id>/.system_generated/logs/transcript.jsonl`:
//! decoding, turn boundaries and normalized messages. Pure apart from reading the file;
//! the records are narrow and unknown fields and step types are tolerated.

use super::io_err;
use crate::model::{Caller, Message, Result};
use crate::providers::{first_line, strip_provenance};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// One line of `transcript.jsonl`: one step, written when it first finishes or runs.
#[derive(Debug, Deserialize)]
struct Step {
    #[serde(default)]
    step_index: Option<i64>,
    /// USER_INPUT | PLANNER_RESPONSE | SYSTEM_MESSAGE | GENERIC | RUN_COMMAND | …
    /// Required: a line without it is not a step.
    #[serde(rename = "type")]
    kind: String,
    /// USER_EXPLICIT | MODEL | SYSTEM
    #[serde(default)]
    source: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    #[serde(default)]
    name: String,
    /// Values are JSON-encoded strings (`"CommandLine": "\"ls\""`).
    #[serde(default)]
    args: Value,
}

impl Step {
    fn is_user(&self) -> bool {
        self.kind == "USER_INPUT"
    }

    fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("").trim()
    }

    fn is_planner(&self) -> bool {
        self.kind == "PLANNER_RESPONSE"
    }

    /// A planner response with text and no tool call: a reply that can end a turn.
    fn is_reply(&self) -> bool {
        self.is_planner() && self.tool_calls.is_empty() && !self.text().is_empty()
    }

    /// A tool's own step (GENERIC, RUN_COMMAND, VIEW_FILE, …). System bookkeeping steps
    /// (CHECKPOINT, CONVERSATION_HISTORY) are not.
    fn is_tool(&self) -> bool {
        !matches!(
            self.kind.as_str(),
            "USER_INPUT" | "PLANNER_RESPONSE" | "SYSTEM_MESSAGE"
        ) && self.source != "SYSTEM"
    }
}

/// A transcript line kept both raw (for `--raw`) and decoded (decoding is best-effort).
pub struct Entry {
    pub raw: Value,
    /// `None` for a line that is not a step object.
    step: Option<Step>,
}

pub fn parse_entries(text: &str) -> Vec<Entry> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let raw: Value = serde_json::from_str(l).unwrap_or_else(|_| json!({"unparsed": l}));
            let step = Step::deserialize(&raw).ok();
            Entry { raw, step }
        })
        .collect()
}

/// The transcript's entries; none when it does not exist (yet).
pub fn load(path: &Path) -> Result<Vec<Entry>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_entries(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io_err(&path.display().to_string(), e)),
    }
}

/// Whether the transcript has any line, and the first prompt's first line, reading no
/// further than the first USER_INPUT. An unreadable transcript has neither.
pub fn preview(path: &Path) -> (bool, Option<String>) {
    let Ok(f) = File::open(path) else {
        return (false, None);
    };
    let mut any = false;
    for l in BufReader::new(f).lines().map_while(std::result::Result::ok) {
        if l.trim().is_empty() {
            continue;
        }
        any = true;
        if let Ok(s) = serde_json::from_str::<Step>(&l)
            && s.is_user()
        {
            return (
                true,
                Some(first_line(strip_provenance(user_text(s.text())), 200)),
            );
        }
    }
    (any, None)
}

/// Line position of the USER_INPUT step whose index is `turn_id`.
pub fn user_input(lines: &[Entry], turn_id: &str) -> Option<usize> {
    lines.iter().position(|l| {
        l.step.as_ref().is_some_and(|s| {
            s.is_user() && s.step_index.map(|n| n.to_string()).as_deref() == Some(turn_id)
        })
    })
}

/// The user's text in a USER_INPUT step: agy wraps it in `<USER_REQUEST>` and appends
/// metadata blocks (`<ADDITIONAL_METADATA>`, `<USER_SETTINGS_CHANGE>`, …) after it.
fn user_text(content: &str) -> &str {
    match content.strip_prefix("<USER_REQUEST>\n") {
        Some(rest) => rest
            .split_once("\n</USER_REQUEST>")
            .map_or(rest, |(t, _)| t),
        None => content,
    }
}

/// A SYSTEM_MESSAGE step's message without the vendor's preamble.
fn system_text(content: &str) -> &str {
    content
        .split_once("<SYSTEM_MESSAGE>\n")
        .and_then(|(_, rest)| rest.split_once("\n</SYSTEM_MESSAGE>"))
        .map_or(content, |(t, _)| t)
        .trim()
}

/// `[tool_use name] detail` for one call of a planner response.
fn tool_summary(call: &ToolCall) -> String {
    let arg = |k: &str| {
        call.args[k]
            .as_str()
            .map(|s| serde_json::from_str::<String>(s).unwrap_or_else(|_| s.to_string()))
    };
    let detail = arg("CommandLine")
        .or_else(|| arg("toolSummary"))
        .map(|d| format!(" {}", first_line(&d, 120)))
        .unwrap_or_default();
    format!("[tool_use {}]{detail}", call.name)
}

/// Normalized messages with their line position, oldest first. A turn is the run of lines
/// from one USER_INPUT to the next, its id that step's index; its last reply is `final`,
/// earlier planner text `commentary`, tool calls and tool steps `other`. A SYSTEM_MESSAGE
/// is a user message from `runtime`; the caller fills in `from` of the other user messages.
pub fn messages(lines: &[Entry]) -> Vec<(usize, Message)> {
    let mut finals = HashSet::new();
    let mut last_reply = None;
    for (i, l) in lines.iter().enumerate() {
        match &l.step {
            Some(s) if s.is_user() => finals.extend(last_reply.take()),
            Some(s) if s.is_reply() => last_reply = Some(i),
            _ => {}
        }
    }
    finals.extend(last_reply);

    let mut out = Vec::new();
    let mut turn_id = String::new();
    // Tool names of the latest planner responses, consumed by the tool steps that follow.
    let mut pending_tools: VecDeque<String> = VecDeque::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(s) = &l.step else { continue };
        let step = s.step_index.map(|n| n.to_string()).unwrap_or_default();
        if s.is_user() {
            turn_id = step.clone();
        }
        let msg = |item_id: String, role, phase, text: String, from| Message {
            turn_id: turn_id.clone(),
            item_id,
            role,
            phase,
            text,
            from,
            timestamp: s.created_at.clone(),
        };
        let item = format!("{step}:{i}");
        if s.is_user() {
            out.push((
                i,
                msg(item, "user", "other", user_text(s.text()).to_string(), None),
            ));
        } else if s.kind == "SYSTEM_MESSAGE" {
            out.push((
                i,
                msg(
                    item,
                    "user",
                    "other",
                    system_text(s.text()).to_string(),
                    Some(Caller::RUNTIME),
                ),
            ));
        } else if s.is_planner() {
            if !s.text().is_empty() {
                let phase = if finals.contains(&i) {
                    "final"
                } else {
                    "commentary"
                };
                out.push((
                    i,
                    msg(item.clone(), "assistant", phase, s.text().to_string(), None),
                ));
            }
            if !s.tool_calls.is_empty() {
                pending_tools.extend(s.tool_calls.iter().map(|c| c.name.clone()));
                let text = s.tool_calls.iter().map(tool_summary).collect::<Vec<_>>();
                out.push((
                    i,
                    msg(
                        format!("{item}:tools"),
                        "assistant",
                        "other",
                        text.join("\n"),
                        None,
                    ),
                ));
            }
        } else if s.is_tool() {
            let name = pending_tools
                .pop_front()
                .unwrap_or_else(|| s.kind.to_lowercase());
            out.push((
                i,
                msg(
                    item,
                    "assistant",
                    "other",
                    format!("[{name}] {}", s.status),
                    None,
                ),
            ));
        }
    }
    out
}

/// The turn the USER_INPUT at line `start` began, from the transcript alone: (span end,
/// bounded), bounded when a later USER_INPUT ends it, else the span so far.
pub fn turn_span(lines: &[Entry], start: usize) -> (usize, bool) {
    match lines[start + 1..]
        .iter()
        .position(|l| l.step.as_ref().is_some_and(Step::is_user))
    {
        Some(n) => (start + 1 + n, true),
        None => (lines.len(), false),
    }
}

/// Whether the span's last model step is a reply without tool calls.
pub fn ends_with_reply(span: &[Entry]) -> bool {
    span.iter()
        .filter_map(|l| l.step.as_ref())
        .rfind(|s| s.is_planner() || s.is_tool())
        .is_some_and(Step::is_reply)
}

pub fn span_reply(span: &[Entry]) -> Option<String> {
    span.iter()
        .filter_map(|l| l.step.as_ref())
        .rfind(|s| s.is_reply())
        .map(|s| s.text().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal transcript lines (brain/<id>/.system_generated/logs/transcript.jsonl, agy
    // 1.2.16, three conversations): a chat with a runtime system message, an open turn
    // whose last step is a tool step, and a turn whose tool step stays RUNNING (background
    // task) before the reply.
    const CHAT: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-03T12:59:21Z","content":"<USER_REQUEST>\nRemember the word: banana. Reply OK.\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-03T06:59:21-06:00.\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Gemini 3.8 Flash (Low). No need to comment on this change if the user doesn't ask about it. If reporting what model you are, please use a human readable name instead of the exact string.\n</USER_SETTINGS_CHANGE>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-03T12:59:22Z","input_tokens":12330,"cache_read_tokens":0,"output_tokens":24,"content":"OK"}
{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-03T12:59:31Z","content":"<USER_REQUEST>\nWhat word did I ask you to remember?\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-03T06:59:31-06:00.\n</ADDITIONAL_METADATA>"}
{"step_index":3,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-10-03T12:59:32Z","content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user. It is provided by the system as important information to pay attention to.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-03T12:59:31Z sender=system priority=MESSAGE_PRIORITY_LOW content=[Notice] All your subagents and background tasks have been stopped due to server restart. If you want a subagent to continue working, it needs to be revived by sending it a new message. If resuming work, please check on status and restart as needed.\n</SYSTEM_MESSAGE>"}
{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-03T12:59:32Z","input_tokens":12563,"cache_read_tokens":0,"output_tokens":26,"content":"You asked me to remember **banana**."}"#;

    const TOOL_OPEN: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-03T13:07:54Z","content":"<USER_REQUEST>\nRun 'ls' and reply with the number of files.\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-03T07:07:54-06:00.\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Gemini 3.8 Flash (High). No need to comment on this change if the user doesn't ask about it. If reporting what model you are, please use a human readable name instead of the exact string.\n</USER_SETTINGS_CHANGE>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-03T13:07:54Z","input_tokens":12325,"cache_read_tokens":0,"output_tokens":356,"tool_calls":[{"name":"run_command","args":{"CommandLine":"\"ls\"","Cwd":"\"/Users/me/projects/demo\"","WaitMsBeforeAsync":"5000","toolAction":"\"Listing directory\"","toolSummary":"\"List directory contents\""}}]}
{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-10-03T13:07:58Z","content":"Created At: 2026-10-03T07:07:58-06:00\nCompleted At: 2026-10-03T07:07:59-06:00\n\nThe command exited with code 0.\nOutput:\nc1.txt\t\tc5.txt\t\te1.txt\t\terr.txt\t\tto.err\r\nc2.txt\t\td1.txt\t\te2.txt\t\tmarker.txt\tto.json\r\nc3.txt\t\td2.txt\t\te3.txt\t\treq.txt\t\tto2.err\r\nc4.txt\t\td3.txt\t\te4.txt\t\tt1.json\r\n\n"}"#;

    const BACKGROUND: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-03T13:00:34Z","content":"<USER_REQUEST>\nRun 'sleep 60' in the shell, then reply DONE.\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-03T07:00:34-06:00.\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Gemini 3.8 Flash (Low). No need to comment on this change if the user doesn't ask about it. If reporting what model you are, please use a human readable name instead of the exact string.\n</USER_SETTINGS_CHANGE>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-03T13:00:34Z","input_tokens":12341,"cache_read_tokens":0,"output_tokens":357,"tool_calls":[{"name":"run_command","args":{"CommandLine":"\"sleep 60\"","Cwd":"\"/Users/me/projects/demo\"","WaitMsBeforeAsync":"500","toolAction":"\"Running sleep command\"","toolSummary":"\"Run sleep 60\""}}]}
{"step_index":2,"source":"MODEL","type":"GENERIC","status":"RUNNING","created_at":"2026-10-03T13:00:38Z","content":"Created At: 2026-10-03T07:00:38-06:00\nTool is running as a background task with task id: 1e880a1a-480a-4b24-8456-e971eb1f5656/task-2\nTask Description: sleep 60\nTask logs are available at: file:///Users/me/.gemini/antigravity-cli/brain/1e880a1a-480a-4b24-8456-e971eb1f5656/.system_generated/tasks/task-2.log\nYOU MUST TAKE ONE OF THE FOLLOWING TWO ACTIONS: A) either proceed to other relevant work (if any) or, B) simply update the user with a short message (that you have launched the command and will wait for it to finish) and end the turn.\n DO NOTHING ELSE."}
{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-03T13:00:38Z","input_tokens":12924,"cache_read_tokens":0,"output_tokens":53,"content":"I have started `sleep 60` in the background and will wait for it to complete."}"#;

    fn view(lines: &[Entry]) -> Vec<(usize, String, &'static str, &'static str, String)> {
        messages(lines)
            .into_iter()
            .map(|(i, m)| (i, m.turn_id, m.role, m.phase, m.text))
            .collect()
    }

    fn row(
        i: usize,
        turn: &str,
        role: &'static str,
        phase: &'static str,
        text: &str,
    ) -> (usize, String, &'static str, &'static str, String) {
        (i, turn.into(), role, phase, text.into())
    }

    #[test]
    fn transcript_messages() {
        let chat = parse_entries(CHAT);
        assert_eq!(
            view(&chat),
            vec![
                row(
                    0,
                    "0",
                    "user",
                    "other",
                    "Remember the word: banana. Reply OK."
                ),
                row(1, "0", "assistant", "final", "OK"),
                row(
                    2,
                    "2",
                    "user",
                    "other",
                    "What word did I ask you to remember?"
                ),
                row(
                    3,
                    "2",
                    "user",
                    "other",
                    "[Message] timestamp=2026-10-03T12:59:31Z sender=system priority=MESSAGE_PRIORITY_LOW content=[Notice] All your subagents and background tasks have been stopped due to server restart. If you want a subagent to continue working, it needs to be revived by sending it a new message. If resuming work, please check on status and restart as needed."
                ),
                row(
                    4,
                    "2",
                    "assistant",
                    "final",
                    "You asked me to remember **banana**."
                ),
            ]
        );
        assert_eq!(messages(&chat)[3].1.from, Some(Caller::RUNTIME));

        let tools = parse_entries(TOOL_OPEN);
        assert_eq!(
            view(&tools),
            vec![
                row(
                    0,
                    "0",
                    "user",
                    "other",
                    "Run 'ls' and reply with the number of files."
                ),
                row(1, "0", "assistant", "other", "[tool_use run_command] ls"),
                row(2, "0", "assistant", "other", "[run_command] DONE"),
            ]
        );
        let m = messages(&tools);
        assert_eq!(
            (m[1].1.item_id.as_str(), m[2].1.item_id.as_str()),
            ("1:1:tools", "2:2")
        );

        assert_eq!(
            view(&parse_entries(BACKGROUND)),
            vec![
                row(
                    0,
                    "0",
                    "user",
                    "other",
                    "Run 'sleep 60' in the shell, then reply DONE."
                ),
                row(
                    1,
                    "0",
                    "assistant",
                    "other",
                    "[tool_use run_command] sleep 60"
                ),
                row(2, "0", "assistant", "other", "[run_command] RUNNING"),
                row(
                    3,
                    "0",
                    "assistant",
                    "final",
                    "I have started `sleep 60` in the background and will wait for it to complete."
                ),
            ]
        );
    }

    #[test]
    fn transcript_turn_bounds() {
        let chat = parse_entries(CHAT);
        assert_eq!(user_input(&chat, "2"), Some(2));
        // Step 1 is a planner response, not a USER_INPUT.
        assert_eq!(user_input(&chat, "1"), None);
        // Turn 0 is bounded by the next USER_INPUT and ends with a reply.
        assert_eq!(turn_span(&chat, 0), (2, true));
        assert!(ends_with_reply(&chat[0..2]));
        // The last turn is open; its reply follows a system message.
        assert_eq!(turn_span(&chat, 2), (5, false));
        assert!(ends_with_reply(&chat[2..5]));
        assert_eq!(
            span_reply(&chat[2..5]).as_deref(),
            Some("You asked me to remember **banana**.")
        );
        // An open turn whose last step is a tool step has no reply yet.
        let tools = parse_entries(TOOL_OPEN);
        assert_eq!(turn_span(&tools, 0), (3, false));
        assert!(!ends_with_reply(&tools));
        assert_eq!(span_reply(&tools), None);
    }

    /// The preview is the first prompt without its wrapper; history is any line at all.
    #[test]
    fn transcript_preview() {
        let dir = std::env::temp_dir().join(format!("bagy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, CHAT).unwrap();
        assert_eq!(
            preview(&path),
            (true, Some("Remember the word: banana. Reply OK.".into()))
        );
        std::fs::write(&path, "\n{\"note\":\"not a step\"}\n").unwrap();
        assert_eq!(preview(&path), (true, None));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(preview(&path), (false, None));
    }
}
