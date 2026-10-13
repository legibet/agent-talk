//! stdout of `claude -p --output-format stream-json`, as written to a run log. Narrow;
//! unknown event types and subtypes are tolerated.

use super::transcript::tool_summary;
use crate::model::{self, Approval, ApprovalOutcome, TurnStatus};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    System(System),
    Result(ResultEvent),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum System {
    Init {
        session_id: Option<String>,
    },
    /// Claude's own rules denied a tool that would have prompted; the model is told and
    /// nothing is pending (DESIGN.md §6.2).
    PermissionDenied {
        tool_name: Option<String>,
        tool_use_id: Option<String>,
        message: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// The `result` event: the turn ended.
#[derive(Debug, Deserialize)]
pub struct ResultEvent {
    /// `success`, or the error kind (`error_max_turns`, …).
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    /// The final reply, or the error message.
    result: Option<String>,
    duration_ms: Option<i64>,
    // Passed through into the turn's error as claude wrote them.
    #[serde(default)]
    api_error_status: Value,
    #[serde(default)]
    terminal_reason: Value,
    /// Tools claude denied during the turn. Each entry is passed through as an
    /// approval's raw record.
    #[serde(default)]
    permission_denials: Vec<Value>,
}

/// One run-log line; an error for a line that is not JSON or not a known shape.
pub fn decode(line: &str) -> serde_json::Result<StreamEvent> {
    serde_json::from_str(line)
}

/// What a run log says about one `claude -p` process.
#[derive(Default)]
pub struct Run {
    pub init: bool,
    pub result: Option<ResultEvent>,
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

    pub fn apply(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::System(System::Init { .. }) => self.init = true,
            StreamEvent::Result(r) => self.result = Some(r),
            _ => {}
        }
    }

    /// Claude took the input: the run reported `init`, or a `result` without one.
    pub fn started(&self) -> bool {
        self.init || self.result.is_some()
    }
}

impl ResultEvent {
    /// The turn this event ends, of a `claude -p` process agent-talk started.
    pub fn turn(&self, handle: String, turn_id: String) -> model::Turn {
        let failed = self.subtype.as_deref() != Some("success") || self.is_error;
        model::Turn {
            handle,
            turn_id,
            status: if failed {
                TurnStatus::Failed
            } else {
                TurnStatus::Completed
            },
            error: failed.then(|| {
                json!({
                    "subtype": self.subtype,
                    "message": self.result,
                    "api_error_status": self.api_error_status,
                    "terminal_reason": self.terminal_reason,
                })
            }),
            final_text: self.result.clone(),
            duration_ms: self.duration_ms,
            basis: Some("result event of the claude -p process agent-talk started".into()),
        }
    }

    /// `permission_denials`, as approvals of the turn.
    pub fn denials(&self, handle: &str, turn_id: &str) -> Vec<Approval> {
        self.permission_denials
            .iter()
            .map(|d| Approval {
                handle: handle.into(),
                turn_id: Some(turn_id.into()),
                item_id: d["tool_use_id"].as_str().map(String::from),
                request_id: d["tool_use_id"].clone(),
                kind: "permission_denial".into(),
                summary: format!(
                    "denied: {}",
                    tool_summary(d["tool_name"].as_str().unwrap_or("tool"), &d["tool_input"])
                ),
                outcome: ApprovalOutcome::Denied,
                raw: d.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal samples from the `claude -p` probes of 2026-10-03 (usage and long fields trimmed)
    // and the permissions probe (claude 2.1.288).

    const RESULT: &str = r#"{"duration_api_ms":1622,"stop_reason":"end_turn","session_id":"37648f74-4df1-44ca-8ba1-bbe20ba3f8ff","total_cost_usd":0.079788,"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","is_error":false,"num_turns":1,"subtype":"success","api_error_status":null,"result":"pong","ttft_ms":2646,"type":"result","duration_ms":2667,"uuid":"4de890e9-3a91-440d-ad58-903d147953f5","queued_turn_count":0,"result_index":0}"#;

    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/Users/me/projects/demo","session_id":"37648f74-4df1-44ca-8ba1-bbe20ba3f8ff","tools":["Task","Bash"],"mcp_servers":[],"model":"claude-sonnet-5-5","permissionMode":"default"}"#;

    const PERMISSION_DENIED: &str = r#"{"type": "system", "subtype": "permission_denied", "tool_name": "Bash", "tool_use_id": "toolu_01BxEqv4rpsNSRPRaFGnwrQR", "message": "touch in '/Users/me/projects/demo/claude-probes/approval-test-a.txt' needs approval.", "uuid": "143bd9ef-e98d-4bc4-97f6-4b7faa4e96a4", "session_id": "297f263c-340e-496b-9698-f010b8d4aee7"}"#;

    #[test]
    fn stream_events() {
        let mut run = Run::default();
        run.apply(decode(INIT).unwrap());
        assert!(run.init);
        run.apply(decode(RESULT).unwrap());
        let r = run.result.as_ref().unwrap();
        let t = r.turn("claude:s".into(), "fd2165bd".into());
        assert_eq!(t.status, TurnStatus::Completed);
        assert_eq!(t.final_text.as_deref(), Some("pong"));
        assert!(t.error.is_none());
        let failed = RESULT.replace(r#""subtype":"success""#, r#""subtype":"error_max_turns""#);
        let StreamEvent::Result(failed) = decode(&failed).unwrap() else {
            panic!("a result event");
        };
        assert_eq!(
            failed.turn("x".into(), "y".into()).status,
            TurnStatus::Failed
        );
        assert!(matches!(
            decode(PERMISSION_DENIED).unwrap(),
            StreamEvent::System(System::PermissionDenied { tool_use_id: Some(ref id), .. })
                if id == "toolu_01BxEqv4rpsNSRPRaFGnwrQR"
        ));
    }
}
