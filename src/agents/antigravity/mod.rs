//! Antigravity CLI adapter (DESIGN.md §6.5).
//!
//! Each mutation runs one `agy -p "" --input-format stream-json --output-format stream-json`
//! process in the conversation's cwd (`--conversation <id>` for `send`) and writes one user
//! line; its stdout goes to a run log, `~/.agent-talk/antigravity-runs/<receipt>.ndjson`,
//! which this command tails: `init` names the conversation, the `user_input` step update is
//! the acceptance and its step index the turn id, `result` ends the turn. The process keeps
//! running when the command stops observing, and its pid is recorded, as for Claude.
//!
//! History comes from Antigravity's files under `~/.gemini/antigravity-cli/`: the per-
//! conversation `brain/<id>/.system_generated/logs/transcript.jsonl` (lossy, see `caps`),
//! the shared `conversation_summaries.db` (read-only, in place) and `presence/<id>.lock`,
//! which the process that has a conversation open keeps flocked. agy itself ignores that
//! lock, so agent-talk refuses to write while it is held.
//!
//! Approvals: headless agy denies every tool the user's `settings.json` does not allow and
//! ends the turn; nothing is pending and there is nothing to answer. The denials arrive as
//! `result.denied_actions` and are recorded as `denied`.

mod process;
mod stream;
mod transcript;

use self::process::{agent_error, args, spawn, wait_init};
use self::stream::{Event, Run, decode, denials, result_turn};
use self::transcript::{
    ends_with_reply, load, messages, preview, span_reply, turn_span, user_input,
};
use super::{
    Agent, Caps, Check, ListFilter, Mode, Operation, ReadPage, ReadRange, SendRequest,
    StartRequest, WaitTarget, bounded, cli_version, lock, pid_alive, record, settle, tail,
    wait_receipt,
};
use crate::model::{
    self, Approval, Error, ErrorCode, Observations, Outcome, Page, ReceiptState, Result, Session,
};
use crate::store::{NewIntent, Store};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::time::Instant;

/// Why agent-talk refuses a second writer (DESIGN.md §6.5).
const SECOND_WRITER: &str = "agy has no lock between writers: a second process on the same conversation overwrites its steps by index and silently drops messages";

/// What the transcript cannot show, attached to every turn derived from it.
const TRANSCRIPT_LOSSES: &str = "steps that never ran are absent (agy 1.2.16 also left out denied tool steps; 1.2.17 writes them as ERROR), a tool step written as RUNNING is never updated, long content is truncated, and two writers leave duplicate step indices";

pub struct Antigravity<'a> {
    store: &'a Store,
    /// `~/.gemini/antigravity-cli`
    dir: PathBuf,
    /// `~/.agent-talk/antigravity-runs`
    runs: PathBuf,
}

fn handle(id: &str) -> String {
    format!("antigravity:{id}")
}

/// Conversation ids are UUIDs; anything else is refused before it reaches a path.
fn check_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map(|_| ()).map_err(|_| {
        Error::new(
            ErrorCode::Precondition,
            format!("invalid Antigravity conversation id {id}; expected a UUID"),
        )
    })
}

fn io_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Transport, format!("{what}: {e}"))
}

/// What this command observes of the `agy -p` run it started.
struct RunWatch {
    /// The conversation the run must be in.
    id: String,
    receipt_id: String,
    run: Run,
}

impl RunWatch {
    /// Apply one run-log line: refuse a run in another conversation, accept the receipt
    /// on the user step.
    fn on_event(&mut self, store: &Store, line: &str) -> Result<()> {
        let Some(ev) = decode(line) else {
            if serde_json::from_str::<Value>(line).is_err() {
                tracing::warn!("non-JSON line from agy: {line}");
            }
            return Ok(());
        };
        match &ev {
            // agy starts a new conversation, with a warning only, when it does not find the
            // id (DESIGN.md §6.5).
            Event::Init { conversation_id } if *conversation_id != self.id => {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    format!(
                        "agy ran the message in conversation {} instead of {} (agy starts a new conversation when it does not find the id); see {}",
                        handle(conversation_id),
                        handle(&self.id),
                        handle(conversation_id)
                    ),
                ));
            }
            Event::StepUpdate { step_update: s }
                if s.step_type == "user_input" && self.run.turn_id().is_none() =>
            {
                store.accept(
                    &self.receipt_id,
                    None,
                    Some(&s.step_index.to_string()),
                    None,
                )?;
            }
            _ => {}
        }
        self.run.apply(&ev);
        Ok(())
    }
}

