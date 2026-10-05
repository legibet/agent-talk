# agent-talk

agent-talk lets an agent send a message to another agent's session and read the reply, across
vendors, using only each vendor's official non-interactive interfaces. It works as a command-line
tool and as an MCP server, and it supports Codex, Claude Code, OpenCode, Grok CLI and Antigravity
CLI.

agent-talk does not run a daemon of its own. Each command connects to the vendor, performs one
operation and exits. The session it reaches is the vendor's own session, with its full history.

## Install

```sh
cargo install --git https://github.com/legibet/agent-talk
```

Requires Rust 1.89 or later.

## Vendors

| vendor          | interface                                                          | prerequisite                                           |
| --------------- | ------------------------------------------------------------------ | ------------------------------------------------------ |
| Codex           | shared app-server daemon                                           | `codex app-server daemon start` before the TUI opens   |
| Claude Code     | `claude -p`                                                        | none                                                   |
| OpenCode        | shared background service                                          | an open `opencode` client, or `opencode service start` |
| Grok CLI        | ACP to `grok agent stdio`, through the shared leader when one runs | none                                                   |
| Antigravity CLI | `agy -p`                                                           | none                                                   |

agent-talk connects to the shared daemon, service or leader when it is running, but never starts
one itself.

## Usage

```sh
agent-talk ls --cwd .
agent-talk new claude --cwd . --wait "list the failing tests"
agent-talk send codex:<thread id> --wait "what are you working on?"
agent-talk read opencode:<session id> --tail 4
agent-talk wait codex:<thread id> --receipt <receipt id>
agent-talk caps
```

Each session is identified by a handle such as `codex:<thread id>`, which `ls` and `new` print.
Every command accepts `--json` for machine-readable output. By default, `send` queues the message
to run after the current reply. `--mode steer` adds the message to the running turn instead, and
is refused when the session is idle.

When an agent sends a message to another agent, agent-talk adds the first line
`[from <handle> via agent-talk]` so that the receiving agent can see who sent it. The handle is
not verified. To prevent agents from forwarding messages to each other indefinitely, agent-talk
refuses a send whose reply chain is longer than `--max-hops` (3 by default) with the error
`E_MAX_HOPS`.

The exit code is one of:

- 0: success.
- 2: the request was refused, with a stable `E_*` error code.
- 3: the outcome is unknown. The vendor accepted the message, but the command stopped waiting
  because of a timeout or Ctrl-C. agent-talk does not resend the message, and `wait --receipt`
  reports what happened to it.
- 4: transport failure.

## MCP

`agent-talk mcp` runs agent-talk as an MCP server. It provides the tools `ls`, `new`, `send`,
`read` and `wait`, which correspond to the CLI commands of the same name and return the same JSON
as `--json`. Register it with the absolute path of the binary.

For Claude Code, run:

```sh
claude mcp add --scope user talk -- /abs/path/agent-talk mcp
```

Grok CLI reads MCP servers from `~/.claude.json`, so this registration also applies to Grok.

For Codex, add the server to `~/.codex/config.toml`. The `default_tools_approval_mode` line is
required, because without it MCP calls fail under the approval policy `never`.

```toml
[mcp_servers.talk]
command = "/abs/path/agent-talk"
args = ["mcp"]
default_tools_approval_mode = "approve"
```

For OpenCode, add it to `~/.config/opencode/opencode.json`:

```json
{ "mcp": { "servers": { "talk": { "type": "local", "command": ["/abs/path/agent-talk", "mcp"] } } } }
```

## Limits

|                                      | Codex | Claude Code | OpenCode | Grok CLI                | Antigravity |
| ------------------------------------ | ----- | ----------- | -------- | ----------------------- | ----------- |
| send to a session open in a terminal | yes   | refused     | yes      | only through the leader | refused     |
| steer a running turn                 | yes   | no          | yes      | only through the leader | no          |

Sessions that agent-talk refuses to write to can still be read. agent-talk never approves tool
calls. With `--approvals observe` it reports pending approval requests, and with
`--approvals deny` it declines them. `agent-talk caps` prints what the installed CLIs and running
daemons support, and [DESIGN.md](DESIGN.md) describes how each vendor is handled.

## License

MIT
