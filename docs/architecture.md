# Architecture

agent-talk lets a person or an agent send a message to another agent's session and read the
reply, across agents, through each agent's official non-interactive interface. This document
covers how it is built and why. What a caller can rely on is in [behaviour.md](behaviour.md);
how each agent is driven is in [agents/](agents/).

## Scope

In: Codex, Claude Code, OpenCode 2.x, Grok CLI, Antigravity CLI, pi and Devin CLI (local
sessions); a CLI and a stdio MCP server exposing the same operations; sender attribution and a
hop limit for agent-to-agent traffic.

Out: a daemon of agent-talk's own, a GUI, PTY or terminal scraping, remote hosts, worktree
management, approving tool calls on anyone's behalf, and anything undocumented or that
impersonates an interactive user. agent-talk deletes no session, conversation or agent file, on
any agent.

## Principles

1. **Native only.** Agent protocols and CLI flags as the agent offers them. No terminal
   injection, no writing to another process's stdio, no binary patching, no edits to agent
   configuration.
2. **Observations gate operations, rechecked when the operation runs.** Each session carries
   observations ([behaviour.md](behaviour.md#sessions)); a refusal names the observation that
   blocks it.
3. **Declare the limits.** `agent-talk status` reports what the installed CLI and the running
   daemon or service can do. An unsupported case fails with a stable error code, never a silent
   downgrade.
4. **The user's install and login.** Same binaries, accounts and permission defaults, unless the
   caller asks for full access.
5. **Thin.** Normalize only conversation-level events: text, role, phase, turn boundaries,
   approval requests. Everything else passes through as the agent's raw JSON (`read --raw`).
6. **Receipts, not assumptions.** Every mutation returns a receipt stating what was accepted. A
   timeout is an unknown outcome, not a rejection; mutations are never retried automatically.

## Processes and connections

agent-talk runs no daemon. State lives where the agent already keeps it, and each command
connects, acts, observes and exits:

| agent                                                       | who holds the live session           | agent-talk's connection                                   |
| ----------------------------------------------------------- | ------------------------------------ | --------------------------------------------------------- |
| Codex                                                       | shared app-server daemon             | per command: connect, subscribe, act, observe, disconnect |
| OpenCode                                                    | `opencode serve --service`           | per command: HTTP, plus SSE while observing               |
| Grok, leader live                                           | shared leader process                | per command: a `grok agent --leader stdio` child (ACP)    |
| Claude, Grok without leader, Antigravity, pi, Devin (owned) | nobody between turns; files on disk  | one agent child per mutation                              |
| Claude, Grok, Antigravity TUI (foreign)                     | the user's terminal process          | read-only                                                 |
| Devin TUI, `-p` or ACP client (foreign)                     | that process, under Devin's own lock | read-only while it runs                                   |
| pi TUI (foreign)                                            | the user's terminal process          | undetectable; a send branches the session                 |

"Owned" means agent-talk started the session. It records creation, not exclusive access: the
user can open an owned session in their own client.

**No daemon of agent-talk's own.** The agents already run the shared processes that hold live
sessions. A second daemon would add installation, pid and version-skew problems for state that
fits in SQLite. The one long-lived process is `agent-talk mcp`, which the calling agent starts
and stops like any MCP server; the Grok and Devin turns it keeps after their call returned live
there ([behaviour.md](behaviour.md#mcp-server)).

**Agent multi-client processes are joined, never started.** The Codex daemon, the OpenCode
service and the Grok leader are used when they are live; otherwise agent-talk takes the agent's
single-process path or refuses. Starting and stopping them is the user's decision, since they
outlive the command and carry the user's configuration.

**One child per mutation where the agent has no shared process.** Claude, Antigravity and pi run
one agent CLI process per `new` or `send`, which runs the turn and exits; its stdout goes to a
run log that the command tails and later commands read. Grok in direct mode and Devin run an ACP
child that holds the session only while it lives, so their turns need agent-talk's connection
until they end.

## Local state

`~/.agent-talk/store.db` (SQLite) holds what agent-talk itself knows, and no connection state:

- the intent of every message, written before anything is sent, with its sender and hop depth;
- its receipt;
- the sessions agent-talk started and their settings;
- the agent children it spawned, by pid, which is how a later command tells an agent-talk run
  from a foreign writer;
- every approval request seen, and what happened to it.

**Session locks.** Claude, Grok in direct mode, Antigravity and pi have no lock between
processes: a second writer forks or overwrites the conversation. agent-talk therefore takes an OS
file lock in `~/.agent-talk/locks/` for the length of a command, and refuses to write while the
agent's own liveness marker shows another process holding the session. The lock is released at
process exit and never inherited by an agent child; a child that outlives its command is tracked
by pid instead. pi has no liveness marker, so only agent-talk's own children are refused there.
Devin enforces a lock of its own, so agent-talk takes none and maps Devin's refusal to its error
codes.

**Run logs.** The children of Claude, Antigravity and pi write their stdout to a run log per
receipt in `~/.agent-talk/<agent>-runs/`, which is how `wait` finds the outcome of a turn after
the sending command exited.

**Child environment.** Agent children get the user's environment minus `AGENT_TALK_CALLER` and
the agents' session variables. Otherwise the agent would pass them on to its own shells and MCP
servers, and messages from the child session would be attributed to agent-talk's caller.

## Code structure

agent-talk is a single Rust binary: agents call it many times per session, so startup time
matters, and one artifact per platform keeps installation simple.

| module                | responsibility                                                                          |
| --------------------- | --------------------------------------------------------------------------------------- |
| `main.rs`             | CLI front-end: arguments, sender from the shell environment, human output, exit codes   |
| `mcp.rs`              | MCP front-end over stdio: tools, sender from `_meta` or the environment, kept turns     |
| `ops.rs`              | the operations, independent of front-end: handles, hop limit, settings, dispatch        |
| `model.rs`            | agent-neutral types and error codes                                                     |
| `store.rs`            | `store.db`                                                                              |
| `agents/mod.rs`       | the adapter interface, request types, provenance header                                 |
| `agents/lifecycle.rs` | receipt and approval lifecycle, observation bounds (deadline, signals)                  |
| `agents/history.rs`   | read paging; remote history shared by Codex and OpenCode                                |
| `agents/acp.rs`       | an ACP agent child (JSON-RPC over its stdio), shared by Grok and Devin                  |
| `agents/child.rs`     | agent CLI children: environment, version, session lock, run logs                        |
| `agents/decode.rs`    | tolerant field decoders                                                                 |
| `agents/<agent>/`     | one adapter per agent                                                                   |

`tests/live.py` is the regression harness against the real agents
([tests/README.md](../tests/README.md)); `skills/agent-talk/` is the skill that teaches agents
the CLI.

## Adapters

**Shared lifecycle, separate observe loops.** The receipt and approval lifecycle is
agent-talk's semantics, not an agent's, so it is implemented once, in `agents/lifecycle.rs`. That
includes an agent child that failed to spawn or exited before taking the message: its receipt is
`rejected` whatever the error, since nothing ran. An adapter supplies discovery, submission, the
event source, history lookup and message normalization, and keeps its own observe loop, because
what proves a turn ended differs between agents: the `turn/completed` event on Codex, a re-read of
history on OpenCode, the child and its run log on Claude, Antigravity and pi, the
`session/prompt` response on Grok and Devin. A generic event-loop driver would hide those
differences behind mode flags.

**Turn ids come from the agent or from agent-talk's own client id**, never made up from text, so
a turn found later is the turn that was sent. Where the agent stores a client-chosen id with the
message, agent-talk sets one; otherwise it takes the id the agent reports at acceptance.

**Subscribe before submit.** On agents with an event stream (Codex, OpenCode, Grok, Devin), a
command that observes the turn subscribes before it submits, so no event of the turn is missed.

**No tight polling.** Observation uses the agents' event streams. agent-talk polls only where no
event source exists: session files of turns it is not running, its own run logs, and the few
agent states that have no event (each adapter's doc names them).

**History is paged to the answer.** `read` and turn lookup follow the agent's cursor, newest
first, until the page is full, the turn is found or history ends, because the wanted turn is
usually recent.

## Decoding agent records

Agents change their record formats between versions, and a field written differently must not
cost a record, a listing or the end of a turn. Records and events are decoded into small structs
where their shape is stable or a decoding failure must be an error, such as the ids that
correlate a response or a record with agent-talk's own request. A record read for a few fields
whose shape must be tolerated stays `serde_json::Value` with accessors, as do payloads agent-talk
only passes through.

The tolerance rules apply either way:

- a field that does not fit reads as absent or as its default and never drops its record (in a
  struct, through `agents/decode.rs`);
- an odd list entry is skipped on its own;
- a listed session, model or request without the id that addresses it is skipped;
- the raw record is kept, for `read --raw` and error data;
- content that is not recognized never counts as the end of a turn.