/// One row of `conversation_summaries.db`.
struct Summary {
    id: String,
    title: String,
    /// `YYYY-MM-DD HH:MM:SS.ffffff+00:00`; sorts as text.
    modified: String,
    cwd: Option<String>,
    /// CASCADE_RUN_STATUS_IDLE | CASCADE_RUN_STATUS_RUNNING; empty on rows agy has not
    /// rewritten since an older version.
    status: String,
    raw: Value,
}

const SUMMARY_COLUMNS: &str = "conversation_id, title, preview, step_count, last_modified_time, workspace_uris, status, not_fully_idle, killed, last_user_input_time";

fn summary_row(row: &rusqlite::Row) -> rusqlite::Result<Summary> {
    let id: String = row.get(0)?;
    let title: String = row.get(1)?;
    let modified: String = row.get(4)?;
    let uris: String = row.get(5)?;
    let status: String = row.get(6)?;
    let raw = json!({
        "conversation_id": id,
        "title": title,
        "preview": row.get::<_, String>(2)?,
        "step_count": row.get::<_, i64>(3)?,
        "last_modified_time": modified,
        "workspace_uris": uris,
        "status": status,
        "not_fully_idle": row.get::<_, Option<i64>>(7)?,
        "killed": row.get::<_, Option<i64>>(8)?,
        "last_user_input_time": row.get::<_, String>(9)?,
    });
    let cwd = serde_json::from_str::<Vec<String>>(&uris)
        .ok()
        .and_then(|u| u.first().and_then(|u| uri_path(u)));
    Ok(Summary {
        id,
        title,
        modified,
        cwd,
        status,
        raw,
    })
}

