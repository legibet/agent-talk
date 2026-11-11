# pi

agent-talk runs one `pi --mode json` process per `new` or `send` and reads pi's session files for
history, listings and the turns it is not running. pi has no shared process to join and no marker
of a live one. Verified on pi 1.5.2, except the TUI and `--mode rpc` cases, last checked on 1.0.3.

## Interface

`new` runs `pi --mode json --session-id <uuid>`, `send` runs `pi --mode json --session <file>`.
The prompt goes on stdin, which agent-talk then closes: pi reads piped stdin to EOF as the prompt,
so the text never meets argument parsing (`@file`, options). stdout is JSONL events, written to the
run log ([architecture.md](../architecture.md#local-state)); the process runs the turn to
`agent_settled` and exits, with status 0 even when the turn failed.

`--mode rpc` is not used: closing its stdin or SIGTERM aborts the running turn without persisting
the partial reply, and what it adds (steer, abort) is reachable only from the process that holds
its stdin, never from a later command.

## Sessions and history

- Files are `<agent dir>/sessions/--<cwd>--/<timestamp>_<id>.jsonl`, the agent dir being
  `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`. The timestamp has no `_` and an id may contain one,
  so the id is everything after the first `_`. Ids are unique per directory only; a handle whose
  id exists in two directories is refused (`E_PRECONDITION`).
- `--session <path>` opens the file in place and runs in the directory its header records.
  `--session <id>` is not used: an id found only in another project's directory makes pi ask on
  stdin whether to fork, and json mode would read the prompt as the answer.
- A custom session directory (`--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, the `sessionDir`
  setting) is a flat layout agent-talk does not read. While the variable or the setting names one,
  every operation but `models` is refused (`E_UNSUPPORTED`), and `status` names the directory.
- Entries form a tree (`id`, `parentId`); on load pi takes the file's last entry as the leaf. The
  live branch is the newest entry's ancestry; `read` and turns read from the file follow it, and
  `wait` refuses a turn off it (`E_PRECONDITION`). Steer and follow-up messages are plain user
  entries, so they read as prompts of their own.

## Writers and liveness

pi has no lock between processes, no marker of a live process and no re-read: a process holding a
session, a TUI included, never sees entries another process appends, its next message hangs off
its own leaf, and the next load follows the file's last entry. agent-talk takes its session lock
([architecture.md](../architecture.md#local-state)) and refuses a send while a pi process it
started still runs the session (`E_LOCKED`). A session open in a TUI cannot be told from an idle
one, so a send to it is not refused ([Limitations](#limitations)). `ls` reports `loaded: yes` while
an own run lives, else `unknown`.

## Sending and turn end

- **Turn id.** The id of the user message's entry. pi appends the entry before it writes the user
  `message_end` event, so the receipt is accepted at that event and the entry found by its
  `message.timestamp` and text.
- **No queue, no steer.** A send while an own run lives is `E_LOCKED`. `--steer` is `E_NO_STEER`:
  pi takes messages into a running turn only on that process's stdin, which agent-talk closed.
- **Own turns** end at `agent_settled`, not at an assistant message without tool calls, since an
  extension can continue a run after one. The status comes from the newest assistant message's
  `stopReason`. While the process lives, `wait` reads only its run log, because the file may hold
  a failed message pi is about to retry.
- **A process gone without `agent_settled`** that never took the message leaves the receipt
  `rejected` with pi's stderr (an unknown `--model` exits so, before any file exists). One that
  took it is read from the file; when the file does not end the turn, it is `failed` if the
  sending command saw an unsuccessful exit status, else `unknown`.
- **From the file**, best effort, a turn ends at the first assistant entry on the live branch
  without tool calls, skipping one that a later `context_edit` hides (how pi's auto-retry leaves a
  failed message), or `unknown` at a later user message.
- No `duration_ms`: pi stamps an assistant message when its request starts, not when it ends.

## Approvals

None. Tools run with the process's rights, and extension dialogs resolve to their defaults in json
mode without blocking. `--full-access` changes nothing and is recorded as given.

## Settings

- **Model** (`--model provider/model`): a resumed session takes the model of its newest model
  change or assistant message, so `send` passes `--model` only to change it, and later loads, the
  user's included, keep the change.
- **Effort** (`--thinking`): a resumed session takes the level of its newest thinking-level entry.
  pi 1.0.3 writes one only for a new session; 1.5.2 also writes one when a run's level differs
  from the branch's, so there a change stays for later loads, the user's included. On a session
  agent-talk started `send` passes the stored effort on every run, which 1.0.3 needs. The level
  pi records is the model's own mapping of the one asked. An invalid level is only a warning.
- `models` reads the table of `pi --list-models` (every model of every provider with credentials)
  as `provider/model`, with pi's thinking levels as efforts for reasoning models.

## Caller identity

pi sets `PI_SESSION_ID` for each shell command of its `bash` tool
([sender order](../behaviour.md#provenance-and-hops)). MCP servers get pi's own environment with
no session variable, and `tools/call` `_meta` carries no session, so a message a pi session sends
through `agent-talk mcp` has no sender unless the server is pinned with `--caller`.

## Limitations

- A send to a session open in a TUI branches the conversation: the TUI shows neither the message
  nor its reply, its next message starts a branch beside them, and whichever process writes last
  decides the branch the next load continues, so a turn agent-talk sent can drop out of the
  session's context. Continuing agent-talk's branch instead would need moving the leaf, which only
  extensions (`navigateTree`) can do.
- Turn ends read from the file are best effort: no marker separates a running turn from one whose
  process died, and a steer message reads as a new prompt.
- A custom flat session directory is not read; nobody has asked for it.
