# agent-talk design

`agent-talk` lets a person or an agent send a message to another agent's session and read the
reply, across agents, using only each agent's official non-interactive interfaces. This file
states the key design and the agent behaviour it rests on; the code holds the details. Agent
behaviour in §6 was observed on macOS with the version named in each heading unless marked as
read in agent source or documentation.

## 1. Scope

In: Codex (shared app-server daemon), Claude Code (`claude -p`), OpenCode 2.x (shared background
service; 1.x is not supported), Grok CLI (ACP to `grok agent stdio`, through the shared leader
when one runs), Antigravity CLI (`agy -p`), pi (`pi --mode json`); a CLI and an MCP server
exposing the same operations; sender attribution and a provenance header for agent-to-agent
traffic; a hop limit.

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
   unless `--full-access` asks for more.
5. **Thin.** Normalize only conversation-level events (text, role, phase, turn boundaries,
   approval requests). Everything else passes through as raw agent JSON behind `read --raw`.
6. **Receipts, not assumptions.** Every mutation returns a receipt stating what was accepted. A
   timeout is an unknown outcome, not a rejection; mutations are never retried automatically.

## 3. Architecture

### No agent-talk daemon

State lives where the agent already keeps it:

| agent                                                | who holds the live session          | agent-talk's connection                                   |
| ---------------------------------------------------- | ----------------------------------- | --------------------------------------------------------- |
| Codex                                                | shared app-server daemon            | per command: connect, subscribe, act, observe, disconnect |
| OpenCode                                             | `opencode serve --service`          | per command: HTTP, plus SSE while observing               |
| Grok, leader live                                    | shared leader process               | per command: a `grok agent --leader stdio` child (ACP)    |
| Claude, Grok without leader, Antigravity, pi (owned) | nobody between turns; files on disk | one agent child per mutation                              |
| Claude, Grok, Antigravity TUI (foreign)              | the user's terminal process         | read-only                                                 |
| pi TUI (foreign)                                     | the user's terminal process         | undetectable; a send branches the session (§6.6)          |

"Owned" means agent-talk started the session; it records creation, not current exclusivity.
Each CLI command holds its connection, subscription and correlation state only while it runs.
`agent-talk mcp` is the one long-lived process, one per agent session, started and stopped by the
agent like any MCP server; state that must outlive a command would belong there, not in a system
daemon (§7).

### Local state (`~/.agent-talk/`)

`store.db` (SQLite) holds what agent-talk itself knows: the intent of every message, written
before anything is sent, with its sender and hop depth; its receipt; the sessions agent-talk
started and their settings; the agent children it spawned, by pid, which is how a later command
tells an agent-talk run from a foreign writer; and every approval request seen with what happened
to it. It holds no connection state.

Sessions of agents with no lock of their own between processes (Claude, Grok in direct mode,
Antigravity, pi) are locked for the duration of a command with an OS file lock in `locks/`, so two
agent-talk commands never write to one session at once. A lock is released by process exit and
never inherited by an agent child; a child that outlives its command is tracked by pid instead.
The children of Claude, Antigravity and pi write their stdout to a run log per receipt
(`<agent>-runs/`), which the command tails and later commands read to recover the receipt.

Agent children get the user's environment minus `AGENT_TALK_CALLER` and the agents' session
variables (§4); otherwise the agent would pass them to its own shells and MCP servers and the
child session's messages would be attributed to agent-talk's caller.

### Layout

```
src/
  main.rs              CLI: clap definitions, sender from the shell environment, human output, exit codes
  mcp.rs               the operations as MCP tools (rmcp over stdio); sender from _meta / env
  ops.rs               the operations, independent of front-end: handles, hop limit, settings, dispatch
  model.rs             agent-neutral types and error codes
  store.rs             SQLite (rusqlite, bundled)
  agents/mod.rs        the adapter interface and dispatch, request types, provenance header
  agents/lifecycle.rs  receipt and approval lifecycle, observation bounds (deadline, signals)
  agents/history.rs    read paging (query, page cut, cursors); remote history shared by Codex and OpenCode
  agents/child.rs      agent CLI children: environment, version, session lock, run logs
  agents/decode.rs     tolerant field decoders
  agents/<agent>/      one directory per adapter: codex, claude, opencode, grok, antigravity, pi
tests/live.py          regression harness against the real agents; see tests/README.md
skills/agent-talk/     the skill that teaches agents the CLI (installed with `npx skills add`)
```

The receipt and approval lifecycle (§4) is agent-talk's semantics, not an agent's, so it is
implemented once, in `agents/lifecycle.rs`. That includes an agent child that failed to spawn or
exited before taking the message: its receipt is `rejected` whatever the error, since nothing
ran. Adapters supply discovery, submission, the event source, history lookup and message
normalization, and each keeps its own observe loop, because what proves a turn ended differs
between agents (§7).

Agent records and events are decoded into small structs where that reads better than indexing
JSON. The decoding is tolerant, so that one field an agent writes differently does not lose a
record, a listing or the end of a turn: optional fields default, unknown variants are tolerated,
and a field that does not fit reads as its default (an odd list entry is skipped) instead of
dropping the record around it (`agents/decode.rs`). The raw record is kept for `read --raw` and
error data, and content a struct does not recognize never counts as the end of a turn. Decoding
is strict only where a failure must be an error, such as the ids that correlate a response or a
record with agent-talk's own request. Payloads agent-talk only passes through stay
`serde_json::Value`.

## 4. Model

### Observations

- `history`: `visible` or `none`, from the agent's history listing or files.
- `loaded`: `yes`, `no` or `unknown`: whether a live process holds the session, as far as the
  agent shows it (§6).
- `origin`: the agent's label for where the session came from (`codex-tui`, `codex_exec`,
  `claude-interactive`, `headless`, ...), or `agent-talk` for sessions it started.

For Codex, "not loaded" cannot distinguish a stopped thread from a live standalone `codex exec`
or `--no-daemon` run; agent-talk proceeds through the daemon.

### Types

