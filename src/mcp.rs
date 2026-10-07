//! `agent-talk mcp`: the CLI's models, ls, new, send, read and wait as MCP tools on stdio.
//!
//! Each tool builds the same request the CLI builds and runs it through `ops::run`,
//! so results are exactly what `--json` prints, and refusals are the CLI's error object
//! returned as a tool error (`isError: true`), not a protocol error.

use crate::agents::{ReadQuery, Settings, WaitTarget};
use crate::model::{self, Caller, Error, ErrorCode, Outcome};
use crate::ops::{
    self, DEFAULT_TIMEOUT, LS_LIMIT, LsArgs, MODELS_LIMIT, Models, ModelsArgs, NewArgs, READ_LIMIT,
    Read, ReadArgs, Request, SendArgs, Sessions, WaitArgs, caller_from_handle, env_var,
};
use crate::store::Store;
use rmcp::handler::server::tool::schema_for_type;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, RequestMetaObject, ServerCapabilities,
    ServerConfig,
};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

const INSTRUCTIONS: &str = "Tools for sending messages to sessions of other agents on this \
machine and reading their replies. The agents are codex (Codex), claude (Claude Code), opencode \
(OpenCode), grok (Grok CLI), antigravity (Antigravity CLI) and pi. A session is identified by a \
handle, <agent>:<id>, which new and ls return. A turn can take minutes. If new or send times \
out or loses its connection, the message may already have been delivered. Resending can \
duplicate it. When the error includes a receipt, use its receipt_id with wait to check the \
outcome. Without a receipt, use ls to locate the session and read to inspect its history. \
A successful tool call can return a failed or interrupted turn. turn.status reports the \
outcome, and approvals records permission requests and denials.";

