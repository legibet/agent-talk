# agent-talk design

`agent-talk` lets a person or an agent send a message to another agent's session and read the
reply, across agents, using only each agent's official non-interactive interfaces. This is the
design as built. Agent behaviour in §6 was observed on macOS with the version named in each
heading; facts only read in agent source or documentation are marked as such.

## 1. Scope

In: Codex (shared app-server daemon), Claude Code (`claude -p`), OpenCode 2.x (shared background
service; 1.x is not supported), Grok CLI (ACP to `grok agent stdio`, through the shared leader
when one runs), Antigravity CLI (`agy -p`); a CLI and an MCP server exposing the same five operations; sender
attribution and a provenance header for agent-to-agent traffic; a hop limit.

Out: an agent-talk daemon, GUI, PTY or terminal scraping, remote hosts, worktree management,
anything undocumented or that impersonates an interactive user, approving tool calls on anyone's
behalf, deleting sessions.

## 2. Principles

1. **Native only.** Agent protocols and CLI flags as documented. No terminal injection, no
   touching another process's stdio, no binary patching, no edits to agent configuration.
2. **Observations gate operations, rechecked at execution time.** Each session carries
   observations (§4); a refusal names the observation that blocks it.
3. **Declare the limits.** `agent-talk status` prints what the installed CLI and the running daemon
   or service can do. Unsupported cases fail with a stable error code, never a silent downgrade.
4. **The user's install and login.** Same binaries, same accounts, same permission defaults,
   unless `new --full-access` asks for more.
5. **Thin.** Normalize only conversation-level events (text, role, phase, turn boundaries,
   approval requests). Everything else passes through as raw agent JSON behind `read --raw`.
6. **Receipts, not assumptions.** Every mutation returns a receipt stating what was accepted. A
   timeout is an unknown outcome, not a rejection; mutations are never retried automatically.

## 3. Architecture

### No agent-talk daemon

State lives where the agent already keeps it:

| agent                                            | who holds the live session          | agent-talk's connection                                   |
| ------------------------------------------------ | ----------------------------------- | --------------------------------------------------------- |
| Codex                                            | shared app-server daemon            | per command: connect, subscribe, act, observe, disconnect |
| OpenCode                                         | `opencode serve --service`          | per command: HTTP, plus SSE while observing               |
| Grok, leader live                                | shared leader process               | per command: a `grok agent --leader stdio` child (ACP)    |
| Claude, Grok without leader, Antigravity (owned) | nobody between turns; files on disk | one agent child per mutation                              |
| Claude, Grok, Antigravity TUI (foreign)          | the user's terminal process         | read-only                                                 |

"Owned" means agent-talk started the session; the `owned` table records creation, not current
exclusivity. The CLI process is short-lived, but within one command it owns a live connection
holding the subscription, pending requests and correlation state. SQLite stores intents and
receipts, not connection state. `agent-talk mcp` is the one long-lived process, one per agent
session, started and stopped by the agent like any MCP server; state that must outlive a command
belongs there, not in a system daemon (§7).

### Local state (`~/.agent-talk/`)

`store.db` (SQLite) has five tables:

- `intents`, written **before** any send: receipt id, handle, client message id, text, delivered
  text when it differs (provenance header), sender, hop depth.
- `receipts`: `pending | accepted | unknown | rejected`, queue id, turn id, item id, agent error.
  Intent and receipt are written in one transaction.
- `owned`: sessions agent-talk started (handle, cwd, start arguments).
- `processes`: agent children spawned for an intent (receipt, handle, pid): `claude -p`, a
  direct-mode `grok agent`, `agy -p`. This is how later commands tell an agent-talk run from a
  foreign writer.
- `approvals`: every approval request seen and what happened to it.

Claude, Grok (direct mode) and Antigravity sessions are also locked with OS file locks,
`locks/<agent>-<id>`, opened close-on-exec so an agent child does not inherit them and released
by process exit, never with SQLite rows. A child that outlives its command is tracked by pid
instead. Claude and Antigravity children write stdout to a run log per receipt
(`claude-runs/`, `antigravity-runs/`), which the command tails and later commands read to recover
a receipt.

Agent children get the user's environment minus `AGENT_TALK_CALLER` and the agents' session
variables of §4; otherwise the agent would pass them to its own shells and MCP servers and the
child session's messages would be attributed to agent-talk's caller.

### Layout

```
src/
  main.rs              clap definitions, sender from the shell environment, human output, exit codes
  mcp.rs               the five operations as MCP tools (rmcp over stdio); sender from _meta / env
  ops.rs               the operations: handles, provenance header, hop limit, dispatch, typed output
  model.rs             agent-neutral types and error codes
  store.rs             SQLite (rusqlite, bundled)
  agents/mod.rs        agent interface, adapter dispatch, shared receipt/approval lifecycle
  agents/codex/        adapter, protocol (narrow response types), transport (WebSocket over AF_UNIX)
  agents/claude/       adapter, transcript rules, stream-json events, process spawning
  agents/opencode/     adapter, transport (HTTP + SSE)
  agents/grok/         adapter, ACP client of a grok agent child, updates.jsonl rules
  agents/antigravity/  adapter (summaries db, presence lock), transcript, stream, process
tests/live.py          regression harness against the real agents; see tests/README.md
```

The receipt and approval lifecycle (default policy, reject on agent refusal, record approvals,
mark answered requests resolved, validate the receipt a `wait` names, settle the outcome) is
agent-talk's semantics and is implemented once in `agents/mod.rs`. One exception: a process
adapter whose child failed to spawn or exited before taking the message settles the receipt
`rejected` itself, since nothing ran. Adapters supply discovery, submission, event source,
history lookup and message normalization. Observe loops stay per adapter because the authority
for "the turn ended" differs: an agent event in Codex, a re-read of history in OpenCode, the `-p`
process in Claude and Antigravity, the `session/prompt` response in Grok (§7).

Agent responses are decoded into narrow types only where a decode failure must be an error
(correlation ids, turn status, pages of correlated objects); everything displayed or passed
through stays untyped JSON.

## 4. Model

### Observations

