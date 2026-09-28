"""The checks. Each is a small function tagged with the incident id(s) it
covers (docs/incidents.md). `mode=ANY` checks only issue GETs and are safe
against a live server; `mode=SANDBOX` checks need the fake world (they POST,
spawn panes, write fixture files).

Known failures are declared per implementation: `ctx.expect(cond, msg,
known={"python": "INC-041"})` — a failure there is XFAIL, a pass XPASS.
"""
import json
import os
import plistlib
import re
import socket
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import quote

from world import compact
from core import ANY, SANDBOX, Skip, check, pct, port_open, spare_port, Client, wait_http, HOME

BOTH = "*"
# Fixed in the daemon (fix/control), still broken in Python.
PY = "python"


# ── helpers ─────────────────────────────────────────────────────────────

def rows_of(payload):
    """[(group, row)] over recent / momentum / areas."""
    out = [("recent", r) for r in payload.get("recent", [])]
    out += [("momentum", r) for r in payload.get("momentum", [])]
    for a, v in (payload.get("areas") or {}).items():
        out += [("area:" + a, r) for r in v]
    return out


def by_id(payload):
    return {r["id"]: r for _, r in rows_of(payload)}


def get_list(ctx, timeout=60):
    r = ctx.c.get("/api/local-sessions", timeout=timeout)
    ctx.require(r.status == 200, f"/api/local-sessions → {r.status} {r.error or r.body[:120]!r}")
    return r.json()


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def append(path, data):
    with open(path, "ab") as f:
        f.write(data)


def rec(role, text, sid, cwd, ts=None):
    ts = ts or now_iso()
    if role == "user":
        return {"type": "user", "sessionId": sid, "cwd": cwd, "timestamp": ts,
                "message": {"role": "user", "content": text}}
    return {"type": "assistant", "sessionId": sid, "cwd": cwd, "timestamp": ts,
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}


def wheel_events(log_path, start=0):
    """SGR wheel events [(btn, col, row)] the bench input app received."""
    try:
        lines = Path(log_path).read_text().splitlines()[start:]
    except FileNotFoundError:
        return []
    data = b"".join(bytes.fromhex(l) for l in lines if l)
    return [(int(a), int(b), int(c)) for a, b, c in re.findall(rb"\x1b\[<(\d+);(\d+);(\d+)M", data)]


def nlines(p):
    try:
        return len(Path(p).read_text().splitlines())
    except FileNotFoundError:
        return 0


def raw_http(port, payload, read_secs=3.0, host="127.0.0.1"):
    """Send raw bytes on one socket; return everything read until close or
    `read_secs` of silence."""
    s = socket.create_connection((host, port), 5)
    s.settimeout(read_secs)
    s.sendall(payload)
    buf = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
    except socket.timeout:
        pass
    except OSError:
        pass
    s.close()
    return buf


def statuses(buf):
    return [int(m) for m in re.findall(rb"HTTP/1\.[01] (\d{3})", buf)]


# ════════════════════════════════════════════════════════════════════════
# 1. live pane ↔ row mapping (top-10 #1)
# ════════════════════════════════════════════════════════════════════════

def _mapping_asserts(ctx, payload, when):
    w, I, P = ctx.world, ctx.world.ids, ctx.world.panes
    rows = by_id(payload)
    want = [("CL_RUN", P["claude"], "claude", "claude --resume"),
            ("CL_FLAG", P["chrome"], "claude", "claude --chrome ... --resume"),
            ("CX_RUN", P["codex"], "codex", "codex resume <id>"),
            ("CX_BARE", P["codexbare"], "codex", "bare codex via app-server + cwd")]
    for key, pane, origin, how in want:
        r = rows.get(I[key])
        if not ctx.expect(r is not None, f"{when}: row for {how} ({key}) present"):
            continue
        ctx.expect(r.get("tmux_session") == pane,
                   f"{when}: {how} row has tmux_session={pane!r} (got {r.get('tmux_session')!r})")
        ctx.expect(r.get("origin") == origin, f"{when}: {key} origin {origin} (got {r.get('origin')!r})")
    st = rows.get(I["CL_STALE"])
    ctx.expect(st is not None and st.get("tmux_session") is None,
               f"{when}: stale tty-map entry doesn't claim the codex pane "
               f"(CL_STALE tmux_session={st and st.get('tmux_session')!r})")
    idle = [rows.get(I[k]) for k in ("CL_IDLE", "CX_IDLE")]
    ctx.expect(all(r is not None and r.get("tmux_session") is None for r in idle),
               f"{when}: non-running rows carry no tmux_session")


@check("INC-001,INC-011,INC-015", mode=SANDBOX, name="pane-row-mapping")
def pane_row_mapping(ctx):
    """Rows of live panes (claude --resume, claude --chrome --resume, codex resume, bare codex) carry tmux_session — on the first list after start (prewarm race) and later."""
    t = ctx.target
    ctx.note(f"first list after {t.first_list_s:.2f}s (health after {t.first_health_s:.2f}s)")
    _mapping_asserts(ctx, t.first_list.json(), "first list after start")
    _mapping_asserts(ctx, get_list(ctx), "current list")


@check("INC-001,INC-011,INC-015", mode=ANY, name="live-pane-rows")
def live_pane_rows(ctx):
    """Every `claude … --resume <id>` / `codex resume <id>` process running under a tmux server (ps, read-only) has a row with a tmux_session that /api/sessions lists."""
    if not ctx.live:
        raise Skip("live-only (sandbox covers it in pane-row-mapping)")
    sess = ctx.c.get("/api/sessions", timeout=20)
    ctx.require(sess.status == 200, f"/api/sessions → {sess.status}")
    names = {s.get("name") for s in sess.json() or [] if isinstance(s, dict)}
    rows = by_id(get_list(ctx))
    ps = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], capture_output=True, timeout=10).stdout.decode("utf-8", "replace")
    parent, cmd = {}, {}
    for line in ps.splitlines():
        p = line.split(None, 2)
        if len(p) == 3 and p[0].isdigit() and p[1].isdigit():
            parent[int(p[0])], cmd[int(p[0])] = int(p[1]), p[2]
    tmux_servers = {pid for pid, c in cmd.items() if re.match(r"^(\S*/)?tmux\b", c)}

    def under_tmux(pid):
        for _ in range(30):
            pid = parent.get(pid)
            if pid is None or pid <= 1:
                return False
            if pid in tmux_servers:
                return True
        return False
    rx = re.compile(r"(?:\bclaude\b[^\n;]*?--resume|\bcodex resume) ([0-9a-f-]{36})")
    running = {}
    for pid, c in cmd.items():
        m = rx.search(c)
        if m and under_tmux(pid):
            running[m.group(1)] = pid
    checked = 0
    for sid in running:
        r = rows.get(sid)
        if r is None:
            continue           # not a list row (short / hidden session)
        checked += 1
        ctx.expect(bool(r.get("tmux_session")) and r["tmux_session"] in names,
                   f"{r.get('origin')} session {sid[:8]} runs in tmux but its row has tmux_session={r.get('tmux_session')!r}")
    for sid, r in rows.items():
        if r.get("tmux_session"):
            ctx.expect(r["tmux_session"] in names, f"row {sid[:8]} claims pane {r['tmux_session']!r} that /api/sessions doesn't list")
    # Panes named after a session whose process isn't running (e.g. the
    # dead shells a Claude resume of a Codex id leaves behind, INC-012).
    dead = [n for n in names if re.match(r"^(claude|codex)-[0-9a-f]{8}$", n or "")
            and not any(sid.startswith(n.split("-", 1)[1]) for sid in running)
            and any(sid.startswith(n.split("-", 1)[1]) and rows[sid].get("origin") == "codex" for sid in rows)]
    ctx.note(f"{len(names)} tmux sessions, {len(running)} resume processes under tmux, {checked} with list rows checked")
    if dead:
        ctx.note(f"panes named claude-<codex id> with nothing running (INC-012 aftermath): {sorted(dead)}")


# ════════════════════════════════════════════════════════════════════════
# 2. resume by origin (top-10 #2)
# ════════════════════════════════════════════════════════════════════════

def _wait_argv(w, n0, secs=6):
    end = time.time() + secs
    while time.time() < end:
        a = w.argv()
        if len(a) > n0:
            time.sleep(0.3)
            return w.argv()[n0:]
        time.sleep(0.1)
    return []


