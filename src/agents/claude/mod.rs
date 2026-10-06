//! Claude Code adapter.
//!
//! Each mutation runs one `claude -p --input-format stream-json` process (`new` with
//! `--session-id`, `send` with `--resume`) and writes one user line carrying the
//! agent-talk's uuid, which becomes the transcript `user.uuid` and so the turn id. The
//! process's stdout goes to a run log, `~/.agent-talk/claude-runs/<receipt>.ndjson`,
//! which this command tails; the process keeps running if the command stops observing
//! (deadline, Ctrl-C), and its pid is recorded so later commands know the run is
//! still alive. History, turn boundaries and listings come from the transcripts under
//! `~/.claude/projects` and from `claude agents --json --all`.
//!
//! Approvals: agent-talk never passes `--permission-prompt-tool stdio`, so claude never
//! asks the host (`control_request`); it denies on its own and the denials are surfaced.
//! An answerer for that stdio prompt protocol is an open item (DESIGN.md §8).

mod process;
mod stream;
mod transcript;

use self::process::{AgentEntry, agents};
use self::stream::{Run, StreamEvent, System, denial, result_turn};
use self::transcript::{TurnEnd, head, live_branch, load, slug, transcript_turn, turn_end};
use super::{
    Agent, AgentStatus, ApprovalPolicy, Check, ListFilter, Operation, ReadPage, ReadQuery,
    SendRequest, StartRequest, WaitTarget, approval_policy, bounded, cli_version, cut, first_line,
    lock, pid_alive, record, settle, tail, wait_receipt,
};
use crate::model::{
    self, AgentError, Approval, Error, ErrorCode, Model, Observations, Outcome, Page, ReceiptState,
    Result, Session,
};
use crate::store::{NewIntent, Store};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

/// Why agent-talk refuses a second writer (DESIGN.md §6.2).
const SECOND_WRITER: &str = "a concurrent --resume would fork the transcript and silently drop one branch of the conversation";

/// Claude Code has no listing interface; `--model` takes an alias for the latest model of a
/// family (`claude --help`, plus `haiku`, accepted on 2.1.291) and `--effort` one of these
/// levels. The one model list agent-talk carries itself (DESIGN.md §5).
const MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub struct Claude<'a> {
    store: &'a Store,
    /// `~/.claude/projects`
    projects: PathBuf,
    /// `~/.agent-talk/claude-runs`
    runs: PathBuf,
}

fn handle(id: &str) -> String {
    format!("claude:{id}")
}

/// Session ids are UUIDs; anything else is refused before it reaches a path.
fn check_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map(|_| ()).map_err(|_| {
        Error::new(
            ErrorCode::Precondition,
            format!("invalid Claude session id {id}; expected a UUID"),
        )
    })
}

fn io_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Transport, format!("{what}: {e}"))
}

/// Who, besides this command, is running the session right now.
enum Live {
    /// A claude process agent-talk spawned (by an earlier, possibly finished, command).
    Own(u32),
    /// A process listed by `claude agents --json` that agent-talk did not spawn.
    Foreign(AgentEntry),
}

/// What this command observes of the `claude -p` turn it started.
struct RunWatch {
    id: String,
    receipt_id: String,
    /// The input line's uuid, which becomes the transcript `user.uuid`.
    turn_id: String,
    run: Run,
    approvals: Vec<Approval>,
}

impl RunWatch {
    /// Apply one run-log line: accept the receipt when the run starts and record
    /// the agent's permission denials.
    fn on_event(&mut self, store: &Store, line: &str) -> Result<()> {
        let Ok(raw) = serde_json::from_str::<Value>(line) else {
            tracing::warn!("non-JSON line from claude: {line}");
            return Ok(());
        };
        let Ok(ev) = StreamEvent::deserialize(&raw) else {
            return Ok(());
        };
        // Accepted on the first init, or on a result without init: either means claude
        // took the input.
        let unaccepted = !self.run.init && self.run.result.is_none();
        match &ev {
            StreamEvent::System(System::Init { session_id }) if !self.run.init => {
                if session_id.as_deref() != Some(self.id.as_str()) {
                    tracing::warn!(
                        "system/init reports session {session_id:?}, expected {}",
                        self.id
                    );
                }
            }
            StreamEvent::System(System::PermissionDenied {
                tool_name,
                tool_use_id,
                message,
            }) => {
                let a = Approval {
                    handle: handle(&self.id),
                    turn_id: Some(self.turn_id.clone()),
                    item_id: tool_use_id.clone(),
                    request_id: json!(tool_use_id),
                    kind: "system/permission_denied".into(),
                    summary: match (tool_name, message) {
                        (Some(t), Some(m)) => format!("{t}: {}", first_line(m, 160)),
                        (Some(t), None) => t.clone(),
                        _ => "permission-related event".into(),
                    },
                    outcome: "denied",
                    raw: raw.clone(),
                };
                record(store, &a);
                self.approvals.push(a);
            }
            _ => {}
        }
        self.run.apply(&ev, &raw);
        if unaccepted && (self.run.init || self.run.result.is_some()) {
            store.accept(
                &self.receipt_id,
                None,
                Some(&self.turn_id),
                Some(&self.turn_id),
            )?;
        }
        Ok(())
    }
}

