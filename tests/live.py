# /// script
# requires-python = ">=3.12"
# dependencies = ["jsonschema>=4", "websockets>=14"]
# ///
"""Live regression harness for agent-talk.

Runs the release `agent-talk` binary against the installed agents and checks agent-talk's own
end-to-end behaviour: receipts and their recovery, turn correlation, queue and steer, refusals,
foreign holders, approvals, settings, paging, MCP. Facts about the agents themselves live in the
findings, not here.

    uv run tests/live.py (offline|codex|claude|opencode|grok|antigravity|pi ... | all) [--keep]

Models: LIVE_CODEX_MODEL, LIVE_CLAUDE_MODEL, LIVE_OPENCODE_MODEL, LIVE_GROK_MODEL,
LIVE_ANTIGRAVITY_MODEL, LIVE_PI_MODEL override the defaults below. Exit 0 only when every
requested check ran and passed.
"""

import base64
import contextlib
import fcntl
import json
import os
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
from dataclasses import dataclass, field
from pathlib import Path

import jsonschema
from websockets.sync.client import unix_connect

REPO = Path(__file__).resolve().parent.parent
B = str(REPO / "target/release/agent-talk")
DIR = REPO / "target/live-work"
CMD_TIMEOUT = 900  # seconds a single agent-talk command may take before the check fails
T = Path(tempfile.mkdtemp(prefix="agent-talk-live-"))
GROK_HOME = T / "grok"  # Grok runs in a GROK_HOME of its own, away from the user's settings

CODEX_MODEL = os.environ.get("LIVE_CODEX_MODEL", "gpt-6-luna")
CLAUDE_MODEL = os.environ.get("LIVE_CLAUDE_MODEL", "sonnet")
OPENCODE_MODEL = os.environ.get("LIVE_OPENCODE_MODEL", "deepseek/deepseek-flash")
GROK_MODEL = os.environ.get("LIVE_GROK_MODEL", "grok-4.7")
ANTIGRAVITY_MODEL = os.environ.get("LIVE_ANTIGRAVITY_MODEL", "gemini-3.8-flash")
PI_MODEL = os.environ.get("LIVE_PI_MODEL", "deepseek/deepseek-flash")

SESSION_VARS = (
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "OPENCODE_SESSION_ID",
    "GROK_SESSION_ID",
    "ANTIGRAVITY_CONVERSATION_ID",
    "PI_SESSION_ID",
    "AGENT_TALK_CALLER",
)
BASE_ENV = {k: v for k, v in os.environ.items() if k not in SESSION_VARS}

# A turn long enough for a short deadline to pass, without tools (whose permissions are the
# user's) on every agent but Codex, where `sleep` runs under any sandbox.
COUNT = "count from 1 to 1000, one number per line, nothing else"

RESULTS: list[tuple[str, str]] = []  # (status, name)
LAST: dict = {}  # last command, for FAIL output
st: dict = {}  # state shared between checks: handles by agent, helper processes
created: dict[str, list] = {}  # agent -> [(native id, env)] of sessions the run created
receipts: dict[str, list] = {}  # agent -> receipt ids whose run logs the run created


class Fail(Exception):
    pass


class Skip(Exception):
    pass


class Inconclusive(Exception):
    pass


def expect(cond, what: str):
    if not cond:
        raise Fail(what)


def need(key: str):
    if key not in st:
        raise Skip(f"needs {key} from an earlier check")
    return st[key]


def env_with(extra: dict | None) -> dict:
    return {**BASE_ENV, **(extra or {})}


def native(handle: str) -> str:
    return handle.split(":", 1)[1]


def track(agent: str, sid: str, env: dict | None = None):
    created.setdefault(agent, []).append((sid, env))


def excerpt(v, limit=1500) -> str:
    def strip(x):
        if isinstance(x, dict):
            return {k: strip(val) for k, val in x.items() if k != "raw"}
        if isinstance(x, list):
            return [strip(i) for i in x]
        return x

    s = json.dumps(strip(v), ensure_ascii=False)
    return s if len(s) <= limit else s[:limit] + " ..."


