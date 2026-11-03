# Codex

Each command connects to the shared app-server daemon's control socket, subscribes to a
thread, submits, observes and disconnects; the turn goes on in the daemon. Verified on codex-cli
0.160.0. The daemon updates itself and can run another version than the CLI; `status` reports
both.

## Interface

The socket `~/.codex/app-server-control/app-server-control.sock` speaks JSON-RPC over WebSocket
over AF_UNIX, with no local auth. An absent or refusing socket is `E_NO_DAEMON`; agent-talk never
starts the daemon ([architecture.md](../architecture.md#processes-and-connections)). In a sandbox
without local network access the socket fails with EPERM while the daemon is fine: `E_TRANSPORT`.
The queue methods need `experimentalApi` at `initialize` (`E_CAP_MISSING` otherwise). `-32600`
covers many unrelated refusals, so errors are told apart by method and message.

## Sessions and history

- `new` is `thread/start` with `approvalPolicy: never`, which inherits the daemon's sandbox, MCP
  servers and hooks, then `turn/start`. `thread/name/set` fails for a moment after `turn/start`
  (empty rollout), so `--name` is set before the first turn, and the thread is recorded as owned
  only once naming succeeded: a thread with no turn is listed nowhere.
- History (`thread/turns/list`) needs no resume. For about a second after the first
  `turn/start` it is unreadable: `thread/resume` and `thread/turns/list` fail with `-32603 "...
  rollout at <path> is empty"`, `thread/turns/list` also with `-32601 "list_turns is not
  supported yet"` (cause read in daemon source), and `thread/read` answers `idle` with no turns.
  agent-talk retries the resume once a second, its only polling on Codex; `read` and turn
  lookup treat the window as an empty history.
- `read` maps `agentMessage` phases (`final_answer`, `commentary`) and skips other items, so
  `read --all` shows no tool calls. Items carry no time: a prompt gets its turn's start, a reply
  its end.
- `thread/list` returns only interactive sources unless source kinds are named (`ls --all` adds
  sub-agent and unknown kinds). It has one row per rollout file, so a thread resumed into a new
  file repeats and only its newest row counts. The state-DB-only listing avoids the repeats but
  has null `originator` and stale `updatedAt` on most rows. A thread that is not loaded shows
  `state: unknown`.

## Writers and liveness

- The TUI runs inside the daemon unless started with `--no-daemon` or before the daemon. Within
  the daemon every client shares the loaded thread and gets identical events, so a turn
  agent-talk starts shows live in the user's TUI.
- `codex exec` is always standalone; the ChatGPT desktop app and the VS Code extension run their
  own app-server. Across processes Codex holds a per-thread writer lock, and `thread/resume` from
  another process fails with `-32600 "thread <id> already has an active writer"` until the holder
  exits: `E_FOREIGN_LIVE`, refused before an intent is recorded.
- "Not loaded" cannot tell a stopped thread from one running in a standalone process; agent-talk
  proceeds through the daemon.
- `thread/resume` loads a thread and rejoins a loaded one without replaying earlier
  notifications, except pending server requests. A thread unloads `thread_unload_delay_secs`
  after the later of its last subscriber leaving and its last turn ending (60 s up to 0.162, 30
  minutes on 0.163 to 0.165); a connected subscriber holds it loaded. A running thread keeps
  running and drains its queue with nobody subscribed.

## Sending and turn end

Every send resumes the thread first: an add to a thread that is not loaded stays dormant until
some client resumes, and the response's newest turn tells whether the thread is idle and which
turn a steer targets.

- **Queue** is `thread/queue/add`, drained FIFO as separate turns. Correlation is the client's
  `clientUserMessageId`, found as `userMessage.clientId` in the containing turn.
- **Dormant items.** On an idle loaded thread an add starts a turn at once, unless the last turn
  was interrupted (or hit a budget limit): the thread then stays `Interrupted`, also across a
  reload, and nothing runs the item until `thread/queue/start` (read in codex-rs source). So
  `send` waits briefly for the turn to start, then calls `thread/queue/start`, which refuses
  harmlessly (`"queued submission not found: <id>"`, `"thread already has an active or pending
  turn"`) when nothing is dormant.
- A message is `pending` only while it is known to wait in the queue: added behind a running
  turn, or found in `thread/queue/list`. After an add on an idle thread, a `turn/started` may
  belong to a dormant item queued ahead, and on a cold thread the `userMessage` that ties a turn
  to the message comes seconds after `turn/started` (hooks run first; 0.165.0); until then the
  state is `unknown`.
- **Steer** is `turn/steer {expectedTurnId}` against the newest turn; the input lands at the next
  model step. A stale turn id or an idle thread is refused (`E_PRECONDITION`).
- A busy `turn/start` is not refused and creates no turn: its input joins the active turn and its
  settings apply at once. A turn that starts between the resume and a settings `turn/start` is
  joined; that window is not closed.
- A turn ends at `turn/completed` with matching thread and turn id; the `idle` status change
  before it is not a turn end. `wait` observes a turn an accepted receipt names even while
  history is still empty, and refuses a turn id nobody vouches for.

## Approvals

Policy: [behaviour.md](../behaviour.md#approvals). Threads agent-talk started run with
`approvalPolicy: never`; a request that still arrives gets `decline` (or the request kind's
equivalent refusal), never `cancel`, which would also interrupt the turn. Only the command
`decline` was exercised; the answers for other kinds follow the schema.

Every subscribed connection gets the same request id, and any answer resolves it for all
(`serverRequest/resolved`); an unanswered one was still pending after 20 s, the longest window
observed. Closing a connection is not an answer; a later resume replays the request, so requests
are recorded once by request and item id. Declined command items appear only in live events, not
in history.

## Settings

Rules: [behaviour.md](../behaviour.md#settings). `new` puts model and sandbox on `thread/start`
and effort, which it does not take, on `turn/start`. Full access is the `danger-full-access`
sandbox. Effort is a model-specific string; an invalid value fails the turn, not the submission.

What `turn/start` sets (`model`, `effort`, `sandboxPolicy`, `approvalPolicy`) becomes the
thread's setting for later turns (0.161.0). Model, effort and approval policy survive unloading;
the sandbox does not (a resume after an unload restores the daemon's, 0.160.1), so every resume of
a full-access thread passes it again (a loaded busy thread ignores it, 0.161.0).
`thread/queue/add` ignores all four fields (0.161.0), so a send that changes settings goes
through `turn/start` and is refused (`E_PRECONDITION`) while a turn is running.

## Caller identity

MCP `tools/call` carries `_meta.threadId` and, in `_meta["x-codex-turn-metadata"]`, the thread and
turn ids (verified for threads started over the daemon socket only); MCP server environments
carry no `CODEX_*` variable. Shell commands see `CODEX_THREAD_ID`, and whatever the daemon
inherited from the shell that started it ([sender order](../behaviour.md#provenance-and-hops)).
Under approval policy `never`, Codex fails MCP tool calls unless the server entry sets
`default_tools_approval_mode = "approve"`.

## Limitations

- `wait --turn` on a steered turn attaches the newest receipt on that turn (steered messages
  share the turn id).
- A thread written by another app-server process is refused only at `thread/resume`, and `ls`
  does not show which process holds it.
- Only `send` starts a dormant queued item; `wait --receipt` on one left dormant earlier reports
  it `pending`.
- A connection lost mid-`wait` ends with `E_TRANSPORT` without a history re-read.
- `status` marks a method unavailable only when the daemon reports it missing.
- Unverified: how a terminal TUI renders a decline from agent-talk; `send` changing the model
  (effort and full access were exercised); whether `turn/start` and `turn/steer` store the
  `clientUserMessageId` as `userMessage.clientId`, which `read` needs to name the sender (checked
  for `thread/queue/add` only).