| observation | values                                                                                                                                   | learned from                                                                                                                                                                             |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `history`   | `visible` / `none` / `unknown`                                                                                                           | the agent's history listing or files                                                                                                                                                     |
| `loaded`    | `yes` / `no` / `unknown`                                                                                                                 | Codex `thread/loaded/list`; Claude `claude agents` (pid present); OpenCode `yes` while the service answers; Grok own child, live TUI row or leader `resident`; Antigravity presence lock |
| `origin`    | agent label (`codex-tui`, `codex_exec`, `claude-interactive`, `opencode agent build`, `headless`, ...), `agent-talk` for owned sessions  | agent metadata; the `owned` table                                                                                                                                                        |

Declared blind spot: for Codex, "not loaded" cannot distinguish a stopped thread from a live
standalone `codex exec` or `--no-daemon` run. agent-talk proceeds through the daemon.

### Types

```
session  { handle, agent, id, cwd?, name?, preview?, observations, state: idle|running|waiting|unknown, owned }
turn     { handle, turn_id, status: completed|failed|interrupted|unknown, final_text?, error?, duration_ms?, basis? }
message  { turn_id, item_id, role: user|assistant, phase: final|commentary|other, text, from?, timestamp? }
approval { handle, turn_id?, item_id?, request_id, kind, summary, outcome: pending|declined|resolved|denied, raw }
receipt  { receipt_id, handle, client_msg_id, state, queue_id?, turn_id?, item_id?, agent_error?, delivered_text? }
caller   { kind: agent|unknown|runtime, session?, turn? }
outcome  { handle, receipt?, turn?, approvals[], from? }            result of new / send / wait
sessions { sessions[], next_cursors{agent: cursor?}, errors[] }     result of ls
read     { handle, next_cursor?, messages[] | raw[] }              result of read
```

Handles are `<agent>:<id>`. `turn.basis` says how the end of a turn was established when it was
not an agent turn-end event on agent-talk's own connection. `caller.kind: runtime` marks input
the agent runtime injected (task notifications, system messages).

### Receipts

`new` and `send` write the intent, submit, and mark the receipt `accepted` with the agent's ids.
An agent refusal marks it `rejected` (also later, when a run log proves the message was never
taken). When observation ends without a result (deadline, Ctrl-C, transport loss, HTTP 5xx) a
`pending` receipt becomes `unknown`; the error carries the receipt, the approvals seen so far, and
a `state`: `pending` (queued, not running), `running`, `waiting` (an approval is unanswered) or
`unknown`. `rejected` and `unknown` are reachable only from `pending`; any later proof of delivery
(history, queue, inbox, run log) moves a receipt to `accepted` and fills in its ids. `wait
--receipt R` or `wait --turn T` resolves it later; both attach the receipt when one exists for
that turn.

### Approvals

Agents fan approval requests out to every subscribed client, and any one answer resolves them
for all. agent-talk never accepts.

- Sessions agent-talk started have nobody else to answer, so they must not wait on approvals.
  Where the agent allows it they never ask (Codex `approvalPolicy: never`, Claude
  `--permission-prompts none`, OpenCode session rules that deny what the user's rules would ask;
  headless Claude and Antigravity deny on their own), and what still reaches agent-talk is
  declined (Codex `decline`, OpenCode `reject` with a message, Grok `reject_once`).
- Sessions agent-talk did not start belong to the user: agent-talk never answers there and never
  changes their settings. It records pending requests and lets the deadline pass with
  `state: waiting`.

Requests answered by someone else become `resolved`; denials made by the agent CLI itself are
recorded as `denied`.

### Full access

`new --full-access` (MCP `full_access`) gives a new session every permission and no approval
prompts: Codex `sandbox: danger-full-access`, Claude `--permission-mode bypassPermissions`,
OpenCode a session rule allowing every action, Grok `_meta.yoloMode: true`, Antigravity
`--dangerously-skip-permissions`. Without it the session runs under the agent's own
configuration. The choice is stored with the owned session and applied again on every send,
because Claude and Antigravity take it per process, a resumed Codex thread falls back to the
daemon's sandbox once it has unloaded, and a direct-mode Grok load starts a new process.

### Provenance and loops (best-effort)

A message from another agent is answered in the receiving turn's final response, which the
sender gets through `send --wait`, `wait` or `read`; the provenance header says so. An agent
that answers with `send` instead, or forwards the task again, adds a hop.

Each intent records `from` and `depth`. Depth, computed before any agent call:

- agent sender: 1 + depth of the intent that started the caller's current turn when known, else
  of the newest non-rejected intent delivered to the caller's session (none: 0).
- unknown sender (a person at the CLI): 0.

A depth above 3 is refused with `E_MAX_HOPS`, a fixed backstop against agents messaging each
other without end; the message says not to forward again.

Sender identity, in priority order:

1. Explicit pin: `--from` on the CLI, `--caller` on `agent-talk mcp`, or `AGENT_TALK_CALLER`.
2. `agent-talk mcp`, per call, from `tools/call` `params._meta`: Codex `threadId` (with the turn
   id), OpenCode `ai.opencode/sessionID`, Antigravity `antigravity.google/conversation_id`. Then
   the server's environment: `GROK_SESSION_ID`, then `CLAUDE_CODE_SESSION_ID`, which Grok and
   Claude Code set for each MCP server they spawn (Claude's `claudecode/toolUseId` is kept as a
   turn hint). Metadata goes before the environment, and Grok before Claude, because environments
   are inherited: a Codex daemon started from a Claude shell carries an unrelated
   `CLAUDE_CODE_SESSION_ID`.
3. CLI from an agent's shell: `CODEX_THREAD_ID`, `OPENCODE_SESSION_ID`, `GROK_SESSION_ID`,
   `ANTIGRAVITY_CONVERSATION_ID`, then `CLAUDE_CODE_SESSION_ID`. The first four are set by their
   agent for that one session's shell; Claude's is inherited, so it goes last. A daemon or
   service started from another agent's shell still carries that agent's variable, which the
   order cannot detect.
4. `unknown`.

An agent sender's message is delivered with a first line
`[from <handle> via agent-talk; answer in your final response]` and a blank line (not doubled
when the text already starts with `[from`). The intent keeps the original text and records the
delivered one. All of this is attribution, not authenticated identity.

### MCP server

`agent-talk mcp [--caller H]` is an ordinary stdio MCP server that the user
configures once, globally; agent-talk never writes agent configuration or injects itself into
sessions. It serves `ls / new / send / read / wait` with the JSON the CLI prints under `--json`;
refusals are tool errors (`isError`), not protocol errors. `new` takes `full_access` as on the
CLI, and requires `cwd`, because the server's working directory is not the caller's.

