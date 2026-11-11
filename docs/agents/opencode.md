# OpenCode

agent-talk is an HTTP client of the user's background service (`opencode serve --service`),
shared by every OpenCode client, and follows turns over its SSE event stream. Verified on
OpenCode 2.2.7; the desktop app cases were last read in source on 2.2.0.

## Interface

- The registration `$XDG_STATE_HOME/opencode/service.json` (default
  `~/.local/state/opencode/service.json`) holds `{url, pid, version, password}`; requests use
  basic auth with the password, read on every connect and never stored. `GET /api/info` has no
  service id, so its pid is compared with the registration's.
- A missing registration, a dead or different pid, or a `401` (stale password) is `E_NO_DAEMON`;
  a connect failure while the pid is alive is `E_TRANSPORT` (a sandbox, for instance).
- Any request naming a directory makes the service load that location for its lifetime, so
  agent-talk sends only directories it was given.
- The service answers a location's first requests before its plugins applied the configuration:
  `GET /api/agent` lists no agent, then the built-in agents with their built-in rules, and only
  then the user's (2.2.7). Which request waits for the plugins changed between versions: `GET
  /api/integration` up to 2.2.0, `GET /api/model/default` on 2.2.7, for at most 5 s (both read
  in source). So `new`, `send --full-access`, and `send --effort` on a session without a model
  first call both.
- Events come over one global stream (`GET /api/event`) without replay, and executions run on
  without subscribers. So agent-talk subscribes before submitting, filters by session id, and
  then reads history and the pending permission requests.

## Sessions and history

- `new` creates the session with its location, model, effort as the model's `variant`, title
  and permission rules ([Approvals](#approvals)), then sends the first prompt as a queue send. An
  effort without a model applies to the user's default model. An unknown model or variant is
  accepted, and the turn then fails.
- There is no turn id; execution events carry only the session id. The client may choose the
  user message id and attach `metadata`, both stored verbatim, so agent-talk's message id (in the
  service's ascending format, so that clients sorting by id keep the order) is the turn id and
  the queue id, and the metadata carries receipt, sender and depth.
- History pages by opaque cursor, in delivery order. A page that starts mid-turn takes its turn
  from the newest `user` row behind the page. A turn lookup searches a bounded number of pages
  back; an older message reports as absent.
- `read` shows `user` rows as prompts and service-written `synthetic` rows as `other`. The last
  text of an assistant row that finished with `stop` is `final`, its other text `commentary`.
- `ls` lists top-level sessions, archived ones included (`--all` adds sub-sessions); `--cwd`
  matches the directory exactly. `name` and `preview` are both the session's title.

## Writers and liveness

The service holds every session and every client goes through it, so there is no foreign writer
to refuse and no lock; `loaded` is always `yes`. `GET /api/session/active` is the only running
indicator, and a session waiting on a permission request still counts as running.

## Sending and turn end

- A prompt to a busy session never fails, and the default delivery is `steer`, so agent-talk
  always sets it. `queue` runs after the current reply, inside the same execution. `steer` joins
  the running execution at the next step boundary together with every other pending steer item,
  possibly answered in one reply. On an idle session either starts an execution.
- Idle steer is refused (`E_PRECONDITION`) after a check of the active list, which can race an
  execution that is just starting.
- A message is `pending` while in the session's inbox and `running` once delivered. A lookup reads
  the inbox before history, so a message moving from one to the other is not missed.
- Queue items pending at a user interrupt stay dormant until another execution runs; after a
  `shutdown` interrupt they run at once in a successor.
- An execution end writes an `idle` row with `outcome: succeeded|failed|interrupted`; a failed
  one carries its `error` and can have no assistant row (an unknown model). The exception is an
  interrupt with reason `shutdown` (a message-less permission reject, the service going down):
  no `idle`, and the aborted assistant row stays last. Read in source, not observed: a restart
  resumes such a turn behind a `synthetic` row with `metadata.notice: restart`.
- A turn ends at the first `idle` after its user message (status = its outcome), at an aborted
  assistant row followed directly by a user row, or at an aborted row still last when the
  session is no longer active on a second read (`interrupted`). Anything else after an aborted
  row (its `idle`, a restart notice, more assistant rows) belongs to the same turn. History is
  re-read at each execution end of the session, since one execution can serve several messages.
- Every message delivered into one execution ends at the same `idle`, so one reply can cover
  several messages. The turn's reply is its last `final` message, else the last assistant text.

## Approvals

`permission.asked` goes to every subscriber and is not replayed, so agent-talk also lists pending
requests. An unanswered request blocks with no timeout. `reject` with a `message` hands the text
to the model and the execution continues; a message-less `reject` interrupts it (`shutdown`), so
agent-talk always sends a message. The TUI's Reject on a top-level session sends none, so a user
who rejects there ends the turn `interrupted`. Any `reject` rejects every pending request of the
session, so a reject that finds its request gone records `resolved`. `always` would save a
project-wide rule.

A request is answered by the last rule matching action and resource in the agent's ruleset
followed by the session's, and asks when none matches; the agent's ruleset already holds the
user's configuration. A session without an agent runs the default agent, which `GET /api/agent`
lists first: the configured `default_agent` if it is a primary agent and not hidden, else
`build` on the same condition, else the first such agent. A session agent-talk starts therefore
gets session rules at creation, and again at the `send --full-access` that turns full access
on, so that the service itself answers what would wait for an answer:

- the default agent's rules from its first `ask` on, in order, with every `ask` turned into
  `deny`, or into `allow` under full access. Allowed and denied requests keep their answer, the
  user's denials included. Appending one `deny` or `allow` per `ask` rule instead would override
  a later, narrower rule of the user's. The rules before the first `ask` are left out because
  sub-sessions of the subagent tool inherit session rules, and a full copy (which starts with
  `* * allow`) overrode a sub-agent's own denials;
- the question tool is denied: nobody answers it in these sessions.

Full access keeps the user's denials rather than allowing everything: the user decided what the
agent may not do, and agent-talk only stands in for the person who would answer the asks. On
2.2.0, for instance, the browser plugin denies its tools to every agent after the user's
configuration, and appends allow rules to a session's rules while the desktop app attaches a
browser to it (read in source). Each `send --full-access` sets the session's rules again from the
default agent's current ones, which replaces the allow-all rule agent-talk 0.4.0 and earlier gave
and any rule added to the session since.

The model sees a tool denied for every resource as absent, and a denied call as a tool error;
the turn continues (2.0.23). Requests that still reach agent-talk (a plugin hook can turn an
answer into `ask`) get `reject` with a message, and only once agent-talk's own message was
delivered: earlier requests belong to the execution ahead of it. A TUI in auto-accept mode
(`session.permissions: "autoaccept"` in its `cli.json`, or `--auto`: a setting of the TUI
process, not of a session) answers `once` to every pending request of the session it displays
and its sub-sessions, so while such a TUI shows a session agent-talk started, its requests are
approved.

## Settings

The session stores its model, variant and rules, and the prompt has no model field, so `send
--model`, `--effort` and `--full-access` change the session itself (`POST
/api/session/{id}/model`, a `PATCH` of its permissions), before the message is recorded: a
refused change is a plain error without a receipt. A model change carries the session's stored
effort. The service reads the session's values at each step, so a change also covers the rest
of an execution already running and the prompts queued behind it, and the user's TUI follows it;
a permission request already pending is not re-evaluated.

A changed toolset writes a `system` row into history and the model's context, and a model that
was refused a tool earlier can go on treating it as missing (2.0.23). `send --full-access` adds
only the tools of actions that were asks, so the row stays small; `new --full-access` writes no
row.

## Caller identity

Shell commands get `OPENCODE_SESSION_ID`, which overwrites an inherited value. MCP `tools/call`
carries `_meta["ai.opencode/sessionID"]` (the docs say it can be absent). One MCP server process
serves every session of a location, so its environment does not name the caller.

## Limitations

- A clean end of the event stream during `wait` re-reads history once before `E_TRANSPORT`.
- History alone cannot tell a message-less reject from a graceful shutdown, a crash mid-step from
  a running step, a `shutdown` interrupt before the first step from steer batching, or a queued
  item delivered into a reloaded continuation from a successor execution.
- The rules of a session agent-talk started are a copy taken at `new` or `send --full-access`:
  later changes to the user's configuration, approvals saved with `always` and plugin permission
  hooks do not reach them, and sub-sessions inherit them. A sub-agent, or an agent the user
  switches the session to (in the TUI the switch takes effect with the user's next prompt
  there), thus gets the default agent's answer for what the copied rules match: its own denial
  is lifted for what the default agent allows after its first `ask`, and under full access for
  what the default agent asks. Observed for an ask: after a switch to the plan agent, full
  access let it read a file its own rules ask about. A lifted denial was read in source only.
- A `send --full-access` while the desktop app has a browser attached to the session replaces the
  allow rules the attachment appended (read in source).
