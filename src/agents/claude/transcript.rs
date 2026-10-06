//! Session transcripts, `~/.claude/projects/<cwd-slug>/<session>.jsonl`: decoding, the
//! live branch, turn boundaries and normalized messages. Pure; the records are narrow and
//! unknown fields, line types and block types are tolerated.

use super::io_err;
use crate::agents::{first_line, strip_provenance};
use crate::model::{self, Caller, Message, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// `message` of a transcript `user`/`assistant` line.
#[derive(Debug, Default, Deserialize)]
struct Body {
    #[serde(default)]
    content: Content,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        #[serde(default)]
        content: Value,
        #[serde(default)]
        is_error: Option<bool>,
    },
    /// thinking, image, …
    #[serde(other)]
    Other,
}

impl Body {
    fn text(&self) -> String {
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
        }
    }

    fn tool_result_only(&self) -> bool {
        matches!(&self.content, Content::Blocks(b)
            if !b.is_empty() && b.iter().all(|b| matches!(b, Block::ToolResult { .. })))
    }
}

/// One transcript line.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    parent_uuid: Option<String>,
    /// Set where `parentUuid` is null after a compaction boundary.
    #[serde(default)]
    logical_parent_uuid: Option<String>,
    /// Shared by a turn's prompt line and its tool_result lines.
    #[serde(default)]
    prompt_id: Option<String>,
    #[serde(default)]
    is_sidechain: Option<bool>,
    #[serde(default)]
    is_meta: Option<bool>,
    #[serde(default)]
    message: Option<Body>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    entrypoint: Option<String>,
    /// `{"type":"custom-title"}` line: the session name (`claude --name`, `/rename`).
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    /// Who produced a user line: `{"kind":"human"}`, `{"kind":"task-notification",…}`.
    #[serde(default)]
    origin: Option<Value>,
    /// `typed`, `sdk`, `system`, …
    #[serde(default)]
    prompt_source: Option<String>,
    /// Set on the `[Request interrupted by user]` line.
    #[serde(default)]
    interrupted_message_id: Option<String>,
}

impl Line {
    fn sidechain(&self) -> bool {
        self.is_sidechain == Some(true)
    }

    /// The line a TUI appends when the user presses Esc mid-turn; the turn ends there and
    /// no `system/turn_duration` is written. Structural marker: `interruptedMessageId`
    /// (no isMeta, origin or promptSource on it). Without it, the text decides
    /// (`[Request interrupted by user]`, `… for tool use]`).
    fn is_interrupt(&self) -> bool {
        self.kind == "user"
            && (self.interrupted_message_id.is_some()
                || self
                    .message
                    .as_ref()
                    .is_some_and(|m| m.text().starts_with("[Request interrupted by user")))
    }

    /// Local slash-command bookkeeping (`<command-name>…`, `<local-command-stdout>…`).
    /// These lines carry no flag (no `origin`, no `promptSource`; only the companion
    /// `<local-command-caveat>` line has `isMeta`), so the leading tag is the marker.
    fn is_local_command(&self) -> bool {
        self.kind == "user"
            && self.message.as_ref().is_some_and(|m| {
                let t = m.text();
                let t = t.trim_start();
                t.starts_with("<command-name>") || t.starts_with("<local-command-")
            })
    }

    /// User input injected by the Claude Code runtime, not typed or sent by a client:
    /// `origin.kind` other than `human` (e.g. `task-notification`) or
    /// `promptSource: system`. Lines without either flag fall back to the leading tag.
    fn is_runtime(&self) -> bool {
        if self.kind != "user" {
            return false;
        }
        if self.is_interrupt() {
            return true;
        }
        let origin = self.origin.as_ref().and_then(|o| o["kind"].as_str());
        match (origin, self.prompt_source.as_deref()) {
            (Some(k), _) if k != "human" => true,
            (_, Some("system")) => true,
            (None, None) => self.message.as_ref().is_some_and(|m| {
                let t = m.text();
                let t = t.trim_start();
                t.starts_with("<task-notification>") || t.starts_with("<system-reminder>")
            }),
            _ => false,
        }
    }

    /// A user line that starts a turn: content that is not only tool results, and not
    /// injected meta content, a sub-agent line or an interrupt marker.
    fn is_prompt(&self) -> bool {
        self.kind == "user"
            && !self.sidechain()
            && self.is_meta != Some(true)
            && !self.is_interrupt()
            && !self.is_local_command()
            && self.message.as_ref().is_some_and(|m| !m.tool_result_only())
    }