Code-mode clients (Codex for its gpt-6 models, OpenCode, pi) call tools from a script and pass
results on without the model reading them, so every tool declares an `outputSchema` generated
from the CLI's types, returns the JSON as `structuredContent` and as text, and carries a `title`
and behaviour annotations (`ls`, `read`, `wait` read-only and closed-world; `new`, `send`
additive, not idempotent). Through rmcp 3.5 the server speaks protocol 2026-07-28 (stateless,
`server/discover`, used by Claude Code and Antigravity) and the `initialize` handshake of earlier
versions (Codex sends 2025-06-18, Grok 2025-11-25). Logging is stderr only.

## 5. CLI

```
agent-talk status
agent-talk ls [--agent A] [--cwd DIR] [--all] [--limit N] [--cursor C]
agent-talk new A "prompt" [--cwd DIR] [--name N] [--model M] [--effort E] [--full-access]
    [--wait] [--timeout S] [--from H]
agent-talk send H "text" [--steer] [--wait] [--timeout S] [--from H]
agent-talk read H [--tail N | --since CURSOR [--limit N]] [--raw]
agent-talk wait H (--turn ID | --receipt R) [--timeout S]
agent-talk mcp [--caller H]
```

- `--json` on every command; default output is for humans. Exit codes: 0 ok, 2 refused with a
  stable error code, 3 unknown outcome (timeout or Ctrl-C after an accepted intent), 4 transport
  failure.
- `send` without `--wait` promises only that the agent accepted the message. Claude, Grok and
  Antigravity sends still run the turn to its end, because the turn lives in the child.
- `send --wait` subscribes, persists the intent, submits, correlates and observes on one
  connection; notifications arriving before the RPC response are buffered.
- `wait` checks history first (the turn may be complete), then observes. It matches session and
  turn id and returns the terminal status as reported. "Latest" does not exist.
- `send` queues by default: the message runs after the current reply, as its own turn on Codex,
  Grok and Antigravity, inside the same execution on OpenCode. `--steer` joins the running turn
  (Codex, OpenCode, Grok through a live leader) and is refused when the session is idle.
- `new --cwd` defaults to the current directory.
- `new --model` and `--effort` pass through unvalidated in the agent's syntax (OpenCode
  `--model` is `provider/model`). The effort belongs to the session: later sends keep it and
  cannot change it. Claude and Antigravity take both per process, so sends to a session
  agent-talk started pass the model and effort `new` recorded again; the other agents store them
  with the session (§6).
- `new --name N` stores a title at the agent (Codex thread name, Claude `custom-title` line,
  OpenCode `title`, Grok `_x.ai/session/rename`; Antigravity has no interface outside the TUI and
  refuses). agent-talk keeps no copy. `ls` shows `name` and strips the provenance header from
  `preview`, so a session an agent created is recognizable without reading its history.
- `read --raw` prints the agent's records of the span instead of messages. `read` shows each tool
  call as one `other` message (`[tool name] status input`), the one place normalization goes past
  text, because an agent reading a session must see that tools ran.
- `ls` without `--agent` returns every agent's first page; a failing agent is reported
  in `errors` and does not hide the others.

Errors carry the agent's `code`/`message`/`data` under `agent_error` when there is one:

```
E_NO_DAEMON     Codex socket absent; OpenCode registration missing, stale, or its pid gone
E_CAP_MISSING   a required agent capability is missing (Codex queue methods, `claude agents`)
E_PRECONDITION  agent rejected (stale turn id, idle steer, unknown session or receipt)
E_LOCKED        another agent-talk command or child holds the session (Claude, direct Grok, Antigravity)
E_FOREIGN_LIVE  held by a process agent-talk cannot talk to (Claude TUI, Grok TUI outside the
                leader, Antigravity TUI, a Codex thread written by another app-server process)
E_NO_STEER      agent cannot steer (Claude, Antigravity, Grok without a live leader)
E_MAX_HOPS      hop depth exceeded
E_TIMEOUT       deadline passed; outcome unknown, receipt retained
E_INTERRUPTED   Ctrl-C while observing; outcome unknown, receipt retained
E_TRANSPORT     connection failed or dropped, or a live service unreachable (sandbox); outcome
                unknown if an intent was sent
E_UNSUPPORTED   option or method absent in this agent
```

## 6. Agents

### 6.1 Codex (codex-cli 0.160.0)

**Transport.** Control socket `~/.codex/app-server-control/app-server-control.sock`, WebSocket
over AF_UNIX (HTTP/1.1 Upgrade on `/`, one JSON-RPC message per text frame, no local auth).
Handshake `initialize {clientInfo, capabilities: {experimentalApi: true}}`, then `initialized`;
the queue methods need `experimentalApi` (`E_CAP_MISSING`). In a sandbox without local network
access `stat` and `connect` on the socket fail with EPERM while the daemon is fine: `E_TRANSPORT`,
not `E_NO_DAEMON`.

**Who holds a thread.** The TUI runs inside the daemon unless started with `--no-daemon`, provided
`codex app-server daemon start` ran first. A second client can resume a thread open in the user's
terminal and start a turn; the TUI renders it live and keeps the context. Within the daemon every
client shares the loaded thread and gets identical events. `codex exec` is always standalone; its
history is readable through the daemon. The ChatGPT desktop app and the VS Code extension run
their own app-server; across processes Codex holds a per-thread writer lock, and `thread/resume`
from another process fails with `-32600 "thread <id> already has an active writer"` until that
process exits. agent-talk maps this to `E_FOREIGN_LIVE` (`send` refuses before recording an
intent; `wait` leaves its receipt as it was).

**Threads and history.**

- `thread/start` inherits the daemon's sandbox, MCP servers and hooks; agent-talk sets
  `approvalPolicy: never`, and `sandbox: danger-full-access` for full access. After about 60 s
  without a subscriber an idle thread unloads, and a `thread/resume` without parameters then
  restores the daemon's sandbox but keeps the approval policy (codex 0.160.1), so every resume of
  a full-access thread passes the sandbox again. `thread/queue/add` takes neither. It has no
  effort parameter but accepts `config.model_reasoning_effort`; agent-talk sends `--effort` as
  `turn/start.effort` instead (a model-specific string), which also applies to the thread's later
  turns. An invalid value is accepted and fails the turn with the model's enum error.
- `thread/name/set` fails for a moment after `turn/start` (empty rollout), so `new` names the
  thread between `thread/start` and `turn/start`.
