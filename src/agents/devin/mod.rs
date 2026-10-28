//! Devin CLI adapter (DESIGN.md §6.7).
//!
//! Every mutation runs one `devin acp` child speaking ACP in the session's working
//! directory. The child holds Devin's session lock until it exits, and a running turn dies
//! with it, so a command observes its turn to the end even without `--wait`, and at a
//! deadline ends it with `session/cancel`. Devin enforces its lock between processes:
//! `session/load` of a session another process holds fails `session_locked`, which
//! agent-talk maps to `E_LOCKED` (its own child) or `E_FOREIGN_LIVE`; agent-talk takes no
//! lock of its own.
//!
//! agent-talk's client message id goes into `session/prompt` as
//! `_meta["cognition.ai/clientMessageId"]`, and Devin stores it with the user node, so it is
//! the turn id. History, turn ends after the command and previews come from `sessions.db`
//! (`db.rs`); listings and liveness from `session/list`.

mod db;
mod watch;

use self::db::{Db, History};
use self::watch::Watch;
use crate::agents::acp::{self, Conn};
use crate::agents::child::{agent_cmd, cli_version, pid_alive};
use crate::agents::history::{ReadPage, ReadQuery, cut, first_line};
use crate::agents::lifecycle::{
    ApprovalPolicy, approval_policy, bounded, note_waiting, reject, reject_unstarted, settle,
    wait_receipt,
};
use crate::agents::{
    Agent, AgentStatus, Check, ListFilter, Operation, SendRequest, Settings, StartRequest, Wait,
    WaitTarget, strip_provenance,
};
use crate::model::{
    self, Error, ErrorCode, Loaded, Model, Observations, Outcome, Page, Progress, ReceiptState,
    Result, Session, SessionState, TurnStatus,
};
use crate::store::{NewIntent, Store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

/// How long a command whose deadline passed keeps the child to end the turn cleanly
/// (`session/cancel`, then the `cancelled` response, which came within 40 ms on 3000.11.228).
const WIND_DOWN: Duration = Duration::from_secs(10);
/// How long the rename after a first turn may take.
const RENAME_TIMEOUT: Duration = Duration::from_secs(10);
/// How often `wait` on a turn no agent-talk child runs asks `session/list` whether a
/// process still holds the session (each ask starts a `devin acp`).
const HOLDER_POLL: Duration = Duration::from_secs(5);

pub struct Devin<'a> {
    store: &'a Store,
    /// `sessions.db`: `$DEVIN_SESSIONS_DIR/sessions.db` (docs), default
    /// `~/.local/share/devin/cli/sessions.db`.
    db_path: PathBuf,
}

fn handle(id: &str) -> String {
    format!("devin:{id}")
}

/// A session as `session/list` describes it.
struct Listed {
    id: String,
    cwd: Option<String>,
    title: Option<String>,
    /// ISO 8601 with a fixed offset, so it sorts as text.
    updated_at: String,
    /// Whether a process holds Devin's lock on the session (an idle TUI does too); `None`
    /// when the listing does not say, which proves nothing either way.
    locked: Option<bool>,
    /// The pid of that process (`null` while unlocked on 3000.11.228).
    holder: Option<u32>,
}

impl Listed {
    /// A row of the list; none without a session id. Other fields that do not fit read as
    /// absent.
    fn new(row: &Value) -> Option<Listed> {
        Some(Listed {
            id: row["sessionId"].as_str()?.to_string(),
            cwd: row["cwd"].as_str().map(String::from),
            title: row["title"].as_str().map(String::from),
            updated_at: row["updatedAt"].as_str().unwrap_or_default().to_string(),
            locked: row["_meta"]["cognition.ai/isLocked"].as_bool(),
            holder: row["_meta"]["cognition.ai/lockHolderPid"]
                .as_u64()
                .and_then(|p| u32::try_from(p).ok()),
        })
    }

