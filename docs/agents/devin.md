# Devin CLI

agent-talk drives local Devin sessions over ACP, one `devin acp` child per command, and reads
history from Devin's SQLite session store. Verified on Devin CLI 3000.11.92; points marked
3000.11.228 were verified on that version. Devin updates itself in the background
(`auto_update`), so behaviour can change under an installation.

## Interface

ACP (line-delimited JSON-RPC over stdio) to one `devin acp` child per `new`, `send`, `ls` and
`models`, started in the session's working directory with the child environment
([architecture.md](../architecture.md#local-state)). The child uses the CLI's stored login; no
`authenticate` call is needed.

`devin -p` is not used: it prints the final text only, names no session, ends the turn at the
first permission request, and rewrites the session's `working_directory` even when it is refused.

## Sessions and history

- One store for TUI, `-p` and ACP sessions: `sessions.db` (SQLite, WAL) in
  `~/.local/share/devin/cli/`, or `$DEVIN_SESSIONS_DIR` (read in Devin's docs, not observed),
  opened read-only and never written. Session ids are two-word names.
- The conversation is the `parent_node_id` chain of `message_nodes` from
  `sessions.main_chain_id`, read in one statement so that it is one snapshot. Devin copies the
  system prefix into a new root when it changes, so one logical message can appear in several
  chains under one `message_id`.
- `session/prompt` takes a client message id as `_meta["cognition.ai/clientMessageId"]` (a UUID;
  anything else is ignored silently), echoes it in the `user_message_chunk` and stores it with
  the user node (`metadata.extensions["chisel/client-message-id"]`). `-p` and the TUI set one too.
  A prompt's turn id is that id, else the node's `message_id`.
- `read`: a user node with `is_user_input` is a `prompt`; the assistant node that ends a turn
  (`finish_reason: stop`, no tool calls) is `final`, other assistant text `commentary`; each tool
  call is one `other` message, its status from the matching tool node. Item ids are
  `<message_id>-<phase>` and the tool call id, both stable across the copies. `--raw` gives the
  span's `chat_message` records.
- In the chain a turn ends at the first later `final` node, or `interrupted` at a tool node
  Devin's cancel marked `Canceled`. A prompt that comes before either leaves the earlier turn
  `unknown`: the process that took it died, or Devin answered both together (a prompt sent on the
  holding connection mid-turn is inserted after the running tool result), and the chain does not
  tell which.
- `new` is `session/new`, the settings, then the prompt. Devin writes the session row only at the
  first prompt, so a refused setting leaves just a lock file, and the error says so; the session
  is recorded as owned at `session/new`. `--name` is `_cognition.ai/session/rename` once the
  first turn is over, because the title Devin writes after the first prompt overrides an earlier
  rename; a failed rename is a note on the turn, not a failure.
- `ls` is `session/list`, followed through `nextCursor`, newest `updatedAt` first; `name` is the
  title, `preview` the chain's first prompt. `origin` is `devin` for every session agent-talk did
  not start: TUI, `-p` and ACP sessions look the same.

## Writers and liveness

A process holds `session_locks/<id>.lock` from opening a session until it exits, idle or not, and
a second opener is refused: `session/load` fails `-32015` with
`data["cognition.ai/errorKind"] = "session_locked"` and `lockHolderPid`, before anything is
written. agent-talk maps it to `E_LOCKED` when the pid is a child it recorded for the session,
else `E_FOREIGN_LIVE`, keeping the agent's error. Since Devin enforces this lock, agent-talk takes
none of its own. Nor does it probe the lock file: a probe takes Devin's lock for an instant, and a
Devin process opening the session at that moment would fail.

`session/list` reports the holder in `_meta`: `cognition.ai/isLocked` and
`cognition.ai/lockHolderPid`, which is `null` once the holder exited (3000.11.228) while the lock
file keeps its pid. That, not a recorded pid being alive, tells `ls` and `wait` whether
agent-talk's child still holds the session, because pids are reused. In `ls` a session is
`running` while the listing names a child agent-talk recorded for it as the holder and that
child's turn has no end in the chain; otherwise `unknown` with `loaded: yes` while locked (an
idle TUI holds the lock too), `idle` when the listing says it is not locked, and `unknown` when
it does not say.

## Sending and turn end

- `send` checks the session row and its directory, then the mode of a session agent-talk did not
  start, before and after `session/load` ([Approvals](#approvals)); the load's history replay is
  discarded. It then applies a settings change, records the intent and the child's pid, and
  prompts.
- Devin admits one process per session, so there is no queue: a send while a turn runs is refused
  by the lock. A second `session/prompt` on the holding connection would join the running turn,
  but no later command can reach that connection, so `--steer` is refused (`E_NO_STEER`).
- The echoed `user_message_chunk` is the acceptance. The `session/prompt` response
  (`stopReason: end_turn | cancelled`) ends the turn; the reply is the agent text after the last
  tool call. An error response before acceptance rejects the receipt, one after it fails the
  turn; a connection lost after acceptance leaves the outcome unknown.
- Closing the child's stdin or SIGTERM ends it at once; the lock is released and the running turn
  dies, leaving only its user node. So a command observes its turn to the end even without
  `--wait`, and over MCP the server keeps it ([behaviour.md](../behaviour.md#mcp-server)). At a
  deadline or signal agent-talk ends the turn with `session/cancel`, then answers pending
  permission requests `cancelled`; the response comes `cancelled` at once. A cancel during a tool
  call, or while its permission request waits, leaves the assistant node and a `Canceled` tool
  node, which later readers see; one before the first step leaves only the user node.
- `wait` on a turn no command is observing polls the db once a second until the turn ends, the
  deadline passes, or something proves nothing will write the end. For a receipt agent-talk ran,
  that is its recorded child gone, or not holding the session as far as `session/list` says
  (its pid may have been reused): `interrupted`, or `unknown` when its user node is missing.
  Otherwise it is `session/list` saying explicitly that no process holds the session: `unknown`,
  since a killed turn and one that ended without a final reply (a `-p` run after a rejected
  permission) leave the same chain. The chain is read once more after the proof, in case the
  writer finished in between. Each `session/list` starts a `devin acp`, so it is asked at most
  every five seconds.

## Approvals

ACP sessions start in `accept-edits` whatever the user's default mode, `DEVIN_PERMISSION_MODE` or
configuration (`devin acp` takes no `--permission-mode`): workspace edits run, other commands and
MCP calls ask, and the user's `permissions.allow` rules apply. Sessions agent-talk starts
therefore run in `accept-edits` rather than the user's default.

ACP offers no `normal` mode, the CLI's default: a session stored in it loads as `accept-edits`,
and the `devin acp` process stores that when it exits, even without a prompt (3000.12.43; on
3000.11.228 only a prompt stored it). So `send` refuses a `normal` session agent-talk did not
start before `session/load`. Any other session it did not start is sent to only when the mode
`session/load` reports equals its stored `agent_mode`, read again after the load (the lock then
excludes every other writer). Both refusals are `E_UNSUPPORTED`, before anything is recorded.
The first check is not atomic with the load: a session that another process turns `normal` in
between is refused after the load, and the child's exit then stores `accept-edits`. `read` and
`wait` work on every session.

On sessions agent-talk started it answers `session/request_permission` with `reject_once` (Devin
offers no reject-always): the tool fails, the model goes on and the turn ends `end_turn`. On the
user's sessions requests are recorded and left pending ([behaviour.md](../behaviour.md#approvals)).

When agent-talk cancels a turn, the `cancelled` answers follow the `session/cancel`, because
Devin reads a `cancelled` answer that comes first as a skip ("User skipped this tool call") and
the model goes on. After the cancel the turn ends the same way with or without the answer (a
`Canceled` tool node, the response `cancelled`); left unanswered, the request is dropped with a
warning in Devin's log, and an answer after the response is accepted silently (3000.11.228).

Full access is `session/set_mode {modeId: "bypass"}`, which Devin stores with the session and
every later load keeps, including the user's TUI.

## Settings

`session/new`, `session/load` and `session/set_config_option` return the current mode and
`configOptions`, so a value is checked when it is set, without a prompt.

- Model is `set_config_option model`, which accepts exactly the values its options list (one
  variant id per family, such as `swe-2-high`) and refuses anything else (`-32602`,
  `E_PRECONDITION`). `devin acp --model` is not used: it applies to `session/new` only, matches
  names loosely and ignores an unknown one.
- Effort is `thought_level` (`medium|high|max` for swe-2), set after the model, since a model
  change resets it to the family's default; after a model change agent-talk applies the effort
  the session's settings hold.
- Devin stores the resulting variant (`swe-2-medium`) and keeps model, effort and mode across
  loads, so `send` applies only a change. A refused value fails the send before anything is
  recorded; a value applied before it stays with the session.
- `models` lists the model options of a `session/new` in a temporary directory, with no prompt
  (no session row, one lock file): only those options name the ids `model` accepts, which
  `devin models list` does not. Efforts are empty, since `thought_level` lists them only for a
  session's current model.

## Caller identity

Devin sets no session variable in shell commands or MCP server environments and sends no session
in `tools/call` `_meta`. It imports MCP servers from other agents' configuration
(`~/.claude.json`), so every Devin session starts `agent-talk mcp` when the user registered it
there.

## Limitations

- A Devin caller is `unknown`, or attributed to the agent whose session variable Devin inherited
  (a Devin started from a Claude shell carries `CLAUDE_CODE_SESSION_ID`), unless pinned with
  `--from` or `--caller`.
- Sessions agent-talk starts run in `accept-edits`, not the user's default mode, and a
  `normal`-mode session agent-talk did not start is refused for `send`, until Devin offers
  `normal` over ACP.
- A turn killed with its process leaves no marker, so a later reader cannot tell it from a running
  one without the lock, nor from one that ended without a final reply, nor a prompt joined
  mid-turn from one whose process died.
- A pending approval on the user's session cannot be answered by anyone while agent-talk's child
  holds the lock. `send --wait` waits for its deadline and then cancels the turn; `send` without
  `--wait` on the CLI observes until it is interrupted; an MCP call with `wait=false` keeps the
  turn until the server exits.
- Each `devin acp` run (also for `ls`, `models` and `wait`'s holder checks) leaves a log file
  under `~/.local/share/devin/cli/logs/`; `models` also leaves a lock file.
- Unverified: errors from the model service over ACP, `session/close`, whether a TUI shows a
  turn another process ran once it reopens the session, and whether a load also changes the
  stored mode of a session in a mode other than `normal` that ACP opens differently.
