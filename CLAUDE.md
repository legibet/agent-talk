# agent-talk

Rust CLI and stdio MCP server that sends messages to coding-agent sessions (Codex, Claude Code,
OpenCode, Grok CLI, Antigravity CLI) through each vendor's official interface, and reads the
replies. No daemon of its own; state lives with the vendors, plus intents and receipts in
`~/.agent-talk/store.db`.

## Where to read

- `DESIGN.md` is the design as built. Read §2 (principles) and §7 (decisions) before changing
  behaviour, and the provider's §6.x before touching `src/providers/<vendor>/`.
- `.agents/findings/<vendor>.md` (git-ignored, present only on the maintainer's machine) holds
  the vendor evidence. Reach for it when a vendor fact in DESIGN.md is in doubt or a vendor CLI
  was upgraded.
- `tests/README.md` before running or changing the live harness.

## The machine has real sessions on it

The vendors on this machine hold the user's own sessions, configuration and daemons. Work
alongside them:

- Read vendor configuration (`~/.codex`, `~/.claude*`, `~/.config/opencode`, `~/.grok`,
  `~/.gemini`), never edit it.
- Connect to running daemons, services and leaders; leave starting and stopping them to the
  user. Grok leader experiments use an isolated `GROK_HOME` and a short socket path.
- Clean up only sessions and files you created. The user's sessions are never deleted,
  archived or written to by tests.
- agent-talk answers approval requests with decline or reject only. On OpenCode that means
  `reject` with a message; `once` and `always` are never sent.
- Reach sessions through vendor interfaces only: no PTY, no terminal input injection. TUI cases
  run in herdr panes opened with `--no-focus`; close only the panes you opened.
- The OpenCode service password stays in `service.json`; it goes into no file of this repo.

## Checks

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
uv run tests/live.py offline <tier>    # tiers: codex claude opencode grok antigravity, or all
```

Every live tier but `offline` spends real model calls, Grok and Antigravity the most. Run the
tier of the adapter you changed; run `all` before a release. Tests use the cheap default models
in `tests/live.py`. Add a test only when it would catch a real regression. Python runs through
`uv run` with PEP 723 metadata; `tests/live.py` is formatted with `ruff format --line-length 120`.

## Conventions

- Receipt and approval semantics live once in `src/providers/mod.rs`; each adapter keeps its own
  observe loop (DESIGN.md §3, §7).
- Decode vendor responses into types only where a decode failure must be an error; everything
  else stays `serde_json::Value`.
- Comments cite `DESIGN.md §N`; a vendor fact the design does not cover names the version it was
  observed on.
- A behaviour change updates DESIGN.md in the same change. README.md is for users and stays
  short. Short-term status and plans go to `.agents/STATUS.md`.
