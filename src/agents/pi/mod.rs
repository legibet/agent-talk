//! pi adapter (DESIGN.md §6.6).
//!
//! Each mutation runs one `pi --mode json` process (`new` with `--session-id`, `send` with
//! `--session <file>`), writes the prompt to its stdin and closes it. The process's stdout
//! goes to a run log, `~/.agent-talk/pi-runs/<receipt>.ndjson`, which this command tails;
//! the process keeps running if the command stops observing (deadline, Ctrl-C), and its
//! pid is recorded so later commands know the run is still alive. The turn id is the id of
//! the user message's entry in the session file, learned when pi reports the message
//! taken. History, turn boundaries and listings come from the session files under
//! `~/.pi/agent/sessions`.
//!
//! pi has no permission prompts (tools run with the process's rights) and no marker of a
//! live process, so there are no approvals to record and a session open in a TUI cannot be
//! told from an idle one (DESIGN.md §8).

mod run;
mod session;

use self::run::Run;
use self::session::{check_id, head, live_branch, load, transcript_turn, turn_end};
use crate::agents::child::{Runs, agent_cmd, cli_version, lock, observe, pid_alive, tail};
use crate::agents::history::{ReadPage, ReadQuery, cut};
use crate::agents::lifecycle::{bounded, note_waiting, reject_unstarted, settle, wait_receipt};
use crate::agents::{
    Agent, AgentStatus, Check, ListFilter, Operation, SendRequest, StartRequest, WaitTarget,
};
use crate::model::{
    Error, ErrorCode, History, Loaded, Model, Observations, Outcome, Page, Phase, Progress, Result,
    Session, SessionState,
};
use crate::store::{NewIntent, Store};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, UNIX_EPOCH};
use tokio::time::Instant;

/// Why agent-talk refuses a second writer of its own (DESIGN.md §6.6).
const SECOND_WRITER: &str = "a second pi process appending to the session file would branch the conversation, and the next load follows only the newest branch";

/// pi's thinking levels (`pi --help`), what `--effort` takes on a reasoning model.
const EFFORTS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub struct Pi<'a> {
    store: &'a Store,
    /// `<agent dir>/sessions`; the agent dir is `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
    sessions: PathBuf,
    /// `~/.agent-talk/pi-runs`
    runs: Runs,
    /// Set when pi is configured to keep sessions in a flat custom directory, which
    /// agent-talk does not read (DESIGN.md §8): the setting, for the refusal.
    custom_dir: Option<String>,
}

fn handle(id: &str) -> String {
    format!("pi:{id}")
}

/// What this command observes of the `pi` run it started.
struct RunWatch {
    id: String,
    receipt_id: String,
    /// The session file, once it exists (a new session's file appears with the first
    /// user message).
    file: Option<PathBuf>,
    run: Run,
    /// The user entry's id, learned from the session file when pi reports the message taken.
    turn_id: Option<String>,
}

impl RunWatch {
    /// Apply one run-log line: accept the receipt when pi reports the user message taken,
    /// with the entry id the session file gives it.
    fn on_line(&mut self, store: &Store, sessions: &Path, line: &str) -> Result<()> {
        let unaccepted = self.run.user.is_none();
        self.run.apply(line);
        if unaccepted && self.run.user.is_some() {
            if self.file.is_none() {
                self.file = session::find(sessions, &self.id)?;
            }
            self.turn_id = self.file.as_deref().and_then(|f| self.run.turn_id(f));
            if self.turn_id.is_none() {
                tracing::warn!(
                    "pi took the message but its entry was not found in the session file; the turn id is recovered by wait"
                );
            }
            let t = self.turn_id.as_deref();
            store.accept(&self.receipt_id, None, t, t)?;
        }
        Ok(())
    }
}

