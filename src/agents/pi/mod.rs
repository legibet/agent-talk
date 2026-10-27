//! pi adapter (DESIGN.md §6.6).
//!
//! Each mutation runs one `pi --mode json` process (`new` with `--session-id`, `send` with
//! `--session <file>`), writes the prompt to its stdin and closes it. The process's stdout
//! goes to a run log, `~/.agent-talk/pi-runs/<receipt>.ndjson`, which this command tails;
//! the process keeps running when the command stops observing (deadline, Ctrl-C, or the
//! message taken without one), and its pid is recorded so later commands know the run is
//! still alive. The turn id is the id of the user message's entry in the session file,
//! learned when pi reports the message taken. History, turn boundaries and listings come
//! from the session files under pi's agent dir.
//!
//! pi has no permission prompts (tools run with the process's rights) and no marker of a
//! live process, so there are no approvals to record and a session open in a TUI cannot be
//! told from an idle one (DESIGN.md §8).

mod run;
mod session;

use self::run::Run;
use self::session::{Entry, check_id, live_branch, load, transcript_turn, turn_end};
use crate::agents::child::{Runs, agent_cmd, cli_version, lock, pid_alive, tail};
use crate::agents::history::{ReadPage, ReadQuery, cut};
use crate::agents::lifecycle::{bounded, note_waiting, reject_unstarted, settle, wait_receipt};
use crate::agents::{
    Agent, AgentStatus, Check, ListFilter, Operation, Outgoing, SendRequest, StartRequest, Wait,
    WaitTarget,
};
use crate::model::{
    Error, ErrorCode, History, Loaded, Model, Observations, Outcome, Page, Phase, Progress, Result,
    Session, SessionState, Turn,
};
use crate::store::{NewIntent, Store};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

/// Why agent-talk refuses a second writer of its own (DESIGN.md §6.6).
const SECOND_WRITER: &str = "a second pi process appending to the session file would branch the conversation, and the next load follows only the newest branch";

/// pi's thinking levels (`pi --help`), what `--effort` takes on a reasoning model.
const EFFORTS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub struct Pi<'a> {
    store: &'a Store,
    /// `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
    agent_dir: PathBuf,
    /// `<agent dir>/sessions`
    sessions: PathBuf,
    /// `~/.agent-talk/pi-runs`
    runs: Runs,
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
    /// with the entry id the session file gives it. Whether pi has taken the message.
    fn on_line(&mut self, store: &Store, sessions: &Path, line: &str) -> Result<bool> {
        let unaccepted = self.run.user.is_none();
        self.run.apply(line);
        if unaccepted && self.run.user.is_some() {
            if self.file.is_none() {
                self.file = session::find(sessions, &self.id)?;
            }
            self.turn_id = self
                .file
                .as_deref()
                .and_then(|f| session::load(f).ok())
                .and_then(|e| self.run.turn_id(&e));
            if self.turn_id.is_none() {
                tracing::warn!(
                    "pi took the message but its entry was not found in the session file; the turn id is recovered by wait"
                );
            }
            let t = self.turn_id.as_deref();
            store.accept(&self.receipt_id, None, t, t)?;
        }
        Ok(self.run.user.is_some())
    }
}

/// What the run log of the pi process agent-talk started for a receipt says, when the
/// process did take the message.
enum RunState {
    /// The process still runs the turn.
    Running(u32),
    /// `agent_settled`: the turn as the run log has it.
    Settled(Turn),
    /// Gone without `agent_settled`: the session file says what it left behind.
    Exited,
}