- History (`thread/turns/list`, `itemsView: full`) needs no resume and works on not-loaded
  threads. For about a second after `turn/start` on a new thread it is unreadable:
  `thread/resume` and `thread/turns/list` fail with `-32603 "... rollout at <path> is empty"`, and
  `thread/turns/list` can fail with `-32601 "list_turns is not supported yet"` while the thread has
  no state-DB row (seen once; cause read in daemon source). `thread/read` then answers `idle` with
  no turns and is no status source. agent-talk retries `thread/resume` on these errors once a
  second, at most ten times.
- `thread/resume` on a loaded thread rejoins without replaying earlier notifications, except
  pending server requests. An idle thread unloads about 60 s after its last subscriber leaves; a
  running thread keeps running and drains its queue with nobody subscribed.
- `thread/list` returns only interactive sources unless source kinds are named (agent-talk names
  them; `--all` adds sub-agent and unknown kinds). `source` is origin, not ownership. The listing
  has one row per rollout file, so a thread the desktop app resumed into a new file repeats;
  agent-talk keeps the first (newest) row per id. `useStateDbOnly` lists each thread once but has
  null `originator` on most rows and stale `updatedAt`, so it is not used.

**Turns and sending.**

- A busy `turn/start` creates no turn: its input is folded into the active turn and displaces the
  original answer. Only `new` uses it, on the thread it just created.
- Steer is `turn/steer {expectedTurnId}` with the newest turn from the resume response; the input
  lands at the next model step.
- Queue is `thread/resume`, then `thread/queue/add`, which the daemon drains FIFO as separate
  turns. Queue id, user item id and turn id differ; correlation is `clientUserMessageId` to
  `userMessage.clientId` to the containing turn.
- On a not-loaded thread an add stays dormant until some client resumes, hence the resume first.
  On an idle loaded thread it starts a turn within milliseconds, unless the last turn was
  interrupted (or hit a budget limit): the thread then stays `Interrupted`, also across a reload,
  and nothing runs the item until `thread/queue/start` (reproduced; mechanism read in codex-rs
  source). So after an add on an idle thread `send` waits up to 2 s for `turn/started`, then calls
  `thread/queue/start` (without `--wait` the command stays connected that long). That call is
  serialized with the daemon's drain and fails, running nothing, with `"queued submission not
  found: <id>"` for an item already started and `"thread already has an active or pending turn"`
  for one behind an active turn.
- A turn ends at `turn/completed` with matching thread and turn id, carrying `status` and summary
  items; full items come from history. The `idle` status change before it is not a turn end.
  `wait` reads history first, then resumes and observes; a turn an accepted receipt names is
  observed even while history is still empty, and a turn id nobody vouches for is refused.
- `-32600` is reused for: not initialized, missing experimental capability, stale turn id, idle
  steer, `no rollout found` (a thread with no turn yet), `already has an active writer`, and both
  `thread/queue/start` refusals. agent-talk maps it by method and message.

**Approvals.** Every subscribed connection gets the same request id; any answer resolves it for all
(`serverRequest/resolved`). An unanswered request was still pending after 20 s, the longest window
observed. Closing a connection is not an answer; a later `thread/resume` replays the request.
Declined command items appear only in live events, not in history. Only the command `decline` was
exercised; the deny answers for other request kinds follow the schema. Requests still buffered
when a command ends are answered per policy before the connection closes. Under `never`, what
would need approval fails and the model is told; MCP tools then work only from servers that set
`default_tools_approval_mode = "approve"` (below).

**Identity.** MCP `tools/call` carries `params._meta.threadId` and the turn id (verified for threads
started over the daemon socket only); MCP server environments carry no `CODEX_*` variable. Shell
commands see `CODEX_THREAD_ID` plus whatever the daemon inherited. Under approval policy `never`,
Codex fails MCP tool calls unless the server entry sets `default_tools_approval_mode = "approve"`.

### 6.2 Claude Code (2.1.288)