@check("INC-001,INC-012", mode=SANDBOX, name="resume-by-origin")
def resume_by_origin(ctx):
    """POST /api/resume runs `codex resume <id>` (in `codex-<short>`, the rollout's cwd) for a Codex id and `claude --resume <id>` (right cwd) for a Claude id; a running Codex session returns its pane; unknown id → 404, no pane."""
    w, I = ctx.world, ctx.world.ids
    # Claude: the command and the cwd.
    n0 = len(w.argv())
    r = ctx.c.post("/api/resume", {"session_id": I["CL_IDLE"]}, timeout=20)
    ctx.expect(r.status == 200 and (r.json() or {}).get("tmux_session"), f"claude resume → 200 with tmux_session ({r.status})")
    got = _wait_argv(w, n0)
    ctx.expect(any(p == "claude" and a == f"--resume {I['CL_IDLE']}" for p, _, a in got),
               f"claude resume ran `claude --resume <id>` (got {got})")
    ctx.expect(any(p == "claude" and cwd == str(w.proj) for p, cwd, _ in got), "claude resume ran in the transcript's cwd")
    # Codex: must run codex, not claude.
    n0 = len(w.argv())
    r = ctx.c.post("/api/resume", {"session_id": I["CX_IDLE"]}, timeout=20)
    ctx.expect(r.status == 200, f"codex resume → 200 ({r.status} {r.body[:100]!r})")
    got = _wait_argv(w, n0)
    ctx.expect(any(p == "codex" and a == f"resume {I['CX_IDLE']}" for p, _, a in got),
               f"codex resume ran `codex resume <id>` (got {[(p, a) for p, _, a in got]})", known={PY: "INC-012"})
    ctx.expect(any(p == "codex" and cwd == str(w.proj) for p, cwd, _ in got),
               "codex resume ran in the rollout's cwd", known={PY: "INC-012"})
    ctx.expect(not any(p == "claude" for p, _, _ in got), "codex resume did not start claude", known={PY: "INC-012"})
    ctx.expect((r.json() or {}).get("tmux_session") == "codex-" + I["CX_IDLE"][:8],
               f"codex resume pane is codex-<short> (got {(r.json() or {}).get('tmux_session')!r})", known={PY: "INC-012"})
    # Codex id with a live pane: reuse, spawn nothing.
    n0, panes0 = len(w.argv()), set(w.pane_names())
    r = ctx.c.post("/api/resume", {"session_id": I["CX_RUN"]}, timeout=20)
    time.sleep(1.5)
    ctx.expect(r.status == 200 and (r.json() or {}).get("tmux_session") == w.panes["codex"],
               f"resume of a running Codex session returns its pane {w.panes['codex']} "
               f"(got {r.status} {(r.json() or {}).get('tmux_session')!r})", known={PY: "INC-012"})
    ctx.expect(set(w.pane_names()) == panes0 and len(w.argv()) == n0,
               "resume of a running Codex session spawns nothing", known={PY: "INC-012"})
    # Bare `codex` mapped through the app-server (tty/cwd mapping): same.
    n0, panes0 = len(w.argv()), set(w.pane_names())
    r = ctx.c.post("/api/resume", {"session_id": I["CX_BARE"]}, timeout=20)
    time.sleep(1.0)
    ctx.expect(r.status == 200 and (r.json() or {}).get("tmux_session") == w.panes["codexbare"],
               f"resume of a bare-codex session returns its pane {w.panes['codexbare']} "
               f"(got {r.status} {(r.json() or {}).get('tmux_session')!r})", known={PY: "INC-012"})
    ctx.expect(set(w.pane_names()) == panes0 and len(w.argv()) == n0,
               "resume of a bare-codex session spawns nothing", known={PY: "INC-012"})
    # Unknown id → 4xx, no pane.
    panes0 = set(w.pane_names())
    r = ctx.c.post("/api/resume", {"session_id": "deadbeef-0000-4000-8000-000000000000"}, timeout=20)
    time.sleep(1.0)
    ctx.expect(r.status == 404, f"unknown id → 404 (got {r.status})", known={PY: "INC-012"})
    ctx.expect(set(w.pane_names()) == panes0, f"unknown id spawns no pane (new: {set(w.pane_names()) - panes0})",
               known={PY: "INC-012"})


@check("INC-012", mode=SANDBOX, name="resume-existing-pane")
def resume_existing_pane(ctx):
    """A pane named for the id that holds only an idle shell (the agent exited) is respawned with the resume command, not returned silently; one running something is returned as is and nothing is typed into it."""
    w, I = ctx.world, ctx.world.ids
    # Idle shell in claude-<short> (what an origin-blind resume leaves behind).
    idle = "claude-" + I["CL_MD"][:8]
    w.tmux("new-session", "-d", "-s", idle, "-c", str(w.proj), check=True)
    time.sleep(0.5)
    n0, panes0 = len(w.argv()), set(w.pane_names())
    r = ctx.c.post("/api/resume", {"session_id": I["CL_MD"]}, timeout=20)
    got = _wait_argv(w, n0)
    ctx.expect(r.status == 200 and (r.json() or {}).get("tmux_session") == idle,
               f"idle pane: resume answers with {idle} ({r.status} {r.body[:100]!r})")
    ctx.expect(any(p == "claude" and a == f"--resume {I['CL_MD']}" for p, _, a in got),
               f"idle pane: `claude --resume <id>` started in it (got {got})", known={PY: "INC-012"})
    ctx.expect((r.json() or {}).get("respawned") is True, "idle pane: response says respawned", known={PY: "INC-012"})
    ctx.expect(set(w.pane_names()) == panes0, f"idle pane: no extra pane (new: {set(w.pane_names()) - panes0})")
    # Busy pane (something other than a shell prompt): returned, untouched.
    busy = "claude-" + I["CL_HEADLESS"][:8]
    w.tmux("new-session", "-d", "-s", busy, "-c", str(w.proj), "exec sleep 100000", check=True)
    time.sleep(0.3)
    before = w.tmux("capture-pane", "-p", "-t", "=" + busy + ":").stdout
    n0, panes0 = len(w.argv()), set(w.pane_names())
    r = ctx.c.post("/api/resume", {"session_id": I["CL_HEADLESS"]}, timeout=20)
    time.sleep(1.0)
    ctx.expect(r.status == 200 and (r.json() or {}).get("tmux_session") == busy,
               f"busy pane: resume returns {busy} ({r.status} {r.body[:100]!r})")
    ctx.expect(len(w.argv()) == n0 and set(w.pane_names()) == panes0, "busy pane: nothing spawned")
    after = w.tmux("capture-pane", "-p", "-t", "=" + busy + ":").stdout
    ctx.expect(after == before, f"busy pane: nothing typed into it ({after[-80:]!r})")


@check("INC-047", mode=SANDBOX, name="resume-id-validation")
def resume_id_validation(ctx):
    """Resume ids that are globs or paths (`*`, `../x`, `[a-f]*`) → 400, no pane, no fake-binary run."""
    w = ctx.world
    for bad in ("*", "../x", "[a-f]*"):
        n0, panes0 = len(w.argv()), set(w.pane_names())
        r = ctx.c.post("/api/resume", {"session_id": bad}, timeout=20)
        time.sleep(1.2)
        ctx.expect(r.status == 400, f"resume {bad!r} → 400 (got {r.status})", known={PY: "INC-047"})
        ctx.expect(set(w.pane_names()) == panes0 and len(w.argv()) == n0,
                   f"resume {bad!r} spawned nothing (new panes {set(w.pane_names()) - panes0})", known={PY: "INC-047"})


# ════════════════════════════════════════════════════════════════════════
# 3. per-request deadline sweep (top-10 #3)
# ════════════════════════════════════════════════════════════════════════

@check("INC-001,INC-017,INC-028,INC-029", mode=ANY, name="deadline-sweep", slow=True)
def deadline_sweep(ctx):
    """For every row: /api/session, /api/messages?limit=50, 2 s of /api/tail, /api/children → 200 within 10 s; whole sweep < 120 s; messages non-empty when msg_count > 0."""
    payload = get_list(ctx)
    rows = list(by_id(payload).values())
    t0 = time.time()
    problems, slow, resets = [], [], [0]
    lock = threading.Lock()
    pace = {"next": 0.0}
    gap = 0.03 if ctx.live else 0.01

    def paced():
        """Space connection attempts out: the Python server listens with
        backlog 5 and resets bursts (NEW-1 — its own check)."""
        with lock:
            now = time.time()
            wait = max(0.0, pace["next"] - now)
            pace["next"] = max(now, pace["next"]) + gap
        time.sleep(wait)

    def retryable(err):
        return bool(re.search(r"reset|Broken pipe|Invalid argument", err or ""))

    def gets(r):
        sid, out = r["id"], []
        for path in (f"/api/session/{sid}", f"/api/messages/{sid}?limit=50", f"/api/children/{sid}"):
            for _ in range(4):
                paced()
                x = ctx.c.get(path, timeout=10)
                if x.status or not retryable(x.error):
                    break
                resets[0] += 1
            if x.status != 200:
                out.append(f"{path} → {x.status or x.error}")
            elif x.ms > 3000:
                slow.append((x.ms, path))
            if "/messages/" in path and x.status == 200 and (r.get("msg_count") or 0) > 0:
                if not (x.json() or {}).get("messages"):
                    out.append(f"{path}: empty messages but msg_count={r.get('msg_count')}")
        return out

    def tail(r):
        sid = r["id"]
        for _ in range(4):
            paced()
            s = ctx.c.sse(f"/api/tail/{sid}", timeout=10)
            ok = s.ready.wait(10)
            if s.status or not retryable(s.error):
                break
            resets[0] += 1
        time.sleep(max(0.0, 2.0 - (time.perf_counter() - s.t_open)))
        s.close()
        return [] if ok and s.status == 200 else [f"/api/tail/{sid} → {s.status} {s.error or 'no headers in 10 s' if not ok else ''}"]

    with ThreadPoolExecutor(4) as gp, ThreadPoolExecutor(16) as tp:
        fg = [gp.submit(gets, r) for r in rows]
        ft = [tp.submit(tail, r) for r in rows]
        for f in fg + ft:
            problems += f.result()
    total = time.time() - t0
    for p in problems[:25]:
        ctx.expect(False, p)
    ctx.expect(total < 120, f"sweep over {len(rows)} rows took {total:.0f}s (budget 120s)")
    slow.sort(reverse=True)
    ctx.note(f"{len(rows)} rows, {total:.1f}s, {len(problems)} problems, {len(slow)} requests > 3s, "
             f"{resets[0]} connection resets retried" + (f"; slowest: {[f'{p} {ms:.0f}ms' for ms, p in slow[:3]]}" if slow else ""))