    fn is_turn_duration(&self) -> bool {
        self.kind == "system" && self.subtype.as_deref() == Some("turn_duration")
    }

    fn parent(&self) -> Option<&str> {
        self.parent_uuid
            .as_deref()
            .or(self.logical_parent_uuid.as_deref())
    }
}

/// A transcript line kept both raw (for `--raw` and tolerant id reads) and decoded
/// (decoding is best-effort).
pub struct Entry {
    pub raw: Value,
    line: Option<Line>,
}

impl Entry {
    pub fn uuid(&self) -> Option<&str> {
        self.raw["uuid"].as_str()
    }

    pub fn is_prompt(&self) -> bool {
        self.line.as_ref().is_some_and(Line::is_prompt)
    }
}

fn parse_entries(text: &str) -> Vec<Entry> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<Value>(l) {
            Ok(raw) => {
                let line = Line::deserialize(&raw).ok();
                Some(Entry { raw, line })
            }
            // A line still being written; the next read sees it whole.
            Err(e) => {
                tracing::debug!("skipping unparsable transcript line: {e}");
                None
            }
        })
        .collect()
}

pub fn load(path: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(path).map_err(|e| io_err(&path.display().to_string(), e))?;
    Ok(parse_entries(&text))
}

/// Indices of `idx` and its ancestors, following `parentUuid` (or `logicalParentUuid`).
fn ancestry(entries: &[Entry], by_uuid: &HashMap<&str, usize>, idx: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut cur = Some(idx);
    while let Some(i) = cur {
        // A parent cycle would loop forever.
        if out.len() > entries.len() {
            break;
        }
        out.push(i);
        cur = entries[i]
            .line
            .as_ref()
            .and_then(Line::parent)
            .and_then(|p| by_uuid.get(p).copied());
    }
    out
}

/// Lines on the live branch: the newest main-chain line, if it descends from the newest
/// `last-prompt.leafUuid`, else that leaf, and its ancestry. A concurrent `--resume`
/// leaves orphaned branches in the file (DESIGN.md §6.2); they are excluded.
pub fn live_branch(entries: &[Entry]) -> HashSet<usize> {
    let by_uuid: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.uuid().map(|u| (u, i)))
        .collect();
    let newest = entries.iter().rposition(|e| {
        e.uuid().is_some()
            && e.line.as_ref().is_some_and(|l| {
                !l.sidechain() && matches!(l.kind.as_str(), "user" | "assistant" | "system")
            })
    });
    let leaf_prompt = entries
        .iter()
        .rev()
        .find(|e| e.raw["type"] == "last-prompt")
        .and_then(|e| e.raw["leafUuid"].as_str())
        .and_then(|u| by_uuid.get(u).copied());
    let leaf = match (newest, leaf_prompt) {
        (Some(n), Some(l)) if !ancestry(entries, &by_uuid, n).contains(&l) => Some(l),
        (n, l) => n.or(l),
    };
    let mut live: HashSet<usize> = leaf
        .map(|l| ancestry(entries, &by_uuid, l).into_iter().collect())
        .unwrap_or_default();
    // Parallel tool calls: each tool_result hangs off its own tool_use line and only
    // one of them is on the chain. Keep the siblings that belong to a live turn
    // (same promptId); a concurrent resume's synthesized tool_result carries that
    // other process's promptId and stays out.
    let live_prompts: HashSet<&str> = live
        .iter()
        .filter_map(|&i| entries[i].line.as_ref())
        .filter(|l| l.is_prompt())
        .filter_map(|l| l.prompt_id.as_deref())
        .collect();
    let siblings: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(i, e)| {
            !live.contains(i)
                && e.line.as_ref().is_some_and(|l| {
                    l.kind == "user"
                        && l.message.as_ref().is_some_and(Body::tool_result_only)
                        && l.prompt_id
                            .as_deref()
                            .is_some_and(|p| live_prompts.contains(p))
                        && l.parent()
                            .and_then(|p| by_uuid.get(p))
                            .is_some_and(|p| live.contains(p))
                })
        })
        .map(|(i, _)| i)
        .collect();
    live.extend(siblings);
    live
}

/// What ended a turn in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    /// `system/turn_duration` at this line: an interactive session finished the turn.
    Duration(usize),
    /// `[Request interrupted by user]` at this line (Esc in a TUI).
    Interrupt(usize),
    /// The next turn's prompt is at this line.
    NextPrompt(usize),
    /// No end marker, and no live process holds the session any more.
    ProcessGone,
}

