# Antigravity CLI

agent-talk runs one `agy -p` process per `new` or `send` and reads history and liveness from
agy's files. Verified on agy 1.2.16 and 1.2.17, and the headless behaviour again on 1.5.5. agy
updates itself in the background, so behaviour can change under an installation; the database
layout and the TUI overwrite cases were seen on 1.2.16 only.

## Interface

`agy -p "" --input-format stream-json --output-format stream-json`, with `--conversation <id>`
to resume, runs in the conversation's directory: there is no cwd flag, and the process cwd
becomes the workspace. agent-talk writes one user line, closes stdin, and the process runs the
turn to its `result` event and exits; stdout is the run log
([architecture.md](../architecture.md#local-state)). Events: `init {conversation_id}`,
`step_update`, `result`.

Every agy process, TUI or `-p`, embeds its own language server, reachable only with a
per-process token its own tool shells receive. So there is no shared daemon and no way to join a
conversation another process holds.

`agy models` prints one `<alias>-<effort>` line per combination; `models` folds them into the
alias, since `--model` takes the alias and `--effort` the level.

## Sessions and history

The store is `~/.gemini/antigravity-cli/`:

- `conversations/<id>.db`: SQLite, one protobuf blob per step, authoritative.
- `brain/<id>/.system_generated/logs/transcript.jsonl`: one object per step, which `read`,
  previews and turns agent-talk did not run use. Lossy: steps that never ran are absent, a tool
  step written `RUNNING` is never updated, long content is truncated, and two writers leave
  duplicate step indices, so the read cursor is the line position. User text is wrapped in
  `<USER_REQUEST>` plus metadata blocks, which agent-talk strips.
- `conversation_summaries.db`: title, workspaces (the first is the cwd), run status and
  modification time; `ls` reads it. agy deletes its WAL files when its last process exits, after
  which a read-only open fails, so agent-talk falls back to `immutable=1`, exact while no writer
  is open.

TUI and `-p` conversations are indistinguishable (`origin: antigravity`). The CLI has no list,
rename or delete command, so `new --name` is refused (`E_UNSUPPORTED`).

`--conversation <unknown id>` silently starts a new conversation. So `send` requires the
conversation's db or summary row (`E_PRECONDITION`), and a run whose `init` names another
conversation has its receipt rejected. A `send` to a conversation agent-talk did not start runs
in the summary's first workspace and is refused when that directory is gone.

## Writers and liveness

`presence/<id>.lock` is flocked by the `-p` or TUI process that has the conversation open. A `-p`
process takes it as it emits `init` and keeps it while its stdin is open. With stdin closed, as
agent-talk runs it, it releases the lock when it writes `result`, about half a second before it
exits (1.5.5), so a send right after a turn's `result` finds the lock free. A TUI takes it at its
first prompt, moves it on `/new` and releases it on exit. `kill -9` releases it too. The lock
does not name its holder. An `agy acp` process (the ACP server agy has had since 1.5.0) takes no
lock and ignores one that is held (1.5.5).

agy ignores its own lock. A second `-p --conversation X` runs while another process holds X;
both write by step index, the database keeps one process's rows and the transcript both. A TUI's
next prompt overwrites steps a headless run appended, and a TUI that switched away with `/new`
keeps X in memory and overwrites on `/resume`. So agent-talk takes its own session lock and
refuses `E_LOCKED` while its own child still runs a turn (known from its pid records), and
`E_FOREIGN_LIVE` while the presence lock is held otherwise.

The summary status is `RUNNING` during a turn and `IDLE` after, but it proves neither. A killed
run leaves `RUNNING`. A `-p` or `acp` process resets all rows to `IDLE` when it starts, also the
row of a turn another process is running, which turns `RUNNING` again only at that turn's next
step (1.5.5). A run waiting on a backgrounded command is `IDLE`, and Esc in the TUI stores the
partial reply as done. Liveness is therefore the presence lock. In `ls` a conversation is
`running` while an agent-talk child runs its turn or the lock is held with status `RUNNING`, and
`idle` when the lock is free and the status is not `RUNNING`. Otherwise it is `unknown`: a held
lock without `RUNNING` is an idle TUI or a turn whose status was reset, and a free lock with
`RUNNING` is a killed run or a turn in an `agy acp` process.

## Sending and turn end

- A `-p` process emits `init` and takes the presence lock at startup (4 to 5 s on 1.5.5), before
  reading stdin. `new` records the intent once `init` names the conversation, so a deadline before
  `init` leaves no receipt and possibly an empty conversation.
- The stream has no client message id and no turn id. The `user_input` step update is the first
  proof the message was taken, and its step index is the turn id.
- `result` ends the turn, whatever its payload. `result.response` joins every response of the
  turn, commentary included, so the reply is the last response step's text. On a resumed
  conversation `result` reports conversation totals, so there is no `duration_ms`.
- A run that took the message and exited without `result` is `failed` when the sending command
  saw an unsuccessful exit status, else `unknown`.
- `wait --receipt` recovers acceptance, the `result` and the denials from the run log; without
  the log, a receipt that names no turn stays `unknown` (no text matching).
- A turn agent-talk did not run ends at the next user input, or at a final response without tool
  calls once the lock is free and the status is not `RUNNING`; short of that it is `running` or
  `unknown` with a `basis`.
- A line written during a turn becomes the next turn: there is no steer (`E_NO_STEER`). A send
  while a turn runs meets the lock and is refused.

## Approvals

Headless agy denies every tool not allow-listed in the user's `settings.json` and ends the turn
(`denied_actions` in `result`, naming only the action; exit 0). Nothing pends, so the
[approval policy](../behaviour.md#approvals) holds without an answer from agent-talk; denials
are recorded as `denied`. A denied step is absent from the transcript on 1.2.16 and written as an
error step on 1.2.17.

Full access passes `--dangerously-skip-permissions` on every run. `--mode plan` and `--sandbox`
change nothing in headless runs, so there is no restricted level.

## Settings

Nothing persists across runs: a resumed conversation forgets its model and effort and falls back
to the user's tool permissions. So every run of a session agent-talk started passes model, effort
and full access again; agy itself keeps no model given for one turn
([behaviour.md](../behaviour.md#settings)). Other sessions get none of them. agent-talk adds no
default effort: `--model <alias>` without `--effort` runs at agy's default effort for the model
(1.5.5; 1.2.17 refused it). An invalid model or effort fails before `init`, and agy's error is
returned.

## Caller identity

MCP `tools/call` carries `_meta["antigravity.google/conversation_id"]`; the MCP server's
environment has no `ANTIGRAVITY_*` variable. Shell commands get `ANTIGRAVITY_CONVERSATION_ID`,
and inherit the variables of whoever launched agy, so it ranks before `CLAUDE_CODE_SESSION_ID`
([behaviour.md](../behaviour.md#provenance-and-hops)). MCP servers are configured globally only.

## Limitations

- A TUI that switched conversations can overwrite steps an agent-talk `send` appended once it
  resumes its cached copy; nothing on disk reveals that copy.
- A conversation open in an `agy acp` process holds no presence lock. So `send` is not refused
  and can write over that process's steps; `ls` shows the conversation as `idle`, or `unknown`
  while its status is `RUNNING`; and `wait` on a turn that process runs can report it
  `completed` at a reply that is not its last, once another agy start has reset the status.
- A denied action is not tied to its tool step.
- A foreign turn interrupted with Esc reads as completed.
- The `immutable=1` fallback can read a stale page when a writer starts at that moment.
- Unverified: `result` statuses other than `SUCCESS` and `ERROR` (anything else is `failed`).