# ════════════════════════════════════════════════════════════════════════
# 4. tail semantics (top-10 #4)
# ════════════════════════════════════════════════════════════════════════

@check("INC-016", mode=SANDBOX, name="tail-semantics")
def tail_semantics(ctx):
    """/api/tail: starts at EOF, ?since= replays only newer records in order, a record written in 3 pieces arrives once and whole, Codex rollouts stream Claude-shaped records."""
    w, I = ctx.world, ctx.world.ids
    big = w.claude_file(I["CL_BIG"])
    # since=<ts of #998> → #999, #1000
    s = ctx.c.sse(f"/api/tail/{I['CL_BIG']}?since={quote(w.big_ts[997])}")
    time.sleep(2.5)
    got = [d.get("timestamp") for d in s.data() if isinstance(d, dict)]
    s.close()
    ctx.expect(got == w.big_ts[998:1000], f"?since=#998 replays exactly #999,#1000 in order (got {len(got)}: {got[:4]})")
    # EOF start, then one record in 3 pieces
    s = ctx.c.sse(f"/api/tail/{I['CL_BIG']}")
    s.ready.wait(5)
    time.sleep(2.0)
    ctx.expect(s.status == 200, f"tail → 200 ({s.status})")
    ctx.expect(len(s.data()) == 0, f"0 records in the first 2 s (starts at EOF; got {len(s.data())}, {s.bytes} bytes)")
    line = compact(rec("assistant", "tail piece test é", I["CL_BIG"], str(w.proj))) + b"\n"
    k = len(line) // 3
    for part in (line[:k], line[k:2 * k], line[2 * k:]):
        append(big, part)
        time.sleep(0.05)
    time.sleep(2.5)
    d = s.data()
    s.close()
    ctx.expect(len(d) == 1 and isinstance(d[0], dict) and "tail piece test" in json.dumps(d[0], ensure_ascii=False),
               f"a record written in 3 pieces arrives once, whole (got {len(d)}: {str(d)[:160]})")
    # Codex rollout: Claude-shaped records
    s = ctx.c.sse(f"/api/tail/{I['CX_RUN']}?from=0")
    s.ready.wait(5)
    time.sleep(2.2)
    d = [x for x in s.data() if isinstance(x, dict)]
    s.close()
    ctx.expect(s.status == 200, f"codex tail → 200 (got {s.status})")
    ctx.expect(bool(d) and all(x.get("type") and (x.get("message") or {}).get("role") for x in d),
               f"codex records carry type + message.role ({len(d)} records; first {str(d[:1])[:160]})")


@check("INC-016", mode=ANY, name="tail-eof-lite")
def tail_eof_lite(ctx):
    """Newest session: /api/tail sends < 10 KB in the first 2 s (no whole-file replay)."""
    payload = get_list(ctx)
    rows = sorted(by_id(payload).values(), key=lambda r: r.get("last_ts") or "", reverse=True)
    ctx.require(bool(rows), "list has rows")
    s = ctx.c.sse(f"/api/tail/{rows[0]['id']}")
    time.sleep(2.0)
    s.close()
    ctx.expect(s.status == 200, f"tail newest → 200 ({s.status})")
    ctx.expect(s.bytes < 10240, f"first 2 s: {s.bytes} bytes (< 10 KB)")


# ════════════════════════════════════════════════════════════════════════
# 5. malformed input fuzz (top-10 #5)
# ════════════════════════════════════════════════════════════════════════

POSTS = ["/api/spawn", "/api/resume", "/api/kill-session", "/api/respond", "/api/send", "/api/scroll",
         "/api/send-key", "/api/register-device", "/api/hook-event", "/api/client-metrics",
         "/api/bench/job", "/api/bench/done"]

# (label, raw body)
BAD_BODIES = [("invalid JSON", b"{not json"), ("JSON list", b"[]"), ("JSON number", b"5"),
              ("JSON string", b'"x"'), ("JSON null", b"null"), ("non-UTF-8", b'{"a":"\xff"}')]

# endpoint → [(label, body)] wrong field types
TYPE_FUZZ = {
    "/api/scroll": [("session:int", {"session": 1, "direction": "up"}),
                    ("direction:int", {"session": "csm-bench-seq", "direction": 1}),
                    ("ticks:str", {"session": "csm-bench-seq", "direction": "up", "ticks": "x"})],
    "/api/send-key": [("keys:int", {"session": "csm-bench-seq", "keys": 1}),
                      ("session:list", {"session": ["x"], "keys": ["Up"]})],
    "/api/send": [("session:int", {"session": 1, "message": "x"}),
                  ("message:int", {"session": "csm-bench-nosuch", "message": 5})],
    "/api/kill-session": [("session:int", {"session": 1})],
    "/api/resume": [("session_id:int", {"session_id": 1})],
    "/api/respond": [("session:int", {"session": 1})],
    "/api/register-device": [("token:int", {"token": 1})],
    "/api/spawn": [("cwd:int", {"cwd": 1, "open_terminal": False})],
    "/api/client-metrics": [("records:dict", {"records": {"a": 1}}), ("records:int", {"records": 5})],
    "/api/bench/job": [("reps:str", {"reps": "x"})],
}

# Python's read_body() json.loads an invalid body and dies → 500 on every
# JSON endpoint (INC-045); `body.get` on a non-object → 500 likewise. The
# daemon parses every POST body with one parser (http.rs) and answers 400
# with a JSON error — including /api/bench/*, which it validates before
# proxying when not in --dev.
PY_BODY_BUG = {PY: "INC-045"}
# INC-041 names scroll/send-key; the same `body.get(...).strip()` pattern
# 500s on every endpoint below in Python. The daemon's typed field access
# makes each a 400.
KNOWN_TYPE = {k: {PY: "INC-041"} for k in [
    ("/api/scroll", "session:int"), ("/api/scroll", "direction:int"),
    ("/api/send-key", "keys:int"), ("/api/send-key", "session:list"),
    ("/api/send", "session:int"), ("/api/send", "message:int"),
    ("/api/kill-session", "session:int"), ("/api/resume", "session_id:int"),
    ("/api/respond", "session:int"), ("/api/register-device", "token:int"),
    ("/api/spawn", "cwd:int"), ("/api/bench/job", "reps:str")]}


@check("INC-038,INC-041,INC-045", mode=SANDBOX, name="post-fuzz")
def post_fuzz(ctx):
    """Every POST endpoint with invalid / non-object / non-UTF-8 JSON and wrong field types → 4xx, never 5xx or a dropped connection."""
    seen = []
    for ep in POSTS:
        for label, raw in BAD_BODIES:
            r = ctx.c.request("POST", ep, raw=raw, headers={"Content-Type": "application/json"}, timeout=10)
            seen.append((ep, label, r.status))
            ctx.expect(r.status == 400 and isinstance((r.json() or {}).get("error"), str),
                       f"{ep} {label} → {r.status or r.error} (want 400 with a JSON error)", known=PY_BODY_BUG)
        for label, body in TYPE_FUZZ.get(ep, []):
            r = ctx.c.post(ep, body, timeout=10)
            seen.append((ep, label, r.status))
            ctx.expect(0 < r.status < 500, f"{ep} {label} → {r.status or r.error} {r.body[:60]!r}",
                       known=KNOWN_TYPE.get((ep, label)))
    h = ctx.c.get("/api/health", timeout=5)
    ctx.expect(h.status == 200, "server healthy after the fuzz")
    ctx.note(f"{len(seen)} requests")


