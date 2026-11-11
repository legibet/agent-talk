# Grok CLI

agent-talk speaks ACP (line-delimited JSON-RPC over stdio) to one `grok agent ... stdio` child per
command: a proxy into the user's shared leader when one runs, otherwise an in-process agent
(direct mode). Verified on Grok CLI 1.0.77. Grok updates itself, so behaviour can change under an
installation.

## Interface

- When the leader socket accepts a connection, the child is `grok agent --leader stdio`, and
  sessions in the leader behave like Codex threads in the daemon. Otherwise it is `grok agent
  --no-leader stdio`, which holds the session only while the command runs.
- The socket is `$GROK_LEADER_SOCKET`, else `<GROK_HOME>/leader.sock` (`GROK_HOME` defaults to
  `~/.grok`).
- `--leader` with no live socket spawns a persistent leader, so agent-talk passes it only after a
  connect succeeded ([architecture.md](../architecture.md#processes-and-connections)). If the
  leader exits in between, Grok spawns a new one; agent-talk compares the leader's pid (in the
  lock file next to the socket) before and after and reports a change.
- Only a missing socket, a refused connection (a stale socket) and a path too long for a Unix
  socket select direct mode. `EPERM` or `EACCES` (a sandbox) is `E_TRANSPORT`, since direct mode
  beside a live leader would make a second writer.
- The child gets `GROK_DISABLE_AUTOUPDATER=1` (`grok agent` has no `--no-auto-update`).
- `initialize` returns the available models with their reasoning efforts and writes no session,
  so `models` reads them from a direct child.

## Sessions and history

The store is `<GROK_HOME>/sessions/<urlencoded cwd>/<id>/`, one format for TUI, `-p`, ACP and
leader sessions.

- `updates.jsonl` is the conversation. A turn opens with `user_message_chunk
  {_meta.promptIndex}` and closes with `turn_completed {prompt_id, stop_reason}`; agent lines
  carry `params._meta.promptId`. A prompt cancelled within milliseconds gets a `turn_completed`
  and no user line, and reads as a turn of its own.
- `session/prompt` takes the client's `_meta.promptId`, which Grok persists as
  `turn_completed.prompt_id`: agent-talk's client message id is the turn id.
- A steer message is a user line with `interjection: true` inside the running turn. Its text is
  wrapped in instructions to the model; `read` shows the original (`content._meta.displayText`).
- `summary.json` has id, cwd, title and `session_kind` (`headless` for `-p`, null or absent for
  TUI, ACP and leader sessions alike), so `ls` shows origin `headless` or `grok`.

## Writers and liveness

Grok has no lock between processes: a second writer marks the running turn `interrupted` on disk,
both keep appending (a prompt can get two `turn_completed`; the last counts), and their memories
diverge. Lines are attributed to turns by prompt id where they carry one.

Inside one leader all clients share one in-memory session: `session/load` attaches live, a busy
`session/prompt` queues a separate turn, `_x.ai/interject` joins the running turn, events fan out to
every client, and a turn survives its client's disconnect. A TUI started with `--leader`, or with
`[cli] use_leader = true` in its configuration, is joinable the same way and shows the other
clients' turns as they run. The `activity` and `resident` of `_x.ai/sessions/list` describe the
answering process, so they mean something only through a leader.

The only holder markers are the leader's list and `<GROK_HOME>/active_sessions.json`, which lists
live TUIs with pid (`kill -9` leaves a row, so pids are checked; `-p`, ACP and leader-only sessions
never appear). A TUI without a leader keeps a row for every session it has loaded: after `/new` the
old session stays loaded and listed until the TUI exits. agent-talk refuses `E_FOREIGN_LIVE` when a
live TUI row names the session and no leader lists it `resident`, and `E_LOCKED` while its own
direct child runs an unfinished turn. It takes its own lock only in direct mode, since clients of
one leader do not exclude each other ([architecture.md](../architecture.md#local-state)).

In `ls` a session is `running` while agent-talk's child runs it or the leader lists it `working`,
`waiting` when the leader lists it `needs_input` (a permission request is open), `idle` when the
leader lists it `idle`, else `unknown`.

## Sending and turn end

- `new` is `session/new`, the settings, `_x.ai/session/rename` for `--name`, then
  `session/prompt`.
- Queue is `session/load` plus `session/prompt`, through a leader only when its list shows the
  session `resident`: a listed but dormant session may be held in process by a TUI.
- Acceptance is the first `_x.ai/queue/changed` listing the prompt, then `runningPromptId` naming
  it. The `session/prompt` response ends the turn, possibly before `turn_completed` is persisted,
  so the reply comes from the prompt's live message chunks. A permission cancellation is an
  accepted prompt whose turn ended `cancelled`.
- Steer is `_x.ai/interject` on a session `working` in a live leader (`E_NO_STEER` without one,
  `E_PRECONDITION` when it is idle or waiting on a permission request). It has no client id and no
  expected-turn check: the ack is acceptance, the turn comes from `_x.ai/session/interjection`, and
  if the turn ended meanwhile Grok starts a fallback turn that the receipt then names. The steer
  message's turn ends at the joined turn's `_x.ai/session/prompt_complete`, a fallback turn at its
  `turn_completed` notification, since Grok sends no `prompt_complete` for it. A steer receipt
  accepted before the interjection was seen names no turn, and `wait --receipt` on it is refused
  (`E_PRECONDITION`, pointing at `read`), since nothing in the session store carries its id.
- In direct mode the turn dies with the child: closing stdin makes Grok cancel it and write
  `turn_completed {stop_reason: cancelled}` (on 1.0.46 it died within about two seconds with no
  `turn_completed`, and a `session/cancel` sent before the prompt was running was ignored).
  So agent-talk observes the turn to its end even without `--wait`, in the command or in the MCP
  server ([behaviour.md](../behaviour.md#mcp-server)). When a deadline or signal ends observation,
  it waits briefly for the prompt to run, cancels it, answers its pending permission requests
  `cancelled` and waits for the `cancelled` response.
- Through a leader the turn continues after the command. The command and the MCP server still
  observe it to its end without `--wait`, so requests on sessions agent-talk started are declined.
- `wait` on a turn agent-talk is not observing polls `updates.jsonl` for its `turn_completed`; a
  direct child that exited without one leaves the turn `interrupted` or `unknown`.

## Approvals

The user's `permission_mode` applies to ACP sessions, and agent-talk keeps it; Grok has no
session-level setting that stops it asking. `session/request_permission` goes to every attached
client with one id, and the first answer wins. An unanswered request was still pending after
25 s.

- On sessions agent-talk started it answers `reject_once` to its own prompt's requests, matched by
  tool call (through a leader other clients' turns raise requests too); `reject_once` ends the
  turn `cancelled`.
- After agent-talk's direct-mode cancel, each pending request is answered `cancelled`
  ([behaviour.md](../behaviour.md#approvals)). Grok then reports `interaction_resolved`, writes
  `permission_resolved {decision: cancelled}` to `events.jsonl` and ends the prompt `cancelled`
  within 10 ms. Unanswered, it resolves the request itself at the cancel, without a decision, and
  the `interaction_resolved` that follows would read as an answer from another client. A
  `cancelled` answer without a cancel ends the turn too (`PermissionCancelled`), and one after the
  turn ended is dropped silently.
- Which commands prompt with yolo off is Grok's choice. On 1.0.70, without a rule, Grok resolved
  requests `allow` itself without reaching the client, while an `ask` rule in its configuration
  (`[permission] ask = ["Bash(touch *)"]`) made it ask every time. With that rule Grok also
  asked with yolo on, for a command it could not split into simple ones (`sleep 20; echo
  $VAR`), so full access does not silence the user's `ask` rules.

## Settings

- Model and effort are set with `session/set_config_option`, never as `grok agent -m` or
  `--reasoning-effort`: an unknown `-m` silently runs the default model, and a leader proxy
  ignores `--reasoning-effort`. Grok refuses a model it does not list and an effort the session's
  current model does not offer (`-32602`), so the model goes first.
- A model change keeps the effort; a model without efforts drops it, and the next model starts at
  its default. The response lists `reasoning_effort` only while the model has efforts, so after a
  model change agent-talk stores the effort Grok reports, or none.
- Grok stores the value in `summary.json` at once, and every later load, by any process or
  client, runs with it, so later sends pass neither again; through a leader a change applies to
  the next prompt of every attached client. A refused value fails with `E_PRECONDITION` before
  anything is recorded; on `new` the empty session stays on disk and the error names it.
- **Full access** sends `_meta.yoloMode: true` on `session/new` and on a direct-mode
  `session/load`. It holds for that process only and is never stored, so every direct-mode send
  passes it again. On a session resident in a leader it would turn full access on for every
  client's turns until the session is unloaded, and `false` does not turn it off, so `send
  --full-access` there is refused (`E_UNSUPPORTED`).

## Caller identity

`GROK_SESSION_ID` is set in shell commands and in each MCP server's environment (one server per
session); `tools/call` `_meta` carries no session
([behaviour.md](../behaviour.md#provenance-and-hops)). Grok also loads MCP servers from
`~/.claude.json`, so a Grok session starts `agent-talk mcp` if the user registered it there.

## Limitations

- A foreign `-p` or direct ACP writer leaves no marker, so a send is not refused while one runs.
- A turn that continues in a leader after observation stopped waits for another client to answer
  a permission request it raises.
- A steer receipt whose outcome is unknown and that names no turn is not refused by `wait
  --receipt`: it polls the session to the deadline.
- Unverified: leader behaviour with a grok.com subscription login (it may open a relay); whether
  `/new` in a leader TUI keeps the old session resident; the effect of `GROK_DISABLE_AUTOUPDATER` on
  a stdio agent.
