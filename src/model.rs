//! Agent-neutral types printed by the CLI.
//!
//! The doc comments of this file are the MCP output schemas' descriptions; what they say
//! is for callers, how a value is derived stays in plain comments.
//! `#[serde(default)]` on a field of a type that is never deserialized still matters: it
//! makes the field optional in the schema.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// A closed set of strings as an enum, each variant listed once with its wire string:
/// `as_str`, `Display`, `Serialize` as the string, and an inline `{"type": "string",
/// "enum": [...]}` schema.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $wire:literal,)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)*
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)*
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                s.serialize_str(self.as_str())
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({"type": "string", "enum": [$($wire),*]})
            }
        }
    };
}

string_enum! {
    /// Stable error codes (DESIGN.md §5).
    pub enum ErrorCode {
        NoDaemon = "E_NO_DAEMON",
        CapMissing = "E_CAP_MISSING",
        Precondition = "E_PRECONDITION",
        Locked = "E_LOCKED",
        ForeignLive = "E_FOREIGN_LIVE",
        NoSteer = "E_NO_STEER",
        MaxHops = "E_MAX_HOPS",
        Timeout = "E_TIMEOUT",
        /// Ctrl-C during an observation; outcome unknown like a timeout.
        Interrupted = "E_INTERRUPTED",
        Transport = "E_TRANSPORT",
        Unsupported = "E_UNSUPPORTED",
    }
}

impl ErrorCode {
    /// The outcome of a submission is unknown: the deadline passed, Ctrl-C or SIGTERM
    /// ended observation, or the transport was lost (DESIGN.md §4).
    pub fn is_unknown_outcome(self) -> bool {
        matches!(self, Self::Timeout | Self::Interrupted | Self::Transport)
    }

    /// 2 refused, 3 unknown outcome, 4 transport failure.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Timeout | Self::Interrupted => 3,
            Self::Transport => 4,
            _ => 2,
        }
    }
}

/// The error object as the agent sent it (JSON-RPC shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct AgentError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, thiserror::Error, Serialize, JsonSchema)]
#[error("{code}: {message}")]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_error: Option<Box<AgentError>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Box<Receipt>>,
    /// Progress when the outcome is unknown: waiting (approval pending), pending (still
    /// queued), running, unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<Progress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Approval>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            agent_error: None,
            receipt: None,
            approvals: Vec::new(),
            state: None,
        }
    }

    pub fn from_agent(code: ErrorCode, e: AgentError) -> Self {
        Self {
            message: e.message.clone(),
            agent_error: Some(Box::new(e)),
            ..Self::new(code, "")
        }
    }

    /// A local I/O or transport failure: `<what>: <e>`.
    pub fn transport(what: impl fmt::Display, e: impl fmt::Display) -> Self {
        Self::new(ErrorCode::Transport, format!("{what}: {e}"))
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::transport("store", e)
    }
}

string_enum! {
    /// How far an intent whose outcome is unknown got (`Error::state`).
    pub enum Progress {
        Waiting = "waiting",
        Pending = "pending",
        Running = "running",
        Unknown = "unknown",
    }
}

string_enum! {
    pub enum ReceiptState {
        Pending = "pending",
        Accepted = "accepted",
        Unknown = "unknown",
        Rejected = "rejected",
    }
}

/// Read back from the string `as_str` stored; any other text is an error.
impl FromSql for ReceiptState {
    fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = v.as_str()?;
        [Self::Pending, Self::Accepted, Self::Unknown, Self::Rejected]
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| FromSqlError::Other(format!("not a receipt state: {s:?}").into()))
    }
}

/// What agent-talk knows about one submitted intent.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Receipt {
    pub receipt_id: String,
    pub handle: String,
    pub client_msg_id: String,
    pub state: ReceiptState,
    pub queue_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub agent_error: Option<AgentError>,
    /// The text the agent received, when it differs from the caller's (provenance header).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_text: Option<String>,
}

/// A permission or input request raised by a session, and what happened to it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Approval {
    pub handle: String,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub request_id: Value,
    pub kind: String,
    pub summary: String,
    /// pending (nobody answered yet), declined (by agent-talk), resolved (answered by
    /// another client), denied (by the agent CLI's own permission handling), cancelled
    /// (unanswered when agent-talk cancelled the turn, which withdrew it).
    pub outcome: ApprovalOutcome,
    pub raw: Value,
}

