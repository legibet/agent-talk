//! The five operations behind both front-ends (CLI and MCP): handle parsing, sender
//! provenance header, hop limit, provider dispatch, typed output.

use crate::model::{Caller, Error, ErrorCode, Message, Outcome, Result, Session};
use crate::providers::{
    Adapter, ApprovalPolicy, Caps, ListFilter, Mode, Provider, ReadRange, SendRequest,
    StartRequest, WaitTarget, delivered,
};
use crate::store::Store;
use futures_util::future::join_all;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
pub const DEFAULT_MAX_HOPS: u32 = 3;
pub const LS_LIMIT: u32 = 25;
pub const READ_LIMIT: u32 = 20;

pub enum Request {
    /// `sender` is who this shell's `new` and `send` would be attributed to.
    Caps {
        sender: Caller,
    },
    Ls(LsArgs),
    New(NewArgs),
    Send(SendArgs),
    Read(ReadArgs),
    Wait(WaitArgs),
}

pub struct LsArgs {
    /// One provider, or every provider's first page.
    pub provider: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Include sub-sessions too (`ListFilter::all`).
    pub all: bool,
    pub limit: u32,
    /// Only with `provider`: cursors belong to one provider.
    pub cursor: Option<String>,
    /// Keep the vendor records (`raw`) in the output.
    pub raw: bool,
}

pub struct NewArgs {
    pub provider: String,
    pub cwd: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    /// Vendor-side title of the new session.
    pub name: Option<String>,
    pub effort: Option<String>,
    /// Claude only.
    pub max_turns: Option<u32>,
    /// Codex only.
    pub approval_policy: Option<String>,
    /// Codex only.
    pub sandbox: Option<String>,
    pub approvals: Option<ApprovalPolicy>,
    /// Observe the first turn for this long; `None` returns once the vendor accepted it.
    pub wait: Option<Duration>,
    pub from: Caller,
    pub max_hops: u32,
    /// Keep the vendor record of the turn (`turn.raw`) in the output.
    pub raw: bool,
}

pub struct SendArgs {
    pub handle: String,
    pub text: String,
    pub mode: Mode,
    /// Steer only: the running turn the caller expects.
    pub expect_turn: Option<String>,
    pub reply_to: Option<String>,
    pub model: Option<String>,
    /// Claude only.
    pub max_turns: Option<u32>,
    pub approvals: Option<ApprovalPolicy>,
    /// Observe the turn for this long; `None` returns once the vendor accepted the message.
    pub wait: Option<Duration>,
    pub from: Caller,
    pub max_hops: u32,
    /// Keep the vendor record of the turn (`turn.raw`) in the output.
    pub raw: bool,
}

pub struct ReadArgs {
    pub handle: String,
    pub range: ReadRange,
    /// Vendor records instead of normalized messages.
    pub raw: bool,
}

pub struct WaitArgs {
    pub handle: String,
    pub target: WaitTarget,
    pub approvals: Option<ApprovalPolicy>,
    pub timeout: Duration,
    /// Keep the vendor record of the turn (`turn.raw`) in the output.
    pub raw: bool,
}

/// What a request produces; `--json` prints it as is.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Output {
    Caps {
        providers: Vec<Caps>,
        sender: Caller,
    },
    Sessions(Sessions),
    Read(Read),
    Outcome(Box<Outcome>),
}

