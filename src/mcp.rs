//! `agent-talk mcp`: the CLI's ls, new, send, read and wait as MCP tools on stdio.
//!
//! Each tool builds the same request the CLI builds and runs it through `ops::run`,
//! so results are exactly what `--json` prints, and refusals are the CLI's error object
//! returned as a tool error (`isError: true`), not a protocol error.

use crate::agents::{Mode, ReadRange, WaitTarget};
use crate::model::{self, Caller, Error, ErrorCode, Outcome};
use crate::ops::{
    self, DEFAULT_TIMEOUT, LS_LIMIT, LsArgs, NewArgs, READ_LIMIT, Read, ReadArgs, Request,
    SendArgs, Sessions, WaitArgs, caller_from_handle, env_var,
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

const INSTRUCTIONS: &str = "Tools to talk to other agent sessions (Codex threads, Claude Code \
sessions, OpenCode sessions, Grok CLI sessions, Antigravity CLI conversations) on this machine: \
find one with ls, start one with new, ask it something with send (wait=true returns its reply), \
read its history with read, and wait for a turn with wait. Messages you send are attributed to \
your own session (this server's --caller or AGENT_TALK_CALLER, else the calling Codex thread, \
OpenCode session, Antigravity conversation, Grok session or Claude session), which is \
attribution, not authentication. Agent-to-agent chains are limited: a send or new deeper than \
the hop limit is refused with E_MAX_HOPS; report the refusal instead of retrying.";

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
    max_hops: u32,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
struct LsParams {
    /// `codex`, `claude`, `opencode`, `grok` or `antigravity`; default: all.
    agent: Option<String>,
    /// Only sessions whose working directory is this absolute path.
    cwd: Option<String>,
    /// Page size (default 25).
    limit: Option<u32>,
    /// `next_cursor` from a previous call with the same agent.
    cursor: Option<String>,
    /// Include the agent's raw record under `raw` (large; default false).
    raw: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct NewParams {
    /// `codex`, `claude`, `opencode`, `grok` or `antigravity`.
    agent: String,
    /// Working directory of the new session (absolute path).
    cwd: String,
    /// First message for the new session.
    prompt: String,
    /// Model name for the agent (e.g. a Codex model id, `sonnet` for Claude, `grok-4.7`
    /// for Grok, `gemini-3.8-flash` for Antigravity).
    model: Option<String>,
    /// Short title for the session, stored by the agent and shown by ls; name it so you
    /// can find it again (Antigravity has no title interface and refuses it).
    name: Option<String>,
    /// Reasoning effort (Codex, Grok, Antigravity; the model's values, e.g. low, medium, high).
    effort: Option<String>,
    /// Give the session every permission, with no sandbox and no approval prompts (default
    /// false: the agent's own configuration).
    full_access: Option<bool>,
    /// Wait for the first turn to finish and return it (reply in `turn.final_text`).
    wait: Option<bool>,
    /// Seconds to wait with wait=true (default 600). On timeout the outcome is unknown
    /// and the receipt is kept; use `wait` with the receipt id later.
    timeout_s: Option<u64>,
    /// Include the agent's raw record under `raw` (large; default false).
    raw: Option<bool>,
}

fn wait_for(wait: Option<bool>, timeout_s: Option<u64>) -> Option<Duration> {
    wait.unwrap_or(false)
        .then(|| timeout_s.map_or(DEFAULT_TIMEOUT, Duration::from_secs))
}

#[derive(Deserialize, JsonSchema)]
struct SendParams {
    /// Session handle from ls or new, e.g. `codex:<thread id>`, `claude:<uuid>`,
    /// `opencode:<ses_…>`, `grok:<uuid>` or `antigravity:<uuid>`.
    handle: String,
    /// The message.
    text: String,
    /// `queue` (default): delivered after the current reply (Codex, Grok: as its own turn;
    /// OpenCode: inside the same execution; Antigravity: as the next turn). `steer`: added
    /// to the running turn (Codex, OpenCode, Grok through a live leader); refused when the
    /// session is idle.
    mode: Option<Mode>,
    /// Wait for the turn that consumes the message and return it (reply in
    /// `turn.final_text`; on OpenCode the final reply of the execution that consumed it,
    /// which may also have answered other messages queued meanwhile).
    wait: Option<bool>,
    /// Seconds to wait with wait=true (default 600).
    timeout_s: Option<u64>,
    /// Receipt, turn or message id this message answers; sets the hop depth from that
    /// message instead of from the newest message delivered to your own session.
    reply_to: Option<String>,
    /// Include the agent's raw record under `raw` (large; default false).
    raw: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ReadParams {
    /// Session handle.
    handle: String,
    /// The newest N messages (oldest first). Use this to see the latest exchange.
    tail: Option<u32>,
    /// `next_cursor` from a previous read, to page forward from the oldest messages.
    since: Option<String>,
    /// Page size when paging forward (default 20; Codex counts turns, OpenCode message rows,
    /// Claude, Grok and Antigravity messages).
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitParams {
    /// Session handle.
    handle: String,
    /// Turn id to wait for.
    turn: Option<String>,
    /// Receipt id from a send or new, to wait for the turn that consumed it.
    receipt: Option<String>,
    /// Seconds to wait (default 600).
    timeout_s: Option<u64>,
    /// Include the agent's raw record under `raw` (large; default false).
    raw: Option<bool>,
}

fn invalid(message: &str) -> CallToolResult {
    tool_error(Error::new(ErrorCode::Precondition, message))
}

fn tool_error(e: Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(json!({ "error": e }).to_string())])
}

impl Server {
    fn new(pinned: Option<Caller>, env: Option<Caller>, max_hops: u32) -> Self {
        Server {
            pinned,
            env,
            max_hops,
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
        title = "List sessions",
        output_schema = schema_for_type::<Sessions>(),
        annotations(read_only_hint = true, open_world_hint = false),
        description = "List agent sessions on this machine (Codex threads, Claude Code sessions, OpenCode sessions, Grok CLI sessions, Antigravity CLI conversations) that you can message: handle, agent, cwd, state, a preview of the first prompt, and observations (loaded, origin). Use it to find another agent session to talk to; pass its handle to send or read."
    )]
    async fn ls(&self, Parameters(p): Parameters<LsParams>) -> CallToolResult {
        self.exec(Request::Ls(LsArgs {
            agent: p.agent,
            cwd: p.cwd.map(PathBuf::from),
            all: false,
            limit: p.limit.unwrap_or(LS_LIMIT),
            cursor: p.cursor,
            raw: p.raw.unwrap_or(false),
        }))
        .await
    }

    #[tool(
        name = "new",
        title = "New session",
        output_schema = schema_for_type::<Outcome>(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false),
        description = "Start a new agent session (agent codex, claude, opencode, grok or antigravity) in a directory with a first prompt, e.g. to hand a task to a fresh agent; give it a name to find it again in ls. Returns its handle and a receipt; with wait=true also the finished first turn, whose reply is turn.final_text. agent-talk prefixes your prompt with a one-line provenance header naming your session; do not add your own. Refused with E_MAX_HOPS when this would exceed the hop limit of agent-to-agent chains; report that instead of retrying."
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
            model: p.model,
            name: p.name,
            effort: p.effort,
            full_access: p.full_access.unwrap_or(false),
            wait: wait_for(p.wait, p.timeout_s),
            from: self.sender(&meta),
            max_hops: self.max_hops,
            raw: p.raw.unwrap_or(false),
        }))
        .await
    }

    #[tool(
        title = "Send message",
        output_schema = schema_for_type::<Outcome>(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false),
        description = "Send a message to another agent session (handle from ls or new). To ask it a question and wait for its reply, set wait=true: the reply is turn.final_text (on OpenCode, the final reply of the execution that consumed your message; if other messages were queued to that session meanwhile it may answer them together, so check the adjacent messages with read when that matters). agent-talk prefixes your message with a one-line provenance header naming your session; do not add your own. Without wait it returns a receipt once the message is accepted (Claude, Grok and Antigravity sessions still run the turn before returning). Refusals come back as an error object with a code; do not retry them: E_MAX_HOPS when the message would exceed the hop limit of an agent-to-agent chain, E_FOREIGN_LIVE when the session is held by a live process agent-talk did not start (e.g. a Claude Code session open in a terminal), E_LOCKED when another agent-talk command is running it. E_TIMEOUT means the outcome is unknown; use wait with the receipt id."
    )]
    async fn send(
        &self,
        meta: RequestMetaObject,
        Parameters(p): Parameters<SendParams>,
    ) -> CallToolResult {
        self.exec(Request::Send(SendArgs {
            handle: p.handle,
            text: p.text,
            mode: p.mode.unwrap_or(Mode::Queue),
            expect_turn: None,
            reply_to: p.reply_to,
            wait: wait_for(p.wait, p.timeout_s),
            from: self.sender(&meta),
            max_hops: self.max_hops,
            raw: p.raw.unwrap_or(false),
        }))
        .await
    }

    #[tool(
        title = "Read history",
        output_schema = schema_for_type::<Read>(),
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Read the conversation of another agent session: user and assistant messages, oldest first, each with turn id and, for messages sent through agent-talk, who sent them (from). Use tail=N for the latest messages, or page forward from the start with since/limit."
    )]
    async fn read(&self, Parameters(p): Parameters<ReadParams>) -> CallToolResult {
        let range = match p.tail {
            Some(_) if p.since.is_some() || p.limit.is_some() => {
                return invalid("pass tail, or since/limit, not both");
            }
            Some(n) => ReadRange::Tail(n),
            None => ReadRange::Forward {
                since: p.since,
                limit: p.limit.unwrap_or(READ_LIMIT),
            },
        };
        self.exec(Request::Read(ReadArgs {
            handle: p.handle,
            range,
            raw: false,
        }))
        .await
    }

    #[tool(
        title = "Wait for turn",
        output_schema = schema_for_type::<Outcome>(),
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Wait for a turn of another agent session to finish and return it (reply in turn.final_text): pass the receipt id from a send or new without wait (or one that timed out), or a turn id. Returns at once if the turn already finished. On OpenCode a turn is one execution, which may have answered several queued messages."
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
            raw: p.raw.unwrap_or(false),
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
pub async fn serve(caller_arg: Option<String>, max_hops: u32) -> model::Result<()> {
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
    tracing::info!("agent-talk mcp: pinned {pinned:?}, env {env:?}, max hops {max_hops}");
    let server = Server::new(pinned, env, max_hops);
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
        let pinned = Server::new(Some(pin.clone()), Some(env.clone()), 3);
        assert_eq!(pinned.sender(&codex), pin);
        assert_eq!(pinned.sender(&claude), pin);

        // 2. Codex _meta wins over CLAUDE_CODE_SESSION_ID (a daemon may inherit an unrelated one).
        let b = Server::new(None, Some(env.clone()), 3);
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
        let bare = Server::new(None, None, 3);
        assert_eq!(bare.sender(&claude), Caller::UNKNOWN);
    }
}