    /// Whether `pid` holds the session: `Some(false)` when the listing says it is unlocked
    /// or names another holder, `None` when it does not say. A recorded child's pid alone
    /// may since belong to another process (DESIGN.md §6.7).
    fn held_by(&self, pid: u32) -> Option<bool> {
        match (self.locked?, self.holder) {
            (false, _) => Some(false),
            (true, Some(h)) => Some(h == pid),
            (true, None) => None,
        }
    }
}

/// `session/set_config_option`: `config` (`model`, `thought_level`) of the session, which
/// Devin stores with it (DESIGN.md §6.7). Devin refuses a value outside the option's list.
async fn set_config(conn: &Conn, id: &str, config: &str, value: &str) -> Result<()> {
    let params = json!({"sessionId": id, "configId": config, "value": value});
    match conn.request("session/set_config_option", params).await {
        Ok(_) => Ok(()),
        Err(mut e) => {
            e.message = format!("{config} {value} refused: {}", e.message);
            Err(e)
        }
    }
}

/// Apply `change` to the loaded session; `settings` are the session's settings with the
/// change included. A model change resets Devin's effort to the family's default, so the
/// effort the settings hold is applied after it, changed or not (DESIGN.md §6.7). Full access
/// is the session's `bypass` mode, which Devin stores.
async fn apply(conn: &Conn, id: &str, change: &Settings, settings: &Settings) -> Result<()> {
    if let Some(m) = &change.model {
        set_config(conn, id, "model", m).await?;
    }
    let effort = match change.model {
        Some(_) => settings.effort.as_ref(),
        None => change.effort.as_ref(),
    };
    if let Some(e) = effort {
        set_config(conn, id, "thought_level", e).await?;
    }
    if change.full_access {
        conn.request(
            "session/set_mode",
            json!({"sessionId": id, "modeId": "bypass"}),
        )
        .await
        .map_err(|mut e| {
            e.message = format!("mode bypass refused: {}", e.message);
            e
        })?;
    }
    Ok(())
}

/// The deadline or a signal ended observation. The child dies with this command and would
/// take the turn down with nothing in the db; end it Devin's way instead, which leaves the
/// cancelled tool call in the db when one ran, and answer the permission requests still
/// open `cancelled`, as ACP requires after a cancel (DESIGN.md §4, §6.7).
async fn wind_down(w: &mut Watch<'_>, conn: &mut Conn, id: &str) {
    if w.ended() {
        return;
    }
    let _ = conn
        .notify("session/cancel", json!({"sessionId": id}))
        .await;
    w.cancel(conn).await;
    let _ = tokio::time::timeout(WIND_DOWN, w.finish(conn)).await;
}

impl<'a> Devin<'a> {
    pub fn new(store: &'a Store) -> Self {
        let dir = match std::env::var_os("DEVIN_SESSIONS_DIR") {
            Some(d) => PathBuf::from(d),
            None => std::env::home_dir()
                .unwrap_or_default()
                .join(".local/share/devin/cli"),
        };
        Devin {
            store,
            db_path: dir.join("sessions.db"),
        }
    }

    fn db(&self) -> Result<Db> {
        if !self.db_path.is_file() {
            return Err(Error::new(
                ErrorCode::Precondition,
                format!(
                    "{} does not exist; Devin has stored no session",
                    self.db_path.display()
                ),
            ));
        }
        Db::open(&self.db_path)
    }

    /// The session's row; an id Devin does not know is refused.
    fn session_row(&self, db: &Db, id: &str) -> Result<db::Row> {
        db.session(id)?.ok_or_else(|| {
            Error::new(
                ErrorCode::Precondition,
                format!(
                    "unknown Devin session {id} (not in {})",
                    self.db_path.display()
                ),
            )
        })
    }

    /// A `devin acp` child in `cwd`, initialized.
    async fn connect(&self, cwd: &Path) -> Result<Conn> {
        let mut cmd = agent_cmd("devin");
        cmd.arg("acp").current_dir(cwd);
        acp::spawn(cmd, "devin acp").await
    }