impl<'a> Claude<'a> {
    pub fn new(store: &'a Store) -> Self {
        let home = std::env::home_dir().unwrap_or_default();
        let data = home.join(".agent-talk");
        Claude {
            store,
            projects: home.join(".claude/projects"),
            runs: data.join("claude-runs"),
        }
    }

    fn log_path(&self, receipt_id: &str) -> PathBuf {
        self.runs.join(format!("{receipt_id}.ndjson"))
    }

    /// Of the claude processes recorded for the session (`Store::processes`), the pid of one
    /// still running its turn: alive, and its run log has no `result` yet.
    fn own_run(&self, procs: &[(String, u32)]) -> Option<u32> {
        procs
            .iter()
            .find(|(receipt_id, pid)| {
                pid_alive(*pid) && Run::from_log(&self.log_path(receipt_id)).result.is_none()
            })
            .map(|(_, pid)| *pid)
    }

    /// Any process running this session, classified by agent-talk's pid records,
    /// never by `kind`.
    async fn live(&self, id: &str) -> Result<Option<Live>> {
        let procs = self.store.processes(&handle(id))?;
        if let Some(pid) = self.own_run(&procs) {
            return Ok(Some(Live::Own(pid)));
        }
        let agents = agents().await.map_err(|e| {
            Error::new(
                ErrorCode::CapMissing,
                format!("cannot check whether the session is live: {e}"),
            )
        })?;
        Ok(agents
            .into_iter()
            .find(|a| a.session_id.as_deref() == Some(id) && a.pid.is_some())
            .map(|a| {
                let pid = a.pid.and_then(|p| u32::try_from(p).ok()).unwrap_or(0);
                if procs.iter().any(|(_, p)| *p == pid) {
                    Live::Own(pid)
                } else {
                    Live::Foreign(a)
                }
            }))
    }

    /// Refuse when anything else runs the session: the agent has no lock and a
    /// second writer forks the transcript.
    async fn refuse_live(&self, id: &str) -> Result<()> {
        match self.live(id).await? {
            None => Ok(()),
            Some(Live::Own(pid)) => Err(Error::new(
                ErrorCode::Locked,
                format!(
                    "a claude process started by another agent-talk command (pid {pid}) is still running {}; {SECOND_WRITER}",
                    handle(id)
                ),
            )),
            Some(Live::Foreign(a)) => Err(Error::new(
                ErrorCode::ForeignLive,
                format!(
                    "{} is live in claude process pid {} (status {}), which agent-talk did not start; {SECOND_WRITER}, so agent-talk only reads this session",
                    handle(id),
                    a.pid.unwrap_or_default(),
                    a.status.as_deref().unwrap_or("unknown")
                ),
            )),
        }
    }

    /// Transcript by session id across every project folder: resume keeps the
    /// original folder, a fork writes under the forking cwd's folder.
    fn transcript(&self, id: &str) -> Option<PathBuf> {
        let file = format!("{id}.jsonl");
        std::fs::read_dir(&self.projects)
            .ok()?
            .flatten()
            .map(|d| d.path().join(&file))
            .find(|p| p.is_file())
    }

    fn require_transcript(&self, id: &str) -> Result<PathBuf> {
        self.transcript(id).ok_or_else(|| {
            Error::new(
                ErrorCode::Precondition,
                format!(
                    "no transcript for {} under {}",
                    handle(id),
                    self.projects.display()
                ),
            )
        })
    }

