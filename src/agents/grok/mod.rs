//! Grok CLI adapter (DESIGN.md §6.4).
//!
//! Every mutation runs one `grok agent … stdio` child speaking ACP. When the leader
//! socket (`$GROK_LEADER_SOCKET`, default `<GROK_HOME>/leader.sock`) answers a connect,
//! the child is `grok agent --leader stdio`, a proxy into the user's shared leader,
//! where sessions behave like Codex threads in the daemon; otherwise it is `grok agent
//! --no-leader stdio`, an in-process agent that holds the session only while the
//! command runs (closing its stdin ends it within about two seconds, and a running
//! turn dies with it). `--leader` is never passed without a live socket: it spawns a
//! persistent leader when none listens.
//!
//! agent-talk's `promptId` (the intent's client message id) becomes the persisted
//! `turn_completed.prompt_id`, so it is the turn id. History, turn ends after the
//! command and listings come from the session store (`sessions.rs`, `updates.rs`).

mod acp;
mod sessions;
mod updates;
mod watch;

use self::acp::{Conn, Spawn};
use self::sessions::{Active, Summary};
use self::updates::{History, first_prompt};
use self::watch::{End, Watch};
use crate::agents::child::{cli_version, lock, pid_alive};
use crate::agents::history::{ReadPage, ReadQuery, cut, first_line};
use crate::agents::lifecycle::{
    ApprovalPolicy, approval_policy, bounded, note_waiting, reject, settle, wait_receipt,
};
use crate::agents::{
    Agent, AgentStatus, Check, ListFilter, Operation, SendRequest, StartRequest, WaitTarget,
    strip_provenance,
};
use crate::model::{
    self, Error, ErrorCode, Loaded, Model, Observations, Outcome, Page, Progress, ReceiptState,
    Result, Session, SessionState, TurnStatus,
};
use crate::store::{NewIntent, Store};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

/// Why agent-talk refuses a second writer (DESIGN.md §6.4).
const SECOND_WRITER: &str = "Grok has no lock between processes: a second writer marks the running turn interrupted and the two processes' memories of the session diverge";
/// How long a command whose deadline passed keeps the direct-mode child to end the
/// turn cleanly (wait for it to start, `session/cancel`, wait for its response).
const WIND_DOWN: Duration = Duration::from_secs(10);

pub struct Grok<'a> {
    store: &'a Store,
    /// `$GROK_HOME`, default `~/.grok`.
    home: PathBuf,
    /// `$GROK_LEADER_SOCKET`, passed on as `--leader-socket`.
    leader_socket: Option<PathBuf>,
}

fn handle(id: &str) -> String {
    format!("grok:{id}")
}

/// Session ids are UUIDs; anything else is refused before it reaches a path.
fn check_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map(|_| ()).map_err(|_| {
        Error::new(
            ErrorCode::Precondition,
            format!("invalid Grok session id {id}; expected a UUID"),
        )
    })
}

/// `params` with `_meta.yoloMode: true` when the session gets full access: on `session/new`
/// and on a direct-mode `session/load`, for this process's session only (DESIGN.md §6.4).
fn with_yolo(mut params: Value, full_access: bool) -> Value {
    if full_access {
        params["_meta"] = json!({"yoloMode": true});
    }
    params
}

/// `session/set_config_option`: `config` (`model`, `reasoning_effort`) of the session,
/// which Grok stores in `summary.json` at once (DESIGN.md §6.4).
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

/// A field that is missing or does not fit reads as its default, so a non-essential field
/// never drops the record it is part of. For
/// `#[serde(default, deserialize_with = "or_default")]`.
fn or_default<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> std::result::Result<T, D::Error> {
    Ok(T::deserialize(Value::deserialize(d)?).unwrap_or_default())
}

/// One entry of the `initialize` response's `_meta.modelState.availableModels`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvailableModel {
    #[serde(default)]
    model_id: String,
    #[serde(rename = "_meta", default, deserialize_with = "or_default")]
    meta: ModelMeta,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelMeta {
    #[serde(default, deserialize_with = "or_default")]
    reasoning_efforts: Vec<Effort>,
}

#[derive(Deserialize)]
struct Effort {
    #[serde(default, deserialize_with = "or_default")]
    value: Option<String>,
}

