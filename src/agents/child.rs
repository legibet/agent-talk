//! Agent CLIs as children of agent-talk, and the local files around them: the session
//! lock and the run log a child writes.

use crate::model::{AgentError, Error, ErrorCode, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
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

/// The children of an agent CLI that runs one child per mutation (Claude, Antigravity, pi).
/// Each child writes its stdout to the run log `~/.agent-talk/<agent>-runs/<receipt>.ndjson`,
/// which the command tails and later commands read to recover the receipt, and its stderr
/// to `<receipt>.stderr` next to it (DESIGN.md §3).
pub struct Runs {
    program: &'static str,
    /// `~/.agent-talk/<agent>-runs`
    dir: PathBuf,
}

impl Runs {
    pub fn new(program: &'static str, agent: &str) -> Runs {
        let home = std::env::home_dir().unwrap_or_default();
        Runs {
            program,
            dir: home.join(".agent-talk").join(format!("{agent}-runs")),
        }
    }

    /// The run log of the child spawned for `receipt_id`.
    pub fn log(&self, receipt_id: &str) -> PathBuf {
        self.dir.join(format!("{receipt_id}.ndjson"))
    }

    /// Spawn `cmd` (an `agent_cmd` of the program, arguments set) in `cwd` for `receipt_id`:
    /// stdout to its run log, stderr next to it, stdin piped for the message, and in its own
    /// process group so that Ctrl-C on agent-talk stops observing, not the turn.
    pub fn spawn(&self, cmd: &mut Command, cwd: &str, receipt_id: &str) -> Result<Child> {
        let log = self.log(receipt_id);
        let stderr = log.with_extension("stderr");
        std::fs::create_dir_all(&self.dir).map_err(|e| Error::transport("create run dir", e))?;
        let out = File::create(&log).map_err(|e| Error::transport(log.display(), e))?;
        let err = File::create(&stderr).map_err(|e| Error::transport(stderr.display(), e))?;
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(out)
            .stderr(err)
            .process_group(0)
            .spawn()
            .map_err(|e| Error::transport(format!("spawn {}", self.program), e))
    }

    /// Write `message` to the child's stdin and close it: Claude and agy exit after the turn
    /// it starts, and pi reads stdin to EOF as its prompt.
    pub async fn deliver(&self, child: &mut Child, message: &[u8]) -> Result<()> {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin
            .write_all(message)
            .await
            .map_err(|e| Error::transport(format!("write the message to {}", self.program), e))?;
        // Dropping the handle closes the pipe; `shutdown` does not (a no-op on tokio 1.53.2).
        drop(stdin);
        Ok(())
    }

    /// Why the child of `receipt_id` exited before taking the message: its stderr, or how it
    /// exited when it wrote none. `status` is `None` when this command did not see the exit.
    pub fn startup_error(&self, receipt_id: &str, status: Option<ExitStatus>) -> AgentError {
        let stderr = std::fs::read_to_string(self.log(receipt_id).with_extension("stderr"))
            .unwrap_or_default();
        let exited = format!("{} exited before taking the message", self.program);
        AgentError {
            code: status.and_then(|s| s.code()).unwrap_or(-1).into(),
            message: match (stderr.trim(), status) {
                ("", Some(s)) => format!("{exited} ({s})"),
                ("", None) => exited,
                (stderr, _) => stderr.to_string(),
            },
            data: None,
        }
    }

    /// Of the children recorded for a session (`Store::processes`), the pid of one still
    /// running its turn: alive, and its run log not `ended` yet.
    pub fn running(&self, procs: &[(String, u32)], ended: impl Fn(&Path) -> bool) -> Option<u32> {
        procs
            .iter()
            .find(|(receipt_id, pid)| pid_alive(*pid) && !ended(&self.log(receipt_id)))
            .map(|(_, pid)| *pid)
    }
}

/// Feed each line of a child's run log (its stdout, redirected to `log`) to `on_line` as it
/// is written; `on_line` says whether the child has taken the message (Claude, Antigravity
/// and pi runs). Returns the exit status once the process exits, or `None` as soon as the
/// message is taken when `until_accepted`: the run then goes on without this command,
/// tracked by its pid (DESIGN.md §3).
pub async fn tail(
    child: &mut Child,
    log: &Path,
    until_accepted: bool,
    mut on_line: impl FnMut(&str) -> Result<bool>,
) -> Result<Option<ExitStatus>> {
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
                let accepted = !buf.trim().is_empty() && on_line(buf.trim())?;
                buf.clear();
                if accepted && until_accepted {
                    return Ok(None);
                }
            }
            continue;
        }
        // End of what has been written so far.
        if let Some(status) = exited {
            if !buf.trim().is_empty() {
                on_line(buf.trim())?;
            }
            return Ok(Some(status));
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