@check("INC-038", mode=SANDBOX, name="keepalive-framing")
def keepalive_framing(ctx):
    """Two pipelined POST /api/scroll on one socket → both 200; body longer than Content-Length → 4xx for the leftover, no 500; short body → 400 or timeout, never 500; fresh connection → 200."""
    port = ctx.c.port
    auth = f"Authorization: Bearer {ctx.c.token}\r\n".encode() if ctx.c.token else b""
    body = json.dumps({"session": ctx.world.panes["seq"], "direction": "up", "ticks": 1}).encode()
    one = (b"POST /api/scroll HTTP/1.1\r\nHost: x\r\n" + auth + b"Content-Type: application/json\r\n"
           b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    st = statuses(raw_http(port, one + one, read_secs=2.5))
    ctx.expect(st[:2] == [200, 200], f"pipelined pair → {st}")
    st = statuses(raw_http(port, one[:-len(body)] + body + b'{"extra": "garbage after the body"}', read_secs=2.5))
    ctx.expect(bool(st) and st[0] == 200 and 500 not in st, f"body longer than Content-Length → {st} (first 200, leftover never 500)")
    short = one.replace(b"Content-Length: " + str(len(body)).encode(), b"Content-Length: 500")
    st = statuses(raw_http(port, short, read_secs=3.0))
    ctx.expect(500 not in st, f"short body → {st or 'timeout'} (400 or timeout, never 500)")
    r = ctx.c.post("/api/scroll", {"session": ctx.world.panes["seq"], "direction": "down", "ticks": 1})
    ctx.expect(r.status == 200, f"fresh connection afterwards → {r.status}")


def _glob_leak(r):
    """A 200 carrying a session's content (preview markdown or messages)."""
    j = r.json() if r.status == 200 else None
    return isinstance(j, dict) and bool(j.get("markdown") or j.get("messages"))


@check("INC-048,INC-049", mode=ANY, name="path-param-fuzz")
def path_param_fuzz(ctx):
    """Path params with `*`, `?`, `[`, `;`, `..%2F`, `%ff` → 4xx, never 500, never another session's content."""
    cases = [("/api/session/*", "INC-048"), ("/api/session/%2A", None), ("/api/messages/a*", "INC-048"),
             ("/api/messages/%5Ba-f%5D%2A", None), ("/api/children/*", None),
             ("/api/session/..%2F..%2Fetc%2Fpasswd", None), ("/api/messages/%ff%fe", None),
             ("/api/session/", None), ("/api/messages/a;b", None), ("/api/pane/a;b", None),
             ("/api/pane/a%3Bb", None), ("/api/pane/bad%20name", None), ("/api/pane/..%2F..", None)]
    for path, inc in cases:
        r = ctx.c.get(path, timeout=10)
        known = {BOTH: inc} if inc else None
        ctx.expect(0 < r.status < 500, f"GET {path} → {r.status or r.error}")
        ctx.expect(not _glob_leak(r) and not (r.status == 200 and "/pane/" in path),
                   f"GET {path} → {r.status} serves content ({r.body[:60]!r})", known=known)
    # tail with a glob id must not stream some other session
    s = ctx.c.sse("/api/tail/*", timeout=5)
    s.ready.wait(5)
    time.sleep(0.5)
    s.close()
    ctx.expect(s.status is not None and 400 <= s.status < 500, f"GET /api/tail/* → {s.status}", known={BOTH: "INC-048"})


@check("INC-040", mode=ANY, name="browse-errors")
def browse_errors(ctx):
    """/api/browse on a missing dir → 404, on a file → 400, `~` → 200 or 400; never 500."""
    f = "/etc/hosts"
    for q, ok in (("/nonexistent-baton-regress", (404,)), (f, (400, 404)), ("~", (200, 400))):
        r = ctx.c.get(f"/api/browse?dir={quote(q)}", timeout=10)
        ctx.expect(r.status in ok, f"browse {q} → {r.status} (want {ok})", known={BOTH: "INC-040"})


# ════════════════════════════════════════════════════════════════════════
# 6. liveness under streams (top-10 #6)
# ════════════════════════════════════════════════════════════════════════

@check("INC-026,INC-042", mode=ANY, name="streams-liveness")
def streams_liveness(ctx):
    """With 20 SSE streams open (events, tail, and pane-stream in the sandbox), 20× health and 5× local-sessions all answer < 500 ms; RST-closing them logs no errors."""
    payload = get_list(ctx)
    ids = [r["id"] for r in sorted(by_id(payload).values(), key=lambda r: r.get("last_ts") or "", reverse=True)][:6]
    paths = ["/api/events"] * 7 + [f"/api/tail/{ids[i % len(ids)]}" for i in range(7)]
    if ctx.live:
        paths += ["/api/events"] * 6
    else:
        paths += [f"/api/pane-stream/{ctx.world.panes['seq']}"] * 6
    err_before = _errors(ctx)
    streams = []
    for p in paths:          # opened 25 ms apart (a burst is checked below)
        streams.append(ctx.c.sse(p, timeout=60))
        time.sleep(0.025)
    for s in streams:
        s.ready.wait(10)
    ctx.expect(all(s.status == 200 for s in streams),
               f"20 streams open ({sum(s.status == 200 for s in streams)} × 200: {[(p, s.status, s.error) for p, s in zip(paths, streams) if s.status != 200][:3]})")
    time.sleep(0.5)
    hs = [ctx.c.get("/api/health", timeout=5) for _ in range(20)]
    ls = [ctx.c.get("/api/local-sessions", timeout=5) for _ in range(5)]
    for s in streams:
        s.close(rst=True)
    hms = [r.ms for r in hs if r.status == 200]
    lms = [r.ms for r in ls if r.status == 200]
    ctx.expect(len(hms) == 20 and max(hms) < 500, f"health under 20 streams: max {max(hms or [0]):.0f} ms ({len(hms)}/20 ok)")
    ctx.expect(len(lms) == 5 and max(lms) < 500, f"local-sessions under 20 streams: max {max(lms or [0]):.0f} ms ({len(lms)}/5 ok)")
    ctx.note(f"health p50 {pct(hms, 50):.0f} ms, list p50 {pct(lms, 50):.0f} ms")
    if err_before is not None:
        time.sleep(1.5)
        new = _errors(ctx)[len(err_before):]
        ctx.expect(not new, f"client RSTs logged {len(new)} error records: {str(new[:1])[:200]}")
    # 20 connections arriving at the same instant (a phone reconnecting
    # every stream after a network change) must all be accepted.
    if ctx.live:
        return          # a connect burst makes the live Python server reset the phone's connections too
    bad = _burst(ctx, 30)
    # Timing-dependent (Python listens with backlog 5 and RSTs the overflow
    # when its accept thread is behind), so it's reported, not asserted.
    ctx.note(f"burst of 30 simultaneous connects right after: {len(bad)} failed" + (f" ({bad[0]})" if bad else ""))


def _burst(ctx, n):
    """Connect n sockets back to back, then send a health request on each;
    returns the failures."""
    socks, bad = [], []
    for _ in range(n):
        try:
            socks.append(socket.create_connection((ctx.c.host, ctx.c.port), 5))
        except OSError as e:
            bad.append(f"connect: {e}")
    for s in socks:
        try:
            s.sendall(b"GET /api/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        except OSError as e:
            bad.append(f"send: {e}")
    for s in socks:
        try:
            s.settimeout(10)
            head = s.recv(64)
            if not head.startswith(b"HTTP/1.") or b" 200" not in head[:13]:
                bad.append(f"reply {head[:20]!r}")
        except OSError as e:
            bad.append(f"recv: {type(e).__name__} {e}")
        finally:
            s.close()
    return bad


@check("NEW-1", mode=SANDBOX, name="concurrent-connects")
def concurrent_connects(ctx):
    """8 and 16 clients hammering /api/health (10 requests each): every connection accepted — no resets (Python listens with backlog 5)."""
    for n in (8, 16):
        from collections import Counter
        with ThreadPoolExecutor(n) as pool:
            res = list(pool.map(lambda _: ctx.c.get("/api/health", timeout=10), range(n * 10)))
        bad = Counter((r.error or str(r.status)).split(":")[0] for r in res if r.status != 200)
        ctx.expect(not bad, f"{n} concurrent clients × 10 health: {sum(bad.values())}/{n * 10} failed {dict(bad)}",
                   known={"python": "NEW-1 socketserver listen backlog 5 → connection resets"})
    bad = _burst(ctx, 30)
    ctx.note(f"burst of 30 simultaneous connects: {len(bad)} failed")


def _errors(ctx):
    if ctx.live:
        return None
    p = ctx.target.log_dir() / "cs-server.errors.jsonl"
    try:
        return p.read_text().splitlines()
    except FileNotFoundError:
        return []


# ════════════════════════════════════════════════════════════════════════
# 7. start-up as launchd would (top-10 #7)
# ════════════════════════════════════════════════════════════════════════

def _plist_for(ctx):
    """(ProgramArguments, EnvironmentVariables, source) for the impl under test."""
    if ctx.impl == "python":
        p = HOME / "Library/LaunchAgents/com.workbench.cs-server.plist"
        if not p.exists():
            raise Skip("no cs-server plist installed")
        d = plistlib.loads(p.read_bytes())
        return list(d["ProgramArguments"]), dict(d.get("EnvironmentVariables") or {}), str(p), d
    # daemon: the plist scripts/launchd.sh writes
    src = (Path(__file__).resolve().parents[2] / "scripts/launchd.sh").read_text()
    ok = "<string>serve</string>" in src and "<key>KeepAlive</key><true/>" in src
    args = [str(ctx.target.binary), "serve", "--listen", "0.0.0.0:19877", "--upstream", "http://127.0.0.1:19878"]
    env = {"PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"}
    return args, env, "scripts/launchd.sh template", {"KeepAlive": ok, "RunAtLoad": "<key>RunAtLoad</key><true/>" in src}


@check("INC-002,INC-003,INC-004,INC-039", mode=SANDBOX, name="startup-launchd", slow=True)
def startup_launchd(ctx):
    """Exec the plist's ProgramArguments with only the plist env (+ launchd's HOME/USER/SHELL/TMPDIR, sandbox HOME): port bound < 10 s; a second instance on the same port exits non-zero within 2 s, once; plist has KeepAlive + RunAtLoad and no osascript."""
    w = ctx.world
    args, penv, src, d = _plist_for(ctx)
    ctx.expect(d.get("KeepAlive") is True and d.get("RunAtLoad") is True, f"{src}: KeepAlive + RunAtLoad")
    ctx.expect(not any("osascript" in a or "Terminal" in a for a in args), f"{src}: no osascript/Terminal in ProgramArguments")
    port = spare_port()
    # Same args, only the port moved off 19877 and the host pinned to loopback.
    a = list(args)
    if ctx.impl == "python":
        a[a.index("--port") + 1] = str(port)
        a[a.index("--host") + 1] = "127.0.0.1"
        upstream = None
    else:
        a[a.index("--listen") + 1] = f"127.0.0.1:{port}"
        upstream = ctx.target.servers[0].port
        a[a.index("--upstream") + 1] = f"http://127.0.0.1:{upstream}"
    env = dict(penv)
    # launchd adds these; PATH keeps the plist's dirs but the sandbox fakes
    # (socket-pinned tmux shim, fake osascript) go first so nothing reaches
    # the user's tmux server or Terminal.
    env.update(HOME=str(w.home), USER=os.environ.get("USER", ""), LOGNAME=os.environ.get("USER", ""),
               SHELL="/bin/sh", TMPDIR=str(w.root) + "/", TMUX_TMPDIR=str(w.tdir),
               PATH=f"{w.fakes}:{penv.get('PATH', '/usr/bin:/bin')}", BATON_LOG_DIR=str(w.root / "startup-logs"),
               BATON_BEARER=w.token)
    ctx.note("argv: " + " ".join(x if len(x) < 60 else "…" + x[-50:] for x in a))
    # argparse accepts the arguments (INC-002: positional port/host)
    if ctx.impl == "python":
        h = subprocess.run(a + ["--help"], env=env, capture_output=True, text=True, timeout=30)
        ctx.expect(h.returncode == 0, f"plist args + --help parse (rc {h.returncode}: {h.stderr[-160:]})")
    out = open(w.root / "startup.err", "w")
    t0 = time.time()
    p1 = subprocess.Popen(a, env=env, cwd="/", stdin=subprocess.DEVNULL, stdout=out, stderr=out)
    try:
        bound = None
        while time.time() - t0 < 10 and p1.poll() is None:
            if port_open(port):
                bound = time.time() - t0
                break
            time.sleep(0.05)
        ctx.expect(bound is not None, f"port bound within 10 s (rc={p1.poll()}; {(w.root / 'startup.err').read_text()[-200:]!r})")
        if bound is None:
            return
        c = Client("127.0.0.1", port, w.token)
        hs = wait_http(c, 10)
        ctx.expect(hs is not None, "/api/health 200 within 10 s")
        ctx.note(f"bound after {bound:.2f}s, health after {(hs or 0) + bound:.2f}s")
        # second instance on the occupied port: exits non-zero promptly
        out2 = open(w.root / "startup2.err", "w")
        t1 = time.time()
        p2 = subprocess.Popen(a, env=env, cwd="/", stdin=subprocess.DEVNULL, stdout=out2, stderr=out2)
        try:
            rc = p2.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p2.kill()
            p2.wait()
            rc = None
        dt = time.time() - t1
        err2 = (w.root / "startup2.err").read_text()
        ctx.expect(rc not in (None, 0) and dt < 2.0,
                   f"second instance exits non-zero within 2 s (rc={rc}, {dt:.2f}s)")
        ctx.expect(err2.count("\n") <= 30, f"second instance wrote {err2.count(chr(10))} lines (one failure, no loop)")
        ctx.expect(c.get("/api/health", timeout=5).status == 200, "first instance still serving")
    finally:
        p1.terminate()
        try:
            p1.wait(5)
        except subprocess.TimeoutExpired:
            p1.kill()


@check("INC-002", mode=SANDBOX, name="import-no-fs-walk")
def import_no_fs_walk(ctx):
    """Importing api_server with os.scandir / os.listdir / Path.iterdir patched to raise succeeds (no filesystem walk at import)."""
    if ctx.impl != "python":
        raise Skip("Python-only")
    w = ctx.world
    code = ("import os, sys, pathlib\n"
            "def boom(*a, **k): raise InterruptedError(4, 'walk at import', a[0] if a else '')\n"
            "os.scandir = boom; os.listdir = boom; pathlib.Path.iterdir = boom\n"
            "sys.path.insert(0, sys.argv[1]); import api_server\nprint('ok')\n")
    r = subprocess.run(["/opt/homebrew/bin/python3.11", "-c", code,
                        str(Path(ctx.target.ref) / "server")], env=w.env(), capture_output=True, text=True, timeout=60)
    ctx.expect(r.returncode == 0 and r.stdout.strip().endswith("ok"),
               f"import with walks forbidden (rc {r.returncode}: {r.stderr.strip()[-200:]})")


# ════════════════════════════════════════════════════════════════════════
# 8. asset policy (top-10 #8)
# ════════════════════════════════════════════════════════════════════════

@check("INC-019,INC-020,INC-051", mode=SANDBOX, name="asset-policy")
def asset_policy(ctx):
    """/api/asset: each allowed root → 200; secrets, `..`, symlinks out → 4xx; > 100 MB → 413 < 100 ms and list still 200; unicode name → 200; reopen → 304."""
    w = ctx.world
    a = lambda p, **k: ctx.c.get("/api/asset?path=" + quote(str(p)), **k)  # noqa: E731
    for root, p in w.asset_ok.items():
        r = a(p)
        ctx.expect(r.status == 200 and r.body.startswith(b"allowed"), f"allowed root {root} → {r.status}")
    for rel, p in w.asset_secret.items():
        r = a(p)
        ctx.expect(400 <= r.status < 500, f"secret {rel} → {r.status}")
    for label, p in (("~/.claude/.credentials.json", w.home / ".claude/.credentials.json"),
                     ("~/.codex/auth.json", w.home / ".codex/auth.json"),
                     ("GitHub/../.ssh/id_x", f"{w.home}/GitHub/../.ssh/id_x"),
                     ("outside every root", w.home / "outside.txt"),
                     ("symlink → outside", w.home / "GitHub/repo/link-to-outside.txt")):
        r = a(p)
        ctx.expect(400 <= r.status < 500, f"{label} → {r.status}")
    r = a(w.home / "Documents/café é.txt")
    # INC-051 is about non-UTF-8 (latin-1) names, which APFS can't store;
    # this covers the non-ASCII UTF-8 case.
    ctx.expect(r.status == 200 and r.body == b"unicode name\n", f"non-ASCII file name → {r.status}")
    r = a(w.home / "Documents/big.md")
    et = r.headers.get("etag")
    ctx.expect(r.status == 200 and bool(et), f"text asset 200 with ETag ({r.status})")
    if et:
        r2 = a(w.home / "Documents/big.md", headers={"If-None-Match": et})
        ctx.expect(r2.status == 304, f"unchanged reopen → 304 (got {r2.status})")
    huge = w.home / "Documents/huge.bin"
    with open(huge, "wb") as f:
        f.truncate(5 << 30)   # sparse 5 GB
    try:
        r = a(huge, timeout=30)
        ctx.expect(r.status == 413 and r.ms < 100, f"sparse 5 GB → {r.status} in {r.ms:.0f} ms (413 < 100 ms)")
        l = ctx.c.get("/api/local-sessions", timeout=30)
        ctx.expect(l.status == 200, f"/api/local-sessions right after → {l.status}")
    finally:
        huge.unlink()
    mid = w.home / "Documents/mid.pdf"
    mid.write_bytes(b"%PDF" + os.urandom(1024) * 50 * 1024)
    try:
        r = a(mid, timeout=60)
        ctx.expect(r.status == 200 and len(r.body) == mid.stat().st_size
                   and r.headers.get("content-length") == str(mid.stat().st_size),
                   f"50 MB file streams whole with Content-Length ({r.status}, {len(r.body)} B)")
    finally:
        mid.unlink()


# ════════════════════════════════════════════════════════════════════════
# 9. terminal input correctness (top-10 #9)
# ════════════════════════════════════════════════════════════════════════

@check("INC-021,INC-030", mode=SANDBOX, name="scroll-centre")
def scroll_centre(ctx):
    """POST /api/scroll {up, ticks 3} → exactly 3 wheel-up events at the pane centre (100×30 → 50;15); one tmux spawn per scroll; p50 < 30 ms, p90 < 100 ms."""
    w = ctx.world
    pane, log = w.panes["input"], w.app_log["input"]
    n0 = nlines(log)
    r = ctx.c.post("/api/scroll", {"session": pane, "direction": "up", "ticks": 3})
    ctx.require(r.status == 200, f"scroll → {r.status} {r.body[:80]!r}")
    time.sleep(0.6)
    ev = wheel_events(log, n0)
    ctx.expect(ev == [(64, 50, 15)] * 3, f"3 wheel-up events at 50;15 (got {ev})")
    # warm (pane size cached) → count tmux spawns per scroll
    s0 = len(w.spawns())
    N = 30
    ms = []
    for i in range(N):
        r = ctx.c.post("/api/scroll", {"session": pane, "direction": "down" if i % 2 else "up", "ticks": 1})
        ms.append(r.ms)
    time.sleep(0.3)
    new = [l for l in w.spawns()[s0:] if l.startswith("tmux ")]
    sends = [l for l in new if "send-keys" in l]
    other = [l for l in new if "send-keys" not in l]
    ctx.expect(len(sends) == N, f"{N} scrolls → {len(sends)} tmux send-keys")
    ctx.expect(len(other) <= 3, f"≤ 3 other tmux calls during {N} scrolls (got {len(other)}: {other[:3]})")
    ctx.expect(pct(ms, 50) < 30 and pct(ms, 90) < 100, f"scroll p50 {pct(ms, 50):.0f} ms, p90 {pct(ms, 90):.0f} ms")


@check("INC-014,INC-015,INC-031,INC-049,INC-050", mode=SANDBOX, name="input-targeting")
def input_targeting(ctx):
    """scroll/send/send-key to a missing pane → 404 < 100 ms; `csm-bench-a` never reaches `csm-bench-ab`; `a;b` → 400; send to a bench pane shows up in the pane within 1 s."""
    w = ctx.world
    # send reaches the right pane
    before = w.tmux("capture-pane", "-p", "-t", "=" + w.panes["send"] + ":").stdout
    n0 = nlines(w.app_log["send"])
    r = ctx.c.post("/api/send", {"session": w.panes["send"], "message": "hello-regress"})
    ctx.expect(r.status == 200, f"send → {r.status}")
    t0, seen = time.time(), False
    while time.time() - t0 < 1.5 and not seen:
        data = b"".join(bytes.fromhex(l) for l in Path(w.app_log["send"]).read_text().splitlines()[n0:] if l)
        seen = b"hello-regress" in data
        time.sleep(0.05)
    ctx.expect(seen, f"sent text reached the pane's input within 1.5 s (capture before: {len(before)} chars)")
    # missing pane
    gone = "csm-bench-gone"
    for ep, body in (("/api/scroll", {"session": gone, "direction": "up", "ticks": 1}),
                     ("/api/send", {"session": gone, "message": "x"}),
                     ("/api/send-key", {"session": gone, "keys": ["Up"]})):
        r = ctx.c.post(ep, body)
        ctx.expect(r.status == 404 and r.ms < 100, f"{ep} to a missing pane → {r.status} in {r.ms:.0f} ms (want 404 < 100 ms)",
                   known={PY: "INC-031" if ep == "/api/scroll" else "INC-015"})
    # prefix mis-targeting: csm-bench-a doesn't exist, csm-bench-ab does
    n0 = nlines(w.app_log["ab"])
    for ep, body in (("/api/scroll", {"session": "csm-bench-a", "direction": "up", "ticks": 1}),
                     ("/api/send", {"session": "csm-bench-a", "message": "prefix-leak"}),
                     ("/api/send-key", {"session": "csm-bench-a", "keys": ["Up"]})):
        r = ctx.c.post(ep, body)
        time.sleep(0.4)
        leaked = nlines(w.app_log["ab"]) > n0
        ctx.expect(not leaked, f"{ep} to csm-bench-a didn't reach csm-bench-ab ({r.status})", known={PY: "INC-050"})
        ctx.expect(r.status == 404, f"{ep} to csm-bench-a → {r.status} (want 404)", known={PY: "INC-050"})
        n0 = nlines(w.app_log["ab"])
    r = ctx.c.get("/api/pane/csm-bench-a")
    ctx.expect(r.status == 404, f"/api/pane/csm-bench-a → {r.status} (want 404, not csm-bench-ab's screen)", known={PY: "INC-050"})
    # unsafe names
    for ep, body in (("/api/scroll", {"session": "a;b", "direction": "up"}),
                     ("/api/send", {"session": "a;b", "message": "x"}),
                     ("/api/send-key", {"session": "a;b", "keys": ["Up"]}),
                     ("/api/kill-session", {"session": "a;b"}),
                     ("/api/respond", {"session": "a;b"})):
        r = ctx.c.post(ep, body)
        known = {PY: "INC-049"} if ep in ("/api/send", "/api/send-key", "/api/respond") else None
        ctx.expect(r.status == 400, f"{ep} session 'a;b' → {r.status} (want 400)", known=known)


@check("INC-022", mode=SANDBOX, name="pane-stream-open")
def pane_stream_open(ctx):
    """/api/pane-stream with X-Pane-Cols/Rows equal to the pane size: first frame < 150 ms and no resize; a different size → dims equal to the target."""
    w = ctx.world
    pane = w.panes["seq"]
    size = w.tmux("display", "-p", "-t", "=" + pane + ":", "#{window_width}x#{window_height}").stdout.strip()
    cols, rows = map(int, size.split("x"))
    s = ctx.c.sse(f"/api/pane-stream/{pane}", headers={"X-Pane-Cols": str(cols), "X-Pane-Rows": str(rows)})
    t0 = time.time()
    while time.time() - t0 < 3 and len(s.events) < 2:
        time.sleep(0.01)
    s.close()
    ev = s.events
    ctx.require(len(ev) >= 2, f"dims + first frame arrive ({len(ev)} events, status {s.status})")
    ctx.expect(ev[0][1] == f"event: dims\ndata: {cols}x{rows}".encode(), f"dims event {ev[0][1][:40]!r}")
    ff = (ev[1][0] - s.t_open) * 1000
    ctx.expect(ff < 150, f"same-size first frame in {ff:.0f} ms (< 150)")
    after = w.tmux("display", "-p", "-t", "=" + pane + ":", "#{window_width}x#{window_height}").stdout.strip()
    ctx.expect(after == size, f"same-size open didn't resize ({size} → {after})")
    s = ctx.c.sse(f"/api/pane-stream/{pane}", headers={"X-Pane-Cols": "80", "X-Pane-Rows": "24"})
    t0 = time.time()
    while time.time() - t0 < 3 and len(s.events) < 2:
        time.sleep(0.01)
    s.close()
    ctx.expect(bool(s.events) and s.events[0][1] == b"event: dims\ndata: 80x24", f"resize open → dims 80x24 ({s.events[:1]})")
    w.tmux("resize-window", "-t", "=" + pane + ":", "-x", str(cols), "-y", str(rows))


# ════════════════════════════════════════════════════════════════════════
# other ANY / SANDBOX detections
# ════════════════════════════════════════════════════════════════════════

@check("INC-058", mode=ANY, name="auth")
def auth(ctx):
    """/api/health without a token → 200; list without / with a wrong token → 401 JSON."""
    r = ctx.c.get("/api/health", auth=False)
    ctx.expect(r.status == 200, f"health without token → {r.status}")
    if not ctx.c.token:
        raise Skip("server has no token")
    r = ctx.c.get("/api/local-sessions", auth=False)
    ctx.expect(r.status == 401 and isinstance(r.json(), dict), f"list without token → {r.status}")
    r = ctx.c.get("/api/local-sessions", headers={"Authorization": "Bearer wrong"}, auth=False)
    ctx.expect(r.status == 401 and isinstance(r.json(), dict), f"list with wrong token → {r.status}")


@check("INC-006", mode=ANY, name="health-fast")
def health_fast(ctx):
    """20× /api/health: p95 < 100 ms; in the sandbox, no tmux/ps spawns per health call after warm-up."""
    ctx.c.get("/api/health")
    w = ctx.world
    s0 = len(w.spawns()) if w else 0
    ms = [ctx.c.get("/api/health").ms for _ in range(20)]
    ctx.expect(pct(ms, 95) < 100, f"health p95 {pct(ms, 95):.0f} ms")
    if w:
        new = [l for l in w.spawns()[s0:] if l.startswith(("tmux list-sessions", "tmux -V", "ps "))]
        ctx.expect(len(new) <= 2, f"20 health calls → {len(new)} tmux/ps spawns ({new[:3]})")


@check("INC-009", mode=ANY, name="list-cold")
def list_cold(ctx):
    """First /api/local-sessions after start (sandbox) or a fresh one (live) < 3 s; ETag → 304; gzip when asked."""
    if not ctx.live:
        ctx.expect(ctx.target.first_list_s < 3, f"first list {ctx.target.first_list_s:.2f}s after spawn")
    r = ctx.c.get("/api/local-sessions", timeout=30)
    ctx.expect(r.status == 200 and r.ms < 3000, f"list → {r.status} in {r.ms:.0f} ms")
    et = r.headers.get("etag")
    if et:
        r2 = ctx.c.get("/api/local-sessions", headers={"If-None-Match": et})
        ctx.expect(r2.status in (200, 304), f"If-None-Match → {r2.status}")
    r3 = ctx.c.get("/api/local-sessions", headers={"Accept-Encoding": "gzip"})
    ctx.expect(r3.headers.get("content-encoding") == "gzip", "gzip with Accept-Encoding")


@check("INC-028", mode=ANY, name="list-dedup")
def list_dedup(ctx):
    """Ids unique within each group; (sandbox) moved-project duplicate → one row + the larger copy opens; headless sdk-cli hidden but in /api/background; Codex subagent hidden but in /api/children."""
    payload = get_list(ctx)
    groups = {}
    for g, r in rows_of(payload):
        groups.setdefault(g, []).append(r["id"])
    for g, ids in groups.items():
        ctx.expect(len(ids) == len(set(ids)), f"group {g}: {len(ids) - len(set(ids))} duplicate ids")
    if ctx.live:
        return
    I = ctx.world.ids
    rows = by_id(payload)
    d = rows.get(I["CL_DUP"])
    ctx.expect(d is not None and d.get("msg_count") == 30, f"duplicate id → the 30-message copy ({d and d.get('msg_count')})")
    m = ctx.c.get(f"/api/messages/{I['CL_DUP']}?limit=200").json() or {}
    txt = json.dumps(m)
    ctx.expect("dup-full" in txt and "dup-stub" not in txt, "messages open the larger copy")
    ctx.expect(I["CL_HEADLESS"] not in rows, "headless (sdk-cli) row hidden")
    bg = ctx.c.get("/api/background").json()
    ctx.expect(I["CL_HEADLESS"] in json.dumps(bg), "headless run listed in /api/background")
    ctx.expect(I["CX_SUB"] not in rows, "codex subagent thread hidden from the list")
    ch = ctx.c.get(f"/api/children/{I['CX_RUN']}")
    ctx.expect(ch.status == 200 and I["CX_SUB"] in ch.body.decode(), f"codex subagent in /api/children/<parent> ({ch.status})")


@check("INC-017", mode=SANDBOX, name="codex-open")
def codex_open(ctx):
    """A Codex session opens: /api/session and /api/messages 200 with its messages."""
    I = ctx.world.ids
    r = ctx.c.get(f"/api/session/{I['CX_RUN']}")
    ctx.expect(r.status == 200 and b"running-codex" in r.body, f"/api/session codex → {r.status}")
    r = ctx.c.get(f"/api/messages/{I['CX_RUN']}?limit=50")
    msgs = (r.json() or {}).get("messages") or []
    ctx.expect(r.status == 200 and len(msgs) >= 10, f"/api/messages codex → {r.status}, {len(msgs)} messages")


@check("INC-023", mode=SANDBOX, name="sessions-delta")
def sessions_delta(ctx):
    """/api/events: an appended assistant record → sessions_delta (changed, last_role assistant, groups has recent) < 6 s; new transcript → changed; deleted → removed."""
    w, I = ctx.world, ctx.world.ids
    s = ctx.c.sse("/api/events", timeout=60)
    s.ready.wait(5)
    time.sleep(1.0)
    append(w.claude_file(I["CL_IDLE"]), compact(rec("assistant", "delta reply", I["CL_IDLE"], str(w.proj))) + b"\n")

    def changed(sid, pred=lambda r: True):
        return lambda d: isinstance(d, dict) and d.get("type") == "sessions_delta" and any(
            r.get("id") == sid and pred(r) for r in d.get("changed") or [])
    ok = s.wait(changed(I["CL_IDLE"], lambda r: r.get("last_role") == "assistant" and "recent" in (r.get("groups") or [])), 8)
    if not ctx.expect(ok, "append → sessions_delta with last_role=assistant in recent (8 s)"):
        ctx.note("events seen: " + str([(d.get("type"), [(r.get("id", "")[:8], r.get("last_role"), r.get("groups"))
                                                        for r in d.get("changed") or []], d.get("removed"))
                                        for d in s.data() if isinstance(d, dict) and d.get("type") != "sessions_update"])[:600])
    new = "b0b0b0b0-b0b0-40b0-80b0-b0b0b0b0b0b0"
    from world import claude_records, dump
    from datetime import timedelta
    p = w.claude_file(new)
    dump(p, claude_records(new, str(w.proj), 6, datetime.now(timezone.utc) - timedelta(seconds=30), "new-file"))
    ctx.expect(s.wait(changed(new), 8), "new transcript → changed")
    p.unlink()
    ctx.expect(s.wait(lambda d: isinstance(d, dict) and d.get("type") == "sessions_delta" and new in (d.get("removed") or []), 8),
               "deleted transcript → removed")
    s.close()


@check("INC-037", mode=SANDBOX, name="malformed-record")
def malformed_record(ctx):
    """`{"message":123}` + a truncated line appended to an active transcript: list stays 200 with every other row; the row stays."""
    w, I = ctx.world, ctx.world.ids
    before = set(by_id(get_list(ctx)))
    p = w.claude_file(I["CL_MALFORMED"])
    orig = p.read_bytes()
    append(p, b'{"type":"assistant","message":123}\n{"type":"user","message":{"role":"us\n')
    try:
        time.sleep(4)
        r = ctx.c.get("/api/local-sessions", timeout=30)
        ctx.expect(r.status == 200, f"list after malformed append → {r.status}")
        after = set(by_id(r.json() or {}))
        ctx.expect(before <= after, f"rows lost: {[x[:8] for x in before - after]}")
        m = ctx.c.get(f"/api/messages/{I['CL_MALFORMED']}?limit=50")
        ctx.expect(m.status == 200, f"messages of the malformed transcript → {m.status} {m.body[:80]!r}",
                   known={BOTH: "NEW-2 /api/messages 500 on a malformed record (not in the catalog)"})
    finally:
        p.write_bytes(orig)


@check("INC-053", mode=SANDBOX, name="messages-bad-utf8")
def messages_bad_utf8(ctx):
    """A transcript with an invalid UTF-8 line: /api/messages → 200 with the other messages."""
    I = ctx.world.ids
    r = ctx.c.get(f"/api/messages/{I['CL_BADUTF']}?limit=50")
    msgs = (r.json() or {}).get("messages") or []
    ctx.expect(r.status == 200 and len(msgs) >= 10, f"/api/messages → {r.status}, {len(msgs)} messages", known={BOTH: "INC-053"})
    r = ctx.c.get(f"/api/session/{I['CL_BADUTF']}")
    ctx.expect(r.status == 200, f"/api/session → {r.status}")


@check("INC-052,INC-061", mode=SANDBOX, name="preview-markdown")
def preview_markdown(ctx):
    """Preview html keeps text after an unpaired `<b>` / `<details>` and a 500-char token intact."""
    I = ctx.world.ids
    r = ctx.c.get(f"/api/session/{I['CL_MD']}?limit=50")
    ctx.require(r.status == 200, f"/api/session → {r.status}")
    body = str((r.json() or {}).get("html") or "")
    ctx.note(f"html {len(body)} chars")
    ctx.expect("zzAFTERBOLD" in body, "html keeps the text after `a <b>`")
    ctx.expect("zzAFTERDETAILS" in body, "html keeps the text after `<details>`")
    # INC-061's wrap CSS lives in the clients; the server part is only that
    # the long token reaches the html intact.
    ctx.expect("x" * 500 + " zzLONGTOKEN" in body, "500-char token intact in the html")


@check("INC-027,INC-062", mode=SANDBOX, name="tmux-sessions-list")
def tmux_sessions_list(ctx):
    """/api/sessions lists every tmux session of the (private) server, each with a status."""
    names = set(ctx.world.pane_names())
    r = ctx.c.get("/api/sessions", timeout=15)
    got = r.json() or []
    gn = {s.get("name") for s in got if isinstance(s, dict)}
    ctx.expect(r.status == 200 and names <= gn, f"/api/sessions covers the {len(names)} panes (missing {names - gn})")
    ctx.expect(all(s.get("status") for s in got if isinstance(s, dict)), "every session has a status")


@check("INC-056", mode=SANDBOX, name="ps-bad-utf8")
def ps_bad_utf8(ctx):
    """A `\\xff` byte in one `ps` command line doesn't disable the process walk: running rows keep tmux_session."""
    w, I = ctx.world, ctx.world.ids
    w.poison.write_text("1")
    try:
        # a new transcript forces a rebuild (and a fresh tmux map)
        from world import claude_records, dump
        from datetime import timedelta
        nid = "b1b1b1b1-b1b1-41b1-81b1-b1b1b1b1b1b1"
        p = w.claude_file(nid)
        dump(p, claude_records(nid, str(w.proj), 6, datetime.now(timezone.utc) - timedelta(seconds=20), "poison-trigger"))
        end, rows = time.time() + 12, {}
        while time.time() < end:
            rows = by_id(get_list(ctx))
            if nid in rows:
                break
            time.sleep(0.5)
        ctx.expect(nid in rows, "list rebuilt with the poisoned ps")
        ctx.expect((rows.get(I["CL_RUN"]) or {}).get("tmux_session") == w.panes["claude"],
                   f"CL_RUN still mapped (tmux_session={(rows.get(I['CL_RUN']) or {}).get('tmux_session')!r})",
                   known={"python": "INC-056"})
        p.unlink()
        end = time.time() + 12      # leave the list as it was for later checks
        while time.time() < end and nid in by_id(get_list(ctx)):
            time.sleep(0.5)
    finally:
        w.poison.unlink()


@check("INC-046", mode=SANDBOX, name="bad-tty-map")
def bad_tty_map(ctx):
    """A malformed tty_session_map.json: list and /api/orphans still 200."""
    w = ctx.world
    tm = w.home / ".claude/tty_session_map.json"
    orig = tm.read_text()
    try:
        tm.write_text(json.dumps({"/dev/ttys999": "garbage", "/dev/ttys998": {"session_id": 5, "ts": "x"}, "x": [1]}))
        r = ctx.c.get("/api/orphans", timeout=20)
        ctx.expect(r.status == 200, f"/api/orphans → {r.status}")
        r = ctx.c.get("/api/local-sessions", timeout=30)
        ctx.expect(r.status == 200, f"/api/local-sessions → {r.status}")
    finally:
        tm.write_text(orig)


@check("INC-039,INC-044", mode=SANDBOX, name="voice-no-backend")
def voice_no_backend(ctx):
    """POST /api/voice with no transcription backend → 503 (or a clean 4xx/200), never 500."""
    wav = b"RIFF" + (36 + 32000).to_bytes(4, "little") + b"WAVEfmt " + (16).to_bytes(4, "little") + \
        (1).to_bytes(2, "little") + (1).to_bytes(2, "little") + (16000).to_bytes(4, "little") + \
        (32000).to_bytes(4, "little") + (2).to_bytes(2, "little") + (16).to_bytes(2, "little") + b"data" + \
        (32000).to_bytes(4, "little") + b"\0" * 32000
    # The sandbox has no Groq/OpenAI key and CS_MLX_PYTHON points nowhere
    # (world.env); a working whisper-cpp / whisper on PATH would transcribe.
    if subprocess.run(["which", "whisper-cpp"], capture_output=True, env=ctx.world.env()).returncode == 0:
        raise Skip("whisper-cpp on the sandbox PATH: a backend exists")
    r = ctx.c.request("POST", "/api/voice", raw=wav, headers={"Content-Type": "audio/wav"}, timeout=60)
    ctx.expect(r.status == 503 and (r.json() or {}).get("error") == "no transcription backend",
               f"/api/voice → {r.status} {r.body[:80]!r} (want 503 no transcription backend)", known={PY: "INC-039"})
    # An unsafe X-Session is rejected before anything is transcribed or typed.
    r = ctx.c.request("POST", "/api/voice", raw=wav, headers={"Content-Type": "audio/wav", "X-Session": "a;b"}, timeout=60)
    ctx.expect(r.status == 400, f"/api/voice X-Session 'a;b' → {r.status}", known={PY: "INC-049"})


@check("INC-043", mode=SANDBOX, name="kill-missing")
def kill_missing(ctx):
    """POST /api/kill-session for a session that doesn't exist → 404 or 200 {already_gone}, never 500."""
    r = ctx.c.post("/api/kill-session", {"session": "csm-bench-never-existed"})
    ctx.expect(r.status in (200, 404) and r.status != 500, f"kill missing → {r.status} {r.body[:80]!r}")


@check("INC-042", mode=ANY, name="events-rst")
def events_rst(ctx):
    """Opening /api/events and RST-closing it leaves the server healthy (and, sandboxed, logs no error record)."""
    before = _errors(ctx)
    for _ in range(5):
        s = ctx.c.sse("/api/events", timeout=10)
        s.ready.wait(5)
        time.sleep(0.2)
        s.close(rst=True)
    time.sleep(1.0)
    ctx.expect(ctx.c.get("/api/health").status == 200, "healthy after 5 RSTs")
    if before is not None:
        new = _errors(ctx)[len(before):]
        ctx.expect(not new, f"{len(new)} error records after RSTs: {str(new[:1])[:200]}")


@check("NEW-3", mode=SANDBOX, name="session-cold-warm")
def session_cold_warm(ctx):
    """/api/session for a never-opened transcript: the first (cold) answer equals the second (warm) one."""
    from world import claude_records, dump
    from datetime import timedelta
    w = ctx.world
    sid = "b2b2b2b2-b2b2-42b2-82b2-b2b2b2b2b2b2"
    p = w.claude_file(sid)
    dump(p, claude_records(sid, str(w.proj), 8, datetime.now(timezone.utc) - timedelta(days=3), "cold-warm"))
    try:
        a = ctx.c.get(f"/api/session/{sid}").json() or {}
        b = ctx.c.get(f"/api/session/{sid}").json() or {}
        ma, mb = a.get("markdown", ""), b.get("markdown", "")
        ctx.expect(ma == mb and ma, f"cold vs warm markdown: {len(ma)} vs {len(mb)} chars, ends {ma[-12:]!r} vs {mb[-12:]!r}",
                   known={"python": "NEW-3 Python's first /api/session of a transcript lacks the trailing \\n later calls (and the daemon) have"})
    finally:
        p.unlink()


@check("NEW-4", mode=ANY, name="orphans-fast")
def orphans_fast(ctx):
    """/api/orphans (after one warm-up call) answers in < 1 s, 3 times in a row."""
    ctx.c.get("/api/orphans", timeout=60)
    ms = []
    for _ in range(3):
        r = ctx.c.get("/api/orphans", timeout=60)
        ctx.require(r.status == 200, f"/api/orphans → {r.status}")
        ms.append(r.ms)
    ctx.expect(max(ms) < 1000, f"/api/orphans warm: {[round(x) for x in ms]} ms")


@check("INC-049", mode=SANDBOX, name="tmux-arg-separator")
def tmux_arg_separator(ctx):
    """A key or message ending in `;` reaches the pane literally: tmux reads a trailing `;` as a command separator, so send-key ["x;", "kill-server"] must not run kill-server."""
    if ctx.impl == "python":
        # Python passes the argument through: this would kill the sandbox
        # tmux server and take the rest of the run with it.
        raise Skip("python: known to split on a trailing ';' (INC-049); not run")
    w = ctx.world
    pane, log = w.panes["send"], w.app_log["send"]
    n0 = nlines(log)
    r = ctx.c.post("/api/send-key", {"session": pane, "keys": ["x;", "kill-server"]})
    ctx.expect(r.status == 200, f"send-key → {r.status} {r.body[:80]!r}")
    r = ctx.c.post("/api/send", {"session": pane, "message": "tail;"})
    ctx.expect(r.status == 200, f"send → {r.status} {r.body[:80]!r}")
    time.sleep(0.6)
    ctx.expect(w.panes["seq"] in w.pane_names(), "sandbox tmux server still up")
    data = b"".join(bytes.fromhex(l) for l in Path(log).read_text().splitlines()[n0:] if l)
    ctx.expect(b"x;kill-server" in data and b"tail;" in data, f"pane got the text literally ({data[-60:]!r})")