**Transport.** Owned sessions only: one `claude -p --verbose --input-format stream-json
--output-format stream-json` process per `new` (`--session-id <uuid>`, `--name`) or `send`
(`--resume <id>`), in the session's recorded cwd. agent-talk writes one user line with `"uuid":
<client message id>` and closes stdin; the turn runs to its `result` event and the process exits,
so `send` is synchronous. `--verbose` is mandatory for stream-json in print mode.

**Sessions and turns.**

- The input `uuid` becomes the prompt line's `user.uuid` in the transcript, so the turn id is known
  before the run.
- Transcript `~/.claude/projects/<slug>/<id>.jsonl`, append-only. `--resume` from another cwd
  appends to the original file, so transcripts are located by id across all project folders.
- Turns group by `promptId`; user lines that only carry `tool_result`, and lines the runtime
  injects, are not turn starts. The live branch is the `parentUuid` chain from the newest
  `last-prompt.leafUuid` (`logicalParentUuid` crosses a compaction); `read --raw` adds orphan
  branches. Sub-agent transcripts are not read.
- End markers: the TUI writes `system/turn_duration`, or a `[Request interrupted by user]` line
  after Esc; `-p` writes none. So `wait` ends an owned turn at its run log's `result` or pid exit.
  Other turns end at a marker, a later prompt on the live branch, or once no live process holds
  the session (status from the last assistant line's `stop_reason`), all labelled best effort; a
  foreign `-p` turn that is still running stays `running` until the deadline.

**Liveness and writers.** `claude agents --json --all` lists every live process, TUI and `-p` alike,
as `kind: "interactive"` with pid and `busy`/`idle` status; background sessions are `kind:
"background"` without pid or status; exited processes disappear. agent-talk classifies pids by its
`processes` table, never by `kind`, and treats an undecodable entry as an error. Claude has no lock
between processes: a concurrent `--resume` mid-turn writes a second branch and the next `--resume`
follows only the newest leaf, dropping the other. Hence `E_FOREIGN_LIVE` for a live pid that is not
an agent-talk child, `E_LOCKED` for one that is (pid alive, no `result` in its run log),
`E_CAP_MISSING` when `claude agents` fails.

**Sending.** A second line written to a running process is absorbed into the turn or becomes the
next one at Claude's discretion, so there is no steer (`E_NO_STEER`).

**Approvals.** In `-p` Claude denies a tool that would prompt and tells the model
(`result.permission_denials`); nothing pends. agent-talk records these as `denied`, and on
sessions it started adds `--permission-prompts none`, which also tells the model not to retry.
`--permission-mode` is not kept across `--resume` (the user's `defaultMode` applies again,
claude 2.1.291), so every send to a full-access session passes `bypassPermissions`. `--effort` is
not kept across `--resume` either and is passed again like the model; `CLAUDE_CODE_EFFORT_LEVEL`
in the environment silently overrides `--effort`, so a child given `--effort` runs without it
(claude 2.1.291).
`--permission-prompt-tool stdio` would block on a `control_request` until answered on stdin, so it
is not used.

**Identity.** Claude speaks MCP 2026-07-28 without `initialize`; `tools/call` `_meta` has no session
id. Claude sets `CLAUDE_CODE_SESSION_ID` in each MCP server's environment to the spawning process's
session, overriding an inherited value (verified with `--mcp-config` only, not with a global
`~/.claude.json` registration); one server per `claude` process. Shell commands inherit the
variable unchanged. When a TUI switches session (`/clear`, `/resume`) while its MCP server keeps
running, attribution through that server may name the old session (unverified).

### 6.3 OpenCode (2.0.22)

**Transport.** HTTP to the user's background service (`opencode serve --service`), shared by every
OpenCode client. Registration `$XDG_STATE_HOME/opencode/service.json` (default
`~/.local/state/opencode/service.json`) holds `{url, pid, version, password}`; basic auth
`opencode:<password>`; `GET /api/info` has no service id, so its pid is compared with the
registration. A connect failure while that pid is alive is `E_TRANSPORT` (a sandbox, for
instance). HTTP 401 maps to `E_NO_DAEMON`, 400/404/409/422 to `E_PRECONDITION`, the rest to
`E_TRANSPORT`. Any request naming a directory makes the service load that location for its
lifetime, so agent-talk sends only directories it was given. Events: SSE `GET /api/event`, global,
unfiltered and without replay; executions continue without subscribers, so agent-talk subscribes
before submitting and filters on `data.sessionID`.

**Sessions and turns.**

- `new` is `POST /api/session {location: {directory}, model?, title?, permissions}` (model
  `{providerID, id, variant?}` from `--model provider/model`), then the first prompt as a queue
  send. `--effort` is the model's `variant`, stored with the session (the prompt has no model
  field); without `--model` it goes with the user's default model, `GET /api/model/default`. An
  unknown variant is stored, and the first turn then fails at once with no error text
  (opencode 2.0.23). The service answers a location's first requests before its configuration is
  applied, with no default model and agents lacking the user's rules (2.0.23), so `new` first
  calls `GET /api/integration`, which waits for plugin activation (read in source).
- There is no turn id; execution events carry only the session id. The client may choose the user
  message id (`msg_` prefix) and attach `metadata`, both stored verbatim. agent-talk sends `{id,
  text, delivery, metadata: {"agent-talk": {receipt, from, depth}}}` with an id in the
  service's ascending format; turn id = queue id = that message id.
- An execution end writes an `idle` row with `outcome: succeeded|failed|interrupted`, except an
  interrupt with reason `shutdown` (a message-less permission reject, the service going down): no
  `idle`, and the aborted assistant row stays last. Read in source, not observed: a restart
  resumes such a turn behind a `synthetic` row with `metadata.notice: restart`.
- `GET /api/session/active` is the only running indicator; a session waiting on a permission still
  counts as running.
- `wait` ends the turn at the first `idle` after the user message (status = its outcome, reply =
  the last assistant text before it), or at an aborted assistant row followed directly by a user
  row, or at an aborted row still last when `session/active` no longer lists the session on a
  second read (`interrupted`). Anything else after an aborted row (its `idle`, a restart notice,
  more assistant rows) belongs to the same turn. History is re-read at each execution end.
- History pages by opaque cursor, in delivery order; the empty final page has null cursors, so a
  caller keeps its old cursor as a "since" marker. A `type=user` read behind a page's cursor gives
  the turn of a page that starts mid-turn. `read` hides `idle` and bookkeeping rows.
- `ls` is `GET /api/session?parentID=null` (`--all` drops the filter) plus `/api/session/active`.

**Sending.** Busy prompts never fail, and the default `delivery` is `steer`, so agent-talk always
sets it. Steer joins the running execution at the next step boundary with every other pending
steer item, possibly answered in one reply; queue runs after the current reply in the same
execution; on an idle session either starts an execution at once. Idle steer is refused
(`E_PRECONDITION`) after a check of `/api/session/active`, which can race an execution that is
just starting. Queue items pending at a user interrupt stay dormant until another execution;
after a `shutdown` interrupt they run at once in a successor. One reply can cover several messages.

**Approvals.** `permission.asked` goes to every subscriber and is not replayed, so agent-talk also
lists `GET /api/session/{id}/permission`. An unanswered request blocks with no timeout. `reject`
with a `message` hands the text to the model and the execution continues; a message-less `reject`
interrupts it (`shutdown`); any `reject` rejects every pending request of the session, so a deny
that finds its request gone records `resolved`. `always` would save a project-wide rule.
A request is answered by the last rule matching action and resource (`*` and `?` wildcards) in
the agent's ruleset followed by the session's, and asks when none matches; the agent's ruleset
already holds the user's configuration (read in source; verified live on 2.0.23: a session
rule `edit *allowed.txt allow` after `edit * deny` wrote `allowed.txt` and refused
`denied.txt`). A session agent-talk starts therefore gets session rules at creation, so that the
service itself refuses what would wait for an answer:

- full access: `{action: "*", resource: "*", effect: "allow"}`, which overrides the user's rules
  (verified against `edit: ask`);
- otherwise the rules of the agent a new session runs (the default agent, which `GET /api/agent`
  lists first) from its first `ask` on, in order, with every `ask` turned into `deny`. Allowed
  and denied requests keep their answer; one that would ask is refused with nothing pending. The
  rules before the first `ask` answer the same without the copy and are left out, because
  sub-sessions of the subagent tool inherit session rules (an `explore` child given the whole
  copy, which starts with `* * allow`, ran `ls` its own rules deny). Every agent's rules start
  with `* * allow`, so no request goes unmatched (which would ask);
- in both cases `question * deny` last: nobody answers the question tool in these sessions.

The model sees a tool denied for every resource (the user's `edit: ask`) as absent from its
toolset, and a denied call as a tool error `permission.rejected` ("Permission denied: edit"); the
turn continues (verified on 2.0.23 with `send` without `--wait`). Sessions agent-talk did not
start keep their rules.

Requests that still reach agent-talk (a plugin hook can turn an answer into `ask`) get `reject`
with a message, and only once its own message was delivered (earlier requests belong to the
execution ahead of it).

**Identity.** Shell commands get `OPENCODE_SESSION_ID`, overwriting an inherited value. MCP
`tools/call` carries `_meta["ai.opencode/sessionID"]`: read in source and docs, never observed
live; the docs say it can be absent for calls without session context. One MCP server process
serves every session of a location, so its environment does not name the caller.

### 6.4 Grok CLI (1.0.46)

**Transport.** ACP (line-delimited JSON-RPC over stdio) to a `grok agent ... stdio` child. When the
leader socket accepts a connection the child is `grok agent --leader stdio`, a proxy into the
shared leader, where sessions behave like Codex threads in the daemon. Otherwise it is `grok agent
--no-leader stdio`, an in-process agent that holds the session only while the command runs.

- The socket is `$GROK_LEADER_SOCKET`, else `<GROK_HOME>/leader.sock` (`GROK_HOME` defaults to
  `~/.grok`). `--leader-socket` is passed only when the variable is set.
- `--leader` with no live socket spawns a persistent leader, so agent-talk passes it only after a
  connect succeeded. If the leader exits between probe and child, Grok spawns a new one;
  agent-talk compares the pid in the lock file next to the socket before and after and reports a
  change. The default lock is `leader.lock`; that `x.sock` locks at `x.lock` is the adapter's
  assumption, not observed.
- Only `ENOENT`, `ECONNREFUSED` (a stale socket) and a path too long for `SUN_LEN` select direct
  mode; `EPERM`/`EACCES` (a sandbox) is `E_TRANSPORT`.
- The child gets `GROK_DISABLE_AUTOUPDATER=1` (`grok agent` has no `--no-auto-update`).

**Store.** `<GROK_HOME>/sessions/<urlencoded cwd>/<id>/`, one format for TUI, `-p`, ACP and leader.

- `updates.jsonl` is the conversation. A turn opens with `user_message_chunk
  {_meta.promptIndex}` and closes with `turn_completed {prompt_id, stop_reason:
  end_turn|cancelled|interrupted}`. User lines carry no prompt id, agent lines carry
  `_meta.promptId`; a steer message is a user line with `interjection: true`. A prompt cancelled
  within milliseconds of starting gets a `turn_completed` and no user line. `read` and foreign
  `wait` use these rules; the cursor is the line number.
- `summary.json` has id, cwd, `generated_title` and `session_kind` (`headless` for `-p`).
- `<GROK_HOME>/active_sessions.json` lists live TUIs with pid, updated on start, `/new` and `/exit`;
  `kill -9` leaves a row, so pids are checked. `-p`, ACP and leader-only sessions never appear.
- `ls` reads the summaries and `active_sessions.json`, plus the leader's `_x.ai/sessions/list` when
  a leader runs (spawning a client, about 0.7 s).

**Writers.** Grok has no lock between processes: a second writer marks the running turn
`interrupted` on disk, both keep appending (a prompt can get two `turn_completed`; the last
counts), and their memories diverge. Inside one leader all clients share one in-memory session:
`session/load` attaches live, a busy `session/prompt` queues a separate turn, `_x.ai/interject`
joins the running turn, events fan out to every client, and a turn survives its client's
disconnect. A TUI started with `--leader` is joinable the same way; an idle session is unloaded
when its last client leaves. The only holder markers are `active_sessions.json` and the leader's
list, so a foreign `-p` or direct ACP writer is invisible. agent-talk refuses `E_FOREIGN_LIVE` when
`active_sessions.json` names a live pid and no leader lists the session `resident`, and `E_LOCKED`
for its own direct child still running; it takes its own lock only on the direct path, after the
leader check, since clients of one leader do not exclude each other.

**Turns and sending.**

- `new`: `session/new {cwd, mcpServers: []}`, `session/set_config_option {sessionId,
  configId: "reasoning_effort", value}` for `--effort`, `_x.ai/session/rename` for `--name`, then
  `session/prompt {_meta: {promptId}}`. The client's `promptId` is persisted as
  `turn_completed.prompt_id`, which makes it the turn id.
- The effort is set on the session because a leader proxy ignores `grok agent
  --reasoning-effort` (it honours `-m`; grok 1.0.46). It is stored in `summary.json` and kept by
  later loads, direct and through a leader. An unknown value is refused with `-32602 "unknown
  reasoning_effort value"`; `new` then fails with `E_PRECONDITION` before recording anything, and
  the error names the new session, which stays on disk without messages and which `grok sessions
  delete` removes (agent-talk deletes nothing, §7).
