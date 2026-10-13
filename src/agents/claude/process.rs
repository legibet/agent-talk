//! The `claude` CLI as a child process: command line and `claude agents --json --all`.
//! Spawning into a run log and reading it are `agents::child`.

use crate::agents::child::agent_cmd;
use crate::agents::lifecycle::ApprovalPolicy;
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;

/// What one `claude -p` turn runs with.
pub struct TurnArgs<'a> {
    /// `--session-id <id>` (a new session) or `--resume <id>`.
    pub session: [&'a str; 2],
    /// `--name`, on a new session.
    pub name: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub full_access: bool,
    pub policy: ApprovalPolicy,
}

/// `claude -p` for one turn. `--resume` restores the model from the transcript but neither the
/// effort nor the permission mode (DESIGN.md §6.2), so those pass again on every turn.
pub fn command(a: &TurnArgs) -> Command {
    // A nested `claude -p` with the inherited CLAUDECODE=1 runs normally (observed on
    // claude 2.1.288).
    let mut cmd = agent_cmd("claude");
    cmd.args([
        "-p",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ])
    .args(a.session);
    if let Some(m) = a.model {
        cmd.args(["--model", m]);
    }
    if let Some(e) = a.effort {
        // CLAUDE_CODE_EFFORT_LEVEL silently overrides --effort (claude 2.1.291, DESIGN.md §6.2).
        cmd.args(["--effort", e])
            .env_remove("CLAUDE_CODE_EFFORT_LEVEL");
    }
    if a.full_access {
        cmd.args(["--permission-mode", "bypassPermissions"]);
    }
    match a.policy {
        // Claude's default: it denies whatever would prompt, tells the model, and
        // reports system/permission_denied and result.permission_denials.
        ApprovalPolicy::Observe => {}
        // Also tells the model not to retry anything that needs approval.
        ApprovalPolicy::Deny => {
            cmd.args(["--permission-prompts", "none"]);
        }
    }
    if let Some(n) = a.name {
        cmd.args(["--name", n]);
    }
    cmd
}

/// One entry of `claude agents --json --all`. Fields seen: cwd, id, kind, name, pid,
/// sessionId, startedAt, state, status. `kind: interactive` also covers running
/// `claude -p` processes, so it does not tell a TUI from a headless run.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub kind: Option<String>,
    /// Background sessions: running, failed, …
    pub state: Option<String>,
    /// Live processes: busy | idle
    pub status: Option<String>,
    pub pid: Option<u32>,
    pub name: Option<String>,
    pub started_at: Option<i64>,
}

/// `claude agents --json --all`, each entry decoded.
pub async fn agents() -> std::result::Result<Vec<AgentEntry>, String> {
    let out = agent_cmd("claude")
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
    // Every entry must decode: a dropped one could be the process that is writing the
    // session, and the caller would then proceed as if nothing ran it.
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("claude agents --json: unexpected output: {e}"))
}