/// The first line on the live branch after the prompt at `start` that ends its turn.
/// Interactive sessions (entrypoint `cli`) write `system/turn_duration` at turn end, or an
/// interrupt line on Esc; -p sessions write no marker, so only the next prompt ends their
/// turns (DESIGN.md §6.2).
pub fn turn_end(entries: &[Entry], branch: &HashSet<usize>, start: usize) -> Option<TurnEnd> {
    (start + 1..entries.len())
        .filter(|i| branch.contains(i))
        .find_map(|i| {
            let l = entries[i].line.as_ref()?;
            if l.is_turn_duration() {
                Some(TurnEnd::Duration(i))
            } else if l.is_interrupt() {
                Some(TurnEnd::Interrupt(i))
            } else if l.is_prompt() {
                Some(TurnEnd::NextPrompt(i))
            } else {
                None
            }
        })
}

/// Turn derived from the live-branch lines from the prompt at `start` to `end`.
/// `agent_status` is the session's status in `claude agents`, reported in the basis.
pub fn transcript_turn(
    handle: String,
    entries: &[Entry],
    branch: &HashSet<usize>,
    start: usize,
    end: TurnEnd,
    agent_status: Option<&str>,
) -> model::Turn {
    let stop = match end {
        // The turn_duration line belongs to the turn it closes.
        TurnEnd::Duration(i) => i + 1,
        TurnEnd::Interrupt(i) | TurnEnd::NextPrompt(i) => i,
        TurnEnd::ProcessGone => entries.len(),
    };
    let assistants: Vec<&Line> = (start..stop)
        .filter(|i| branch.contains(i))
        .filter_map(|i| entries[i].line.as_ref())
        .filter(|l| l.kind == "assistant")
        .collect();
    let final_text = assistants
        .iter()
        .rev()
        .filter_map(|l| l.message.as_ref().map(Body::text))
        .find(|t| !t.is_empty());
    // Without a marker, the last assistant line stopping on end_turn is the only hint.
    let by_stop_reason = if assistants
        .last()
        .and_then(|l| l.message.as_ref())
        .and_then(|m| m.stop_reason.as_deref())
        == Some("end_turn")
    {
        "completed"
    } else {
        "unknown"
    };
    let (status, duration_ms, basis) = match end {
        TurnEnd::Duration(i) => {
            let td = &entries[i].raw;
            (
                "completed",
                td["durationMs"].as_i64(),
                format!(
                    "transcript-derived, best effort: system/turn_duration after the prompt (pendingBackgroundAgentCount {}; claude agents status {})",
                    td["pendingBackgroundAgentCount"],
                    agent_status.unwrap_or("not listed")
                ),
            )
        }
        TurnEnd::Interrupt(_) => (
            "interrupted",
            None,
            "transcript-derived, best effort: '[Request interrupted by user]' after the prompt"
                .into(),
        ),
        TurnEnd::NextPrompt(_) => (
            by_stop_reason,
            None,
            "transcript-derived: a later user message exists".into(),
        ),
        TurnEnd::ProcessGone => (
            by_stop_reason,
            None,
            "transcript-derived: no live process holds the session".into(),
        ),
    };
    model::Turn {
        handle,
        turn_id: entries[start].uuid().unwrap_or_default().into(),
        status,
        error: None,
        final_text,
        duration_ms,
        basis: Some(basis),
    }
}

/// Messages on the live branch with their line index, oldest first. A turn's id is the
/// uuid of its prompt line; a tool_result line joins its own prompt's turn through
/// `promptId`, even when it is written after a later prompt.
pub fn messages(entries: &[Entry], branch: &HashSet<usize>) -> Vec<(usize, Message)> {
    let mut out = Vec::new();
    let mut turns_by_prompt: HashMap<&str, &str> = HashMap::new();
    let mut turn_id = "";
    for (i, e) in entries.iter().enumerate() {
        let (Some(line), Some(uuid)) = (&e.line, e.uuid()) else {
            continue;
        };
        if !branch.contains(&i) || line.sidechain() {
            continue;
        }
        if line.is_prompt() {
            turn_id = uuid;
            if let Some(p) = &line.prompt_id {
                turns_by_prompt.insert(p, uuid);
            }
        }
        let this_turn = match (line.kind.as_str(), line.prompt_id.as_deref()) {
            ("user", Some(p)) => turns_by_prompt.get(p).copied().unwrap_or(turn_id),
            _ => turn_id,
        };
        if let Some(m) = to_message(line, this_turn, uuid) {
            out.push((i, m));
        }
    }
    out
}

