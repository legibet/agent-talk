//! The operations behind both front-ends (CLI and MCP): handle parsing, sender
//! provenance header, hop limit, agent dispatch, typed output.

use crate::agents::{
    Adapter, Agent, AgentStatus, ListFilter, ReadQuery, SendRequest, StartRequest, WaitTarget,
    delivered,
};
use crate::model::{Caller, Error, ErrorCode, Message, Model, Outcome, Result, Session};
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
/// Agent-to-agent hops allowed before a message is refused (DESIGN.md §4).
pub const MAX_HOPS: u32 = 3;
pub const LS_LIMIT: u32 = 25;
pub const READ_LIMIT: usize = 20;
pub const MODELS_LIMIT: usize = 50;

pub enum Request {
    /// `sender` is who this shell's `new` and `send` would be attributed to.
    Status {
        sender: Caller,
    },
    Models(ModelsArgs),
    Ls(LsArgs),
    New(NewArgs),
    Send(SendArgs),
    Read(ReadArgs),
    Wait(WaitArgs),
}

pub struct ModelsArgs {
    pub agent: String,
    /// Only models whose id contains this, case-insensitively.
    pub query: Option<String>,
    pub limit: usize,
    /// The last id of the previous page.
    pub cursor: Option<String>,
}

pub struct LsArgs {
    /// One agent, or every agent's first page.
    pub agent: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Include sub-sessions too (`ListFilter::all`).
    pub all: bool,
    pub limit: u32,
    /// Only with `agent`: cursors belong to one agent.
    pub cursor: Option<String>,
}

pub struct NewArgs {
    pub agent: String,
    pub cwd: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    /// Title the agent stores for the new session.
    pub name: Option<String>,
    pub effort: Option<String>,
    pub full_access: bool,
    /// Observe the first turn for this long; `None` returns once the agent accepted it.
    pub wait: Option<Duration>,
    pub from: Caller,
}

pub struct SendArgs {
    pub handle: String,
    pub text: String,
    /// Into the running turn instead of queued after it.
    pub steer: bool,
    /// Observe the turn for this long; `None` returns once the agent accepted the message.
    pub wait: Option<Duration>,
    pub from: Caller,
}

pub struct ReadArgs {
    pub handle: String,
    pub query: ReadQuery,
    /// The agent's raw records instead of normalized messages.
    pub raw: bool,
}

pub struct WaitArgs {
    pub handle: String,
    pub target: WaitTarget,
    pub timeout: Duration,
}

/// What a request produces; `--json` prints it as is.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Output {
    Status {
        agents: Vec<AgentStatus>,
        sender: Caller,
    },
    Models(Models),
    Sessions(Sessions),
    Read(Read),
    Outcome(Box<Outcome>),
}

/// Result of `models`: one page of an agent's models, sorted by id.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Models {
    pub models: Vec<Model>,
    /// Cursor of the next page; absent on the last.
    pub next_cursor: Option<String>,
}

/// Result of `ls`: one page per agent asked; an unavailable agent does not hide
/// the others.
#[derive(Serialize, JsonSchema)]
pub struct Sessions {
    pub sessions: Vec<Session>,
    /// Per agent, the cursor of its next page; null when there is none.
    pub next_cursors: BTreeMap<&'static str, Option<String>>,
    /// Agents that could not be listed.
    pub errors: Vec<ListError>,
}

#[derive(Serialize, JsonSchema)]
pub struct ListError {
    pub agent: &'static str,
    pub error: Error,
}

/// Result of `read`: one page of messages, oldest first.
#[derive(Serialize, JsonSchema)]
pub struct Read {
    pub handle: String,
    /// Cursor of the page of older messages; absent at the start of history.
    pub next_cursor: Option<String>,
    /// Normalized messages (absent with `--raw`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    /// The agent's raw records of the span (with `--raw`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Vec<Value>>,
}

