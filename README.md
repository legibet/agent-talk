# agent-talk

agent-talk lets an agent talk with another agent. It can start a session, continue an
existing one, wait for a reply and read a conversation. It runs as a command-line tool
or as an MCP server, supporting Codex, Claude Code, OpenCode, Grok CLI, Antigravity CLI and pi.

agent-talk uses each agent's official interface and works on the agent's own sessions. agent-talk
runs no daemon. Each command connects to the agent, performs one operation and exits.

## Install

On macOS and Linux, install the prebuilt binary with:

```sh
curl -LsSf https://github.com/legibet/agent-talk/releases/latest/download/agent-talk-installer.sh | sh
```

The script installs `agent-talk` into `~/.local/bin`. To build it from source (requires Rust 1.89+):

```sh
cargo install agent-talk
```

## Agents

| agent           | interface                                                          | prerequisite                                           |
| --------------- | ------------------------------------------------------------------ | ------------------------------------------------------ |
| Codex           | shared app-server daemon                                           | `codex app-server daemon start` before the TUI opens   |
| Claude Code     | `claude -p`                                                        | none                                                   |
| OpenCode 2.x    | shared background service                                          | an open `opencode` client, or `opencode service start` |
| Grok CLI        | ACP to `grok agent stdio`, through the shared leader when one runs | none                                                   |
| Antigravity CLI | `agy -p`                                                           | none                                                   |
| pi              | `pi --mode json`                                                   | none                                                   |

agent-talk connects to the shared daemon, service or leader when it is running, but never starts
one itself.

## Usage

```sh
agent-talk ls --cwd .
agent-talk new claude --wait "list the failing tests"
agent-talk send codex:<thread id> --wait "what are you working on?"
agent-talk read opencode:<session id> --limit 4
agent-talk wait codex:<thread id> --receipt <receipt id>
agent-talk models opencode deepseek
agent-talk status
```

Each session is identified by a handle such as `codex:<thread id>`, which `ls` and `new` print.
Every command accepts `--json` for machine-readable output. By default, `send` queues the message
to run after the current reply. `--steer` adds the message to the running turn instead.
Steering works on Codex, on OpenCode and on Grok CLI through the leader, and it is refused when the
session is idle.

When an agent sends a message to another agent, agent-talk adds the first line
`[from <handle> via agent-talk; answer in your final response]`, so that the receiving agent can
see who sent it and answers in its final response, which the sender reads with `send --wait`,
`wait` or `read`. The handle is not verified. To keep agents from messaging each other without end,
agent-talk refuses a message more than three hops down an agent-to-agent chain with the error
`E_MAX_HOPS`.

`new --model` and `--effort` choose a session's model and reasoning effort. Without
`--full-access` the session runs under the agent's own permission settings; with it, the session
has every permission and no approval prompts. `send` takes the same three options to change them
from that message on, but only on sessions agent-talk started.

agent-talk never approves tool calls. In a session it started, nobody is there to answer approval
requests, so the agent is told not to ask, and anything that still needs approval is declined. In
a session it did not start, approval requests are left to the user. `agent-talk status` prints
what the installed CLIs and running daemons support, and [DESIGN.md](DESIGN.md) describes how
each agent is handled.

The exit code is one of:

- 0: success.
- 2: the request was refused, with a stable `E_*` error code.
- 3: the outcome is unknown. The agent accepted the message, but the command stopped waiting
  because of a timeout, Ctrl-C or SIGTERM. agent-talk does not resend the message, and
  `wait --receipt` reports what happened to it.
- 4: transport failure.

## Sessions open in a TUI or app

agent-talk can read any session. Whether it can send a message to a session that is open in the
agent's own TUI or app depends on the agent.

- Codex: yes, if the TUI runs inside the shared app-server daemon. This is the case when
  `codex app-server daemon start` ran before the TUI started and the TUI was not started with
  `--no-daemon`. The TUI then shows the message and the reply as they arrive. The ChatGPT desktop
  app and the VS Code extension each run their own app-server, so agent-talk cannot write to a
  thread that is open in either of them and refuses with `E_FOREIGN_LIVE` until that app exits.
- Claude Code: no. While a Claude Code TUI has the session open, agent-talk refuses with
  `E_FOREIGN_LIVE`, because a second writer would split the conversation. After the TUI exits,
  agent-talk can send, and the new messages appear when the session is resumed.
- OpenCode: yes. The TUI is a client of the same background service that agent-talk uses.
- Grok CLI: yes, if the TUI runs through the leader, which is off by default and is enabled with
  `--leader`. Otherwise agent-talk refuses with `E_FOREIGN_LIVE`.
- Antigravity CLI: no. While the TUI has the conversation open, agent-talk refuses with
  `E_FOREIGN_LIVE`. After the TUI switches to another conversation with `/new`, agent-talk can
  send, but the TUI still holds the old conversation in memory. If the user returns to it with
  `/resume`, the TUI overwrites the messages that agent-talk added.
- pi: yes, on a separate branch. pi records nothing about running processes, so agent-talk cannot
  tell a session open in a TUI from an idle one and does not refuse. The TUI does not show the
  message or the reply, and its next message starts a branch beside them. pi always continues the
  newest branch, so a later message from agent-talk follows the TUI's messages, and the turn
  agent-talk sent before is no longer in the session's context.

## Skill

The repository includes a skill that teaches agents to use the CLI. Install it with
[skills](https://github.com/vercel-labs/skills):

```sh
npx skills add legibet/agent-talk -g
```

## MCP

`agent-talk mcp` serves `models`, `ls`, `new`, `send`, `read` and `wait` as MCP tools over stdio.
They return the same JSON as `--json`. Register it with your agent, for example:

```sh
claude mcp add --scope user talk -- "$(command -v agent-talk)" mcp
```

## License

MIT