/// Result of `ls`: one page per provider asked; an unavailable provider does not hide
/// the others.
#[derive(Serialize, JsonSchema)]
pub struct Sessions {
    pub sessions: Vec<Session>,
    /// Per provider, the cursor of its next page; null when there is none.
    pub next_cursors: BTreeMap<&'static str, Option<String>>,
    /// Providers that could not be listed.
    pub errors: Vec<ProviderError>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderError {
    pub provider: &'static str,
    pub error: Error,
}

/// Result of `read`: one page of messages, oldest first.
#[derive(Serialize, JsonSchema)]
pub struct Read {
    pub handle: String,
    /// Cursor of the next page when paging forward; absent for `--tail`.
    pub next_cursor: Option<String>,
    /// Normalized messages (absent with `--raw`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    /// The vendor records of the span (with `--raw`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Vec<Value>>,
}

pub async fn run(store: &Store, req: Request) -> Result<Output> {
    match req {
        Request::Caps { sender } => {
            let adapters = Adapter::all(store);
            let providers = join_all(adapters.iter().map(|p| p.caps())).await;
            Ok(Output::Caps { providers, sender })
        }
        Request::Ls(a) => ls(store, a).await,
        Request::New(a) => new(store, a).await,
        Request::Send(a) => send(store, a).await,
        Request::Read(a) => read(store, a).await,
        Request::Wait(a) => wait(store, a).await,
    }
}

async fn ls(store: &Store, a: LsArgs) -> Result<Output> {
    let cwd = a.cwd.as_deref().map(absolute).transpose()?;
    let filter = ListFilter {
        cwd: cwd.as_deref(),
        all: a.all,
    };
    if let Some(name) = &a.provider {
        let provider = Adapter::named(store, name)?;
        let page = provider.list(&filter, a.limit, a.cursor.as_deref()).await?;
        return Ok(Output::Sessions(Sessions {
            sessions: sessions_output(page.items, a.raw),
            next_cursors: BTreeMap::from([(provider.name(), page.next_cursor)]),
            errors: Vec::new(),
        }));
    }
    if a.cursor.is_some() {
        return Err(Error::new(
            ErrorCode::Precondition,
            "--cursor belongs to one provider; pass --provider with it",
        ));
    }
    let adapters = Adapter::all(store);
    let pages = join_all(adapters.iter().map(|p| p.list(&filter, a.limit, None))).await;
    let mut sessions = Vec::new();
    let mut next_cursors = BTreeMap::new();
    let mut errors = Vec::new();
    for (p, res) in adapters.iter().zip(pages) {
        match res {
            Ok(page) => {
                sessions.extend(page.items);
                next_cursors.insert(p.name(), page.next_cursor);
            }
            Err(error) => errors.push(ProviderError {
                provider: p.name(),
                error,
            }),
        }
    }
    Ok(Output::Sessions(Sessions {
        sessions: sessions_output(sessions, a.raw),
        next_cursors,
        errors,
    }))
}

/// Vendor records are large and rarely needed; they stay only on request.
fn sessions_output(mut sessions: Vec<Session>, raw: bool) -> Vec<Session> {
    if !raw {
        for s in &mut sessions {
            s.raw = Value::Null;
        }
    }
    sessions
}

fn outcome(mut o: Outcome, raw: bool) -> Output {
    if !raw && let Some(t) = &mut o.turn {
        t.raw = Value::Null;
    }
    Output::Outcome(Box::new(o))
}

async fn new(store: &Store, a: NewArgs) -> Result<Output> {
    let provider = Adapter::named(store, &a.provider)?;
    let cwd = absolute(&a.cwd)?;
    let delivered = delivered(&a.from, &a.prompt);
    let req = StartRequest {
        cwd: &cwd,
        prompt: &a.prompt,
        delivered: &delivered,
        model: a.model.as_deref(),
        name: a.name.as_deref(),
        effort: a.effort.as_deref(),
        approval_policy: a.approval_policy.as_deref(),
        sandbox: a.sandbox.as_deref(),
        depth: hop_depth(store, &a.from, None, a.max_hops)?,
        from: &a.from,
        max_turns: a.max_turns,
    };
    let mut o = provider
        .start(&req, a.approvals, a.wait.map(deadline))
        .await?;
    o.from = Some(a.from);
    Ok(outcome(o, a.raw))
}

async fn send(store: &Store, a: SendArgs) -> Result<Output> {
    let (name, id) = split_handle(&a.handle)?;
    let provider = Adapter::named(store, name)?;
    if a.expect_turn.is_some() && a.mode != Mode::Steer {
        return Err(Error::new(
            ErrorCode::Precondition,
            "--expect-turn applies to --mode steer only",
        ));
    }
    let delivered = delivered(&a.from, &a.text);
    let req = SendRequest {
        text: &a.text,
        delivered: &delivered,
        mode: a.mode,
        depth: hop_depth(store, &a.from, a.reply_to.as_deref(), a.max_hops)?,
        from: &a.from,
        reply_to: a.reply_to.as_deref(),
        expect_turn: a.expect_turn.as_deref(),
        model: a.model.as_deref(),
        max_turns: a.max_turns,
    };
    let mut o = provider
        .send(id, &req, a.approvals, a.wait.map(deadline))
        .await?;
    o.from = Some(a.from);
    Ok(outcome(o, a.raw))
}

async fn read(store: &Store, a: ReadArgs) -> Result<Output> {
    let (name, id) = split_handle(&a.handle)?;
    let page = Adapter::named(store, name)?.read(id, a.range).await?;
    Ok(Output::Read(Read {
        handle: a.handle,
        next_cursor: page.messages.next_cursor,
        messages: (!a.raw).then_some(page.messages.items),
        raw: a.raw.then_some(page.raw),
    }))
}

async fn wait(store: &Store, a: WaitArgs) -> Result<Output> {
    let (name, id) = split_handle(&a.handle)?;
    let o = Adapter::named(store, name)?
        .wait(id, &a.target, a.approvals, deadline(a.timeout))
        .await?;
    Ok(outcome(o, a.raw))
}

/// Split `<provider>:<id>`.
fn split_handle(handle: &str) -> Result<(&str, &str)> {
    match handle.split_once(':') {
        Some((p, id)) if !id.is_empty() => Ok((p, id)),
        _ => Err(Error::new(
            ErrorCode::Precondition,
            format!("invalid handle {handle}; expected <provider>:<id>"),
        )),
    }
}

/// A sender named by a handle (`--from`, `--caller`, AGENT_TALK_CALLER).
pub fn caller_from_handle(h: &str) -> Result<Caller> {
    split_handle(h)?;
    Ok(Caller::agent(h))
}

/// A non-empty environment variable.
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn absolute(p: &Path) -> Result<String> {
    std::fs::canonicalize(p)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| Error::new(ErrorCode::Precondition, format!("{}: {e}", p.display())))
}

