//! The `pi --mode json` child: command line, spawn into a run log, and what the run log
//! (its stdout) says about the turn. Reading the log as it is written is `agents::tail`.

use super::io_err;
use super::session::{self, Body};
use crate::agents::agent_cmd;
use crate::model::{self, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::File;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use tokio::process::Child;

/// `pi --mode json` arguments for one turn; `session` is `--session-id <id>` for a new
/// session or `--session <path>` to resume one. Model, thinking level and name are stored
/// in the session file by the first run, so only `new` passes them (DESIGN.md §6.6).
pub fn args(
    session: [&str; 2],
    model: Option<&str>,
    effort: Option<&str>,
    name: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = ["--mode", "json", session[0], session[1]]
        .map(String::from)
        .to_vec();
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

/// Spawn `pi` with stdout to the run log `log` (stderr next to it), in its own process
/// group so that Ctrl-C on agent-talk stops observing, not the turn. The prompt goes to
/// stdin: pi reads piped stdin to EOF as the prompt, which keeps `@file` and option
/// parsing away from the text.
pub fn spawn(cwd: &str, args: &[String], log: &Path) -> Result<Child> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_err("create run dir", e))?;
    }
    let out = File::create(log).map_err(|e| io_err(&log.display().to_string(), e))?;
    let err = File::create(log.with_extension("stderr"))
        .map_err(|e| io_err(&log.display().to_string(), e))?;
    agent_cmd("pi")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| io_err("spawn pi", e))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    MessageEnd {
        message: Value,
    },
    AgentSettled,
    #[serde(other)]
    Other,
}

/// What a run log says about one `pi --mode json` process.
#[derive(Default)]
pub struct Run {
    /// The user message pi took, as the event carried it: the receipt is accepted at this
    /// point, and the persisted entry is found by its `timestamp` and text.
    pub user: Option<Value>,
    /// The newest assistant message; the turn's status and reply once the run settled.
    pub assistant: Option<Value>,
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
        let Ok(raw) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match Event::deserialize(&raw) {
            Ok(Event::MessageEnd { message }) => match message["role"].as_str() {
                Some("user") if self.user.is_none() => self.user = Some(message),
                Some("assistant") => self.assistant = Some(message),
                _ => {}
            },
            Ok(Event::AgentSettled) => self.settled = true,
            _ => {}
        }
    }

    /// The id of the session entry holding the user message this run took: the entry
    /// with the same `message.timestamp` and text. pi appends the entry before it writes
    /// the event, so the entry is on disk once the event is.
    pub fn turn_id(&self, session_file: &Path) -> Option<String> {
        let user = Body::deserialize(self.user.as_ref()?).ok()?;
        let entries = session::load(session_file).ok()?;
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

    /// Turn from a settled run. No duration: pi stamps an assistant message when its
    /// request starts, not when the reply ends (DESIGN.md §6.6).
    pub fn turn(&self, handle: String, turn_id: String) -> model::Turn {
        let body = self
            .assistant
            .as_ref()
            .and_then(|a| Body::deserialize(a).ok());
        let stop = body.as_ref().and_then(|m| m.stop_reason.as_deref());
        let status = session::status(stop);
        model::Turn {
            handle,
            turn_id,
            status,
            error: (status != "completed").then(|| {
                json!({
                    "stop_reason": stop,
                    "message": body.as_ref().and_then(|m| m.error_message.clone()),
                })
            }),
            final_text: body.as_ref().map(Body::text).filter(|t| !t.is_empty()),
            duration_ms: None,
            basis: Some("agent_settled event of the pi process agent-talk started".into()),
        }
    }
}

/// Turn of a run whose process ended without `agent_settled`: `failed` when the exit
/// status says so, otherwise `unknown` (the file may still hold a reply).
pub fn exited(handle: String, turn_id: String, exit: Option<ExitStatus>) -> model::Turn {
    model::Turn {
        handle,
        turn_id,
        status: match exit {
            Some(s) if !s.success() => "failed",
            _ => "unknown",
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
            ("completed", Some("stdin OK"))
        );
        assert!(t.error.is_none());
        run.apply(ABORTED);
        let t = run.turn("pi:s".into(), "8bd197f0".into());
        assert_eq!((t.status, t.final_text), ("failed", None));
        assert_eq!(t.error.unwrap()["message"], "This operation was aborted");
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
