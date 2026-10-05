//! stdout of `claude -p --output-format stream-json`, as written to a run log. Narrow;
//! unknown event types and subtypes are tolerated.

use super::transcript::tool_summary;
use crate::model::{self, Approval};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    System(System),
    /// Turn end; read through the raw value (`result_turn`).
    Result {},
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum System {
    Init {
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Claude's own rules denied a tool that would have prompted; the model is told and
    /// nothing is pending (DESIGN.md §6.2).
    PermissionDenied {
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        tool_use_id: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// What a run log says about one `claude -p` process.
#[derive(Default)]
pub struct Run {
    pub init: bool,
    pub result: Option<Value>,
}

impl Run {
    pub fn from_log(path: &Path) -> Run {
        let mut run = Run::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return run;
        };
        for l in text.lines() {
            if let Ok(raw) = serde_json::from_str::<Value>(l)
                && let Ok(ev) = StreamEvent::deserialize(&raw)
            {
                run.apply(&ev, &raw);
            }
        }
        run
    }

    pub fn apply(&mut self, ev: &StreamEvent, raw: &Value) {
        match ev {
            StreamEvent::System(System::Init { .. }) => self.init = true,
            StreamEvent::Result {} => self.result = Some(raw.clone()),
            _ => {}
        }
    }
}

/// Turn from the `result` event of a `claude -p` process agent-talk started.
pub fn result_turn(handle: String, turn_id: String, ev: &Value) -> model::Turn {
    let subtype = ev["subtype"].as_str().unwrap_or("");
    let failed = subtype != "success" || ev["is_error"].as_bool() == Some(true);
    model::Turn {
        handle,
        turn_id,
        status: if failed { "failed" } else { "completed" },
        error: failed.then(|| {
            json!({
                "subtype": ev["subtype"],
                "message": ev["result"],
                "api_error_status": ev["api_error_status"],
                "terminal_reason": ev["terminal_reason"],
            })
        }),
        final_text: ev["result"].as_str().map(String::from),
        duration_ms: ev["duration_ms"].as_i64(),
        basis: Some("result event of the claude -p process agent-talk started".into()),
        raw: ev.clone(),
    }
}

/// One entry of `result.permission_denials`.
pub fn denial(handle: String, turn_id: String, d: &Value) -> Approval {
    let tool = d["tool_name"].as_str().unwrap_or("tool");
    Approval {
        handle,
        turn_id: Some(turn_id),
        item_id: d["tool_use_id"].as_str().map(String::from),
        request_id: d["tool_use_id"].clone(),
        kind: "permission_denial".into(),
        summary: format!("denied: {}", tool_summary(tool, &d["tool_input"])),
        outcome: "denied",
        raw: d.clone(),
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

    fn decode(s: &str) -> (StreamEvent, Value) {
        let raw: Value = serde_json::from_str(s).unwrap();
        (StreamEvent::deserialize(&raw).unwrap(), raw)
    }

    #[test]
    fn stream_events() {
        let mut run = Run::default();
        let (ev, raw) = decode(INIT);
        run.apply(&ev, &raw);
        assert!(run.init);
        let (ev, raw) = decode(RESULT);
        run.apply(&ev, &raw);
        assert!(run.result.is_some());
        let t = result_turn("claude:s".into(), "fd2165bd".into(), &raw);
        assert_eq!(t.status, "completed");
        assert_eq!(t.final_text.as_deref(), Some("pong"));
        assert!(t.error.is_none());
        let mut failed = raw.clone();
        failed["subtype"] = json!("error_max_turns");
        assert_eq!(
            result_turn("x".into(), "y".into(), &failed).status,
            "failed"
        );
        let (ev, _) = decode(PERMISSION_DENIED);
        assert!(matches!(
            ev,
            StreamEvent::System(System::PermissionDenied { tool_use_id: Some(ref id), .. })
                if id == "toolu_01BxEqv4rpsNSRPRaFGnwrQR"
        ));
    }
}
