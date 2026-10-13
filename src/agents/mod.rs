pub mod antigravity;
pub mod child;
pub mod claude;
pub mod codex;
pub mod decode;
pub mod grok;
pub mod history;
pub mod lifecycle;
pub mod opencode;
pub mod pi;

use self::history::{ReadPage, ReadQuery};
use crate::model::{Caller, CallerKind, Error, ErrorCode, Model, Outcome, Page, Result, Session};
use crate::store::Store;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// The text delivered for `text` from `from`: agent senders get a one-line provenance
/// header so the receiving model, and a person watching its session, sees who is
/// talking and that the answer belongs in the turn's final response (DESIGN.md §4). Not
/// doubled when the text already starts with one (`[from `).
pub fn delivered(from: &Caller, text: &str) -> String {
    match (&from.kind, &from.session) {
        (CallerKind::Agent, Some(h)) if !text.starts_with("[from ") => {
            format!("[from {h} via agent-talk; answer in your final response]\n\n{text}")
        }
        _ => text.to_string(),
    }
}

/// `text` without the provenance header `delivered` adds, for previews: the agent's
/// first-prompt preview would otherwise show only the header. Also strips the older
/// `[from <handle> via agent-talk]` form still in session histories.
pub fn strip_provenance(text: &str) -> &str {
    match text.split_once("\n\n") {
        Some((line, body))
            if line.starts_with("[from ")
                && line.ends_with(']')
                && line.contains(" via agent-talk") =>
        {
            body
        }
        _ => text,
    }
}

/// The adapters, dispatched by agent name.
pub enum Adapter<'a> {
    Codex(codex::Codex<'a>),
    Claude(claude::Claude<'a>),
    OpenCode(opencode::OpenCode<'a>),
    Grok(grok::Grok<'a>),
    Antigravity(antigravity::Antigravity<'a>),
    Pi(pi::Pi<'a>),
}

impl<'a> Adapter<'a> {
    pub fn all(store: &'a Store) -> [Adapter<'a>; 6] {
        [
            Adapter::Codex(codex::Codex::new(store)),
            Adapter::Claude(claude::Claude::new(store)),
            Adapter::OpenCode(opencode::OpenCode::new(store)),
            Adapter::Grok(grok::Grok::new(store)),
            Adapter::Antigravity(antigravity::Antigravity::new(store)),
            Adapter::Pi(pi::Pi::new(store)),
        ]
    }

    pub fn named(store: &'a Store, name: &str) -> Result<Adapter<'a>> {
        Adapter::all(store)
            .into_iter()
            .find(|a| a.name() == name)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Unsupported,
                    format!(
                        "agent {name} is not supported (codex, claude, opencode, grok, antigravity, pi)"
                    ),
                )
            })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Adapter::Codex(_) => "codex",
            Adapter::Claude(_) => "claude",
            Adapter::OpenCode(_) => "opencode",
            Adapter::Grok(_) => "grok",
            Adapter::Antigravity(_) => "antigravity",
            Adapter::Pi(_) => "pi",
        }
    }
}

impl Agent for Adapter<'_> {
    async fn status(&self) -> AgentStatus {
        match self {
            Adapter::Codex(p) => p.status().await,
            Adapter::Claude(p) => p.status().await,
            Adapter::OpenCode(p) => p.status().await,
            Adapter::Grok(p) => p.status().await,
            Adapter::Antigravity(p) => p.status().await,
            Adapter::Pi(p) => p.status().await,
        }
    }

    async fn models(&self) -> Result<Vec<Model>> {
        match self {
            Adapter::Codex(p) => p.models().await,
            Adapter::Claude(p) => p.models().await,
            Adapter::OpenCode(p) => p.models().await,
            Adapter::Grok(p) => p.models().await,
            Adapter::Antigravity(p) => p.models().await,
            Adapter::Pi(p) => p.models().await,
        }
    }

    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>> {
        match self {
            Adapter::Codex(p) => p.list(filter, limit, cursor).await,
            Adapter::Claude(p) => p.list(filter, limit, cursor).await,
            Adapter::OpenCode(p) => p.list(filter, limit, cursor).await,
            Adapter::Grok(p) => p.list(filter, limit, cursor).await,
            Adapter::Antigravity(p) => p.list(filter, limit, cursor).await,
            Adapter::Pi(p) => p.list(filter, limit, cursor).await,
        }
    }

    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome> {
        match self {
            Adapter::Codex(p) => p.start(req, deadline).await,
            Adapter::Claude(p) => p.start(req, deadline).await,
            Adapter::OpenCode(p) => p.start(req, deadline).await,
            Adapter::Grok(p) => p.start(req, deadline).await,
            Adapter::Antigravity(p) => p.start(req, deadline).await,
            Adapter::Pi(p) => p.start(req, deadline).await,
        }
    }

    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        match self {
            Adapter::Codex(p) => p.send(id, req, deadline).await,
            Adapter::Claude(p) => p.send(id, req, deadline).await,
            Adapter::OpenCode(p) => p.send(id, req, deadline).await,
            Adapter::Grok(p) => p.send(id, req, deadline).await,
            Adapter::Antigravity(p) => p.send(id, req, deadline).await,
            Adapter::Pi(p) => p.send(id, req, deadline).await,
        }
    }

    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage> {
        match self {
            Adapter::Codex(p) => p.read(id, q).await,
            Adapter::Claude(p) => p.read(id, q).await,
            Adapter::OpenCode(p) => p.read(id, q).await,
            Adapter::Grok(p) => p.read(id, q).await,
            Adapter::Antigravity(p) => p.read(id, q).await,
            Adapter::Pi(p) => p.read(id, q).await,
        }
    }

    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome> {
        match self {
            Adapter::Codex(p) => p.wait(id, target, deadline).await,
            Adapter::Claude(p) => p.wait(id, target, deadline).await,
            Adapter::OpenCode(p) => p.wait(id, target, deadline).await,
            Adapter::Grok(p) => p.wait(id, target, deadline).await,
            Adapter::Antigravity(p) => p.wait(id, target, deadline).await,
            Adapter::Pi(p) => p.wait(id, target, deadline).await,
        }
    }
}