string_enum! {
    pub enum ApprovalOutcome {
        Pending = "pending",
        Declined = "declined",
        Resolved = "resolved",
        Denied = "denied",
        Cancelled = "cancelled",
    }
}

string_enum! {
    pub enum History {
        Visible = "visible",
        Absent = "none",
    }
}

string_enum! {
    pub enum Loaded {
        Yes = "yes",
        No = "no",
        Unknown = "unknown",
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Observations {
    pub history: History,
    pub loaded: Loaded,
    pub origin: String,
}

string_enum! {
    pub enum SessionState {
        Idle = "idle",
        Running = "running",
        Waiting = "waiting",
        Unknown = "unknown",
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Session {
    pub handle: String,
    pub agent: &'static str,
    pub id: String,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub preview: Option<String>,
    pub observations: Observations,
    pub state: SessionState,
    pub owned: bool,
}

string_enum! {
    pub enum TurnStatus {
        Completed = "completed",
        Failed = "failed",
        Interrupted = "interrupted",
        Unknown = "unknown",
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Turn {
    pub handle: String,
    pub turn_id: String,
    pub status: TurnStatus,
    pub error: Option<Value>,
    pub final_text: Option<String>,
    pub duration_ms: Option<i64>,
    /// How the end of the turn was established, when it was not an agent turn-end event
    /// on agent-talk's own connection.
    // Claude, Antigravity: the transcript, the `result` event or the `-p` process exit;
    // Grok: the `session/prompt` response or `updates.jsonl`; OpenCode: the history rule
    // the adapter applied. The adapters word it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
}

/// Who sent a message. Attribution, not authenticated identity.
// Configured or derived: `--caller`, `--from`, `AGENT_TALK_CALLER`, the MCP call's `_meta`
// (Codex, OpenCode, Antigravity), the agents' session variables (DESIGN.md §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Caller {
    pub kind: CallerKind,
    /// Handle of the sending session, for `kind: agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Where in the sender's session the message came from (its turn or tool-call id), as
    /// a correlation hint.
    // The Codex turn id (`_meta["x-codex-turn-metadata"].turn_id`) or the Claude tool-use
    // id (`_meta["claudecode/toolUseId"]`) of the MCP call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CallerKind {
    /// An agent session, named by its handle.
    Agent,
    /// Nothing configured: a person or an agent using the CLI without `--from`.
    Unknown,
    /// Input the agent's runtime injected (task notifications, reminders).
    Runtime,
}

impl Caller {
    pub fn agent(handle: &str) -> Self {
        Caller {
            kind: CallerKind::Agent,
            session: Some(handle.into()),
            turn: None,
        }
    }

    pub const UNKNOWN: Caller = Caller {
        kind: CallerKind::Unknown,
        session: None,
        turn: None,
    };

    pub const RUNTIME: Caller = Caller {
        kind: CallerKind::Runtime,
        session: None,
        turn: None,
    };
}

string_enum! {
    pub enum Role {
        User = "user",
        Assistant = "assistant",
    }
}

string_enum! {
    pub enum Phase {
        Prompt = "prompt",
        Final = "final",
        Commentary = "commentary",
        Other = "other",
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Message {
    pub turn_id: String,
    pub item_id: String,
    pub role: Role,
    /// `prompt`: input a person or an agent sent; `final`: the reply that ended a turn;
    /// `commentary`: text the agent wrote on the way; `other`: tool calls and results,
    /// input the agent's runtime injected, interruptions and errors.
    pub phase: Phase,
    pub text: String,
    /// User messages: the caller recorded for the intent that sent it, or
    /// `{kind: runtime}` for input the agent's runtime injected. Absent when the message
    /// did not come through agent-talk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Caller>,
    /// When the message was written, ISO 8601.
    // Claude, Grok (`updates.jsonl`), Antigravity: the transcript line. OpenCode: the
    // message's time. Codex: the turn (items carry no time): `startedAt` for user items,
    // `completedAt` for agent items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// A model an agent can start a session on.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Model {
    /// What `new --model` takes (OpenCode: `provider/model`).
    pub id: String,
    /// What `new --effort` takes with this model; empty when it has no effort setting.
    pub efforts: Vec<String>,
}

/// Result of `new`, `send` and `wait`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Outcome {
    pub handle: String,
    pub receipt: Option<Receipt>,
    pub turn: Option<Turn>,
    pub approvals: Vec<Approval>,
    /// `new` / `send`: the sender recorded for the intent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Caller>,
}