/// A session as `_x.ai/sessions/list` describes it. `resident` and `activity` describe
/// the process that answers, so they mean something only through a leader (DESIGN.md
/// §6.4).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    session_id: String,
    #[serde(default, deserialize_with = "or_default")]
    resident: bool,
    #[serde(default, deserialize_with = "or_default")]
    activity: Activity,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Activity {
    Working,
    Idle,
    Dormant,
    #[default]
    #[serde(other)]
    Unknown,
}

impl Activity {
    fn as_str(self) -> &'static str {
        match self {
            Activity::Working => "working",
            Activity::Idle => "idle",
            Activity::Dormant => "dormant",
            Activity::Unknown => "unknown",
        }
    }
}

/// The leader as seen at the start of this command: its socket answered a connect.
struct Leader {
    /// The pid in `leader.lock` at the probe, to report a leader Grok spawned since.
    pid: Option<u32>,
}

/// How this command reaches a session (DESIGN.md §6.4).
enum Route {
    /// Through the live leader, which holds the session; clients of one leader do not
    /// exclude each other.
    Leader(Leader),
    /// A `--no-leader` child that holds the session while the command runs, under
    /// agent-talk's lock.
    Direct { _lock: File },
}

impl<'a> Grok<'a> {
    pub fn new(store: &'a Store) -> Self {
        let home_dir = std::env::home_dir().unwrap_or_default();
        let home = std::env::var_os("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".grok"));
        Grok {
            store,
            home,
            leader_socket: std::env::var_os("GROK_LEADER_SOCKET").map(PathBuf::from),
        }
    }

    /// `$GROK_LEADER_SOCKET`, default `<home>/leader.sock`.
    fn socket(&self) -> PathBuf {
        self.leader_socket
            .clone()
            .unwrap_or_else(|| self.home.join("leader.sock"))
    }

    /// The pid in the leader's lock file, which sits next to its socket (`leader.sock`,
    /// `leader.lock`; a custom socket `x.sock` has `x.lock`).
    fn leader_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.socket().with_extension("lock"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Connect to the leader socket. Only a missing socket, a refused connection (a
    /// stale socket file, e.g. after the leader died) or a path too long for a Unix
    /// socket (macOS: 104 bytes; Grok cannot listen there either, DESIGN.md §6.4)
    /// means "no leader".
    fn leader(&self) -> Result<Option<Leader>> {
        let pid = self.leader_pid();
        let socket = self.socket();
        match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(_) => Ok(Some(Leader { pid })),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::InvalidInput
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(Error::new(
                ErrorCode::Transport,
                format!(
                    "cannot probe the Grok leader socket {}: {e}. A sandbox that blocks local network access fails this way; agent-talk needs to reach the socket to tell whether a leader runs",
                    socket.display()
                ),
            )),
        }
    }

    /// A note when `leader.lock` names another pid than at the probe: the leader that
    /// answered exited in between and the `--leader` child spawned a new one.
    fn leader_change(&self, leader: &Leader) -> Option<String> {
        let before = leader.pid;
        let after = self.leader_pid();
        (before != after).then(|| {
            let note = format!(
                "the Grok leader pid changed during this command ({before:?} -> {after:?}): the leader that answered the probe exited and grok agent --leader spawned a new one, which keeps running"
            );
            tracing::warn!("{note}");
            note
        })
    }

    /// A `grok agent` child in `cwd`, initialized: `--leader` (only after `leader()`
    /// found a live socket) or `--no-leader`.
    async fn connect(&self, cwd: &Path, leader: bool, model: Option<&str>) -> Result<Conn> {
        let opts = Spawn {
            leader,
            leader_socket: self.leader_socket.as_deref(),
            model,
        };
        acp::spawn(cwd, &opts).await
    }

    /// `_x.ai/sessions/list`: every session the answering process knows.
    async fn listed(&self, conn: &Conn) -> Result<Vec<Listed>> {
        let v = conn.request("_x.ai/sessions/list", json!({})).await?;
        // The sessions sit under a nested `result` (seen in direct and leader mode, grok
        // 1.0.46).
        Ok(v["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| Listed::deserialize(s).ok())
            .collect())
    }

    /// The leader's own view of the sessions, when one runs; what cannot be read is
    /// left out with a warning.
    async fn leader_sessions(&self) -> Vec<Listed> {
        match self.leader() {
            Ok(Some(_)) => {}
            Ok(None) => return Vec::new(),
            Err(e) => {
                tracing::warn!("{e}");
                return Vec::new();
            }
        }
        // The client process needs some cwd; `_x.ai/sessions/list` covers all.
        let conn = match self.connect(&std::env::temp_dir(), true, None).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("leader client: {e}");
                return Vec::new();
            }
        };
        let listed = self.listed(&conn).await.unwrap_or_else(|e| {
            tracing::warn!("leader sessions/list: {e}");
            Vec::new()
        });
        conn.close().await;
        listed
    }

    /// A recorded child of the session that is still alive and whose turn has
    /// no `turn_completed` yet in the session directory `dir` (a pid alone may have been
    /// reused).
    fn own_run(&self, id: &str, dir: &Path) -> Result<Option<u32>> {
        let alive: Vec<_> = self
            .store
            .processes(&handle(id))?
            .into_iter()
            .filter(|(_, pid)| pid_alive(*pid))
            .collect();
        if alive.is_empty() {
            return Ok(None);
        }
        let history = History::new(updates::load(&dir.join("updates.jsonl"))?);
        for (receipt_id, pid) in alive {
            let ended = self.store.receipt(&receipt_id)?.is_some_and(|r| {
                history
                    .find(&r.client_msg_id)
                    .is_some_and(|s| history.spans[s].end.is_some())
            });
            if !ended {
                return Ok(Some(pid));
            }
        }
        Ok(None)
    }

    /// Refuse a direct-mode write while another process holds the session: a TUI
    /// listed in `active_sessions.json` (no leader lists the session resident, or the
    /// leader path would have been taken), or a grok agent another agent-talk command
    /// started.
    fn refuse_live(&self, id: &str, dir: &Path) -> Result<()> {
        if let Some(a) = sessions::active(&self.home)?
            .into_iter()
            .find(|a| a.session_id == id && pid_alive(a.pid))
        {
            return Err(Error::new(
                ErrorCode::ForeignLive,
                format!(
                    "{} is open in a Grok TUI (pid {}, active_sessions.json) that runs it in process, outside any leader; {SECOND_WRITER}, so agent-talk only reads this session. Exit the TUI, or run it with --leader so agent-talk can join it through the leader",
                    handle(id),
                    a.pid
                ),
            ));
        }
        if let Some(pid) = self.own_run(id, dir)? {
            return Err(Error::new(
                ErrorCode::Locked,
                format!(
                    "a grok agent started by another agent-talk command (pid {pid}) is still running {}; {SECOND_WRITER}",
                    handle(id)
                ),
            ));
        }
        Ok(())
    }

    /// How a queued send reaches the session, and the connection: through the leader
    /// only when the leader lists the session resident (a listed but dormant session may
    /// be held in process by a TUI); otherwise directly, under agent-talk's lock and only
    /// while no other process holds the session.
    async fn route(&self, id: &str, dir: &Path, cwd: &Path) -> Result<(Route, Conn)> {
        if let Some(leader) = self.leader()? {
            let conn = self.connect(cwd, true, None).await?;
            let resident = self.listed(&conn).await.map(|l| {
                l.iter()
                    .find(|s| s.session_id == id)
                    .is_some_and(|s| s.resident)
            });
            match resident {
                Ok(true) => return Ok((Route::Leader(leader), conn)),
                Ok(false) => conn.close().await,
                Err(e) => {
                    conn.close().await;
                    return Err(e);
                }
            }
        }
        let lock = lock(&handle(id), SECOND_WRITER)?;
        self.refuse_live(id, dir)?;
        let conn = self.connect(cwd, false, None).await?;
        Ok((Route::Direct { _lock: lock }, conn))
    }

    /// The session's working directory, from `summary.json`; it must still exist.
    fn session_cwd(&self, dir: &Path) -> Result<String> {
        match sessions::summary(dir)?.info.cwd {
            Some(c) if Path::new(&c).is_dir() => Ok(c),
            Some(c) => Err(Error::new(
                ErrorCode::Precondition,
                format!(
                    "the session's directory {c} no longer exists; session/load would run it elsewhere"
                ),
            )),
            None => Err(Error::new(
                ErrorCode::Precondition,
                "summary.json records no working directory for this session",
            )),
        }
    }

    /// Record the intent, submit its text as a prompt on the loaded session and observe
    /// it (until the deadline, or to the end of the turn without one), then settle the
    /// receipt. The caller closes `conn`.
    async fn run(
        &self,
        conn: &mut Conn,
        id: &str,
        intent: &NewIntent<'_>,
        policy: ApprovalPolicy,
        deadline: Option<Instant>,
        route: &Route,
    ) -> Result<Outcome> {
        self.store.insert_intent(intent)?;
        // Direct mode: the child is the session's writer until it exits; later commands
        // tell it from a foreign writer by its pid.
        if let Route::Direct { .. } = route
            && let Some(pid) = conn.pid()
        {
            self.store
                .insert_process(intent.receipt_id, intent.handle, pid)?;
        }
        let response = conn
            .submit(
                "session/prompt",
                json!({
                    "sessionId": id,
                    "prompt": [{"type": "text", "text": intent.message.delivered}],
                    "_meta": {"promptId": intent.client_msg_id},
                }),
            )
            .await
            .map_err(|e| reject(self.store, intent.receipt_id, e))?;
        let shared = matches!(route, Route::Leader(_));
        let mut w = Watch::prompt(
            self.store,
            id,
            intent.receipt_id,
            intent.client_msg_id,
            response,
            policy,
            shared,
        );
        // Without --wait there is no deadline: the command still observes to the end of
        // the turn (DESIGN.md §5) and reports no turn. Only a signal ends it early.
        let mut res = bounded(deadline, w.finish(conn)).await;
        let stopped = |e: &Error| matches!(e.code, ErrorCode::Timeout | ErrorCode::Interrupted);
        let note = match route {
            Route::Leader(leader) => {
                if let Err(e) = &mut res
                    && stopped(e)
                {
                    e.message
                        .push_str("; the turn continues in the Grok leader");
                }
                self.leader_change(leader)
            }
            Route::Direct { .. } => {
                if let Err(e) = &mut res
                    && stopped(e)
                {
                    wind_down(&mut w, conn, id).await;
                    e.message.push_str("; in direct mode no process outlives the command, so agent-talk ended the turn with session/cancel unless it finished first; wait --receipt reads how it ended from updates.jsonl");
                }
                None
            }
        };
        let h = handle(id);
        let res = res.and_then(|end| match end.result {
            Ok(v) => Ok(w.turn(
                h.clone(),
                end.turn_id,
                &v,
                "session/prompt response",
                note.as_deref(),
            )),
            // A refusal of the prompt itself; an error after acceptance is a failed turn.
            Err(e) if !w.accepted() => Err(reject(self.store, intent.receipt_id, e)),
            Err(e) => Ok(model::Turn {
                handle: h.clone(),
                turn_id: end.turn_id,
                status: TurnStatus::Failed,
                error: Some(json!({"message": e.message, "agent_error": e.agent_error})),
                final_text: w.final_text(),
                duration_ms: None,
                basis: Some("session/prompt error response".into()),
            }),
        });
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

    /// `session/load` (replays the history as notifications before its response;
    /// they are discarded) and the load result.
    async fn session_load(
        &self,
        conn: &mut Conn,
        id: &str,
        cwd: &str,
        full_access: bool,
    ) -> Result<Value> {
        let params = with_yolo(
            json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
            full_access,
        );
        let r = conn.request("session/load", params).await?;
        conn.discard_notifications();
        Ok(r)
    }

    /// `send --steer`: `_x.ai/interject` on a session working in the leader.
    async fn steer(
        &self,
        id: &str,
        cwd: &str,
        req: &SendRequest<'_>,
        policy: ApprovalPolicy,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        let h = handle(id);
        let Some(leader) = self.leader()? else {
            return Err(Error::new(
                ErrorCode::NoSteer,
                format!(
                    "Grok steers only through a live leader ({} does not answer), and in direct mode no process of an earlier command survives to steer; send without --steer",
                    self.socket().display()
                ),
            ));
        };
        let mut conn = self.connect(Path::new(cwd), true, None).await?;
        let res = async {
            let listed = self.listed(&conn).await?;
            match listed.iter().find(|s| s.session_id == id) {
                Some(l) if l.resident && l.activity == Activity::Working => {}
                Some(l) if l.resident => {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!(
                            "{h} is {} in the leader; steer needs a running turn (Grok would start a fallback turn instead); send without --steer",
                            l.activity.as_str()
                        ),
                    ));
                }
                _ => {
                    return Err(Error::new(
                        ErrorCode::NoSteer,
                        format!(
                            "{h} is not resident in the Grok leader, so no running turn can be joined; send without --steer"
                        ),
                    ));
                }
            }
            let loaded = self.session_load(&mut conn, id, cwd, false).await?;
            let running = loaded["_meta"]["x.ai/runningPromptId"]
                .as_str()
                .map(String::from);
            let receipt_id = uuid::Uuid::new_v4().to_string();
            let client_msg_id = uuid::Uuid::new_v4().to_string();
            self.store.insert_intent(&NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &client_msg_id,
                message: &req.message,
            })?;
            let params = json!({"sessionId": id, "text": req.message.delivered});
            // Bounded like the observation below: a deadline or a signal leaves the receipt
            // `unknown` instead of `pending`.
            bounded(deadline, conn.request("_x.ai/interject", params))
                .await
                .map_err(|e| reject(self.store, &receipt_id, e))?;
            // The ack (`status: queued`) is the acceptance; the turn it joined comes with
            // `_x.ai/session/interjection`.
            self.store.accept(&receipt_id, None, None, None)?;
            let mut w = Watch::steer(
                self.store,
                id,
                &receipt_id,
                req.message.delivered,
                running,
                policy,
            );
            let observe = async {
                match deadline {
                    Some(_) => w.finish(&mut conn).await.map(Some),
                    None => {
                        // Without --wait: only until the joined turn is known.
                        let joined = w.until_running(&mut conn);
                        let _ = tokio::time::timeout(Duration::from_secs(5), joined).await;
                        Ok(None)
                    }
                }
            };
            let res = bounded(deadline, observe).await;
            let note = self.leader_change(&leader);
            // A steer message's turn ends with the joined turn's
            // `_x.ai/session/prompt_complete`, never an error.
            let res = res.map(|end| match end {
                Some(End {
                    turn_id,
                    result: Ok(v),
                }) => Some(w.turn(
                    h.clone(),
                    turn_id,
                    &v,
                    "_x.ai/session/prompt_complete of the joined turn",
                    note.as_deref(),
                )),
                _ => None,
            });
            settle(
                self.store,
                h.clone(),
                res,
                Some(&receipt_id),
                w.approvals,
                Progress::Running,
            )
        }
        .await;
        conn.close().await;
        res
    }

    /// An `ls` row for the session in `dir`.
    fn session(
        &self,
        dir: &Path,
        s: Summary,
        active: &[Active],
        listed: &[Listed],
    ) -> Result<Session> {
        let id = s.info.id;
        let h = handle(&id);
        let owned = self.store.is_owned(&h)?;
        let tui = active
            .iter()
            .any(|a| a.session_id == id && pid_alive(a.pid));
        let resident = listed
            .iter()
            .find(|l| l.session_id == id)
            .filter(|l| l.resident);
        let own_pid = self.own_run(&id, dir)?;
        let state = match (own_pid, resident.map(|l| l.activity)) {
            (Some(_), _) | (None, Some(Activity::Working)) => SessionState::Running,
            (None, Some(Activity::Idle)) => SessionState::Idle,
            _ => SessionState::Unknown,
        };
        let loaded = own_pid.is_some() || tui || resident.is_some();
        let updates_path = dir.join("updates.jsonl");
        Ok(Session {
            handle: h,
            agent: "grok",
            cwd: s.info.cwd,
            name: s.generated_title,
            preview: first_prompt(&updates_path).map(|p| first_line(strip_provenance(&p), 200)),
            observations: Observations {
                history: if updates_path.is_file() {
                    model::History::Visible
                } else {
                    model::History::Absent
                },
                loaded: if loaded { Loaded::Yes } else { Loaded::No },
                origin: if owned {
                    "agent-talk".into()
                } else if s.session_kind.as_deref() == Some("headless") {
                    "headless".into()
                } else {
                    "grok".into()
                },
            },
            state,
            owned,
            id,
        })
    }
}