    /// `session/list`, followed through `nextCursor`; `cwd` narrows it to one directory.
    async fn listed(&self, cwd: Option<&str>) -> Result<Vec<Listed>> {
        let conn = self.connect(&std::env::temp_dir()).await?;
        let res = async {
            let mut rows = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let mut params = json!({});
                if let Some(c) = cwd {
                    params["cwd"] = json!(c);
                }
                if let Some(c) = &cursor {
                    params["cursor"] = json!(c);
                }
                let v = conn.request("session/list", params).await?;
                rows.extend(
                    v["sessions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Listed::new),
                );
                match v["nextCursor"].as_str() {
                    Some(c) if cursor.as_deref() != Some(c) => cursor = Some(c.to_string()),
                    _ => return Ok(rows),
                }
            }
        }
        .await;
        conn.close().await;
        res
    }

    /// The session's `session/list` entry; `None` when it is not listed.
    async fn entry(&self, id: &str) -> Result<Option<Listed>> {
        Ok(self.listed(None).await?.into_iter().find(|l| l.id == id))
    }

    /// Whether the listing names a child agent-talk recorded for the session as the holder
    /// of its lock, with that child's turn not ended in the chain yet.
    fn own_run(&self, l: &Listed, history: &History) -> Result<bool> {
        for (receipt_id, pid) in self.store.processes(&handle(&l.id))? {
            if l.held_by(pid) != Some(true) {
                continue;
            }
            let ended = self.store.receipt(&receipt_id)?.is_some_and(|r| {
                history
                    .find(&r.client_msg_id)
                    .is_some_and(|s| history.spans[s].end.is_some())
            });
            return Ok(!ended);
        }
        Ok(false)
    }

    /// `e`, a refused `session/load`, as agent-talk's error: Devin's `session_locked` names
    /// the holder's pid, which is `E_LOCKED` when it is a child agent-talk recorded for the
    /// session and `E_FOREIGN_LIVE` otherwise (DESIGN.md §6.7). The agent's error is kept.
    fn locked(&self, id: &str, mut e: Error) -> Error {
        let data = e
            .agent_error
            .as_ref()
            .and_then(|a| a.data.clone())
            .unwrap_or_default();
        if data["cognition.ai/errorKind"] != "session_locked" {
            return e;
        }
        let h = handle(id);
        let pid = data["cognition.ai/lockHolderPid"].as_u64();
        let ours = pid.is_some_and(|p| {
            self.store
                .processes(&h)
                .is_ok_and(|procs| procs.iter().any(|(_, q)| u64::from(*q) == p))
        });
        let holder = pid.map_or_else(|| "a process".into(), |p| format!("pid {p}"));
        (e.code, e.message) = if ours {
            (
                ErrorCode::Locked,
                format!(
                    "a devin acp started by another agent-talk command ({holder}) still holds {h}; Devin lets one process open a session at a time"
                ),
            )
        } else {
            (
                ErrorCode::ForeignLive,
                format!(
                    "{h} is open in another process ({holder}: a Devin TUI, devin -p or another ACP client); Devin lets one process open a session at a time, so agent-talk only reads it until that process exits"
                ),
            )
        };
        e
    }