    /// Run one `claude -p` turn to completion (or until `deadline` / Ctrl-C, after
    /// which the process keeps running and is tracked by its pid), then settle the receipt.
    /// The caller holds the session lock.
    async fn execute(
        &self,
        cwd: &str,
        args: Vec<String>,
        mut watch: RunWatch,
        prompt: &str,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let h = handle(&watch.id);
        let log = self.log_path(&watch.receipt_id);
        let mut child = match process::spawn(cwd, &args, &log) {
            Ok(c) => c,
            Err(e) => {
                self.store
                    .settle_unaccepted(&watch.receipt_id, ReceiptState::Rejected, None)?;
                return Err(e);
            }
        };
        if let Some(pid) = child.id() {
            self.store.insert_process(&watch.receipt_id, &h, pid)?;
        }
        // claude 2.1.288 honors the input uuid as the transcript user.uuid, so the turn
        // id is known before the run.
        let line = json!({
            "type": "user",
            "uuid": watch.turn_id,
            "message": {"role": "user", "content": prompt},
        })
        .to_string();
        let work = async {
            let mut stdin = child.stdin.take().expect("stdin is piped");
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| io_err("write prompt to claude", e))?;
            // Closing stdin makes claude exit after this turn's result.
            stdin
                .shutdown()
                .await
                .map_err(|e| io_err("close claude stdin", e))?;
            drop(stdin);
            let status = tail(&mut child, &log, |l| watch.on_event(self.store, l)).await?;
            if !watch.run.init && watch.run.result.is_none() {
                let stderr = std::fs::read_to_string(log.with_extension("stderr"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let agent_error = AgentError {
                    code: status.code().unwrap_or(-1).into(),
                    message: if stderr.is_empty() {
                        format!("claude exited with {status} before starting the session")
                    } else {
                        stderr
                    },
                    data: None,
                };
                self.store.settle_unaccepted(
                    &watch.receipt_id,
                    ReceiptState::Rejected,
                    Some(&agent_error),
                )?;
                return Err(Error::from_agent(ErrorCode::Precondition, agent_error));
            }
            let Some(r) = &watch.run.result else {
                return Ok(model::Turn {
                    handle: h.clone(),
                    turn_id: watch.turn_id.clone(),
                    status: if status.success() {
                        "unknown"
                    } else {
                        "failed"
                    },
                    error: Some(json!({
                        "message": "claude exited without a result event",
                        "exit_code": status.code(),
                    })),
                    final_text: None,
                    duration_ms: None,
                    basis: Some("claude -p process exited".into()),
                });
            };
            for d in r["permission_denials"].as_array().into_iter().flatten() {
                let a = denial(h.clone(), watch.turn_id.clone(), d);
                // A denial already reported live (`system/permission_denied`) stays as recorded.
                if a.item_id.is_some() && watch.approvals.iter().any(|x| x.item_id == a.item_id) {
                    continue;
                }
                record(self.store, &a);
                watch.approvals.push(a);
            }
            Ok(result_turn(h.clone(), watch.turn_id.clone(), r))
        };
        // Without --wait the turn still runs to completion (synchronous sends);
        // only Ctrl-C stops observing early.
        let res = bounded(deadline, work).await;
        // Without a deadline the turn is not reported even though it ran to completion.
        let res = res.map(|turn| deadline.is_some().then_some(turn));
        settle(
            self.store,
            h,
            res,
            Some(&watch.receipt_id),
            watch.approvals,
            "running",
        )
    }
}

impl Agent for Claude<'_> {
    async fn models(&self) -> Result<Vec<Model>> {
        Ok(MODELS
            .iter()
            .map(|id| Model {
                id: (*id).into(),
                efforts: EFFORTS.map(String::from).into(),
            })
            .collect())
    }