pub enum WaitTarget {
    Turn(String),
    Receipt(String),
}

pub struct ListFilter<'a> {
    pub cwd: Option<&'a str>,
    /// Include sub-sessions too: Codex lists every source kind (sub-agents, unknown), not
    /// only interactive and exec; OpenCode drops its `parentID=null` filter.
    pub all: bool,
}

/// A session's `--model`, `--effort` and `--full-access`, passed through unvalidated in the
/// agent's syntax (DESIGN.md §4). `new` chooses them; agent-talk stores them for the sessions
/// it started, where a later `send` may change them. Each adapter passes again only what its
/// agent does not keep between turns (DESIGN.md §6). As a change, an absent field keeps the
/// current value; full access is only ever turned on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub full_access: bool,
}

impl Settings {
    pub fn is_empty(&self) -> bool {
        *self == Settings::default()
    }

    /// The change from these settings to `wanted`: its values that differ.
    pub fn change_to(&self, wanted: &Settings) -> Settings {
        Settings {
            model: wanted
                .model
                .clone()
                .filter(|m| self.model.as_ref() != Some(m)),
            effort: wanted
                .effort
                .clone()
                .filter(|e| self.effort.as_ref() != Some(e)),
            full_access: wanted.full_access && !self.full_access,
        }
    }

    /// These settings with `change` applied.
    pub fn with(&self, change: &Settings) -> Settings {
        Settings {
            model: change.model.clone().or_else(|| self.model.clone()),
            effort: change.effort.clone().or_else(|| self.effort.clone()),
            full_access: self.full_access || change.full_access,
        }
    }
}