/// `file:///a/b%20c` → `/a/b c` (agy percent-encodes the workspace URIs).
fn uri_path(uri: &str) -> Option<String> {
    let path = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let hex = path
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (path[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

impl<'a> Antigravity<'a> {
    pub fn new(store: &'a Store) -> Self {
        let home = std::env::home_dir().unwrap_or_default();
        let data = home.join(".agent-talk");
        Antigravity {
            store,
            dir: home.join(".gemini/antigravity-cli"),
            runs: data.join("antigravity-runs"),
        }
    }

    fn log_path(&self, receipt_id: &str) -> PathBuf {
        self.runs.join(format!("{receipt_id}.ndjson"))
    }

    fn transcript_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("brain")
            .join(id)
            .join(".system_generated/logs/transcript.jsonl")
    }

    /// `conversation_summaries.db`, read-only in place; `None` when agy never ran. The db
    /// is in WAL mode, and agy deletes the `-wal` and `-shm` files when its last process
    /// exits; a read-only connection cannot recreate them (SQLITE_CANTOPEN), so the db
    /// is then opened as immutable, which is exact while no writer is open.
    fn summaries_db(&self) -> Result<Option<Connection>> {
        let path = self.dir.join("conversation_summaries.db");
        if !path.is_file() {
            return Ok(None);
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
        let db = Connection::open_with_flags(&path, flags).or_else(|e| {
            let uri = format!("file:{}?immutable=1", path.display());
            Connection::open_with_flags(&uri, flags).map_err(|_| e)
        })?;
        Ok(Some(db))
    }

    /// Every summary row, newest first (id as tie-breaker).
    fn summaries(&self) -> Result<Vec<Summary>> {
        let Some(db) = self.summaries_db()? else {
            return Ok(Vec::new());
        };
        let mut stmt = db.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM conversation_summaries
             ORDER BY last_modified_time DESC, conversation_id ASC"
        ))?;
        let rows = stmt
            .query_map([], summary_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn summary(&self, id: &str) -> Result<Option<Summary>> {
        let Some(db) = self.summaries_db()? else {
            return Ok(None);
        };
        let row = db
            .query_row(
                &format!(
                    "SELECT {SUMMARY_COLUMNS} FROM conversation_summaries WHERE conversation_id = ?1"
                ),
                [id],
                summary_row,
            )
            .optional()?;
        Ok(row)
    }

    /// An unknown `--conversation` id silently starts a new conversation, so `send`
    /// requires the conversation's database or summary row first.
    fn require_conversation(&self, id: &str) -> Result<Option<Summary>> {
        let summary = self.summary(id)?;
        let db = self.dir.join("conversations").join(format!("{id}.db"));
        if summary.is_none() && !db.is_file() {
            return Err(Error::new(
                ErrorCode::Precondition,
                format!(
                    "no Antigravity conversation {} ({} and its summary row are absent); agy would silently start a new conversation instead",
                    handle(id),
                    db.display()
                ),
            ));
        }
        Ok(summary)
    }

    /// Whether some process has the conversation open: a non-blocking flock on
    /// `presence/<id>.lock` fails while agy (TUI or `-p`) holds it. An absent file is
    /// unheld; any other failure is an error, never "held".
    fn presence_held(&self, id: &str) -> Result<bool> {
        let path = self.dir.join("presence").join(format!("{id}.lock"));
        let f = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err(&path.display().to_string(), e)),
        };
        // Released when `f` is dropped.
        match f.try_lock() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(io_err(&path.display().to_string(), e)),
        }
    }

    /// Of the conversation's recorded agy processes, the pid of one still running its
    /// turn: alive, and its run log has no `result` yet.
    fn own_run(&self, procs: &[(String, u32)]) -> Option<u32> {
        procs
            .iter()
            .find(|(receipt_id, pid)| {
                pid_alive(*pid) && Run::from_log(&self.log_path(receipt_id)).result.is_none()
            })
            .map(|(_, pid)| *pid)
    }

    /// Refuse while anything else has the conversation open. agent-talk's own children are
    /// classified by its `processes` table; flock does not name the holder.
    fn refuse_live(&self, id: &str) -> Result<()> {
        if let Some(pid) = self.own_run(&self.store.processes(&handle(id))?) {
            return Err(Error::new(
                ErrorCode::Locked,
                format!(
                    "an agy process started by another agent-talk command (pid {pid}) is still running {}; {SECOND_WRITER}",
                    handle(id)
                ),
            ));
        }
        if self.presence_held(id)? {
            return Err(Error::new(
                ErrorCode::ForeignLive,
                format!(
                    "{} is open in an agy process agent-talk did not start (a TUI or another agy -p; its presence lock is held, and flock does not name the holder); {SECOND_WRITER}, so agent-talk only reads this conversation",
                    handle(id)
                ),
            ));
        }
        Ok(())
    }

    /// What the run log of a run that is over (a `result`, or the process gone) means:
    /// without a user step agy never took the message and the receipt is `rejected` with
    /// agy's error; without a `result` the turn's outcome is unknown; otherwise the turn is
    /// the `result`, and its denials are recorded. `exit_code` is known when this command
    /// saw the process exit.
    fn finished(
        &self,
        h: &str,
        receipt_id: &str,
        log: &Path,
        run: &Run,
        exit_code: Option<i32>,
    ) -> Result<(model::Turn, Vec<Approval>)> {
        let Some(turn_id) = run.turn_id() else {
            let agent_error = agent_error(log, run, exit_code);
            self.store
                .settle_unaccepted(receipt_id, ReceiptState::Rejected, Some(&agent_error))?;
            return Err(Error::from_agent(ErrorCode::Precondition, agent_error));
        };
        let Some(result) = &run.result else {
            let turn = model::Turn {
                handle: h.to_string(),
                turn_id,
                status: "unknown",
                error: Some(json!({
                    "message": "agy exited without a result event",
                    "exit_code": exit_code,
                })),
                final_text: None,
                duration_ms: None,
                basis: Some(
                    "the agy -p process agent-talk started exited without a result event".into(),
                ),
                raw: Value::Null,
            };
            return Ok((turn, Vec::new()));
        };
        let approvals = denials(h, &turn_id, receipt_id, result);
        for a in &approvals {
            record(self.store, a);
        }
        Ok((result_turn(h.to_string(), run, result), approvals))
    }

    /// Write the message, close stdin, and observe the run to its `result` (or until
    /// `deadline` / Ctrl-C, after which the process keeps running and is tracked by its
    /// pid); then settle the receipt. The caller holds the conversation lock.
    async fn execute(
        &self,
        mut child: Child,
        log: PathBuf,
        mut watch: RunWatch,
        prompt: &str,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let h = handle(&watch.id);
        if let Some(pid) = child.id() {
            self.store.insert_process(&watch.receipt_id, &h, pid)?;
        }
        let line = json!({"event": "user", "message": {"content": prompt}}).to_string();
        let mut approvals = Vec::new();
        let work = async {
            let mut stdin = child.stdin.take().expect("stdin is piped");
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| io_err("write the message to agy", e))?;
            // Closing stdin makes agy exit after this turn's result.
            stdin
                .shutdown()
                .await
                .map_err(|e| io_err("close agy stdin", e))?;
            drop(stdin);
            let status = tail(&mut child, &log, |l| watch.on_event(self.store, l)).await?;
            let (turn, denied) =
                self.finished(&h, &watch.receipt_id, &log, &watch.run, status.code())?;
            approvals = denied;
            Ok(turn)
        };
        // Without --wait the turn still runs to completion (synchronous sends); only
        // Ctrl-C stops observing early.
        let res = bounded(deadline, work).await;
        // Without a deadline the turn is not reported even though it ran to completion.
        let res = res.map(|turn| deadline.is_some().then_some(turn));
        settle(
            self.store,
            h,
            res,
            Some(&watch.receipt_id),
            approvals,
            "running",
        )
    }

    /// A turn agent-talk did not run (or whose run log is gone), from the transcript
    /// (DESIGN.md §6.5): the USER_INPUT `turn_id` must exist; the turn is
    /// bounded by the next USER_INPUT, or by a final reply while no process holds the
    /// conversation and the summary is not RUNNING. `Ok(Err(w))`: still open, `w` says what
    /// the check is waiting for.
    fn foreign_turn(
        &self,
        id: &str,
        turn_id: &str,
    ) -> Result<std::result::Result<model::Turn, String>> {
        let lines = load(&self.transcript_path(id))?;
        let Some(start) = user_input(&lines, turn_id) else {
            return Err(Error::new(
                ErrorCode::Precondition,
                format!(
                    "turn {turn_id} is not a USER_INPUT step in the transcript of {}",
                    handle(id)
                ),
            ));
        };
        let (end, closed) = turn_span(&lines, start);
        let span = &lines[start..end];
        let replied = ends_with_reply(span);
        let turn = |status, basis: String, observed: Value| model::Turn {
            handle: handle(id),
            turn_id: turn_id.to_string(),
            status,
            error: None,
            final_text: span_reply(span),
            duration_ms: None,
            basis: Some(format!(
                "{basis}; the transcript is lossy: {TRANSCRIPT_LOSSES}"
            )),
            raw: json!({
                "observed": observed,
                "lines": span.iter().map(|l| &l.raw).collect::<Vec<_>>(),
            }),
        };
        if closed {
            return Ok(Ok(turn(
                if replied { "completed" } else { "unknown" },
                "transcript-derived: a later USER_INPUT exists".into(),
                Value::Null,
            )));
        }
        let held = self.presence_held(id)?;
        let status = self.summary(id)?.map(|s| s.status).unwrap_or_default();
        let observed = json!({"presence_lock_held": held, "summary_status": status});
        if held {
            return Ok(Err(format!(
                "transcript-derived: waiting for a later USER_INPUT, or for the presence lock to be released after a final reply (held by some agy process; summary status {status:?})"
            )));
        }
        // Nobody holds the conversation, so nothing will add to the turn.
        let running = status == "CASCADE_RUN_STATUS_RUNNING";
        Ok(Ok(if replied && !running {
            turn(
                "completed",
                format!(
                    "transcript-derived, best effort: the turn ends with a reply without tool calls, no process holds the conversation and the summary status is {status:?}"
                ),
                observed,
            )
        } else {
            turn(
                "unknown",
                format!(
                    "transcript-derived: no process holds the conversation, the summary status is {status:?} (a killed run may leave RUNNING) and the transcript {} (a denied tool ends the turn without a reply)",
                    if replied {
                        "ends with a reply"
                    } else {
                        "does not end with a reply"
                    }
                ),
                observed,
            )
        }))
    }
}