```
session  { handle, agent, id, cwd?, name?, preview?, observations, state: idle|running|waiting|unknown, owned }
turn     { handle, turn_id, status: completed|failed|interrupted|unknown, final_text?, error?, duration_ms?, basis? }
message  { turn_id, item_id, role: user|assistant, phase: prompt|final|commentary|other, text, from?, timestamp? }
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

`new` and `send` write the intent, submit, and mark the receipt `accepted` with the agent's ids;
an agent refusal marks it `rejected`. When observation ends without a result (deadline, Ctrl-C or
SIGTERM, transport loss) a `pending` receipt becomes `unknown`, and the error carries the receipt,
the approvals seen so far and how far the message got (`state`): `pending` (queued, not running),
`running`, `waiting` (an approval is unanswered) or `unknown`. An `accepted` or `rejected`
receipt never falls back to `unknown`, so a concurrent command's proof is not overwritten. An
`unknown` one is resolved by later proof: delivery seen in history, the queue or a run log moves
it to `accepted`, a run log showing the message was never taken to `rejected`. `wait --receipt R`
or `wait --turn T` looks for that proof.

### Approvals

Agents fan approval requests out to every subscribed client, and any one answer resolves them
for all. agent-talk never approves.

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

### Settings

`--model`, `--effort` and `--full-access` are a session's settings. Model and effort pass through
unvalidated in the agent's syntax: the agent owns the set of valid values. `new` sets them.
`send` changes them from its message on, only on a session agent-talk started and not with
`--steer`, which has no turn of its own; otherwise it is refused with `E_PRECONDITION`.

agent-talk stores the settings of the sessions it started and passes again on each send what the
agent does not keep between turns (§6). The agent itself keeps a model change on every agent but
Antigravity, an effort change on Codex, OpenCode and Grok, and full access on OpenCode; the
user's own client then sees the change too. A change the agent refused is taken back from
agent-talk's store, so a refused value is not passed again.

Full access gives a session every permission and no approval prompts (Codex
`danger-full-access`, Claude `bypassPermissions`, an OpenCode allow-all session rule, Grok
`yoloMode`, Antigravity `--dangerously-skip-permissions`; pi has no prompts to lift). Without it
the session runs under the agent's own configuration. Once given it is not taken back.

### Provenance and loops (best-effort)

A message from another agent is answered in the receiving turn's final response, which the
sender gets through `send --wait`, `wait` or `read`. An agent sender's message is delivered with
the first line `[from <handle> via agent-talk; answer in your final response]`, so the receiving
model, and a person watching the session, sees who is talking and where the answer goes. The
intent keeps the original text and records the delivered one. All of this is attribution, not
authenticated identity.

Each intent records its sender and a hop depth: for an agent sender, one more than the depth of
the intent that started the caller's current turn (or of the newest non-rejected intent
delivered to the caller's session); for a person at the CLI, 0. A depth above 3 is refused with
`E_MAX_HOPS`, a fixed backstop against agents messaging each other without end.

The sender is taken, in this order, from:

1. an explicit pin: `--from` on the CLI, `--caller` on `agent-talk mcp`, or `AGENT_TALK_CALLER`;
2. in `agent-talk mcp`, the `tools/call` `_meta` of agents that send one (Codex thread and turn,
   OpenCode session, Antigravity conversation), then the server's environment: `GROK_SESSION_ID`,
   then `CLAUDE_CODE_SESSION_ID`, which those agents set for each MCP server they spawn;
3. on the CLI, the agent session variables of the calling shell: `CODEX_THREAD_ID`,
   `OPENCODE_SESSION_ID`, `GROK_SESSION_ID`, `ANTIGRAVITY_CONVERSATION_ID`, `PI_SESSION_ID`, then
   `CLAUDE_CODE_SESSION_ID`;
4. otherwise `unknown`.

The order exists because environments are inherited: Claude's variable reaches every shell
command below it, so it goes last, and a Codex daemon started from a Claude shell carries an
unrelated `CLAUDE_CODE_SESSION_ID`, so `_meta` goes before the environment. A daemon or service
started from another agent's shell still carries that agent's variable, which no order can detect
(§8).

### MCP server

`agent-talk mcp [--caller H]` is an ordinary stdio MCP server that the user configures once,
globally; agent-talk never writes agent configuration or injects itself into sessions. It serves
`models / ls / new / send / read / wait` with the JSON the CLI prints under `--json`; refusals are
tool errors, not protocol errors. `new` requires `cwd`, because the server's working directory is
not the caller's.

Code-mode clients (Codex for some models, OpenCode, pi) call tools from a script and pass results
on without the model reading them, so every tool declares an `outputSchema` generated from the
CLI's types and returns `structuredContent` as well as text, with behaviour annotations. The
server speaks both the stateless 2026-07-28 protocol (Claude Code, Antigravity) and the
`initialize` handshake of earlier versions (Codex, Grok). Logging is stderr only.

Only the CLI ends observation on Ctrl-C or SIGTERM. Once tokio listens for a signal, its default
action never returns for the life of the process, so the server keeps the default actions: a
signal stops the server and the requests it is running.

## 5. CLI

```
agent-talk status
agent-talk models A [QUERY] [--limit N] [--cursor C]
agent-talk ls [--agent A] [--cwd DIR] [--all] [--limit N] [--cursor C]
agent-talk new A "prompt" [--cwd DIR] [--name N] [--model M] [--effort E] [--full-access]
    [--wait] [--timeout S] [--from H]
agent-talk send H "text" [--steer] [--model M] [--effort E] [--full-access] [--wait] [--timeout S]
    [--from H]
