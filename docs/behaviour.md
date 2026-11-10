# Behaviour

What a caller of the CLI or the MCP server can rely on, on every agent. Where an agent departs
from it or adds to it, its document in [agents/](agents/) says how. `agent-talk --help` lists the
commands and options.

## Sessions

A session is addressed by a handle, `<agent>:<id>`. Each carries observations, which gate the
operations on it:

- `history`: `visible` or `none`, from the agent's history listing or files;
- `loaded`: `yes`, `no` or `unknown`: whether a live process holds the session, as far as the
  agent shows it;
- `origin`: the agent's label for where the session came from (`codex-tui`, `headless`, ...), or
  `agent-talk` for sessions it started.

A session also has a `state` (`idle`, `running`, `waiting`, `unknown`) and is `owned` when
agent-talk started it. A session that no process holds is `idle`, since nothing can be running a
turn in it. Its state is `unknown` where the agent cannot show whether a process holds it, or
where the agent's own record says a turn is running in a session no process holds.

`ls` without `--agent` returns every agent's first page; an agent that fails is reported in
`errors` and does not hide the others. Each row shows the session's `name` and a `preview`, so a
session an agent created is recognizable without reading its history: the first line of its
first prompt without the provenance header, or the session title on OpenCode. `status` reports
which agents and operations are available. `models <agent>` lists what `--model` and `--effort`
take, read from the agent each time.

## Sending

`new` starts a session in a working directory and sends the first prompt; `send` continues an
existing one, including sessions agent-talk did not start. Both record the intent before
anything reaches the agent.