impl Agent for Antigravity<'_> {
    async fn caps(&self) -> Caps {
        let version = cli_version("agy").await;
        let cli: Check = version.as_ref().map(|_| ()).map_err(String::clone);
        let store: Check = if self.dir.is_dir() {
            Ok(())
        } else {
            Err(format!("{} does not exist", self.dir.display()))
        };
        Caps {
            agent: "antigravity",
            version: version.ok(),
            shared: None,
            operations: vec![
                Operation::new("ls", &store),
                Operation::new("new", &cli),
                Operation::new("send", &cli),
                Operation::new("read", &store),
                Operation::new("wait", &Ok(())),
                Operation::new(
                    "steer",
                    &Err("Antigravity CLI cannot add a message to a running turn".into()),
                ),
                Operation::new(
                    "name",
                    &Err("Antigravity CLI can name a conversation only in its TUI".into()),
                ),
            ],
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        let mut rows: Vec<Summary> = self
            .summaries()?
            .into_iter()
            .filter(|r| filter.cwd.is_none() || r.cwd.as_deref() == filter.cwd)
            .collect();
        // Cursor: `<last_modified_time>|<id>` of the last row of the previous page.
        if let Some(c) = cursor {
            let (t, id) = c.split_once('|').ok_or_else(|| {
                Error::new(ErrorCode::Precondition, format!("invalid cursor {c}"))
            })?;
            rows.retain(|r| r.modified.as_str() < t || (r.modified == t && r.id.as_str() > id));
        }
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next_cursor = more
            .then(|| rows.last().map(|r| format!("{}|{}", r.modified, r.id)))
            .flatten();
        let mut items = Vec::new();
        for row in rows {
            let h = handle(&row.id);
            let owned = self.store.is_owned(&h)?;
            let own_pid = self.own_run(&self.store.processes(&h)?);
            let held = self.presence_held(&row.id).ok();
            let running = row.status == "CASCADE_RUN_STATUS_RUNNING";
            let state = if own_pid.is_some() {
                "running"
            } else {
                // A killed run leaves RUNNING with the lock released.
                match (held, running) {
                    (Some(true), true) => "running",
                    (Some(false), true) | (None, _) => "unknown",
                    (Some(_), false) => "idle",
                }
            };
            let (has_history, first_prompt) = preview(&self.transcript_path(&row.id));
            items.push(Session {
                handle: h,
                agent: "antigravity",
                cwd: row.cwd,
                name: (!row.title.is_empty()).then_some(row.title),
                preview: first_prompt,
                observations: Observations {
                    history: if has_history { "visible" } else { "none" },
                    loaded: match (own_pid, held) {
                        (Some(_), _) | (_, Some(true)) => "yes",
                        (None, Some(false)) => "no",
                        (None, None) => "unknown",
                    },
                    origin: if owned { "agent-talk" } else { "antigravity" }.into(),
                },
                state,
                owned,
                raw: row.raw,
                id: row.id,
            });
        }
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        if req.name.is_some() {
            return Err(Error::new(
                ErrorCode::Unsupported,
                "Antigravity has no title interface outside its TUI (/rename); start without --name",
            ));
        }
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let log = self.log_path(&receipt_id);
        let mut child = spawn(
            req.cwd,
            &args(None, req.model, req.effort, req.full_access),
            &log,
        )?;
        // The conversation id exists only once agy reports it; the intent is written then,
        // before the message is.
        let id = match bounded(deadline, wait_init(&mut child, &log)).await {
            Ok(id) => id,
            Err(mut e) => {
                // Closing stdin unsent makes agy exit without a turn.
                drop(child.stdin.take());
                if matches!(e.code, ErrorCode::Timeout | ErrorCode::Interrupted) {
                    e.message = format!(
                        "agy did not report a conversation id before the deadline; no message was sent (agy may leave an empty conversation; run log {})",
                        log.display()
                    );
                }
                return Err(e);
            }
        };
        // The id reaches the lock and brain paths.
        check_id(&id)?;
        let h = handle(&id);
        let _lock = lock(&h, SECOND_WRITER)?;
        let stored_args = json!({
            "model": req.model,
            "effort": req.effort,
            "full_access": req.full_access,
        });
        self.store.insert_owned(&h, req.cwd, &stored_args)?;
        self.store.insert_intent(&NewIntent {
            receipt_id: &receipt_id,
            handle: &h,
            // No client message id reaches agy; the receipt id stands in.
            client_msg_id: &receipt_id,
            text: req.prompt,
            delivered_text: (req.delivered != req.prompt).then_some(req.delivered),
            from: req.from,
            reply_to: None,
            depth: req.depth,
        })?;
        let watch = RunWatch {
            id,
            receipt_id,
            run: Run::default(),
        };
        self.execute(child, log, watch, req.delivered, deadline)
            .await
    }

    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        check_id(id)?;
        if req.mode == Mode::Steer {
            return Err(Error::new(
                ErrorCode::NoSteer,
                "Antigravity conversations cannot be steered (a line written during a turn becomes the next turn); send with --mode queue",
            ));
        }
        let h = handle(id);
        let summary = self.require_conversation(id)?;
        let _lock = lock(&h, SECOND_WRITER)?;
        self.refuse_live(id)?;
        // agy makes its process cwd the conversation's workspace: never pick one silently.
        let cwd = match self.store.owned_cwd(&h)? {
            Some(c) => c,
            None => match summary.and_then(|s| s.cwd) {
                Some(c) if Path::new(&c).is_dir() => c,
                Some(c) => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!(
                            "the conversation's workspace {c} no longer exists; agy would run it elsewhere"
                        ),
                    ));
                }
                None => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        "conversation_summaries.db records no workspace for this conversation",
                    ));
                }
            },
        };
        // A conversation agent-talk started keeps the model, effort and permissions `new`
        // gave it.
        let started = self.store.owned_args(&h)?.unwrap_or_default();
        let args = args(
            Some(id),
            started["model"].as_str(),
            started["effort"].as_str(),
            started["full_access"] == true,
        );
        let receipt_id = uuid::Uuid::new_v4().to_string();
        self.store.insert_intent(&NewIntent {
            receipt_id: &receipt_id,
            handle: &h,
            client_msg_id: &receipt_id,
            text: req.text,
            delivered_text: (req.delivered != req.text).then_some(req.delivered),
            from: req.from,
            reply_to: req.reply_to,
            depth: req.depth,
        })?;
        let log = self.log_path(&receipt_id);
        let child = match spawn(&cwd, &args, &log) {
            Ok(c) => c,
            Err(e) => {
                self.store
                    .settle_unaccepted(&receipt_id, ReceiptState::Rejected, None)?;
                return Err(e);
            }
        };
        let watch = RunWatch {
            id: id.to_string(),
            receipt_id,
            run: Run::default(),
        };
        self.execute(child, log, watch, req.delivered, deadline)
            .await
    }

    async fn read(&self, id: &str, range: ReadRange) -> Result<ReadPage> {
        check_id(id)?;
        self.require_conversation(id)?;
        let h = handle(id);
        let lines = load(&self.transcript_path(id))?;
        let mut messages = messages(&lines);
        for (_, m) in &mut messages {
            if m.role == "user" && m.from.is_none() {
                m.from = self.store.sender_by_turn(&h, &m.turn_id)?;
            }
        }
        let (start_msg, end_msg, start_line) = match range {
            ReadRange::Tail(n) => {
                let s = messages.len().saturating_sub(n as usize);
                let line = messages.get(s).map_or(lines.len(), |(i, _)| *i);
                (s, messages.len(), line)
            }
            ReadRange::Forward { since: None, limit } => {
                (0, (limit as usize).min(messages.len()), 0)
            }
            ReadRange::Forward {
                since: Some(c),
                limit,
            } => {
                let line: usize = c.parse().map_err(|_| {
                    Error::new(
                        ErrorCode::Precondition,
                        format!("invalid cursor {c}; expected a transcript line position"),
                    )
                })?;
                let s = messages.partition_point(|(i, _)| *i <= line);
                (s, (s + limit as usize).min(messages.len()), line + 1)
            }
        };
        // A planner response with text and tool calls yields two messages from one line;
        // the cursor is a line, so a page never ends between them.
        let mut end_msg = end_msg;
        while end_msg > start_msg
            && end_msg < messages.len()
            && messages[end_msg].0 == messages[end_msg - 1].0
        {
            end_msg += 1;
        }
        let more = end_msg < messages.len();
        let page: Vec<_> = messages.drain(start_msg..end_msg).collect();
        // An empty page (`--limit 0`) with more left gets no cursor (it would skip the
        // rest) and no raw lines.
        let end_line = match (more, page.last()) {
            (true, Some((i, _))) => i + 1,
            (true, None) => start_line.min(lines.len()),
            (false, _) => lines.len(),
        };
        let next_cursor = more
            .then(|| page.last().map(|(i, _)| i.to_string()))
            .flatten();
        Ok(ReadPage {
            messages: Page {
                items: page.into_iter().map(|(_, m)| m).collect(),
                next_cursor,
            },
            raw: lines[start_line.min(lines.len())..end_line]
                .iter()
                .map(|l| l.raw.clone())
                .collect(),
        })
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        check_id(id)?;
        let h = handle(id);
        let (turn_id, receipt) = match target {
            WaitTarget::Turn(t) => (Some(t.clone()), self.store.receipt_by_turn(&h, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &h, r)?;
                (rec.turn_id.clone(), Some(rec))
            }
        };
        self.require_conversation(id)?;
        let mut approvals = Vec::new();
        // The sending command stopped observing before the user step: accept the receipt
        // here, once, when the run log shows it.
        let mut unaccepted = receipt.as_ref().is_some_and(|r| r.turn_id.is_none());
        let receipt_id = receipt.map(|r| r.receipt_id);
        let run_log = receipt_id.as_deref().map(|r| (r, self.log_path(r)));
        // What the last poll was waiting for, reported at the deadline.
        let mut waiting_for = String::new();
        let work = async {
            loop {
                // 1. Own run: its log is authoritative.
                if let Some((r, log)) = &run_log
                    && log.is_file()
                {
                    let run = Run::from_log(log);
                    if unaccepted && let Some(t) = run.turn_id() {
                        self.store.accept(r, None, Some(&t), None)?;
                        unaccepted = false;
                    }
                    let pid = self
                        .store
                        .processes(&h)?
                        .into_iter()
                        .find(|(rid, _)| rid == r)
                        .map(|(_, pid)| pid);
                    if run.result.is_some() || !pid.is_some_and(pid_alive) {
                        // This command did not start the process: no exit code.
                        let (turn, denied) = self.finished(&h, r, log, &run, None)?;
                        approvals = denied;
                        return Ok(turn);
                    }
                    waiting_for = format!(
                        "waiting for the result event of the agy process agent-talk started (pid {})",
                        pid.unwrap_or_default()
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                // 2. Anything else: the transcript rule.
                let Some(t) = &turn_id else {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        "the receipt has no turn id and no run log; its outcome cannot be recovered",
                    ));
                };
                match self.foreign_turn(id, t)? {
                    Ok(turn) => return Ok(turn),
                    Err(w) => waiting_for = w,
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
        settle(
            self.store,
            h,
            res,
            receipt_id.as_deref(),
            approvals,
            "running",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_conversation_is_refused() {
        let s = Store::memory();
        let mut w = RunWatch {
            id: "11111111-2222-3333-4444-555555555555".into(),
            receipt_id: "r".into(),
            run: Run::default(),
        };
        let init = r#"{"event":"init","conversation_id":"06615f44-46a4-4237-8139-581ebfb0c167","init":{}}"#;
        let e = w.on_event(&s, init).unwrap_err();
        assert_eq!(e.code, ErrorCode::Precondition);
        assert!(e.message.contains("06615f44") && e.message.contains("11111111"));
    }

    #[test]
    fn workspace_uri_is_decoded() {
        assert_eq!(
            uri_path("file:///Users/me/projects/dir%20with%20space").as_deref(),
            Some("/Users/me/projects/dir with space")
        );
        assert_eq!(uri_path("/no/scheme"), None);
    }
}