/// Normalize one transcript line. Only `user` and `assistant` lines carry content;
/// thinking-only lines yield nothing. Injected meta content and local slash-command
/// bookkeeping are not messages (they stay in `--raw`); runtime-injected input is kept
/// with `from: runtime`, since the model saw it.
fn to_message(line: &Line, turn_id: &str, item_id: &str) -> Option<Message> {
    if line.is_meta == Some(true) || line.is_local_command() {
        return None;
    }
    let body = line.message.as_ref()?;
    let (role, phase, text) = match line.kind.as_str() {
        "user" if body.tool_result_only() => ("user", "other", tool_result_summary(body)),
        "user" if line.is_interrupt() => ("user", "other", "[interrupted]".to_string()),
        "user" => ("user", "other", body.text()),
        "assistant" => {
            let text = body.text();
            if !text.is_empty() {
                let phase = if body.stop_reason.as_deref() == Some("end_turn") {
                    "final"
                } else {
                    "commentary"
                };
                ("assistant", phase, text)
            } else {
                let Content::Blocks(blocks) = &body.content else {
                    return None;
                };
                let tools: Vec<String> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { name, input } => Some(tool_summary(name, input)),
                        _ => None,
                    })
                    .collect();
                if tools.is_empty() {
                    return None;
                }
                ("assistant", "other", tools.join("\n"))
            }
        }
        _ => return None,
    };
    Some(Message {
        turn_id: turn_id.into(),
        item_id: item_id.into(),
        role,
        phase,
        text,
        from: line.is_runtime().then_some(Caller::RUNTIME),
        timestamp: line.timestamp.clone(),
    })
}

pub fn tool_summary(name: &str, input: &Value) -> String {
    let detail = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "description",
    ]
    .iter()
    .find_map(|k| input[k].as_str())
    .map(|d| format!(" {}", first_line(d, 120)))
    .unwrap_or_default();
    format!("[tool_use {name}]{detail}")
}