agent-talk read H [--limit N] [--cursor C] [--all] [--raw]
agent-talk wait H (--turn ID | --receipt R) [--timeout S]
agent-talk mcp [--caller H]
```

- `--json` on every command; default output is for humans. Exit codes: 0 ok, 2 refused with a
  stable error code, 3 deadline or Ctrl-C/SIGTERM ended the command (outcome unknown), 4
  transport failure.
- `send` without `--wait` promises only that the agent accepted the message. On Claude, Grok,
  Antigravity and pi the command still runs to the end of the turn, because the turn lives in the
  child (and, for Grok, because a direct-mode turn dies with it).
- On agents with an event stream (Codex, OpenCode, Grok) `send` subscribes before it submits,
  so no event of the turn is missed.
- `wait` checks history first (the turn may be complete), then observes. It names one turn or
  receipt; there is no "latest".
- `send` queues by default: the message runs after the current reply, as its own turn on Codex,
  Grok, Antigravity and pi, inside the same execution on OpenCode. `--steer` joins the running turn
  (Codex, OpenCode, Grok through a live leader) and is refused when the session is idle.
- `models A` lists what `--model` and `--effort` take on one agent, as `{id, efforts}` rows read
  from the agent each time. Claude Code has no listing interface; its rows are the aliases and
  effort levels `claude --help` names, the one model list agent-talk carries itself.
- `new --name N` stores a title at the agent (Antigravity has no interface for it and refuses).
  `ls` shows `name` and strips the provenance header from `preview`, so a session an agent
  created is recognizable without reading its history.
- `read` returns the newest N messages, oldest first, and `next_cursor` names the page of older
  ones. By default a page holds `prompt` and `final` messages, the conversation itself; `--all`
  adds `commentary` and `other`, each tool call being one `other` message (`[tool name] status
  input`), the one place normalization goes past text. N counts the messages returned, on every
  agent. `--raw` prints the agent's records of the span instead.
- `ls` without `--agent` returns every agent's first page; a failing agent is reported in
  `errors` and does not hide the others.

Errors carry the agent's `code`/`message`/`data` under `agent_error` when there is one:

```
E_NO_DAEMON     Codex socket absent; OpenCode registration missing, stale, or its pid gone
E_CAP_MISSING   a required agent capability is missing (Codex queue methods, `claude agents`)
E_PRECONDITION  agent rejected (stale turn id, idle steer, unknown session or receipt), or a
                settings change §4 does not allow
E_LOCKED        another agent-talk command or child holds the session (Claude, direct Grok,
                Antigravity, pi)
E_FOREIGN_LIVE  held by a process agent-talk cannot talk to (Claude TUI, Grok TUI outside the
                leader, Antigravity TUI, a Codex thread written by another app-server process)
E_NO_STEER      agent cannot steer (Claude, Antigravity, pi, Grok without a live leader)
E_MAX_HOPS      hop depth exceeded
E_TIMEOUT       deadline passed; outcome unknown, receipt retained
E_INTERRUPTED   Ctrl-C or SIGTERM while observing; outcome unknown, receipt retained
E_TRANSPORT     connection failed or dropped, or a live service unreachable (sandbox); outcome
                unknown if an intent was sent
E_UNSUPPORTED   option or method absent in this agent
```

## 6. Agents

Each subsection states the interface agent-talk uses, who can hold a session, how delivery,
acceptance, the end of a turn and approvals are observed, which settings the agent keeps, and how
a calling session is identified.

### 6.1 Codex (codex-cli 0.160.0)

**Interface.** The shared app-server daemon's control socket
`~/.codex/app-server-control/app-server-control.sock`: WebSocket over AF_UNIX, JSON-RPC, no local
auth. The queue methods need `experimentalApi` at `initialize` (`E_CAP_MISSING` otherwise). In a
sandbox without local network access the socket fails with EPERM while the daemon is fine:
`E_TRANSPORT`, not `E_NO_DAEMON`.

**Who holds a thread.** The TUI runs inside the daemon unless started with `--no-daemon`, provided
`codex app-server daemon start` ran first. Within the daemon every client shares the loaded
thread and gets identical events, so a turn agent-talk starts shows live in the user's TUI.
`codex exec` is always standalone; its history is readable through the daemon. The ChatGPT
desktop app and the VS Code extension run their own app-server; across processes Codex holds a
per-thread writer lock, and `thread/resume` from another process fails with `-32600 "thread <id>
already has an active writer"` until that process exits: `E_FOREIGN_LIVE`, refused before an
intent is recorded.

**Threads and history.**

