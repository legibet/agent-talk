//! The Codex app-server protocol as agent-talk uses it (codex-cli 0.160.0): hand-written,
//! narrow structs for responses and notifications (unknown fields are ignored; callers
//! keep the raw `Value`), plus the few request pieces shared by several call sites.

use crate::model::{AgentError, Error, ErrorCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

// ---- requests ----
// Request bodies are built with `json!` at the call sites, or with a struct when the call
// site would add several optional fields; optional fields are omitted rather than sent as
// `null`.

/// `turn/start` params. The settings are `None` (omitted) unless this turn changes them.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams<'a> {
    pub thread_id: &'a str,
    pub input: Value,
    pub client_user_message_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<Value>,
}

/// Text-only `UserInput` list.
pub fn text_input(text: &str) -> Value {
    json!([{"type": "text", "text": text, "text_elements": []}])
}

/// `thread/resume` params: subscribes the connection to the thread. Turns are excluded;
/// the newest turn comes back in `initialTurnsPage`, which tells `send` whether the thread
/// is idle and which turn `steer` targets.
pub fn resume_latest_turn(thread_id: &str) -> Value {
    json!({
        "threadId": thread_id,
        "excludeTurns": true,
        "initialTurnsPage": {"limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"},
    })
}

/// Interactive and exec origins. `thread/list` defaults to interactive only.
pub const DEFAULT_SOURCE_KINDS: &[&str] = &["cli", "vscode", "exec", "appServer"];
/// Every `ThreadSourceKind` in the 0.160.0 schema.
pub const ALL_SOURCE_KINDS: &[&str] = &[
    "cli",
    "vscode",
    "exec",
    "appServer",
    "subAgent",
    "subAgentReview",
    "subAgentCompact",
    "subAgentThreadSpawn",
    "subAgentOther",
    "unknown",
];

// ---- responses ----

/// One page of a cursor-paged list method (`thread/list`, `thread/loaded/list`,
/// `thread/turns/list`, `thread/queue/list`, `model/list`); also the resume
/// `initialTurnsPage`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", bound(deserialize = "T: DeserializeOwned"))]
pub struct CursorPage<T> {
    #[serde(deserialize_with = "entries")]
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}

/// The entries of a list that decode: one odd entry (a turn without an id, a model without
/// its name) does not fail the whole list, and `null` is an empty list. For
/// `#[serde(deserialize_with = "entries")]`.
fn entries<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let items = Option::<Vec<Value>>::deserialize(d)?.unwrap_or_default();
    Ok(items
        .into_iter()
        .filter_map(|v| {
            T::deserialize(v)
                .inspect_err(|e| tracing::warn!("codex: skipped a list entry: {e}"))
                .ok()
        })
        .collect())
}

/// A field whose value does not fit reads as its default. For
/// `#[serde(default, deserialize_with = "or_default")]`.
fn or_default<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> Result<T, D::Error> {
    Ok(T::deserialize(Value::deserialize(d)?).unwrap_or_default())
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub source: Value,
    pub originator: Option<String>,
    #[serde(default)]
    pub status: ThreadStatus,
    pub name: Option<String>,
    pub preview: Option<String>,
}

#[derive(Deserialize, Debug, Default, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadStatus {
    Idle,
    #[serde(rename_all = "camelCase")]
    Active {
        #[serde(default)]
        active_flags: Vec<String>,
    },
    /// `notLoaded`, `systemError`, or anything newer.
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug)]
pub struct ThreadStartResponse {
    pub thread: Thread,
}

