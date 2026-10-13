//! Agent CLIs as children of agent-talk, and the local files around them: the session
//! lock and the run log a child writes.

use crate::model::{Error, ErrorCode, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};

/// The environment variables naming the session an agent-talk command runs in, with
/// the agent each one belongs to, in the order the CLI derives its sender from them (DESIGN.md §4).
pub const SESSION_VARS: [(&str, &str); 6] = [
    ("CODEX_THREAD_ID", "codex"),
    ("OPENCODE_SESSION_ID", "opencode"),
    ("GROK_SESSION_ID", "grok"),
    ("ANTIGRAVITY_CONVERSATION_ID", "antigravity"),
    ("PI_SESSION_ID", "pi"),
    ("CLAUDE_CODE_SESSION_ID", "claude"),
];

/// An agent CLI as a child of agent-talk. The environment is passed through except
/// `AGENT_TALK_CALLER` and the session variables: the agent would pass them on to its own
/// shell commands and MCP servers, attributing the child session's messages to the
/// caller of this command. Each agent sets its own variable for its children itself.
pub fn agent_cmd(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_remove("AGENT_TALK_CALLER");
    for (var, _) in SESSION_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// `<program> --version`, or why it cannot run.
pub async fn cli_version(program: &str) -> std::result::Result<String, String> {
    match agent_cmd(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().into()),
        Ok(o) => Err(format!(
            "`{program} --version` failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("`{program}` is not on PATH"))
        }
        Err(e) => Err(format!("cannot run `{program}`: {e}")),
    }
}

/// Whether a process with this pid exists (a pid we may not signal counts as alive).
pub fn pid_alive(pid: u32) -> bool {
    let Some(p) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match rustix::process::test_kill_process(p) {
        Ok(()) => true,
        Err(e) => e == rustix::io::Errno::PERM,
    }
}

/// Non-blocking exclusive OS lock on a session, held for the duration of the command so
/// two agent-talk commands never write to one session at once (DESIGN.md §3): the file
/// `~/.agent-talk/locks/<agent>-<id>`, opened with O_CLOEXEC so an agent child does
/// not inherit it (a run that outlives the command is tracked by pid). `why` says what a
/// second writer would break.
pub fn lock(handle: &str, why: &str) -> Result<File> {
    let dir = std::env::home_dir()
        .unwrap_or_default()
        .join(".agent-talk/locks");
    std::fs::create_dir_all(&dir).map_err(|e| Error::transport("create lock dir", e))?;
    let path = dir.join(handle.replacen(':', "-", 1));
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Error::transport(path.display(), e))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(TryLockError::WouldBlock) => Err(Error::new(
            ErrorCode::Locked,
            format!(
                "another agent-talk command holds {handle} ({}); {why}",
                path.display()
            ),
        )),
        Err(TryLockError::Error(e)) => Err(Error::transport(path.display(), e)),
    }
}

/// Feed each line of a child's run log (its stdout, redirected to `log`) to `on_line` as it
/// is written, until the process exits (Claude, Antigravity and pi runs).
pub async fn tail(
    child: &mut Child,
    log: &Path,
    mut on_line: impl FnMut(&str) -> Result<()>,
) -> Result<ExitStatus> {
    let file = tokio::fs::File::open(log)
        .await
        .map_err(|e| Error::transport(log.display(), e))?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = String::new();
    let mut exited = None;
    loop {
        let n = reader
            .read_line(&mut buf)
            .await
            .map_err(|e| Error::transport("read run log", e))?;
        if n > 0 {
            // A partial line is completed by the next read, unless the process is gone.
            if buf.ends_with('\n') || exited.is_some() {
                if !buf.trim().is_empty() {
                    on_line(buf.trim())?;
                }
                buf.clear();
            }
            continue;
        }
        // End of what has been written so far.
        if let Some(status) = exited {
            if !buf.trim().is_empty() {
                on_line(buf.trim())?;
            }
            return Ok(status);
        }
        match child
            .try_wait()
            .map_err(|e| Error::transport("child process", e))?
        {
            // Read once more: output written just before exit.
            Some(status) => exited = Some(status),
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}