#[derive(Clone)]
pub struct Server {
    /// `--caller`, else AGENT_TALK_CALLER: an explicit pin, wins over everything.
    pinned: Option<Caller>,
    /// The session named by this server's environment at startup: `grok:<GROK_SESSION_ID>`,
    /// else `claude:<CLAUDE_CODE_SESSION_ID>`. Grok and Claude Code set their variable for
    /// each MCP server they spawn (one server per session) and send no session id in
    /// `_meta` (DESIGN.md §4). Grok's goes first because Claude's is also inherited by
    /// every shell command.
    env: Option<Caller>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
struct ModelsParams {
    /// `codex`, `claude`, `opencode`, `grok`, `antigravity` or `pi`.
    agent: String,
    /// Filter model IDs by a case-insensitive substring.
    query: Option<String>,
    /// Models per page (default 50).
    limit: Option<usize>,
    /// `next_cursor` of a previous page.
    cursor: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct LsParams {
    /// Filter by agent: `codex`, `claude`, `opencode`, `grok`, `antigravity` or `pi`. Lists all agents when omitted.
    agent: Option<String>,
    /// Only sessions in this directory, given as an absolute path.
    cwd: Option<String>,
    /// Sessions per agent per page. Defaults to 25.
    limit: Option<u32>,
    /// The value of `next_cursors[agent]` from the previous response. Requires the same agent.
    cursor: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct NewParams {
    /// `codex`, `claude`, `opencode`, `grok`, `antigravity` or `pi`.
    agent: String,
    /// Working directory of the session, given as an absolute path.
    cwd: String,
    /// The first message.
    prompt: String,
    /// A model ID returned by `models`. The agent's default applies when omitted.
    model: Option<String>,
    /// A title that ls shows. Antigravity does not support titles.
    name: Option<String>,
    /// An effort value returned by `models` for the selected model. Defaults to the agent's setting. Antigravity requires this when model is supplied.
    effort: Option<String>,
    /// Allow file edits, commands and network access without approval prompts. Defaults to false, which uses the agent's own permissions. The session runs unattended, so actions requiring approval are denied by the agent or declined by agent-talk.
    full_access: Option<bool>,
    /// Wait for the first turn to end and include its result. Defaults to false.
    wait: Option<bool>,
    /// Seconds to wait when wait=true. Defaults to 600. Use a shorter limit than the client's tool timeout to leave time for a response. On Grok without a leader, this timeout cancels a running turn.
    timeout_s: Option<u64>,
}

fn wait_for(wait: Option<bool>, timeout_s: Option<u64>) -> Option<Duration> {
    wait.unwrap_or(false)
        .then(|| timeout_s.map_or(DEFAULT_TIMEOUT, Duration::from_secs))
}

#[derive(Deserialize, JsonSchema)]
struct SendParams {
    /// Session handle from ls or new.
    handle: String,
    /// The message.
    text: String,
    /// Add the message to the running turn. Defaults to false. Supported on Codex, OpenCode and Grok through a live leader. Refused when the session is idle.
    steer: Option<bool>,
    /// Switch the session to this model ID (from `models`) from this message on. Only on sessions created by agent-talk.
    model: Option<String>,
    /// Switch the session to this effort value (from `models`) from this message on. Only on sessions created by agent-talk.
    effort: Option<String>,
    /// Give the session file edits, commands and network access without approval prompts, from this message on. Only on sessions created by agent-talk. Cannot be turned off again.
    full_access: Option<bool>,
    /// Wait for the turn to end and include its result. Defaults to false.
    wait: Option<bool>,
    /// Seconds to wait when wait=true. Defaults to 600. Use a shorter limit than the client's tool timeout to leave time for a response. On Grok without a leader, this timeout cancels a running turn.
    timeout_s: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct ReadParams {
    /// Session handle.
    handle: String,
    /// Messages per page (default 20).
    limit: Option<usize>,
    /// `next_cursor` of a previous page, for older messages.
    cursor: Option<String>,
    /// Include intermediate text and tool calls. Defaults to false.
    all: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitParams {
    /// Session handle.
    handle: String,
    /// The turn ID. Mutually exclusive with receipt.
    turn: Option<String>,
    /// The receipt_id returned by new or send. Mutually exclusive with turn.
    receipt: Option<String>,
    /// Seconds to wait. Defaults to 600. Use a shorter limit than the client's tool timeout to leave time for a response.
    timeout_s: Option<u64>,
}

fn invalid(message: &str) -> CallToolResult {
    tool_error(Error::new(ErrorCode::Precondition, message))
}

fn tool_error(e: Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(json!({ "error": e }).to_string())])
}

impl Server {
    fn new(pinned: Option<Caller>, env: Option<Caller>) -> Self {
        Server {
            pinned,
            env,
            tool_router: Self::tool_router(),
        }
    }

    /// Sender of one call, explicit pins first, then what the agent says:
    /// 1. `--caller`, else AGENT_TALK_CALLER;
    /// 2. the calling Codex thread from the request's `_meta` (`threadId`, else
    ///    `x-codex-turn-metadata.thread_id`; sent on every tools/call; DESIGN.md §4),
    ///    with its turn id. Checked before the environment: a Codex daemon started from a
    ///    Claude shell carries an unrelated CLAUDE_CODE_SESSION_ID;
    /// 3. the calling OpenCode session from `_meta["ai.opencode/sessionID"]` (sent on every
    ///    tools/call). One stdio server serves every session of a
    ///    directory, so the environment says nothing about the caller;
    /// 4. the calling Antigravity conversation from `_meta["antigravity.google/conversation_id"]`
    ///    (sent on every tools/call);
    /// 5. the Grok or Claude session from this server's environment (`env`), with
    ///    `claudecode/toolUseId` from `_meta` as a correlation hint when present (only
    ///    Claude sends it);
    /// 6. unknown.
    fn sender(&self, meta: &RequestMetaObject) -> Caller {
        if let Some(c) = &self.pinned {
            return c.clone();
        }
        let meta = &meta.0.0;
        let text = |v: Option<&serde_json::Value>| {
            v.and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let turn_meta = meta.get("x-codex-turn-metadata");
        let thread =
            text(meta.get("threadId")).or_else(|| text(turn_meta.and_then(|t| t.get("thread_id"))));
        if let Some(t) = thread {
            return Caller {
                turn: text(turn_meta.and_then(|t| t.get("turn_id"))),
                ..Caller::agent(&format!("codex:{t}"))
            };
        }
        if let Some(s) = text(meta.get("ai.opencode/sessionID")) {
            return Caller::agent(&format!("opencode:{s}"));
        }
        if let Some(c) = text(meta.get("antigravity.google/conversation_id")) {
            return Caller::agent(&format!("antigravity:{c}"));
        }
        match &self.env {
            Some(c) => Caller {
                turn: text(meta.get("claudecode/toolUseId")),
                ..c.clone()
            },
            None => Caller::UNKNOWN,
        }
    }

    /// Run one request and return what `--json` would print.
    async fn exec(&self, req: Request) -> CallToolResult {
        // Agent futures hold the store's rusqlite connection (Send, not Sync), so they
        // are not Send; run each request on a blocking thread of the same runtime.
        let rt = tokio::runtime::Handle::current();
        let res = tokio::task::spawn_blocking(move || {
            rt.block_on(async {
                let store = Store::open()?;
                ops::run(&store, req).await
            })
        })
        .await;
        match res {
            Ok(Ok(out)) => CallToolResult::structured(serde_json::to_value(&out).unwrap()),
            Ok(Err(e)) => tool_error(e),
            Err(e) => tool_error(Error::new(
                ErrorCode::Transport,
                format!("command failed: {e}"),
            )),
        }
    }
}

#[tool_router]
impl Server {
    #[tool(
        title = "List models",
        output_schema = schema_for_type::<Models>(),
        annotations(read_only_hint = true, open_world_hint = false),
        description = "List model IDs and their supported effort values for the model and effort parameters of new. Results are sorted by model ID. OpenCode includes every configured provider, so a query can narrow the results by provider or model family. Pass next_cursor as cursor to get the next page."
    )]
    async fn models(&self, Parameters(p): Parameters<ModelsParams>) -> CallToolResult {
        self.exec(Request::Models(ModelsArgs {
            agent: p.agent,
            query: p.query,
            limit: p.limit.unwrap_or(MODELS_LIMIT),
            cursor: p.cursor,
        }))
        .await
    }

    #[tool(
        title = "List sessions",
        output_schema = schema_for_type::<Sessions>(),
        annotations(read_only_hint = true, open_world_hint = false),
        description = "List sessions on this machine, newest first for each agent. Each entry includes a handle, working directory, state and a preview of the first message. Each agent has a separate cursor in next_cursors. To get its next page, pass next_cursors[agent] as cursor and select that agent."
    )]
    async fn ls(&self, Parameters(p): Parameters<LsParams>) -> CallToolResult {
        self.exec(Request::Ls(LsArgs {
            agent: p.agent,
            cwd: p.cwd.map(PathBuf::from),
            all: false,
            limit: p.limit.unwrap_or(LS_LIMIT),
            cursor: p.cursor,
        }))
        .await
    }