/// `turn/start` and `thread/queue/start`.
#[derive(Deserialize, Debug)]
pub struct TurnResponse {
    pub turn: Turn,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeResponse {
    pub initial_turns_page: Option<CursorPage<Turn>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResponse {
    pub turn_id: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ThreadQueueAddResponse {
    pub queued_submission: QueuedEntry,
}

/// An entry of `thread/queue/list`; also the submission `thread/queue/add` returns.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QueuedEntry {
    pub id: String,
    pub client_user_message_id: Option<String>,
}

/// A turn. Only the id is required: any other field that is missing or does not fit reads
/// as absent (status `Unknown`).
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub items: Vec<Item>,
    #[serde(default, deserialize_with = "or_default")]
    pub status: TurnStatus,
    pub error: Option<Value>,
    #[serde(default, deserialize_with = "or_default")]
    pub duration_ms: Option<i64>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "or_default")]
    pub started_at: Option<i64>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "or_default")]
    pub completed_at: Option<i64>,
}

#[derive(Deserialize, Debug, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
    InProgress,
    #[default]
    #[serde(other)]
    Unknown,
}

/// A `model/list` entry.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model: String,
    #[serde(default, deserialize_with = "entries")]
    pub supported_reasoning_efforts: Vec<EffortOption>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EffortOption {
    pub reasoning_effort: String,
}

// ---- notifications ----

/// `turn/completed`, decoded only for the turn being observed: another turn's completion
/// is skipped by its raw id before decoding.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    pub thread_id: String,
    pub turn: Turn,
}

/// `item/started` and `item/completed`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub turn_id: String,
    pub item: Item,
}

// ---- items ----

/// A turn item. Only user and agent messages are read; any other kind, and a user or
/// agent message that does not decode, is `Other`, never displayed or matched.
#[derive(Deserialize, Debug)]
#[serde(from = "Value")]
pub enum Item {
    User(UserMessage),
    Agent(AgentMessage),
    Other,
}