    /// Record the intent, submit its text as a prompt on the open session and observe it
    /// until it ends or `wait`'s deadline passes, then settle the receipt. Without a deadline
    /// the outcome goes to `wait`'s keeper, if any, once the prompt is accepted. `name` is set
    /// once the turn is over, since Devin's automatic title after the first prompt would
    /// override an earlier rename (DESIGN.md §6.7). The caller closes `conn`.
    async fn run(
        &self,
        conn: &mut Conn,
        id: &str,
        intent: &NewIntent<'_>,
        policy: ApprovalPolicy,
        wait: Wait,
        name: Option<&str>,
    ) -> Result<Outcome> {
        self.store.insert_intent(intent)?;
        // The child holds the session until it exits; a later command tells it from a
        // foreign holder by its pid.
        if let Some(pid) = conn.pid() {
            self.store
                .insert_process(intent.receipt_id, intent.handle, pid)?;
        }
        let response = conn
            .submit(
                "session/prompt",
                json!({
                    "sessionId": id,
                    "prompt": [{"type": "text", "text": intent.message.delivered}],
                    "_meta": {"cognition.ai/clientMessageId": intent.client_msg_id},
                }),
            )
            .await
            .map_err(|e| reject_unstarted(self.store, intent.receipt_id, e))?;
        let mut w = Watch::new(
            self.store,
            id,
            intent.receipt_id,
            intent.client_msg_id,
            response,
            policy,
        );
        // The turn needs this connection until it ends, so without a deadline it is still
        // observed to its end, and no turn is reported; only a signal ends it early. A keeper
        // (the MCP server) gets the outcome at acceptance.
        let deadline = wait.deadline();
        let observe = async {
            if let Wait::Accepted(Some(keeper)) = wait {
                if let Some(end) = w.until_accepted(conn).await? {
                    return Ok(end);
                }
                match settle(
                    self.store,
                    handle(id),
                    Ok(None),
                    Some(intent.receipt_id),
                    w.approvals.clone(),
                    w.progress(),
                ) {
                    Ok(o) => {
                        let _ = keeper.send(o);
                    }
                    Err(e) => tracing::warn!("{e}"),
                }
            }
            w.finish(conn).await
        };
        let mut res = bounded(deadline, observe).await;
        if let Err(e) = &mut res
            && matches!(e.code, ErrorCode::Timeout | ErrorCode::Interrupted)
        {
            wind_down(&mut w, conn, id).await;
            e.message.push_str("; no Devin process outlives the command, so agent-talk ended the turn with session/cancel unless it finished first; wait --receipt reads how it ended from sessions.db");
        }
        let note = match name {
            Some(n) => self.rename(conn, id, n).await,
            None => None,
        };
        let h = handle(id);
        let turn_id = w.turn_id().to_string();
        let res = match res {
            Ok(Ok(v)) => Ok(w.turn(h.clone(), &v, note.as_deref())),
            // A refusal of the prompt itself; an error after acceptance is a failed turn,
            // except a lost connection, whose outcome is unknown (DESIGN.md §4).
            Ok(Err(e)) if !w.accepted() => Err(reject(self.store, intent.receipt_id, e)),
            Ok(Err(e)) if e.code == ErrorCode::Transport => Err(e),
            Ok(Err(e)) => Ok(model::Turn {
                handle: h.clone(),
                turn_id,
                status: TurnStatus::Failed,
                error: Some(json!({"message": e.message, "agent_error": e.agent_error})),
                final_text: w.final_text(),
                duration_ms: None,
                basis: Some(match &note {
                    Some(n) => format!("session/prompt error response; {n}"),
                    None => "session/prompt error response".into(),
                }),
            }),
            Err(mut e) => {
                if let Some(n) = &note {
                    e.message = format!("{}; {n}", e.message);
                }
                Err(e)
            }
        };
        let progress = w.progress();
        settle(
            self.store,
            h,
            res.map(|t| deadline.is_some().then_some(t)),
            Some(intent.receipt_id),
            w.approvals,
            progress,
        )
    }

    /// `_cognition.ai/session/rename`; a failure is a note, not a failed turn.
    async fn rename(&self, conn: &Conn, id: &str, name: &str) -> Option<String> {
        let params = json!({"sessionId": id, "title": name});
        let res = tokio::time::timeout(
            RENAME_TIMEOUT,
            conn.request("_cognition.ai/session/rename", params),
        )
        .await;
        let e = match res {
            Ok(Ok(_)) => return None,
            Ok(Err(e)) => e.message,
            Err(_) => format!("no response within {}s", RENAME_TIMEOUT.as_secs()),
        };
        let note = format!("the session was not named {name:?}: {e}");
        tracing::warn!("{note}");
        Some(note)
    }

