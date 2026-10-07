---
name: agent-talk
description: Message other agents (Codex, Claude Code, OpenCode, Grok, Antigravity) through the agent-talk CLI. Use it to hand one a task, ask it a question or for a review, follow up in an existing session, or read what a session did.
---

# agent-talk

agent-talk sends messages to other agents' sessions on this machine and reads their replies.
The agent names are `codex`, `claude`, `opencode`, `grok` and `antigravity`. A session is
identified by a handle, `<agent>:<id>`, returned by `new` or `ls`. Every command accepts `--json`
for structured output.

## Start a session

```sh
agent-talk new <agent> "<prompt>" --cwd <directory> --full-access --wait
```

The session starts in `--cwd`, or the current directory if omitted. It does not inherit your
conversation history, so the prompt must include the task and any context it needs from this
conversation.

`--full-access` lets the session edit files, run commands and use the network without approval
prompts. Without it, the agent's own permission settings apply. Sessions created by agent-talk
run unattended: actions that require approval are denied by the agent or declined by agent-talk.

`--model <id>` and `--effort <value>` select the model and reasoning effort; omitted options use
the agent's defaults. `agent-talk models <agent> [query]` lists model IDs and their accepted effort
values. The optional query filters IDs by substring. Antigravity requires `--effort` when
`--model` is supplied.

Codex requires its app-server daemon to be running. OpenCode requires its background service,
which an open OpenCode client also provides. agent-talk connects to these services without
starting them. `agent-talk status` reports which agents and operations are available.

## Continue a session

```sh
agent-talk ls --cwd <directory> --agent <agent>
agent-talk send <handle> "<text>" --wait
```

`ls` lists matching sessions with their handles, states and message previews. Omit `--agent` to
list all five agents. `send` continues the selected session with its history, including sessions
created outside agent-talk. Sessions created by agent-talk retain their model, effort and
`--full-access` setting on subsequent sends.

On Codex, OpenCode and Grok through a live leader, `send` queues the message after the current
reply. `--steer` adds it to the running turn instead and is refused when the session is idle.

A session open in another client accepts messages when that client shares it with agent-talk: a
Codex TUI attached to the shared app-server daemon, any OpenCode client, or a Grok TUI started with
`--leader`. Other live clients, including Claude Code and Antigravity TUIs, cause `E_FOREIGN_LIVE`
until they release the session. In sessions created outside agent-talk, agent-talk leaves approval
requests for the user to answer.

Antigravity releases a session when the user switches away with `/new`, but keeps it in memory.
Returning to it with `/resume` overwrites messages that agent-talk added in the meantime.

## Wait for the result

`new --wait` and `send --wait` wait for the turn to end and print its status and reply, along with
the session handle and receipt. Turns can take minutes. Use a background job or a shell execution
timeout longer than `--timeout <seconds>`, which defaults to 600 seconds.

For parallel work, Codex and OpenCode allow `new` and `send` without `--wait`. These return once
the message is accepted, with a handle and receipt. Use that receipt to retrieve the result:

```sh
agent-talk wait <handle> --receipt <receipt-id>
```

If the turn has finished, `wait` returns its result immediately. Otherwise, it waits up to 600
seconds by default; `--timeout <seconds>` sets a different limit.

Claude Code, Grok and Antigravity run the turn to its end even without `--wait`. For parallel work
with these agents, run each command as a separate background job and keep `--wait` to include the
reply in its output.

Exit code 0 can still accompany a failed turn. The turn status reports `completed`, `failed`,
`interrupted` or `unknown`; a reply from an unfinished or failed turn may be partial. Each
`approval declined` or `approval denied` line identifies an action that did not run. On OpenCode,
one reply can cover several queued messages; `read` shows the conversation containing them.

Exit code 3 means a timeout or interrupt left the outcome unknown. Use the receipt printed with
the error to check the result with `wait`; resending may duplicate the message. If `new` did not
print a handle, `ls` identifies sessions it created with `origin=agent-talk`. Grok without a
leader cancels its turn when the command times out or is interrupted; `wait` reports how that
turn ended and does not restart it.

Exit code 2 reports a refused request with an `E_*` code and an explanation of the cause. Exit
code 4 reports a transport failure, which can occur before or after delivery. If the error
includes a receipt, use it with `wait` to check the outcome after restoring connectivity. A
sandbox can block access to an otherwise running local socket or service.

## Read session history

```sh
agent-talk read <handle>
```

`read` returns the newest prompts and final replies, oldest first. `--limit <count>` sets the
number of messages. Pass the returned cursor to `--cursor <cursor>` for the preceding page.
`--all` includes tool calls and intermediate text.
