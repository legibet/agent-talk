//! The `agy` CLI as a child process: command line, spawn, the run's start-up, and what a
//! run that never took the message reports.

use super::io_err;
use super::stream::Run;
use crate::agents::agent_cmd;
use crate::model::{AgentError, Error, ErrorCode, Result};
use std::fs::File;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Child;

/// `agy -p` arguments for one turn. `--model` without `--effort` is refused by agy
/// for its model aliases (`--model gemini-3.8-flash requires --effort`), so both pass through
/// as given. Nothing here persists across runs, so every turn passes it again.
pub fn args(
    conversation: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    full_access: bool,
) -> Vec<String> {
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
    if let Some(m) = model {
        args.extend(["--model".into(), m.into()]);
    }
    if let Some(e) = effort {
        args.extend(["--effort".into(), e.into()]);
    }
    if full_access {
        args.push("--dangerously-skip-permissions".into());
    }
    if let Some(c) = conversation {
        args.extend(["--conversation".into(), c.into()]);
    }
    args
}

/// Spawn `agy` with stdout to the run log `log` (stderr next to it), in its own process
/// group so that Ctrl-C on agent-talk stops observing, not the turn.
pub fn spawn(cwd: &str, args: &[String], log: &Path) -> Result<Child> {
    let runs = log.parent().expect("run log has a directory");
    std::fs::create_dir_all(runs).map_err(|e| io_err("create run dir", e))?;
    let out = File::create(log).map_err(|e| io_err(&log.display().to_string(), e))?;
    let err = File::create(log.with_extension("stderr"))
        .map_err(|e| io_err(&log.display().to_string(), e))?;
    agent_cmd("agy")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| io_err("spawn agy", e))
}

/// Why a run ended without taking the message: the `result` error or stderr.
pub fn agent_error(log: &Path, run: &Run, status: Option<i32>) -> AgentError {
    let stderr = std::fs::read_to_string(log.with_extension("stderr"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let message = match run.result.as_ref().and_then(|r| r["error"].as_str()) {
        Some(e) if !e.is_empty() => e.to_string(),
        _ if !stderr.is_empty() => stderr,
        _ => format!("agy exited ({status:?}) before taking the message"),
    };
    AgentError {
        code: status.unwrap_or(-1).into(),
        message,
        data: run.result.clone(),
    }
}

/// Until the run log shows `init`: the id of the conversation a new run created. agy
/// emits it at startup, before it reads stdin.
pub async fn wait_init(child: &mut Child, log: &Path) -> Result<String> {
    loop {
        let exited = child.try_wait().map_err(|e| io_err("agy process", e))?;
        let run = Run::from_log(log);
        if let Some(id) = run.conversation_id {
            return Ok(id);
        }
        if let Some(status) = exited {
            return Err(Error::from_agent(
                ErrorCode::Precondition,
                agent_error(log, &run, status.code()),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
