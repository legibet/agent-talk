# /// script
# requires-python = ">=3.12"
# dependencies = ["jsonschema>=4", "websockets>=14"]
# ///
"""Live regression harness for agent-talk.

Runs the real `agent-talk` binary against the installed agents (Codex daemon, `claude -p`,
OpenCode service, `grok agent stdio`, `agy -p`, `pi --mode json`) on cheap models (Grok and Antigravity have no
cheaper model than grok-4.7 / gemini-3.8-flash) and checks the behavior documented in DESIGN.md.

    uv run tests/live.py (offline|codex|claude|opencode|grok|antigravity|pi ... | all) [--keep]

Models: LIVE_CODEX_MODEL, LIVE_CLAUDE_MODEL, LIVE_OPENCODE_MODEL, LIVE_GROK_MODEL,
LIVE_ANTIGRAVITY_MODEL, LIVE_PI_MODEL override the defaults below. Exit 0 only when every requested check ran
and passed.
"""

import base64
import contextlib
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
from pathlib import Path

import jsonschema
from websockets.sync.client import unix_connect

REPO = Path(__file__).resolve().parent.parent
B = str(REPO / "target/release/agent-talk")
DIR = REPO / "target/live-work"
CMD_TIMEOUT = 900  # seconds a single agent-talk command may take before the check fails
T = tempfile.mkdtemp(prefix="agent-talk-live-")

CODEX_MODEL = os.environ.get("LIVE_CODEX_MODEL", "gpt-6-luna")
CLAUDE_MODEL = os.environ.get("LIVE_CLAUDE_MODEL", "sonnet")
OPENCODE_MODEL = os.environ.get("LIVE_OPENCODE_MODEL", "deepseek/deepseek-flash")
GROK_MODEL = os.environ.get("LIVE_GROK_MODEL", "grok-4.7")
ANTIGRAVITY_MODEL = os.environ.get("LIVE_ANTIGRAVITY_MODEL", "gemini-3.8-flash")
PI_MODEL = os.environ.get("LIVE_PI_MODEL", "deepseek/deepseek-flash")

BASE_ENV = {
    k: v
    for k, v in os.environ.items()
    if k
    not in (
        "CLAUDE_CODE_SESSION_ID",
        "CODEX_THREAD_ID",
        "OPENCODE_SESSION_ID",
        "GROK_SESSION_ID",
        "ANTIGRAVITY_CONVERSATION_ID",
        "PI_SESSION_ID",
        "AGENT_TALK_CALLER",
    )
}

ESCALATION_PROMPT = (
    "run `echo approval-test` with your shell tool using sandbox_permissions=require_escalated "
    "and justification='agent-talk live check', then reply done. If the request is declined, do not "
    "retry and reply declined."
)

RESULTS: list[tuple[str, str]] = []  # (status, name)
LAST: dict = {}  # last agent-talk command, for FAIL output
st: dict = {}  # state shared between checks (handles, receipts, turn ids)
created = {
    "codex": [],
    "opencode": [],
    "claude_sessions": [],
    "grok": [],
    "antigravity": [],
    "pi_sessions": [],
    # receipts whose run logs under ~/.agent-talk/<agent>-runs the run created
    "claude_receipts": [],
    "antigravity_receipts": [],
    "pi_receipts": [],
}


class Fail(Exception):
    pass


class Skip(Exception):
    pass


class Inconclusive(Exception):
    pass


def expect(cond, what: str):
    if not cond:
        raise Fail(what)


def need(*keys):
    for k in keys:
        if k not in st:
            raise Skip(f"needs {k} from an earlier check")


def excerpt(v, limit=1500) -> str:
    def strip(x):
        if isinstance(x, dict):
            return {k: strip(val) for k, val in x.items() if k != "raw"}
        if isinstance(x, list):
            return [strip(i) for i in x]
        return x

    s = json.dumps(strip(v), ensure_ascii=False)
    return s if len(s) <= limit else s[:limit] + " ..."


def env_with(extra: dict | None) -> dict:
    e = dict(BASE_ENV)
    e.update(extra or {})
    return e


def cli(*args, env=None, expect_exit=0, during=None) -> dict:
    """Run `agent-talk <args> --json`, parse stdout, assert the exit code (int or tuple).
    `during` is called repeatedly while the command runs."""
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
        cmd=" ".join(([f"{k}={v}" for k, v in (env or {}).items()]) + cmd[1:]),
        exit=p.returncode,
        out=out,
        stderr=stderr[-500:],
    )
    # remember the run logs the harness created
    for r in (out.get("receipt"), (out.get("error") or {}).get("receipt")):
        agent = str(r.get("handle", "")).split(":", 1)[0] if r else ""
        if agent in ("claude", "antigravity", "pi") and args and args[0] in ("new", "send"):
            created[f"{agent}_receipts"].append(r["receipt_id"])
        if agent == "codex" and args and args[0] == "new":
            created["codex"].append(native(r["handle"]))
    allowed = expect_exit if isinstance(expect_exit, tuple) else (expect_exit,)
    if p.returncode not in allowed:
        raise Fail(f"exit {p.returncode}, expected {expect_exit}")
    return out


def err(out: dict) -> dict:
    return out.get("error") or {}


def final(out: dict) -> str:
    return ((out.get("turn") or {}).get("final_text") or "").lower()


def native(handle: str) -> str:
    return handle.split(":", 1)[1]


def paged(handle: str, limit: int) -> list[dict]:
    """Every message of the session, oldest first, read `limit` at a time from the newest."""
    messages, seen = [], set()
    out = cli("read", handle, "--all", "--limit", str(limit))
    while True:
        messages = out["messages"] + messages
        cur = out.get("next_cursor")
        if not cur or cur in seen:
            return messages
        seen.add(cur)
        out = cli("read", handle, "--all", "--limit", str(limit), "--cursor", cur)


def check(agent: str, name: str, fn):
    label = f"{agent}/{name}"
    t0 = time.monotonic()
    LAST.clear()
    try:
        fn()
        status, detail = "PASS", ""
    except Fail as e:
        status = "FAIL"
        detail = f"  {e}"
        if LAST:
            detail += f"\n  cmd: {LAST['cmd']}\n  exit: {LAST['exit']}\n  json: {excerpt(LAST['out'])}"
            if LAST.get("stderr"):
                detail += f"\n  stderr: {LAST['stderr'].strip()}"
    except Inconclusive as e:
        status, detail = "INCONCLUSIVE", f"  {e}"
    except Skip as e:
        status, detail = "SKIP", f"  {e}"
    secs = time.monotonic() - t0
    print(f"{status} {label} {secs:.1f}s", flush=True)
    if detail:
        print(detail, flush=True)
    RESULTS.append((status, label))