pub async fn run(store: &Store, req: Request) -> Result<Output> {
    match req {
        Request::Status { sender } => {
            let adapters = Adapter::all(store);
            let agents = join_all(adapters.iter().map(|p| p.status())).await;
            Ok(Output::Status { agents, sender })
        }
        Request::Models(a) => {
            let all = Adapter::named(store, &a.agent)?.models().await?;
            Ok(Output::Models(page_models(all, &a)?))
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
    if let Some(name) = &a.agent {
        let agent = Adapter::named(store, name)?;
        let page = agent.list(&filter, a.limit, a.cursor.as_deref()).await?;
        return Ok(Output::Sessions(Sessions {
            sessions: page.items,
            next_cursors: BTreeMap::from([(agent.name(), page.next_cursor)]),
            errors: Vec::new(),
        }));
    }
    if a.cursor.is_some() {
        return Err(Error::new(
            ErrorCode::Precondition,
            "--cursor belongs to one agent; pass --agent with it",
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
            Err(error) => errors.push(ListError {
                agent: p.name(),
                error,
            }),
        }
    }
    Ok(Output::Sessions(Sessions {
        sessions,
        next_cursors,
        errors,
    }))
}

async fn new(store: &Store, a: NewArgs) -> Result<Output> {
    let agent = Adapter::named(store, &a.agent)?;
    let cwd = absolute(&a.cwd)?;
    let delivered = delivered(&a.from, &a.prompt);
    let req = StartRequest {
        cwd: &cwd,
        prompt: &a.prompt,
        delivered: &delivered,
        model: a.model.as_deref(),
        name: a.name.as_deref(),
        effort: a.effort.as_deref(),
        full_access: a.full_access,
        depth: hop_depth(store, &a.from)?,
        from: &a.from,
    };
    let mut o = agent.start(&req, a.wait.map(deadline)).await?;
    o.from = Some(a.from);
    Ok(Output::Outcome(Box::new(o)))
}

async fn send(store: &Store, a: SendArgs) -> Result<Output> {
    let (name, id) = split_handle(&a.handle)?;
    let agent = Adapter::named(store, name)?;
    let delivered = delivered(&a.from, &a.text);
    let req = SendRequest {
        text: &a.text,
        delivered: &delivered,
        steer: a.steer,
        depth: hop_depth(store, &a.from)?,
        from: &a.from,
    };
    let mut o = agent.send(id, &req, a.wait.map(deadline)).await?;
    o.from = Some(a.from);
    Ok(Output::Outcome(Box::new(o)))
}

async fn read(store: &Store, a: ReadArgs) -> Result<Output> {
    let (name, id) = split_handle(&a.handle)?;
    let page = Adapter::named(store, name)?.read(id, &a.query).await?;
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
        .wait(id, &a.target, deadline(a.timeout))
        .await?;
    Ok(Output::Outcome(Box::new(o)))
}

/// The page of `all` after the cursor, among the models matching the query, sorted by id.
fn page_models(mut all: Vec<Model>, a: &ModelsArgs) -> Result<Models> {
    all.sort_by(|x, y| x.id.cmp(&y.id));
    let query = a.query.as_deref().map(str::to_lowercase);
    let mut rest = all.into_iter().filter(|m| {
        query
            .as_ref()
            .is_none_or(|q| m.id.to_lowercase().contains(q))
    });
    if let Some(c) = &a.cursor
        && !rest.by_ref().any(|m| m.id == *c)
    {
        return Err(Error::new(
            ErrorCode::Precondition,
            format!("unknown cursor {c}"),
        ));
    }
    let models: Vec<Model> = rest.by_ref().take(a.limit).collect();
    let next_cursor = rest.next().and(models.last()).map(|m| m.id.clone());
    Ok(Models {
        models,
        next_cursor,
    })
}

/// Split `<agent>:<id>`.
fn split_handle(handle: &str) -> Result<(&str, &str)> {
    match handle.split_once(':') {
        Some((p, id)) if !id.is_empty() => Ok((p, id)),
        _ => Err(Error::new(
            ErrorCode::Precondition,
            format!("invalid handle {handle}; expected <agent>:<id>"),
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

/// Hop depth of a new intent, refused above `MAX_HOPS`.
fn hop_depth(store: &Store, from: &Caller) -> Result<u32> {
    let (depth, basis) = store.hop_depth(from)?;
    if depth > MAX_HOPS {
        return Err(Error::new(
            ErrorCode::MaxHops,
            format!(
                "refused: this message would be hop {depth} of an agent-to-agent chain and the limit is {MAX_HOPS} ({basis}). \
                 Do not forward again; answer whoever asked you in your final response."
            ),
        ));
    }
    Ok(depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewIntent;

    #[test]
    fn models_page_by_id_after_the_cursor_within_the_query() {
        let all: Vec<Model> = ["b/m2", "a/m1", "b/M1", "c/x"]
            .map(|id| Model {
                id: id.into(),
                efforts: Vec::new(),
            })
            .into();
        let page = |query: Option<&str>, limit, cursor: Option<&str>| {
            page_models(
                all.clone(),
                &ModelsArgs {
                    agent: "codex".into(),
                    query: query.map(String::from),
                    limit,
                    cursor: cursor.map(String::from),
                },
            )
        };
        fn ids(p: &Models) -> Vec<&str> {
            p.models.iter().map(|m| m.id.as_str()).collect()
        }

        let p = page(None, 3, None).unwrap();
        assert_eq!(ids(&p), ["a/m1", "b/M1", "b/m2"]);
        assert_eq!(p.next_cursor.as_deref(), Some("b/m2"));
        let p = page(None, 3, Some("b/m2")).unwrap();
        assert_eq!(ids(&p), ["c/x"]);
        assert_eq!(p.next_cursor, None);

        let p = page(Some("M1"), 1, None).unwrap();
        assert_eq!(ids(&p), ["a/m1"]);
        let p = page(Some("M1"), 1, Some("a/m1")).unwrap();
        assert_eq!((ids(&p), p.next_cursor.is_none()), (vec!["b/M1"], true));

        assert_eq!(
            page(Some("M1"), 1, Some("c/x")).unwrap_err().code,
            ErrorCode::Precondition
        );
    }

    #[test]
    fn hop_limit_is_inclusive() {
        let store = Store::memory();
        let agent = Caller::agent("codex:t1");
        let deliver = |id: &str, depth| {
            store
                .insert_intent(&NewIntent {
                    receipt_id: id,
                    handle: "codex:t1",
                    client_msg_id: id,
                    text: "x",
                    from: &Caller::UNKNOWN,
                    depth,
                    delivered_text: None,
                })
                .unwrap()
        };
        // A person's message is not a hop.
        assert_eq!(hop_depth(&store, &Caller::UNKNOWN).unwrap(), 0);
        // An agent acting on a message one hop below the limit may still send.
        deliver("r1", MAX_HOPS - 1);
        assert_eq!(hop_depth(&store, &agent).unwrap(), MAX_HOPS);
        deliver("r2", MAX_HOPS);
        let e = hop_depth(&store, &agent).unwrap_err();
        assert_eq!(e.code, ErrorCode::MaxHops);
    }
}