- Without `--wait`, the command returns once the agent accepted the message, and the turn goes on
  in the agent's shared process or in the agent child. Grok and Devin turns need agent-talk until
  they end (in Grok's direct mode and on Devin the turn dies with agent-talk's child; through a
  Grok leader only an observing command declines its requests), so on them the CLI observes to
  the end anyway, and the MCP server returns at acceptance and keeps the turn
  ([MCP server](#mcp-server)).
- Where a shared agent process holds the session (Codex daemon, OpenCode service, Grok leader),
  `send` queues: the message runs after the current reply. `--steer` joins the running turn
  instead, and is refused when the session is idle, because the agent would otherwise silently
  start a new turn with it. Without a shared process, `--steer` is refused with `E_NO_STEER`.
- A session another live process holds is written only when that process shares the session with
  agent-talk (a Codex TUI in the daemon, any OpenCode client, a Grok TUI in the leader).
  Otherwise the send is refused with `E_FOREIGN_LIVE`, or with `E_LOCKED` when the holder is an
  agent-talk command or child. So on the agents driven through one child per command (Claude,
  Grok in direct mode, Antigravity, pi, Devin), a send while a turn runs is refused rather than
  queued.

## Receipts

`new` and `send` write the intent, submit, and mark the receipt `accepted` with the agent's ids;
an agent refusal marks it `rejected`. When observation ends without a result (deadline, Ctrl-C or
SIGTERM, transport loss), a `pending` receipt becomes `unknown`, and the error carries the
receipt, the approvals seen so far and how far the message got:

- `pending`: queued, not running;
- `running`;
- `waiting`: an approval was unanswered when observation ended;
- `unknown`.

An `accepted` or `rejected` receipt never falls back to `unknown`, so a concurrent command's
proof is not overwritten. An `unknown` one is resolved by later proof: delivery seen in history,
the queue or a run log moves it to `accepted`; a run log showing the message was never taken
moves it to `rejected`. `wait --receipt` looks for that proof. A timeout is never treated as a
rejection, and nothing is resent automatically, since the message may already have been
delivered.

## Reading and waiting

`read` returns the newest N messages, oldest first, and `next_cursor` names the page of older
ones. N counts the messages returned, on every agent. By default a page holds `prompt` and
`final` messages, the conversation itself; `--all` adds `commentary` and `other`. Tool calls
appear as `other` messages (`[tool name] status input`), the one place normalization goes past
text; how calls are grouped into messages differs between agents. `--raw` returns the agent's own
records of the span instead. A message an agent sent through agent-talk carries its sender in
`from`; input the agent runtime injected (task notifications, system messages) has
`from.kind: runtime` where the agent marks it as such.

`wait` names one turn (`--turn`) or one receipt (`--receipt`); there is no "latest". It first
checks what is persisted, history or the run log of an agent-talk child, since the turn may be
over, then observes until the turn ends or the deadline passes. `turn.basis` says how the end of a
turn was established when it was not an end event on agent-talk's own connection.

## Approvals

Agents send an approval request to every attached client, and any one answer resolves it for all.
agent-talk never approves: decline or reject is the strongest answer it gives, and `once`,
`always` or `accept` are never sent.

- **Sessions agent-talk started** have nobody else to answer, so they must not wait on approvals.
  Where the agent allows it they never ask, and what still reaches agent-talk is declined.
- **Sessions agent-talk did not start** belong to the user. agent-talk never answers their
  requests, since an answer would change a turn the user's client is attached to; it records them
  and lets the deadline pass with `state: waiting`.

The one other answer is ACP's `cancelled`. When agent-talk ends a turn itself with
`session/cancel` (Grok in direct mode, Devin), ACP requires the client to answer every pending
permission request of that turn with `cancelled`, so agent-talk answers each one still pending,
and any that arrives later, right after the cancel. `cancelled` approves nothing: the tool does
not run and the turn ends `cancelled`.

Each request is recorded with its outcome: `pending`, `declined` (by agent-talk), `resolved` (by
someone else), `denied` (by the agent CLI itself) or `cancelled`.

## Settings

`--model`, `--effort` and `--full-access` are a session's settings. Model and effort pass through
unvalidated in the agent's own syntax, because the agent owns the set of valid values (some
refuse a bad value only when the turn runs). They are explicit options with help text rather than
a generic option map, so the CLI documents itself.

`new` sets them. `send` changes them from its message on, but only on a session agent-talk
started and never with `--steer`, which has no turn of its own; otherwise it is refused with
`E_PRECONDITION`. Changing settings is limited to owned sessions because on every agent but
Antigravity a model given for one turn stays with the session, so there is no per-turn override
to offer on the user's sessions.

agent-talk stores the settings of the sessions it started and passes again, on each send, what
the agent does not keep between turns. Where the agent keeps a change, the user's own client sees
it too. A change the agent refused is taken back from agent-talk's store. On OpenCode, Grok and
Devin a change is a separate request made before the message; if the message is then refused, the
session keeps the change.

**Full access** gives a session every permission and no approval prompts. On OpenCode it allows
what the user's rules would ask and keeps what they deny
([opencode.md](agents/opencode.md#approvals)). Without it the session runs under the agent's own
configuration, except on Devin, whose ACP interface starts every session in `accept-edits`
([devin.md](agents/devin.md#approvals)). Once given, full access is not taken back. Restricted levels (read-only, workspace write) were left out: tested in
October 2026, only Claude could enforce one with web tools allowed; Codex loses the sandbox when a
thread unloads and its queue path cannot carry one; OpenCode's shell bypasses its edit rules; and
Grok and Antigravity have no per-session control.

## Provenance and hops

A message from another agent is answered in the receiving turn's final response, which the
sender gets through `send --wait`, `wait` or `read`. An agent sender's message is delivered with
the first line `[from <handle> via agent-talk; answer in your final response]`, so the receiving
model, and a person watching the session, sees who is talking and where the answer goes. The
intent keeps the caller's text and sender, from which the header follows. This is attribution,
not authenticated identity.

Each intent records a hop depth: for an agent sender, one more than the depth of the intent that
started the caller's current turn (or of the newest non-rejected intent delivered to the caller's
session); for a person at the CLI, 0. A depth above 3 is refused with `E_MAX_HOPS`, a fixed
backstop against agents messaging each other without end.

The sender is taken, in this order, from:

1. an explicit pin: `--from` on the CLI, `--caller` on `agent-talk mcp`, or `AGENT_TALK_CALLER`;
2. in `agent-talk mcp`, the `tools/call` `_meta` of agents that send one (Codex, OpenCode,
   Antigravity), then the server's environment: `GROK_SESSION_ID`, then `CLAUDE_CODE_SESSION_ID`;
3. on the CLI, the session variables of the calling shell: `CODEX_THREAD_ID`,
   `OPENCODE_SESSION_ID`, `GROK_SESSION_ID`, `ANTIGRAVITY_CONVERSATION_ID`, `PI_SESSION_ID`, then
   `CLAUDE_CODE_SESSION_ID`;
4. otherwise `unknown`.

The order exists because environments are inherited. Claude's variable reaches every shell
command below it, so it goes last; a Codex daemon started from a Claude shell carries an unrelated
`CLAUDE_CODE_SESSION_ID`, so `_meta` goes before the environment. pi and Devin send no session in
`_meta` and set none in an MCP server's environment, so their messages through the MCP server
need `--caller`. Devin sets no session variable in shell commands either, so a CLI send from a
Devin session needs `--from`; without a pin it is `unknown`, or attributed to whichever agent's
variable Devin inherited.

## MCP server

`agent-talk mcp [--caller H]` is an ordinary stdio MCP server that the user registers once,
globally. agent-talk never writes agent configuration or injects itself into a session, even
where an agent's API allows per-session servers. It serves `models`, `ls`, `new`, `send`, `read`
and `wait` with the JSON the CLI prints under `--json`; refusals are tool errors, not protocol
errors. `new` requires `cwd`, because the server's working directory is not the caller's.

Code-mode clients call tools from a script and pass results on without the model reading them,
so every tool declares an `outputSchema` generated from the CLI's types and returns
`structuredContent` as well as text. The server speaks both the stateless 2026-07-28 protocol and
the `initialize` handshake of earlier versions. Logging goes to stderr only.

`new` and `send` with `wait=false` return once the agent accepted the message. A Grok or Devin
turn needs agent-talk until it ends ([Sending](#sending)), so the server keeps the turn a prompt
starts: the call returns the receipt at acceptance, and the server goes on observing the turn,
declining permission requests on sessions agent-talk started, until it ends. A steer message
joins a turn that a prompt started and is observed only until it joined. A client's cancellation
or tool timeout ends only the client's wait: the call runs on to its own end and never interrupts
a turn. When the client closes stdin, the server exits once its running calls and kept turns have
ended, however long that takes (a turn on the user's session may wait for the user to answer an
approval).

Only the CLI ends observation on Ctrl-C or SIGTERM. Once tokio listens for a signal, the default
action is replaced for the life of the process, so the server keeps the default actions: a
signal stops the server, its running requests and the turns it keeps, and a Grok direct-mode or
Devin turn ends with it.

## Errors

Every refusal has a stable code; the agent's `code`, `message` and `data` are kept under
`agent_error` when there is one.

| code             | meaning                                                                                       |
| ---------------- | --------------------------------------------------------------------------------------------- |
| `E_NO_DAEMON`    | the agent's shared daemon or service is not running, or its registration is stale             |
| `E_CAP_MISSING`  | a capability the operation needs is missing from the installed agent                          |
| `E_PRECONDITION` | the agent rejected the request (stale turn, idle steer, unknown session or receipt), or a settings change agent-talk does not allow |
| `E_LOCKED`       | another agent-talk command or child holds the session                                         |
| `E_FOREIGN_LIVE` | a process agent-talk cannot talk to holds the session                                         |
| `E_NO_STEER`     | the agent, or the session's current holder, cannot steer                                      |
| `E_MAX_HOPS`     | hop depth exceeded                                                                            |
| `E_TIMEOUT`      | the deadline passed before the turn ended; the outcome is unknown until `wait` reports it     |
| `E_INTERRUPTED`  | Ctrl-C or SIGTERM while observing; the outcome is unknown until `wait` reports it             |
| `E_TRANSPORT`    | the connection failed or dropped, or a live service was unreachable (a sandbox, for instance); the outcome is unknown if an intent was sent |
| `E_UNSUPPORTED`  | the option or operation does not exist on this agent or for this session                      |

CLI exit codes: 0 ok, 2 refused with a stable code, 3 the deadline or a signal ended the command
(outcome unknown), 4 transport failure.

## Limitations

- A connection lost mid-`wait` ends with `E_TRANSPORT`; recovery is `wait --receipt`.
- Sender attribution cannot detect a daemon or service started from another agent's shell. A
  message through it is attributed to that agent's session, and its hop depth counted from there.
- Claude, Grok, Antigravity and pi identify a recorded agent-talk child by pid and an unfinished
  turn. After a crash and pid reuse, a session can stay `E_LOCKED` until the row ages out of the
  newest ten.
