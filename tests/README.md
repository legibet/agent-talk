# Live regression harness

`tests/live.py` runs the release `agent-talk` against the real agents and checks agent-talk's own
end-to-end behaviour: receipts and their recovery, turn correlation, queue and steer, refusals,
foreign holders, approvals, settings, paging and MCP. Facts about the agents themselves belong
in the findings, and the parsing rules in the unit tests (`cargo test`).

    uv run tests/live.py (offline|codex|claude|opencode|grok|antigravity|pi ... | all) [--keep]

Every agent runs the same three checks, `start`, `converse` and `recover`, driven by its entry in
`AGENTS`; the checks that only one agent has follow in its tier. A check belongs here when it
covers behaviour of agent-talk that can break, through the CLI or MCP, and passes or fails the
same way on any machine with the agent installed and logged in: no dependence on the user's
permission rules, sandbox or other configuration. Grok runs in a `GROK_HOME` of its own for that
reason; prompts that must outlast a deadline count numbers instead of calling tools, except on
Codex, where `sleep` runs under any sandbox.

Every tier but `offline` makes real model calls, so name the tiers you need: the one for the
adapter you changed, `offline` always, `all` before a release. Default models are gpt-6-luna,
sonnet, deepseek/deepseek-flash, grok-4.7, gemini-3.8-flash and deepseek/deepseek-flash; override
them with `LIVE_CODEX_MODEL`, `LIVE_CLAUDE_MODEL`, `LIVE_OPENCODE_MODEL`, `LIVE_GROK_MODEL`,
`LIVE_ANTIGRAVITY_MODEL`, `LIVE_PI_MODEL`. Codex's `foreign-writer` runs last, once its thread has
unloaded (about 60 s after its last use).

The script builds `target/release/agent-talk` first and runs a tier only when `agent-talk status`
reports `new` and `send` available for its agent; otherwise it prints `SKIP <agent>: <reason>`.
Approval checks on sessions agent-talk did not start create those sessions themselves, with
approvals on, and end the request with `turn/interrupt` (Codex) or `reject` (OpenCode), never an
approval. Grok leader cases are not covered: they need a leader, which the harness never starts.

Cleanup removes only what the run created: Codex threads are archived, OpenCode and Grok sessions
deleted through the agent, Claude and pi session files and agent-talk's run logs unlinked. agy has
no delete command, so Antigravity conversations stay and are printed at the end; intents stay in
`~/.agent-talk/store.db`; files the checks write stay under `target/live-work`.

Each check prints `PASS|FAIL|SKIP|INCONCLUSIVE <agent>/<name> <secs>s`. INCONCLUSIVE means the
model did not do what the check needs (ended a turn before its deadline, made no approval
request), so the behaviour under test was not exercised. The exit status is 0 only when every
requested check ran and passed.
