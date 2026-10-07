//! `~/.agent-talk/store.db`: intents, receipts, owned sessions, processes, approvals.
//! One connection per command, owned by the command's main task.

use crate::agents::Settings;
use crate::model::{
    AgentError, Approval, Caller, CallerKind, Error, ErrorCode, Receipt, ReceiptState, Result,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use std::path::PathBuf;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS intents (
    receipt_id    TEXT PRIMARY KEY,
    handle        TEXT NOT NULL,
    client_msg_id TEXT NOT NULL UNIQUE,
    text          TEXT NOT NULL,
    "from"        TEXT,                       -- JSON model::Caller
    depth         INTEGER NOT NULL DEFAULT 0, -- hop depth, see Store::hop_depth
    delivered_text TEXT,                      -- what reached the agent, when not `text`
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TABLE IF NOT EXISTS receipts (
    receipt_id   TEXT PRIMARY KEY REFERENCES intents(receipt_id),
    state        TEXT NOT NULL,
    queue_id     TEXT,
    turn_id      TEXT,
    item_id      TEXT,
    agent_error TEXT,
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TABLE IF NOT EXISTS owned (
    handle     TEXT PRIMARY KEY,
    cwd        TEXT NOT NULL,
    args       TEXT NOT NULL,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
-- Agent processes agent-talk spawned for an intent (one `claude -p`, `grok agent` or
-- `agy -p` per mutation), so later commands can tell a run that is still alive.
CREATE TABLE IF NOT EXISTS processes (
    receipt_id TEXT PRIMARY KEY REFERENCES intents(receipt_id),
    handle     TEXT NOT NULL,
    pid        INTEGER NOT NULL,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TABLE IF NOT EXISTS approvals (
    id         INTEGER PRIMARY KEY,
    handle     TEXT NOT NULL,
    turn_id    TEXT,
    item_id    TEXT NOT NULL,
    request_id TEXT NOT NULL,
    kind       TEXT NOT NULL,
    summary    TEXT NOT NULL,
    outcome    TEXT NOT NULL,
    raw        TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- A replayed request (after resume) has the same id and item.
    UNIQUE (handle, request_id, item_id)
);
"#;

pub struct Store {
    db: Connection,
}

pub struct NewIntent<'a> {
    pub receipt_id: &'a str,
    pub handle: &'a str,
    pub client_msg_id: &'a str,
    pub text: &'a str,
    pub from: &'a Caller,
    pub depth: u32,
    /// Set when the agent got something other than `text` (the provenance header).
    pub delivered_text: Option<&'a str>,
}

impl Store {
    pub fn open() -> Result<Store> {
        let dir = std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".agent-talk");
        std::fs::create_dir_all(&dir).map_err(|e| {
            Error::new(
                ErrorCode::Transport,
                format!("store: create {}: {e}", dir.display()),
            )
        })?;
        let db = Connection::open(dir.join("store.db"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        Store::init(db)
    }

    fn init(db: Connection) -> Result<Store> {
        db.execute_batch(SCHEMA)?;
        // Stores created before these columns existed.
        for (column, ddl) in [
            ("depth", "INTEGER NOT NULL DEFAULT 0"),
            ("delivered_text", "TEXT"),
        ] {
            let present: i64 = db.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('intents') WHERE name = ?1",
                params![column],
                |row| row.get(0),
            )?;
            if present == 0 {
                db.execute_batch(&format!("ALTER TABLE intents ADD COLUMN {column} {ddl}"))?;
            }
        }
        Ok(Store { db })
    }

    #[cfg(test)]
    pub fn memory() -> Store {
        Store::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    /// Persist an intent and its pending receipt. Called before anything is sent.
    pub fn insert_intent(&self, i: &NewIntent) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            r#"INSERT INTO intents (receipt_id, handle, client_msg_id, text, "from", depth,
                                    delivered_text)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                i.receipt_id,
                i.handle,
                i.client_msg_id,
                i.text,
                serde_json::to_string(i.from).unwrap(),
                i.depth,
                i.delivered_text
            ],
        )?;
        tx.execute(
            "INSERT INTO receipts (receipt_id, state) VALUES (?1, 'pending')",
            params![i.receipt_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The agent took the intent: `accepted`, with the correlation ids known so far.
    /// Called again when more become known (the turn a queued message started, the
    /// user item); a recorded id is never cleared. Also the recovery path: a receipt
    /// left `unknown` becomes `accepted` once history or the queue proves delivery.
    pub fn accept(
        &self,
        receipt_id: &str,
        queue_id: Option<&str>,
        turn_id: Option<&str>,
        item_id: Option<&str>,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE receipts SET state = 'accepted', queue_id = COALESCE(?2, queue_id),
             turn_id = COALESCE(?3, turn_id), item_id = COALESCE(?4, item_id),
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE receipt_id = ?1",
            params![receipt_id, queue_id, turn_id, item_id],
        )?;
        Ok(())
    }

    /// The submission did not get through: `rejected` (the agent refused it, or its run
    /// log proves the message was never taken) or `unknown` (the outcome was lost). A
    /// receipt already `accepted` or `rejected` does not move, so a concurrent command that
    /// proved acceptance is not overwritten; an `unknown` one can still become `rejected`
    /// (the counterpart of `accept`'s recovery path).
    pub fn settle_unaccepted(
        &self,
        receipt_id: &str,
        state: ReceiptState,
        agent_error: Option<&AgentError>,
    ) -> Result<()> {
        let agent_error = agent_error.map(|v| serde_json::to_string(v).unwrap());
        self.db.execute(
            "UPDATE receipts SET state = ?2, agent_error = COALESCE(?3, agent_error),
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE receipt_id = ?1 AND state IN ('pending', 'unknown')",
            params![receipt_id, state.as_str(), agent_error],
        )?;
        Ok(())
    }

    pub fn receipt(&self, receipt_id: &str) -> Result<Option<Receipt>> {
        let r = self
            .db
            .query_row(
                "SELECT i.receipt_id, i.handle, i.client_msg_id, r.state, r.queue_id, r.turn_id,
                        r.item_id, r.agent_error, i.delivered_text
                 FROM intents i JOIN receipts r USING (receipt_id) WHERE i.receipt_id = ?1",
                params![receipt_id],
                |row| {
                    let state: String = row.get(3)?;
                    let agent_error: Option<String> = row.get(7)?;
                    Ok(Receipt {
                        receipt_id: row.get(0)?,
                        handle: row.get(1)?,
                        client_msg_id: row.get(2)?,
                        state: ReceiptState::parse(&state),
                        queue_id: row.get(4)?,
                        turn_id: row.get(5)?,
                        item_id: row.get(6)?,
                        agent_error: agent_error.and_then(|v| serde_json::from_str(&v).ok()),
                        delivered_text: row.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(r)
    }

    /// The newest receipt on `handle` whose intent became `turn_id`.
    pub fn receipt_by_turn(&self, handle: &str, turn_id: &str) -> Result<Option<Receipt>> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT i.receipt_id FROM intents i JOIN receipts r USING (receipt_id)
                 WHERE i.handle = ?1 AND r.turn_id = ?2
                 ORDER BY i.created_at DESC, i.rowid DESC LIMIT 1",
                params![handle, turn_id],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.receipt(&id),
            None => Ok(None),
        }
    }

    /// Hop depth for a new intent sent by `from`, and how it was derived:
    /// - from an agent: 1 + the depth of the intent that started the caller's turn when the
    ///   caller's turn id is known and recorded, else of the newest intent delivered to the
    ///   caller's own session, the message it is presumably acting on (none recorded: 0);
    /// - else (a person or an unknown caller): 0.
    pub fn hop_depth(&self, from: &Caller) -> Result<(u32, String)> {
        let Caller {
            kind: CallerKind::Agent,
            session: Some(session),
            turn,
        } = from
        else {
            return Ok((0, "the sender is not an agent".into()));
        };
        if let Some(t) = turn {
            let parent: Option<u32> = self.db.query_row(
                "SELECT MAX(i.depth) FROM intents i JOIN receipts r USING (receipt_id)
                 WHERE i.handle = ?1 AND r.turn_id = ?2",
                params![session, t],
                |row| row.get(0),
            )?;
            if let Some(d) = parent {
                return Ok((
                    d + 1,
                    format!("the message that started the caller's turn {t} has depth {d}"),
                ));
            }
        }
        let parent: Option<u32> = self
            .db
            .query_row(
                "SELECT i.depth FROM intents i JOIN receipts r USING (receipt_id)
                 WHERE i.handle = ?1 AND r.state != 'rejected'
                 ORDER BY i.created_at DESC, i.rowid DESC LIMIT 1",
                params![session],
                |row| row.get(0),
            )
            .optional()?;
        let basis = match parent {
            Some(d) => {
                format!("the newest message delivered to the caller {session} has depth {d}")
            }
            None => format!("no message delivered to the caller {session} is recorded (depth 0)"),
        };
        Ok((parent.unwrap_or(0) + 1, basis))
    }

    /// Caller recorded for the intent with this client message id or user item id, if any.
    pub fn sender_of(&self, id: &str) -> Result<Option<Caller>> {
        let from: Option<String> = self
            .db
            .query_row(
                r#"SELECT i."from" FROM intents i JOIN receipts r USING (receipt_id)
                   WHERE i.client_msg_id = ?1 OR r.item_id = ?1 LIMIT 1"#,
                params![id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        // Rows from before callers were recorded as JSON hold a bare label, not a caller.
        Ok(from.and_then(|f| serde_json::from_str(&f).ok()))
    }

    /// Caller recorded for the newest intent on `handle` that became `turn_id`, for
    /// agents whose turn ids are per session (Antigravity step indices).
    pub fn sender_by_turn(&self, handle: &str, turn_id: &str) -> Result<Option<Caller>> {
        let from: Option<String> = self
            .db
            .query_row(
                r#"SELECT i."from" FROM intents i JOIN receipts r USING (receipt_id)
                   WHERE i.handle = ?1 AND r.turn_id = ?2
                   ORDER BY i.created_at DESC, i.rowid DESC LIMIT 1"#,
                params![handle, turn_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(from.and_then(|f| serde_json::from_str(&f).ok()))
    }

    /// agent-talk started this session: its working directory and settings.
    pub fn insert_owned(&self, handle: &str, cwd: &str, settings: &Settings) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO owned (handle, cwd, args) VALUES (?1, ?2, ?3)",
            params![handle, cwd, json!(settings).to_string()],
        )?;
        Ok(())
    }

    /// The settings of a session agent-talk started, as `new` chose or a `send` changed them;
    /// `None` for any other session. A row stored in another shape (0.1.0 kept OpenCode's
    /// request body) reads as the default settings.
    pub fn settings(&self, handle: &str) -> Result<Option<Settings>> {
        let args: Option<String> = self
            .db
            .query_row(
                "SELECT args FROM owned WHERE handle = ?1",
                params![handle],
                |row| row.get(0),
            )
            .optional()?;
        Ok(args.map(|a| serde_json::from_str(&a).unwrap_or_default()))
    }

    pub fn set_settings(&self, handle: &str, settings: &Settings) -> Result<()> {
        self.db.execute(
            "UPDATE owned SET args = ?2 WHERE handle = ?1",
            params![handle, json!(settings).to_string()],
        )?;
        Ok(())
    }

    /// Working directory recorded when agent-talk started this session.
    pub fn owned_cwd(&self, handle: &str) -> Result<Option<String>> {
        let cwd = self
            .db
            .query_row(
                "SELECT cwd FROM owned WHERE handle = ?1",
                params![handle],
                |row| row.get(0),
            )
            .optional()?;
        Ok(cwd)
    }

    pub fn is_owned(&self, handle: &str) -> Result<bool> {
        let n: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM owned WHERE handle = ?1",
            params![handle],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn insert_process(&self, receipt_id: &str, handle: &str, pid: u32) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO processes (receipt_id, handle, pid) VALUES (?1, ?2, ?3)",
            params![receipt_id, handle, pid],
        )?;
        Ok(())
    }

    /// `(receipt_id, pid)` of the newest processes spawned for `handle`, newest first, for
    /// the liveness scan. While a run is alive, agent-talk refuses further sends to the
    /// handle, so a live run is always among the newest ten.
    pub fn processes(&self, handle: &str) -> Result<Vec<(String, u32)>> {
        let mut stmt = self.db.prepare(
            "SELECT receipt_id, pid FROM processes WHERE handle = ?1
             ORDER BY started_at DESC LIMIT 10",
        )?;
        let rows = stmt
            .query_map(params![handle], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The pid of the process spawned for `receipt_id`.
    pub fn process(&self, receipt_id: &str) -> Result<Option<u32>> {
        Ok(self
            .db
            .query_row(
                "SELECT pid FROM processes WHERE receipt_id = ?1",
                params![receipt_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn insert_approval(&self, a: &Approval) -> Result<()> {
        self.db.execute(
            "INSERT INTO approvals (handle, turn_id, item_id, request_id, kind, summary, outcome, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (handle, request_id, item_id) DO UPDATE SET outcome = excluded.outcome,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![
                a.handle,
                a.turn_id,
                a.item_id.as_deref().unwrap_or(""),
                a.request_id.to_string(),
                a.kind,
                a.summary,
                a.outcome,
                a.raw.to_string()
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(s: &Store, id: &str, handle: &str, from: &Caller) -> u32 {
        let (depth, _) = s.hop_depth(from).unwrap();
        s.insert_intent(&NewIntent {
            receipt_id: id,
            handle,
            client_msg_id: &format!("c-{id}"),
            text: "x",
            from,
            depth,
            delivered_text: None,
        })
        .unwrap();
        depth
    }

    #[test]
    fn hop_depth_follows_agent_callers() {
        let s = Store::memory();
        let a = Caller::agent("claude:a");
        let b = Caller::agent("codex:b");
        // A person starts claude:a; claude:a asks codex:b; codex:b answers claude:a; …
        assert_eq!(intent(&s, "r0", "claude:a", &Caller::UNKNOWN), 0);
        assert_eq!(intent(&s, "r1", "codex:b", &a), 1);
        assert_eq!(intent(&s, "r2", "claude:a", &b), 2);
        assert_eq!(intent(&s, "r3", "codex:b", &a), 3);
        // An agent with nothing delivered to it: depth 1.
        assert_eq!(s.hop_depth(&Caller::agent("codex:z")).unwrap().0, 1);
        // A known caller turn picks the intent that started that turn, not the newest.
        s.accept("r1", None, Some("turn-1"), None).unwrap();
        let in_turn_1 = Caller {
            turn: Some("turn-1".into()),
            ..b.clone()
        };
        assert_eq!(s.hop_depth(&in_turn_1).unwrap().0, 2);
        let unknown_turn = Caller {
            turn: Some("nope".into()),
            ..b.clone()
        };
        assert_eq!(s.hop_depth(&unknown_turn).unwrap().0, 4);
        // Rejected intents are not what the caller is acting on.
        s.settle_unaccepted("r3", ReceiptState::Rejected, None)
            .unwrap();
        assert_eq!(s.hop_depth(&b).unwrap().0, 2);
    }

    #[test]
    fn sender_round_trips_and_ignores_legacy_labels() {
        let s = Store::memory();
        intent(&s, "r0", "codex:b", &Caller::agent("claude:a"));
        assert_eq!(
            s.sender_of("c-r0").unwrap(),
            Some(Caller::agent("claude:a"))
        );
        s.db.execute(r#"UPDATE intents SET "from" = 'cli'"#, [])
            .unwrap();
        assert_eq!(s.sender_of("c-r0").unwrap(), None);
    }
}