- Queue: `session/load` plus `session/prompt`, through a leader only when `_x.ai/sessions/list`
  there shows the session `resident` (a listed but dormant session may be held in-process by a
  TUI). That list's `activity` and `resident` describe the answering process, so they mean
  something only through a leader.
- Acceptance is the first `_x.ai/queue/changed` listing the prompt, then `runningPromptId` names
  it. The `session/prompt` response resolves at turn end, possibly milliseconds before
  `turn_completed` is persisted, so `final_text` comes from the prompt's live message chunks. A
  permission cancellation is an accepted prompt whose turn ended `cancelled`.
- Steer: `_x.ai/interject` on a session `working` in a live leader (`E_NO_STEER` without one,
  `E_PRECONDITION` when idle). It has no client id and no expected-turn check: the ack is
  acceptance, the turn comes from `_x.ai/session/interjection`, and if the turn ended meanwhile
  Grok starts a fallback turn (`interject-fallback-...`) that the receipt then names.
- In direct mode the turn dies with the child (closing stdin ends it within about two seconds,
  with no `turn_completed`), and a `session/cancel` sent before `runningPromptId` names the prompt
  is ignored. So the command runs until the turn ends even without `--wait`; when a deadline or
  Ctrl-C ends it, agent-talk waits up to 10 s for the prompt to run, sends `session/cancel` and
  waits for the `cancelled` response. Through a leader the turn continues after the command.
- `wait` on a turn agent-talk is not running polls `updates.jsonl` for its `turn_completed`; a
  direct child that exited without one leaves the turn `interrupted` or `unknown`.

