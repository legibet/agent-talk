# Claude Code

One `claude -p` process per `new` or `send` runs the turn; history, other processes' turns and
listings come from Claude's transcripts and `claude agents`. Verified on Claude Code 2.1.288.
Claude Code updates itself, so behaviour can change under an installation.

## Interface

`claude -p --verbose --input-format stream-json --output-format stream-json`, with `--session-id
<uuid>` for `new` and `--resume <id>` for `send`, in the session's directory. agent-talk writes
one user line carrying its own `uuid` and closes stdin; the process runs the turn to its `result`
event and exits, writing to a run log ([architecture.md](../architecture.md#local-state)).

Claude has no model listing, so `models` returns the aliases and effort levels `claude --help`
names, plus `haiku` (accepted on 2.1.291): the one model list agent-talk carries itself.

## Sessions and history

- Transcripts are `~/.claude/projects/<folder>/<id>.jsonl`, append-only. `--resume` from another
  directory appends to the original file but runs the turn there, so transcripts are found by id
  in every folder, and `send` runs in the directory recorded at `new`, else the transcript's
  first `cwd`; when that is gone it refuses (`E_PRECONDITION`) rather than run elsewhere.
  `ls --cwd` computes Claude's folder name (the cut of long paths read in the 2.1.311 binary).
- The input `uuid` becomes the prompt line's `uuid`, so the turn id is known before the run.
- Turns group by `promptId`. User lines of tool results only, meta lines, sub-agent lines,
  local-command bookkeeping and interrupt markers start no turn; runtime input (a task
  notification) does, shown `from: runtime`.
- The live branch is the `parentUuid` chain from the newest line, or from the newest
  `last-prompt.leafUuid` when the newest line does not descend from it; `logicalParentUuid`
  crosses a compaction. Other branches appear only in `read --raw`, and `wait` on a turn there is
  refused. Sub-agent transcripts are not read.
- `ls` adds `claude agents`: `running` while an agent-talk child runs the session or the listing
  says `busy`, `idle` when it says `idle`, else `unknown`.

## Writers and liveness

Claude has no lock between processes: a `--resume` during another process's turn writes a second
branch, and the next `--resume` follows only the newest leaf, dropping the other. agent-talk
therefore takes its own lock ([architecture.md](../architecture.md#local-state)) and refuses to
send while any live process holds the session, an idle TUI included: `E_LOCKED` for an
agent-talk child, `E_FOREIGN_LIVE` otherwise, `E_CAP_MISSING` when `claude agents` fails.

`claude agents --json --all` lists every live process (TUI, `-p`, `claude --bg`) with pid and a
`busy`/`idle` status. It also lists ended background sessions, with a `state` and no pid, so only
an entry with a pid is live (2.1.302). `kind` is `interactive` for `-p` too, so agent-talk tells
its own children by its pid records. An entry whose session id or pid has an unexpected type fails
the listing, since a dropped entry could be the writer; an absent one is accepted.

## Sending and turn end

- The receipt is accepted at `system/init`, or at a `result` without one; a process that exits
  before either is rejected with its stderr.
- A turn agent-talk ran is decided by its run log: `result` ends it. A process gone without one
  leaves the turn `failed` when the sending command saw an unsuccessful exit, else `unknown`.
- Other turns are read from the transcript, best effort. The TUI ends a turn with
  `system/turn_duration`, or a `[Request interrupted by user]` line after Esc; `-p` writes no
  marker. So a turn ends at a marker, a later prompt on the live branch, or once no live process
  holds the session (`completed` only when the last assistant line stopped on `end_turn`).
  Liveness is checked before the read, so a read after every writer is gone is complete.
- No steer (`E_NO_STEER`): a line written to a running process is absorbed into the turn or
  becomes the next one at Claude's discretion. No queue: a running turn's process holds the
  session, so a send is refused as above.

## Approvals

In `-p`, Claude denies a tool that would prompt and tells the model; nothing pends. agent-talk
records the denials as `denied`, from `system/permission_denied` and `result.permission_denials`,
once per `tool_use_id`. On sessions it started it adds `--permission-prompts none`, which also
tells the model not to retry. `--permission-prompt-tool stdio` would block the turn on a
`control_request` until answered, so it is not used. Full access is `--permission-mode
bypassPermissions`.

## Settings

`--resume` restores the newest assistant message's model unless `--model` is given (2.1.292), so
`send` passes `--model` only to change it, and the user's TUI follows on its next resume. Effort
and permission mode are not kept (the user's defaults apply again, 2.1.291), so every send to a
session agent-talk started passes them again. `CLAUDE_CODE_EFFORT_LEVEL` silently overrides
`--effort` (2.1.291), so the child runs without it when an effort is passed.

## Caller identity

Every shell command below a session inherits `CLAUDE_CODE_SESSION_ID`, which is why it goes last
in the sender order ([behaviour.md](../behaviour.md#provenance-and-hops)). Claude sets it in each
MCP server's environment to the spawning session, overriding an inherited value. Claude speaks MCP
2026-07-28 without `initialize`; `tools/call` `_meta` has no session id, only
`claudecode/toolUseId`, kept as the caller's `turn`.

## Limitations

- Without full access a tool that would prompt never runs: a `control_request` would need an
  answerer, and agent-talk relies on Claude's own denial.
- Other processes' turn ends are best effort, and after `system/turn_duration` `claude agents`
  can still say `busy` while a background agent is pending (the `basis` reports both).
- Unverified: `CLAUDE_CODE_SESSION_ID` in a globally registered MCP server (verified with
  `--mcp-config` only); whether a TUI's MCP server names the old session after `/clear` or
  `/resume`; the end marker of TUI turns that fail mid-turn.