impl<'a> Pi<'a> {
    pub fn new(store: &'a Store) -> Self {
        let home = std::env::home_dir().unwrap_or_default();
        let agent_dir = match std::env::var("PI_CODING_AGENT_DIR") {
            // pi expands a leading `~` itself.
            Ok(d) if !d.is_empty() => match d.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(d),
            },
            _ => home.join(".pi/agent"),
        };
        let custom_dir = match std::env::var("PI_CODING_AGENT_SESSION_DIR") {
            Ok(d) if !d.is_empty() => Some(format!("PI_CODING_AGENT_SESSION_DIR={d}")),
            _ => std::fs::read_to_string(agent_dir.join("settings.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v["sessionDir"].as_str().map(String::from))
                .filter(|d| !d.is_empty())
                .map(|d| format!("the sessionDir setting, {d}")),
        };
        Pi {
            store,
            sessions: agent_dir.join("sessions"),
            runs: Runs::new("pi", "pi"),
            custom_dir,
        }
    }

    /// Refuse while pi keeps its sessions where agent-talk does not look (DESIGN.md §8).
    fn default_layout(&self) -> Result<()> {
        match &self.custom_dir {
            None => Ok(()),
            Some(why) => Err(Error::new(
                ErrorCode::Unsupported,
                format!(
                    "pi keeps its sessions in a custom directory ({why}), which agent-talk does not read"
                ),
            )),
        }
    }

    /// A run pi left before taking the message: the receipt is rejected with pi's stderr as
    /// the reason (DESIGN.md §3).
    fn reject_startup(&self, receipt_id: &str, status: Option<ExitStatus>) -> Error {
        let e = self.runs.startup_error(receipt_id, status);
        reject_unstarted(
            self.store,
            receipt_id,
            Error::from_agent(ErrorCode::Precondition, e),
        )
    }

    /// Of the pi processes recorded for the session, the pid of one still running its turn:
    /// its run log not settled.
    fn own_run(&self, procs: &[(String, u32)]) -> Option<u32> {
        self.runs.running(procs, |log| Run::from_log(log).settled)
    }

    /// Refuse while a pi process agent-talk started still runs the session. pi has no lock
    /// and no liveness marker, so a foreign process cannot be detected (DESIGN.md §8).
    fn refuse_own_live(&self, id: &str) -> Result<()> {
        let h = handle(id);
        match self.own_run(&self.store.processes(&h)?) {
            None => Ok(()),
            Some(pid) => Err(Error::new(
                ErrorCode::Locked,
                format!(
                    "a pi process started by another agent-talk command (pid {pid}) is still running {h}; {SECOND_WRITER}"
                ),
            )),
        }
    }

    fn require_file(&self, id: &str) -> Result<PathBuf> {
        session::find(&self.sessions, id)?.ok_or_else(|| {
            Error::new(
                ErrorCode::Precondition,
                format!(
                    "no session file for {} under {}",
                    handle(id),
                    self.sessions.display()
                ),
            )
        })
    }

    /// Run one `pi --mode json` turn to completion (or until `deadline` / Ctrl-C, after
    /// which the process keeps running and is tracked by its pid), then settle the receipt.
    /// The caller holds the session lock.
    async fn execute(
        &self,
        cwd: &str,
        args: &[String],
        mut watch: RunWatch,
        prompt: &str,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let h = handle(&watch.id);
        let log = self.runs.log(&watch.receipt_id);
        let mut child = self
            .runs
            .spawn(agent_cmd("pi").args(args), cwd, &watch.receipt_id)
            .map_err(|e| reject_unstarted(self.store, &watch.receipt_id, e))?;
        if let Some(pid) = child.id() {
            self.store.insert_process(&watch.receipt_id, &h, pid)?;
        }
        let work = async {
            // pi reads piped stdin to EOF as the prompt, which keeps `@file` and option
            // parsing away from the text (DESIGN.md §6.6).
            self.runs.deliver(&mut child, prompt.as_bytes()).await?;
            let status = tail(&mut child, &log, |l| {
                watch.on_line(self.store, &self.sessions, l)
            })
            .await?;
            if watch.run.user.is_none() {
                return Err(self.reject_startup(&watch.receipt_id, Some(status)));
            }
            let turn_id = watch.turn_id.clone().unwrap_or_default();
            if !watch.run.settled {
                return Ok(run::exited(h.clone(), turn_id, Some(status)));
            }
            Ok(watch.run.turn(h.clone(), turn_id))
        };
        let res = observe(deadline, work).await;
        settle(
            self.store,
            h,
            res,
            Some(&watch.receipt_id),
            Vec::new(),
            Progress::Running,
        )
    }
}

impl Agent for Pi<'_> {
    /// `pi --list-models`: the models of every provider with credentials, one padded table
    /// row each (`provider model context max-out thinking images`), as `provider/model`.
    async fn models(&self) -> Result<Vec<Model>> {
        let out = agent_cmd("pi")
            .arg("--list-models")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| Error::transport("run pi --list-models", e))?;
        if !out.status.success() {
            return Err(Error::new(
                ErrorCode::Transport,
                format!(
                    "pi --list-models exited with {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(parse_models(&String::from_utf8_lossy(&out.stdout)))
    }

    async fn status(&self) -> AgentStatus {
        let version = cli_version("pi").await;
        let cli: Check = version.as_ref().map(|_| ()).map_err(String::clone);
        let layout: Check = self.default_layout().map_err(|e| e.message);
        let files: Check = layout.clone().and(if self.sessions.is_dir() {
            Ok(())
        } else {
            Err(format!("{} does not exist", self.sessions.display()))
        });
        AgentStatus {
            agent: "pi",
            version: version.ok(),
            shared: None,
            operations: vec![
                Operation::new("ls", &files),
                Operation::new("new", &cli.clone().and(layout.clone())),
                Operation::new("send", &cli.clone().and(files.clone())),
                Operation::new("read", &files),
                Operation::new("wait", &layout),
                Operation::new(
                    "steer",
                    &Err("pi takes messages into a running turn only on that process's stdin, which agent-talk does not keep".into()),
                ),
                Operation::new("name", &cli.and(layout)),
            ],
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        self.default_layout()?;
        struct Row {
            id: String,
            file: PathBuf,
            mtime_ms: i64,
        }
        let want_dir = filter.cwd.map(session::dir_name);
        let mut rows = Vec::new();
        for dir in std::fs::read_dir(&self.sessions)
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(d) = &want_dir
                && dir.file_name().to_string_lossy() != d.as_str()
            {
                continue;
            }
            for f in std::fs::read_dir(dir.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = f.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // `<timestamp>_<id>.jsonl`
                let Some(id) = name
                    .strip_suffix(".jsonl")
                    .and_then(|n| n.split_once('_'))
                    .map(|(_, id)| id)
                else {
                    continue;
                };
                if check_id(id).is_err() {
                    continue;
                }
                let mtime_ms = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                rows.push(Row {
                    id: id.to_string(),
                    file: path,
                    mtime_ms,
                });
            }
        }
        rows.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms).then(a.id.cmp(&b.id)));
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
            let (cwd, name, preview) = head(&row.file);
            let running = self.own_run(&self.store.processes(&h)?).is_some();
            items.push(Session {
                handle: h,
                agent: "pi",
                cwd,
                name,
                preview,
                observations: Observations {
                    history: History::Visible,
                    // pi keeps no marker of a live process: only agent-talk's own runs are known.
                    loaded: if running {
                        Loaded::Yes
                    } else {
                        Loaded::Unknown
                    },
                    origin: if owned {
                        "agent-talk".into()
                    } else {
                        "pi".into()
                    },
                },
                state: if running {
                    SessionState::Running
                } else {
                    SessionState::Unknown
                },
                owned,
                id: row.id,
            });
        }
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        self.default_layout()?;
        let id = uuid::Uuid::new_v4().to_string();
        let h = handle(&id);
        let _lock = lock(&h, SECOND_WRITER)?;
        // pi has no permission prompts, so full access changes nothing; recorded as given.
        self.store.insert_owned(&h, req.cwd, req.settings)?;
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let client_msg_id = uuid::Uuid::new_v4().to_string();
        self.store.insert_intent(&NewIntent {
            receipt_id: &receipt_id,
            handle: &h,
            client_msg_id: &client_msg_id,
            message: &req.message,
        })?;
        let args = run::args(
            ["--session-id", &id],
            req.settings.model.as_deref(),
            req.settings.effort.as_deref(),
            req.name,
        );
        let watch = RunWatch {
            id,
            receipt_id,
            file: None,
            run: Run::default(),
            turn_id: None,
        };
        self.execute(req.cwd, &args, watch, req.message.delivered, deadline)
            .await
    }

    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        check_id(id)?;
        self.default_layout()?;
        if req.steer {
            return Err(Error::new(
                ErrorCode::NoSteer,
                "pi sessions cannot be steered (agent-talk keeps no long-lived pi process); send without --steer",
            ));
        }
        let h = handle(id);
        let path = self.require_file(id)?;
        let _lock = lock(&h, SECOND_WRITER)?;
        self.refuse_own_live(id)?;
        // pi runs a resumed session in the directory its header records and refuses when
        // that directory is gone; the check here gives the clearer message.
        let cwd = match head(&path).0 {
            Some(c) if Path::new(&c).is_dir() => c,
            Some(c) => {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    format!("the session's directory {c} no longer exists; pi cannot resume it"),
                ));
            }
            None => {
                return Err(Error::new(
                    ErrorCode::Precondition,
                    "the session file records no working directory",
                ));
            }
        };
        let receipt_id = uuid::Uuid::new_v4().to_string();
        let client_msg_id = uuid::Uuid::new_v4().to_string();
        self.store.insert_intent(&NewIntent {
            receipt_id: &receipt_id,
            handle: &h,
            client_msg_id: &client_msg_id,
            message: &req.message,
        })?;
        // pi restores the model from the session file, so only a change passes it; the
        // thinking level is not restored and comes from the session's settings.
        let effort = req.settings.and_then(|s| s.effort.as_deref());
        let args = run::args(
            ["--session", &path.display().to_string()],
            req.change.model.as_deref(),
            effort,
            None,
        );
        let watch = RunWatch {
            id: id.to_string(),
            receipt_id,
            file: Some(path),
            run: Run::default(),
            turn_id: None,
        };
        self.execute(&cwd, &args, watch, req.message.delivered, deadline)
            .await
    }

    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage> {
        check_id(id)?;
        self.default_layout()?;
        let entries = load(&self.require_file(id)?)?;
        let branch = live_branch(&entries);
        let mut messages = session::messages(&entries, &branch);
        for (_, m) in &mut messages {
            if m.phase == Phase::Prompt {
                m.from = self.store.sender_of(&m.item_id)?;
            }
        }
        let cut = cut(q, messages, entries.len(), false)?;
        Ok(ReadPage {
            messages: Page {
                next_cursor: cut.cursor_item().map(|(_, m)| m.item_id.clone()),
                items: cut.messages.into_iter().map(|(_, m)| m).collect(),
            },
            // Every entry in the span, other branches included.
            raw: entries[cut.records].iter().map(|e| e.raw.clone()).collect(),
        })
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        check_id(id)?;
        self.default_layout()?;
        let h = handle(id);
        let (mut turn_id, receipt) = match target {
            WaitTarget::Turn(t) => (Some(t.clone()), self.store.receipt_by_turn(&h, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &h, r)?;
                (rec.turn_id.clone(), Some(rec))
            }
        };
        // What the last poll was waiting for, reported at the deadline.
        let mut waiting_for = String::new();
        let work = async {
            loop {
                let file = session::find(&self.sessions, id)?;
                // 1. Own run: while the process lives only its run log counts (the file
                // may hold a failed message pi is about to retry), and the log names the
                // turn when the sending command stopped observing before pi took the message.
                let mut exited = false;
                if let Some(r) = &receipt
                    && self.runs.log(&r.receipt_id).is_file()
                {
                    let run = Run::from_log(&self.runs.log(&r.receipt_id));
                    if turn_id.is_none()
                        && run.user.is_some()
                        && let Some(t) = file.as_deref().and_then(|f| run.turn_id(f))
                    {
                        self.store.accept(&r.receipt_id, None, Some(&t), Some(&t))?;
                        turn_id = Some(t);
                    }
                    if run.settled {
                        return Ok(run.turn(h.clone(), turn_id.clone().unwrap_or_default()));
                    }
                    let pid = self.store.process(&r.receipt_id)?;
                    if pid.is_some_and(pid_alive) {
                        waiting_for = format!(
                            "waiting for the agent_settled event of the pi process agent-talk started (pid {})",
                            pid.unwrap_or_default()
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    // Gone without settling: a startup failure if it never took the
                    // message, otherwise the file says what it left behind.
                    if run.user.is_none() {
                        return Err(self.reject_startup(&r.receipt_id, None));
                    }
                    exited = true;
                }
                let Some(file) = &file else {
                    if exited {
                        return Ok(run::exited(
                            h.clone(),
                            turn_id.clone().unwrap_or_default(),
                            None,
                        ));
                    }
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!("no session file for {h} under {}", self.sessions.display()),
                    ));
                };
                let entries = load(file)?;
                let branch = live_branch(&entries);
                // Only the live branch is read: a prompt on a branch a later load left
                // behind has no answer of its own there.
                let found = turn_id.as_deref().and_then(|t| {
                    entries
                        .iter()
                        .position(|e| e.line.id.as_deref() == Some(t) && e.line.is_prompt())
                });
                let start = found.filter(|i| branch.contains(i));
                if start.is_none() && receipt.is_none() {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        match found {
                            Some(_) => format!(
                                "turn {} is on a branch of this session the newest entry does not descend from",
                                turn_id.as_deref().unwrap_or("?")
                            ),
                            None => format!(
                                "turn {} is not a user message in this session",
                                turn_id.as_deref().unwrap_or("?")
                            ),
                        },
                    ));
                }
                // 2. A reply without tool calls or a later prompt closes the turn.
                if let Some(s) = start
                    && let Some(end) = turn_end(&entries, &branch, s)
                {
                    return Ok(transcript_turn(h.clone(), &entries, s, end));
                }
                if exited {
                    return Ok(run::exited(
                        h.clone(),
                        turn_id.clone().unwrap_or_default(),
                        None,
                    ));
                }
                waiting_for = "session file: waiting for an assistant message without tool calls or a later user message after the prompt (best effort; pi records no process liveness)".into();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        let res = bounded(Some(deadline), work)
            .await
            .map(Some)
            .map_err(|e| note_waiting(e, &waiting_for));
        let receipt_id = receipt.map(|r| r.receipt_id);
        settle(
            self.store,
            h,
            res,
            receipt_id.as_deref(),
            Vec::new(),
            Progress::Running,
        )
    }
}