fn tool_result_summary(body: &Body) -> String {
    let Content::Blocks(blocks) = &body.content else {
        return "[tool_result]".into();
    };
    let mut parts = Vec::new();
    for b in blocks {
        if let Block::ToolResult { content, is_error } = b {
            let text = match content {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|i| i["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let tag = if *is_error == Some(true) {
                "[tool_result error]"
            } else {
                "[tool_result]"
            };
            parts.push(
                format!("{tag} {}", first_line(&text, 120))
                    .trim()
                    .to_string(),
            );
        }
    }
    parts.join("\n")
}

/// cwd, name, first prompt and entrypoint from the start of a transcript.
pub fn head(
    path: &Path,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let (mut cwd, mut name, mut preview, mut entrypoint) = (None, None, None, None);
    let Ok(f) = File::open(path) else {
        return (cwd, name, preview, entrypoint);
    };
    for l in BufReader::new(f)
        .lines()
        .map_while(std::result::Result::ok)
        .take(2000)
    {
        let Ok(line) = serde_json::from_str::<Line>(&l) else {
            continue;
        };
        cwd = cwd.or(line.cwd.clone());
        name = name.or(line.custom_title.clone());
        entrypoint = entrypoint.or(line.entrypoint.clone());
        if line.is_prompt() {
            preview = line
                .message
                .as_ref()
                .map(|m| first_line(strip_provenance(&m.text()), 200));
            break;
        }
    }
    (cwd, name, preview, entrypoint)
}

/// Project directory name Claude uses for a cwd.
pub fn slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal samples: transcript lines from
    // ~/.claude/projects/<slug>/37648f74-….jsonl.

    const USER_LINE: &str = r#"{"parentUuid":null,"isSidechain":false,"promptId":"529ea16b-7924-402d-94f9-89123e5a0f9e","type":"user","message":{"role":"user","content":"Reply with the single word pong."},"uuid":"fd2165bd-f040-4bc7-95d7-79241d222fbb","timestamp":"2026-10-03T12:33:43.295Z","permissionMode":"default","promptSource":"sdk","turnOrigin":"sdk","turnPosition":{"promptIndex":0,"turnIndex":1},"userType":"external","entrypoint":"sdk-cli","cwd":"/Users/me/projects/demo","sessionId":"37648f74-4df1-44ca-8ba1-bbe20ba3f8ff","version":"2.1.288","gitBranch":"HEAD"}"#;

    const ATTACHMENT_LINE: &str = r#"{"parentUuid":"fd2165bd-f040-4bc7-95d7-79241d222fbb","isSidechain":false,"type":"attachment","uuid":"02c63a74-aa3c-435f-8a85-1e873d755883","sessionId":"37648f74-4df1-44ca-8ba1-bbe20ba3f8ff"}"#;

    const ASSISTANT_LINE: &str = r#"{"parentUuid":"02c63a74-aa3c-435f-8a85-1e873d755883","isSidechain":false,"message":{"model":"claude-sonnet-5-5","id":"msg_011CffJyfGSEsRKWWTTKLQan","type":"message","role":"assistant","content":[{"type":"text","text":"pong"}],"container":null,"stop_reason":"end_turn","stop_sequence":null,"stop_details":null,"input_transformations":[],"diagnostics":null,"context_management":null},"apiBlockIndex":0,"requestId":"req_011CffJyefxk4AUaMLanQ46x","type":"assistant","uuid":"ef3a61c5-486b-400b-bce2-11e12cd41331","timestamp":"2026-10-03T12:33:45.925Z","userType":"external","entrypoint":"sdk-cli","cwd":"/Users/me/projects/demo","sessionId":"37648f74-4df1-44ca-8ba1-bbe20ba3f8ff","version":"2.1.288","gitBranch":"HEAD"}"#;

    const NEXT_USER_LINE: &str = r#"{"parentUuid":"ef3a61c5-486b-400b-bce2-11e12cd41331","isSidechain":false,"promptId":"18821898-a885-453c-ba93-8cf55e55d405","type":"user","message":{"role":"user","content":"Which single word did you reply with just before? Answer with that word only."},"uuid":"b475aedd-640d-45c0-8278-f1902361e4bd","timestamp":"2026-10-03T12:34:05.328Z","entrypoint":"sdk-cli","cwd":"/Users/me/projects/demo","sessionId":"37648f74-4df1-44ca-8ba1-bbe20ba3f8ff"}"#;

    // Captured from a stream-json probe run (2026-10-03).
    const TOOL_RESULT_LINE: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01UbJPaVihK2S9yynAdZR9Ta","type":"tool_result","content":"hello","is_error":false}]},"parent_tool_use_id":null,"session_id":"94f6d5f6-37b4-41b0-ab38-6e8de10bbf1b","uuid":"a1eb0645-8581-4d46-aa2e-c7763595805f","timestamp":"2026-10-03T14:56:28.783Z","tool_use_result":{"stdout":"hello","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false}}"#;

    fn line(s: &str) -> Line {
        serde_json::from_str(s).unwrap()
    }

    fn handle() -> String {
        "claude:s".into()
    }

    #[test]
    fn transcript_lines() {
        let user = line(USER_LINE);
        assert!(user.is_prompt());
        let m = to_message(&user, "t", "fd2165bd-f040-4bc7-95d7-79241d222fbb").unwrap();
        assert_eq!((m.role, m.phase), ("user", "other"));
        assert_eq!(m.text, "Reply with the single word pong.");

        let tool = line(TOOL_RESULT_LINE);
        assert!(!tool.is_prompt());
        let m = to_message(&tool, "t", "i").unwrap();
        assert_eq!(m.text, "[tool_result] hello");

        let assistant = line(ASSISTANT_LINE);
        let m = to_message(&assistant, "t", "i").unwrap();
        assert_eq!(
            (m.role, m.phase, m.text.as_str()),
            ("assistant", "final", "pong")
        );
    }

    #[test]
    fn turn_boundaries_from_transcript() {
        let text = [USER_LINE, ATTACHMENT_LINE, ASSISTANT_LINE, NEXT_USER_LINE].join("\n");
        let entries = parse_entries(&text);
        let live = live_branch(&entries);
        assert_eq!(live.len(), 4);
        assert_eq!(turn_end(&entries, &live, 0), Some(TurnEnd::NextPrompt(3)));
        assert_eq!(turn_end(&entries, &live, 3), None);
        let t = transcript_turn(handle(), &entries, &live, 0, TurnEnd::NextPrompt(3), None);
        assert_eq!(t.status, "completed");
        assert_eq!(t.final_text.as_deref(), Some("pong"));
    }

    // From the parent interactive transcript (claude 2.1.288), parent rewired to
    // ASSISTANT_LINE.
    const TURN_DURATION_LINE: &str = r#"{"parentUuid":"ef3a61c5-486b-400b-bce2-11e12cd41331","isSidechain":false,"type":"system","subtype":"turn_duration","durationMs":45007,"messageCount":684,"pendingBackgroundAgentCount":1,"timestamp":"2026-10-03T15:11:19.977Z","uuid":"3f82b4db-0000-0000-0000-000000000000","sessionId":"1be3c3b1-e4db-4aad-85e9-09ae4a2a161b"}"#;

    #[test]
    fn turn_duration_marks_turn_end() {
        let text = [
            USER_LINE,
            ATTACHMENT_LINE,
            ASSISTANT_LINE,
            TURN_DURATION_LINE,
        ]
        .join("\n");
        let entries = parse_entries(&text);
        let live = live_branch(&entries);
        let end = turn_end(&entries, &live, 0);
        assert_eq!(end, Some(TurnEnd::Duration(3)));
        let t = transcript_turn(handle(), &entries, &live, 0, end.unwrap(), None);
        assert_eq!((t.status, t.duration_ms), ("completed", Some(45007)));
    }

    /// Probe 1 shape: a concurrent resume (B) branches off A's tool_use; A's later
    /// lines are the live leaf, B's turn is orphaned. a3b is a parallel tool_result sibling of A.
    #[test]
    fn orphaned_branch_is_excluded() {
        let lines = [
            r#"{"type":"user","uuid":"a1","parentUuid":null,"promptId":"pa","message":{"role":"user","content":"sleep then first"}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"sleep 20"}}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"user","uuid":"b1","parentUuid":"a2","promptId":"pb","message":{"role":"user","content":[{"type":"tool_result","content":"[Tool call interrupted]","is_error":true}]}}"#,
            r#"{"type":"user","uuid":"b2","parentUuid":"b1","promptId":"pb","message":{"role":"user","content":"second"}}"#,
            r#"{"type":"assistant","uuid":"b3","parentUuid":"b2","message":{"role":"assistant","content":[{"type":"text","text":"second"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"last-prompt","leafUuid":"b3"}"#,
            r#"{"type":"user","uuid":"a3","parentUuid":"a2","promptId":"pa","message":{"role":"user","content":[{"type":"tool_result","content":""}]}}"#,
            r#"{"type":"user","uuid":"a3b","parentUuid":"a2","promptId":"pa","message":{"role":"user","content":[{"type":"tool_result","content":"parallel"}]}}"#,
            r#"{"type":"assistant","uuid":"a4","parentUuid":"a3","message":{"role":"assistant","content":[{"type":"text","text":"first"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"last-prompt","leafUuid":"a4"}"#,
        ];
        let entries = parse_entries(&lines.join("\n"));
        let live = live_branch(&entries);
        let uuids: HashSet<&str> = live.iter().filter_map(|&i| entries[i].uuid()).collect();
        assert_eq!(uuids, HashSet::from(["a1", "a2", "a3", "a3b", "a4"]));
        assert_eq!(turn_end(&entries, &live, 0), None);
    }

    /// After a compaction the boundary line has `parentUuid: null` and points back
    /// through `logicalParentUuid`; the lines before it stay on the live branch.
    #[test]
    fn compaction_boundary_keeps_earlier_lines() {
        // Boundary shape from a real interactive transcript (compactMetadata trimmed).
        let boundary = r#"{"parentUuid":null,"logicalParentUuid":"ef3a61c5-486b-400b-bce2-11e12cd41331","isSidechain":false,"type":"system","subtype":"compact_boundary","content":"Conversation compacted","level":"info","uuid":"c0000000-0000-0000-0000-000000000000"}"#;
        let next = NEXT_USER_LINE.replace(
            r#""parentUuid":"ef3a61c5-486b-400b-bce2-11e12cd41331""#,
            r#""parentUuid":"c0000000-0000-0000-0000-000000000000""#,
        );
        let text = [
            USER_LINE,
            ATTACHMENT_LINE,
            ASSISTANT_LINE,
            boundary,
            next.as_str(),
        ]
        .join("\n");
        let entries = parse_entries(&text);
        let live = live_branch(&entries);
        assert_eq!(live.len(), 5);
        assert_eq!(turn_end(&entries, &live, 0), Some(TurnEnd::NextPrompt(4)));
    }

    // From ~/.claude/projects/-Users-me-projects/8369ab5f-….jsonl (environment fields trimmed).
    const CAVEAT_LINE: &str = r#"{"parentUuid":null,"isSidechain":false,"promptId":"8c54d57d-da40-4a60-8fae-c792739881d1","type":"user","message":{"role":"user","content":"<local-command-caveat>The command below was run directly in Claude Code, not sent to you as a request, and its output goes straight to the user. It's recorded here as context for later messages.</local-command-caveat>"},"isMeta":true,"uuid":"37b40075-0fb8-486d-9c10-05e54bebca93","timestamp":"2026-10-03T15:33:34.419Z"}"#;
    const COMMAND_LINE: &str = r#"{"parentUuid":"37b40075-0fb8-486d-9c10-05e54bebca93","isSidechain":false,"promptId":"8c54d57d-da40-4a60-8fae-c792739881d1","type":"user","message":{"role":"user","content":"<command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args></command-args>"},"uuid":"aaf0d84f-2ae0-4227-b5f1-57c1ef0b5b73","timestamp":"2026-10-03T15:33:34.419Z"}"#;
    const COMMAND_STDOUT_LINE: &str = r#"{"parentUuid":"aaf0d84f-2ae0-4227-b5f1-57c1ef0b5b73","isSidechain":false,"promptId":"8c54d57d-da40-4a60-8fae-c792739881d1","type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to `Opus 5.5` for this session only</local-command-stdout>"},"uuid":"c95a4fb4-c4c7-40c6-a138-27ff570d2045","timestamp":"2026-10-03T15:33:34.419Z"}"#;
    const HUMAN_LINE: &str = r#"{"parentUuid":"77641974-744f-4f93-ad1c-5ba473af9235","isSidechain":false,"promptId":"2fc91780-b4f8-4acb-9a8b-7877f3e622fb","type":"user","message":{"role":"user","content":"reply with the single word hello"},"uuid":"3f96dc4b-d6dd-4989-8564-9ec7918b62f7","timestamp":"2026-10-03T15:33:49.026Z","origin":{"kind":"human"},"promptSource":"typed","turnOrigin":"human"}"#;
    const TASK_NOTIFICATION_LINE: &str = r#"{"parentUuid":"b9116c14-dd32-42d0-ba3e-8e144fc36dcf","isSidechain":false,"promptId":"2855a424-3eed-4042-97c9-b87ad47c1cbc","type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>bgevs17w6</task-id>\n<status>completed</status>\n</task-notification>"},"uuid":"e3fa3913-1d7e-4069-aaf3-2d1cf966a11e","timestamp":"2026-10-03T15:43:41.194Z","origin":{"kind":"task-notification","producer":"session-task"},"promptSource":"system","turnOrigin":"task_notification"}"#;

    #[test]
    fn local_commands_meta_and_runtime_input() {
        for s in [CAVEAT_LINE, COMMAND_LINE, COMMAND_STDOUT_LINE] {
            let l = line(s);
            assert!(!l.is_prompt());
            assert!(to_message(&l, "t", "i").is_none());
        }
        let human = line(HUMAN_LINE);
        assert!(human.is_prompt() && !human.is_runtime());
        let m = to_message(&human, "t", "i").unwrap();
        assert_eq!(m.from, None);
        assert_eq!(m.timestamp.as_deref(), Some("2026-10-03T15:33:49.026Z"));
        let task = line(TASK_NOTIFICATION_LINE);
        assert!(task.is_prompt() && task.is_runtime());
        let m = to_message(&task, "t", "i").unwrap();
        assert_eq!(
            (m.role, m.phase, m.from),
            ("user", "other", Some(Caller::RUNTIME))
        );
        // No flags (older transcripts): the leading tag decides.
        let bare = line(
            r#"{"type":"user","uuid":"x","message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}"#,
        );
        assert!(bare.is_runtime());
    }

    /// A local slash command or an isMeta line between the prompt and the reply does not
    /// end the turn.
    #[test]
    fn local_command_and_meta_lines_do_not_end_the_turn() {
        // Prompt → caveat (isMeta) → /model → its stdout → assistant, rewired into one chain.
        let caveat = CAVEAT_LINE.replace(
            r#""parentUuid":null"#,
            r#""parentUuid":"fd2165bd-f040-4bc7-95d7-79241d222fbb""#,
        );
        let assistant = ASSISTANT_LINE.replace(
            "02c63a74-aa3c-435f-8a85-1e873d755883",
            "c95a4fb4-c4c7-40c6-a138-27ff570d2045",
        );
        let text = [
            USER_LINE,
            caveat.as_str(),
            COMMAND_LINE,
            COMMAND_STDOUT_LINE,
            assistant.as_str(),
        ]
        .join("\n");
        let entries = parse_entries(&text);
        let live = live_branch(&entries);
        assert_eq!(live.len(), 5);
        assert_eq!(turn_end(&entries, &live, 0), None);
        let t = transcript_turn(handle(), &entries, &live, 0, TurnEnd::ProcessGone, None);
        assert_eq!(t.final_text.as_deref(), Some("pong"));
    }

    // From ~/.claude/projects/-Users-me-projects/a55180c7-….jsonl: Esc during a TUI turn.
    const INTERRUPT_LINE: &str = r#"{"parentUuid":"27fad0b7-9d51-4afc-871d-d7150f29a6eb","isSidechain":false,"promptId":"23d5e927-4c25-4549-86ee-dcdfa9494f42","type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"uuid":"ae5ff31e-679b-4e1a-934d-3f810a42e1dd","timestamp":"2026-10-03T16:02:24.937Z","interruptedMessageId":"msg_011CffatPWd87URezyHScx6K","session_id":"a55180c7-9408-4e91-ad9f-1ddfa9c152c0","userType":"external","entrypoint":"cli","cwd":"/Users/me/projects","sessionId":"a55180c7-9408-4e91-ad9f-1ddfa9c152c0","version":"2.1.288","gitBranch":"HEAD"}"#;

    /// Prompt → unfinished assistant line → `interrupt`, as one transcript.
    fn interrupted_turn(interrupt: &str) -> Vec<Entry> {
        let assistant = ASSISTANT_LINE
            .replace("\"stop_reason\":\"end_turn\"", "\"stop_reason\":null")
            .replace(
                "ef3a61c5-486b-400b-bce2-11e12cd41331",
                "27fad0b7-9d51-4afc-871d-d7150f29a6eb",
            );
        parse_entries(&[USER_LINE, ATTACHMENT_LINE, assistant.as_str(), interrupt].join("\n"))
    }

    #[test]
    fn esc_interrupt_ends_the_turn() {
        let l = line(INTERRUPT_LINE);
        assert!(l.is_interrupt() && l.is_runtime() && !l.is_prompt());
        let m = to_message(&l, "t", "i").unwrap();
        assert_eq!(
            (m.role, m.phase, m.text.as_str(), m.from),
            ("user", "other", "[interrupted]", Some(Caller::RUNTIME))
        );
        let entries = interrupted_turn(INTERRUPT_LINE);
        let live = live_branch(&entries);
        assert_eq!(live.len(), 4);
        let end = turn_end(&entries, &live, 0);
        assert_eq!(end, Some(TurnEnd::Interrupt(3)));
        let t = transcript_turn(handle(), &entries, &live, 0, end.unwrap(), None);
        assert_eq!(t.status, "interrupted");
    }

    /// Without `interruptedMessageId` the text alone marks the interrupt.
    #[test]
    fn interrupt_detected_from_text_only() {
        let interrupt = INTERRUPT_LINE
            .replace(
                r#""interruptedMessageId":"msg_011CffatPWd87URezyHScx6K","#,
                "",
            )
            .replace(
                "[Request interrupted by user]",
                "[Request interrupted by user for tool use]",
            );
        assert!(!interrupt.contains("interruptedMessageId"));
        let entries = interrupted_turn(&interrupt);
        let live = live_branch(&entries);
        let end = turn_end(&entries, &live, 0);
        assert_eq!(end, Some(TurnEnd::Interrupt(3)));
        let t = transcript_turn(handle(), &entries, &live, 0, end.unwrap(), None);
        assert_eq!(t.status, "interrupted");
    }

    /// A tool_result written after a later prompt still belongs to its own prompt's turn.
    #[test]
    fn tool_result_joins_its_own_prompts_turn() {
        let lines = [
            r#"{"type":"user","uuid":"a1","parentUuid":null,"promptId":"pa","message":{"role":"user","content":"first"}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"sleep 5"}}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"user","uuid":"b1","parentUuid":"a2","promptId":"pb","message":{"role":"user","content":"second"}}"#,
            r#"{"type":"user","uuid":"a3","parentUuid":"a2","promptId":"pa","message":{"role":"user","content":[{"type":"tool_result","content":"done"}]}}"#,
            r#"{"type":"assistant","uuid":"b2","parentUuid":"b1","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"last-prompt","leafUuid":"b2"}"#,
        ];
        let entries = parse_entries(&lines.join("\n"));
        let turns: Vec<(String, String)> = messages(&entries, &live_branch(&entries))
            .into_iter()
            .map(|(_, m)| (m.item_id, m.turn_id))
            .collect();
        let expected = [
            ("a1", "a1"),
            ("a2", "a1"),
            ("b1", "b1"),
            ("a3", "a1"),
            ("b2", "b1"),
        ]
        .map(|(i, t)| (i.to_string(), t.to_string()));
        assert_eq!(turns, expected);
    }

    #[test]
    fn slug_matches_project_dirs() {
        assert_eq!(
            slug("/Users/me/projects/demo/experiments"),
            "-Users-me-projects-demo-experiments"
        );
    }
}