- `new` is `thread/start` (inherits the daemon's sandbox, MCP servers and hooks) with
  `approvalPolicy: never`, then `turn/start`. `thread/name/set` fails for a moment after
  `turn/start` (empty rollout), so the thread is named before its first turn, and recorded as
  owned only once naming succeeded (a thread with no turn is listed nowhere).
- History (`thread/turns/list`) needs no resume. For about a second after the first `turn/start`
  it is unreadable: `thread/resume` and `thread/turns/list` fail with `-32603 "... rollout at
  <path> is empty"`, and `thread/turns/list` can fail with `-32601 "list_turns is not supported
  yet"` (seen once; cause read in daemon source), while `thread/read` answers `idle` with no turns.
  agent-talk retries the resume once a second, at most ten times.
- `thread/resume` on a loaded thread rejoins without replaying earlier notifications, except
  pending server requests. An idle thread unloads about 60 s after its last subscriber leaves; a
  running thread keeps running and drains its queue with nobody subscribed.
- `thread/list` returns only interactive sources unless source kinds are named (`ls --all` adds
  sub-agent and unknown kinds). It has one row per rollout file, so a thread resumed into a new
  file repeats and only its newest row counts. (The state-DB-only listing avoids the repeats but
  has null `originator` and stale `updatedAt` on most rows.)

**Settings.** What `turn/start` sets (`model`, `effort`, `sandboxPolicy`, `approvalPolicy`)
becomes the thread's setting for later turns (daemon 0.161.0). Effort exists only there, as a
model-specific string; an invalid value fails the turn. Model, effort and approval policy survive
unloading; the sandbox does not (a resume after an unload restores the daemon's, codex 0.160.1),
so every resume of a full-access thread passes it again. `thread/queue/add` ignores all four
fields (0.161.0), so a send that changes settings goes through `turn/start` and is refused
(`E_PRECONDITION`) while a turn is running.

**Sending and turn end.**

- A busy `turn/start` is not refused and creates no turn: its input is folded into the active turn
  and its settings apply at once. A turn that starts between the resume and `turn/start` is
  joined; that window is not closed.
- Steer is `turn/steer {expectedTurnId}` against the newest turn; the input lands at the next
  model step.
- Queue is `thread/resume`, then `thread/queue/add`, drained FIFO as separate turns. Correlation
  is the client's `clientUserMessageId`, found as `userMessage.clientId` in the containing turn.
- An add on a not-loaded thread stays dormant until some client resumes, hence the resume first.
  On an idle loaded thread it starts a turn at once, unless the last turn was interrupted (or hit
  a budget limit): the thread then stays `Interrupted`, also across a reload, and nothing runs
  the item until `thread/queue/start` (reproduced; mechanism read in codex-rs source). So after an
  add on an idle thread `send` waits briefly for the turn to start, then calls
  `thread/queue/start`, which refuses harmlessly (`"queued submission not found: <id>"`,
  `"thread already has an active or pending turn"`) when there is nothing dormant.
- A turn ends at `turn/completed` with matching thread and turn id; the `idle` status change
  before it is not a turn end. `wait` searches history newest turn first, then observes; a turn
  an accepted receipt names is observed even while history is still empty, and a turn id nobody
  vouches for is refused.
- `-32600` covers many different refusals, so they are told apart by method and message.

**Approvals.** Every subscribed connection gets the same request id; any answer resolves it for all
(`serverRequest/resolved`). An unanswered request was still pending after 20 s, the longest window
observed. Closing a connection is not an answer; a later resume replays the request. Declined
command items appear only in live events, not in history. Only the command `decline` was
exercised; the deny answers for other request kinds follow the schema.

**Identity.** MCP `tools/call` carries `_meta.threadId` and the turn id (verified for threads
started over the daemon socket only); MCP server environments carry no `CODEX_*` variable. Shell
commands see `CODEX_THREAD_ID`. Under approval policy `never`, Codex fails MCP tool calls unless
the server entry sets `default_tools_approval_mode = "approve"`.

### 6.2 Claude Code (2.1.288)

**Interface.** Owned sessions only: one `claude -p --verbose --input-format stream-json
--output-format stream-json` process per `new` (`--session-id <uuid>`) or `send` (`--resume
<id>`), in the session's recorded cwd. agent-talk writes one user line with `"uuid": <client
message id>` and closes stdin; the process runs the turn to its `result` event and exits.

**Sessions and turns.**

- The input `uuid` becomes the prompt line's `uuid` in the transcript, so the turn id is known
  before the run.
- Transcripts `~/.claude/projects/<slug>/<id>.jsonl` are append-only. `--resume` from another cwd
  appends to the original file, so transcripts are located by id across all project folders.
- Turns group by `promptId`; user lines that only carry tool results, and lines the runtime
  injects, do not start a turn. The live branch is the `parentUuid` chain from the newest
  `last-prompt.leafUuid` (`logicalParentUuid` crosses a compaction); other branches are left out
  except by `read --raw`. Sub-agent transcripts are not read.
- The TUI ends a turn with `system/turn_duration`, or a `[Request interrupted by user]` line after
  Esc; `-p` writes no end marker. So a turn agent-talk ran is decided by its run log: the `result`
  event ends it, with the denials in `result.permission_denials`; a pid gone without one ends it
  `unknown`, or rejects the receipt when the run never took the message. Other turns end, best
  effort, at a marker, a later prompt on the live branch, or once no live process holds the
  session; a foreign `-p` turn still running stays `running`.

**Liveness and writers.** `claude agents --json --all` lists every live process, TUI and `-p`
alike, with pid and a `busy`/`idle` status; exited processes disappear. agent-talk tells its own
children by its pid records, not by the listing's `kind`. Claude has no lock between processes: a
concurrent `--resume` mid-turn writes a second branch, and the next `--resume` follows only the
newest leaf, dropping the other. Hence `E_FOREIGN_LIVE` for a live pid that is not an agent-talk
child, `E_LOCKED` for one that is, and `E_CAP_MISSING` when `claude agents` fails.

**Sending.** A second line written to a running process is absorbed into the turn or becomes the
next one at Claude's discretion, so there is no steer (`E_NO_STEER`).

**Approvals.** In `-p` Claude denies a tool that would prompt and tells the model
(`result.permission_denials`); nothing pends. agent-talk records these as `denied`, and on
sessions it started adds `--permission-prompts none`, which also tells the model not to retry.
`--permission-prompt-tool stdio` would block on a `control_request` until answered, so it is not
used.

**Settings.** `--permission-mode` and `--effort` are not kept across `--resume` (the user's
`defaultMode` applies again; claude 2.1.291), so every send passes them again.
`CLAUDE_CODE_EFFORT_LEVEL` in the environment silently overrides `--effort` (2.1.291). The model
is kept: `--resume` restores the newest assistant message's model unless `--model` is given
(2.1.292), so `send` passes `--model` only to change it, and the change also reaches the user's
TUI on its next resume.

**Identity.** Claude speaks MCP 2026-07-28 without `initialize`; `tools/call` `_meta` has no
session id. Claude sets `CLAUDE_CODE_SESSION_ID` in each MCP server's environment to the spawning
process's session, overriding an inherited value (verified with `--mcp-config` only); shell
commands inherit the variable unchanged.

### 6.3 OpenCode (2.0.22)

**Interface.** HTTP to the user's background service (`opencode serve --service`), shared by every
OpenCode client. The registration `$XDG_STATE_HOME/opencode/service.json` (default
`~/.local/state/opencode/service.json`) holds `{url, pid, version, password}` (basic auth);
`GET /api/info` has no service id, so its pid is compared with the registration. A connect
failure while that pid is alive is `E_TRANSPORT` (a sandbox, for instance). Any request naming a
directory makes the service load that location for its lifetime, so agent-talk sends only
directories it was given. Events come over one global SSE stream without replay, and executions
continue without subscribers, so agent-talk subscribes before submitting and filters by session.

**Sessions and turns.**

- `new` creates the session with its location, model (`provider/model`, effort as the model's
  `variant`), title and permission rules, then sends the first prompt as a queue send. The
  service answers a location's first requests before its configuration is applied, without the
  user's default model and rules (2.0.23), so `new` first calls `GET /api/integration`, which
  waits for plugin activation (read in source). An unknown model or variant is accepted and the
  turn then fails.
- There is no turn id; execution events carry only the session id. The client may choose the user
  message id and attach `metadata`, both stored verbatim, so agent-talk's message id (in the
  service's ascending format) is the turn id and the queue id, and the metadata carries receipt,
  sender and depth.
- An execution end writes an `idle` row with `outcome: succeeded|failed|interrupted`, except an
  interrupt with reason `shutdown` (a message-less permission reject, the service going down): no
  `idle`, and the aborted assistant row stays last. Read in source, not observed: a restart
  resumes such a turn behind a `synthetic` row with `metadata.notice: restart`.
- `GET /api/session/active` is the only running indicator; a session waiting on a permission still
  counts as running.
- A turn ends at the first `idle` after its user message (status = its outcome), at an aborted
  assistant row followed directly by a user row, or at an aborted row still last when the session
  is no longer active on a second read (`interrupted`). Anything else after an aborted row (its
  `idle`, a restart notice, more assistant rows) belongs to the same turn. History is re-read at
  each execution end. The reply is the message `read` marks `final`: the last text of the last
  assistant row that finished with `stop`; without one, the last assistant text.
- History pages by opaque cursor, in delivery order. A turn lookup searches a bounded number of
  pages back (40); older messages report as absent.

**Sending.** Busy prompts never fail, and the default delivery is `steer`, so agent-talk always
sets it. Steer joins the running execution at the next step boundary together with every other
pending steer item, possibly answered in one reply; queue runs after the current reply in the
same execution; on an idle session either starts an execution. Idle steer is refused
(`E_PRECONDITION`) after a check of the active list, which can race an execution that is just
starting. Queue items pending at a user interrupt stay dormant until another execution; after a
`shutdown` interrupt they run at once in a successor. One reply can cover several messages.

**Settings.** The session stores its model, variant and rules, so `send --model`, `--effort` and
`--full-access` change the session itself (`POST /api/session/{id}/model`, `PATCH` of its
permissions), before the message is recorded: a refused change is a plain error without a
receipt. The service reads the session's values at each step, so a change also covers the rest
of an execution already running and the prompts queued behind it, and the user's TUI follows it
(2.0.23). A changed toolset writes a `system` row into history and the model's context (about
8.7 KB for allow-all), and a model that was refused a tool earlier can go on treating it as
missing (2.0.23).

**Approvals.** `permission.asked` goes to every subscriber and is not replayed, so agent-talk also
lists pending requests. An unanswered request blocks with no timeout. `reject` with a `message`
hands the text to the model and the execution continues; a message-less `reject` interrupts it
(`shutdown`); any `reject` rejects every pending request of the session, so a deny that finds its
request gone records `resolved`. `always` would save a project-wide rule.

A request is answered by the last rule matching action and resource in the agent's ruleset
followed by the session's, and asks when none matches; the agent's ruleset already holds the
user's configuration (read in source; verified live on 2.0.23). A session agent-talk starts
therefore gets session rules at creation, so that the service itself refuses what would wait for
an answer:

- full access: one rule allowing every action on every resource, which overrides the user's
  rules;
- otherwise the default agent's rules from its first `ask` on, in order, with every `ask` turned
  into `deny`. Allowed and denied requests keep their answer; one that would ask is refused with
  nothing pending. The rules before the first `ask` are left out because sub-sessions of the
  subagent tool inherit session rules, and a full copy (which starts with `* * allow`) overrode a
  sub-agent's own denials;
- in both cases the question tool is denied: nobody answers it in these sessions.

The model sees a tool denied for every resource as absent from its toolset, and a denied call as
a tool error; the turn continues (2.0.23). Requests that still reach agent-talk (a plugin hook can
turn an answer into `ask`) get `reject` with a message, and only once agent-talk's own message was
delivered (earlier requests belong to the execution ahead of it). A TUI with
`session.permissions: "autoaccept"` answers `once` to every pending request of the session it
displays and its sub-sessions (read in source, 2.0.23), so while such a TUI shows a session
agent-talk started, its requests are approved.

**Identity.** Shell commands get `OPENCODE_SESSION_ID`. MCP `tools/call` carries
`_meta["ai.opencode/sessionID"]` (read in source and docs, never observed live; the docs say it
can be absent). One MCP server process serves every session of a location, so its environment
does not name the caller.

### 6.4 Grok CLI (1.0.46)

**Interface.** ACP (line-delimited JSON-RPC over stdio) to a `grok agent ... stdio` child. When the
leader socket accepts a connection, the child is `grok agent --leader stdio`, a proxy into the
shared leader, where sessions behave like Codex threads in the daemon. Otherwise it is `grok agent
--no-leader stdio`, an in-process agent that holds the session only while the command runs
(direct mode).

- The socket is `$GROK_LEADER_SOCKET`, else `<GROK_HOME>/leader.sock` (`GROK_HOME` defaults to
  `~/.grok`).
- `--leader` with no live socket spawns a persistent leader, so agent-talk passes it only after a
  connect succeeded. If the leader exits in between, Grok spawns a new one; agent-talk compares
  the leader's pid (from the lock file next to the socket) before and after and reports a change.
- Only a missing socket, a refused connection (a stale socket) and a path too long for a Unix
  socket select direct mode; `EPERM`/`EACCES` (a sandbox) is `E_TRANSPORT`.
- The child gets `GROK_DISABLE_AUTOUPDATER=1` (`grok agent` has no `--no-auto-update`).
- `initialize` returns the available models with their reasoning efforts and writes no session,
  so `models` reads them from a direct child.

**Store.** `<GROK_HOME>/sessions/<urlencoded cwd>/<id>/`, one format for TUI, `-p`, ACP and leader.

- `updates.jsonl` is the conversation. A turn opens with `user_message_chunk
  {_meta.promptIndex}` and closes with `turn_completed {prompt_id, stop_reason}`. Agent lines
  carry `_meta.promptId`; a steer message is a user line with `interjection: true`. A prompt
  cancelled within milliseconds gets a `turn_completed` and no user line.
- `summary.json` has id, cwd, title and `session_kind` (`headless` for `-p`).
- `<GROK_HOME>/active_sessions.json` lists live TUIs with pid; `kill -9` leaves a row, so pids are
  checked. `-p`, ACP and leader-only sessions never appear.

**Writers.** Grok has no lock between processes: a second writer marks the running turn
`interrupted` on disk, both keep appending (a prompt can get two `turn_completed`; the last
counts), and their memories diverge. Inside one leader all clients share one in-memory session:
`session/load` attaches live, a busy `session/prompt` queues a separate turn, `_x.ai/interject`
joins the running turn, events fan out to every client, and a turn survives its client's
disconnect. A TUI started with `--leader` is joinable the same way. The only holder markers are
`active_sessions.json` and the leader's list, so a foreign `-p` or direct ACP writer is
invisible. agent-talk refuses `E_FOREIGN_LIVE` when `active_sessions.json` names a live pid and no
leader lists the session `resident`, and `E_LOCKED` for its own direct child still running. It
takes its own lock only in direct mode, since clients of one leader do not exclude each other.

**Turns and sending.**

- `new` is `session/new`, effort and name set on the session, then `session/prompt` with the
  client's `_meta.promptId`, which Grok persists as `turn_completed.prompt_id`: the turn id.
- Model and effort are set with `session/set_config_option` (a leader proxy ignores `grok agent
  --reasoning-effort`; 1.0.46). Grok writes the value to `summary.json` at once, and every later
  load, by any process or client, runs with it, so later sends pass neither again; through a
  leader a change applies to the next prompt of every attached client. A refused value fails with
  `E_PRECONDITION` before anything is recorded; on `new` the empty session stays on disk and the
  error names it.
- Queue: `session/load` plus `session/prompt`, through a leader only when its list shows the
  session `resident` (a listed but dormant session may be held in-process by a TUI). The list's
  `activity` and `resident` describe the answering process, so they mean something only through a
  leader.
- Acceptance is the first `_x.ai/queue/changed` listing the prompt, then `runningPromptId` naming
  it. The `session/prompt` response resolves at turn end, possibly before `turn_completed` is
  persisted, so the reply comes from the prompt's live message chunks. A permission cancellation
  is an accepted prompt whose turn ended `cancelled`.
- Steer: `_x.ai/interject` on a session `working` in a live leader (`E_NO_STEER` without one,
  `E_PRECONDITION` when idle). It has no client id and no expected-turn check: the ack is
  acceptance, the turn comes from `_x.ai/session/interjection`, and if the turn ended meanwhile
  Grok starts a fallback turn that the receipt then names.
- In direct mode the turn dies with the child (closing stdin ends it within about two seconds,
  with no `turn_completed`), and a `session/cancel` sent before the prompt is running is ignored.
  So the command runs until the turn ends even without `--wait`; when a deadline or signal ends
  it, agent-talk waits briefly for the prompt to run, cancels it and waits for the `cancelled`
  response. Through a leader the turn continues after the command.
- `wait` on a turn agent-talk is not observing polls `updates.jsonl` for its `turn_completed`; a
  direct child that exited without one leaves the turn `interrupted` or `unknown`.

**Approvals.** The user's `permission_mode` applies to ACP sessions, and agent-talk keeps it. On
sessions it started agent-talk answers `reject_once` to its prompt's requests, matched by tool
call; `reject_once` ends the turn `cancelled`. Full access sends `_meta.yoloMode: true` on
`session/new` and on a direct-mode `session/load`; it holds for that process only and is never
stored, so every direct-mode send passes it again. It has no effect on a session resident in a
leader, so `send --full-access` there is refused (`E_UNSUPPORTED`). `session/request_permission`
goes to every attached client with one id, and the first answer wins. An unanswered request was
still pending after 25 s. Which commands prompt with yolo off varied between runs on 1.0.46 and
later runs provoked none, so the deny and observe paths rest on frames captured earlier, replayed
by unit tests. Grok has no session-level setting that stops it asking, so a request a leader-held
turn raises after the command stopped observing waits for another client (§8).

**Identity.** `GROK_SESSION_ID` is set in shell commands and in each MCP server's environment (one
server per session); `tools/call` `_meta` carries no session. Grok also loads MCP servers from
`~/.claude.json`, so a Grok session starts `agent-talk mcp` if the user registered it there.

### 6.5 Antigravity CLI (agy 1.2.16 and 1.2.17)

agy updates itself in the background (1.2.16 became 1.2.17 between two runs), so behaviour can
change under an installation. Facts hold for both versions unless noted; the database layout and
the TUI overwrite cases were seen on 1.2.16 only.

**Interface.** One `agy -p "" --input-format stream-json --output-format stream-json` process per
`new`/`send` (`--conversation <id>` to resume), in the conversation's cwd (there is no cwd flag);
one user line on stdin, then stdin closed. Events: `init {conversation_id}`, `step_update`,
`result` (turn end). Every agy process, TUI or `-p`, embeds its own language server behind a
per-process token, so there is no shared daemon and no way to join a conversation another process
holds.

**Store** (`~/.gemini/antigravity-cli/`).

- `conversations/<id>.db`: SQLite, one protobuf blob per step, authoritative.
- `brain/<id>/.system_generated/logs/transcript.jsonl`: one object per step. Lossy: steps that
  never ran are absent, a tool step written `RUNNING` is never updated, long content is
  truncated, two writers leave duplicate indices. User text is wrapped in `<USER_REQUEST>` plus
  metadata blocks. `read` and foreign turns use it.
- `conversation_summaries.db`: title, workspaces (the first is the cwd), run status and
  modification time. After the last agy process exits, a read-only open fails (its WAL files are
  gone), so agent-talk falls back to `immutable=1`, exact while no writer is open.
- `presence/<id>.lock`: flock held by the process that has the conversation open (`-p` from
  startup, a TUI from its first prompt, moved on `/new`, released on exit or `kill -9`). The lock
  does not name its holder.
- TUI and `-p` conversations are indistinguishable. The CLI has no list, rename or delete
  command.

**Runs and turns.**

- A `-p` process emits `init` and takes the presence lock at startup (about 7 s), before reading
  stdin. `new` records the intent once `init` names the conversation, so a deadline before `init`
  leaves no receipt and possibly an empty conversation.
- `--conversation <unknown id>` silently starts a new conversation, so `send` checks that the
  conversation exists and that `init` returns the same id.
- Nothing persists across runs: a resumed conversation forgets its model and effort (and falls
  back to the user's tool permissions), so every run passes the session's settings again (§4).
  `--model <alias>` without `--effort` fails before `init`. `agy models` prints one
  `<alias>-<effort>` line per combination; `models` folds them into the alias.
- The stream has no client message id and no turn id. The `user_input` step update is the first
  proof the message was taken, and its step index is the turn id; `wait --receipt` recovers it,
  the `result` and the denials from the run log (without the log an unknown receipt stays
  unknown; no text matching).
- A run that took the message and exited without `result` is `failed` when the sending command
  saw an unsuccessful exit status, as for Claude and pi, and `unknown` otherwise.
- `result.response` joins every response of the turn, commentary included, so the reply is the
  last response step's text. On a resumed conversation `result` reports conversation totals.
- A line written during a turn becomes the next turn: queue only (`E_NO_STEER`).
- The summary status is `RUNNING` during a turn and `IDLE` after, but a killed run left either
  (1.2.17), a run waiting on a backgrounded command is `IDLE`, and Esc in the TUI stores the
  partial reply as done. So the status is no evidence of liveness; the presence lock is. A foreign
  turn ends at the next user input, or at a final response without tool calls once the lock is
  free and the status is not `RUNNING`; short of that it is `running` or `unknown` with a `basis`.
  In `ls` a conversation is `running` while an agent-talk child runs or the lock is held with
  status `RUNNING`, `unknown` when the lock is free but the status says `RUNNING`, else `idle`.

**Writers.** agy ignores its own presence lock. A second `-p --conversation X` runs while another
process holds X; both write by step index, the database keeps one process's rows and the
transcript both. A TUI's next prompt overwrites steps a headless run appended, and a TUI that
switched away with `/new` keeps X in memory and overwrites on `/resume`; nothing on disk reveals
that cache. agent-talk takes its own lock, then refuses `E_FOREIGN_LIVE` while the presence lock is
held by anything but a live agent-talk child and `E_LOCKED` for such a child.

**Approvals.** Headless agy denies every tool not allow-listed in the user's `settings.json` and
ends the turn (`denied_actions` in `result`, naming only the action; exit 0). Nothing pends, and
denials are recorded as `denied`. A denied step is absent from the transcript on 1.2.16 and
written as an error step on 1.2.17. Full access passes `--dangerously-skip-permissions` on every
run. `--mode plan` and `--sandbox` change nothing in headless runs.

**Identity.** MCP `tools/call` carries `_meta["antigravity.google/conversation_id"]`; the server's
environment has no `ANTIGRAVITY_*` variable. Shell commands get `ANTIGRAVITY_CONVERSATION_ID`. MCP
servers are configured globally only.

### 6.6 pi (1.0.3)

**Interface.** Owned processes only: one `pi --mode json` process per `new` (`--session-id
<uuid>`) or `send` (`--session <file>`), the prompt on its stdin. pi reads piped stdin to EOF as
the prompt, so the text never meets argument parsing (`@file`, options). stdout is JSONL events,
written to a run log; the process runs the turn to `agent_settled` and exits. `--mode rpc` is not
used: closing its stdin or SIGTERM aborts the running turn without persisting the partial reply,
and what it adds (steer, abort) is reachable only from the process that holds its stdin, never
from a later command.

**Sessions and turns.**

- Session files are `<agent dir>/sessions/--<cwd>--/<timestamp>_<id>.jsonl` (agent dir
  `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`; the cwd with separators and `:` replaced by `-`).
  The timestamp has no `_` and an id may contain one, so the id is everything after the first `_`.
  `--session <path>` opens the file in place and runs in the directory its header records.
  `--session <id>` is not used: an id found only in another project's directory makes pi ask on
  stdin whether to fork, and json mode would read the prompt as the answer. Ids are unique per
  directory only; a handle whose id exists in two directories is refused. A custom session
  directory is a flat layout agent-talk does not read (§8).
- Entries form a tree (`id`, `parentId`); on load the leaf is the file's last entry, and nothing
  re-reads the file while a process runs. The live branch is the newest entry's ancestry; `read`
  and foreign `wait` follow it.
- The turn id is the user message's entry id. pi appends the entry before it writes the user
  `message_end` event, so the receipt is accepted at that event and the entry found by its
  timestamp and text.
- A resumed session takes the model of its newest model change or assistant message, and the
  thinking level of its newest thinking-level entry, which only a new session writes. `send`
  therefore passes `--model` only to change it, and `--thinking` on every run. The level pi records
  is the model's own mapping of the one asked (1.0.3).
- An own turn ends at `agent_settled`, its status from the newest assistant message's
  `stopReason`. While the process lives, `wait` reads only its run log, because the file may hold
  a failed message pi is about to retry. Once it is gone without `agent_settled`, a run that never
  took the message is `rejected` with pi's stderr, and one that did is read from the file, best
  effort, like any other turn: it ends at the first assistant entry on the live branch without
  tool calls (skipping one a later context edit hides, which is how pi's auto-retry leaves a
  failed message), or at a later user message. Steer and follow-up messages are plain user
  entries, so they read as prompts of their own. No `duration_ms`: pi stamps an assistant message
  when its request starts, not when the reply ends.
- An unknown `--model` exits before any file exists and the receipt is `rejected` with pi's
  stderr; an invalid `--thinking` is only a warning.

**Writers.** pi has no lock between processes, no marker of a live process and no re-read: a TUI
holding the session never sees entries another process appends, its next message hangs off its
own leaf (verified in a herdr pane), and the next load follows the file's last entry. agent-talk
refuses only its own running child (`E_LOCKED`). A session open in a TUI cannot be told from an
idle one, so a send to it is not refused, and the conversation branches (§8). `ls` reports
`loaded: yes` for an own run and `unknown` otherwise.

**Approvals.** None. Tools run with the process's rights, and extension dialogs resolve to their
defaults in json mode without blocking. `--full-access` changes nothing and is recorded as given.

`models` reads the padded table of `pi --list-models` as `provider/model`, with pi's thinking
levels as efforts for reasoning models.

**Identity.** pi sets `PI_SESSION_ID` for each shell command of its `bash` tool. MCP servers get
pi's own environment with no session variable, and `tools/call` `_meta` carries no session, so a
message a pi session sends through `agent-talk mcp` has no sender unless the server is pinned
with `--caller`.

## 7. Key decisions

- **Rust, single binary.** Agents call the CLI many times per session; startup time and one
  artifact per platform matter.
- **No agent-talk daemon.** The agents already run the shared processes; another daemon would add
  install, pid and version-skew problems for state that fits in SQLite. Long-lived state, if ever
  needed, goes into the per-session `agent-talk mcp` process.
- **The user configures the MCP server globally.** agent-talk never injects itself into a session
  or edits agent configuration, even where the agent's API allows per-thread servers.
- **Never approve.** Decline or reject is the strongest answer; `once`, `always` and `accept` are
  never sent.
- **Sessions agent-talk did not start are the user's.** agent-talk never answers their approval
  requests and never changes their settings; declining would change a turn the user's TUI is
  attached to.
- **Permissions: the agent's own, or full access.** Restricted levels (read-only, workspace write)
  were tested on 2026-10-06 and left out: only Claude could enforce one with web tools allowed,
  Codex loses the sandbox when a thread unloads and the queue path cannot carry it, OpenCode's
  shell bypasses its edit rules, and Grok and Antigravity have no per-session control.
- **Settings change only where agent-talk started the session.** `send --model/--effort/
  --full-access` change the session as the agent's own client would, and the agent decides what
  stays (§4). On every agent but Antigravity a model given for one turn stays with the session
  (probes of 2026-10-07), so there is no per-turn override to offer on the user's sessions.
- **Delete nothing.** No session, conversation or agent file is ever deleted, on any agent.
- **Agent multi-client processes are joined, never started.** Codex daemon, OpenCode service,
  Grok leader: agent-talk connects when they are live and takes the agent's single-process path
  otherwise; starting and stopping them is left to the user. Grok's `--leader` would spawn a
  leader, so it is passed only after the socket answered (§6.4).
- **Two-writer agents get agent-talk's lock plus the agent's liveness marker.** Claude, Grok
  without a leader and Antigravity have no lock between processes, and a second writer forks or
  overwrites the conversation; agent-talk refuses to write while the agent's marker says another
  process holds the session and locks its own children. pi has no marker at all, so only its own
  children are refused and the hazard is declared (§6.6, §8).
- **Turn ids come from the agent or from agent-talk's own client id**, never made up from text
  (§6), so a turn found later is the turn that was sent.
- **History is paged to the answer.** `read` and turn lookup follow the agent's cursor, newest
  first, until the page is full, the turn is found or history ends, because the wanted turn is
  usually recent (Codex: about 105 s oldest first on a thread of about 700 turns, 0.162.1).
- **Shared lifecycle, separate observe loops.** Receipt and approval handling lives once (§3). The
  end of a turn has a different authority on each agent: the `turn/completed` event in Codex, a
  re-read of history in OpenCode, the child and its run log in Claude, Antigravity and pi, the
  `session/prompt` response in Grok (for a steer message, the joined turn's
  `_x.ai/session/prompt_complete`). A generic event-loop driver would hide those differences
  behind mode flags.
- **OpenCode asks become denials by copying the default agent's rules from the first `ask` on**,
  not by appending a `deny` per `ask` rule, which would override a later, narrower `allow` in the
  user's configuration, and not by copying them all, which would override sub-agents' rules
  (§6.3).
- **Explicit agent options, not a generic option map.** `--model`, `--effort` and `--full-access`
  with clap help, instead of `--opt k=v` with per-adapter parsing. Values pass through
  unvalidated, because the agent owns the enum (Codex rejects an unknown effort in the turn, not
  at submission).
- **Steer is refused when idle** on every agent that would silently start a new turn instead.
- **No tight polling.** Observation uses the agents' event streams. agent-talk polls only where
  no event source exists: session files of turns it is not running (once a second), its own run
  logs (a short file tail), and Codex history in the window after a thread's first turn.

## 8. Known limitations and open items

- Connection loss mid-`wait` ends with `E_TRANSPORT` (OpenCode re-reads history once on a clean
  stream end, Codex does not); recovery is `wait --receipt`.
- MCP clients have their own tool timeout (Codex documents a 60 s default for
  [`tool_timeout_sec`](https://learn.chatgpt.com/docs/config-file/config-reference)). Claude
  Code, Grok and Antigravity `new` and `send` wait for the turn even with `wait=false`, so the
  client can time out before it receives the handle and receipt; `ls` recovers the handle, not the
  receipt. The server does not forward a client's cancellation, so the operation may go on after
  the client stopped waiting. Neither tells whether the turn completed. Client timeout and server
  shutdown behaviour are untested.
- Sender attribution cannot detect a daemon or service started from another agent's shell; the
  message is then attributed to that agent's session, and its hop depth counted from there.
- OpenCode and Grok apply a settings change before the message is submitted; when the message
  is then refused, the session keeps the change and agent-talk takes back only its own stored
  settings.
- Claude and Grok identify a recorded child by pid and an unfinished turn only; after a crash and
  pid reuse a session can stay `E_LOCKED` until the row ages out of the newest ten.
- Codex: `wait --turn` on a steered turn attaches the newest receipt on that turn (steered
  messages share the turn id). A thread written by another app-server process is refused only at
  `thread/resume`, and `ls` does not show which process holds it. Only `send` starts a dormant
  queued item; `wait --receipt` on one left dormant earlier reports it `pending`. `status` marks a
  method unavailable only when the daemon reports it missing. Unverified: how a terminal TUI
  renders a decline from agent-talk; `send` changing the model (effort and full access were
  exercised).
- Claude: `control_request` approvals would need an answerer (`--permission-prompt-tool stdio`);
  agent-talk relies on the agent's own denial. Unverified: whether `CLAUDE_CODE_SESSION_ID`
  reaches a globally registered MCP server the same way, and whether attribution through a TUI's
  MCP server names the old session after `/clear` or `/resume`; the end marker of TUI turns that
  fail mid-turn.
- OpenCode: history alone cannot tell a message-less reject from a graceful shutdown, a crash
  mid-step from a running step, a `shutdown` interrupt before the first step from steer batching,
  or a queued item delivered into a reloaded continuation from a successor execution.
  `session.execution.failed`, the restart resume and the MCP `_meta` session id were never
  observed. The rules of a session agent-talk started are a copy taken at `new`: later changes to
  the user's configuration, approvals saved with `always` and plugin permission hooks do not reach
  them, and sub-sessions inherit them (§6.3).
- Grok: a turn that continues in a leader after the sending command stopped observing waits for
  another client to answer a permission request it raises. `reject_once`, an unanswered request
  and the `session/cancel` at the deadline rest on captured frames, not re-verified. Unverified:
  whether a TUI renders live a turn another leader client ran; leader behaviour with a grok.com
  subscription login (it may open a relay).
- pi: a session open in a TUI cannot be detected, so `send` to it branches the conversation:
  the TUI shows neither the message nor its reply, its next message starts a branch beside them,
  and whichever process writes last decides the branch the next load continues, so a turn
  agent-talk sent can drop out of the session's context. Continuing agent-talk's own branch
  instead would need moving the leaf, which only extensions (`navigateTree`) can do. Turn ends of
  sessions agent-talk did not start are best effort (no marker separates a running turn from one
  whose process died, and a steer message reads as a new prompt). A custom flat session directory
  (`--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, the `sessionDir` setting) is not read: every pi
  operation but `models` is refused (`E_UNSUPPORTED`) and `status` names the directory; nobody
  has asked for it. Whether two processes racing to create one session collide is untested.
- Antigravity: a TUI that switched conversations can overwrite steps an agent-talk `send`
  appended once it resumes its cached copy; self-updates change behaviour between versions (the
  denied step already differs); a denied action is not tied to its tool step; the `immutable=1`
  fallback can read a stale page when a writer starts at that moment; `--print-timeout` expiry
  was not observed.
