//! The `agy` CLI as a child process: command line, the run's start-up, and what a run that
//! never took the message reports. Spawning into a run log and reading it are `agents::child`.

use super::stream::Run;
use crate::agents::child::Runs;
use crate::model::{AgentError, Error, ErrorCode, Result};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Child;

/// `agy -p` arguments for one turn. `--model` without `--effort` is refused by agy
/// for its model aliases (`--model gemini-3.8-flash requires --effort`), so both pass through
/// as given. Nothing here persists across runs (DESIGN.md §6.5), so every turn passes it again.
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

/// Why a run ended without taking the message: the `result` error, else its stderr or how it
/// exited (`Runs::startup_error`).
pub fn agent_error(
    runs: &Runs,
    receipt_id: &str,
    run: &Run,
    status: Option<ExitStatus>,
) -> AgentError {
    let mut e = runs.startup_error(receipt_id, status);
    if let Some(m) = run.result.as_ref().and_then(|r| r["error"].as_str())
        && !m.is_empty()
    {
        e.message = m.to_string();
    }
    e.data = run.result.clone();
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
