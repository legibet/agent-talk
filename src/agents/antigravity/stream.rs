//! stdout of `agy -p --output-format stream-json`, as written to a run log. Narrow;
//! unknown events are tolerated.

use crate::model::{self, Approval};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Init {
        conversation_id: String,
    },
    StepUpdate {
        step_update: StepUpdate,
    },
    /// Turn end; read through the raw value.
    Result {
        result: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct StepUpdate {
    pub step_index: i64,
    /// user_input | agent_response | tool | system_message
    pub step_type: String,
    #[serde(default)]
    text_delta: Option<String>,
}

pub fn decode(line: &str) -> Option<Event> {
    serde_json::from_str(line).ok()
}

/// What a run log says about one `agy -p` process (one input line, so one turn).
#[derive(Default)]
pub struct Run {
    pub conversation_id: Option<String>,
    /// Step index of the user step: the turn id.
    user_step: Option<i64>,
    /// Text of each agent_response step, in order (deltas joined).
    replies: Vec<(i64, String)>,
    pub result: Option<Value>,
}

impl Run {
    pub fn from_log(path: &Path) -> Run {
        let mut run = Run::default();
        let text = std::fs::read_to_string(path).unwrap_or_default();
        for ev in text.lines().filter_map(decode) {
            run.apply(&ev);
        }
        run
    }

    pub fn apply(&mut self, ev: &Event) {
        match ev {
            Event::Init { conversation_id } => {
                self.conversation_id = Some(conversation_id.clone());
            }
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
            Event::Result { result } => self.result = Some(result.clone()),
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
}

/// Turn from the `result` event of an `agy -p` process agent-talk started.
pub fn result_turn(handle: String, run: &Run, result: &Value) -> model::Turn {
    let failed = result["status"].as_str() != Some("SUCCESS");
    model::Turn {
        handle,
        turn_id: run.turn_id().unwrap_or_default(),
        status: if failed { "failed" } else { "completed" },
        error: failed.then(|| json!({"status": result["status"], "error": result["error"]})),
        final_text: run.final_text(),
        // `duration_seconds`, `num_turns` and `usage` are conversation totals on a resumed
        // conversation, not this turn's.
        duration_ms: None,
        basis: Some("result event of the agy -p process agent-talk started".into()),
        raw: result.clone(),
    }
}

/// `result.denied_actions`: tools headless agy denied (not allow-listed in the user's
/// settings.json); the denial ended the turn.
pub fn denials(handle: &str, turn_id: &str, receipt_id: &str, result: &Value) -> Vec<Approval> {
    result["denied_actions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, d)| Approval {
            handle: handle.into(),
            turn_id: Some(turn_id.into()),
            item_id: None,
            // Antigravity gives no id: the receipt and the position make it unique and replayable.
            request_id: json!(format!("{receipt_id}/{i}")),
            kind: "denied_action".into(),
            summary: format!(
                "denied: {} ({})",
                d["display_name"].as_str().unwrap_or("tool"),
                d["action"].as_str().unwrap_or("unknown action")
            ),
            outcome: "denied",
            raw: d.clone(),
        })
        .collect()
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
            run.apply(&decode(l).expect("decodes"));
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
        let result = r.result.as_ref().unwrap();
        let t = result_turn("antigravity:x".into(), &r, result);
        assert_eq!(t.status, "completed");
        // The last reply, not result.response (which joins the commentary in).
        assert_eq!(t.final_text.as_deref(), Some("DONE1"));
        assert!(denials("antigravity:x", "0", "r", result).is_empty());

        let r = run(&[INIT, SLEEP_RUN[0], DENIED_RESULT]);
        let a = denials("antigravity:x", "0", "r", r.result.as_ref().unwrap());
        assert_eq!(a.len(), 1);
        assert_eq!(
            (a[0].outcome, a[0].summary.as_str()),
            ("denied", "denied: RunCommand (command)")
        );

        let r = run(&[MODEL_ERROR]);
        assert!(r.conversation_id.is_none() && r.turn_id().is_none());
        assert_eq!(
            result_turn("x".into(), &r, r.result.as_ref().unwrap()).status,
            "failed"
        );
    }
}
