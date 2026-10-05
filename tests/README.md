# Live regression harness

`tests/live.py` runs the release `agent-talk` against the real vendors and checks the behavior in
`DESIGN.md`: receipts and their recovery, turn correlation, queue/steer, timeouts, approvals,
sender attribution, hop limits, paging, foreign-session refusals, MCP over stdio. The unit tests
(`cargo test`) cover the parsing rules; this covers what only the vendors can show.

    uv run tests/live.py (offline|codex|claude|opencode|grok|antigravity ... | all) [--keep]

Every tier but `offline` makes real model calls, so name the tiers you need: the one for the
adapter you changed, `offline` always, `all` before a release. The Grok and Antigravity tiers
are the expensive ones (grok-4.7 and gemini-3.8-flash have no cheaper sibling). Default models
are gpt-6-luna, sonnet, opencode/big-pickle (free), grok-4.7 and gemini-3.8-flash; override them
with `LIVE_CODEX_MODEL`, `LIVE_CLAUDE_MODEL`, `LIVE_OPENCODE_MODEL`, `LIVE_GROK_MODEL`,
`LIVE_ANTIGRAVITY_MODEL`. A full run takes about 8 minutes, including a deliberate ~65 s idle
wait before C6.

The script builds `target/release/agent-talk` first and reads preconditions from
`agent-talk caps --json` (Codex daemon connected with the queue method, `claude agents --json`,
OpenCode service connected, `grok` and `agy` installed); a provider whose preconditions fail
prints `SKIP <provider>: <reason>`. The Grok tier runs direct mode only; leader cases need an
isolated `GROK_HOME` and a short socket path and are run by hand, never against the user's
`~/.grok/leader.sock`.

All sessions use `target/live-work`. Cleanup deletes only what the run created: its OpenCode
session, its Claude transcripts, its Grok sessions (`grok sessions delete`) and its
`~/.agent-talk/*-runs` logs. Codex threads cannot be deleted and stay in the Codex history (cwd
`target/live-work`); agy has no delete command, so Antigravity conversations stay and are
printed at the end; intents stay in `~/.agent-talk/store.db`. The run also spawns and kills its
own foreign test processes: a `claude -p`, an `agy -p` holding a presence lock, a standalone
`codex app-server` holding a thread's writer lock.

Each check prints `PASS|FAIL|SKIP|INCONCLUSIVE <provider>/<name> <secs>s`. INCONCLUSIVE means the
model or the vendor did not do what the check needs (did not run `sleep`, did not request an
approval, was killed before agy marked the run RUNNING), so the behavior under test was not
exercised. The exit status is 0 only when every requested check ran and passed.