fn deadline(after: Duration) -> Instant {
    Instant::now() + after
}

/// Hop depth of a new intent, refused above `max_hops`.
fn hop_depth(store: &Store, from: &Caller, reply_to: Option<&str>, max_hops: u32) -> Result<u32> {
    let (depth, basis) = store.hop_depth(from, reply_to)?;
    if depth > max_hops {
        return Err(Error::new(
            ErrorCode::MaxHops,
            format!(
                "refused: this message would be hop {depth} of an agent-to-agent chain and the limit is {max_hops} ({basis}). \
                 Hop depth is 1 + the depth of the message being answered: reply_to if given, else, for an agent sender, \
                 the message that started its current turn or the newest message delivered to its own session; \
                 an unknown sender without reply_to starts at 0. Do not forward again; answer whoever asked you instead."
            ),
        ));
    }
    Ok(depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_limit_is_inclusive() {
        let store = Store::memory();
        // An unknown sender starts at depth 0: allowed even with a limit of 0.
        assert_eq!(hop_depth(&store, &Caller::UNKNOWN, None, 0).unwrap(), 0);
        // An agent with nothing delivered to it is at depth 1: refused at 0, allowed at 1.
        let agent = Caller::agent("codex:t1");
        let e = hop_depth(&store, &agent, None, 0).unwrap_err();
        assert_eq!(e.code, ErrorCode::MaxHops);
        assert_eq!(hop_depth(&store, &agent, None, 1).unwrap(), 1);
    }
}