/// A message on its way to a session: what the caller wrote and what reaches the agent.
pub struct Outgoing<'a> {
    /// The caller's text, as recorded in the intent.
    pub text: &'a str,
    /// What reaches the agent: `text`, with a provenance header for agent senders.
    pub delivered: &'a str,
    pub from: &'a Caller,
    /// Hop depth of this message (`Store::hop_depth`).
    pub depth: u32,
}

pub struct StartRequest<'a> {
    pub cwd: &'a str,
    /// The first prompt.
    pub message: Outgoing<'a>,
    /// Title the agent stores for the session (Codex `thread/name/set`, Claude `--name`,
    /// OpenCode `title`, Grok `_x.ai/session/rename`, pi `--name`; Antigravity has none and
    /// refuses it); `ls` shows it as `name`.
    pub name: Option<&'a str>,
    pub settings: &'a Settings,
}

pub struct SendRequest<'a> {
    pub message: Outgoing<'a>,
    /// Into the running turn (`send --steer`) instead of queued after it.
    pub steer: bool,
    /// What this send changes; empty unless the session is one agent-talk started.
    pub change: &'a Settings,
    /// The session's settings, `change` included; `None` for a session agent-talk did not
    /// start, which runs as the agent has it.
    pub settings: Option<&'a Settings>,
}

/// One agent's entry in `status`: the installed version, the state of the shared process
/// agent-talk joins, and which operations can run now (DESIGN.md §2).
#[derive(Serialize)]
pub struct AgentStatus {
    pub agent: &'static str,
    /// The agent CLI's `--version` output (OpenCode: the running service's version).
    pub version: Option<String>,
    /// The shared daemon, service or leader, for the agents that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
    pub operations: Vec<Operation>,
}

/// Whether one operation can run, and why not.
#[derive(Serialize)]
pub struct Operation {
    pub name: &'static str,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What an operation depends on: `Err` carries the reason it cannot run.
pub type Check = std::result::Result<(), String>;

impl Operation {
    pub fn new(name: &'static str, check: &Check) -> Self {
        Operation {
            name,
            available: check.is_ok(),
            reason: check.clone().err(),
        }
    }
}

/// One adapter per agent. Mutations persist their intent before submitting and
/// return a receipt; a `deadline` makes them observe the resulting turn on the
/// same connection.
pub trait Agent {
    async fn status(&self) -> AgentStatus;
    /// Every model `new --model` takes, in any order.
    async fn models(&self) -> Result<Vec<Model>>;
    async fn list(
        &self,
        filter: &ListFilter<'_>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<Page<Session>>;
    async fn start(&self, req: &StartRequest<'_>, deadline: Option<Instant>) -> Result<Outcome>;
    async fn send(
        &self,
        id: &str,
        req: &SendRequest<'_>,
        deadline: Option<Instant>,
    ) -> Result<Outcome>;
    async fn read(&self, id: &str, q: &ReadQuery) -> Result<ReadPage>;
    async fn wait(&self, id: &str, target: &WaitTarget, deadline: Instant) -> Result<Outcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_header_only_for_agents() {
        let agent = Caller::agent("codex:01a1029c-17b8-75c1-9e19-0fc6d58b0512");
        let text = delivered(&agent, "what is the word?");
        assert_eq!(
            text,
            "[from codex:01a1029c-17b8-75c1-9e19-0fc6d58b0512 via agent-talk; answer in your final response]\n\nwhat is the word?"
        );
        assert_eq!(strip_provenance(&text), "what is the word?");
        // A header the model wrote itself is not doubled.
        let own = "[from codex:x via agent-talk]\n\nhi";
        assert_eq!(delivered(&agent, own), own);
        assert_eq!(strip_provenance(own), "hi");
        // People and unknown senders: unchanged, and a text that merely starts with
        // "[from " is not a header.
        assert_eq!(delivered(&Caller::UNKNOWN, "hi"), "hi");
        assert_eq!(
            strip_provenance("[from the top] again"),
            "[from the top] again"
        );
    }
}