/// The deadline or a signal ended observation of a direct-mode prompt. The child dies
/// with this command and would take the turn down mid-step; end it Grok's way instead,
/// so updates.jsonl records turn_completed (stop_reason cancelled). A cancel sent before
/// the prompt runs is ignored, hence the wait for it to start.
async fn wind_down(w: &mut Watch<'_>, conn: &mut Conn, id: &str) {
    let ended = tokio::time::timeout(WIND_DOWN, w.until_running(conn)).await;
    if !matches!(ended, Ok(Ok(true))) {
        let _ = conn
            .notify("session/cancel", json!({"sessionId": id}))
            .await;
        let _ = tokio::time::timeout(WIND_DOWN, w.finish(conn)).await;
    }
}

impl Agent for Grok<'_> {
    async fn models(&self) -> Result<Vec<Model>> {
        // A direct child's `initialize` response lists the account's models with their
        // effort choices; no session is opened (DESIGN.md §6.4).
        let conn = self.connect(&self.home, false, None).await?;
        let models = conn.init["_meta"]["modelState"]["availableModels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| AvailableModel::deserialize(m).ok())
            .map(|m| Model {
                id: m.model_id,
                efforts: m
                    .meta
                    .reasoning_efforts
                    .into_iter()
                    .filter_map(|e| e.value)
                    .collect(),
            })
            .collect();
        // Dropping kills the child: a clean exit takes about 4 s and there is nothing to
        // flush.
        drop(conn);
        Ok(models)
    }

    async fn status(&self) -> AgentStatus {
        let version = cli_version("grok").await;
        let cli: Check = version.as_ref().map(|_| ()).map_err(String::clone);
        let (shared, leader): (&str, Check) = match self.leader() {
            Ok(Some(_)) => ("leader running", Ok(())),
            Ok(None) => (
                "no leader running",
                Err("needs a running Grok leader".into()),
            ),
            Err(e) => ("leader not reachable", Err(e.message)),
        };
        let dir = sessions::sessions_dir(&self.home);
        let store: Check = if dir.is_dir() {
            Ok(())
        } else {
            Err(format!("{} does not exist", dir.display()))
        };
        AgentStatus {
            agent: "grok",
            version: version.ok(),
            shared: Some(shared.into()),
            operations: vec![
                Operation::new("ls", &store),
                Operation::new("new", &cli),
                Operation::new("send", &cli),
                Operation::new("read", &store),
                Operation::new("wait", &Ok(())),
                Operation::new("steer", &cli.clone().and(leader)),
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
        let mut rows = sessions::summaries(&self.home);
        rows.retain(|(_, s)| filter.cwd.is_none() || s.info.cwd.as_deref() == filter.cwd);
        // Newest first, ties by id; the cursor is `<updated_at>|<id>` of the last row of
        // the previous page.
        rows.sort_by(|(_, a), (_, b)| b.at().cmp(a.at()).then(a.info.id.cmp(&b.info.id)));
        if let Some(c) = cursor {
            let (at, id) = c.split_once('|').ok_or_else(|| {
                Error::new(ErrorCode::Precondition, format!("invalid cursor {c}"))
            })?;
            rows.retain(|(_, s)| s.at() < at || (s.at() == at && s.info.id.as_str() > id));
        }
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        let next_cursor = more
            .then(|| {
                rows.last()
                    .map(|(_, s)| format!("{}|{}", s.at(), s.info.id))
            })
            .flatten();
        if rows.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                next_cursor,
            });
        }
        let active = sessions::active(&self.home).unwrap_or_else(|e| {
            tracing::warn!("{e}");
            Vec::new()
        });
        let listed = self.leader_sessions().await;
        let items = rows
            .into_iter()
            .map(|(dir, s)| self.session(&dir, s, &active, &listed))
            .collect::<Result<_>>()?;
        Ok(Page { items, next_cursor })
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        let leader = self.leader()?;
        let mut conn = self
            .connect(
                Path::new(req.cwd),
                leader.is_some(),
                req.settings.model.as_deref(),
            )
            .await?;
        let res = async {
            let params = with_yolo(
                json!({"cwd": req.cwd, "mcpServers": []}),
                req.settings.full_access,
            );
            let created = conn.request("session/new", params).await?;
            let id = created["sessionId"].as_str().ok_or_else(|| {
                Error::new(
                    ErrorCode::Transport,
                    format!("session/new returned no sessionId: {created}"),
                )
            })?;
            // Set on the session, not as `--reasoning-effort`, which a leader proxy
            // ignores (DESIGN.md §6.4). A refusal leaves the new session empty and
            // unrecorded; agent-talk deletes nothing, so the error names it.
            if let Some(e) = &req.settings.effort {
                set_config(&conn, id, "reasoning_effort", e)
                    .await
                    .map_err(|mut err| {
                        let _ = write!(
                            err.message,
                            "; the new session {id} has no messages and is not used (`grok sessions delete {id}` removes it)"
                        );
                        err
                    })?;
            }
            let h = handle(id);
            let route = match leader {
                Some(leader) => Route::Leader(leader),
                None => Route::Direct {
                    _lock: lock(&h, SECOND_WRITER)?,
                },
            };
            self.store.insert_owned(&h, req.cwd, req.settings)?;
            if let Some(n) = req.name {
                conn.request("_x.ai/session/rename", json!({"sessionId": id, "title": n}))
                    .await?;
            }
            let receipt_id = uuid::Uuid::new_v4().to_string();
            // The promptId: becomes turn_completed.prompt_id, i.e. the turn id.
            let prompt_id = uuid::Uuid::new_v4().to_string();
            let intent = NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &prompt_id,
                message: &req.message,
            };
            self.run(
                &mut conn,
                id,
                &intent,
                ApprovalPolicy::Deny,
                deadline,
                &route,
            )
            .await
        }
        .await;
        conn.close().await;
        res
    }

    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        check_id(id)?;
        let h = handle(id);
        let dir = sessions::require(&self.home, id)?;
        let cwd = self.session_cwd(&dir)?;
        let policy = approval_policy(self.store, &h)?;
        if req.steer {
            return self.steer(id, &cwd, req, policy, deadline).await;
        }
        let (route, mut conn) = self.route(id, &dir, Path::new(&cwd)).await?;
        let res = async {
            let direct = matches!(route, Route::Direct { .. });
            // yoloMode only in direct mode: on a session resident in a leader it has no
            // effect (`yolo` stayed unchanged for every client after such a load; grok
            // 1.0.46).
            if !direct && req.change.full_access {
                return Err(Error::new(
                    ErrorCode::Unsupported,
                    format!(
                        "full access cannot be given to {h} while it is resident in the Grok leader (DESIGN.md §6.4); send without --full-access"
                    ),
                ));
            }
            let yolo = direct && req.settings.is_some_and(|s| s.full_access);
            self.session_load(&mut conn, id, &cwd, yolo).await?;
            // Model and effort are set on the session, which Grok stores at once for
            // every later load and, in a leader, every attached client (DESIGN.md §6.4).
            // A refusal fails the send before anything is recorded, as in `start`. A
            // value set here stays stored even if the prompt below, or the second value,
            // is then refused.
            if let Some(m) = &req.change.model {
                set_config(&conn, id, "model", m).await?;
            }
            if let Some(e) = &req.change.effort {
                set_config(&conn, id, "reasoning_effort", e).await?;
            }
            let receipt_id = uuid::Uuid::new_v4().to_string();
            let prompt_id = uuid::Uuid::new_v4().to_string();
            let intent = NewIntent {
                receipt_id: &receipt_id,
                handle: &h,
                client_msg_id: &prompt_id,
                message: &req.message,
            };
            self.run(&mut conn, id, &intent, policy, deadline, &route)
                .await
        }
        .await;
        conn.close().await;
        res
    }

    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage> {
        check_id(id)?;
        let dir = sessions::require(&self.home, id)?;
        let history = History::new(updates::load(&dir.join("updates.jsonl"))?);
        let mut messages = history.messages();
        for m in &mut messages {
            if m.prompt
                && let Some(s) = m.span
                && let Some(p) = &history.spans[s].id
            {
                m.message.from = self.store.sender_of(p)?;
            }
        }
        let messages = messages.into_iter().map(|m| (m.line, m.message)).collect();
        let cut = cut(q, q.cursor.as_deref(), messages, history.lines.len(), false)?;
        Ok(ReadPage {
            messages: Page {
                next_cursor: cut.cursor_item().map(|(_, m)| m.item_id.clone()),
                items: cut.messages.into_iter().map(|(_, m)| m).collect(),
            },
            // Every line in the span (thoughts, tool updates, hooks included).
            raw: history.lines[cut.records]
                .iter()
                .map(|l| l.raw.clone())
                .collect(),
        })
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        check_id(id)?;
        let h = handle(id);
        let (turn_id, receipt) = match target {
            WaitTarget::Turn(t) => (t.clone(), self.store.receipt_by_turn(&h, t)?),
            WaitTarget::Receipt(r) => {
                let rec = wait_receipt(self.store, &h, r)?;
                // The promptId, recorded as the client message id before submission.
                (
                    rec.turn_id
                        .clone()
                        .unwrap_or_else(|| rec.client_msg_id.clone()),
                    Some(rec),
                )
            }
        };
        let dir = sessions::require(&self.home, id)?;
        let path = dir.join("updates.jsonl");
        let mut unaccepted = receipt
            .as_ref()
            .is_some_and(|r| matches!(r.state, ReceiptState::Pending | ReceiptState::Unknown));
        // The direct-mode child that ran the receipt's prompt; once it is gone nothing
        // will write the turn's end.
        let child = match &receipt {
            Some(r) => self.store.process(&r.receipt_id)?,
            None => None,
        };
        let work = async {
            loop {
                let history = History::new(updates::load(&path)?);
                let span = history.find(&turn_id);
                if let (Some(_), Some(r)) = (span, &receipt)
                    && unaccepted
                {
                    self.store
                        .accept(&r.receipt_id, Some(&turn_id), Some(&turn_id), None)?;
                    unaccepted = false;
                }
                if let Some(s) = span
                    && let Some(t) = history.turn(h.clone(), s, &history.messages())
                {
                    return Ok(t);
                }
                if span.is_none() && receipt.is_none() {
                    return Err(Error::new(
                        ErrorCode::Precondition,
                        format!("turn {turn_id} is not in {}", path.display()),
                    ));
                }
                if let Some(pid) = child
                    && !pid_alive(pid)
                {
                    let (status, message) = match span {
                        Some(_) => (
                            TurnStatus::Interrupted,
                            "the turn started but has no turn_completed; Grok marks it interrupted when the session is next loaded",
                        ),
                        None => (
                            TurnStatus::Unknown,
                            "updates.jsonl names no turn for this prompt",
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
                            "updates.jsonl; the grok agent agent-talk started for this prompt (pid {pid}) has exited, and in direct mode nothing else continues it"
                        )),
                    });
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        };
        let res = bounded(Some(deadline), work).await.map(Some).map_err(|e| {
            note_waiting(
                e,
                &format!(
                    "waiting for turn_completed of {turn_id} in updates.jsonl (polled every second)"
                ),
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
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn grok<'a>(store: &'a Store, socket: PathBuf) -> Grok<'a> {
        Grok {
            store,
            home: socket.parent().unwrap().to_path_buf(),
            leader_socket: Some(socket),
        }
    }

    /// Direct mode only on a missing socket or a refused connect (a stale socket file
    /// such as one a dead leader left); a socket agent-talk may not reach is an error,
    /// never "no leader" (that would start a second writer beside a live leader).
    #[test]
    fn leader_probe_classification() {
        let store = Store::memory();
        let dir = std::env::temp_dir().join(format!("bgl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("l.sock");
        assert!(
            grok(&store, sock.clone()).leader().unwrap().is_none(),
            "ENOENT"
        );
        let listener = UnixListener::bind(&sock).unwrap();
        assert!(
            grok(&store, sock.clone()).leader().unwrap().is_some(),
            "answering"
        );
        drop(listener);
        assert!(sock.exists());
        assert!(
            grok(&store, sock.clone()).leader().unwrap().is_none(),
            "ECONNREFUSED"
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = grok(&store, sock.clone()).leader();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            denied.err().map(|e| e.code),
            Some(ErrorCode::Transport),
            "EACCES"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A leader row whose activity (or residency) does not fit keeps the row: residency
    /// decides the route, and dropping the row would pick a direct child beside the leader.
    #[test]
    fn odd_listed_fields_keep_the_row() {
        let row =
            |s: &str| Listed::deserialize(&serde_json::from_str::<Value>(s).unwrap()).unwrap();
        let l = row(r#"{"sessionId":"s1","resident":true,"activity":null}"#);
        assert!(l.session_id == "s1" && l.resident && l.activity == Activity::Unknown);
        let l = row(r#"{"sessionId":"s1","resident":null,"activity":"sleeping"}"#);
        assert!(!l.resident && l.activity == Activity::Unknown);
    }
}