**Approvals.** The user's `permission_mode` (`always-approve` on this machine) applies to ACP
sessions, and agent-talk keeps it. On sessions it started agent-talk answers `reject_once` to this
prompt's requests, matched by `toolCallId`. Full access sends `_meta.yoloMode: true` on
`session/new` and on a direct-mode `session/load`; it has no effect on a session resident in a
leader. `session/request_permission` goes to every attached client with one id; the first
answer wins, `reject_once` ends the turn `cancelled`. An unanswered request was still pending after
25 s. Which commands prompt with yolo off varied between runs on 1.0.46, and later runs provoked
none, so the deny and observe paths rest on frames captured earlier.

**Identity.** `GROK_SESSION_ID` is set in shell commands and in each MCP server's environment (one
server per session); `tools/call` `_meta` carries only `progressToken`. Grok also loads MCP servers
from `~/.claude.json`, so a Grok session starts `agent-talk mcp` if the user registered it there.
`grok -p` (one turn per process, no stdin input) is not used.

### 6.5 Antigravity CLI (agy 1.2.16 and 1.2.17)

agy updates itself in the background (1.2.16 became 1.2.17 between two runs), so behaviour can
change under an installation. Facts hold for both versions unless noted; the database layout and
the TUI overwrite cases were seen on 1.2.16 only.

**Transport.** One `agy -p "" --input-format stream-json --output-format stream-json --model M
--effort E [--conversation <id>]` process per `new`/`send`, in the conversation's cwd (there is no
cwd flag); one user line on stdin, then stdin closed. Events: `init {conversation_id}`,
`step_update`, `result` (turn end). Every agy process, TUI or `-p`, embeds its own language server
behind a per-process CSRF token known only to its own tool shells, so there is no shared daemon and
no way to join a conversation another process holds.

**Store** (`~/.gemini/antigravity-cli/`).

- `conversations/<id>.db`: SQLite, one protobuf blob per step, authoritative.
- `brain/<id>/.system_generated/logs/transcript.jsonl`: one object per step (`step_index`, `type`,
  `status`, `content`, `tool_calls`, `created_at` in seconds). Lossy: steps that never ran are
  absent, a tool step written `RUNNING` is never updated, long content is truncated, two writers
  leave duplicate indices. User text is wrapped in `<USER_REQUEST>` plus metadata blocks.
- `conversation_summaries.db`: title, `workspace_uris` (the first is the cwd), `status`
  (`CASCADE_RUN_STATUS_IDLE|RUNNING`, empty on rows an older version wrote), `last_modified_time`;
  WAL. When the last agy process exits it deletes the db's `-wal` and `-shm`, and a read-only open
  then fails with `SQLITE_CANTOPEN` (14) because a read-only connection cannot recreate `-shm`.
  agent-talk opens it read-only and falls back to `immutable=1`, exact while no writer is open.
- `presence/<id>.lock`: flock held by the process that has the conversation open (`-p` from
  startup, a TUI from its first prompt, moved on `/new`, released on exit or `kill -9`). The file
  stays after release, and the lock does not name its holder.
- TUI and `-p` conversations are indistinguishable. The CLI has no list, rename or delete command;
  the TUI and the language server can rename and delete.

**Runs and turns.**

- A `-p` process emits `init` and takes the presence lock at startup (about 7 s), before reading
  stdin. The intent is written once `init` names the conversation, before the message, so a
  deadline before `init` leaves no receipt and possibly an empty conversation.
- `--model <alias>` without `--effort` fails before `init`, and a resumed conversation forgets its
  model, so `send` reuses the model and effort `new` recorded.
- `--conversation <unknown id>` silently starts a new conversation, so `send` checks that the
  conversation exists and that `init` returns the same id.
- The stream has no client message id and no turn id. The `user_input` step update is the first
  proof the message was taken, and its step index is the turn id; `wait --receipt` recovers it,
  the `result` and the denials from the run log (without the log an unknown receipt stays unknown;
  no text matching).
- `result.response` joins every response of the turn, commentary included, so the reply is the
  last `agent_response` step's text. On a resumed conversation `result` reports conversation
  totals, not the turn's.
- A line written during a turn becomes the next turn: queue only (`E_NO_STEER`).
- The summary `status` is `RUNNING` during a turn and `IDLE` after. A killed `agy -p` releases the
  lock at once but left `RUNNING` in some runs and `IDLE` in others (both on 1.2.17), so the status
  is no evidence of liveness either way and the presence lock is used. A run waiting on a
  backgrounded command is `IDLE`, and Esc in the TUI stores the partial reply as `DONE`.
- A foreign turn's `wait` needs its `USER_INPUT` and ends at the next `USER_INPUT`, or at a final
  `PLANNER_RESPONSE` without tool calls once the lock is free and the status is not `RUNNING`;
  short of that it is `running` or `unknown` with a `basis`. Session `IDLE` alone never counts.
- `read` groups the transcript by `USER_INPUT`; `SYSTEM_MESSAGE` steps come `from` the runtime. The
  cursor is the line position, `step_index` only attribution. In `ls`, `state` is `running` while
  an agent-talk child runs or the lock is held with status `RUNNING`, `unknown` when the lock is
  free but the status is `RUNNING`, else `idle`. `ls --all` lists nothing more, since no
  sub-conversation rows were observed.

**Writers.** agy ignores its own presence lock. A second `-p --conversation X` runs while another
process holds X; both write by step index, the database keeps one process's rows and the
transcript both. A TUI's next prompt overwrites steps a headless run appended, and a TUI that
switched away with `/new` keeps X in memory and overwrites on `/resume`; nothing on disk reveals
that cache. agent-talk takes its own lock, then refuses `E_FOREIGN_LIVE` while the presence lock is
held by anything but a recorded live agent-talk child and `E_LOCKED` for such a child; a lock
error other than contention is an error, not "held".

**Approvals.** Headless agy denies every tool not allow-listed in the user's `settings.json` and
ends the turn (`denied_actions` in `result`, naming only the action; exit 0). Nothing pends, and
denials are recorded as `denied`. A denied step is absent from the transcript on 1.2.16 and
written as `GENERIC` `ERROR` on 1.2.17. Full access passes `--dangerously-skip-permissions` on
every run, because a resumed conversation falls back to the user's `toolPermission` (1.2.17).
`--mode plan` and `--sandbox` change nothing in headless runs.

**Identity.** MCP `tools/call` carries `_meta["antigravity.google/conversation_id"]`; the server's
environment has no `ANTIGRAVITY_*` variable. Shell commands get `ANTIGRAVITY_CONVERSATION_ID`. MCP
servers are configured globally only.