    #[tool(
        name = "new",
        title = "New session",
        output_schema = schema_for_type::<Outcome>(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false),
        description = "Start an agent session in cwd and send its first message. The session does not inherit the caller's conversation history. Returns a handle and a receipt. With wait=true, waits for the first turn to end and includes its result, with the reply in turn.final_text. With wait=false, Codex and OpenCode return after accepting the message. Claude Code, Grok and Antigravity still wait for the turn to end."
    )]
    async fn new_session(
        &self,
        meta: RequestMetaObject,
        Parameters(p): Parameters<NewParams>,
    ) -> CallToolResult {
        self.exec(Request::New(NewArgs {
            agent: p.agent,
            cwd: PathBuf::from(p.cwd),
            prompt: p.prompt,
            name: p.name,
            settings: Settings {
                model: p.model,
                effort: p.effort,
                full_access: p.full_access.unwrap_or(false),
            },
            wait: wait_for(p.wait, p.timeout_s),
            from: self.sender(&meta),
        }))
        .await
    }

    #[tool(
        title = "Send message",
        output_schema = schema_for_type::<Outcome>(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false),
        description = "Continue an existing session by sending a message. On Codex, OpenCode and Grok through a live leader, a message sent during a turn is queued by default. model, effort and full_access change the session's settings first; they work only on sessions created by agent-talk and not with steer. Returns a receipt. With wait=true, waits for the turn to end and includes its result, with the reply in turn.final_text. With wait=false, Codex and OpenCode return after accepting the message. Claude Code, Grok and Antigravity still wait for the turn to end. One OpenCode reply can cover several queued messages. Use read to see them in the conversation."
    )]
    async fn send(
        &self,
        meta: RequestMetaObject,
        Parameters(p): Parameters<SendParams>,
    ) -> CallToolResult {
        self.exec(Request::Send(SendArgs {
            handle: p.handle,
            text: p.text,
            steer: p.steer.unwrap_or(false),
            settings: Settings {
                model: p.model,
                effort: p.effort,
                full_access: p.full_access.unwrap_or(false),
            },
            wait: wait_for(p.wait, p.timeout_s),
            from: self.sender(&meta),
        }))
        .await
    }

    #[tool(
        title = "Read history",
        output_schema = schema_for_type::<Read>(),
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Read a session's newest messages, oldest first. By default these are the messages sent to it and its final replies. Pass next_cursor as cursor to get older messages."
    )]
    async fn read(&self, Parameters(p): Parameters<ReadParams>) -> CallToolResult {
        self.exec(Request::Read(ReadArgs {
            handle: p.handle,
            query: ReadQuery {
                limit: p.limit.unwrap_or(READ_LIMIT),
                before: p.cursor,
                all: p.all.unwrap_or(false),
            },
            raw: false,
        }))
        .await
    }

    #[tool(
        title = "Wait for turn",
        output_schema = schema_for_type::<Outcome>(),
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Retrieve the result of a turn, waiting if it is still running or queued. Provide exactly one of receipt or turn, together with the session handle. Returns the turn with its reply in turn.final_text. A turn that has already ended returns immediately. After E_TIMEOUT, call wait again with the same arguments to continue observing."
    )]
    async fn wait(&self, Parameters(p): Parameters<WaitParams>) -> CallToolResult {
        let target = match (p.turn, p.receipt) {
            (Some(t), None) => WaitTarget::Turn(t),
            (None, Some(r)) => WaitTarget::Receipt(r),
            _ => return invalid("pass exactly one of turn or receipt"),
        };
        self.exec(Request::Wait(WaitArgs {
            handle: p.handle,
            target,
            timeout: p.timeout_s.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
        }))
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agent-talk", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

/// Serve until the client closes stdin.
pub async fn serve(caller_arg: Option<String>) -> model::Result<()> {
    // Reject a malformed handle before accepting requests.
    let pinned = caller_arg
        .or_else(|| env_var("AGENT_TALK_CALLER"))
        .as_deref()
        .map(caller_from_handle)
        .transpose()?;
    let env = [
        ("GROK_SESSION_ID", "grok"),
        ("CLAUDE_CODE_SESSION_ID", "claude"),
    ]
    .into_iter()
    .find_map(|(var, agent)| env_var(var).map(|s| Caller::agent(&format!("{agent}:{s}"))));
    tracing::info!("agent-talk mcp: pinned {pinned:?}, env {env:?}");
    let server = Server::new(pinned, env);
    let transport_err =
        |e: &dyn std::fmt::Display| Error::new(ErrorCode::Transport, format!("mcp: {e}"));
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| transport_err(&e))?;
    running.waiting().await.map_err(|e| transport_err(&e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(v: serde_json::Value) -> RequestMetaObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn sender_resolution_order() {
        // `_meta` shapes captured from codex-cli 0.160.0 and claude 2.1.288, trimmed.
        let codex = meta(serde_json::json!({
            "progressToken": 1,
            "threadId": "01a10287-b62c",
            "x-codex-turn-metadata": {"thread_id": "01a10287-b62c", "turn_id": "01a10287-f47a"},
        }));
        let nested_only = meta(serde_json::json!({
            "x-codex-turn-metadata": {"thread_id": "t2", "turn_id": "u2"},
        }));
        let claude = meta(serde_json::json!({
            "claudecode/toolUseId": "toolu_01GLvvh7",
            "progressToken": 4,
        }));
        let pin = Caller::agent("claude:pinned");
        let env = Caller::agent("claude:env");

        // 1. A pin wins over _meta and the environment, and carries no turn hint.
        let pinned = Server::new(Some(pin.clone()), Some(env.clone()));
        assert_eq!(pinned.sender(&codex), pin);
        assert_eq!(pinned.sender(&claude), pin);

        // 2. Codex _meta wins over CLAUDE_CODE_SESSION_ID (a daemon may inherit an unrelated one).
        let b = Server::new(None, Some(env.clone()));
        let c = b.sender(&codex);
        assert_eq!(c.session.as_deref(), Some("codex:01a10287-b62c"));
        assert_eq!(c.turn.as_deref(), Some("01a10287-f47a"));
        assert_eq!(b.sender(&nested_only).session.as_deref(), Some("codex:t2"));

        // 3. OpenCode: `_meta["ai.opencode/sessionID"]` (source-derived key),
        //    before the Claude environment the shared server may have inherited.
        let opencode = meta(serde_json::json!({"ai.opencode/sessionID": "ses_abc"}));
        assert_eq!(
            b.sender(&opencode).session.as_deref(),
            Some("opencode:ses_abc")
        );

        // 4. Antigravity: `_meta["antigravity.google/conversation_id"]`.
        let agy = meta(serde_json::json!({"antigravity.google/conversation_id": "a1b2"}));
        assert_eq!(b.sender(&agy).session.as_deref(), Some("antigravity:a1b2"));

        // 5. The environment (Grok or Claude), with the tool-use id as hint.
        let c = b.sender(&claude);
        assert_eq!(c.session.as_deref(), Some("claude:env"));
        assert_eq!(c.turn.as_deref(), Some("toolu_01GLvvh7"));
        assert_eq!(b.sender(&meta(serde_json::json!({}))), env);

        // 6. Nothing: unknown.
        let bare = Server::new(None, None);
        assert_eq!(bare.sender(&claude), Caller::UNKNOWN);
    }
}
