//! One `agy -p --input-format stream-json --output-format stream-json` child: its command
//! line, its start-up, and what its run log (stdout) says about the turn. Narrow; unknown
//! events are tolerated. Spawning into the run log and reading it are `agents::child`.

use crate::agents::Settings;
use crate::agents::child::Runs;
use crate::agents::decode::or_default;
use crate::model::{
    self, AgentError, Approval, ApprovalOutcome, Error, ErrorCode, Result, TurnStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Child;

/// `agy -p` arguments for one turn. The model and effort pass as given: agent-talk adds no
/// default effort, so agy refuses a model alias without one (`--model gemini-3.8-flash
/// requires --effort`). Nothing here persists across runs (DESIGN.md §6.5), so every turn
/// passes it again.
pub fn args(conversation: Option<&str>, settings: &Settings) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ]
    .map(String::from)
    .to_vec();
    if let Some(m) = &settings.model {
        args.extend(["--model".into(), m.clone()]);
    }
    if let Some(e) = &settings.effort {
        args.extend(["--effort".into(), e.clone()]);
    }
    if settings.full_access {
        args.push("--dangerously-skip-permissions".into());
    }
    if let Some(c) = conversation {
        args.extend(["--conversation".into(), c.into()]);
    }
    args
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Init {
        conversation_id: String,
    },
    StepUpdate {
        step_update: StepUpdate,
    },
    /// Turn end, whatever its payload holds.
    Result {
        #[serde(default)]
        result: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct StepUpdate {
    step_index: i64,
    /// user_input | agent_response | tool | system_message
    step_type: String,
    #[serde(default, deserialize_with = "or_default")]
    text_delta: Option<String>,
}

/// The `result` event's payload. The event ends the turn whatever its fields hold, and the
/// payload passes through as `AgentError.data`, so it stays as agy wrote it.
#[derive(Debug)]
pub struct RunResult(Value);

impl RunResult {
    /// SUCCESS | ERROR
    fn status(&self) -> Option<&str> {
        self.0["status"].as_str()
    }

    fn error(&self) -> Option<&str> {
        self.0["error"].as_str()
    }

    /// Tools headless agy denied (not allow-listed in the user's settings.json); passed
    /// through as `Approval.raw`.
    fn denied_actions(&self) -> &[Value] {
        self.0["denied_actions"]
            .as_array()
            .map_or(&[], Vec::as_slice)
    }
}

/// One run-log line; an error for a line that is not JSON or a known event whose shape
/// does not fit. Unknown events are `Other`.
pub fn decode(line: &str) -> serde_json::Result<Event> {
    serde_json::from_str(line)
}

/// What a run log says about one `agy -p` process (one input line, so one turn).
#[derive(Default)]
pub struct Run {
    pub conversation_id: Option<String>,
    /// Step index of the user step: the turn id.
    user_step: Option<i64>,
    /// Text of each agent_response step, in order (deltas joined).
    replies: Vec<(i64, String)>,
    pub result: Option<RunResult>,
}

impl Run {
    pub fn from_log(path: &Path) -> Run {
        let mut run = Run::default();
        let text = std::fs::read_to_string(path).unwrap_or_default();
        for ev in text.lines().filter_map(|l| decode(l).ok()) {
            run.apply(ev);
        }
        run
    }

    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::Init { conversation_id } => self.conversation_id = Some(conversation_id),
            Event::StepUpdate { step_update: s } => match s.step_type.as_str() {
                "user_input" if self.user_step.is_none() => self.user_step = Some(s.step_index),
                "agent_response" => {
                    let delta = s.text_delta.as_deref().unwrap_or("");
                    match self.replies.last_mut() {
                        Some((i, text)) if *i == s.step_index => text.push_str(delta),
                        _ => self.replies.push((s.step_index, delta.to_string())),
                    }
                }
                _ => {}
            },
            Event::Result { result } => self.result = Some(RunResult(result)),
            Event::Other => {}
        }
    }

    /// The last agent response with text. `result.response` joins every response of the
    /// turn (commentary included), so it is not the reply.
    fn final_text(&self) -> Option<String> {
        self.replies
            .iter()
            .rev()
            .map(|(_, t)| t.trim_end())
            .find(|t| !t.is_empty())
            .map(String::from)
    }

    pub fn turn_id(&self) -> Option<String> {
        self.user_step.map(|s| s.to_string())
    }

    /// The turn, once the run ended with `result`.
    pub fn turn(&self, handle: String) -> Option<model::Turn> {
        let result = self.result.as_ref()?;
        let failed = result.status() != Some("SUCCESS");
        Some(model::Turn {
            handle,
            turn_id: self.turn_id().unwrap_or_default(),
            status: if failed {
                TurnStatus::Failed
            } else {
                TurnStatus::Completed
            },
            error: failed.then(|| json!({"status": result.status(), "error": result.error()})),
            final_text: self.final_text(),
            // `duration_seconds`, `num_turns` and `usage` are conversation totals on a resumed
            // conversation, not this turn's.
            duration_ms: None,
            basis: Some("result event of the agy -p process agent-talk started".into()),
        })
    }

    /// `result.denied_actions` as approvals: the denial ended the turn.
    pub fn denials(&self, handle: &str, receipt_id: &str) -> Vec<Approval> {
        let denied = self.result.iter().flat_map(RunResult::denied_actions);
        denied
            .enumerate()
            .map(|(i, d)| Approval {
                handle: handle.into(),
                turn_id: self.turn_id(),
                item_id: None,
                // Antigravity gives no id: the receipt and the position make it unique and
                // replayable.
                request_id: json!(format!("{receipt_id}/{i}")),
                kind: "denied_action".into(),
                summary: format!(
                    "denied: {} ({})",
                    d["display_name"].as_str().unwrap_or("tool"),
                    d["action"].as_str().unwrap_or("unknown action")
                ),
                outcome: ApprovalOutcome::Denied,
                raw: d.clone(),
            })
            .collect()
    }
}