/// Rows of the `pi --list-models` table as `provider/model`; reasoning models take the
/// thinking levels, the others have no effort setting.
fn parse_models(table: &str) -> Vec<Model> {
    let mut models: Vec<Model> = table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            let (provider, model, thinking) = (cols.first()?, cols.get(1)?, cols.get(4)?);
            Some(Model {
                id: format!("{provider}/{model}"),
                efforts: if *thinking == "yes" {
                    EFFORTS.map(String::from).into()
                } else {
                    Vec::new()
                },
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal rows of `pi --list-models deepseek` (pi 1.0.3, 2026-10-07).
    #[test]
    fn list_models_table() {
        let table = "provider    model                                  context  max-out  thinking  images\n\
                     deepseek    deepseek-flash                         1M       384K     yes       yes\n\
                     newapi2     deepseek-flash                         300K     384K     yes       no\n\
                     openrouter  ~deepseek/deepseek-flash-latest        1.0M     943.7K   no        yes\n";
        let m = parse_models(table);
        let ids: Vec<&str> = m.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "deepseek/deepseek-flash",
                "newapi2/deepseek-flash",
                "openrouter/~deepseek/deepseek-flash-latest"
            ]
        );
        assert_eq!(m[0].efforts, EFFORTS);
        assert!(m[2].efforts.is_empty());
        assert!(parse_models("").is_empty());
    }
}