def cli(*args, env=None, expect_exit=0, during=None) -> dict:
    """Run `agent-talk <args> --json`, parse stdout, assert the exit code (int or tuple), and
    record the sessions and run logs it created. `during` is called repeatedly while it runs."""
    cmd = [B, *args, "--json"]
    p = subprocess.Popen(cmd, env=env_with(env), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    end = time.monotonic() + CMD_TIMEOUT
    while True:
        try:
            stdout, stderr = p.communicate(timeout=0.5 if during else CMD_TIMEOUT)
            break
        except subprocess.TimeoutExpired:
            if time.monotonic() > end:
                p.kill()
                p.communicate()
                raise Fail(f"no exit within {CMD_TIMEOUT}s: {' '.join(cmd[1:])}") from None
            during()
    try:
        out = json.loads(stdout) if stdout.strip() else {}
    except json.JSONDecodeError:
        out = {"_stdout": stdout[-1000:]}
    LAST.clear()
    LAST.update(
        cmd=" ".join([f"{k}={v}" for k, v in (env or {}).items()] + cmd[1:]),
        exit=p.returncode,
        out=out,
        stderr=stderr[-500:],
    )
    if args[0] in ("new", "send"):
        for r in (out.get("receipt"), err(out).get("receipt")):
            if not r:
                continue
            agent, sid = r["handle"].split(":", 1)
            receipts.setdefault(agent, []).append(r["receipt_id"])
            if args[0] == "new" and (sid, env) not in created.get(agent, []):
                track(agent, sid, env)
    allowed = expect_exit if isinstance(expect_exit, tuple) else (expect_exit,)
    if p.returncode not in allowed:
        raise Fail(f"exit {p.returncode}, expected {expect_exit}")
    return out


def err(out: dict) -> dict:
    return out.get("error") or {}


def code(out: dict) -> str | None:
    return err(out).get("code")


def final(out: dict) -> str:
    return ((out.get("turn") or {}).get("final_text") or "").lower()


def ls_row(agent: str, handle: str, env=None) -> dict | None:
    rows = cli("ls", "--agent", agent, "--cwd", str(DIR), "--limit", "50", env=env)["sessions"]
    return next((s for s in rows if s["handle"] == handle), None)


def paged(handle: str, limit: int, env=None) -> list[dict]:
    """Every message of the session, oldest first, read `limit` at a time from the newest."""
    messages, seen = [], set()
    out = cli("read", handle, "--all", "--limit", str(limit), env=env)
    while True:
        messages = out["messages"] + messages
        cur = out.get("next_cursor")
        if not cur or cur in seen:
            return messages
        seen.add(cur)
        out = cli("read", handle, "--all", "--limit", str(limit), "--cursor", cur, env=env)


def wait_for(cond, secs: float, what: str, step: float = 0.5):
    end = time.monotonic() + secs
    while not cond():
        if time.monotonic() > end:
            raise Fail(f"{what} within {secs:.0f}s")
        time.sleep(step)


def check(agent: str, name: str, fn):
    label = f"{agent}/{name}"
    t0 = time.monotonic()
    LAST.clear()
    try:
        fn()
        status, detail = "PASS", ""
    except Fail as e:
        status, detail = "FAIL", f"  {e}"
        if LAST:
            detail += f"\n  cmd: {LAST['cmd']}\n  exit: {LAST['exit']}\n  json: {excerpt(LAST['out'])}"
            if LAST.get("stderr"):
                detail += f"\n  stderr: {LAST['stderr'].strip()}"
    except Inconclusive as e:
        status, detail = "INCONCLUSIVE", f"  {e}"
    except Skip as e:
        status, detail = "SKIP", f"  {e}"
    print(f"{status} {label} {time.monotonic() - t0:.1f}s", flush=True)
    if detail:
        print(detail, flush=True)
    RESULTS.append((status, label))


class Mcp:
    """`agent-talk mcp` over stdio, one JSON-RPC line per message. Protocol 2026-07-28 has no
    handshake and carries version and client info in every request's `_meta`; older versions
    start with `initialize`."""

    CLIENT = {"name": "agent-talk-live", "version": "0"}

    def __init__(self, env: dict | None = None, protocol="2026-07-28"):
        self.p = subprocess.Popen(
            [B, "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=env_with(env),
        )
        self.next_id = 0
        self.meta = {}
        self.schemas: dict[str, dict] = {}
        if protocol >= "2026-07-28":
            self.meta = {
                "io.modelcontextprotocol/protocolVersion": protocol,
                "io.modelcontextprotocol/clientInfo": self.CLIENT,
                "io.modelcontextprotocol/clientCapabilities": {},
            }
        else:
            self.request("initialize", {"protocolVersion": protocol, "capabilities": {}, "clientInfo": self.CLIENT})
            self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.kill()

    def send(self, msg: dict):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def request(self, method: str, params: dict) -> dict:
        self.next_id += 1
        rid = self.next_id
        if self.meta:
            params = {**params, "_meta": {**self.meta, **params.get("_meta", {})}}
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise Fail(f"mcp server closed stdout while waiting for id {rid}")
            msg = json.loads(line)
            if msg.get("id") == rid and "method" not in msg:
                return msg

    def call(self, tool: str, args: dict, meta: dict | None = None) -> tuple[bool, dict]:
        """tools/call; returns (isError, parsed result). A successful result must equal its
        text content and match the tool's outputSchema."""
        params = {"name": tool, "arguments": args, **({"_meta": meta} if meta is not None else {})}
        resp = self.request("tools/call", params)
        LAST.clear()
        LAST.update(cmd=f"mcp tools/call {tool} {json.dumps(args)} meta={meta}", exit="-", out=resp)
        expect("error" not in resp, "no JSON-RPC-level error")
        result = resp["result"]
        inner = json.loads(result["content"][0]["text"])
        if not result.get("isError"):
            expect(result.get("structuredContent") == inner, "structuredContent equals the text content")
            if not self.schemas:
                self.schemas = {t["name"]: t["outputSchema"] for t in self.request("tools/list", {})["result"]["tools"]}
            try:
                jsonschema.validate(inner, self.schemas[tool])
            except jsonschema.ValidationError as e:
                raise Fail(f"result matches {tool}'s outputSchema: {e.message} at {list(e.absolute_path)}") from None
        return bool(result.get("isError")), inner


def run_init(agent: str, receipt_id: str) -> dict:
    """The init event of agent-talk's run log for a Claude or Antigravity receipt."""
    log = Path.home() / f".agent-talk/{agent}-runs/{receipt_id}.ndjson"
    for line in log.read_text().splitlines() if log.exists() else []:
        ev = json.loads(line)
        if agent == "claude" and ev.get("type") == "system" and ev.get("subtype") == "init":
            return ev
        if agent == "antigravity" and ev.get("event") == "init":
            return ev["init"]
    raise Fail(f"no init event in {log}")


# ---------------------------------------------------------------- the flow every agent shares


@dataclass
class Agent:
    name: str
    model: str
    caller_var: str  # the session variable an agent sets for its own shells
    steer_code: str  # refusal of --steer on an idle session
    bogus_turn: str  # a well-formed turn id that does not exist
    slow: str = COUNT  # a prompt whose turn outlasts `slow_timeout`
    slow_timeout: str = "3"
    held_by_child: bool = True  # the turn runs in an agent-talk child: E_LOCKED meanwhile
    recovered: tuple = ("completed",)  # how a turn whose command timed out ends
    names: bool = True  # new --name is supported
    new_args: tuple = ()
    env: dict = field(default_factory=dict)


AGENTS = {
    "codex": Agent(
        "codex",
        CODEX_MODEL,
        "CODEX_THREAD_ID",
        "E_PRECONDITION",
        "bogus",
        slow="run `sleep 15` with your shell tool, then reply done",
        held_by_child=False,
    ),
    "claude": Agent("claude", CLAUDE_MODEL, "CLAUDE_CODE_SESSION_ID", "E_NO_STEER", str(uuid.uuid4())),
    "opencode": Agent(
        "opencode", OPENCODE_MODEL, "OPENCODE_SESSION_ID", "E_PRECONDITION", "msg_bogus", held_by_child=False
    ),
    # Direct mode: the command's deadline cancels the turn, and no child outlives it.
    "grok": Agent(
        "grok",
        GROK_MODEL,
        "GROK_SESSION_ID",
        "E_NO_STEER",
        "bogus",
        slow_timeout="5",
        held_by_child=False,
        recovered=("interrupted", "completed"),
        env={"GROK_HOME": str(GROK_HOME)},
    ),
    # A deadline of 1 s passes before agy's first event: the receipt is recovered from nothing.
    "antigravity": Agent(
        "antigravity",
        ANTIGRAVITY_MODEL,
        "ANTIGRAVITY_CONVERSATION_ID",
        "E_NO_STEER",
        "999",
        slow_timeout="1",
        names=False,
        new_args=("--effort", "low"),
    ),
    "pi": Agent("pi", PI_MODEL, "PI_SESSION_ID", "E_NO_STEER", "nothere", new_args=("--effort", "low")),
}


def new(a: Agent, prompt: str, *extra, env=None, **kw) -> dict:
    return cli(
        "new", a.name, "--cwd", str(DIR), "--model", a.model, *a.new_args, *extra, prompt, env=env or a.env, **kw
    )


def start(a: Agent):
    """new: the receipt names the turn it reports; ls shows the session as agent-talk's, with its
    name; models lists the model it ran on."""
    out = new(a, "reply with the single word pong", *(["--name", f"live-{a.name}"] if a.names else []), "--wait")
    h = st[a.name] = out["handle"]
    r, t = out["receipt"], out["turn"]
    expect(r["state"] == "accepted" and r["turn_id"] == t["turn_id"], "receipt accepted, naming the turn")
    expect(t["status"] == "completed" and "pong" in final(out), "completed ~ pong")
    row = ls_row(a.name, h, env=a.env)
    expect(row is not None, "ls --cwd lists the session")
    expect(row["owned"] and row["observations"]["origin"] == "agent-talk", "ls: owned, origin agent-talk")
    if a.names:
        expect(row["name"] == f"live-{a.name}", f"ls: name live-{a.name}, got {row['name']!r}")
    else:
        out = new(a, "hi", "--name", "x", expect_exit=2)
        expect(code(out) == "E_UNSUPPORTED", "new --name: E_UNSUPPORTED")
    models = cli("models", a.name, a.model, env=a.env)["models"]
    expect(any(m["id"] == a.model for m in models), f"models lists {a.model}")


def converse(a: Agent):
    """send from an agent's shell: attribution and provenance header; read and wait --turn find
    the same turn; refusals of an unknown turn and of an idle steer; paging covers history."""
    h = need(a.name)
    caller = f"{a.name}:{uuid.uuid4()}"
    header = f"[from {caller} via agent-talk; answer in your final response]"
    out = cli("send", h, "reply with the single word kiwi", "--wait", env={**a.env, a.caller_var: native(caller)})
    expect(out["from"].get("session") == caller, f"from.session == {caller}")
    expect((out["receipt"].get("delivered_text") or "").startswith(header), "delivered_text carries the header")
    expect(out["turn"]["status"] == "completed" and "kiwi" in final(out), "completed ~ kiwi")
    turn = out["turn"]["turn_id"]
    msgs = cli("read", h, "--limit", "2", env=a.env)["messages"]
    expect([m["phase"] for m in msgs] == ["prompt", "final"], "read: the prompt and the final reply")
    expect(msgs[0]["turn_id"] == turn and msgs[0]["text"].startswith(header), "read: the prompt of the turn, header")
    expect((msgs[0].get("from") or {}).get("session") == caller, "read: the prompt names its sender")
    again = cli("wait", h, "--turn", turn, "--timeout", "10", env=a.env)
    expect(again["turn"]["final_text"] == out["turn"]["final_text"], "wait --turn: the same reply")
    expect((again.get("receipt") or {}).get("receipt_id") == out["receipt"]["receipt_id"], "wait --turn: receipt")
    out = cli("wait", h, "--turn", a.bogus_turn, "--timeout", "5", env=a.env, expect_exit=2)
    expect(code(out) == "E_PRECONDITION", "unknown turn: E_PRECONDITION")
    out = cli("send", h, "x", "--steer", env=a.env, expect_exit=2)
    expect(code(out) == a.steer_code, f"idle steer: {a.steer_code}")
    full = cli("read", h, "--all", "--limit", "1000", env=a.env)["messages"]
    pages = paged(h, 2, env=a.env)
    expect([m["item_id"] for m in pages] == [m["item_id"] for m in full], "pages of 2 add up to the history")


def recover(a: Agent):
    """A deadline before the turn ends: E_TIMEOUT with the receipt, the session refused while
    agent-talk's child holds it, then wait --receipt finds the turn, delivered once."""
    h = need(a.name)
    out = cli("send", h, a.slow, "--wait", "--timeout", a.slow_timeout, env=a.env, expect_exit=(0, 3))
    if not err(out):
        raise Inconclusive("the turn ended before the deadline")
    expect(code(out) == "E_TIMEOUT", "E_TIMEOUT")
    rec = err(out).get("receipt") or {}
    expect(rec.get("receipt_id"), "the error carries the receipt")
    if a.held_by_child:
        out = cli("send", h, "x", env=a.env, expect_exit=2)
        expect(code(out) == "E_LOCKED", "E_LOCKED while agent-talk's child runs")
    out = cli("wait", h, "--receipt", rec["receipt_id"], "--timeout", "180", env=a.env)
    t, r = out["turn"], out["receipt"]
    expect(t["status"] in a.recovered, f"turn ended {'/'.join(a.recovered)}, got {t['status']}")
    expect(r["state"] == "accepted" and r["turn_id"] == t["turn_id"], "receipt accepted, naming the turn")
    expect(not rec.get("turn_id") or rec["turn_id"] == t["turn_id"], "the turn the receipt named")
    prompts = cli("read", h, "--limit", "1000", env=a.env)["messages"]
    n = sum(1 for m in prompts if m["phase"] == "prompt" and a.slow in m["text"])
    expect(n == 1, f"the message was delivered once, found {n}")


def shared(a: Agent):
    for name, fn in (("start", start), ("converse", converse), ("recover", recover)):
        check(a.name, name, lambda fn=fn: fn(a))


# ---------------------------------------------------------------- offline


def seed_hop_limit(handle: str):
    """Record a message at the hop limit (3) delivered to `handle` in the offline store, so
    whatever `handle` sends next is refused with E_MAX_HOPS before any agent is contacted."""
    db = sqlite3.connect(T / ".agent-talk/store.db")
    with contextlib.closing(db), db:
        rid = str(uuid.uuid4())
        db.execute(
            "INSERT INTO intents (receipt_id, handle, client_msg_id, text, depth) VALUES (?, ?, ?, 'x', 3)",
            (rid, handle, rid),
        )
        db.execute("INSERT INTO receipts (receipt_id, state) VALUES (?, 'accepted')", (rid,))


def offline():
    home = {"HOME": str(T)}

    def unreachable():
        out = cli("ls", "--agent", "codex", env=home, expect_exit=2)
        expect(code(out) == "E_NO_DAEMON", "ls --agent codex without a daemon: E_NO_DAEMON")
        out = cli("ls", "--limit", "1", env={**home, "XDG_STATE_HOME": str(T)})
        errors = {e["agent"]: e["error"]["code"] for e in out.get("errors") or []}
        expect(out.get("sessions") == [], "no sessions")
        expect(errors == {"codex": "E_NO_DAEMON", "opencode": "E_NO_DAEMON"}, f"per-agent errors, got {errors}")

    def refusals():
        for args, want in (
            (("ls", "--agent", "bogus"), "E_UNSUPPORTED"),
            (("read", "nohandle"), "E_PRECONDITION"),
            (("ls", "--cursor", "x"), "E_PRECONDITION"),
            (("send", "codex:x", "hi", "--from", "bogus"), "E_PRECONDITION"),
            (("send", f"claude:{uuid.uuid4()}", "x", "--steer"), "E_NO_STEER"),
            (("send", f"claude:{uuid.uuid4()}", "x", "--effort", "low"), "E_PRECONDITION"),
        ):
            out = cli(*args, env=home, expect_exit=2)
            expect(code(out) == want, f"{' '.join(args)}: {want}")
        p = subprocess.run(
            [B, "mcp", "--caller", "bogus"], stdin=subprocess.DEVNULL, capture_output=True, text=True, env=BASE_ENV
        )
        LAST.update(
            cmd="agent-talk mcp --caller bogus", exit=p.returncode, out={"stdout": p.stdout, "stderr": p.stderr}
        )
        expect(p.returncode == 2 and "E_PRECONDITION" in p.stdout + p.stderr, "mcp --caller bogus: exit 2")

    def hop_limit():
        seed_hop_limit("codex:x")
        out = cli("new", "codex", "hi", env={**home, "AGENT_TALK_CALLER": "codex:x"}, expect_exit=2)
        expect(code(out) == "E_MAX_HOPS", "E_MAX_HOPS before any agent is contacted")

    def mcp():
        for protocol in ("2025-06-18", "2026-07-28"):
            with Mcp(home, protocol=protocol) as m:
                tools = m.request("tools/list", {})["result"]["tools"]
            names = {t["name"] for t in tools}
            expect(names == {"models", "ls", "new", "send", "read", "wait"}, f"({protocol}) tools {names}")
            bare = [t["name"] for t in tools if not (t.get("title") and t.get("annotations") and t.get("outputSchema"))]
            expect(not bare, f"({protocol}) title, annotations and outputSchema on every tool; missing on {bare}")
        # The sender comes from _meta (Codex, OpenCode) or the server's environment (Claude);
        # each sender is at the hop limit, so its message is refused.
        for h in ("codex:T", "opencode:ses_x", "claude:C"):
            seed_hop_limit(h)
        args = {"handle": "codex:x", "text": "hi"}
        with Mcp(home) as m:
            for meta in ({"threadId": "T"}, {"ai.opencode/sessionID": "ses_x"}):
                is_err, out = m.call("send", args, meta)
                expect(is_err and code(out) == "E_MAX_HOPS", f"sender from {meta}: E_MAX_HOPS")
            is_err, out = m.call("send", args)
            expect(is_err and code(out) == "E_NO_DAEMON", "unknown sender: E_NO_DAEMON")
            is_err, out = m.call("wait", {"handle": "codex:x"})
            expect(is_err and code(out) == "E_PRECONDITION", "wait without turn or receipt: E_PRECONDITION")
        with Mcp({**home, "CLAUDE_CODE_SESSION_ID": "C"}) as m:
            is_err, out = m.call("send", args)
            expect(is_err and code(out) == "E_MAX_HOPS", "sender from CLAUDE_CODE_SESSION_ID: E_MAX_HOPS")

    for name, fn in (("unreachable", unreachable), ("refusals", refusals), ("hop-limit", hop_limit), ("mcp", mcp)):
        check("offline", name, fn)


# ---------------------------------------------------------------- codex


class CodexDaemon:
    """A raw connection to the Codex daemon socket (WebSocket over AF_UNIX). Notifications and
    server requests that arrive meanwhile are kept in `seen`; nothing here answers a request."""

    def __init__(self):
        sock = Path.home() / ".codex/app-server-control/app-server-control.sock"
        self.stack = contextlib.ExitStack()
        self.ws = self.stack.enter_context(unix_connect(str(sock), uri="ws://localhost/"))
        self.next_id = 0
        self.seen: list[dict] = []
        self.call("initialize", {"clientInfo": Mcp.CLIENT, "capabilities": {"experimentalApi": True}})
        self.ws.send(json.dumps({"method": "initialized", "params": {}}))

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.stack.close()

    def call(self, method: str, params: dict) -> dict:
        self.next_id += 1
        rid = self.next_id
        self.ws.send(json.dumps({"id": rid, "method": method, "params": params}))
        while "method" in (msg := json.loads(self.ws.recv(timeout=60))) or msg.get("id") != rid:
            self.seen.append(msg)
        LAST.update(cmd=f"daemon {method} {json.dumps(params)}", exit="-", out=msg)
        return msg

    def drain(self, secs: float = 0.2):
        """Read what the daemon pushes for `secs`, so a subscribed connection never backs up."""
        end = time.monotonic() + secs
        while (left := end - time.monotonic()) > 0:
            try:
                self.seen.append(json.loads(self.ws.recv(timeout=left)))
            except TimeoutError:
                return


def codex_turn_context(thread_id: str, turn_id: str) -> dict:
    """The settings a turn ran with, from the rollout's `turn_context` line."""
    for path in (Path.home() / ".codex/sessions").glob(f"*/*/*/rollout-*-{thread_id}.jsonl"):
        for line in path.read_text().splitlines():
            rec = json.loads(line)
            if rec.get("type") == "turn_context" and rec["payload"].get("turn_id") == turn_id:
                return rec["payload"]
    raise Fail(f"no turn_context for turn {turn_id} in the rollout of {thread_id}")


def codex_tier():
    a = AGENTS["codex"]
    shared(a)

    def busy():
        # While a turn runs: steer joins it, a send queues a turn of its own (its receipt
        # pending until then), and a settings change is refused rather than queued.
        out = new(a, "run `sleep 20` with your shell tool, then reply done")
        h, t0 = out["handle"], out["receipt"]["turn_id"]
        probe = cli("wait", h, "--turn", t0, "--timeout", "2", expect_exit=(0, 2, 3))
        expect(code(probe) != "E_PRECONDITION", "wait on the turn a receipt names is not refused")
        if not err(probe):
            raise Inconclusive("the first turn ended at once (the model did not run sleep)")
        out = cli("send", h, "also say STEERED", "--steer")
        expect(out["receipt"]["turn_id"] == t0, "steer joins the running turn")
        out = cli("send", h, "x", "--effort", "medium", expect_exit=2)
        expect(code(out) == "E_PRECONDITION", "a settings change on a busy thread: E_PRECONDITION")
        queued = cli("send", h, "reply with the single word apple")["receipt"]
        expect(queued.get("queue_id"), "queued")
        out = cli("wait", h, "--receipt", queued["receipt_id"], "--timeout", "2", expect_exit=3)
        expect(err(out).get("state") == "pending", "the queued message is pending behind the turn")
        out = cli("wait", h, "--receipt", queued["receipt_id"], "--timeout", "120")
        expect(out["turn"]["status"] == "completed" and "apple" in final(out), "the queued turn ran ~ apple")
        expect(out["turn"]["turn_id"] != t0, "as a turn of its own")
        out = cli("wait", h, "--turn", t0, "--timeout", "5")
        expect(out["turn"]["status"] == "completed", "the first turn completed")

    def dormant():
        # After an interrupt the thread runs no queued item on its own; send starts it.
        out = new(a, "run `sleep 20` with your shell tool, then reply done")
        h, t0 = out["handle"], out["receipt"]["turn_id"]
        with CodexDaemon() as d:
            for method, params in (
                ("thread/resume", {"threadId": native(h), "excludeTurns": True}),
                ("turn/interrupt", {"threadId": native(h), "turnId": t0}),
            ):
                for _ in range(10):  # the new thread may not be resumable yet
                    if "error" not in (resp := d.call(method, params)):
                        break
                    time.sleep(1)
                expect("error" not in resp, f"{method} over the daemon")
        if cli("wait", h, "--turn", t0, "--timeout", "30")["turn"]["status"] != "interrupted":
            raise Inconclusive("the first turn ended before the interrupt")
        r = cli("send", h, "reply with the single word pear")["receipt"]
        expect(r["turn_id"], "send started the queued item")
        out = cli("wait", h, "--receipt", r["receipt_id"], "--timeout", "60")
        expect(out["turn"]["turn_id"] == r["turn_id"] and "pear" in final(out), "that turn ran ~ pear")

    def settings():
        # The change goes out with turn/start and stays for later turns, which a queued
        # message could not carry.
        h = need("codex")

        def effort(out) -> str | None:
            return codex_turn_context(native(h), out["turn"]["turn_id"]).get("effort")

        out = cli("send", h, "reply with the single word two", "--effort", "medium", "--wait")
        expect(not out["receipt"].get("queue_id"), "sent with turn/start, not queued")
        expect(effort(out) == "medium", f"the changed turn runs at effort medium, got {effort(out)}")
        out = cli("send", h, "reply with the single word three", "--wait")
        expect(effort(out) == "medium", f"a later plain send keeps effort medium, got {effort(out)}")

    def foreign_approval():
        # A thread agent-talk did not start, with approvals on: its approval request is left
        # pending, on send and on wait.
        with CodexDaemon() as d:
            params = {"cwd": str(DIR), "model": a.model, "approvalPolicy": "on-request", "sandbox": "read-only"}
            resp = d.call("thread/start", {**params, "approvalsReviewer": "user"})
            expect("error" not in resp, "thread/start over the daemon")
            tid = resp["result"]["thread"]["id"]
            track("codex", tid)
            # A thread without a turn cannot be resumed; give it one.
            first = [{"type": "text", "text": "reply with the single word ok", "text_elements": []}]
            expect("error" not in d.call("turn/start", {"threadId": tid, "input": first}), "turn/start")
            end = time.monotonic() + 120
            while not any(m.get("method") == "turn/completed" for m in d.seen) and time.monotonic() < end:
                d.drain(1)
            prompt = (
                "run `echo approval-test` with your shell tool using sandbox_permissions=require_escalated and "
                "justification='agent-talk live check', then reply done. If the request is declined, reply declined."
            )
            h = f"codex:{tid}"
            out = cli("send", h, prompt, "--wait", "--timeout", "40", expect_exit=(0, 3), during=d.drain)
            if not err(out).get("approvals"):
                raise Inconclusive("no approval request (the model did not escalate)")
            expect(code(out) == "E_TIMEOUT" and err(out).get("state") == "waiting", "E_TIMEOUT, state waiting")
            expect(err(out)["approvals"][0]["outcome"] == "pending", "send: the request is pending")
            receipt = err(out)["receipt"]["receipt_id"]
            out = cli("wait", h, "--receipt", receipt, "--timeout", "3", expect_exit=3, during=d.drain)
            expect(err(out).get("state") == "waiting", "wait: state waiting")
            d.drain(1)
            requests = [m for m in d.seen if "id" in m and m["method"].endswith("requestApproval")]
            expect(requests, "the harness connection saw the request")
            resolved = any(m.get("method") == "serverRequest/resolved" for m in d.seen)
            expect(not resolved, "nobody answered it")
            d.call("turn/interrupt", {"threadId": tid, "turnId": requests[0]["params"]["turnId"]})

    def ls_pages():
        need("codex")
        args = ("ls", "--agent", "codex", "--cwd", str(DIR), "--limit", "2")
        p1 = cli(*args)
        cursor = p1["next_cursors"].get("codex")
        expect(cursor, "the first page has a cursor")
        p2 = cli(*args, "--cursor", cursor)
        h1, h2 = {s["handle"] for s in p1["sessions"]}, {s["handle"] for s in p2["sessions"]}
        expect(h2 and not h1 & h2, f"pages are disjoint: {h1} / {h2}")

    for name, fn in (
        ("busy", busy),
        ("dormant", dormant),
        ("settings", settings),
        ("foreign-approval", foreign_approval),
        ("ls-pages", ls_pages),
    ):
        check("codex", name, fn)


def codex_unloaded():
    """Run last: thread A unloads about 60 s after its last subscriber left. Another app-server
    process that holds it makes send refuse before recording anything; once that process is gone,
    send resumes the unloaded thread."""

    def foreign_writer():
        h = need("codex")
        wait_for(lambda: ls_row("codex", h)["observations"]["loaded"] == "no", 180, "thread unloaded", step=10)
        # `codex` may be a launcher whose child is the server: kill the process group.
        proc = st["codex_proc"] = subprocess.Popen(
            ["codex", "app-server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=BASE_ENV,
            start_new_session=True,
        )

        def call(rid, method, params):
            proc.stdin.write(json.dumps({"id": rid, "method": method, "params": params}) + "\n")
            proc.stdin.flush()
            while "method" in (msg := json.loads(proc.stdout.readline())) or msg.get("id") != rid:
                pass
            return msg

        call(1, "initialize", {"clientInfo": Mcp.CLIENT, "capabilities": {"experimentalApi": True}})
        proc.stdin.write(json.dumps({"method": "initialized", "params": {}}) + "\n")
        proc.stdin.flush()
        resp = call(2, "thread/resume", {"threadId": native(h), "excludeTurns": True})
        expect("error" not in resp, "a standalone app-server resumed the thread")
        out = cli("send", h, "x", expect_exit=2)
        expect(code(out) == "E_FOREIGN_LIVE", "E_FOREIGN_LIVE")
        expect(not err(out).get("receipt"), "refused before the intent")
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        r = cli("send", h, "reply with the single word plum")["receipt"]
        out = cli("wait", h, "--receipt", r["receipt_id"], "--timeout", "120")
        expect(out["turn"]["status"] == "completed" and "plum" in final(out), "send resumes the thread ~ plum")

    check("codex", "foreign-writer", foreign_writer)


# ---------------------------------------------------------------- claude


def claude_transcript(session_id: str) -> Path | None:
    # Found by id: the project slug also maps '.' to '-', so it is not rebuilt from the cwd.
    return next((Path.home() / ".claude/projects").glob(f"*/{session_id}.jsonl"), None)


def claude_tier():
    a = AGENTS["claude"]
    shared(a)

    def settings():
        # Claude keeps neither effort nor permission mode across --resume: agent-talk passes
        # the session's settings again on every send.
        h = need("claude")

        def ran(out) -> tuple:
            lines = [json.loads(l) for l in claude_transcript(native(h)).read_text().splitlines()]
            effort = next(e.get("effort") for e in reversed(lines) if e.get("type") == "assistant")
            return effort, run_init("claude", out["receipt"]["receipt_id"]).get("permissionMode")

        out = cli("send", h, "reply with the single word low", "--effort", "low", "--full-access", "--wait")
        expect(ran(out) == ("low", "bypassPermissions"), f"the changed turn: low, bypass, got {ran(out)}")
        out = cli("send", h, "reply with the single word kept", "--wait")
        expect(ran(out) == ("low", "bypassPermissions"), f"a later plain send: low, bypass, got {ran(out)}")
        out = cli("send", h, "x", "--steer", "--effort", "medium", expect_exit=2)
        expect(code(out) == "E_PRECONDITION", "a settings change with --steer: E_PRECONDITION")

    def foreign_process():
        # A `claude -p` agent-talk did not start: send is refused while it lives, and wait
        # follows its turn.
        sid, prompt_id = str(uuid.uuid4()), str(uuid.uuid4())
        track("claude", sid)
        log = T / "claude-foreign.ndjson"
        with open(log, "w") as fh:
            proc = st["claude_proc"] = subprocess.Popen(
                ["claude", "-p", "--verbose", "--input-format", "stream-json", "--output-format", "stream-json"]
                + ["--session-id", sid, "--model", a.model],
                cwd=DIR,
                stdin=subprocess.PIPE,
                stdout=fh,
                stderr=subprocess.DEVNULL,
                text=True,
                env=BASE_ENV,
            )
        line = {"type": "user", "uuid": prompt_id, "message": {"role": "user", "content": "reply with the word ok"}}
        proc.stdin.write(json.dumps(line) + "\n")
        proc.stdin.flush()
        wait_for(lambda: '"type":"result"' in log.read_text(), 60, "the foreign claude -p wrote its result")
        h = f"claude:{sid}"
        row = ls_row("claude", h)
        expect(row and row["observations"]["loaded"] == "yes" and not row["owned"], "ls: loaded, not owned")
        before = claude_transcript(sid).read_text()
        out = cli("send", h, "x", expect_exit=2)
        expect(code(out) == "E_FOREIGN_LIVE", "E_FOREIGN_LIVE")
        expect(claude_transcript(sid).read_text() == before, "the transcript is unchanged")
        out = cli("wait", h, "--turn", prompt_id, "--timeout", "3", expect_exit=3)
        expect(err(out).get("state") == "running", "wait: running while the process lives")
        proc.stdin.close()
        proc.wait(timeout=30)
        out = cli("wait", h, "--turn", prompt_id, "--timeout", "10")
        expect(out["turn"]["status"] == "completed", "wait: completed once it exits")

    for name, fn in (("settings", settings), ("foreign-process", foreign_process)):
        check("claude", name, fn)


# ---------------------------------------------------------------- opencode


def opencode_api(method: str, path: str, body: dict | None = None) -> dict:
    """A request to the user's OpenCode service; the password is read from service.json each time."""
    state = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "opencode/service.json"
    reg = json.loads(state.read_text())
    auth = base64.b64encode(f"opencode:{reg['password']}".encode()).decode()
    req = urllib.request.Request(
        f"{reg['url']}{path}",
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"Basic {auth}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        data = r.read()
    LAST.update(cmd=f"opencode {method} {path}", exit="-", out=data[:1000].decode(errors="replace"))
    return json.loads(data) if data.strip() else {}


def opencode_tier():
    a = AGENTS["opencode"]
    shared(a)

    def steer():
        # Steer joins the running execution: the steered message and the one it joined end
        # with the same reply.
        h = need("opencode")
        r1 = cli("send", h, COUNT)["receipt"]["receipt_id"]
        wait_for(lambda: ls_row("opencode", h)["state"] == "running", 20, "the session running")
        out = cli("send", h, "stop counting and reply with the single word steered", "--steer", "--wait")
        expect(out["turn"]["status"] == "completed", "the steered message completed")
        joined = cli("wait", h, "--receipt", r1, "--timeout", "120")
        expect(joined["turn"]["final_text"] == out["turn"]["final_text"], "both end with the same reply")

    def settings():
        # The session itself changes: the variant for this and later prompts, the rules to
        # allow everything.
        out = new(a, "reply with the single word plum", "--wait")
        h = out["handle"]
        efforts = next((m["efforts"] for m in cli("models", "opencode", a.model)["models"] if m["id"] == a.model), [])
        effort = next((e for e in efforts if e not in ("none", "default")), None)
        if not effort:
            raise Inconclusive(f"models lists no variant of {a.model}")

        def variants(out) -> set:
            raw = cli("read", h, "--raw", "--limit", "10")["raw"]
            at = next(i for i, r in enumerate(raw) if r["id"] == out["turn"]["turn_id"])
            return {(r.get("model") or {}).get("variant") for r in raw[at + 1 :] if r["type"] == "assistant"}

        out = cli("send", h, "reply with the single word pear", "--effort", effort, "--wait")
        expect(variants(out) == {effort}, f"the changed turn runs variant {effort}")
        out = cli("send", h, "reply with the single word fig", "--full-access", "--wait")
        expect(variants(out) == {effort}, f"a later send keeps variant {effort}")
        rules = opencode_api("GET", f"/api/session/{native(h)}")["data"].get("permissions") or []
        allow_all = {"action": "*", "resource": "*", "effect": "allow"}
        expect(allow_all in rules, f"the session's rules allow everything, got {rules}")

    def foreign_approval():
        # A session agent-talk did not start whose rules ask before an edit: agent-talk leaves
        # the request pending, on send and on wait.
        provider, model = a.model.split("/", 1)
        session = {
            "location": {"directory": str(DIR)},
            "model": {"providerID": provider, "id": model},
            "permissions": [{"action": "edit", "resource": "*", "effect": "ask"}],
        }
        sid = opencode_api("POST", "/api/session", session)["data"]["id"]
        track("opencode", sid)
        h, target = f"opencode:{sid}", DIR / "oc-foreign.txt"
        target.unlink(missing_ok=True)
        out = cli("send", h, f"create {target} with your write tool", "--wait", "--timeout", "30", expect_exit=(0, 3))
        if not err(out).get("approvals"):
            raise Inconclusive("no approval request (the model did not use its write tool)")
        expect(code(out) == "E_TIMEOUT" and err(out).get("state") == "waiting", "E_TIMEOUT, state waiting")
        expect(err(out)["approvals"][0]["outcome"] == "pending", "send: the request is pending")
        receipt = err(out)["receipt"]["receipt_id"]
        out = cli("wait", h, "--receipt", receipt, "--timeout", "3", expect_exit=3)
        expect(err(out).get("state") == "waiting", "wait: state waiting")
        pending = opencode_api("GET", f"/api/session/{sid}/permission").get("data") or []
        expect(pending, "the request is still pending at the service")
        # End the turn with a reject, the only answer the harness gives.
        reply = {"decision": "reject", "message": "agent-talk live harness"}
        opencode_api("POST", f"/api/session/{sid}/permission/{pending[0]['id']}/reply", reply)
        out = cli("wait", h, "--receipt", receipt, "--timeout", "120")
        expect(out["turn"]["status"] == "completed" and not target.exists(), "completed, file absent")

    def mcp():
        # send over MCP: the sender from _meta, the result as structured content.
        h = need("opencode")
        caller = "opencode:ses_harnesscaller"
        with Mcp() as m:
            args = {"handle": h, "text": "reply with the single word plum", "wait": True}
            is_err, out = m.call("send", args, {"ai.opencode/sessionID": native(caller)})
        expect(not is_err and out["from"].get("session") == caller, f"from.session == {caller}")
        expect("plum" in final(out), "final ~ plum")

    for name, fn in (("steer", steer), ("settings", settings), ("foreign-approval", foreign_approval), ("mcp", mcp)):
        check("opencode", name, fn)


# ---------------------------------------------------------------- grok


def grok_session_file(session_id: str, name: str) -> Path | None:
    return next((GROK_HOME / "sessions").glob(f"*/{session_id}/{name}"), None)


def grok_yolo(session_id: str) -> list:
    """`yolo_mode` of each turn the session started, in order."""
    path = grok_session_file(session_id, "events.jsonl")
    lines = path.read_text().splitlines() if path else []
    return [e.get("yolo_mode") for e in map(json.loads, lines) if e.get("type") == "turn_started"]


def grok_tier():
    a = AGENTS["grok"]
    GROK_HOME.mkdir(exist_ok=True)
    shared(a)

    def settings():
        # Effort is stored with the session; full access passes again on every direct-mode load.
        out = new(a, "reply with the single word one", "--wait")
        h = st["grok_settings"] = out["handle"]
        sid = native(h)
        cli("send", h, "reply with the single word two", "--effort", "low", "--full-access", "--wait", env=a.env)
        summary = json.loads(grok_session_file(sid, "summary.json").read_text())
        expect(summary.get("reasoning_effort") == "low", f"effort low, got {summary.get('reasoning_effort')}")
        cli("send", h, "reply with the single word three", "--wait", env=a.env)
        yolo = grok_yolo(sid)
        expect(yolo == [False, True, True], f"yolo off, then on for the changed and the later turn, got {yolo}")

    def foreign_tui():
        # A live TUI row names the session: send is refused; a row whose pid is dead is not.
        h = need("grok_settings")
        sid = native(h)
        sleeper = subprocess.Popen(["sleep", "300"])
        try:
            row = {"session_id": sid, "pid": sleeper.pid, "cwd": str(DIR), "opened_at": "2026-10-05T00:00:00Z"}
            (GROK_HOME / "active_sessions.json").write_text(json.dumps([row]))
            before = grok_session_file(sid, "updates.jsonl").read_text()
            out = cli("send", h, "x", env=a.env, expect_exit=2)
            expect(code(out) == "E_FOREIGN_LIVE", "a live TUI row: E_FOREIGN_LIVE")
            expect(grok_session_file(sid, "updates.jsonl").read_text() == before, "updates.jsonl unchanged")
            expect(ls_row("grok", h, env=a.env)["observations"]["loaded"] == "yes", "ls: loaded")
        finally:
            sleeper.kill()
            sleeper.wait()
        out = cli("send", h, "reply with the single word four", "--wait", env=a.env)
        expect(out["turn"]["status"] == "completed", "a row with a dead pid: completed")

    for name, fn in (("settings", settings), ("foreign-tui", foreign_tui)):
        check("grok", name, fn)


# ---------------------------------------------------------------- antigravity

AGY_HOME = Path.home() / ".gemini/antigravity-cli"


def agy_lock_held(conv: str) -> bool:
    try:
        with open(AGY_HOME / "presence" / f"{conv}.lock", "rb") as f:
            try:
                fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
            fcntl.flock(f, fcntl.LOCK_UN)
            return False
    except FileNotFoundError:
        return False


def antigravity_tier():
    a = AGENTS["antigravity"]
    shared(a)

    def settings():
        # agy keeps nothing across runs: every send passes the model and full access again.
        h = need("antigravity")

        def ran(out) -> tuple:
            init = run_init("antigravity", out["receipt"]["receipt_id"])
            return init.get("model"), init.get("permission_mode")

        want = (a.model, "always-proceed")
        out = cli("send", h, "reply with the single word two", "--full-access", "--wait")
        expect(ran(out) == want, f"the changed run: {want}, got {ran(out)}")
        out = cli("send", h, "reply with the single word three", "--wait")
        expect(ran(out) == want, f"a later plain send: {want}, got {ran(out)}")

    def unknown():
        # agy would silently start a new conversation for an unknown id.
        before = len(list((AGY_HOME / "conversations").glob("*.db")))
        out = cli("send", f"antigravity:{uuid.uuid4()}", "x", "--wait", expect_exit=2)
        expect(code(out) == "E_PRECONDITION", "unknown conversation: E_PRECONDITION")
        expect(len(list((AGY_HOME / "conversations").glob("*.db"))) == before, "no conversation created")

    def killed():
        # agent-talk's child dies mid-turn: the turn is not reported completed.
        h = need("antigravity")
        p = subprocess.Popen(
            [B, "send", h, COUNT, "--json"], env=BASE_ENV, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True
        )
        runs = Path.home() / ".agent-talk/antigravity-runs"
        t0, child = time.time(), []

        def taken():
            kids = subprocess.run(["pgrep", "-P", str(p.pid)], capture_output=True, text=True).stdout.split()
            logs = [f for f in runs.glob("*.ndjson") if f.stat().st_mtime >= t0 - 1]
            if kids and logs and '"user_input"' in logs[0].read_text():
                child[:] = [int(kids[0]), logs[0]]
            return bool(child)

        wait_for(taken, 60, "agy took the message")
        finished = '"event":"result"' in child[1].read_text()
        os.kill(child[0], signal.SIGKILL)
        stdout, _ = p.communicate(timeout=60)
        LAST.update(cmd="agent-talk send (child killed)", exit=p.returncode, out=stdout[-1000:])
        expect(p.returncode == 0, "send returns the receipt")
        if finished:
            raise Inconclusive("the turn finished before the kill")
        rec = json.loads(stdout)["receipt"]
        out = cli("wait", h, "--turn", rec["turn_id"], "--timeout", "10", expect_exit=(0, 3))
        expect((out.get("turn") or {}).get("status") != "completed", "the killed turn is not completed")

    def foreign_process():
        # An agy process agent-talk did not start holds the conversation: send is refused, and
        # wait follows the turn it runs.
        h = need("antigravity")
        conv = native(h)
        log = T / "agy-foreign.ndjson"
        with open(log, "w") as fh:
            proc = st["agy_proc"] = subprocess.Popen(
                ["agy", "-p", "", "--input-format", "stream-json", "--output-format", "stream-json"]
                + ["--model", a.model, "--effort", "low", "--conversation", conv],
                cwd=DIR,
                stdin=subprocess.PIPE,
                stdout=fh,
                stderr=subprocess.DEVNULL,
                text=True,
                env=BASE_ENV,
            )
        wait_for(lambda: agy_lock_held(conv), 30, "the foreign agy process took the presence lock")
        out = cli("send", h, "x", expect_exit=2)
        expect(code(out) == "E_FOREIGN_LIVE", "E_FOREIGN_LIVE")
        expect(ls_row("antigravity", h)["observations"]["loaded"] == "yes", "ls: loaded")
        line = {"event": "user", "message": {"content": "reply with the single word foreign"}}
        proc.stdin.write(json.dumps(line) + "\n")
        proc.stdin.flush()
        wait_for(lambda: '"event":"result"' in log.read_text(), 60, "the foreign turn wrote its result")
        step = next(
            json.loads(l)["step_update"]["step_index"] for l in log.read_text().splitlines() if "user_input" in l
        )
        out = cli("wait", h, "--turn", str(step), "--timeout", "3", expect_exit=3)
        expect(err(out).get("state") == "running", "wait: running while the process holds the lock")
        proc.stdin.close()
        proc.wait(timeout=60)
        out = cli("wait", h, "--turn", str(step), "--timeout", "10")
        expect(out["turn"]["status"] == "completed" and "foreign" in final(out), "wait: completed once it exits")
        out = cli("send", h, "reply with the single word back", "--wait")
        expect("back" in final(out), "send works once the lock is free")

    for name, fn in (("settings", settings), ("unknown", unknown), ("killed", killed), ("foreign", foreign_process)):
        check("antigravity", name, fn)


# ---------------------------------------------------------------- pi

PI_SESSIONS = Path(os.environ.get("PI_CODING_AGENT_DIR") or Path.home() / ".pi/agent") / "sessions"


def pi_session_file(session_id: str) -> Path | None:
    return next(PI_SESSIONS.glob(f"*/*_{session_id}.jsonl"), None)


def pi_messages(session_id: str) -> list[dict]:
    path = pi_session_file(session_id)
    entries = [json.loads(l) for l in path.read_text().splitlines() if l.strip()] if path else []
    return [e["message"] for e in entries if e.get("type") == "message"]


def pi_tier():
    a = AGENTS["pi"]
    shared(a)

    def settings():
        # pi does not restore the thinking level on resume: agent-talk passes it again.
        h = need("pi")

        def level() -> str:
            return next(m.get("thinkingLevel") for m in reversed(pi_messages(native(h))) if m["role"] == "assistant")

        cli("send", h, "reply with the single word high", "--effort", "high", "--wait")
        expect(level() == "high", f"the changed turn runs at high, got {level()}")
        cli("send", h, "reply with the single word kept", "--wait")
        expect(level() == "high", f"a later plain send keeps high, got {level()}")

    def foreign_session():
        # A session pi wrote on its own: wait reads its turn from the file, send resumes it.
        sid = str(uuid.uuid4())
        track("pi", sid)
        p = subprocess.run(
            ["pi", "--mode", "json", "--model", a.model, "--thinking", "low", "--session-id", sid],
            cwd=DIR,
            input="reply with the single word ok",
            capture_output=True,
            text=True,
            env=BASE_ENV,
            timeout=120,
        )
        expect(p.returncode == 0, f"pi exited {p.returncode}: {p.stderr[-300:]}")
        path = pi_session_file(sid)
        entries = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
        prompt = next(e["id"] for e in entries if e.get("type") == "message" and e["message"]["role"] == "user")
        h = f"pi:{sid}"
        out = cli("wait", h, "--turn", prompt, "--timeout", "10")
        expect(out["turn"]["status"] == "completed" and "ok" in final(out), "wait reads the turn ~ ok")
        out = cli("send", h, "reply with the single word two", "--wait")
        expect("two" in final(out), "send resumes the session ~ two")
        msgs = cli("read", h, "--limit", "4")["messages"]
        expect([m["phase"] for m in msgs] == ["prompt", "final", "prompt", "final"], "read: both turns")
        row = ls_row("pi", h)
        expect(row and not row["owned"] and row["observations"]["origin"] == "pi", "ls: not owned, origin pi")

    for name, fn in (("settings", settings), ("foreign-session", foreign_session)):
        check("pi", name, fn)


# ---------------------------------------------------------------- run


def preconditions() -> dict[str, str | None]:
    """agent -> None when new and send can run, else the reasons `status` gives."""
    out = json.loads(subprocess.run([B, "status", "--json"], capture_output=True, text=True, env=BASE_ENV).stdout)
    reasons = {}
    for p in out["agents"]:
        ops = p["operations"]
        blocked = [f"{o['name']}: {o['reason']}" for o in ops if o["name"] in ("new", "send") and not o["available"]]
        reasons[p["agent"]] = "; ".join(blocked) or None
    return reasons


def cleanup():
    """Remove what the run created, never anything else: Codex threads are archived, OpenCode
    and Grok sessions deleted through the agent, Claude and pi session files and agent-talk's run
    logs unlinked. agy has no delete command, so its conversations stay."""
    if ids := [sid for sid, _ in created.get("codex", [])]:
        try:
            with CodexDaemon() as d:
                for tid in ids:
                    resp = d.call("thread/archive", {"threadId": tid})
                    print(f"cleanup: archive codex thread {tid}: {(resp.get('error') or {}).get('message', 'ok')}")
        except Exception as e:  # report and go on with the rest
            print(f"cleanup: could not archive codex threads: {e}")
    for sid, _ in created.get("opencode", []):
        try:
            opencode_api("DELETE", f"/api/session/{sid}")
            print(f"cleanup: deleted opencode session {sid}")
        except Exception as e:  # report and go on with the rest
            print(f"cleanup: could not delete opencode session {sid}: {e}")
    for sid, env in created.get("grok", []):
        p = subprocess.run(["grok", "sessions", "delete", sid], env=env_with(env), capture_output=True, text=True)
        print(f"cleanup: grok sessions delete {sid}: {(p.stdout or p.stderr).strip()}")
    for agent, find in (("claude", claude_transcript), ("pi", pi_session_file)):
        for sid, _ in created.get(agent, []):
            if path := find(sid):
                path.unlink()
                print(f"cleanup: deleted {path}")
    for sid, _ in created.get("antigravity", []):
        print(f"cleanup: antigravity conversation {sid} stays (agy has no delete command)")
    for agent in ("claude", "antigravity", "pi"):
        runs = Path.home() / f".agent-talk/{agent}-runs"
        for rid in set(receipts.get(agent, [])):
            for suffix in (".ndjson", ".stderr"):
                (runs / f"{rid}{suffix}").unlink(missing_ok=True)


def stop_helpers():
    for key in ("claude_proc", "agy_proc"):
        if (proc := st.get(key)) and proc.poll() is None:
            proc.kill()
            proc.wait()
    if proc := st.get("codex_proc"):  # the group outlives its leader when `codex` is a launcher
        with contextlib.suppress(ProcessLookupError):
            os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()


TIERS = {
    "offline": offline,
    "codex": codex_tier,
    "claude": claude_tier,
    "opencode": opencode_tier,
    "grok": grok_tier,
    "antigravity": antigravity_tier,
    "pi": pi_tier,
}


def main():
    args = sys.argv[1:]
    keep = "--keep" in args
    tiers = [a for a in args if a != "--keep"]
    if tiers == ["all"]:
        tiers = list(TIERS)
    if not tiers or set(tiers) - set(TIERS):
        sys.exit(f"usage: uv run tests/live.py ({'|'.join(TIERS)} ... | all) [--keep]")

    start_time = time.monotonic()
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=REPO, check=True)
    DIR.mkdir(parents=True, exist_ok=True)
    reasons = preconditions()
    run = []
    for tier in TIERS:
        if tier not in tiers:
            continue
        if reasons.get(tier):
            print(f"SKIP {tier}: {reasons[tier]}")
            RESULTS.append(("SKIP", tier))
        else:
            run.append(tier)
    try:
        for tier in run:
            TIERS[tier]()
        if "codex" in run:  # last, so that the thread has unloaded meanwhile
            codex_unloaded()
    finally:
        stop_helpers()
        if keep:
            print(f"--keep: left {created}")
        else:
            cleanup()

    counts = {s: sum(1 for r, _ in RESULTS if r == s) for s in ("PASS", "FAIL", "SKIP", "INCONCLUSIVE")}
    print(" ".join(f"{k} {v}" for k, v in counts.items()) + f"  wall {time.monotonic() - start_time:.0f}s")
    sys.exit(0 if counts["PASS"] == len(RESULTS) else 1)


if __name__ == "__main__":
    main()