    async fn status(&self) -> AgentStatus {
        let version = cli_version("claude").await;
        let cli: Check = version.as_ref().map(|_| ()).map_err(String::clone);
        // `send` refuses unless `claude agents` can tell whether the session is open elsewhere.
        let agents: Check = match agents().await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("cannot check whether the session is live: {e}")),
        };
        let transcripts: Check = if self.projects.is_dir() {
            Ok(())
        } else {
            Err(format!("{} does not exist", self.projects.display()))
        };
        AgentStatus {
            agent: "claude",
            version: version.ok(),
            shared: None,
            operations: vec![
                Operation::new("ls", &transcripts),
                Operation::new("new", &cli),
                Operation::new("send", &cli.clone().and(agents)),
                Operation::new("read", &transcripts),
                Operation::new("wait", &Ok(())),
                Operation::new(
                    "steer",
                    &Err("Claude Code cannot add a message to a running turn".into()),
                ),
                Operation::new("name", &cli),
            ],
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        struct Row {
            id: String,
            agent: Option<AgentEntry>,
            file: Option<PathBuf>,
            recency_ms: i64,
        }
        let (agents, agents_ok) = match agents().await {
            Ok(a) => (a, true),
            Err(e) => {
                tracing::warn!("{e}");
                (Vec::new(), false)
            }
        };
        let want_slug = filter.cwd.map(slug);
        let mut rows: HashMap<String, Row> = HashMap::new();
        for dir in std::fs::read_dir(&self.projects)
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(s) = &want_slug
                && dir.file_name().to_string_lossy() != s.as_str()
            {
                continue;
            }
            for f in std::fs::read_dir(dir.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if uuid::Uuid::parse_str(id).is_err() {
                    continue;
                }
                let mtime = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                rows.insert(
                    id.to_string(),
                    Row {
                        id: id.to_string(),
                        agent: None,
                        file: Some(path),
                        recency_ms: mtime,
                    },
                );
            }
        }
        for a in agents {
            let Some(id) = a.session_id.clone() else {
                continue;
            };
            let started = a.started_at.unwrap_or(0);
            match rows.get_mut(&id) {
                Some(row) => {
                    row.recency_ms = row.recency_ms.max(started);
                    row.agent = Some(a);
                }
                None => {
                    if filter.cwd.is_some() && a.cwd.as_deref() != filter.cwd {
                        continue;
                    }
                    rows.insert(
                        id.clone(),
                        Row {
                            id,
                            agent: Some(a),
                            file: None,
                            recency_ms: started,
                        },
                    );
                }
            }
        }
        let mut rows: Vec<Row> = rows.into_values().collect();
        rows.sort_by(|a, b| b.recency_ms.cmp(&a.recency_ms).then(a.id.cmp(&b.id)));
        let offset: usize = match cursor {
            Some(c) => c
                .parse()
                .map_err(|_| Error::new(ErrorCode::Precondition, format!("invalid cursor {c}")))?,
            None => 0,
        };
        let end = (offset + limit as usize).min(rows.len());
        let next_cursor = (end < rows.len()).then(|| end.to_string());
        let mut items = Vec::new();
        for row in rows.into_iter().skip(offset).take(limit as usize) {
            let h = handle(&row.id);
            let owned = self.store.is_owned(&h)?;
            let (t_cwd, name, preview, entrypoint) = match &row.file {
                Some(p) => head(p),
                None => (None, None, None, None),
            };
            let agent = row.agent.as_ref();
            let live_pid = agent
                .and_then(|a| a.pid)
                .and_then(|p| u32::try_from(p).ok());
            let procs = self.store.processes(&h)?;
            let own_pid = self.own_run(&procs);
            let ours = live_pid.is_some_and(|l| procs.iter().any(|(_, p)| *p == l));
            let origin = if owned || ours {
                "agent-talk".to_string()
            } else {
                match agent.and_then(|a| a.kind.as_deref()) {
                    // `interactive` covers TUI sessions and running claude -p processes alike.
                    Some(k) => format!("claude-{k}"),
                    None => match entrypoint.as_deref() {
                        Some(e) => format!("claude-{e}"),
                        None => "claude".into(),
                    },
                }
            };
            let status = agent.and_then(|a| a.status.as_deref().or(a.state.as_deref()));
            let state = if own_pid.is_some() {
                "running"
            } else if live_pid.is_some() {
                match status {
                    Some("busy" | "running") => "running",
                    Some("idle") => "idle",
                    Some(s) if s.contains("wait") || s.contains("permission") => "waiting",
                    _ => "unknown",
                }
            } else {
                "unknown"
            };
            items.push(Session {
                handle: h,
                agent: "claude",
                cwd: agent.and_then(|a| a.cwd.clone()).or(t_cwd),
                name: agent.and_then(|a| a.name.clone()).or(name),
                preview,
                observations: Observations {
                    history: if row.file.is_some() {
                        "visible"
                    } else {
                        "none"
                    },
                    loaded: if live_pid.is_some() || own_pid.is_some() {
                        "yes"
                    } else if agents_ok {
                        "no"
                    } else {
                        "unknown"
                    },
                    origin,
                },
                state,
                owned,
                id: row.id,
            });
        }
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        let id = uuid::Uuid::new_v4().to_string();
        let h = handle(&id);
        let _lock = lock(&h, SECOND_WRITER)?;
        let stored_args = json!({
            "model": req.model,
            "effort": req.effort,
            "full_access": req.full_access,
        });
        self.store.insert_owned(&h, req.cwd, &stored_args)?;
        let receipt_id = uuid::Uuid::new_v4().to_string();
        // The input line's uuid: becomes the transcript user.uuid, i.e. the turn id.
        let client_msg_id = uuid::Uuid::new_v4().to_string();
        self.store.insert_intent(&NewIntent {
            receipt_id: &receipt_id,
            handle: &h,
            client_msg_id: &client_msg_id,
            text: req.prompt,
            delivered_text: (req.delivered != req.prompt).then_some(req.delivered),
            from: req.from,
            depth: req.depth,
        })?;
        let mut args = process::args(
            ["--session-id", &id],
            req.model,
            req.effort,
            ApprovalPolicy::Deny,
            req.full_access,
        );
        if let Some(n) = req.name {
            args.extend(["--name".into(), n.into()]);
        }
        let watch = RunWatch {
            id,
            receipt_id,
            turn_id: client_msg_id,
            run: Run::default(),
            approvals: Vec::new(),
        };
        self.execute(req.cwd, args, watch, req.delivered, deadline)
            .await
    }

    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        check_id(id)?;
        if req.steer {
            return Err(Error::new(
                ErrorCode::NoSteer,
                "Claude sessions cannot be steered (agent-talk keeps no long-lived claude process); send without --steer",
            ));
        }
        let h = handle(id);
        let path = self.require_transcript(id)?;
        let _lock = lock(&h, SECOND_WRITER)?;
        self.refuse_live(id).await?;
        // Resuming from another directory runs the turn there: the transcript stays in
        // its original project folder and only the new lines carry the new cwd
        // (observed on claude 2.1.288). Never pick one silently.
        let cwd = match self.store.owned_cwd(&h)? {
            Some(c) => c,
            None => match head(&path).0 {
                Some(c) if Path::new(&c).is_dir() => c,
                Some(c) => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!(
                            "the session's directory {c} no longer exists; claude -p --resume would run it elsewhere"
                        ),
                    ));
                }
                None => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        "the transcript records no working directory for this session",
                    ));
                }
            },
        };
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let client_msg_id = uuid::Uuid::new_v4().to_string();
        self.store.insert_intent(&NewIntent {
            receipt_id: &receipt_id,
            handle: &h,
            client_msg_id: &client_msg_id,
            text: req.text,
            delivered_text: (req.delivered != req.text).then_some(req.delivered),
            from: req.from,
            depth: req.depth,
        })?;
        // A session agent-talk started keeps the model, effort and permissions `new` gave it.
        let started = self.store.owned_args(&h)?.unwrap_or_default();
        let args = process::args(
            ["--resume", id],
            started["model"].as_str(),
            started["effort"].as_str(),
            approval_policy(self.store, &h)?,
            started["full_access"] == true,
        );
        let watch = RunWatch {
            id: id.to_string(),
            receipt_id,
            turn_id: client_msg_id,
            run: Run::default(),
            approvals: Vec::new(),
        };
        self.execute(&cwd, args, watch, req.delivered, deadline)
            .await
    }

    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage> {
        check_id(id)?;
        let entries = load(&self.require_transcript(id)?)?;
        let branch = live_branch(&entries);
        // (line index, message), oldest first.
        let mut messages = transcript::messages(&entries, &branch);
        for (i, m) in &mut messages {
            if m.from.is_none() && entries[*i].is_prompt() {
                m.from = self.store.sender_of(&m.item_id)?;
            }
        }
        let cut = cut(q, messages, entries.len(), false)?;
        Ok(ReadPage {
            messages: Page {
                next_cursor: cut.cursor_item().map(|(_, m)| m.item_id.clone()),
                items: cut.messages.into_iter().map(|(_, m)| m).collect(),
            },
            // Every line in the span, orphaned branches and sub-agent lines included.
            raw: entries[cut.records].iter().map(|e| e.raw.clone()).collect(),
        })
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        check_id(id)?;
        let h = handle(id);
        let (turn_id, receipt) = match target {
            WaitTarget::Turn(t) => (t.clone(), self.store.receipt_by_turn(&h, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &h, r)?;
                // The input uuid is the turn id, recorded when the intent was written.
                (
                    rec.turn_id.clone().unwrap_or(rec.client_msg_id.clone()),
                    Some(rec),
                )
            }
        };
        let path = self.require_transcript(id)?;
        // The sending command stopped observing before its run started: accept the
        // receipt here, once, when the run log shows the start.
        let mut unaccepted = receipt.as_ref().is_some_and(|r| {
            matches!(r.state, ReceiptState::Pending | ReceiptState::Unknown) && r.turn_id.is_none()
        });
        // What the last poll was waiting for, reported at the deadline.
        let mut waiting_for = String::new();
        let work = async {
            loop {
                // 1. Own run: its result event is authoritative.
                if let Some(r) = &receipt {
                    let run = Run::from_log(&self.log_path(&r.receipt_id));
                    if unaccepted && (run.init || run.result.is_some()) {
                        self.store
                            .accept(&r.receipt_id, None, Some(&turn_id), Some(&turn_id))?;
                        unaccepted = false;
                    }
                    if let Some(result) = &run.result {
                        return Ok(result_turn(h.clone(), turn_id.clone(), result));
                    }
                }
                let entries = load(&path)?;
                let branch = live_branch(&entries);
                // Only the live branch is read: a prompt superseded by a resume from an
                // earlier point has no answer of its own there.
                let found = entries
                    .iter()
                    .position(|e| e.uuid() == Some(turn_id.as_str()) && e.is_prompt());
                let start = found.filter(|i| branch.contains(i));
                if start.is_none() && receipt.is_none() {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        match found {
                            Some(_) => format!(
                                "turn {turn_id} is on a superseded branch of this transcript"
                            ),
                            None => {
                                format!("turn {turn_id} is not a user message in this transcript")
                            }
                        },
                    ));
                }
                let live = self.live(id).await?;
                let agent_status = match &live {
                    Some(Live::Foreign(a)) => a.status.as_deref(),
                    _ => None,
                };
                // 2. A transcript marker or a later prompt closes the turn; 3. so does
                // the end of every process that could still write it.
                if let Some(s) = start {
                    let end = match turn_end(&entries, &branch, s) {
                        None if live.is_none() => Some(TurnEnd::ProcessGone),
                        end => end,
                    };
                    if let Some(end) = end {
                        return Ok(transcript_turn(
                            h.clone(),
                            &entries,
                            &branch,
                            s,
                            end,
                            agent_status,
                        ));
                    }
                }
                match &live {
                    None => {
                        return Ok(model::Turn {
                            handle: h.clone(),
                            turn_id: turn_id.clone(),
                            status: "unknown",
                            error: Some(json!({
                                "message": "the claude process exited without a result event and the turn is not in the transcript",
                            })),
                            final_text: None,
                            duration_ms: None,
                            basis: Some("claude -p process exited".into()),
                        });
                    }
                    Some(Live::Own(pid)) => {
                        waiting_for = format!(
                            "waiting for the result event of the claude process agent-talk started (pid {pid})"
                        );
                    }
                    Some(Live::Foreign(a)) => {
                        waiting_for = format!(
                            "transcript-derived: waiting for system/turn_duration, an interrupt line or a later user message after the prompt (best effort; claude -p sessions write no turn marker); claude agents: pid {} status {}",
                            a.pid.unwrap_or_default(),
                            a.status.as_deref().unwrap_or("unknown")
                        );
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        let res = bounded(Some(deadline), work)
            .await
            .map(Some)
            .map_err(|mut e| {
                if matches!(e.code, ErrorCode::Timeout | ErrorCode::Interrupted)
                    && !waiting_for.is_empty()
                {
                    e.message = format!("{}; {waiting_for}", e.message);
                }
                e
            });
        let receipt_id = receipt.map(|r| r.receipt_id);
        settle(
            self.store,
            h,
            res,
            receipt_id.as_deref(),
            Vec::new(),
            "running",
        )
    }
}
