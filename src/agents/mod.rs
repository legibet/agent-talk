pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod grok;
pub mod opencode;

use crate::model::{
    Approval, Caller, CallerKind, Error, ErrorCode, Message, Outcome, Page, Receipt, ReceiptState,
    Result, Session, Turn,
};
use crate::store::Store;
use serde::Serialize;
use serde_json::Value;
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::time::Instant;

/// The text delivered for `text` from `from`: agent senders get a one-line provenance
/// header so the receiving model, and a person watching its session, sees who is
/// talking. Not doubled when the text already starts with one (`[from `).
pub fn delivered(from: &Caller, text: &str) -> String {
    match (&from.kind, &from.session) {
        (CallerKind::Agent, Some(h)) if !text.starts_with("[from ") => {
            format!("[from {h} via agent-talk]\n\n{text}")
        }
        _ => text.to_string(),
    }
}

/// `text` without the provenance header `delivered` adds, for previews: the agent's
/// first-prompt preview would otherwise show only the header.
pub fn strip_provenance(text: &str) -> &str {
    match text.strip_prefix("[from ") {
        Some(rest) if rest.contains(" via agent-talk]\n") => {
            text.split_once("\n\n").map_or("", |(_, body)| body)
        }
        _ => text,
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

/// The environment variables naming the session an agent-talk command runs in, with
/// the agent each one belongs to, in the order the CLI derives its sender from them (DESIGN.md §4).
pub const SESSION_VARS: [(&str, &str); 5] = [
    ("CODEX_THREAD_ID", "codex"),
    ("OPENCODE_SESSION_ID", "opencode"),
    ("GROK_SESSION_ID", "grok"),
    ("ANTIGRAVITY_CONVERSATION_ID", "antigravity"),
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

/// Non-blocking exclusive OS lock on a session, held for the duration of the command so
/// two agent-talk commands never write to one session at once (DESIGN.md §3): the file
/// `~/.agent-talk/locks/<agent>-<id>`, opened with O_CLOEXEC so an agent child does
/// not inherit it (a run that outlives the command is tracked by pid). `why` says what a
/// second writer would break.
pub fn lock(handle: &str, why: &str) -> Result<File> {
    let io_err =
        |what: &str, e: std::io::Error| Error::new(ErrorCode::Transport, format!("{what}: {e}"));
    let dir = std::env::home_dir()
        .unwrap_or_default()
        .join(".agent-talk/locks");
    std::fs::create_dir_all(&dir).map_err(|e| io_err("create lock dir", e))?;
    let path = dir.join(handle.replacen(':', "-", 1));
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| io_err(&path.display().to_string(), e))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(TryLockError::WouldBlock) => Err(Error::new(
            ErrorCode::Locked,
            format!(
                "another agent-talk command holds {handle} ({}); {why}",
                path.display()
            ),
        )),
        Err(TryLockError::Error(e)) => Err(io_err(&path.display().to_string(), e)),
    }
}

/// The first non-blank line of `s`, trimmed and cut to `n` characters.
pub fn first_line(s: &str, n: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > n {
        format!("{}…", line.chars().take(n).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Feed each line of a child's run log (its stdout, redirected to `log`) to `on_line` as it
/// is written, until the process exits (Claude and Antigravity runs).
pub async fn tail(
    child: &mut Child,
    log: &Path,
    mut on_line: impl FnMut(&str) -> Result<()>,
) -> Result<ExitStatus> {
    let file = tokio::fs::File::open(log)
        .await
        .map_err(|e| Error::new(ErrorCode::Transport, format!("{}: {e}", log.display())))?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = String::new();
    let mut exited = None;
    loop {
        let n = reader
            .read_line(&mut buf)
            .await
            .map_err(|e| Error::new(ErrorCode::Transport, format!("read run log: {e}")))?;
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
            .map_err(|e| Error::new(ErrorCode::Transport, format!("child process: {e}")))?
        {
            // Read once more: output written just before exit.
            Some(status) => exited = Some(status),
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// How agent-talk treats approval requests that reach it while it observes a turn
/// (DESIGN.md §4). Every subscribed client receives them, and any answer resolves them for
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Never answer; record the request and report it as pending. Sessions agent-talk did
    /// not start: the user answers in their own client.
    Observe,
    /// Decline, never accept: Codex answers `decline`, OpenCode `reject` with a message,
    /// Claude gets `--permission-prompts none`, Grok `reject_once`; Antigravity has nothing
    /// to answer. Sessions agent-talk started, where nobody else would answer.
    Deny,
}

/// The adapters, dispatched by agent name.
pub enum Adapter<'a> {
    Codex(codex::Codex<'a>),
    Claude(claude::Claude<'a>),
    OpenCode(opencode::OpenCode<'a>),
    Grok(grok::Grok<'a>),
    Antigravity(antigravity::Antigravity<'a>),
}

impl<'a> Adapter<'a> {
    pub fn all(store: &'a Store) -> [Adapter<'a>; 5] {
        [
            Adapter::Codex(codex::Codex::new(store)),
            Adapter::Claude(claude::Claude::new(store)),
            Adapter::OpenCode(opencode::OpenCode::new(store)),
            Adapter::Grok(grok::Grok::new(store)),
            Adapter::Antigravity(antigravity::Antigravity::new(store)),
        ]
    }

    pub fn named(store: &'a Store, name: &str) -> Result<Adapter<'a>> {
        Adapter::all(store)
            .into_iter()
            .find(|a| a.name() == name)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Unsupported,
                    format!(
                        "agent {name} is not supported (codex, claude, opencode, grok, antigravity)"
                    ),
                )
            })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Adapter::Codex(_) => "codex",
            Adapter::Claude(_) => "claude",
            Adapter::OpenCode(_) => "opencode",
            Adapter::Grok(_) => "grok",
            Adapter::Antigravity(_) => "antigravity",
        }
    }
}

impl Agent for Adapter<'_> {
    async fn caps(&self) -> Caps {
        match self {
            Adapter::Codex(p) => p.caps().await,
            Adapter::Claude(p) => p.caps().await,
            Adapter::OpenCode(p) => p.caps().await,
            Adapter::Grok(p) => p.caps().await,
            Adapter::Antigravity(p) => p.caps().await,
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        match self {
            Adapter::Codex(p) => p.list(filter, limit, cursor).await,
            Adapter::Claude(p) => p.list(filter, limit, cursor).await,
            Adapter::OpenCode(p) => p.list(filter, limit, cursor).await,
            Adapter::Grok(p) => p.list(filter, limit, cursor).await,
            Adapter::Antigravity(p) => p.list(filter, limit, cursor).await,
        }
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        match self {
            Adapter::Codex(p) => p.start(req, deadline).await,
            Adapter::Claude(p) => p.start(req, deadline).await,
            Adapter::OpenCode(p) => p.start(req, deadline).await,
            Adapter::Grok(p) => p.start(req, deadline).await,
            Adapter::Antigravity(p) => p.start(req, deadline).await,
        }
    }

    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        match self {
            Adapter::Codex(p) => p.send(id, req, deadline).await,
            Adapter::Claude(p) => p.send(id, req, deadline).await,
            Adapter::OpenCode(p) => p.send(id, req, deadline).await,
            Adapter::Grok(p) => p.send(id, req, deadline).await,
            Adapter::Antigravity(p) => p.send(id, req, deadline).await,
        }
    }

    async fn read(&self, id: &str, range: ReadRange) -> Result<ReadPage> {
        match self {
            Adapter::Codex(p) => p.read(id, range).await,
            Adapter::Claude(p) => p.read(id, range).await,
            Adapter::OpenCode(p) => p.read(id, range).await,
            Adapter::Grok(p) => p.read(id, range).await,
            Adapter::Antigravity(p) => p.read(id, range).await,
        }
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        match self {
            Adapter::Codex(p) => p.wait(id, target, deadline).await,
            Adapter::Claude(p) => p.wait(id, target, deadline).await,
            Adapter::OpenCode(p) => p.wait(id, target, deadline).await,
            Adapter::Grok(p) => p.wait(id, target, deadline).await,
            Adapter::Antigravity(p) => p.wait(id, target, deadline).await,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Queue,
    Steer,
}

pub enum WaitTarget {
    Turn(String),
    Receipt(String),
}

pub struct ListFilter<'a> {
    pub cwd: Option<&'a str>,
    /// Include sub-sessions too: Codex lists every source kind (sub-agents, unknown), not
    /// only interactive and exec; OpenCode drops its `parentID=null` filter.
    pub all: bool,
}

pub struct StartRequest<'a> {
    pub cwd: &'a str,
    /// The caller's text, as recorded in the intent.
    pub prompt: &'a str,
    /// What reaches the agent: `prompt`, with a provenance header for agent senders.
    pub delivered: &'a str,
    pub model: Option<&'a str>,
    /// Title the agent stores for the session (Codex `thread/name/set`, Claude `--name`,
    /// OpenCode `title`, Grok `_x.ai/session/rename`; Antigravity has none and refuses it);
    /// `ls` shows it as `name`.
    pub name: Option<&'a str>,
    /// Reasoning effort (Codex `turn/start.effort`, Grok `--reasoning-effort`, Antigravity
    /// `--effort`; the values are the model's).
    pub effort: Option<&'a str>,
    /// Every permission, no approval prompts (`new --full-access`, DESIGN.md §4).
    pub full_access: bool,
    pub from: &'a Caller,
    /// Hop depth of the first prompt (`Store::hop_depth`).
    pub depth: u32,
}

pub struct SendRequest<'a> {
    /// The caller's text, as recorded in the intent.
    pub text: &'a str,
    /// What reaches the agent: `text`, with a provenance header for agent senders.
    pub delivered: &'a str,
    pub mode: Mode,
    pub from: &'a Caller,
    pub reply_to: Option<&'a str>,
    /// Hop depth of this message (`Store::hop_depth`).
    pub depth: u32,
    /// Steer only: the active turn id the caller expects; defaults to the newest turn.
    pub expect_turn: Option<&'a str>,
}

/// Which part of the history `read` returns.
pub enum ReadRange {
    /// Oldest first after the cursor. Codex counts turns; Claude, Grok and Antigravity
    /// count messages; OpenCode counts its message rows. The cursor is the agent's
    /// opaque one for Codex and OpenCode, a transcript line uuid for Claude, an
    /// `updates.jsonl` line number for Grok and a transcript line position for Antigravity.
    Forward { since: Option<String>, limit: u32 },
    /// The newest N messages, oldest first, no cursor.
    Tail(u32),
}

pub struct ReadPage {
    pub messages: Page<Message>,
    /// The agent's raw records covering the same span (Codex turns, Claude and Antigravity
    /// transcript lines, Grok `updates.jsonl` lines, OpenCode message rows), for `--raw`.
    pub raw: Vec<Value>,
}

/// One agent's entry in `caps`: the installed version, the state of the shared process
/// agent-talk joins, and which operations can run now (DESIGN.md §2).
#[derive(Serialize)]
pub struct Caps {
    pub agent: &'static str,
    /// The agent CLI's `--version` output (OpenCode: the running service's version).
    pub version: Option<String>,
    /// The shared daemon, service or leader, for the agents that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
    pub operations: Vec<Operation>,
}

/// Whether one operation can run, and why not.
#[derive(Serialize)]
pub struct Operation {
    pub name: &'static str,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What an operation depends on: `Err` carries the reason it cannot run.
pub type Check = std::result::Result<(), String>;

impl Operation {
    pub fn new(name: &'static str, check: &Check) -> Self {
        Operation {
            name,
            available: check.is_ok(),
            reason: check.clone().err(),
        }
    }
}

/// One adapter per agent. Mutations persist their intent before submitting and
/// return a receipt; a `deadline` makes them observe the resulting turn on the
/// same connection.
pub trait Agent {
    async fn caps(&self) -> Caps;
    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>>;
    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome>;
    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome>;
    async fn read(&self, id: &str, range: ReadRange) -> Result<ReadPage>;
    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome>;
}

// Receipt and approval lifecycle shared by every adapter. The semantics are agent-talk's
// (DESIGN.md §4), not an agent's, so they live here and the adapters only supply agent plumbing.

/// Deny on sessions agent-talk started, observe on the others.
pub fn approval_policy(store: &Store, handle: &str) -> Result<ApprovalPolicy> {
    Ok(if store.is_owned(handle)? {
        ApprovalPolicy::Deny
    } else {
        ApprovalPolicy::Observe
    })
}

/// Whether `new --full-access` started this session. Codex loses the sandbox when a thread
/// unloads, and Claude and Antigravity take permissions per process, so later sends apply it
/// again (DESIGN.md §4).
pub fn full_access(store: &Store, handle: &str) -> Result<bool> {
    Ok(store
        .owned_args(handle)?
        .is_some_and(|a| a["full_access"] == true))
}

/// The submission failed. An agent response is a definitive refusal: `rejected`. A lost
/// outcome (connection lost, HTTP 5xx, undecodable reply, no response in time, Ctrl-C)
/// is `unknown`, to be recovered with `wait --receipt`.
pub fn reject(store: &Store, receipt_id: &str, e: Error) -> Error {
    let state = match e.code {
        ErrorCode::Transport | ErrorCode::Timeout | ErrorCode::Interrupted => ReceiptState::Unknown,
        _ => ReceiptState::Rejected,
    };
    if let Err(store_err) = store.settle_unaccepted(receipt_id, state, e.agent_error.as_deref()) {
        tracing::warn!("{store_err}");
    }
    e
}

pub fn record(store: &Store, a: &Approval) {
    if let Err(e) = store.insert_approval(a) {
        tracing::warn!("{e}");
    }
}

/// Another client answered the request: pending approvals with this id are resolved.
pub fn resolve(store: &Store, approvals: &mut [Approval], request_id: &Value) {
    for a in approvals
        .iter_mut()
        .filter(|a| a.request_id == *request_id && a.outcome == "pending")
    {
        a.outcome = "resolved";
        record(store, a);
    }
}

/// The receipt a `wait --receipt` targets: known, on this handle, not rejected.
pub fn wait_receipt(store: &Store, handle: &str, receipt_id: &str) -> Result<Receipt> {
    let rec = store.receipt(receipt_id)?.ok_or_else(|| {
        Error::new(
            ErrorCode::Precondition,
            format!("unknown receipt {receipt_id}"),
        )
    })?;
    if rec.handle != handle {
        return Err(Error::new(
            ErrorCode::Precondition,
            format!("receipt {receipt_id} belongs to {}", rec.handle),
        ));
    }
    if rec.state == ReceiptState::Rejected {
        let mut e = Error::new(ErrorCode::Precondition, "the intent was rejected");
        e.agent_error = rec.agent_error.clone().map(Box::new);
        e.receipt = Some(Box::new(rec));
        return Err(e);
    }
    Ok(rec)
}

/// Settle an observed turn. The receipt is reloaded; when the outcome is unknown
/// (deadline, Ctrl-C, transport loss) a still-pending receipt becomes `unknown` and the
/// error reports `progress` (`waiting` while an approval is unanswered). Approvals seen
/// during the observation travel with the outcome either way.
pub fn settle(
    store: &Store,
    handle: String,
    res: Result<Option<Turn>>,
    receipt_id: Option<&str>,
    approvals: Vec<Approval>,
    progress: &'static str,
) -> Result<Outcome> {
    let unknown_outcome = matches!(
        &res,
        Err(e) if matches!(e.code, ErrorCode::Timeout | ErrorCode::Interrupted | ErrorCode::Transport)
    );
    if unknown_outcome && let Some(r) = receipt_id {
        store.settle_unaccepted(r, ReceiptState::Unknown, None)?;
    }
    let receipt = match receipt_id {
        Some(r) => store.receipt(r)?,
        None => None,
    };
    match res {
        Ok(turn) => Ok(Outcome {
            handle,
            receipt,
            turn,
            approvals,
            from: None,
        }),
        Err(mut e) => {
            e.receipt = e.receipt.or(receipt.map(Box::new));
            if unknown_outcome {
                e.state = Some(if approvals.iter().any(|a| a.outcome == "pending") {
                    "waiting"
                } else {
                    progress
                });
            }
            e.approvals = approvals;
            Err(e)
        }
    }
}

/// Run `fut` until it finishes, the deadline passes, or Ctrl-C arrives. Without a
/// deadline only Ctrl-C ends it early.
pub async fn bounded<T>(
    deadline: Option<Instant>,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    let until = async {
        match deadline {
            Some(d) => tokio::time::sleep_until(d).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        r = fut => r,
        _ = until => Err(Error::new(
            ErrorCode::Timeout,
            "deadline passed before the turn completed; outcome unknown, receipt retained",
        )),
        _ = tokio::signal::ctrl_c() => Err(Error::new(
            ErrorCode::Interrupted,
            "interrupted; outcome unknown, receipt retained",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentError, Caller};
    use crate::store::NewIntent;

    #[test]
    fn provenance_header_only_for_agents() {
        let agent = Caller::agent("codex:01a1029c-17b8-75c1-9e19-0fc6d58b0512");
        let text = delivered(&agent, "what is the word?");
        assert_eq!(
            text,
            "[from codex:01a1029c-17b8-75c1-9e19-0fc6d58b0512 via agent-talk]\n\nwhat is the word?"
        );
        assert_eq!(strip_provenance(&text), "what is the word?");
        // A header the model wrote itself is not doubled.
        let own = "[from codex:x via agent-talk]\n\nhi";
        assert_eq!(delivered(&agent, own), own);
        // People and unknown senders: unchanged, and a text that merely starts with
        // "[from " is not a header.
        assert_eq!(delivered(&Caller::UNKNOWN, "hi"), "hi");
        assert_eq!(
            strip_provenance("[from the top] again"),
            "[from the top] again"
        );
    }

    fn pending(store: &Store, id: &str) {
        store
            .insert_intent(&NewIntent {
                receipt_id: id,
                handle: "codex:t",
                client_msg_id: &format!("c-{id}"),
                text: "x",
                from: &Caller::UNKNOWN,
                reply_to: None,
                depth: 0,
                delivered_text: None,
            })
            .unwrap();
    }

    fn state(store: &Store, id: &str) -> ReceiptState {
        store.receipt(id).unwrap().unwrap().state
    }

    /// A lost outcome keeps the intent recoverable, only an agent refusal is final, and
    /// neither overwrites an acceptance another command already recorded.
    #[test]
    fn submission_failures_settle_by_certainty() {
        let s = Store::memory();
        pending(&s, "lost");
        reject(&s, "lost", Error::new(ErrorCode::Timeout, "no response"));
        assert_eq!(state(&s, "lost"), ReceiptState::Unknown);
        // Later proof of delivery (history, queue, inbox, run log).
        s.accept("lost", None, Some("turn-1"), None).unwrap();
        assert_eq!(state(&s, "lost"), ReceiptState::Accepted);

        pending(&s, "refused");
        let agent_error = AgentError {
            code: -32600,
            message: "stale turn".into(),
            data: None,
        };
        reject(
            &s,
            "refused",
            Error::from_agent(ErrorCode::Precondition, agent_error),
        );
        assert_eq!(state(&s, "refused"), ReceiptState::Rejected);

        pending(&s, "raced");
        s.accept("raced", Some("q1"), None, None).unwrap();
        s.settle_unaccepted("raced", ReceiptState::Unknown, None)
            .unwrap();
        assert_eq!(state(&s, "raced"), ReceiptState::Accepted);
    }
}
