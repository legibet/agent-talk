//! The `pi --mode json` child: command line, and what the run log (its stdout) says about
//! the turn. Spawning into the run log and reading it are `agents::child`.

use super::session::{self, Body, Entry};
use crate::model::{self, TurnStatus};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::process::ExitStatus;

/// `pi --mode json` arguments for one turn; `session` is `--session-id <id>` for a new
/// session or `--session <path>` to resume one. A resumed session takes the model of its
/// newest `model_change` entry or assistant message, whichever is later, but the thinking
/// level only from a `thinking_level_change` entry, which only the first run writes
/// (DESIGN.md §6.6).
pub fn args(
    [flag, session]: [&str; 2],
    model: Option<&str>,
    effort: Option<&str>,
    name: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = ["--mode", "json", flag, session].map(String::from).to_vec();
    if let Some(m) = model {
        args.extend(["--model".into(), m.into()]);
    }
    if let Some(e) = effort {
        args.extend(["--thinking".into(), e.into()]);
    }
    if let Some(n) = name {
        args.extend(["--name".into(), n.into()]);
    }
    args
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    MessageEnd {
        message: Body,
    },
    AgentSettled,
    #[serde(other)]
    Other,
}

/// What a run log says about one `pi --mode json` process.
#[derive(Default)]
pub struct Run {
    /// The user message pi took: the receipt is accepted at this point, and the persisted
    /// entry is found by its `timestamp` and text.
    pub user: Option<Body>,
    /// The newest assistant message; the turn's status and reply once the run settled.
    pub assistant: Option<Body>,
    /// `agent_settled`: pi has no automatic work left and exits.
    pub settled: bool,
}

impl Run {
    pub fn from_log(path: &Path) -> Run {
        let mut run = Run::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return run;
        };
        for l in text.lines() {
            run.apply(l);
        }
        run
    }

    /// Apply one run-log line; non-JSON and unknown events are ignored.
    pub fn apply(&mut self, line: &str) {
        match serde_json::from_str(line) {
            Ok(Event::MessageEnd { message }) => match message.role.as_str() {
                "user" if self.user.is_none() => self.user = Some(message),
                "assistant" => self.assistant = Some(message),
                _ => {}
            },
            Ok(Event::AgentSettled) => self.settled = true,
            _ => {}
        }
    }

    /// The id of the session entry holding the user message this run took: the entry
    /// with the same `message.timestamp` and text. pi appends the entry before it writes
    /// the event, so the entry is on disk once the event is.
    pub fn turn_id(&self, entries: &[Entry]) -> Option<String> {
        let user = self.user.as_ref()?;
        entries
            .iter()
            .rev()
            .find(|e| {
                e.line.is_prompt()
                    && e.line
                        .message
                        .as_ref()
                        .is_some_and(|m| m.timestamp == user.timestamp && m.text() == user.text())
            })
            .and_then(|e| e.line.id.clone())
    }

    /// Turn from a settled run.
    pub fn turn(&self, handle: String, turn_id: String) -> model::Turn {
        session::reply_turn(
            handle,
            turn_id,
            self.assistant.as_ref(),
            "agent_settled event of the pi process agent-talk started".into(),
        )
    }
}

/// Turn of a run whose process ended without `agent_settled`: `failed` when the exit
/// status says so, otherwise `unknown` (the file may still hold a reply).
pub fn exited(handle: String, turn_id: String, exit: Option<ExitStatus>) -> model::Turn {
    model::Turn {
        handle,
        turn_id,
        status: match exit {
            Some(s) if !s.success() => TurnStatus::Failed,
            _ => TurnStatus::Unknown,
        },
        error: Some(json!({
            "message": "pi exited without an agent_settled event",
            "exit_code": exit.and_then(|s| s.code()),
        })),
        final_text: None,
        duration_ms: None,
        basis: Some("pi process exited".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal events from the probe of 2026-10-07 (pi 1.0.3; usage trimmed).
    const USER: &str = r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly OK"}],"timestamp":1791376687658}}"#;
    const ASSISTANT: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"stdin OK"}],"api":"openai-completions","provider":"deepseek","model":"deepseek-flash","stopReason":"stop","timestamp":1791376687671,"responseId":"71b208cd","rawStopReason":"stop","thinkingLevel":"low"}}"#;
    const ABORTED: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"This operation was aborted","timestamp":1791376687700}}"#;

    #[test]
    fn run_from_events() {
        let mut run = Run::default();
        run.apply(r#"{"type":"session","version":3,"id":"x","cwd":"/tmp"}"#);
        run.apply("not json");
        assert!(run.user.is_none());
        run.apply(USER);
        assert!(run.user.is_some() && !run.settled);
        run.apply(ASSISTANT);
        run.apply(r#"{"type":"agent_settled"}"#);
        assert!(run.settled);
        let t = run.turn("pi:s".into(), "8bd197f0".into());
        assert_eq!(
            (t.status, t.final_text.as_deref()),
            (TurnStatus::Completed, Some("stdin OK"))
        );
        assert!(t.error.is_none());
        run.apply(ABORTED);
        let t = run.turn("pi:s".into(), "8bd197f0".into());
        assert_eq!((t.status, t.final_text), (TurnStatus::Failed, None));
        assert_eq!(t.error.unwrap()["message"], "This operation was aborted");
    }

    /// The run's user message is the prompt entry with the same `message.timestamp` and
    /// text, whatever the content's shape: not a prompt with that text at another time, a
    /// system message or another prompt sharing the timestamp.
    #[test]
    fn turn_id_by_timestamp_and_text() {
        let entries = session::parse(concat!(
            r#"{"type":"message","id":"u0","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly OK"}],"timestamp":1791376000000}}"#,
            "\n",
            r#"{"type":"message","id":"u1","message":{"role":"user","content":"Reply with exactly OK","timestamp":1791376687658}}"#,
            "\n",
            r#"{"type":"message","id":"s1","message":{"role":"system","content":"Reply with exactly OK","timestamp":1791376687658}}"#,
            "\n",
            r#"{"type":"message","id":"u2","message":{"role":"user","content":"Reply with exactly KO","timestamp":1791376687658}}"#,
        ));
        let mut run = Run::default();
        assert_eq!(run.turn_id(&entries), None);
        run.apply(USER);
        assert_eq!(run.turn_id(&entries).as_deref(), Some("u1"));
        assert_eq!(run.turn_id(&entries[..1]), None);
    }

    #[test]
    fn args_pass_only_what_is_given() {
        assert_eq!(
            args(["--session", "/s.jsonl"], None, None, None),
            ["--mode", "json", "--session", "/s.jsonl"]
        );
        assert_eq!(
            args(
                ["--session-id", "id"],
                Some("deepseek/deepseek-flash"),
                Some("low"),
                Some("n")
            ),
            [
                "--mode",
                "json",
                "--session-id",
                "id",
                "--model",
                "deepseek/deepseek-flash",
                "--thinking",
                "low",
                "--name",
                "n"
            ]
        );
    }
}