impl<'a> Pi<'a> {
    pub fn new(store: &'a Store) -> Self {
        let home = std::env::home_dir().unwrap_or_default();
        let agent_dir = match std::env::var("PI_CODING_AGENT_DIR") {
            // pi expands a leading `~/`, and so does agent-talk.
            Ok(d) if !d.is_empty() => match d.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(d),
            },
            _ => home.join(".pi/agent"),
        };
        Pi {
            store,
            sessions: agent_dir.join("sessions"),
            agent_dir,
            runs: Runs::new("pi", "pi"),
        }
    }

    /// Refuse while pi is configured to keep its sessions in a flat custom directory, which
    /// agent-talk does not read (DESIGN.md §8). Checked by every pi operation but `models`.
    fn check_layout(&self) -> Result<()> {
        let custom = match std::env::var("PI_CODING_AGENT_SESSION_DIR") {
            Ok(d) if !d.is_empty() => Some(format!("PI_CODING_AGENT_SESSION_DIR={d}")),
            _ => std::fs::read_to_string(self.agent_dir.join("settings.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v["sessionDir"].as_str().map(String::from))
                .filter(|d| !d.is_empty())
                .map(|d| format!("the sessionDir setting, {d}")),
        };
        match custom {
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

    fn missing_file(&self, id: &str) -> Error {
        Error::new(
            ErrorCode::Precondition,
            format!(
                "no session file for {} under {}",
                handle(id),
                self.sessions.display()
            ),
        )
    }

    fn require_file(&self, id: &str) -> Result<PathBuf> {
        session::find(&self.sessions, id)?.ok_or_else(|| self.missing_file(id))
    }

    /// Record the intent, run one `pi --mode json` turn of session `id` and observe it to
    /// its end, until `deadline` or Ctrl-C, or without a deadline until pi took the message;
    /// the process keeps running after that, tracked by its pid. Then settle the receipt.
    /// `file` is the session file, `None` for a new session. The caller holds the session
    /// lock.
    async fn execute(
        &self,
        id: String,
        file: Option<PathBuf>,
        cwd: &str,
        args: &[String],
        message: &Outgoing<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let h = handle(&id);
        let receipt_id = uuid::Uuid::new_v4().to_string();
        self.store.insert_intent(&NewIntent {
            receipt_id: &receipt_id,
            handle: &h,
            client_msg_id: &uuid::Uuid::new_v4().to_string(),
            message,
        })?;
        let log = self.runs.log(&receipt_id);
        let mut child = self
            .runs
            .spawn(agent_cmd("pi").args(args), cwd, &receipt_id)
            .map_err(|e| reject_unstarted(self.store, &receipt_id, e))?;
        if let Some(pid) = child.id() {
            self.store.insert_process(&receipt_id, &h, pid)?;
        }
        let mut watch = RunWatch {
            id,
            receipt_id,
            file,
            run: Run::default(),
            turn_id: None,
        };
        let work = async {
            // pi reads piped stdin to EOF as the prompt, which keeps `@file` and option
            // parsing away from the text (DESIGN.md §6.6).
            self.runs
                .deliver(&mut child, message.delivered.as_bytes())
                .await?;
            let Some(status) = tail(&mut child, &log, deadline.is_none(), |l| {
                watch.on_line(self.store, &self.sessions, l)
            })
            .await?
            else {
                return Ok(None);
            };
            if watch.run.user.is_none() {
                return Err(self.reject_startup(&watch.receipt_id, Some(status)));
            }
            let turn_id = watch.turn_id.clone().unwrap_or_default();
            if !watch.run.settled {
                let turn = self
                    .runs
                    .exited_turn(h.clone(), turn_id, "agent_settled", Some(status));
                return Ok(Some(turn));
            }
            Ok(Some(watch.run.turn(h.clone(), turn_id)))
        };
        let res = bounded(deadline, work).await;
        settle(
            self.store,
            h,
            res,
            Some(&watch.receipt_id),
            Vec::new(),
            Progress::Running,
        )
    }

    /// What the run log of the process agent-talk started for `receipt_id` says, `None`
    /// without a log. While the turn id is unknown (the sending command stopped observing
    /// before pi took the message) and the log shows the message taken, the entry is looked
    /// up in `file` and the receipt accepted with it. A process gone before taking the
    /// message is a startup failure.
    fn run_state(
        &self,
        h: &str,
        receipt_id: &str,
        turn_id: &mut Option<String>,
        file: Option<&Path>,
    ) -> Result<Option<RunState>> {
        let log = self.runs.log(receipt_id);
        if !log.is_file() {
            return Ok(None);
        }
        // Liveness before the log: once the process is gone, the log read after that is
        // complete.
        let alive = self
            .store
            .process(receipt_id)?
            .filter(|&pid| pid_alive(pid));
        let run = Run::from_log(&log);
        if turn_id.is_none()
            && run.user.is_some()
            && let Some(t) = file
                .and_then(|f| load(f).ok())
                .and_then(|e| run.turn_id(&e))
        {
            self.store.accept(receipt_id, None, Some(&t), Some(&t))?;
            *turn_id = Some(t);
        }
        if run.settled {
            let turn = run.turn(h.into(), turn_id.clone().unwrap_or_default());
            return Ok(Some(RunState::Settled(turn)));
        }
        match alive {
            Some(pid) => Ok(Some(RunState::Running(pid))),
            None if run.user.is_none() => Err(self.reject_startup(receipt_id, None)),
            None => Ok(Some(RunState::Exited)),
        }
    }
}

/// The turn `turn_id` names, from the session file: `None` while nothing on the live branch
/// ends it. Only the live branch is read: a prompt on a branch a later load left behind has
/// no answer of its own there.
fn file_turn(h: &str, entries: &[Entry], turn_id: &str) -> Result<Option<Turn>> {
    let branch = live_branch(entries);
    let start = match entries
        .iter()
        .position(|e| e.line.id.as_deref() == Some(turn_id) && e.line.is_prompt())
    {
        Some(i) if branch.contains(&i) => i,
        Some(_) => {
            return Err(Error::new(
                ErrorCode::Precondition,
                format!(
                    "turn {turn_id} is on a branch of this session the newest entry does not descend from"
                ),
            ));
        }
        None => {
            return Err(Error::new(
                ErrorCode::Precondition,
                format!("turn {turn_id} is not a user message in this session"),
            ));
        }
    };
    // A reply without tool calls or a later prompt closes the turn.
    Ok(turn_end(entries, &branch, start).map(|end| transcript_turn(h.into(), entries, start, end)))
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
        let layout: Check = self.check_layout().map_err(|e| e.message);
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
        self.check_layout()?;
        let want_dir = filter.cwd.map(session::dir_name);
        let mut files: Vec<(SystemTime, String, PathBuf)> = session::files(&self.sessions)
            .filter(|(_, path)| {
                want_dir.as_deref().is_none_or(|d| {
                    path.parent()
                        .and_then(|p| p.file_name())
                        .is_some_and(|n| n == d)
                })
            })
            .map(|(id, path)| {
                let mtime = path.metadata().and_then(|m| m.modified());
                (mtime.unwrap_or(SystemTime::UNIX_EPOCH), id, path)
            })
            .collect();
        // Newest first.
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let offset: usize = match cursor {
            Some(c) => c
                .parse()
                .map_err(|_| Error::new(ErrorCode::Precondition, format!("invalid cursor {c}")))?,
            None => 0,
        };
        let end = offset.saturating_add(limit as usize).min(files.len());
        let next_cursor = (end < files.len()).then(|| end.to_string());
        let mut items = Vec::new();
        for (_, id, path) in files.into_iter().skip(offset).take(limit as usize) {
            let h = handle(&id);
            let owned = self.store.is_owned(&h)?;
            let head = session::head(&path);
            let running = self.own_run(&self.store.processes(&h)?).is_some();
            items.push(Session {
                handle: h,
                agent: "pi",
                cwd: head.cwd,
                name: head.name,
                preview: head.preview,
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
                id,
            });
        }
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, wait: Wait) -> Result<Outcome> {
        let deadline = wait.deadline();
        self.check_layout()?;
        let id = uuid::Uuid::new_v4().to_string();
        let h = handle(&id);
        let _lock = lock(&h, SECOND_WRITER)?;
        // pi has no permission prompts, so full access changes nothing; recorded as given.
        self.store.insert_owned(&h, req.cwd, req.settings)?;
        let args = run::args(
            ["--session-id", &id],
            req.settings.model.as_deref(),
            req.settings.effort.as_deref(),
            req.name,
        );
        self.execute(id, None, req.cwd, &args, &req.message, deadline)
            .await
    }

    async fn send(&self, id: &str, req: &SendRequest<'_>, wait: Wait) -> Result<Outcome> {
        let deadline = wait.deadline();
        check_id(id)?;
        self.check_layout()?;
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
        let cwd = match session::cwd(&path) {
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
        // pi restores the model from the session file, so only a change passes it; the
        // thinking level is not restored and comes from the session's settings.
        let effort = req.settings.and_then(|s| s.effort.as_deref());
        let args = run::args(
            ["--session", &path.display().to_string()],
            req.change.model.as_deref(),
            effort,
            None,
        );
        self.execute(id.into(), Some(path), &cwd, &args, &req.message, deadline)
            .await
    }

    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage> {
        check_id(id)?;
        self.check_layout()?;
        let entries = load(&self.require_file(id)?)?;
        let branch = live_branch(&entries);
        let mut messages = session::messages(&entries, &branch);
        for (_, m) in &mut messages {
            if m.phase == Phase::Prompt {
                m.from = self.store.sender_of(&m.item_id)?;
            }
        }
        let cut = cut(q, q.cursor.as_deref(), messages, entries.len(), false)?;
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
        self.check_layout()?;
        let h = handle(id);
        let (mut turn_id, receipt) = match target {
            WaitTarget::Turn(t) => (Some(t.clone()), self.store.receipt_by_turn(&h, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &h, r)?;
                (rec.turn_id.clone(), Some(rec))
            }
        };
        let receipt_id = receipt.map(|r| r.receipt_id);
        // What the last poll was waiting for, reported at the deadline.
        let mut waiting_for = String::new();
        let work = async {
            loop {
                let file = session::find(&self.sessions, id)?;
                // 1. Own run: while the process lives only its run log counts (the file
                // may hold a failed message pi is about to retry).
                let run = match &receipt_id {
                    Some(r) => self.run_state(&h, r, &mut turn_id, file.as_deref())?,
                    None => None,
                };
                let exited = match run {
                    Some(RunState::Settled(turn)) => return Ok(turn),
                    Some(RunState::Running(pid)) => {
                        waiting_for = format!(
                            "waiting for the agent_settled event of the pi process agent-talk started (pid {pid})"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    Some(RunState::Exited) => true,
                    None => false,
                };
                // 2. The session file.
                let turn = match (&file, turn_id.as_deref()) {
                    (Some(f), Some(t)) => file_turn(&h, &load(f)?, t),
                    (Some(_), None) => Err(Error::new(
                        ErrorCode::Precondition,
                        "the receipt has no turn id and no run log; its outcome cannot be recovered",
                    )),
                    (None, _) => Err(self.missing_file(id)),
                };
                match turn {
                    Ok(Some(turn)) => return Ok(turn),
                    // What the file does not tell of a run that exited is unknown.
                    _ if exited => {
                        let turn_id = turn_id.clone().unwrap_or_default();
                        return Ok(self.runs.exited_turn(
                            h.clone(),
                            turn_id,
                            "agent_settled",
                            None,
                        ));
                    }
                    Err(e) => return Err(e),
                    Ok(None) => {}
                }
                waiting_for = "session file: waiting for an assistant message without tool calls or a later user message after the prompt (best effort; pi records no process liveness)".into();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        let res = bounded(Some(deadline), work)
            .await
            .map(Some)
            .map_err(|e| note_waiting(e, &waiting_for));
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