class Mcp:
    """`agent-talk mcp` over stdio, one JSON-RPC line per message. Protocol 2026-07-28 (what Claude
    Code speaks) has no handshake and carries version and client info in every request's
    `_meta`; older versions (Codex 0.160: 2025-06-18) start with `initialize`."""

    CLIENT = {"name": "agent-talk-live", "version": "0"}

    def __init__(self, env: dict | None = None, extra_args=(), protocol="2026-07-28"):
        self.p = subprocess.Popen(
            [B, "mcp", *extra_args],
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
        """tools/call; returns (isError, parsed inner JSON). JSON-RPC errors fail."""
        params = {"name": tool, "arguments": args}
        if meta is not None:
            params["_meta"] = meta
        resp = self.request("tools/call", params)
        LAST.clear()
        LAST.update(cmd=f"mcp tools/call {tool} {json.dumps(args)} meta={meta}", exit="-", out=resp)
        expect("error" not in resp, "JSON-RPC-level error")
        result = resp["result"]
        inner = json.loads(result["content"][0]["text"])
        if not result.get("isError"):
            expect(result.get("structuredContent") == inner, "structuredContent equals the text content")
            if not self.schemas:
                self.schemas = {t["name"]: t["outputSchema"] for t in self.request("tools/list", {})["result"]["tools"]}
            try:
                jsonschema.validate(inner, self.schemas[tool])
            except jsonschema.ValidationError as e:
                expect(
                    False, f"structuredContent matches {tool}'s outputSchema: {e.message} at {list(e.absolute_path)}"
                )
        return bool(result.get("isError")), inner

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.kill()


# ---------------------------------------------------------------- offline


def seed_max_hops(handle: str) -> None:
    """Record a message at the hop limit (3) delivered to `handle` in the offline store, so
    whatever `handle` sends next is refused with E_MAX_HOPS before any agent is contacted."""
    db = sqlite3.connect(Path(T) / ".agent-talk/store.db")
    with contextlib.closing(db), db:
        rid = str(uuid.uuid4())
        db.execute(
            "INSERT INTO intents (receipt_id, handle, client_msg_id, text, depth) VALUES (?, ?, ?, 'x', 3)",
            (rid, handle, rid),
        )
        db.execute("INSERT INTO receipts (receipt_id, state) VALUES (?, 'accepted')", (rid,))


def offline():
    def o1():
        out = cli("ls", "--agent", "codex", env={"HOME": T}, expect_exit=2)
        expect(err(out).get("code") == "E_NO_DAEMON", "E_NO_DAEMON")

    def o2():
        # All agents unreachable: the unavailable ones are reported per agent, the others still answer.
        out = cli("ls", "--limit", "1", env={"HOME": T, "XDG_STATE_HOME": T})
        expect(out.get("sessions") == [], "no sessions")
        errors = {e["agent"]: e["error"]["code"] for e in out.get("errors") or []}
        expect(
            errors == {"codex": "E_NO_DAEMON", "opencode": "E_NO_DAEMON"},
            f"errors == codex+opencode E_NO_DAEMON, got {errors}",
        )

    def o3():
        out = cli("ls", "--agent", "bogus", expect_exit=2)
        expect(err(out).get("code") == "E_UNSUPPORTED", "ls --agent bogus: E_UNSUPPORTED")
        out = cli("read", "nohandle", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "read nohandle: E_PRECONDITION")
        out = cli("ls", "--cursor", "x", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "ls --cursor x: E_PRECONDITION")

    def o4():
        out = cli("send", f"claude:{uuid.uuid4()}", "x", "--steer", env={"HOME": T}, expect_exit=2)
        expect(err(out).get("code") == "E_NO_STEER", "E_NO_STEER")

    def o5():
        seed_max_hops("codex:x")
        out = cli("new", "codex", "hi", env={"HOME": T, "AGENT_TALK_CALLER": "codex:x"}, expect_exit=2)
        expect(err(out).get("code") == "E_MAX_HOPS", "E_MAX_HOPS (hop check before daemon contact)")

    def o6():
        out = cli("send", "codex:x", "hi", "--from", "bogus", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "send --from bogus: E_PRECONDITION")
        p = subprocess.run(
            [B, "mcp", "--caller", "bogus"], stdin=subprocess.DEVNULL, capture_output=True, text=True, env=BASE_ENV
        )
        LAST.update(
            cmd="agent-talk mcp --caller bogus </dev/null",
            exit=p.returncode,
            out={"stdout": p.stdout, "stderr": p.stderr},
        )
        expect(
            p.returncode == 2 and "E_PRECONDITION" in p.stdout + p.stderr, "mcp --caller bogus: E_PRECONDITION, exit 2"
        )

    def o7():
        home = {"HOME": T}
        for protocol in ("2025-06-18", "2026-07-28"):
            m = Mcp(home, protocol=protocol)
            try:
                tools = m.request("tools/list", {})["result"]["tools"]
            finally:
                m.close()
            names = {t["name"] for t in tools}
            expect(names == {"models", "ls", "new", "send", "read", "wait"}, f"({protocol}) tool names {names}")
            incomplete = [
                t["name"] for t in tools if not (t.get("title") and t.get("annotations") and t.get("outputSchema"))
            ]
            expect(
                not incomplete,
                f"({protocol}) every tool has title, annotations and outputSchema; missing on {incomplete}",
            )
        for h in ("codex:T", "opencode:ses_x", "claude:C"):
            seed_max_hops(h)
        m = Mcp(home)
        try:
            args = {"handle": "codex:x", "text": "hi"}
            for label, meta in (("a", {"threadId": "T"}), ("b", {"ai.opencode/sessionID": "ses_x"})):
                is_err, inner = m.call("send", args, meta)
                expect(is_err and err(inner).get("code") == "E_MAX_HOPS", f"({label}) E_MAX_HOPS")
            is_err, inner = m.call("send", args)
            expect(is_err and err(inner).get("code") == "E_NO_DAEMON", "(d) unknown sender: E_NO_DAEMON")
            is_err, inner = m.call("wait", {"handle": "codex:x"})
            expect(is_err and err(inner).get("code") == "E_PRECONDITION", "wait without turn/receipt: E_PRECONDITION")
        finally:
            m.close()
        m = Mcp({**home, "CLAUDE_CODE_SESSION_ID": "C"})
        try:
            is_err, inner = m.call("send", {"handle": "codex:x", "text": "hi"})
            expect(is_err and err(inner).get("code") == "E_MAX_HOPS", "(c) CLAUDE_CODE_SESSION_ID: E_MAX_HOPS")
        finally:
            m.close()

    for name, fn in (("O1", o1), ("O2", o2), ("O3", o3), ("O4", o4), ("O5", o5), ("O6", o6), ("O7", o7)):
        check("offline", name, fn)


# ---------------------------------------------------------------- codex


def codex_new(prompt: str, *extra) -> dict:
    return cli("new", "codex", "--cwd", str(DIR), "--model", CODEX_MODEL, *extra, prompt)


class CodexDaemon:
    """A raw connection to the Codex daemon socket (WebSocket over AF_UNIX, as agent-talk's
    transport). Server requests and notifications that arrive meanwhile are kept in `seen`;
    nothing here answers a server request."""

    def __init__(self):
        sock = Path.home() / ".codex/app-server-control/app-server-control.sock"
        self.stack = contextlib.ExitStack()
        self.ws = self.stack.enter_context(unix_connect(str(sock), uri="ws://localhost/"))
        self.next_id = 0
        self.seen: list[dict] = []
        self.call(
            "initialize",
            {"clientInfo": {"name": "agent-talk-live", "version": "0"}, "capabilities": {"experimentalApi": True}},
        )
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
            if "method" in msg:
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


def codex_interrupt(thread_id: str, turn_id: str) -> dict:
    """`turn/interrupt` from the daemon socket, subscribed first as the TUI would be; returns
    the response. Retried for a few seconds while the new thread is not resumable yet or the
    turn not yet active."""
    with CodexDaemon() as d:
        for method, params in (
            ("thread/resume", {"threadId": thread_id, "excludeTurns": True}),
            ("turn/interrupt", {"threadId": thread_id, "turnId": turn_id}),
        ):
            for _ in range(10):
                if "error" not in (resp := d.call(method, params)):
                    break
                time.sleep(1)
        return resp


def codex_turn_context(thread_id: str, turn_id: str) -> dict:
    """The settings a turn ran with, from the rollout's `turn_context` line (codex 0.160)."""
    for path in (Path.home() / ".codex/sessions").glob(f"*/*/*/rollout-*-{thread_id}.jsonl"):
        for line in path.read_text().splitlines():
            rec = json.loads(line)
            if rec.get("type") == "turn_context" and rec["payload"].get("turn_id") == turn_id:
                return rec["payload"]
    raise Fail(f"no turn_context for turn {turn_id} in the rollout of {thread_id}")


def turn_unfinished(handle: str, turn: str) -> bool:
    # Right after `new` the rollout is still empty (-32603 on thread/turns/list); a turn the
    # accepted receipt names must be observed, not refused.
    out = cli("wait", handle, "--turn", turn, "--timeout", "1", expect_exit=(0, 3))
    code = err(out).get("code")
    expect(code != "E_PRECONDITION", "wait on a receipt-named turn is not refused")
    return code == "E_TIMEOUT"


def codex_main():
    def c1():
        out = codex_new("reply with the single word kumquat", "--wait")
        st["A"] = out["handle"]
        r, t = out["receipt"], out["turn"]
        expect(r["state"] == "accepted", "receipt accepted")
        expect(r["turn_id"] == t["turn_id"], "receipt.turn_id == turn.turn_id")
        expect(t["status"] == "completed", "turn completed")
        expect("kumquat" in final(out), "final ~ kumquat")
        st["A_turn1"] = t["turn_id"]
        ctx = codex_turn_context(native(st["A"]), t["turn_id"])
        expect(
            ctx["approval_policy"] == "never", f"owned thread runs with approval never, got {ctx['approval_policy']}"
        )

    def c2():
        need("A")
        out = cli(
            "send",
            st["A"],
            "reply with the single word fig",
            "--wait",
            env={"CODEX_THREAD_ID": "T1", "OPENCODE_SESSION_ID": "o", "CLAUDE_CODE_SESSION_ID": "c"},
        )
        expect(out["from"].get("session") == "codex:T1", "from.session == codex:T1")
        expect(out["receipt"].get("queue_id"), "receipt.queue_id non-null")
        expect((out["receipt"].get("delivered_text") or "").startswith("[from codex:T1 "), "delivered_text header")
        expect(out["turn"]["turn_id"] != st["A_turn1"], "new turn id")
        expect("fig" in final(out), "final ~ fig")
        out = cli("read", st["A"], "--limit", "2")
        msgs = out["messages"]
        expect(len(msgs) == 2, "two messages")
        expect((msgs[0].get("from") or {}).get("session") == "codex:T1", "messages[0].from.session")
        expect(msgs[0]["text"].startswith("[from codex:T1 "), "messages[0] text starts with header")
        expect(msgs[1]["role"] == "assistant", "messages[1] is assistant")

    def c3():
        out = codex_new("run `sleep 15` with your shell tool, then reply done")
        st["B"], st["T0"] = out["handle"], out["receipt"]["turn_id"]
        expect(st["T0"], "receipt has turn id")

    def busy_guard():
        need("B", "T0")
        if st.get("c3_inconclusive"):
            raise Inconclusive("T0 finished early (model did not run sleep)")
        if not turn_unfinished(st["B"], st["T0"]):
            st["c3_inconclusive"] = True
            raise Inconclusive("T0 finished early (model did not run sleep)")

    def c3a():
        busy_guard()
        out = cli("send", st["B"], "also say STEERED", "--steer")
        expect(out["receipt"]["turn_id"] == st["T0"], "receipt.turn_id == T0")

    def c3b():
        busy_guard()
        out = cli("send", st["B"], "reply with the single word apple", "--wait", "--timeout", "3", expect_exit=3)
        expect(err(out).get("code") == "E_TIMEOUT", "E_TIMEOUT")
        rec = err(out).get("receipt") or {}
        expect(rec.get("receipt_id"), "error.receipt.receipt_id")
        st["RA"] = rec["receipt_id"]
        st["RA_turn"] = rec.get("turn_id")

    def c3c():
        busy_guard()
        need("RA")
        out = cli("wait", st["B"], "--receipt", st["RA"], "--timeout", "2", expect_exit=3)
        expect(err(out).get("state") == "pending", "error.state == pending")

    def c3d():
        need("B", "T0")
        out = cli("send", st["B"], "reply with the single word pear", "--wait")
        expect(out["turn"]["status"] == "completed", "completed")
        expect("pear" in final(out), "final ~ pear")
        st["pear_turn"] = out["turn"]["turn_id"]
        expect(st["pear_turn"] not in (st["T0"], st.get("RA_turn")), "pear turn differs from T0 and RA's turn")

    def c3e():
        need("RA")
        out = cli("wait", st["B"], "--receipt", st["RA"])
        expect(out["turn"]["status"] == "completed", "completed")
        expect("apple" in final(out), "final ~ apple")
        expect(out["turn"]["turn_id"] not in (st["T0"], st.get("pear_turn")), "RA turn differs from T0 and pear")

    def c3f():
        need("B", "T0")
        out = cli("wait", st["B"], "--turn", st["T0"], "--timeout", "5")
        expect(out["turn"]["status"] == "completed", "completed")
        st["c3_end"] = time.monotonic()

    def c3g():
        need("B", "T0")
        out = cli("send", st["B"], "x", "--steer", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "idle steer: E_PRECONDITION")
        expect((err(out).get("agent_error") or {}).get("code") == -32600, "agent_error.code == -32600")

    def c3h():
        need("B")
        out = cli("wait", st["B"], "--turn", "bogus", "--timeout", "5", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "E_PRECONDITION")

    def c4():
        # A thread agent-talk did not start, created over the daemon with approvals on: agent-talk
        # reports its approval request as pending and answers it neither on send nor on wait.
        with CodexDaemon() as d:
            resp = d.call(
                "thread/start",
                {
                    "cwd": str(DIR),
                    "model": CODEX_MODEL,
                    "approvalPolicy": "on-request",
                    "approvalsReviewer": "user",
                    "sandbox": "read-only",
                },
            )
            expect("error" not in resp, "thread/start over the daemon")
            tid = resp["result"]["thread"]["id"]
            created["codex"].append(tid)
            st["P"] = f"codex:{tid}"
            # A thread without a turn has no rollout and cannot be resumed; give it one.
            first = [{"type": "text", "text": "reply with the single word ok", "text_elements": []}]
            expect("error" not in d.call("turn/start", {"threadId": tid, "input": first}), "turn/start over the daemon")
            end = time.monotonic() + 120
            while not any(m.get("method") == "turn/completed" for m in d.seen) and time.monotonic() < end:
                d.drain(1)
            expect(any(m.get("method") == "turn/completed" for m in d.seen), "first turn completed within 120 s")
            out = cli(
                "send", st["P"], ESCALATION_PROMPT, "--wait", "--timeout", "40", expect_exit=(0, 3), during=d.drain
            )
            if not err(out):
                raise Inconclusive("turn completed without an approval request (model did not escalate)")
            expect(err(out).get("code") == "E_TIMEOUT", "E_TIMEOUT")
            if not err(out).get("approvals"):
                raise Inconclusive("no approval request within 40 s")
            expect(err(out).get("state") == "waiting", "error.state == waiting")
            expect(err(out)["approvals"][0]["outcome"] == "pending", "send: approval pending")
            receipt = err(out)["receipt"]["receipt_id"]
            out = cli("wait", st["P"], "--receipt", receipt, "--timeout", "3", expect_exit=3, during=d.drain)
            expect(err(out).get("state") == "waiting", "wait: error.state == waiting")
            expect(all(a["outcome"] == "pending" for a in err(out).get("approvals", [])), "wait: approval pending")
            d.drain(1)
            requests = [m for m in d.seen if "id" in m and m["method"].endswith("requestApproval")]
            LAST.update(cmd="daemon connection: messages seen", exit="-", out=[m.get("method") for m in d.seen])
            expect(requests, "the harness connection received the approval request")
            expect(
                not any(m.get("method") == "serverRequest/resolved" for m in d.seen),
                "nobody answered the request (no serverRequest/resolved)",
            )
            # End the turn without answering the request.
            turn = requests[0]["params"]["turnId"]
            expect("error" not in d.call("turn/interrupt", {"threadId": tid, "turnId": turn}), "turn/interrupt")

    def c8a():
        # Thread for C8 (codex_c6), created early so it is unloaded by then.
        out = codex_new("reply with the single word ok", "--wait")
        st["W"] = out["handle"]

    def c9():
        # After turn/interrupt a queued submission stays dormant until thread/queue/start;
        # `send` starts it when no turn starts on its own (design 6.1).
        out = codex_new("run `sleep 20` with your shell tool, then reply done")
        h, t0 = out["handle"], out["receipt"]["turn_id"]
        expect("error" not in codex_interrupt(native(h), t0), "turn/interrupt accepted")
        out = cli("wait", h, "--turn", t0, "--timeout", "30")
        if out["turn"]["status"] != "interrupted":
            raise Inconclusive(f"T0 ended {out['turn']['status']} before the interrupt")
        out = cli("send", h, "reply with the single word pear")
        r = out["receipt"]
        expect(r["turn_id"], "receipt.turn_id set by send (the submission was started)")
        out = cli("wait", h, "--receipt", r["receipt_id"], "--timeout", "60")
        expect(out["turn"]["status"] == "completed", "completed")
        expect(out["turn"]["turn_id"] == r["turn_id"], "turn id == receipt.turn_id")
        expect("pear" in final(out), "final ~ pear")

    def c7():
        need("A")
        p1 = cli("ls", "--agent", "codex", "--limit", "2")
        expect(p1["next_cursors"].get("codex"), "first page has a codex cursor")
        p2 = cli("ls", "--agent", "codex", "--limit", "2", "--cursor", p1["next_cursors"]["codex"])
        h1 = {s["handle"] for s in p1["sessions"]}
        h2 = {s["handle"] for s in p2["sessions"]}
        expect(h2 and not (h1 & h2), f"pages disjoint: {h1} / {h2}")
        out = cli("ls", "--agent", "codex", "--cwd", str(DIR), "--limit", "50")
        a = next((s for s in out["sessions"] if s["handle"] == st["A"]), None)
        expect(a is not None, "A listed under --cwd")
        expect(a["owned"] is True and a["observations"]["origin"] == "agent-talk", "A owned, origin agent-talk")

    for name, fn in (
        ("C1", c1),
        ("C8a", c8a),
        ("C2", c2),
        ("C3", c3),
        ("C3a", c3a),
        ("C3b", c3b),
        ("C3c", c3c),
        ("C3d", c3d),
        ("C3e", c3e),
        ("C3f", c3f),
        ("C3g", c3g),
        ("C3h", c3h),
        ("C4", c4),
        ("C7", c7),
        ("C9", c9),
    ):
        check("codex", name, fn)


def codex_c6():
    def c6():
        need("A")
        ready = st.get("c3_end", time.monotonic()) + 65
        if (delay := ready - time.monotonic()) > 0:
            print(f"  (sleeping {delay:.0f}s so thread A is idle > 60 s)", flush=True)
            time.sleep(delay)
        out = cli("ls", "--agent", "codex", "--cwd", str(DIR), "--limit", "50")
        a = next((s for s in out["sessions"] if s["handle"] == st["A"]), None)
        expect(a is not None, "A listed under --cwd")
        if a["observations"]["loaded"] != "no":
            raise Skip(f"A still loaded ({a['observations']['loaded']})")
        out = cli("send", st["A"], "reply with the single word plum")
        r = out["receipt"]["receipt_id"]
        out = cli("wait", st["A"], "--receipt", r, "--timeout", "120")
        expect(out["turn"]["status"] == "completed", "completed")
        expect("plum" in final(out), "final ~ plum")
        ctx = codex_turn_context(native(st["A"]), out["turn"]["turn_id"])
        expect(ctx["approval_policy"] == "never", f"approval never after the cold resume, got {ctx['approval_policy']}")

    def c8():
        # A thread held by another app-server process (here a standalone stdio `codex
        # app-server`; in practice the desktop app or VS Code) is refused with E_FOREIGN_LIVE.
        need("W")
        out = cli("ls", "--agent", "codex", "--cwd", str(DIR), "--limit", "50")
        w = next((s for s in out["sessions"] if s["handle"] == st["W"]), None)
        expect(w is not None, "W listed under --cwd")
        if w["observations"]["loaded"] != "no":
            raise Skip(f"W still loaded ({w['observations']['loaded']})")
        # Own session: `codex` may be a launcher whose child is the server; kill the group.
        proc = subprocess.Popen(
            ["codex", "app-server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=BASE_ENV,
            start_new_session=True,
        )
        st["W_proc"] = proc

        def call(rid, method, params):
            proc.stdin.write(json.dumps({"id": rid, "method": method, "params": params}) + "\n")
            proc.stdin.flush()
            while "method" in (msg := json.loads(proc.stdout.readline())) or msg.get("id") != rid:
                pass
            return msg

        call(
            1,
            "initialize",
            {"clientInfo": {"name": "agent-talk-live", "version": "0"}, "capabilities": {"experimentalApi": True}},
        )
        proc.stdin.write(json.dumps({"method": "initialized", "params": {}}) + "\n")
        proc.stdin.flush()
        resp = call(2, "thread/resume", {"threadId": native(st["W"]), "excludeTurns": True})
        LAST.update(cmd="standalone codex app-server: thread/resume W", exit="-", out=resp)
        expect("error" not in resp, "standalone app-server resumed W")
        out = cli("send", st["W"], "x", expect_exit=2)
        expect(err(out).get("code") == "E_FOREIGN_LIVE", "E_FOREIGN_LIVE")
        agent_error = err(out).get("agent_error") or {}
        expect(
            agent_error.get("code") == -32600
            and agent_error.get("message", "").endswith("already has an active writer"),
            "agent error kept",
        )
        expect(not err(out).get("receipt"), "no receipt (refused before the intent)")
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        out = cli("send", st["W"], "reply with the single word ok", "--wait")
        expect(out["turn"]["status"] == "completed", "send works once the other process is gone")

    check("codex", "C6", c6)
    check("codex", "C8", c8)


# ---------------------------------------------------------------- claude


def claude_transcript(session_id: str) -> Path | None:
    # Located by uuid: the project slug also maps '.' to '-', so do not rebuild it from the cwd.
    hits = list((Path.home() / ".claude/projects").glob(f"*/{session_id}.jsonl"))
    return hits[0] if hits else None


def run_init(agent: str, receipt_id: str) -> dict:
    """The init event of the run log of a Claude or Antigravity receipt."""
    log = Path.home() / f".agent-talk/{agent}-runs/{receipt_id}.ndjson"
    for line in log.read_text().splitlines() if log.exists() else []:
        ev = json.loads(line)
        if agent == "claude" and ev.get("type") == "system" and ev.get("subtype") == "init":
            return ev
        if agent == "antigravity" and ev.get("event") == "init":
            return ev["init"]
    raise Fail(f"no init event in {log}")


def claude_tier():
    def l1():
        out = cli(
            "new", "claude", "--cwd", str(DIR), "--model", CLAUDE_MODEL, "--wait", "reply with the single word pong"
        )
        st["S"] = out["handle"]
        created["claude_sessions"].append(native(out["handle"]))
        expect(out["receipt"]["turn_id"] == out["turn"]["turn_id"], "receipt.turn_id == turn.turn_id")
        expect(out["turn"]["status"] == "completed", "completed")
        expect("pong" in final(out), "final ~ pong")
        expect(out["turn"].get("basis"), "turn.basis present")
        st["S_model"] = run_init("claude", out["receipt"]["receipt_id"]).get("model")

    def l2():
        need("S")
        out = cli(
            "send", st["S"], "reply with the single word kiwi", "--wait", env={"AGENT_TALK_CALLER": "codex:harness"}
        )
        expect(out["from"].get("session") == "codex:harness", "from.session == codex:harness")
        expect("kiwi" in final(out), "final ~ kiwi")
        model = run_init("claude", out["receipt"]["receipt_id"]).get("model")
        expect(model == st.get("S_model"), f"send runs new's model {st.get('S_model')}, got {model}")
        turn = out["receipt"]["turn_id"]
        out = cli("read", st["S"], "--limit", "2")
        m0 = out["messages"][0]
        expect(m0["turn_id"] == turn, "messages[0].turn_id == receipt.turn_id")
        expect((m0.get("from") or {}).get("session") == "codex:harness", "messages[0].from.session")
        expect(m0["text"].startswith("[from codex:harness "), "text starts with header")

    def l3():
        need("S")
        out = cli(
            "send", st["S"], "count from 1 to 300, one number per line", "--wait", "--timeout", "3", expect_exit=3
        )
        expect(err(out).get("code") == "E_TIMEOUT", "E_TIMEOUT")
        rec = err(out).get("receipt") or {}
        expect(rec.get("receipt_id"), "receipt present")
        out = cli("send", st["S"], "x", expect_exit=2)
        expect(err(out).get("code") == "E_LOCKED", "E_LOCKED while the child runs")
        out = cli("wait", st["S"], "--receipt", rec["receipt_id"], "--timeout", "120")
        expect(out["turn"]["status"] == "completed", "completed")

    def l4():
        need("S")
        target = DIR / "denied.txt"
        target.unlink(missing_ok=True)
        out = cli("send", st["S"], f"create the file {target} using Bash touch", "--wait")
        expect(out["turn"]["status"] == "completed", "completed")
        if not out["approvals"]:
            if target.exists():
                raise Inconclusive("the user's permission mode allowed the touch (nothing was denied)")
            raise Inconclusive("no approval recorded (model did not try Bash)")
        expect(any(a["outcome"] == "denied" for a in out["approvals"]), "some approval denied")
        expect(not target.exists(), "file absent")

    def l6():
        # full_access through MCP new: bypassPermissions on the first process and again on a
        # later send (`--permission-mode` does not survive --resume).
        m = Mcp()
        try:
            is_err, out = m.call(
                "new",
                {
                    "agent": "claude",
                    "cwd": str(DIR),
                    "model": CLAUDE_MODEL,
                    "prompt": "reply with the single word pong",
                    "full_access": True,
                    "wait": True,
                },
            )
        finally:
            m.close()
        expect(not is_err, "isError false")
        h = out["handle"]
        created["claude_sessions"].append(native(h))
        created["claude_receipts"].append(out["receipt"]["receipt_id"])
        mode = run_init("claude", out["receipt"]["receipt_id"]).get("permissionMode")
        expect(mode == "bypassPermissions", f"new: permissionMode bypassPermissions, got {mode}")
        out = cli("send", h, "reply with the single word two", "--wait")
        mode = run_init("claude", out["receipt"]["receipt_id"]).get("permissionMode")
        expect(mode == "bypassPermissions", f"send: permissionMode bypassPermissions, got {mode}")

    def l5():
        f = str(uuid.uuid4())
        u = str(uuid.uuid4())
        created["claude_sessions"].append(f)
        log = Path(T) / "foreign.ndjson"
        with open(log, "w") as fh:
            proc = subprocess.Popen(
                [
                    "claude",
                    "-p",
                    "--verbose",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--session-id",
                    f,
                    "--model",
                    CLAUDE_MODEL,
                ],
                cwd=DIR,
                stdin=subprocess.PIPE,
                stdout=fh,
                stderr=subprocess.DEVNULL,
                text=True,
                env=BASE_ENV,
            )
        st["F_proc"] = proc
        line = {"type": "user", "uuid": u, "message": {"role": "user", "content": "reply with the single word ok"}}
        proc.stdin.write(json.dumps(line) + "\n")
        proc.stdin.flush()
        for _ in range(60):
            if '"type":"result"' in log.read_text():
                break
            time.sleep(1)
        else:
            raise Fail("foreign claude -p wrote no result within 60 s")
        handle = f"claude:{f}"
        out = cli("ls", "--agent", "claude", "--cwd", str(DIR))
        s = next((x for x in out["sessions"] if x["handle"] == handle), None)
        expect(s is not None, "F listed")
        expect(s["observations"]["loaded"] == "yes" and s["owned"] is False, "F loaded yes, owned false")
        transcript = claude_transcript(f)
        expect(transcript is not None, "F transcript exists")
        before = len(transcript.read_text().splitlines())
        out = cli("send", handle, "x", expect_exit=2)
        expect(err(out).get("code") == "E_FOREIGN_LIVE", "E_FOREIGN_LIVE")
        expect(len(transcript.read_text().splitlines()) == before, "transcript unchanged")
        out = cli("wait", handle, "--turn", u, "--timeout", "3", expect_exit=3)
        expect(err(out).get("state") == "running", "error.state == running while F is alive")
        proc.stdin.close()
        proc.wait(timeout=30)
        out = cli("wait", handle, "--turn", u, "--timeout", "10")
        expect(out["turn"]["status"] == "completed", "completed after F exits")

    for name, fn in (("L1", l1), ("L2", l2), ("L3", l3), ("L4", l4), ("L5", l5), ("L6", l6)):
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
    def p1():
        out = cli(
            "new",
            "opencode",
            "--cwd",
            str(DIR),
            "--model",
            OPENCODE_MODEL,
            "--wait",
            "reply with the single word mango",
        )
        st["O"] = out["handle"]
        created["opencode"].append(native(out["handle"]))
        expect(out["turn"]["status"] == "completed", "completed")
        expect("mango" in final(out), "final ~ mango")
        r = out["receipt"]
        expect(r["turn_id"] == r["queue_id"] and str(r["turn_id"]).startswith("msg_"), "turn_id == queue_id, msg_…")

    def p2():
        need("O")
        out = cli("send", st["O"], "reply with the single word kiwi", "--wait")
        expect(out["from"].get("kind") == "unknown", "from.kind == unknown")
        expect(not out["receipt"].get("delivered_text"), "no delivered_text")
        expect("kiwi" in final(out), "final ~ kiwi")
        text = out["turn"]["final_text"]
        again = cli("wait", st["O"], "--turn", out["turn"]["turn_id"], "--timeout", "5")
        expect(again["turn"]["final_text"] == text, "wait returns the same final_text")

    def p3():
        need("O")
        out = cli("send", st["O"], "x", "--steer", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "idle steer: E_PRECONDITION")
        out = cli("wait", st["O"], "--turn", "msg_bogus", "--timeout", "5", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "unknown turn: E_PRECONDITION")

    def p4():
        need("O")
        page = cli("read", st["O"], "--limit", "2")["messages"]
        expect([m["phase"] for m in page] == ["prompt", "final"], "newest page: the prompt and the final reply")
        full = cli("read", st["O"], "--all", "--limit", "100")["messages"]
        pages = paged(st["O"], 2)
        expect(
            [m["item_id"] for m in pages] == [m["item_id"] for m in full],
            f"paged {len(pages)} items == full {len(full)} items",
        )

    def p5():
        need("O")
        out = cli("send", st["O"], "run `sleep 8` with your shell tool, then reply done")
        r1 = out["receipt"]["receipt_id"]
        out = cli("ls", "--agent", "opencode", "--cwd", str(DIR))
        o = next((s for s in out["sessions"] if s["handle"] == st["O"]), None)
        expect(o is not None, "O listed")
        running = o["state"] == "running"
        out = cli(
            "send",
            st["O"],
            "reply with the single word steered",
            "--steer",
            "--wait",
            expect_exit=(0, 2) if not running else 0,
        )
        if not running:
            cli("wait", st["O"], "--receipt", r1, "--timeout", "120")
            raise Inconclusive(f"O state was {o['state']}, not running")
        expect(out["turn"]["status"] == "completed", "steer completed")
        reply = out["turn"]["final_text"]
        out = cli("wait", st["O"], "--receipt", r1)
        expect(out["turn"]["status"] == "completed", "R1 completed")
        expect(out["turn"]["final_text"] == reply, "R1 has the same reply as the steer")

    def p6():
        # Under the user's `edit: ask` the service itself refuses the write in a session agent-talk
        # started, so nothing waits after a send without --wait. A request that still asked would
        # reach `wait`, which declines it, and show up in its approvals.
        need("O")
        rules = opencode_api("GET", f"/api/session/{native(st['O'])}")["data"].get("permissions") or []
        if not any(r["action"] == "edit" and r["effect"] == "deny" for r in rules):
            raise Inconclusive("the session's rules do not deny edit (the user's rules do not ask for it)")
        target = DIR / "oc-denied.txt"
        target.unlink(missing_ok=True)
        # Without a write tool the model may reach for the shell, which edit rules do not cover.
        out = cli(
            "send", st["O"], f"create {target} with your write tool; if it is unavailable, do not create it at all"
        )
        out = cli("wait", st["O"], "--receipt", out["receipt"]["receipt_id"], "--timeout", "120")
        expect(out["turn"]["status"] == "completed", "completed")
        expect(not out["approvals"], "no approval request")
        expect(not target.exists(), "file absent")

    def p7():
        # A session agent-talk did not start, created over the service API under the user's
        # `edit: ask`: agent-talk reports its approval request as pending and does not answer it.
        provider, model = OPENCODE_MODEL.split("/", 1)
        created_ = opencode_api(
            "POST",
            "/api/session",
            {"location": {"directory": str(DIR)}, "model": {"providerID": provider, "id": model}},
        )
        sid = created_["data"]["id"]
        created["opencode"].append(sid)
        h = f"opencode:{sid}"
        target = DIR / "oc-foreign.txt"
        target.unlink(missing_ok=True)
        out = cli("send", h, f"create {target} with your write tool", "--wait", "--timeout", "30", expect_exit=(0, 3))
        if not err(out):
            raise Inconclusive("turn completed without an approval request")
        expect(err(out).get("code") == "E_TIMEOUT", "E_TIMEOUT")
        expect(err(out).get("state") == "waiting", "error.state == waiting")
        expect(err(out).get("approvals") and err(out)["approvals"][0]["outcome"] == "pending", "send: approval pending")
        receipt = err(out)["receipt"]["receipt_id"]
        out = cli("wait", h, "--receipt", receipt, "--timeout", "3", expect_exit=3)
        expect(err(out).get("state") == "waiting", "wait: error.state == waiting")
        pending = opencode_api("GET", f"/api/session/{sid}/permission").get("data") or []
        expect(pending, "the request is still pending at the service")
        # End the turn with a reject, the only answer agent-talk's harness gives.
        opencode_api(
            "POST",
            f"/api/session/{sid}/permission/{pending[0]['id']}/reply",
            {"decision": "reject", "message": "agent-talk live harness"},
        )
        out = cli("wait", h, "--receipt", receipt, "--timeout", "120")
        expect(out["turn"]["status"] == "completed", "completed after the harness rejected")
        expect(not target.exists(), "file absent")

    def p8():
        # new --full-access: the session's allow-all rule overrides the user's `edit: ask` (P6
        # shows the same write denied without it). The rule lives in the session, so one turn
        # covers later sends too.
        target = DIR / "oc-full.txt"
        target.unlink(missing_ok=True)
        out = cli(
            "new",
            "opencode",
            "--cwd",
            str(DIR),
            "--model",
            OPENCODE_MODEL,
            "--full-access",
            "--wait",
            f"create {target} with your write tool, then reply done",
        )
        created["opencode"].append(native(out["handle"]))
        expect(not out["approvals"] and target.exists(), "file written without an approval request")

    def m1():
        need("O")
        m = Mcp()
        try:
            is_err, inner = m.call(
                "send",
                {"handle": st["O"], "text": "reply with the single word plum", "wait": True},
                {"ai.opencode/sessionID": "ses_harnesscaller"},
            )
        finally:
            m.close()
        expect(not is_err, "isError false")
        expect(inner["from"].get("session") == "opencode:ses_harnesscaller", "from.session")
        expect("plum" in final(inner), "final ~ plum")
        out = cli("read", st["O"], "--limit", "2")
        user = next((x for x in out["messages"] if x["role"] == "user"), None)
        expect(user is not None, "user message in tail")
        expect((user.get("from") or {}).get("session") == "opencode:ses_harnesscaller", "user from.session")
        expect(user["text"].startswith("[from opencode:ses_harnesscaller "), "text starts with header")

    for name, fn in (
        ("P1", p1),
        ("P2", p2),
        ("P3", p3),
        ("P4", p4),
        ("P5", p5),
        ("P6", p6),
        ("P7", p7),
        ("P8", p8),
        ("M1", m1),
    ):
        check("opencode", name, fn)


# ---------------------------------------------------------------- grok


def grok_updates(session_id: str, home: Path | None = None) -> Path | None:
    hits = list(((home or Path.home() / ".grok") / "sessions").glob(f"*/{session_id}/updates.jsonl"))
    return hits[0] if hits else None


def grok_user_lines(session_id: str, text: str, home: Path | None = None) -> int:
    """user_message_chunk lines of the session whose text ends with `text` (no resubmission check)."""
    path = grok_updates(session_id, home)
    n = 0
    for line in path.read_text().splitlines() if path else []:
        u = json.loads(line).get("params", {}).get("update", {})
        if u.get("sessionUpdate") == "user_message_chunk" and (u.get("content") or {}).get("text", "").endswith(text):
            n += 1
    return n


def grok_yolo(session_id: str, home: Path) -> list:
    """`yolo_mode` of each turn_started in the session's events.jsonl, in order."""
    hits = list((home / "sessions").glob(f"*/{session_id}/events.jsonl"))
    lines = hits[0].read_text().splitlines() if hits else []
    return [e.get("yolo_mode") for e in map(json.loads, lines) if e.get("type") == "turn_started"]


def grok_tier():
    # Direct mode only (no leader runs for the harness; leader cases need an isolated
    # GROK_HOME and the user's go-ahead). Sessions go to the user's ~/.grok and are deleted
    # in cleanup, except G8's, which live in an isolated GROK_HOME.
    def g1():
        out = cli(
            "new",
            "grok",
            "--cwd",
            str(DIR),
            "--model",
            GROK_MODEL,
            "--name",
            "agent-talk-live-g1",
            "--wait",
            "reply with the single word kumquat",
        )
        st["G"] = out["handle"]
        created["grok"].append((native(out["handle"]), None))
        r, t = out["receipt"], out["turn"]
        expect(r["state"] == "accepted", "receipt accepted")
        expect(
            r["turn_id"] == t["turn_id"] == r["client_msg_id"],
            "receipt.turn_id == turn.turn_id == agent-talk's promptId",
        )
        expect(t["status"] == "completed", "completed")
        expect("kumquat" in final(out), "final ~ kumquat")
        out = cli("ls", "--agent", "grok", "--cwd", str(DIR))
        s = next((x for x in out["sessions"] if x["handle"] == st["G"]), None)
        expect(s is not None, "G listed with --cwd")
        expect(s["name"] == "agent-talk-live-g1", f"name == agent-talk-live-g1, got {s['name']}")
        expect(s["owned"] and s["observations"]["origin"] == "agent-talk", "owned, origin agent-talk")
        expect(s["preview"] == "reply with the single word kumquat", "preview without provenance header")

    # Receipt recovery runs on a session of its own: Grok tells the model to finish
    # unfinished tasks of earlier turns, so a cancelled `sleep` would be resumed by the next
    # prompt on the same session.
    def recover(args: list, text: str, timeout: str) -> dict:
        out = cli(*args, "--wait", "--timeout", timeout, text, expect_exit=3)
        expect(err(out).get("code") == "E_TIMEOUT", "E_TIMEOUT")
        rec = err(out).get("receipt") or {}
        expect(rec.get("receipt_id"), "receipt present")
        if "R" not in st:
            st["R"] = rec["handle"]
            created["grok"].append((native(rec["handle"]), None))
        out = cli("wait", st["R"], "--receipt", rec["receipt_id"], "--timeout", "60")
        expect(out["receipt"]["state"] == "accepted", "receipt accepted after recovery")
        expect(out["turn"]["turn_id"] == rec["client_msg_id"], "recovered turn is agent-talk's promptId")
        expect(grok_user_lines(native(st["R"]), text) <= 1, "no resubmission: at most one user line with the text")
        return out

    def g2():
        # new: the deadline hits mid-turn; agent-talk cancels (direct mode keeps no process), and
        # wait --receipt reads the end from updates.jsonl.
        text = "Run exactly this shell command and then reply DONE: sleep 40"
        out = recover(["new", "grok", "--cwd", str(DIR), "--model", GROK_MODEL], text, "12")
        expect(out["turn"]["status"] in ("interrupted", "completed"), f"turn ended, got {out['turn']['status']}")
        expect(out["turn"].get("basis") == "updates.jsonl turn_completed", "basis: updates.jsonl turn_completed")
        expect(grok_user_lines(native(st["R"]), text) == 1, "exactly one user line with the text")

    def g3():
        need("G")
        caller = str(uuid.uuid4())
        out = cli("send", st["G"], "reply with the single word kiwi", "--wait", env={"GROK_SESSION_ID": caller})
        expect(out["from"].get("session") == f"grok:{caller}", "from.session == grok:<GROK_SESSION_ID>")
        expect(
            (out["receipt"].get("delivered_text") or "").startswith(
                f"[from grok:{caller} via agent-talk; answer in your final response]"
            ),
            "delivered_text carries the provenance header",
        )
        expect("kiwi" in final(out), "final ~ kiwi")
        st["G_turn"], st["G_text"] = out["turn"]["turn_id"], out["turn"]["final_text"]
        tail = cli("read", st["G"], "--limit", "2")["messages"]
        expect(len(tail) == 2 and tail[0]["turn_id"] == st["G_turn"], "tail[0] is the kiwi prompt")
        expect((tail[0].get("from") or {}).get("session") == f"grok:{caller}", "tail[0].from.session")
        expect(tail[0]["text"].startswith(f"[from grok:{caller} "), "text starts with header")
        expect(tail[1]["role"] == "assistant" and tail[1]["phase"] == "final", "tail[1]: final assistant message")

    def g4():
        need("G")
        full = cli("read", st["G"], "--all", "--limit", "1000")["messages"]
        pages = paged(st["G"], 2)
        expect(
            [m["item_id"] for m in pages] == [m["item_id"] for m in full],
            f"paged {len(pages)} items == full {len(full)} items",
        )
        expect(len(full) >= 4, f"at least 4 messages, got {len(full)}")
        raw = cli("read", st["G"], "--limit", "2", "--raw")["raw"]
        expect(raw and all("params" in line for line in raw), "--raw: updates.jsonl lines")

    def g5():
        need("G_turn")
        out = cli("wait", st["G"], "--turn", st["G_turn"], "--timeout", "5")
        expect(out["turn"]["status"] == "completed", "completed")
        expect(out["turn"]["final_text"] == st["G_text"], "wait --turn returns the same final_text")
        expect(out["receipt"] and out["receipt"]["turn_id"] == st["G_turn"], "receipt attached")
        out = cli("wait", st["G"], "--turn", "bogus", "--timeout", "5", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "unknown turn: E_PRECONDITION")
        out = cli("send", st["G"], "x", "--steer", expect_exit=2)
        expect(err(out).get("code") == "E_NO_STEER", "steer without a leader: E_NO_STEER")

    def g6():
        need("G")
        text = "Run exactly this shell command and then reply DONE: sleep 8"
        proc = subprocess.Popen(
            [B, "send", st["G"], text, "--json"],
            env=BASE_ENV,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        try:
            for _ in range(60):
                if grok_user_lines(native(st["G"]), text):
                    break
                time.sleep(0.5)
            else:
                raise Fail("the background send wrote no user line within 30 s")
            out = cli("send", st["G"], "x", expect_exit=2)
            expect(err(out).get("code") == "E_LOCKED", "E_LOCKED while an agent-talk child runs the session")
            out = cli("ls", "--agent", "grok", "--cwd", str(DIR))
            s = next((x for x in out["sessions"] if x["handle"] == st["G"]), {})
            expect(s.get("state") == "running" and s["observations"]["loaded"] == "yes", "ls: running, loaded yes")
            stdout, _ = proc.communicate(timeout=120)
        finally:
            if proc.poll() is None:
                proc.kill()
        bg = json.loads(stdout)
        expect(
            proc.returncode == 0 and bg["receipt"]["state"] == "accepted" and bg.get("turn") is None,
            "send without --wait ran to the turn end and printed only the receipt",
        )

    def g7():
        home = Path(T) / "ghome"
        home.mkdir(exist_ok=True)
        env = {"GROK_HOME": str(home)}
        out = cli("new", "grok", "--cwd", str(DIR), "--model", GROK_MODEL, "reply with the single word one", env=env)
        sid = native(out["handle"])
        created["grok"].append((sid, home))
        st["G7"] = sid
        expect(grok_updates(sid, home) is not None and grok_updates(sid) is None, "session stored under GROK_HOME only")
        sleeper = subprocess.Popen(["sleep", "300"])
        try:
            (home / "active_sessions.json").write_text(
                json.dumps(
                    [{"session_id": sid, "pid": sleeper.pid, "cwd": str(DIR), "opened_at": "2026-10-05T00:00:00Z"}]
                )
            )
            before = grok_updates(sid, home).read_text()
            out = cli("send", out["handle"], "x", env=env, expect_exit=2)
            expect(err(out).get("code") == "E_FOREIGN_LIVE", "live TUI row: E_FOREIGN_LIVE")
            expect(grok_updates(sid, home).read_text() == before, "updates.jsonl unchanged")
            s = cli("ls", "--agent", "grok", env=env)["sessions"][0]
            expect(s["id"] == sid and s["observations"]["loaded"] == "yes", "ls: loaded yes")
        finally:
            sleeper.kill()
            sleeper.wait()
        out = cli("send", f"grok:{sid}", "reply with the single word two", "--wait", env=env)
        expect(out["turn"]["status"] == "completed", "stale row (dead pid) is not foreign: completed")

    def g8():
        # new --full-access: yolo on session/new and again on the direct-mode session/load of a
        # later send. In an isolated GROK_HOME, where G7's session shows the default (yolo off);
        # the user's always-approve would make every session yolo.
        need("G7")
        home = Path(T) / "ghome"
        env = {"GROK_HOME": str(home)}
        control = grok_yolo(st["G7"], home)
        if not control or any(control):
            raise Inconclusive(f"G7's session in the isolated GROK_HOME is not yolo off: {control}")
        out = cli(
            "new",
            "grok",
            "--cwd",
            str(DIR),
            "--model",
            GROK_MODEL,
            "--full-access",
            "--wait",
            "reply with the single word one",
            env=env,
        )
        sid = native(out["handle"])
        created["grok"].append((sid, home))
        cli("send", out["handle"], "reply with the single word two", "--wait", env=env)
        yolo = grok_yolo(sid, home)
        expect(yolo == [True, True], f"yolo_mode on both turns, got {yolo}")

    for name, fn in (("G1", g1), ("G2", g2), ("G3", g3), ("G4", g4), ("G5", g5), ("G6", g6), ("G7", g7), ("G8", g8)):
        check("grok", name, fn)


# ---------------------------------------------------------------- antigravity

AGY_HOME = Path.home() / ".gemini/antigravity-cli"
AGY_RUNS = Path.home() / ".agent-talk/antigravity-runs"


def agy_new(prompt: str, *extra) -> dict:
    out = cli("new", "antigravity", "--cwd", str(DIR), "--model", ANTIGRAVITY_MODEL, "--effort", "low", *extra, prompt)
    if out.get("handle"):
        created["antigravity"].append(native(out["handle"]))
    return out


def agy_steps(conv: str) -> list[dict]:
    path = AGY_HOME / "brain" / conv / ".system_generated/logs/transcript.jsonl"
    return [json.loads(l) for l in path.read_text().splitlines() if l.strip()] if path.exists() else []


def agy_user_inputs(conv: str, text: str) -> int:
    return sum(1 for s in agy_steps(conv) if s.get("type") == "USER_INPUT" and text in (s.get("content") or ""))


def agy_summary_status(conv: str) -> str | None:
    db = sqlite3.connect(f"file:{AGY_HOME / 'conversation_summaries.db'}?mode=ro", uri=True)
    with contextlib.closing(db):
        row = db.execute("SELECT status FROM conversation_summaries WHERE conversation_id = ?", (conv,)).fetchone()
    return row[0] if row else None


def agy_lock_held(conv: str) -> bool:
    import fcntl

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
    def a1():
        out = agy_new("reply with the single word pong", "--wait")
        st["AG"] = out["handle"]
        expect(out["receipt"]["state"] == "accepted", "receipt accepted")
        expect(
            out["receipt"]["turn_id"] == out["turn"]["turn_id"] == "0",
            "receipt.turn_id == turn.turn_id == '0' (USER_INPUT step index)",
        )
        expect(out["turn"]["status"] == "completed", "completed")
        expect("pong" in final(out), "final ~ pong")
        expect(out["turn"].get("basis"), "turn.basis present")
        out = cli("new", "antigravity", "--cwd", str(DIR), "--name", "x", "hi", expect_exit=2)
        expect(err(out).get("code") == "E_UNSUPPORTED", "new --name: E_UNSUPPORTED")
        out = cli("send", st["AG"], "x", "--steer", expect_exit=2)
        expect(err(out).get("code") == "E_NO_STEER", "steer: E_NO_STEER")

    def a2():
        # Receipt recovery: the deadline passes before agy's first event (startup takes ~6 s).
        need("AG")
        prompt = "count from 1 to 40, one number per line, nothing else"
        out = cli("send", st["AG"], prompt, "--wait", "--timeout", "1", expect_exit=3)
        expect(err(out).get("code") == "E_TIMEOUT", "E_TIMEOUT")
        rec = err(out).get("receipt") or {}
        expect(
            rec.get("state") == "unknown" and not rec.get("turn_id"),
            f"receipt unknown without a turn id, got {rec.get('state')} {rec.get('turn_id')}",
        )
        out = cli("send", st["AG"], "x", expect_exit=2)
        expect(err(out).get("code") == "E_LOCKED", "E_LOCKED while agent-talk's agy child runs")
        out = cli("wait", st["AG"], "--receipt", rec["receipt_id"], "--timeout", "180")
        expect(out["turn"]["status"] == "completed", "recovered: completed")
        expect("40" in final(out), "final ~ 40")
        expect(
            out["receipt"]["state"] == "accepted" and out["receipt"]["turn_id"] == out["turn"]["turn_id"],
            "receipt accepted with the turn id",
        )
        expect(agy_user_inputs(native(st["AG"]), prompt) == 1, "one USER_INPUT with the prompt (no resubmission)")

    def a3():
        # Caller identity from the shell (ANTIGRAVITY_CONVERSATION_ID), queue send, wait --turn.
        need("AG")
        caller = str(uuid.uuid4())
        out = cli(
            "send", st["AG"], "reply with the single word kiwi", "--wait", env={"ANTIGRAVITY_CONVERSATION_ID": caller}
        )
        expect(out["from"].get("session") == f"antigravity:{caller}", "from.session == antigravity:<env>")
        expect("kiwi" in final(out), "final ~ kiwi")
        turn = out["receipt"]["turn_id"]
        expect(turn and turn.isdigit(), "turn id is a step index")
        st["AG_turn"] = turn
        tail = cli("read", st["AG"], "--limit", "3")["messages"]
        user = next((m for m in tail if m["role"] == "user" and m["turn_id"] == turn), None)
        expect(user is not None, "the user message is in the tail")
        expect((user.get("from") or {}).get("session") == f"antigravity:{caller}", "read: user from.session")
        expect(user["text"].startswith(f"[from antigravity:{caller} "), "text starts with the provenance header")
        model = run_init("antigravity", out["receipt"]["receipt_id"]).get("model")
        expect(model == ANTIGRAVITY_MODEL, f"send runs new's model {ANTIGRAVITY_MODEL}, got {model}")
        again = cli("wait", st["AG"], "--turn", turn, "--timeout", "10")
        expect(again["turn"]["final_text"] == out["turn"]["final_text"], "wait --turn returns the same final_text")
        expect(
            (again.get("receipt") or {}).get("receipt_id") == out["receipt"]["receipt_id"],
            "wait --turn attaches the receipt",
        )
        out = cli("wait", st["AG"], "--turn", "999", "--timeout", "5", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "unknown turn: E_PRECONDITION")

    def a4():
        need("AG")
        full = cli("read", st["AG"], "--all", "--limit", "1000")["messages"]
        pages = paged(st["AG"], 3)
        expect(
            [m["item_id"] for m in pages] == [m["item_id"] for m in full],
            f"paged {len(pages)} items == full {len(full)} items",
        )
        raw = cli("read", st["AG"], "--raw", "--limit", "1000")
        expect(
            len(raw.get("raw") or []) == len(agy_steps(native(st["AG"]))), "read --raw returns every transcript line"
        )

    def a5():
        # Kill agent-talk's agy child mid-generation.
        need("AG")
        conv = native(st["AG"])
        t0 = time.time()
        p = subprocess.Popen(
            [B, "send", st["AG"], "count from 1 to 400, one number per line, nothing else", "--json"],
            env=env_with(None),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        agy_pid, log = None, None
        for _ in range(120):
            time.sleep(0.5)
            kids = subprocess.run(["pgrep", "-P", str(p.pid)], capture_output=True, text=True).stdout.split()
            logs = [f for f in AGY_RUNS.glob("*.ndjson") if f.stat().st_mtime >= t0 - 1]
            if kids and logs and '"user_input"' in logs[0].read_text():
                agy_pid, log = int(kids[0]), logs[0]
                break
        expect(agy_pid is not None, "found the agy child with its user step accepted")
        done_before = '"event":"result"' in log.read_text()
        os.kill(agy_pid, 9)
        stdout, _ = p.communicate(timeout=60)
        LAST.update(cmd="agent-talk send (killed child)", exit=p.returncode, out=stdout[-1000:])
        expect(p.returncode == 0, "send without --wait returns the receipt")
        rec = json.loads(stdout)["receipt"]
        if done_before:
            raise Inconclusive("the turn finished before the kill")
        out = cli("wait", st["AG"], "--turn", rec["turn_id"], "--timeout", "10", expect_exit=(0, 3))
        t = out.get("turn") or {}
        expect(t.get("status") != "completed", f"a killed turn is not completed, got {t.get('status')}")
        expect(t.get("basis") or err(out).get("state") in ("running", "unknown"), "basis or running/unknown state")
        expect(not agy_lock_held(conv), "presence lock released by the kill")
        rows = cli("ls", "--agent", "antigravity", "--cwd", str(DIR))["sessions"]
        row = next((s for s in rows if s["handle"] == st["AG"]), {})
        if agy_summary_status(conv) != "CASCADE_RUN_STATUS_RUNNING":
            raise Inconclusive("the kill landed before agy marked the run RUNNING; the summary status stayed IDLE")
        # RUNNING with the lock free: the lock, not the status, is liveness.
        expect(row.get("state") == "unknown", "ls: state unknown (lock free, status RUNNING)")

    def a6():
        out = cli("send", f"antigravity:{uuid.uuid4()}", "x", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "unknown conversation id: E_PRECONDITION")
        before = len(list((AGY_HOME / "conversations").glob("*.db")))
        out = cli("send", f"antigravity:{uuid.uuid4()}", "x", "--wait", expect_exit=2)
        expect(len(list((AGY_HOME / "conversations").glob("*.db"))) == before, "no conversation created")

    def a7():
        # A foreign agy process holding the conversation: it takes the presence lock at
        # startup, before it reads stdin.
        need("AG")
        conv = native(st["AG"])
        log = Path(T) / "agy-foreign.ndjson"
        with open(log, "w") as fh:
            proc = subprocess.Popen(
                [
                    "agy",
                    "-p",
                    "",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--model",
                    ANTIGRAVITY_MODEL,
                    "--effort",
                    "low",
                    "--conversation",
                    conv,
                ],
                cwd=DIR,
                stdin=subprocess.PIPE,
                stdout=fh,
                stderr=subprocess.DEVNULL,
                text=True,
                env=BASE_ENV,
            )
        st["AGY_proc"] = proc
        for _ in range(60):
            if agy_lock_held(conv):
                break
            time.sleep(0.5)
        else:
            raise Fail("the foreign agy process took no presence lock within 30 s")
        expect('"user_input"' not in log.read_text(), "lock taken before any input")
        before = len(agy_steps(conv))
        out = cli("send", st["AG"], "x", expect_exit=2)
        expect(err(out).get("code") == "E_FOREIGN_LIVE", "E_FOREIGN_LIVE")
        expect(len(agy_steps(conv)) == before, "transcript unchanged")
        rows = cli("ls", "--agent", "antigravity", "--cwd", str(DIR))["sessions"]
        row = next((s for s in rows if s["handle"] == st["AG"]), {})
        expect(row.get("observations", {}).get("loaded") == "yes", "ls: loaded yes")
        # A turn agent-talk did not run, then the process idles with the lock held.
        proc.stdin.write(
            json.dumps({"event": "user", "message": {"content": "reply with the single word foreign"}}) + "\n"
        )
        proc.stdin.flush()
        for _ in range(120):
            if '"event":"result"' in log.read_text():
                break
            time.sleep(0.5)
        else:
            raise Fail("the foreign turn wrote no result within 60 s")
        step = next(
            json.loads(l)["step_update"]["step_index"] for l in log.read_text().splitlines() if '"user_input"' in l
        )
        out = cli("wait", st["AG"], "--turn", str(step), "--timeout", "3", expect_exit=3)
        expect(err(out).get("state") == "running", "wait --turn on the foreign turn: running while the lock is held")
        proc.stdin.close()
        proc.wait(timeout=60)
        out = cli("wait", st["AG"], "--turn", str(step), "--timeout", "10")
        expect(out["turn"]["status"] == "completed" and "foreign" in final(out), "completed after the process exits")
        expect("transcript-derived" in (out["turn"].get("basis") or ""), "basis: transcript-derived")
        out = cli("send", st["AG"], "reply with the single word back", "--wait")
        expect("back" in final(out), "send works once the lock is free")

    def a8():
        need("AG")
        out = cli(
            "send",
            st["AG"],
            "run the shell command `echo agent-talk-denied-check` with your run_command tool, then reply done",
            "--wait",
        )
        expect(out["turn"]["status"] == "completed", "completed")
        if not out["approvals"]:
            raise Inconclusive("no denied action (the model did not run the command)")
        a = out["approvals"][0]
        expect(a["outcome"] == "denied" and a["kind"] == "denied_action", "approvals[0] denied_action, denied")
        again = cli("wait", st["AG"], "--receipt", out["receipt"]["receipt_id"], "--timeout", "10")
        expect(
            [x["summary"] for x in again["approvals"]] == [x["summary"] for x in out["approvals"]],
            "wait --receipt recovers the denials",
        )

    def a9():
        # new --full-access: --dangerously-skip-permissions on the first run and again on a later
        # send (agy keeps no permission mode across runs).
        out = agy_new("reply with the single word pong", "--full-access", "--wait")
        mode = run_init("antigravity", out["receipt"]["receipt_id"]).get("permission_mode")
        expect(mode == "always-proceed", f"new: permission_mode always-proceed, got {mode}")
        out = cli("send", out["handle"], "reply with the single word two", "--wait")
        mode = run_init("antigravity", out["receipt"]["receipt_id"]).get("permission_mode")
        expect(mode == "always-proceed", f"send: permission_mode always-proceed, got {mode}")

    for name, fn in (
        ("A1", a1),
        ("A2", a2),
        ("A3", a3),
        ("A4", a4),
        ("A5", a5),
        ("A6", a6),
        ("A7", a7),
        ("A8", a8),
        ("A9", a9),
    ):
        check("antigravity", name, fn)


# ---------------------------------------------------------------- setup, cleanup


# ---------------------------------------------------------------- pi

PI_SESSIONS = Path(os.environ.get("PI_CODING_AGENT_DIR") or Path.home() / ".pi/agent") / "sessions"


def pi_session_file(session_id: str) -> Path | None:
    hits = list(PI_SESSIONS.glob(f"*/*_{session_id}.jsonl"))
    return hits[0] if hits else None


def pi_entries(session_id: str) -> list[dict]:
    path = pi_session_file(session_id)
    return [json.loads(l) for l in path.read_text().splitlines() if l.strip()] if path else []


def pi_tier():
    def p1():
        out = cli(
            "new",
            "pi",
            "--cwd",
            str(DIR),
            "--model",
            PI_MODEL,
            "--effort",
            "low",
            "--name",
            "live",
            "--wait",
            "reply with the single word pong",
        )
        st["PI"] = out["handle"]
        created["pi_sessions"].append(native(out["handle"]))
        expect(out["receipt"]["state"] == "accepted", "receipt accepted")
        expect(out["receipt"]["turn_id"] == out["turn"]["turn_id"], "receipt.turn_id == turn.turn_id")
        expect(out["turn"]["status"] == "completed", "completed")
        expect("pong" in final(out), "final ~ pong")
        expect(out["turn"].get("basis"), "turn.basis present")
        entries = pi_entries(native(out["handle"]))
        user = next((e for e in entries if e.get("type") == "message" and e["message"]["role"] == "user"), None)
        expect(user is not None and user["id"] == out["turn"]["turn_id"], "turn id is the user entry id")
        kinds = {e.get("type"): e for e in entries}
        expect(kinds.get("session_info", {}).get("name") == "live", "session_info name == live")
        expect(kinds.get("thinking_level_change", {}).get("thinkingLevel") == "low", "thinking level low")
        rows = cli("ls", "--agent", "pi", "--cwd", str(DIR))["sessions"]
        row = next((r for r in rows if r["handle"] == out["handle"]), None)
        expect(row is not None and row["owned"] and row["name"] == "live", "ls: owned, name live")
        expect(row["observations"]["origin"] == "agent-talk", "ls: origin agent-talk")

    def p2():
        need("PI")
        out = cli(
            "send", st["PI"], "reply with the single word kiwi", "--wait", env={"AGENT_TALK_CALLER": "codex:harness"}
        )
        expect(out["from"].get("session") == "codex:harness", "from.session == codex:harness")
        expect("kiwi" in final(out), "final ~ kiwi")
        turn = out["receipt"]["turn_id"]
        out = cli("read", st["PI"], "--limit", "2")
        m0, m1 = out["messages"]
        expect(m0["turn_id"] == turn and m0["item_id"] == turn, "messages[0] is the prompt entry of the turn")
        expect((m0.get("from") or {}).get("session") == "codex:harness", "messages[0].from.session")
        expect(m0["text"].startswith("[from codex:harness "), "text starts with header")
        expect(m1["phase"] == "final" and "kiwi" in m1["text"], "messages[1] final ~ kiwi")
        # send passes no model or thinking level; pi restores the ones new stored in the file.
        reply = [
            e
            for e in pi_entries(native(st["PI"]))
            if e.get("type") == "message" and e["message"]["role"] == "assistant"
        ]
        provider, model = PI_MODEL.split("/", 1)
        expect(
            reply and (reply[-1]["message"].get("provider"), reply[-1]["message"].get("model")) == (provider, model),
            f"reply model is {PI_MODEL}",
        )
        expect(reply and reply[-1]["message"].get("thinkingLevel") == "low", "reply thinking level is low")

    def p3():
        need("PI")
        out = cli(
            "send", st["PI"], "count from 1 to 2000, one number per line", "--wait", "--timeout", "3", expect_exit=3
        )
        expect(err(out).get("code") == "E_TIMEOUT", "E_TIMEOUT")
        expect(err(out).get("state") == "running", "error.state == running")
        rec = err(out).get("receipt") or {}
        expect(rec.get("receipt_id") and rec.get("state") == "accepted", "receipt accepted")
        out = cli("send", st["PI"], "x", expect_exit=2)
        expect(err(out).get("code") == "E_LOCKED", "E_LOCKED while the child runs")
        rows = cli("ls", "--agent", "pi", "--cwd", str(DIR))["sessions"]
        row = next((r for r in rows if r["handle"] == st["PI"]), None)
        expect(row is not None and row["state"] == "running", "ls: state running")
        out = cli("wait", st["PI"], "--receipt", rec["receipt_id"], "--timeout", "120")
        expect(out["turn"]["status"] == "completed", "completed")
        expect(out["turn"]["turn_id"] == rec["turn_id"], "wait names the receipt's turn")

    def p4():
        need("PI")
        out = cli("send", st["PI"], "x", "--steer", expect_exit=2)
        expect(err(out).get("code") == "E_NO_STEER", "E_NO_STEER")
        out = cli("send", f"pi:{uuid.uuid4()}", "x", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "E_PRECONDITION for an unknown session")
        out = cli("models", "pi", PI_MODEL.split("/", 1)[1])
        row = next((m for m in out["models"] if m["id"] == PI_MODEL), None)
        expect(row is not None and "low" in row["efforts"], f"models lists {PI_MODEL} with effort low")

    def p5():
        # A session pi wrote on its own (no agent-talk): its turn ends are read from the file,
        # and a send resumes it in place.
        f = str(uuid.uuid4())
        created["pi_sessions"].append(f)
        p = subprocess.run(
            ["pi", "--mode", "json", "--model", PI_MODEL, "--thinking", "low", "--session-id", f],
            cwd=DIR,
            input="reply with the single word ok",
            capture_output=True,
            text=True,
            env=BASE_ENV,
            timeout=120,
        )
        expect(p.returncode == 0, f"pi --mode json exited {p.returncode}: {p.stderr[-300:]}")
        user = next(e for e in pi_entries(f) if e.get("type") == "message" and e["message"]["role"] == "user")
        handle = f"pi:{f}"
        out = cli("wait", handle, "--turn", user["id"], "--timeout", "10")
        expect(out["turn"]["status"] == "completed", "completed from the session file")
        expect("ok" in final(out), "final ~ ok")
        expect("session file" in (out["turn"].get("basis") or ""), "basis names the session file")
        out = cli("wait", handle, "--turn", "nothere", "--timeout", "5", expect_exit=2)
        expect(err(out).get("code") == "E_PRECONDITION", "E_PRECONDITION for an unknown turn")
        out = cli("send", handle, "reply with the single word two", "--wait")
        expect("two" in final(out), "send resumes the foreign session")
        out = cli("read", handle, "--limit", "4")
        expect([m["phase"] for m in out["messages"]] == ["prompt", "final", "prompt", "final"], "read: two turns")
        expect(
            (out["messages"][2].get("from") or {}).get("kind") == "unknown",
            "a prompt sent from the CLI by a person: from.kind unknown",
        )
        rows = cli("ls", "--agent", "pi", "--cwd", str(DIR))["sessions"]
        row = next((r for r in rows if r["handle"] == handle), None)
        expect(row is not None and not row["owned"] and row["observations"]["origin"] == "pi", "ls: not owned")

    def p6():
        m = Mcp()
        try:
            is_err, out = m.call(
                "new",
                {
                    "agent": "pi",
                    "cwd": str(DIR),
                    "model": PI_MODEL,
                    "prompt": "reply with the single word pong",
                    "full_access": True,
                    "wait": True,
                },
            )
        finally:
            m.close()
        expect(not is_err, "isError false")
        created["pi_sessions"].append(native(out["handle"]))
        created["pi_receipts"].append(out["receipt"]["receipt_id"])
        expect(out["turn"]["status"] == "completed" and "pong" in final(out), "completed ~ pong")

    for name, fn in (("P1", p1), ("P2", p2), ("P3", p3), ("P4", p4), ("P5", p5), ("P6", p6)):
        check("pi", name, fn)


def preconditions() -> dict[str, str | None]:
    """agent -> None when new and send can run, else the reasons from status."""
    out = json.loads(subprocess.run([B, "status", "--json"], capture_output=True, text=True, env=BASE_ENV).stdout)
    reasons = {}
    for p in out["agents"]:
        blocked = [
            f"{o['name']}: {o['reason']}"
            for o in p["operations"]
            if o["name"] in ("new", "send") and not o["available"]
        ]
        reasons[p["agent"]] = "; ".join(blocked) or None
    return reasons


def cleanup():
    if created["codex"]:
        try:
            with CodexDaemon() as d:
                for tid in created["codex"]:
                    resp = d.call("thread/archive", {"threadId": tid})
                    print(f"cleanup: archive codex thread {tid}: {(resp.get('error') or {}).get('message', 'ok')}")
        except Exception as e:  # report and continue with the rest
            print(f"cleanup: could not archive codex threads: {e}")
    for sid in created["opencode"]:
        try:
            opencode_api("DELETE", f"/api/session/{sid}")
            print(f"cleanup: deleted opencode session {sid}")
        except Exception as e:  # report and continue with the rest
            print(f"cleanup: could not delete opencode session {sid}: {e}")
    for sid in created["claude_sessions"]:
        if (path := claude_transcript(sid)) is not None:
            path.unlink()
            print(f"cleanup: deleted {path}")
    for sid in created["pi_sessions"]:
        if (path := pi_session_file(sid)) is not None:
            path.unlink()
            print(f"cleanup: deleted {path}")
    for agent in ("claude", "antigravity", "pi"):
        runs = Path.home() / f".agent-talk/{agent}-runs"
        receipts = set(created[f"{agent}_receipts"])
        for rid in receipts:
            for suffix in (".ndjson", ".stderr"):
                (runs / f"{rid}{suffix}").unlink(missing_ok=True)
        if receipts:
            print(f"cleanup: deleted {len(receipts)} {agent} run logs")
    # agy has no delete command: the run's conversations stay (cwd target/live-work).
    for cid in created["antigravity"]:
        print(f"cleanup: antigravity conversation {cid} stays (agy has no delete command)")
    for sid, home in created["grok"]:
        env = env_with({"GROK_HOME": str(home)} if home else None)
        p = subprocess.run(["grok", "sessions", "delete", sid], env=env, capture_output=True, text=True)
        print(f"cleanup: grok sessions delete {sid}: {(p.stdout or p.stderr).strip()}")


def main():
    args = sys.argv[1:]
    keep = "--keep" in args
    all_tiers = ["offline", "codex", "claude", "opencode", "grok", "antigravity", "pi"]
    tiers = [a for a in args if a != "--keep"]
    if not tiers:
        sys.exit(f"usage: uv run tests/live.py ({'|'.join(all_tiers)} ... | all) [--keep]")
    if tiers == ["all"]:
        tiers = all_tiers
    unknown = set(tiers) - set(all_tiers)
    if unknown:
        sys.exit(f"unknown tier(s): {', '.join(sorted(unknown))}")

    start = time.monotonic()
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=REPO, check=True)
    DIR.mkdir(parents=True, exist_ok=True)
    reasons = preconditions()
    run = {"offline": "offline" in tiers}
    for p in all_tiers[1:]:
        if p in tiers:
            if reasons[p]:
                print(f"SKIP {p}: {reasons[p]}")
                RESULTS.append(("SKIP", p))
            run[p] = reasons[p] is None
        else:
            run[p] = False

    try:
        if run["offline"]:
            offline()
        if run["codex"]:
            codex_main()
        if run["claude"]:
            claude_tier()
        if run["opencode"]:
            opencode_tier()
        if run["grok"]:
            grok_tier()
        if run["antigravity"]:
            antigravity_tier()
        if run["pi"]:
            pi_tier()
        if run["codex"]:
            codex_c6()
    finally:
        for key in ("F_proc", "AGY_proc"):
            proc = st.get(key)
            if proc and proc.poll() is None:
                proc.kill()
                proc.wait()
        if proc := st.get("W_proc"):  # the group outlives its leader when `codex` is a launcher
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
        if keep:
            print(
                f"--keep: left {created['opencode']} {created['claude_sessions']} and codex threads {created['codex']}"
            )
        else:
            cleanup()

    counts = {s: sum(1 for r, _ in RESULTS if r == s) for s in ("PASS", "FAIL", "SKIP", "INCONCLUSIVE")}
    print(" ".join(f"{k} {v}" for k, v in counts.items()) + f"  wall {time.monotonic() - start:.0f}s")
    # A gate run passes only when every requested check ran and passed.
    sys.exit(0 if counts["PASS"] == len(RESULTS) else 1)


if __name__ == "__main__":
    main()
