//! Agent-neutral types printed by the CLI.
//!
//! The doc comments of this file are the MCP output schemas' descriptions; what they say
//! is for callers, how a value is derived stays in plain comments.
//! `#[serde(default)]` on a field of a type that is never deserialized still matters: it
//! makes the field optional in the schema.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Stable error codes (DESIGN.md §5); serialized as their `E_*` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NoDaemon,
    CapMissing,
    Precondition,
    Locked,
    ForeignLive,
    NoSteer,
    MaxHops,
    Timeout,
    /// Ctrl-C during an observation; outcome unknown like a timeout.
    Interrupted,
    Transport,
    Unsupported,
}

impl Serialize for ErrorCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl ErrorCode {
    const ALL: [Self; 11] = [
        Self::NoDaemon,
        Self::CapMissing,
        Self::Precondition,
        Self::Locked,
        Self::ForeignLive,
        Self::NoSteer,
        Self::MaxHops,
        Self::Timeout,
        Self::Interrupted,
        Self::Transport,
        Self::Unsupported,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoDaemon => "E_NO_DAEMON",
            Self::CapMissing => "E_CAP_MISSING",
            Self::Precondition => "E_PRECONDITION",
            Self::Locked => "E_LOCKED",
            Self::ForeignLive => "E_FOREIGN_LIVE",
            Self::NoSteer => "E_NO_STEER",
            Self::MaxHops => "E_MAX_HOPS",
            Self::Timeout => "E_TIMEOUT",
            Self::Interrupted => "E_INTERRUPTED",
            Self::Transport => "E_TRANSPORT",
            Self::Unsupported => "E_UNSUPPORTED",
        }
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

impl JsonSchema for ErrorCode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ErrorCode".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let codes: Vec<&str> = Self::ALL.iter().map(|c| c.as_str()).collect();
        schemars::json_schema!({"type": "string", "enum": codes})
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
    #[schemars(extend("enum" = ["waiting", "pending", "running", "unknown", null]))]
    pub state: Option<&'static str>,
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
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::new(ErrorCode::Transport, format!("store: {e}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptState {
    Pending,
    Accepted,
    Unknown,
    Rejected,
}

impl ReceiptState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Unknown => "unknown",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "accepted" => Self::Accepted,
            "rejected" => Self::Rejected,
            _ => Self::Unknown,
        }
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
    /// another client), denied (by the agent CLI's own permission handling).
    #[schemars(extend("enum" = ["pending", "declined", "resolved", "denied"]))]
    pub outcome: &'static str,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Observations {
    #[schemars(extend("enum" = ["visible", "none", "unknown"]))]
    pub history: &'static str,
    #[schemars(extend("enum" = ["yes", "no", "unknown"]))]
    pub loaded: &'static str,
    pub origin: String,
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
    #[schemars(extend("enum" = ["idle", "running", "waiting", "unknown"]))]
    pub state: &'static str,
    pub owned: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Turn {
    pub handle: String,
    pub turn_id: String,
    #[schemars(extend("enum" = ["completed", "failed", "interrupted", "unknown"]))]
    pub status: &'static str,
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

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Message {
    pub turn_id: String,
    pub item_id: String,
    #[schemars(extend("enum" = ["user", "assistant"]))]
    pub role: &'static str,
    #[schemars(extend("enum" = ["final", "commentary", "other"]))]
    pub phase: &'static str,
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