## 7. Key decisions

- **Rust, single binary.** Agents call the CLI many times per session; startup time and one
  artifact per platform matter.
- **No agent-talk daemon.** The agents already run the shared processes; another daemon would add
  install, pid and version-skew problems for state that fits in SQLite. Long-lived state, if ever
  needed, goes into the per-session `agent-talk mcp` process.
- **The user configures the MCP server globally.** agent-talk never injects itself into a session
  or edits agent configuration, even where the agent's API allows per-thread servers.
- **Never approve.** `deny` is the strongest answer; `once`, `always` and `accept` are never sent.
- **Sessions agent-talk did not start are the user's.** agent-talk never answers their approval
  requests and never changes their settings; declining would change a turn the user's TUI is
  attached to.
- **Permissions: the agent's own, or full access.** Restricted levels (read-only, workspace write)
  were tested on 2026-10-06 and left out: only Claude could enforce one with web tools allowed,
  Codex loses the sandbox when a thread unloads and the queue path cannot carry it, OpenCode's
  shell bypasses its edit rules, and Grok and Antigravity have no per-session control.
- **Delete nothing.** No session, conversation or agent file is ever deleted, on any agent.
- **Agent multi-client processes are joined, never started.** Codex daemon, OpenCode service,
  Grok leader: agent-talk connects when they are live and takes the agent's single-process path
  otherwise. Grok's `--leader` is passed only after the socket answered; the remaining window is
  declared and a changed leader pid reported (§6.4).
- **Two-writer agents get agent-talk's lock plus the agent's liveness marker.** Claude, Grok
  without a leader and Antigravity have no lock between processes; agent-talk refuses to write
  while the agent's marker says another process holds the session (Claude live pid, Grok
  `active_sessions.json`, Antigravity presence lock) and locks its own children.
- **Turn ids come from the agent or from agent-talk's own client id**, never from matching text:
  Codex turn id, Claude user-line uuid, OpenCode user message id, Grok `promptId`, Antigravity
  `USER_INPUT` step index (§6).
- **History is paged to the answer.** `read --tail N` and turn attribution follow the agent's
  cursor until the rows are found or history ends. The only fixed caps are the 40 pages an
  OpenCode turn lookup searches (older messages report absent) and the ten Codex resume retries.
- **Shared lifecycle, separate observe loops.** Receipt and approval handling lives once; a generic
  event-loop driver would hide genuinely different end-of-turn authorities behind mode flags.
- **OpenCode asks become denials by copying the default agent's rules from the first `ask` on**,
  not by appending a `deny` per `ask` rule, which would override a later, narrower `allow` in the
  user's configuration, and not by copying them all, which would override sub-agents' rules
  (§6.3).
- **Explicit agent knobs, not a generic option map.** Five knobs with clap help beat `--opt k=v`
  with per-adapter parsing. Values pass through unvalidated: the agent owns the enum (Codex
  rejects an unknown effort in the turn, not at submission).
- **Steer is refused when idle** on every agent that would silently start a new turn instead.
- **No tight polling.** Observation uses the agents' event streams. The polls that exist: Claude
  foreign sessions, Grok `updates.jsonl` and Antigravity transcripts in `wait` (1 s; no event
  source exists), agent-talk's own `claude -p` and `agy -p` run logs (50 ms file tail), and Codex
  `thread/resume` while a new thread's history is unreadable (1 s, at most ten).

## 8. Known limitations and open items

- Claude `control_request` approvals need an answerer (`--permission-prompt-tool stdio`);
  agent-talk relies on the agent's own denial.
- Connection loss mid-`wait` ends with `E_TRANSPORT` (OpenCode re-reads history once on a clean
  stream end, Codex does not); recovery is `wait --receipt`.
- Claude `wait --receipt` needs the transcript before it consults the run log, and returns no
  approvals (denials after the sending command stopped observing are not recovered).
- Claude and Grok identify a recorded child by pid and an unfinished turn only; after a crash and
  pid reuse a session can stay `E_LOCKED` until the row ages out of the newest ten.
- Claude: whether `CLAUDE_CODE_SESSION_ID` reaches a globally registered MCP server the same way,
  and whether it goes stale after the TUI's `/clear` or `/resume`; the end marker of TUI turns
  that fail mid-turn.
- OpenCode: history alone cannot tell a message-less reject from a graceful shutdown, a crash
  mid-step from a running step, a `shutdown` interrupt before the first step from steer batching,
  or a queued item delivered into a reloaded continuation from a successor execution.
  `session.execution.failed`, the restart resume and the MCP `_meta` session id were never
  observed.
- Codex: `wait --turn` on a steered turn attaches the newest receipt on that turn (steered
  messages share the turn id). A thread written by another app-server process is refused only at
  `thread/resume`, and `ls` does not show which process holds it. Only `send` starts a dormant
  queued item; `wait --receipt` on one left dormant earlier reports it `pending`. How a terminal TUI
  renders a decline from agent-talk is unverified.
- `agent-talk mcp` ignores rmcp's cancellation; a cancelled call keeps observing until its
  deadline, and server shutdown waits for it.
- `status` marks a Codex method unavailable only when the daemon reports it missing; any other
  refusal of the probe on a dummy thread counts as available.
- Sender attribution cannot detect a daemon or service started from another agent's shell.
- A Grok turn that continues in a leader after the sending command stopped observing waits for
  another client to answer a permission request it raises.
- OpenCode sessions agent-talk starts: sub-sessions of the subagent tool inherit the session's
  rules, which decide over the sub-agent's own from the default agent's first `ask` on. The rules
  are a copy taken at `new`: later changes to the user's configuration do not reach the session,
  and approvals saved with `always` and plugin permission hooks no longer turn an `ask` into
  `allow` there.
- Grok: `reject_once`, an unanswered request and the `session/cancel` at the deadline rest on
  captured frames, not re-verified; whether a TUI renders live a turn another
  leader client ran; leader behaviour with a grok.com subscription login (it may open a relay);
  the lock path of a custom leader socket is assumed.
- Antigravity: a TUI that switched conversations can overwrite steps an agent-talk `send`
  appended once it resumes its cached copy; `--print-timeout` expiry was not observed;
  self-updates change behaviour between versions (the denied step already differs); a denied
  action is not tied to its tool step; the `immutable=1` fallback can read a stale page when a
  writer starts at that moment.