impl From<Value> for Item {
    fn from(v: Value) -> Item {
        match v["type"].as_str() {
            Some("userMessage") => UserMessage::deserialize(&v).map_or(Item::Other, Item::User),
            Some("agentMessage") => AgentMessage::deserialize(&v).map_or(Item::Other, Item::Agent),
            _ => Item::Other,
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub id: String,
    pub client_id: Option<String>,
    #[serde(default)]
    pub content: Vec<Value>,
}

impl UserMessage {
    /// Concatenated text parts; non-text inputs are skipped.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|c| c["type"] == "text")
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Deserialize, Debug)]
pub struct AgentMessage {
    pub id: String,
    #[serde(default)]
    pub text: String,
    pub phase: Option<AgentPhase>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    FinalAnswer,
    Commentary,
    #[serde(other)]
    Other,
}

// ---- wire envelope ----

/// One incoming JSON-RPC message. Server requests carry both `id` and `method`.
#[derive(Deserialize, Debug)]
pub struct Incoming {
    pub id: Option<Value>,
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    pub result: Option<Value>,
    pub error: Option<AgentError>,
}

// ---- errors ----

/// Classify a Codex error. `-32600` is reused for several unrelated conditions
/// (not initialized, missing capability, stale or idle steer), so the message decides.
pub fn classify_error(e: &AgentError) -> ErrorCode {
    if e.message.contains("requires experimentalApi") {
        ErrorCode::CapMissing
    } else if e.code == -32601 {
        ErrorCode::Unsupported
    } else if e.message == "Not initialized" {
        ErrorCode::Transport
    } else {
        ErrorCode::Precondition
    }
}

/// History of a thread created moments ago is not readable yet. Two transient shapes:
/// `-32603 "… rollout at <path> is empty"` (`thread/resume`, `thread/turns/list`) while
/// the rollout file has no line, and `-32601 "list_turns is not supported yet"` from
/// `thread/turns/list` while the thread has no row in the daemon's state DB (its
/// paginated reads check that row first). Other -32603s are thread-store failures and
/// other -32601s are really unsupported methods.
pub fn history_not_ready(e: &Error) -> bool {
    e.agent_error.as_ref().is_some_and(|v| {
        (v.code == -32603 && v.message.ends_with(" is empty"))
            || (v.code == -32601 && v.message == "list_turns is not supported yet")
    })
}

/// `thread/queue/start` found nothing dormant to start: the submission already started
/// (`queued submission not found: <id>`) or waits behind an active turn (DESIGN.md §6.1).
pub fn nothing_dormant(e: &Error) -> bool {
    e.agent_error.as_ref().is_some_and(|v| {
        v.code == -32600
            && (v.message.starts_with("queued submission not found: ")
                || v.message == "thread already has an active or pending turn")
    })
}

/// `thread/resume` refused because another app-server process holds the thread's
/// cross-process writer lock (`~/.codex/thread-writer-locks/<id>.lock`): `-32600 "thread
/// <id> already has an active writer"`. Becomes `E_FOREIGN_LIVE`; the Codex error stays
/// attached. Every other error passes through unchanged.
pub fn resume_error(e: Error) -> Error {
    let held = e
        .agent_error
        .as_ref()
        .is_some_and(|v| v.code == -32600 && v.message.ends_with(" already has an active writer"));
    if !held {
        return e;
    }
    Error {
        code: ErrorCode::ForeignLive,
        message: format!(
            "{}: the thread is open in another Codex app-server process (the ChatGPT desktop app and the VS Code extension run their own), which holds its writer lock; close it there or wait, then retry",
            e.message
        ),
        ..e
    }
}

/// The deny answer for a server request: a schema-valid refusal that
/// lets the turn continue (never `accept`, never `cancel`). `None` for requests
/// that are not approvals; those are never answered.
pub fn deny_response(method: &str) -> Option<Value> {
    Some(match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"decision": "decline"})
        }
        "item/permissions/requestApproval" => json!({"permissions": {}}),
        "item/tool/requestUserInput" => json!({"answers": {}}),
        "mcpServer/elicitation/request" => json!({"action": "decline"}),
        "applyPatchApproval" | "execCommandApproval" => {
            json!({"decision": {"denied": {"rejection": "declined by agent-talk policy"}}})
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Frames captured from the daemon probes of 2026-10-03 (codex-cli 0.160.0).

    const THREAD_START: &str = r#"{"id":3,"result":{"thread":{"id":"01a101c5-fcb5-77c1-804d-5c1bd9358f1f","environments":[{"environmentId":"local","cwd":"/Users/me/projects/demo","runtimeWorkspaceRoots":["/Users/me/projects/demo"]}],"extra":null,"sessionId":"01a101c5-fcb5-77c1-804d-5c1bd9358f1f","forkedFromId":null,"parentThreadId":null,"preview":"","ephemeral":false,"section":null,"sectionEnteredAt":null,"projectId":null,"historyMode":"paginated","modelProvider":"openai","model":"gpt-6-luna","reasoningEffort":"low","createdAt":1791031117,"updatedAt":1791031117,"recencyAt":1791031117,"status":{"type":"idle"},"path":"/Users/me/.codex/sessions/2026/10/03/rollout-2026-10-03T06-38-37-01a101c5-fcb5-77c1-804d-5c1bd9358f1f.jsonl","cwd":"/Users/me/projects/demo","cliVersion":"0.160.0","originator":"codex-tui","source":"vscode","canAcceptDirectInput":true,"threadSource":null,"agentNickname":null,"agentRole":null,"gitInfo":null,"name":null,"daybreakEnabled":null,"turns":[]},"model":"gpt-6-luna","modelProvider":"openai","serviceTier":"default","disabledPluginIds":[],"cwd":"/Users/me/projects/demo","runtimeWorkspaceRoots":["/Users/me/projects/demo"],"instructionSources":["/Users/me/.codex/AGENTS.md"],"approvalPolicy":"on-request","approvalsReviewer":"user","sandbox":{"type":"dangerFullAccess"},"activePermissionProfile":null,"reasoningEffort":"low","multiAgentMode":"explicitRequestOnly"}}"#;

    const TURN_COMPLETED: &str = r#"{"method":"turn/completed","params":{"threadId":"01a101c5-fcb5-77c1-804d-5c1bd9358f1f","turn":{"id":"01a101c6-07f8-7063-a319-3aa3e4bb0727","items":[{"type":"agentMessage","id":"msg_031ce0c66564c80c016ac0f75450ec87d284421c9b2a3cb26c","text":"pong","phase":"final_answer","memoryCitation":null,"delivery":null,"questions":null}],"itemsView":"summary","status":"completed","error":null,"startedAt":1791031117,"completedAt":1791031124,"durationMs":6982}},"emittedAtMs":1791031124810}"#;

    const ITEM_COMPLETED_QUEUED: &str = r#"{"method":"item/completed","params":{"item":{"type":"userMessage","id":"01a101d3-6fea-78b3-b5d8-74a38dd99438","clientId":"9fb55d65-dcab-47ec-a924-700b0e4d1f6b","content":[{"type":"text","text":"reply with the single word queued","text_elements":[]}]},"threadId":"01a101d2-0538-7a30-99f9-d9bf52b5325e","turnId":"01a101d3-6fba-75f2-8968-568ad3ee019e","completedAtMs":1791031996395},"emittedAtMs":1791031996397}"#;

    const STALE_STEER: &str = r#"{"error":{"code":-32600,"message":"expected active turn id `00000000-0000-0000-0000-000000000000` but found `01a101d2-8f39-7ba1-bf9e-da5be93ca820`"},"id":5}"#;

    // The message is `WriterLockCoordinator::acquire`'s (codex-rs rollout/src/writer_lock.rs),
    // asserted verbatim by app-server tests/suite/v2/thread_resume.rs.
    const ACTIVE_WRITER: &str = r#"{"error":{"code":-32600,"message":"thread 01a10b47-8944-7792-aea9-431f178ad170 already has an active writer"},"id":3}"#;

    const NO_EXPERIMENTAL: &str = r#"{"error":{"code":-32600,"message":"thread/queue/list requires experimentalApi capability"},"id":4}"#;

    // Captured from the approval probes (codex-cli 0.160.0).
    const APPROVAL_REQUEST: &str = r#"{"method":"item/commandExecution/requestApproval","id":3,"params":{"kind":"command","threadId":"01a10204-b257-7b52-a296-e728ae62a61a","turnId":"01a10204-ce9b-7f31-b50d-9e0b331d962c","itemId":"exec-d28876d6-a46e-4459-a552-c5388a1ce9ee","startedAtMs":1791035234282,"environmentId":"local","reason":"Approval routing experiment","command":"/bin/zsh -lc 'echo approval-test'","cwd":"/Users/me/projects/demo","commandActions":[{"type":"unknown","command":"echo approval-test"}],"proposedExecpolicyAmendment":["echo","approval-test"],"availableDecisions":["accept",{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["echo","approval-test"]}},"cancel"]}}"#;

    const REQUEST_RESOLVED: &str = r#"{"method":"serverRequest/resolved","params":{"threadId":"01a10204-b257-7b52-a296-e728ae62a61a","requestId":3},"emittedAtMs":1791035255102}"#;

    #[test]
    fn approval_request_and_resolution() {
        let m = incoming(APPROVAL_REQUEST);
        assert_eq!(m.id, Some(json!(3)));
        assert_eq!(
            m.method.as_deref(),
            Some("item/commandExecution/requestApproval")
        );
        assert_eq!(
            m.params["itemId"],
            "exec-d28876d6-a46e-4459-a552-c5388a1ce9ee"
        );
        // The resolution names the request by the same id, so the two must be matched by it.
        let r = incoming(REQUEST_RESOLVED);
        assert!(r.id.is_none());
        assert_eq!(r.params["requestId"], json!(3));
    }

    fn incoming(s: &str) -> Incoming {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn thread_start_result() {
        let m = incoming(THREAD_START);
        assert_eq!(m.id, Some(json!(3)));
        assert!(m.method.is_none());
        let r = ThreadStartResponse::deserialize(m.result.as_ref().unwrap()).unwrap();
        assert_eq!(r.thread.id, "01a101c5-fcb5-77c1-804d-5c1bd9358f1f");
        assert_eq!(r.thread.status, ThreadStatus::Idle);
    }

    #[test]
    fn turn_completed_notification() {
        let m = incoming(TURN_COMPLETED);
        assert!(m.id.is_none());
        assert_eq!(m.method.as_deref(), Some("turn/completed"));
        let n = TurnCompletedNotification::deserialize(&m.params).unwrap();
        assert_eq!(n.thread_id, "01a101c5-fcb5-77c1-804d-5c1bd9358f1f");
        let turn = n.turn;
        assert_eq!(turn.id, "01a101c6-07f8-7063-a319-3aa3e4bb0727");
        assert_eq!(turn.status, TurnStatus::Completed);
        assert!(turn.error.is_none());
        assert_eq!(turn.duration_ms, Some(6982));
        assert_eq!(turn.completed_at, Some(1791031124));
        match &turn.items[0] {
            Item::Agent(a) => {
                assert_eq!(a.text, "pong");
                assert_eq!(a.phase, Some(AgentPhase::FinalAnswer));
            }
            other => panic!("unexpected item {other:?}"),
        }
    }

    #[test]
    fn item_completed_user_message_with_client_id() {
        let m = incoming(ITEM_COMPLETED_QUEUED);
        let n = ItemNotification::deserialize(&m.params).unwrap();
        assert_eq!(n.turn_id, "01a101d3-6fba-75f2-8968-568ad3ee019e");
        match n.item {
            Item::User(u) => {
                assert_eq!(
                    u.client_id.as_deref(),
                    Some("9fb55d65-dcab-47ec-a924-700b0e4d1f6b")
                );
                assert_eq!(u.text(), "reply with the single word queued");
            }
            other => panic!("unexpected item {other:?}"),
        }
    }

    #[test]
    fn invalid_request_errors() {
        let e = incoming(STALE_STEER).error.unwrap();
        assert_eq!(e.code, -32600);
        assert_eq!(classify_error(&e), ErrorCode::Precondition);

        let e = incoming(NO_EXPERIMENTAL).error.unwrap();
        assert_eq!(classify_error(&e), ErrorCode::CapMissing);
    }

    #[test]
    fn active_writer_is_foreign_live() {
        let v = incoming(ACTIVE_WRITER).error.unwrap();
        let e = resume_error(Error::from_agent(classify_error(&v), v));
        assert_eq!(e.code, ErrorCode::ForeignLive);
        assert!(e.message.contains("another Codex app-server process"));
        let v = e.agent_error.unwrap();
        assert_eq!(v.code, -32600);
        assert_eq!(
            v.message,
            "thread 01a10b47-8944-7792-aea9-431f178ad170 already has an active writer"
        );

        // Other -32600s keep their class.
        let v = incoming(STALE_STEER).error.unwrap();
        let e = resume_error(Error::from_agent(classify_error(&v), v));
        assert_eq!(e.code, ErrorCode::Precondition);
    }

    fn error(frame: &str) -> Error {
        let v = incoming(frame).error.unwrap();
        Error::from_agent(classify_error(&v), v)
    }

    /// The new-thread window's two shapes are retried or read as no history; the same
    /// codes with other messages are real failures.
    #[test]
    fn history_not_ready_errors() {
        // The shape recorded in DESIGN.md §6.1; the text before "rollout at" was not kept.
        let empty = r#"{"error":{"code":-32603,"message":"failed to load thread: rollout at /Users/me/.codex/sessions/2026/10/03/rollout-2026-10-03T06-38-37-01a101c5-fcb5-77c1-804d-5c1bd9358f1f.jsonl is empty"},"id":4}"#;
        let no_row =
            r#"{"error":{"code":-32601,"message":"list_turns is not supported yet"},"id":4}"#;
        assert!(history_not_ready(&error(empty)));
        assert!(history_not_ready(&error(no_row)));

        let store = r#"{"error":{"code":-32603,"message":"failed to read thread store"},"id":4}"#;
        let unknown = r#"{"error":{"code":-32601,"message":"Unknown method thread/foo"},"id":4}"#;
        assert!(!history_not_ready(&error(store)));
        assert!(!history_not_ready(&error(unknown)));
        assert!(!history_not_ready(&Error::new(ErrorCode::Timeout, "x")));
    }

    /// Only the two `thread/queue/start` refusals that mean nothing is dormant are
    /// swallowed; any other refusal reaches the caller.
    #[test]
    fn nothing_dormant_errors() {
        let started = r#"{"error":{"code":-32600,"message":"queued submission not found: 01a101d3-6fea-78b3-b5d8-74a38dd99438"},"id":6}"#;
        let behind = r#"{"error":{"code":-32600,"message":"thread already has an active or pending turn"},"id":6}"#;
        assert!(nothing_dormant(&error(started)));
        assert!(nothing_dormant(&error(behind)));
        assert!(!nothing_dormant(&error(STALE_STEER)));
        assert!(!nothing_dormant(&error(NO_EXPERIMENTAL)));
    }

    /// Settings a turn does not change are left out, not sent as `null`.
    #[test]
    fn turn_start_omits_unset_settings() {
        let p = TurnStartParams {
            thread_id: "t",
            input: text_input("hi"),
            client_user_message_id: "c",
            model: None,
            effort: Some("low"),
            sandbox_policy: None,
        };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"threadId": "t", "input": text_input("hi"), "clientUserMessageId": "c", "effort": "low"})
        );
    }

    #[test]
    fn deny_policy_declines_approvals_only() {
        assert_eq!(
            deny_response("item/commandExecution/requestApproval"),
            Some(json!({"decision": "decline"}))
        );
        assert_eq!(deny_response("account/chatgptAuthTokens/refresh"), None);
    }

    /// One odd entry does not fail a page: a turn whose status or times do not fit keeps
    /// its id, a turn without an id is skipped, and so are a model without its name and an
    /// effort entry without its effort.
    #[test]
    fn odd_list_entries() {
        let turns: CursorPage<Turn> = serde_json::from_str(
            r#"{"data":[{"id":"t2","items":null,"status":null,"startedAt":"soon","completedAt":1791031124.5,"durationMs":"x"},{"status":"completed"},{"id":"t1","items":[],"status":"paused"}],"nextCursor":null}"#,
        )
        .unwrap();
        let ids: Vec<(&str, TurnStatus)> = turns
            .data
            .iter()
            .map(|t| (t.id.as_str(), t.status))
            .collect();
        assert_eq!(
            ids,
            [("t2", TurnStatus::Unknown), ("t1", TurnStatus::Unknown)]
        );
        let t = &turns.data[0];
        assert_eq!(
            (t.started_at, t.completed_at, t.duration_ms),
            (None, None, None)
        );

        let models: CursorPage<ModelInfo> = serde_json::from_str(
            r#"{"data":[{"displayName":"no id"},{"model":"gpt-5.5","supportedReasoningEfforts":[{"reasoningEffort":"low"},{}]},{"model":"gpt-5.5-mini","supportedReasoningEfforts":null}],"nextCursor":null}"#,
        )
        .unwrap();
        let models: Vec<(&str, Vec<&str>)> = models
            .data
            .iter()
            .map(|m| {
                let efforts = m.supported_reasoning_efforts.iter();
                (
                    m.model.as_str(),
                    efforts.map(|e| e.reasoning_effort.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(models, [("gpt-5.5", vec!["low"]), ("gpt-5.5-mini", vec![])]);
    }
}
