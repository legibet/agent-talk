//! The `claude` CLI as a child process: command line, spawn into a run log, and
//! `claude agents --json --all`. Reading the run log is `providers::tail`.

use super::io_err;
use crate::model::Result;
use crate::providers::{ApprovalPolicy, vendor_cmd};
use serde::Deserialize;
use serde_json::Value;
use std::fs::File;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Child;

/// `claude -p` arguments for one turn; `session` is `--session-id <id>` or
/// `--resume <id>`.
pub fn args(
    session: [&str; 2],
    model: Option<&str>,
    max_turns: Option<u32>,
    policy: ApprovalPolicy,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        session[0],
        session[1],
    ]
    .map(String::from)
    .to_vec();
    if let Some(m) = model {
        args.extend(["--model".into(), m.into()]);
    }
    if let Some(n) = max_turns {
        args.extend(["--max-turns".into(), n.to_string()]);
    }
    match policy {
        // Vendor default: claude denies whatever would prompt, tells the model, and
        // reports system/permission_denied and result.permission_denials.
        ApprovalPolicy::Observe => {}
        // Also tells the model not to retry anything that needs approval.
        ApprovalPolicy::Deny => args.extend(["--permission-prompts".into(), "none".into()]),
    }
    args
}

/// Spawn `claude` with stdout to the run log `log` (stderr next to it), in its own
/// process group so that Ctrl-C on agent-talk stops observing, not the turn.
pub fn spawn(cwd: &str, args: &[String], log: &Path) -> Result<Child> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_err("create run dir", e))?;
    }
    let out = File::create(log).map_err(|e| io_err(&log.display().to_string(), e))?;
    let err = File::create(log.with_extension("stderr"))
        .map_err(|e| io_err(&log.display().to_string(), e))?;
    // A nested `claude -p` with the inherited CLAUDECODE=1 runs normally (observed on
    // claude 2.1.288).
    vendor_cmd("claude")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| io_err("spawn claude", e))
}

/// One entry of `claude agents --json --all`. Fields seen: cwd, id, kind, name, pid,
/// sessionId, startedAt, state, status. `kind: interactive` also covers running
/// `claude -p` processes, so it does not tell a TUI from a headless run.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Background sessions: running, failed, …
    #[serde(default)]
    pub state: Option<String>,
    /// Live processes: busy | idle
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
}

/// `claude agents --json --all`, each entry decoded and raw.
pub async fn agents() -> std::result::Result<Vec<(Agent, Value)>, String> {
    let out = vendor_cmd("claude")
        .args(["agents", "--json", "--all"])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("claude agents: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "claude agents exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let list: Vec<Value> = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("claude agents --json: unexpected output: {e}"))?;
    // Every entry must decode: a dropped one could be the process that is writing the
    // session, and the caller would then proceed as if nothing ran it.
    list.into_iter()
        .map(|raw| {
            Agent::deserialize(&raw)
                .map(|a| (a, raw))
                .map_err(|e| format!("claude agents --json: unexpected entry: {e}"))
        })
        .collect()
}