/// Why a run ended without taking the message: the `result` error, else its stderr or how it
/// exited (`Runs::startup_error`).
pub fn agent_error(
    runs: &Runs,
    receipt_id: &str,
    run: &Run,
    status: Option<ExitStatus>,
) -> AgentError {
    let mut e = runs.startup_error(receipt_id, status);
    if let Some(result) = &run.result {
        if let Some(m) = result.error().filter(|m| !m.is_empty()) {
            e.message = m.to_string();
        }
        e.data = Some(result.0.clone());
    }
    e
}

/// Until the run log shows `init`: the id of the conversation a new run created. agy
/// emits it at startup, before it reads stdin.
pub async fn wait_init(child: &mut Child, runs: &Runs, receipt_id: &str) -> Result<String> {
    loop {
        let exited = child
            .try_wait()
            .map_err(|e| Error::transport("agy process", e))?;
        let run = Run::from_log(&runs.log(receipt_id));
        if let Some(id) = run.conversation_id {
            return Ok(id);
        }
        if let Some(status) = exited {
            return Err(Error::from_agent(
                ErrorCode::Precondition,
                agent_error(runs, receipt_id, &run, Some(status)),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal stream-json lines from the agy 1.2.16 probes (`init.tools` trimmed).
    const INIT: &str = r#"{"event":"init","conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","init":{"model":"gemini-3.8-flash","cwd":"/Users/me/projects/demo","tools":["run_command"],"permission_mode":"always-proceed"}}"#;
    const SLEEP_RUN: &[&str] = &[
        r#"{"event":"step_update","step_update":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","step_index":0,"state":"DONE","step_type":"user_input"}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","step_index":1,"state":"DONE","step_type":"agent_response","duration_seconds":4.61709,"usage":{"input_tokens":12341,"output_tokens":243,"thinking_tokens":130,"cache_read_tokens":0,"total_tokens":12584}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"sleep 25"}}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","step_index":3,"state":"DONE","step_type":"agent_response","text_delta":"I have launched `sleep 25` and will wait for it to finish.\n","duration_seconds":2.951253,"usage":{"input_tokens":12808,"output_tokens":80,"thinking_tokens":63,"cache_read_tokens":0,"total_tokens":12888}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","step_index":4,"state":"DONE","step_type":"system_message","duration_seconds":0.000266}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","step_index":5,"state":"ACTIVE","step_type":"agent_response","text_delta":"DONE1"}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","step_index":5,"state":"DONE","step_type":"agent_response","text_delta":"\n","duration_seconds":1.838562,"usage":{"input_tokens":13136,"output_tokens":53,"thinking_tokens":51,"cache_read_tokens":0,"total_tokens":13189}}}"#,
        r#"{"event":"result","result":{"conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","status":"SUCCESS","response":"I have launched `sleep 25` and will wait for it to finish.\nDONE1\n","duration_seconds":31.833063,"num_turns":1,"usage":{"input_tokens":38285,"output_tokens":376,"thinking_tokens":244,"cache_read_tokens":0,"total_tokens":38661}}}"#,
    ];
    const DENIED_RESULT: &str = r#"{"event":"result","result":{"conversation_id":"d3a52273-f08b-48af-8990-ad267df0433b","status":"SUCCESS","response":"","duration_seconds":4.4268979999999996,"num_turns":1,"usage":{"input_tokens":12337,"output_tokens":161,"thinking_tokens":49,"cache_read_tokens":0,"total_tokens":12498},"denied_actions":[{"action":"command","display_name":"RunCommand"}]}}"#;
    // `--model` without `--effort`: no init, an error result (agy exit 1).
    const MODEL_ERROR: &str = r#"{"event":"result","result":{"conversation_id":"","status":"ERROR","response":"","error":"invalid model selection (--model \"gemini-3.8-flash\" --effort \"\"): --model gemini-3.8-flash requires --effort (available: low, medium, high)"}}"#;

    fn run(lines: &[&str]) -> Run {
        let mut run = Run::default();
        for l in lines {
            run.apply(decode(l).expect("decodes"));
        }
        run
    }

    #[test]
    fn stream_events() {
        let mut lines = vec![INIT];
        lines.extend(SLEEP_RUN);
        let r = run(&lines);
        assert_eq!(
            r.conversation_id.as_deref(),
            Some("06615f44-46a4-4237-8139-581ebfb0c167")
        );
        assert_eq!(r.turn_id().as_deref(), Some("0"));
        let t = r.turn("antigravity:x".into()).unwrap();
        assert_eq!(t.status, TurnStatus::Completed);
        // The last reply, not result.response (which joins the commentary in).
        assert_eq!(t.final_text.as_deref(), Some("DONE1"));
        assert!(r.denials("antigravity:x", "r").is_empty());

        let r = run(&[INIT, SLEEP_RUN[0], DENIED_RESULT]);
        let a = r.denials("antigravity:x", "r");
        assert_eq!(a.len(), 1);
        assert_eq!(
            (a[0].outcome, a[0].summary.as_str(), a[0].turn_id.as_deref()),
            (
                ApprovalOutcome::Denied,
                "denied: RunCommand (command)",
                Some("0")
            )
        );

        let r = run(&[MODEL_ERROR]);
        assert!(r.conversation_id.is_none() && r.turn_id().is_none());
        let t = r.turn("x".into()).unwrap();
        assert_eq!(t.status, TurnStatus::Failed);
        assert_eq!(t.error.unwrap()["status"], "ERROR");

        // No result yet: no turn.
        assert!(run(&[INIT, SLEEP_RUN[0]]).turn("x".into()).is_none());
    }

    /// A result event ends the turn whatever its payload holds: odd fields read as absent,
    /// and a result without a payload is a failed turn, not a turn left running.
    #[test]
    fn any_result_ends_the_turn() {
        let odd = r#"{"event":"result","result":{"status":"SUCCESS","response":7,"denied_actions":null,"duration_seconds":"x"}}"#;
        let r = run(&[INIT, SLEEP_RUN[0], odd]);
        assert_eq!(r.turn("x".into()).unwrap().status, TurnStatus::Completed);
        assert!(r.denials("x", "r").is_empty());

        let t = run(&[INIT, r#"{"event":"result"}"#]).turn("x".into());
        assert_eq!(t.unwrap().status, TurnStatus::Failed);
    }

    #[test]
    fn args_pass_only_what_is_given() {
        assert_eq!(
            args(Some("c"), &Settings::default()),
            [
                "-p",
                "",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--conversation",
                "c"
            ]
        );
        let settings = Settings {
            model: Some("gemini-3.8-flash".into()),
            effort: Some("low".into()),
            full_access: true,
        };
        assert_eq!(
            args(None, &settings)[6..],
            [
                "--model",
                "gemini-3.8-flash",
                "--effort",
                "low",
                "--dangerously-skip-permissions"
            ]
        );
    }
}