    /// An `ls` row.
    fn session(&self, db: Option<&Db>, l: Listed) -> Result<Session> {
        let h = handle(&l.id);
        let owned = self.store.is_owned(&h)?;
        let history = match db {
            Some(db) => Some(History::new(db.chain(&l.id)?)),
            None => None,
        };
        let running = match &history {
            Some(history) => self.own_run(&l, history)?,
            None => false,
        };
        let (state, loaded) = match (running, l.locked) {
            (true, _) => (SessionState::Running, Loaded::Yes),
            (false, Some(true)) => (SessionState::Unknown, Loaded::Yes),
            (false, Some(false)) => (SessionState::Idle, Loaded::No),
            (false, None) => (SessionState::Unknown, Loaded::Unknown),
        };
        Ok(Session {
            handle: h,
            agent: "devin",
            cwd: l.cwd,
            name: l.title,
            preview: history
                .as_ref()
                .and_then(|h| h.first_prompt())
                .map(|p| first_line(strip_provenance(p), 200)),
            observations: Observations {
                history: model::History::Visible,
                loaded,
                origin: if owned { "agent-talk" } else { "devin" }.into(),
            },
            state,
            owned,
            id: l.id,
        })
    }
}

impl Agent for Devin<'_> {
    async fn models(&self) -> Result<Vec<Model>> {
        // Only a session's `configOptions` name the ids the `model` option accepts (one
        // variant per family; `devin models list` cannot tell which). `session/new` without
        // a prompt writes no session row, only a lock file (DESIGN.md §6.7).
        let dir = std::env::temp_dir();
        let conn = self.connect(&dir).await?;
        let created = conn
            .request("session/new", json!({"cwd": dir, "mcpServers": []}))
            .await;
        conn.close().await;
        let models = created?["configOptions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| o["id"] == "model")
            .flat_map(|o| o["options"].as_array().into_iter().flatten())
            .filter_map(|o| {
                Some(Model {
                    id: o["value"].as_str()?.to_string(),
                    // `thought_level` lists the efforts of the session's current model only.
                    efforts: Vec::new(),
                })
            })
            .collect();
        Ok(models)
    }

    async fn status(&self) -> AgentStatus {
        let version = cli_version("devin").await;
        let cli: Check = version.as_ref().map(|_| ()).map_err(String::clone);
        let store: Check = if self.db_path.is_file() {
            Ok(())
        } else {
            Err(format!("{} does not exist", self.db_path.display()))
        };
        AgentStatus {
            agent: "devin",
            version: version.ok(),
            shared: None,
            operations: vec![
                Operation::new("ls", &cli),
                Operation::new("new", &cli),
                Operation::new("send", &cli.clone().and(store.clone())),
                Operation::new("read", &store),
                Operation::new("wait", &store),
                Operation::new(
                    "steer",
                    &Err("Devin joins a prompt into a running turn only on the connection that runs it, and its session lock keeps every other process out".into()),
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
        let mut rows = self.listed(filter.cwd).await?;
        rows.retain(|l| filter.cwd.is_none() || l.cwd.as_deref() == filter.cwd);
        // Newest first, ties by id; the cursor is `<updatedAt>|<id>` of the last row of the
        // previous page.
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        if let Some(c) = cursor {
            let (at, id) = c.split_once('|').ok_or_else(|| {
                Error::new(ErrorCode::Precondition, format!("invalid cursor {c}"))
            })?;
            rows.retain(|l| {
                l.updated_at.as_str() < at || (l.updated_at == at && l.id.as_str() > id)
            });
        }
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next_cursor = more
            .then(|| rows.last().map(|l| format!("{}|{}", l.updated_at, l.id)))
            .flatten();
        let db = if rows.is_empty() || !self.db_path.is_file() {
            None
        } else {
            Some(self.db()?)
        };
        let items = rows
            .into_iter()
            .map(|l| self.session(db.as_ref(), l))
            .collect::<Result<_>>()?;
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, wait: Wait) -> Result<Outcome> {
        let mut conn = self.connect(Path::new(req.cwd)).await?;
        let res = async {
            let created = conn
                .request("session/new", json!({"cwd": req.cwd, "mcpServers": []}))
                .await?;
            let id = created["sessionId"].as_str().ok_or_else(|| {
                Error::new(
                    ErrorCode::Transport,
                    format!("session/new returned no sessionId: {created}"),
                )
            })?;
            let h = handle(id);
            // Nothing else lists the session before its first prompt.
            self.store.insert_owned(&h, req.cwd, req.settings)?;
            // Set on the session, never as `devin acp --model`, which matches names loosely
            // and ignores an unknown one (DESIGN.md §6.7).
            apply(&conn, id, req.settings, req.settings)
                .await
                .map_err(|mut e| {
                    e.message = format!(
                        "{}; Devin writes the session {id} at its first prompt, so only its lock file exists",
                        e.message
                    );
                    e
                })?;
            let receipt_id = uuid::Uuid::new_v4().to_string();
            // The client message id Devin stores with the user node: the turn id.
            let client_msg_id = uuid::Uuid::new_v4().to_string();
            let intent = NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &client_msg_id,
                message: &req.message,
            };
            self.run(&mut conn, id, &intent, ApprovalPolicy::Deny, wait, req.name)
                .await
        }
        .await;
        conn.close().await;
        res
    }

    async fn send(&self, id: &str, req: &SendRequest<'_>, wait: Wait) -> Result<Outcome> {
        let h = handle(id);
        if req.steer {
            return Err(Error::new(
                ErrorCode::NoSteer,
                "Devin joins a prompt into a running turn only on the connection that runs it, and its session lock keeps every other process out; send without --steer",
            ));
        }
        let row = self.session_row(&self.db()?, id)?;
        if !Path::new(&row.cwd).is_dir() {
            return Err(Error::new(
                ErrorCode::Precondition,
                format!(
                    "the session's directory {} no longer exists; session/load would run it elsewhere",
                    row.cwd
                ),
            ));
        }
        let policy = approval_policy(self.store, &h)?;
        let owned = self.store.is_owned(&h)?;
        let mut conn = self.connect(Path::new(&row.cwd)).await?;
        let res = async {
            let params = json!({"sessionId": id, "cwd": row.cwd, "mcpServers": []});
            let loaded = match conn.request("session/load", params).await {
                Ok(v) => v,
                Err(e) => return Err(self.locked(id, e)),
            };
            // The history replay, which arrives before the response.
            conn.discard_notifications();
            // ACP offers no `normal` mode: a session stored in it loads as `accept-edits`,
            // and one prompt would store that (DESIGN.md §6.7). A session agent-talk did not
            // start runs only in the mode Devin stored for it, read again now that the lock
            // excludes every other writer.
            let mode = loaded["modes"]["currentModeId"].as_str();
            let stored = self.session_row(&self.db()?, id)?.agent_mode;
            if !owned && mode != Some(stored.as_str()) {
                return Err(Error::new(
                    ErrorCode::Unsupported,
                    format!(
                        "{h} is stored in permission mode {stored}, but Devin's ACP interface opens it in {}; a prompt would change the user's session to that mode, so agent-talk only reads it",
                        mode.unwrap_or("no reported mode")
                    ),
                ));
            }
            // Devin keeps model, effort and mode with the session, so only a change is
            // applied. A refusal fails the send before anything is recorded; a value applied
            // before it stays.
            if let Some(s) = req.settings {
                apply(&conn, id, req.change, s).await?;
            }
            let receipt_id = uuid::Uuid::new_v4().to_string();
            let client_msg_id = uuid::Uuid::new_v4().to_string();
            let intent = NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &client_msg_id,
                message: &req.message,
            };
            self.run(&mut conn, id, &intent, policy, wait, None).await
        }
        .await;
        conn.close().await;
        res
    }

    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage> {
        let db = self.db()?;
        self.session_row(&db, id)?;
        let history = History::new(db.chain(id)?);
        let mut messages = history.messages();
        for (_, m) in &mut messages {
            if m.phase == model::Phase::Prompt {
                m.from = self.store.sender_of(&m.turn_id)?;
            }
        }
        let cut = cut(q, q.cursor.as_deref(), messages, history.nodes.len(), false)?;
        Ok(ReadPage {
            messages: Page {
                next_cursor: cut.cursor_item().map(|(_, m)| m.item_id.clone()),
                items: cut.messages.into_iter().map(|(_, m)| m).collect(),
            },
            // Every node in the span (system and tool nodes included).
            raw: history.nodes[cut.records].to_vec(),
        })
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        let h = handle(id);
        let (turn_id, receipt) = match target {
            WaitTarget::Turn(t) => (t.clone(), self.store.receipt_by_turn(&h, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &h, r)?;
                // The client message id, recorded before submission.
                (
                    rec.turn_id
                        .clone()
                        .unwrap_or_else(|| rec.client_msg_id.clone()),
                    Some(rec),
                )
            }
        };
        let db = self.db()?;
        self.session_row(&db, id)?;
        let mut unaccepted = receipt
            .as_ref()
            .is_some_and(|r| matches!(r.state, ReceiptState::Pending | ReceiptState::Unknown));
        // The child that ran the receipt's prompt; once it is gone nothing will write the
        // turn's end.
        let child = match &receipt {
            Some(r) => self.store.process(&r.receipt_id)?,
            None => None,
        };
        let work = async {
            let mut next_holder_check = Instant::now();
            // Why nothing will write the end, found on the previous round; the chain is read
            // once more after it, since the writer may have finished in between.
            let mut gone: Option<String> = None;
            loop {
                let history = History::new(db.chain(id)?);
                let span = history.find(&turn_id);
                if let (Some(_), Some(r)) = (span, &receipt)
                    && unaccepted
                {
                    self.store
                        .accept(&r.receipt_id, None, Some(&turn_id), None)?;
                    unaccepted = false;
                }
                if let Some(s) = span
                    && let Some(t) = history.turn(h.clone(), s)
                {
                    return Ok(t);
                }
                if span.is_none() && receipt.is_none() {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!("turn {turn_id} is not in the conversation of {h}"),
                    ));
                }
                if let Some(gone) = gone {
                    // agent-talk's own child died or was ended with the turn: interrupted.
                    // Nobody holding a foreign turn proves only that it will not go on: a
                    // killed turn and one that ended without a final reply (a `-p` run after
                    // a rejected permission) leave the same chain (DESIGN.md §6.7).
                    let (status, message) = match (child, span) {
                        (Some(_), Some(_)) => (
                            TurnStatus::Interrupted,
                            "the turn started but has no end in sessions.db; a turn killed with its process leaves no marker",
                        ),
                        (Some(_), None) => (
                            TurnStatus::Unknown,
                            "sessions.db holds no user message for this prompt",
                        ),
                        (None, _) => (
                            TurnStatus::Unknown,
                            "the turn has no end in sessions.db: a turn killed with its process and one that ended without a final reply leave the same chain",
                        ),
                    };
                    return Ok(model::Turn {
                        handle: h.clone(),
                        turn_id: turn_id.clone(),
                        status,
                        error: Some(json!({"message": message})),
                        final_text: None,
                        duration_ms: None,
                        basis: Some(format!(
                            "sessions.db; {gone}, and a Devin turn dies with its process"
                        )),
                    });
                }
                // Proof that nothing will write the end: the recorded child gone or, its pid
                // being possibly reused, no longer holding the session; for a turn no
                // agent-talk child runs, no process holding it. A listing that does not say
                // proves nothing.
                gone = match child {
                    Some(pid) if !pid_alive(pid) => Some(format!(
                        "the devin acp agent-talk started for this prompt (pid {pid}) has exited"
                    )),
                    _ if Instant::now() < next_holder_check => None,
                    _ => {
                        next_holder_check = Instant::now() + HOLDER_POLL;
                        let entry = self.entry(id).await?;
                        let released = match child {
                            Some(pid) => entry.and_then(|l| l.held_by(pid)) == Some(false),
                            None => entry.and_then(|l| l.locked) == Some(false),
                        };
                        released.then(|| match child {
                            Some(pid) => format!(
                                "the devin acp agent-talk started for this prompt (pid {pid}) no longer holds the session (session/list)"
                            ),
                            None => "no process holds the session (session/list)".to_string(),
                        })
                    }
                };
                if gone.is_none() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        let res = bounded(Some(deadline), work).await.map(Some).map_err(|e| {
            note_waiting(
                e,
                &format!("waiting for the end of {turn_id} in sessions.db (polled every second)"),
            )
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentError;

    /// A store that recorded child `pid` for a prompt on `devin:swanky-towel`.
    fn store_with_child(pid: u32) -> Store {
        let store = Store::memory();
        let message = crate::agents::Outgoing {
            text: "x",
            delivered: "x",
            from: &crate::model::Caller::UNKNOWN,
            depth: 0,
        };
        store
            .insert_intent(&NewIntent {
                receipt_id: "r",
                handle: "devin:swanky-towel",
                client_msg_id: "c",
                message: &message,
            })
            .unwrap();
        store
            .insert_process("r", "devin:swanky-towel", pid)
            .unwrap();
        store
    }

    /// `session_locked` names the holder: agent-talk's own recorded child is `E_LOCKED`,
    /// any other process `E_FOREIGN_LIVE`; other refusals pass through. The agent's error is
    /// kept either way.
    #[test]
    fn session_locked_maps_by_holder() {
        let store = store_with_child(4242);
        let devin = Devin::new(&store);
        let refusal = |data: Value| {
            Error::from_agent(
                ErrorCode::Precondition,
                AgentError {
                    code: -32015,
                    message: "Session 'swanky-towel' is already open in another process".into(),
                    data: Some(data),
                },
            )
        };
        let locked = |pid: u64| {
            refusal(
                json!({"cognition.ai/errorKind": "session_locked", "cognition.ai/retryable": true, "cognition.ai/lockHolderPid": pid}),
            )
        };
        let e = devin.locked("swanky-towel", locked(4242));
        assert_eq!(e.code, ErrorCode::Locked);
        assert!(e.agent_error.is_some());
        let e = devin.locked("swanky-towel", locked(77));
        assert_eq!(e.code, ErrorCode::ForeignLive);
        assert!(e.message.contains("pid 77"));
        let e = devin.locked(
            "swanky-towel",
            refusal(json!({"cognition.ai/errorKind": "session_not_found"})),
        );
        assert_eq!(e.code, ErrorCode::Precondition);
    }

    /// A listed session without an id is skipped; other odd fields read as absent.
    #[test]
    fn odd_listed_fields_keep_the_row() {
        let row = |s: &str| Listed::new(&serde_json::from_str::<Value>(s).unwrap());
        let l = row(r#"{"sessionId":"a-b","cwd":7,"updatedAt":null,"_meta":{"cognition.ai/isLocked":"yes"}}"#).unwrap();
        assert!(l.id == "a-b" && l.cwd.is_none() && l.updated_at.is_empty() && l.locked.is_none());
        let l = row(r#"{"sessionId":"a-b","_meta":{"cognition.ai/isLocked":true}}"#).unwrap();
        assert_eq!((l.locked, l.holder), (Some(true), None));
        assert!(row(r#"{"cwd":"/x"}"#).is_none());
    }

    /// Only the listed holder makes a recorded child's unended turn agent-talk's running
    /// one, and only the listing proves the child gone while its pid is alive: the pid may
    /// have been reused (this test's own pid stands for it).
    #[test]
    fn the_listed_holder_decides() {
        let pid = std::process::id();
        let store = store_with_child(pid);
        let devin = Devin::new(&store);
        let history = History::new(Vec::new());
        let listed = |meta: Value| {
            Listed::new(&json!({"sessionId": "swanky-towel", "_meta": meta})).unwrap()
        };
        let ours =
            listed(json!({"cognition.ai/isLocked": true, "cognition.ai/lockHolderPid": pid}));
        let other = listed(json!({"cognition.ai/isLocked": true, "cognition.ai/lockHolderPid": 1}));
        let free =
            listed(json!({"cognition.ai/isLocked": false, "cognition.ai/lockHolderPid": null}));
        let unsaid = listed(json!({"cognition.ai/isLocked": true}));
        assert!(devin.own_run(&ours, &history).unwrap());
        for l in [&other, &free, &unsaid] {
            assert!(!devin.own_run(l, &history).unwrap());
        }
        assert_eq!(
            [&ours, &other, &free, &unsaid].map(|l| l.held_by(pid)),
            [Some(true), Some(false), Some(false), None]
        );
    }
}
