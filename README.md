# agent-talk

agent-talk lets coding agents talk to each other. From a shell, a script or an MCP tool call, you
can list the agent sessions on your machine, start a new one, send a message to one and get its
reply. It supports Codex, Claude Code, OpenCode, Grok CLI and Antigravity CLI.

It reaches each session through the vendor's own interface (the Codex app-server, `claude -p`,
the OpenCode service, Grok's ACP agent, `agy -p`), so you talk to the same session you see in
your terminal, with its full history. There is no agent-talk daemon: each command connects, does
its job and exits.

## Install

```sh
cargo install --git https://github.com/legibet/agent-talk
```

Requires Rust 1.89 or later. agent-talk runs on macOS; it builds on Linux, but has not been
tested there against the vendors. It uses the vendor CLIs and logins you already have.

## Vendors

| vendor | reached through | needs |
|---|---|---|
| Codex | the app-server daemon | `codex app-server daemon start` before you open the TUI |
| Claude Code | `claude -p` | nothing |
| OpenCode | the OpenCode background service | an open `opencode` client, or `opencode service start` |
| Grok CLI | `grok agent stdio`, through the leader when one runs | nothing |
| Antigravity CLI | `agy -p` | nothing |

## Use

```sh
agent-talk ls --cwd .
agent-talk new claude --cwd . --wait "list the failing tests"
agent-talk send codex:<thread id> --wait "what are you working on?"
agent-talk read opencode:<session id> --tail 4
agent-talk wait codex:<thread id> --receipt <receipt id>
agent-talk caps
```

Sessions are named by handles such as `codex:<thread id>`, as `ls` and `new` print them. Every
command takes `--json`. `send` queues the message after the current reply; `--mode steer` adds it
to the running turn where the vendor supports that.

Exit codes: 0 ok, 2 refused (with a stable `E_*` code), 3 outcome unknown, 4 transport failure.
Exit 3 means the message was accepted but the command stopped waiting (timeout or Ctrl-C); the
message is not sent again, and `wait --receipt` tells you what happened to it.

## MCP

`agent-talk mcp` provides `ls`, `new`, `send`, `read` and `wait` as MCP tools, returning the same
JSON as `--json`. Register it once with the binary's absolute path (`which agent-talk`).

Claude Code:

```sh
claude mcp add --scope user talk -- /abs/path/agent-talk mcp
```

Claude asks before running MCP tools; `claude -p` needs an allow rule such as
`--allowedTools "mcp__talk__*"`. Grok reads the MCP servers in `~/.claude.json`, so this also
covers Grok.

Codex, in `~/.codex/config.toml` (without the approval mode, MCP calls fail under approval policy
`never`):

```toml
[mcp_servers.talk]
command = "/abs/path/agent-talk"
args = ["mcp"]
default_tools_approval_mode = "approve"
```

OpenCode, in `~/.config/opencode/opencode.json`:

```json
{ "mcp": { "servers": { "talk": { "type": "local", "command": ["/abs/path/agent-talk", "mcp"] } } } }
```

## Who is talking

agent-talk records who sent each message: the session the command runs in, read from the
vendor's environment variable or MCP request metadata, or the handle given by `--from` or
`AGENT_TALK_CALLER`. When the sender is an agent, the message arrives prefixed with
`[from <handle> via agent-talk]`, so the receiving agent knows who is asking. This is
attribution, not authentication.

To keep agents from forwarding messages in circles, a reply chain deeper than `--max-hops`
(default 3) is refused with `E_MAX_HOPS`.

## Limits

| | Codex | Claude Code | OpenCode | Grok CLI | Antigravity |
|---|---|---|---|---|---|
| send to a session open in a terminal | yes | refused | yes | only through the leader | refused |
| steer a running turn | yes | no | yes | only through the leader | no |

A session agent-talk refuses to write to can still be read. agent-talk never approves a tool
call; `--approvals deny` declines the requests it sees. `agent-talk caps` shows what the
installed versions support, and [DESIGN.md](DESIGN.md) explains how each vendor is handled.

## License

MIT
