#!/usr/bin/env python3
"""Parity: `baton serve` control group vs the Python reference server.

    python3 bench/parity_control.py [--ref ~/GitHub/workbench-exporter-master/claude-session-manager]
                                    [--bin <baton>] [--live http://127.0.0.1:19877] [--keep]

Part A — read-only, against the live Python server (never written to):
  daemon on 127.0.0.1:19884 (upstream = live server, real HOME, logs/devices
  redirected to a temp dir) vs the live server for /api/health, /api/temp,
  /api/browse, /api/plugins, /api/orphans, /api/entity-logo.

Part B — write endpoints, in two sandboxes that never touch the user's
  tmux, ~/.claude, ~/.codex, logs or devices:
  * Python: api_server imported in-process (python3.11) and served on
    127.0.0.1:19886 with HOME=<tmp>/py/home;
  * Rust:   `baton serve --dev` on 127.0.0.1:19885 with HOME=<tmp>/rs/home,
    BATON_LOG_DIR / BATON_DEVICES_FILE pointing at temp files and
    BATON_SESSION_PREFIX=csm-bench-ctl-.
  Each sandbox has its own tmux server (TMUX_TMPDIR, $TMUX unset) whose
  shell is /bin/sh; PATH starts with a tmux shim pinned to that server's
  socket (-S: with only TMUX_TMPDIR, tmux falls back to the user's default
  server when the dir is missing), then stand-ins for `claude`, `codex`,
  `osascript` (records the AppleScript instead of running it) and
  `whisper-cpp`. The same request script runs against both; responses,
  tmux state, files written, AppleScript, broadcast events and the
  request/error/client logs are compared after normalising session names,
  HOME, request ids and timestamps.

Intended divergences (INTENDED below: the daemon implements the incident
catalog's fix, Python still has the bug) are asserted as such rather than
as identical: malformed / wrong-typed bodies are 400 not 500 (INC-041,
INC-045), and a resume of an unknown id is 404 with no pane (INC-012).

Exit code 1 on any mismatch. Everything created is removed at the end
(tmux servers killed, temp dirs deleted) unless --keep.
"""
import argparse
import http.client
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HOME = Path.home()
REF_DEFAULT = HOME / "GitHub/workbench-exporter-master/claude-session-manager"
PY311 = "/opt/homebrew/bin/python3.11"
SID = "11111111-2222-3333-4444-555555555555"

FAILS = []
PASSES = []

# Intended divergences: the daemon implements the incident catalog's fix
# (docs/incidents.md), Python still has the bug. label → (incident,
# Python status, daemon status); the daemon's body must be a JSON error.
INTENDED = {
    "spawn bad json": ("INC-045", 500, 400),
    "spawn list body": ("INC-045", 500, 400),
    "spawn prompt non-str": ("INC-041", 500, 400),
    "resume unknown": ("INC-012", 200, 404),
    "respond key int": ("INC-041", 500, 400),
    "hook-event list": ("INC-045", 500, 400),
}
UNKNOWN_SID = "deadbeef-0000"


def check(name, ok, detail=""):
    (PASSES if ok else FAILS).append(name)
    mark = "ok  " if ok else "FAIL"
    print(f"  {mark} {name}" + ("" if ok or not detail else f"\n       {detail}"))


def token():
    if os.environ.get("BATON_BEARER"):
        return os.environ["BATON_BEARER"].strip()
    cfg = HOME / ".claude-sessions/config"
    tok = ""
    if cfg.exists():
        for line in cfg.read_text().splitlines():
            line = line.strip()
            if line.startswith("export "):
                line = line[7:].strip()
            if line.startswith("BATON_BEARER="):
                tok = line.split("=", 1)[1].strip().strip("'\"")
    return tok


def req(port, method, path, body=None, headers=None, raw=None, tok=None, timeout=60):
    h = dict(headers or {})
    if tok:
        h["Authorization"] = f"Bearer {tok}"
    data = raw
    if body is not None:
        data = body if isinstance(body, (bytes, str)) else json.dumps(body)
        if isinstance(data, str):
            data = data.encode()
        h.setdefault("Content-Type", "application/json")
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    c.request(method, path, body=data, headers=h)
    r = c.getresponse()
    out = r.status, {k.lower(): v for k, v in r.getheaders()}, r.read()
    c.close()
    return out


def wait_port(port, secs=20):
    t = time.time()
    while time.time() - t < secs:
        try:
            req(port, "GET", "/api/health", timeout=2)
            return True
        except OSError:
            time.sleep(0.1)
    return False


class Events(threading.Thread):
    """Collects `data:` JSON lines from an SSE endpoint."""

    def __init__(self, port, path):
        super().__init__(daemon=True)
        self.port, self.path, self.items, self.ready = port, path, [], threading.Event()

    def run(self):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=600)
        c.request("GET", self.path)
        r = c.getresponse()
        self.ready.set()
        while True:
            line = r.fp.readline()
            if not line:
                return
            line = line.decode().rstrip("\n")
            if line.startswith("data: "):
                self.items.append(json.loads(line[6:]))

    def wait_for(self, pred, secs=20):
        t = time.time()
        while time.time() - t < secs:
            if any(pred(e) for e in self.items):
                return True
            time.sleep(0.1)
        return False


# ── Part A: read-only vs the live server ───────────────────────────────

def part_a(a, tmp):
    print("\nPart A — read-only endpoints vs live Python server")
    tok = token()
    env = dict(os.environ)
    env["BATON_LOG_DIR"] = str(tmp / "a-logs")
    env["BATON_DEVICES_FILE"] = str(tmp / "a-devices.json")
    live_port = int(a.live.rsplit(":", 1)[1])
    d = subprocess.Popen([a.bin, "serve", "--listen", "127.0.0.1:19884", "--upstream", a.live],
                         env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not wait_port(19884):
            check("daemon A starts", False)
            return
        py = lambda p, **k: req(live_port, "GET", p, tok=tok, **k)  # noqa: E731
        rs = lambda p, **k: req(19884, "GET", p, tok=tok, **k)  # noqa: E731

        # health (no auth on either)
        sp, _, bp = req(live_port, "GET", "/api/health")
        sr, hr, br = req(19884, "GET", "/api/health")
        jp, jr = json.loads(bp), json.loads(br)
        check("health: 200 without auth", sp == sr == 200 and "x-baton-daemon" not in hr)
        same = [k for k in ("status", "hostname", "version", "tmux") if jp.get(k) == jr.get(k)]
        check("health: status/hostname/version/tmux equal", len(same) == 4,
              f"py={ {k: jp.get(k) for k in ('status','hostname','version','tmux')} } rs={ {k: jr.get(k) for k in ('status','hostname','version','tmux')} }")
        check("health: daemon/api_version/min_client added",
              jr.get("api_version") == 1 and jr.get("min_client") == 1 and bool(jr.get("daemon")))
        check("health: other Python fields kept (release/src/files/pid/started/uptime)",
              all(k in jr for k in ("release", "src", "files", "pid", "started", "uptime")),
              f"missing {[k for k in jp if k not in jr]}")

        # temp
        _, _, bp = py("/api/temp")
        _, hr, br = rs("/api/temp")
        jp, jr = json.loads(bp), json.loads(br)
        mp_ok = abs((jp.get("memory_pressure") or 0) - (jr.get("memory_pressure") or 0)) <= 10
        check("temp: same keys/order, battery equal, memory_pressure within 10",
              list(jp) == list(jr) and mp_ok and "x-baton-daemon" not in hr
              and all(jp[k] == jr[k] for k in ("cpu_temp", "battery_percent", "battery_state")),
              f"py={jp} rs={jr}")

        # browse — a fixture dir, HOME, errors, search
        fx = tmp / "browse-fixture"
        (fx / "sub dir").mkdir(parents=True)
        (fx / "b.txt").write_text("0123456789")
        (fx / "A.md").write_text("x")
        (fx / "é-ü 日本.txt").write_text("unicode")
        (fx / ".hidden").write_text("h")
        (fx / "quote\"s").write_text("q")
        os.symlink(fx / "nowhere", fx / "link-broken")
        os.symlink(fx / "sub dir", fx / "link-dir")
        for i in range(60):
            (fx / f"f{i:02d}.log").write_text("y" * i)
        from urllib.parse import quote
        cases = [f"/api/browse?dir={quote(str(fx))}", f"/api/browse?dir={quote(str(fx))}/",
                 "/api/browse", "/api/browse?dir=", f"/api/browse?dir={quote(str(fx / 'b.txt'))}",
                 "/api/browse?dir=/nonexistent-csm-bench-ctl", f"/api/browse?dir={quote(str(fx))}&q=f0",
                 f"/api/browse?dir={quote(str(fx))}&q=zzz-nomatch", "/api/browse?dir=~"]
        # INC-040 (intended divergence): Python answers 500 for a file, a
        # missing dir and `~`; the daemon answers 400 / 404 / the home listing.
        diverge = {cases[4]: 400, cases[5]: 404, cases[8]: 200}
        for p in cases:
            sp, hp, bp = py(p)
            sr, hr, br = rs(p)
            rid = lambda b: re.sub(rb'"rid": "[0-9a-f]{8}"', b'"rid": "<rid>"', b)  # noqa: E731
            if p in diverge and sp == 500:
                check(f"browse {p.replace(str(fx), '<fx>')}: py 500 → daemon {diverge[p]} (INC-040)",
                      sr == diverge[p] and "x-baton-daemon" not in hr, f"rs={sr} {br[:200]!r}")
                continue
            check(f"browse {p.replace(str(fx), '<fx>')}: {sp}",
                  (sp, rid(bp)) == (sr, rid(br)) and "x-baton-daemon" not in hr,
                  f"py={sp} {bp[:200]!r}\n       rs={sr} {br[:200]!r}")

        # plugins stay proxied
        sp, _, bp = py("/api/plugins")
        sr, hr, br = rs("/api/plugins")
        check("plugins: proxied, identical", (sp, bp) == (sr, br) and "x-baton-daemon" in hr)

        # orphans (idle_seconds may tick between the two calls)
        t = time.time()
        rs("/api/orphans")  # warm the daemon's parse state (first build is a full parse)
        cold_ms = int((time.time() - t) * 1000)
        t = time.time()
        sp, _, bp = py("/api/orphans")
        sr, hr, br = rs("/api/orphans")
        gap = time.time() - t
        jp, jr = json.loads(bp), json.loads(br)
        strip_idle = lambda j: [{k: v for k, v in o.items() if k != "idle_seconds"} for o in j.get("orphans", [])]  # noqa: E731
        idle_ok = all(abs(x["idle_seconds"] - y["idle_seconds"]) <= gap + 1.5
                      for x, y in zip(jp.get("orphans", []), jr.get("orphans", [])))
        check(f"orphans: {jp.get('count')} identical (idle ±{gap + 1.5:.1f}s; cold daemon call {cold_ms} ms)",
              sp == sr == 200 and strip_idle(jp) == strip_idle(jr) and jp.get("count") == jr.get("count")
              and idle_ok and "x-baton-daemon" not in hr,
              f"py={bp[:300]!r}\n       rs={br[:300]!r}")
        check("orphans: key order", bp.split(b'"idle_seconds"')[0][:200] == br.split(b'"idle_seconds"')[0][:200])

        # entity-logo (open path)
        cat = []
        try:
            cat = json.loads((HOME / ".baton/entities.json").read_text())
        except Exception:
            pass
        ids = [None, "nope:unknown"] + [e["id"] for e in cat[:4]]
        for eid in ids:
            p = "/api/entity-logo" + ("" if eid is None else f"?id={quote(eid)}")
            sp, hp, bp = req(live_port, "GET", p)
            sr, hr, br = req(19884, "GET", p)
            how = "proxied" if "x-baton-daemon" in hr else "native"
            check(f"entity-logo {eid}: {sp} ({how})",
                  (sp, bp, hp.get("content-type")) == (sr, br, hr.get("content-type")))
    finally:
        d.terminate()
        d.wait()


# ── Part B: sandboxed write endpoints ──────────────────────────────────

FAKES = {
    "claude": '#!/bin/sh\necho "fake-claude $*"\nsleep 0.5\necho "? for shortcuts"\nexec sleep 300\n',
    "codex": '#!/bin/sh\necho "fake-codex $*"\nexec sleep 300\n',
    "osascript": '#!/bin/sh\nf=$(mktemp "${OSA_LOG:-/tmp}/osa.XXXXXX")\nprintf "%s" "$2" > "$f"\n',
    "whisper-cpp": '#!/bin/sh\necho "  fake transcript $2  "\n',
}


class Side:
    def __init__(self, name, root, port):
        self.name, self.port = name, port
        self.root = root / name
        self.home = self.root / "home"
        self.tmuxdir = self.root / "t"
        self.osa = self.root / "osa"
        self.shim = self.root / "shim"
        for d in (self.home, self.tmuxdir, self.osa, self.home / "proj", self.shim):
            d.mkdir(parents=True, exist_ok=True)
        # The entity catalog is opt-in for the daemon (Python always reads it).
        (self.home / ".config/baton").mkdir(parents=True, exist_ok=True)
        (self.home / ".config/baton/config.toml").write_text("entities = true\n")
        real = shutil.which("tmux") or "/opt/homebrew/bin/tmux"
        (self.shim / "tmux").write_text(
            "#!/bin/sh\n"
            f'[ -d "{self.tmuxdir}" ] || {{ echo "parity: sandbox tmux dir gone, refusing" >&2; exit 97; }}\n'
            f'exec "{real}" -S "{self.sock}" "$@"\n')
        (self.shim / "tmux").chmod(0o755)
        self.proc = None
        self.events = None
        self.crids = {}  # call label → X-Request-Id

    def env(self, fakes):
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("TMUX", "CS_", "BATON_", "CSM_"))}
        env.update(HOME=str(self.home), TMUX_TMPDIR=str(self.tmuxdir), SHELL="/bin/sh",
                   PATH=f"{self.shim}:{fakes}:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin",
                   OSA_LOG=str(self.osa))
        return env

    def tmux(self, *args):
        # Pinned with -S: with only TMUX_TMPDIR, a missing dir makes tmux fall
        # back to the user's default server.
        if not self.tmuxdir.is_dir():
            raise RuntimeError(f"sandbox tmux dir {self.tmuxdir} missing; refusing tmux {args}")
        env = {k: v for k, v in os.environ.items() if not k.startswith("TMUX")}
        env["TMUX_TMPDIR"] = str(self.tmuxdir)
        env["SHELL"] = "/bin/sh"
        return subprocess.run([str(self.shim / "tmux"), *args], env=env, capture_output=True, text=True)

    @property
    def sock(self):
        return str(self.tmuxdir / f"tmux-{os.getuid()}" / "default")

    def norm(self, x):
        s = x if isinstance(x, str) else x.decode("utf-8", "replace")
        s = s.replace(str(self.home), "<HOME>")
        s = re.sub(r"(claude-|csm-bench-ctl-)(resume-)?[0-9a-f]{8}", r"<S:\2>", s)
        s = re.sub(r'"rid": "[0-9a-f]{8}"', '"rid": "<rid>"', s)
        s = re.sub(r"\d{4}-\d{2}-\d{2}_[0-9a-f]{8}\.", "<paste>.", s)
        s = re.sub(r"/tmp[A-Za-z0-9_]{8}\.wav", "/<tmp>.wav", s)
        return s


def run_script(side, logs_dir, devices_file):
    """The request script; returns [(label, normalised observation)]."""
    obs = []
    port = side.port
    n = [0]

    def call(label, method, path, body=None, raw=None, headers=None):
        n[0] += 1
        h = dict(headers or {})
        h["X-Request-Id"] = f"{n[0]:08x}"
        side.crids[label] = h["X-Request-Id"]
        st, hd, b = req(port, method, path, body=body, raw=raw, headers=h)
        obs.append((f"{label} → response", f"{st} {hd.get('content-type')} {side.norm(b)}"))
        return st, b

    def jb(b):
        try:
            return json.loads(b)
        except Exception:
            return {}

    def pane(name, settle=1.5):
        time.sleep(settle)
        cap = side.tmux("capture-pane", "-p", "-t", name).stdout.rstrip("\n")
        path = side.tmux("display", "-p", "-t", name, "#{pane_current_path}").stdout.strip()
        opts = side.tmux("show-options", "-t", name).stdout
        want = [l for l in opts.splitlines() if l.split(" ")[0] in ("status", "mouse", "copy-command", "window-size")]
        return f"cwd={path}\nopts={sorted(want)}\n{cap}"

    # fixtures
    proj = side.home / ".claude/projects/-proj"
    proj.mkdir(parents=True)
    lines = [{"type": "summary"}, "garbage", {"type": "user", "cwd": "~/proj", "sessionId": SID}]
    (proj / f"{SID}.jsonl").write_text("\n".join(x if isinstance(x, str) else json.dumps(x) for x in lines) + "\n")
    (side.home / ".claude/session_classifications.json").write_text(
        json.dumps({SID: {"title": 'Fix "quotes" in a very long title that exceeds sixty characters easily', "drift_topic": "tests"}}))
    (side.home / ".claude/resume_calibration.json").write_text(json.dumps({"records": [{"bytes": 1000, "ms": 10}, {"bytes": 5000, "ms": 0}]}))

    # spawn
    st, b = call("spawn prompt+model", "POST", "/api/spawn",
                 {"cwd": "~/proj", "prompt": "it's a \"test\" é", "model": "sonnet", "open_terminal": False})
    s1 = jb(b).get("session_id")
    obs.append(("spawn prompt+model → tmux", side.norm(pane(s1))))
    st, b = call("spawn default + X-Spawned-By", "POST", "/api/spawn", {}, headers={"X-Spawned-By": " parent-x "})
    s2 = jb(b).get("session_id")
    obs.append(("spawn default → tmux", side.norm(pane(s2, 1.0))))
    st, b = call("spawn parent/kind", "POST", "/api/spawn",
                 {"parent_id": "p1", "spawn_kind": "manual", "open_terminal": 0, "model": "opus", "cwd": ""})
    s3 = jb(b).get("session_id")
    call("spawn bad json", "POST", "/api/spawn", raw=b"{not json", headers={"Content-Type": "application/json"})
    call("spawn list body", "POST", "/api/spawn", [])
    call("spawn prompt non-str", "POST", "/api/spawn", {"prompt": 5})
    lineage = side.home / ".baton/session_lineage.json"
    # Keys are random tmux names (sorted on disk), so compare the entries
    # as a sorted list plus the file's layout with names/times masked.
    ltext = lineage.read_text()
    ldata = json.loads(ltext)
    obs.append(("lineage file layout", side.norm(re.sub(r'"spawned_at": \d+', '"spawned_at": <t>', ltext))
                .replace('"p1"', '"<p>"').replace('"parent-x"', '"<p>"').replace("null", '"<p>"')
                .replace('"manual"', '"<k>"').replace('"task"', '"<k>"').replace('"ios"', '"<k>"')))
    obs.append(("lineage entries", str(sorted(json.dumps({k: v for k, v in e.items() if k != "spawned_at"}, sort_keys=True)
                                              for e in ldata.values())) + f" keys sorted={list(ldata) == sorted(ldata)}"))

    # resume
    call("resume missing id", "POST", "/api/resume", {"session_id": "  "})
    st, b = call("resume fixture", "POST", "/api/resume", {"session_id": SID})
    r1 = jb(b).get("tmux_session")
    ok = side.events.wait_for(lambda e: e.get("type") == "resume-ready", 20)
    obs.append(("resume-ready broadcast", str(ok)))
    obs.append(("resume → tmux", side.norm(pane(r1, 0.2))))
    cal = json.loads((side.home / ".claude/resume_calibration.json").read_text())
    obs.append(("calibration records", str([(r["bytes"], "ms" in r) for r in cal["records"]])))
    call("resume again (reused)", "POST", "/api/resume", {"session_id": SID})
    st, b = call("resume unknown", "POST", "/api/resume", {"session_id": UNKNOWN_SID})
    r2 = jb(b).get("tmux_session")
    obs.append(("resume unknown → tmux", side.norm(pane(r2, 0.8)) if r2 else "no pane"))

    # respond
    call("respond", "POST", "/api/respond", {"session": s1, "key": "y"})
    call("respond default key", "POST", "/api/respond", {"session": s2})
    call("respond key int", "POST", "/api/respond", {"session": s1, "key": 5})
    obs.append(("respond → pane", side.norm(pane(s1, 0.8))))

    # hook-event
    call("hook-event", "POST", "/api/hook-event", {"type": "Stop", "session_id": "x", "é": "ü", "n": 1.5})
    call("hook-event list", "POST", "/api/hook-event", [1])
    call("hook-event empty", "POST", "/api/hook-event")

    # register-device
    call("register-device no token", "POST", "/api/register-device", {"platform": "ios"})
    call("register-device a", "POST", "/api/register-device", {"token": " tok-a ", "bundle_id": "com.x"})
    call("register-device b", "POST", "/api/register-device", {"token": "tok-b", "platform": None, "bundle_id": "é"})
    call("register-device a again", "POST", "/api/register-device", {"token": "tok-a", "platform": "ipad"})
    obs.append(("devices file", re.sub(r'"last_seen": [0-9.e+]+', '"last_seen": <t>', devices_file.read_text())))

    # paste-image
    png = b"\x89PNG\r\n\x1a\n" + bytes(range(256)) * 4
    call("paste-image empty", "POST", "/api/paste-image", raw=b"", headers={"Content-Type": "image/png"})
    for ct in ("image/png", "IMAGE/JPEG ", "image/heic", "application/x-foo"):
        st, b = call(f"paste-image {ct}", "POST", "/api/paste-image", raw=png, headers={"Content-Type": ct})
        p = jb(b).get("path")
        obs.append((f"paste-image {ct} file", str(p and Path(p).read_bytes() == png)))

    # voice
    call("voice short", "POST", "/api/voice", raw=b"\0" * 100)
    call("voice fake whisper-cpp", "POST", "/api/voice", raw=b"\1" * 5000, headers={"X-Session": s2})
    obs.append(("voice → pane", side.norm(pane(s2, 0.8))))

    # client-metrics
    recs = [{"kind": "request", "path": "/api/health", "status": 200, "t": 1790549973.380621, "ttfb_ms": 12,
             "note": "é" * 300, "nested": {"b": 1, "a": [1, 2.0]}, "rx_ts": "client-sent", "ok": True, "none": None}]
    recs.append({f"k{i:02d}": i for i in range(45)})
    recs.append("not a dict")
    call("client-metrics", "POST", "/api/client-metrics", {"records": recs, "device": 12345, "app_build": "b" * 50})
    call("client-metrics bad", "POST", "/api/client-metrics", {"records": {"a": 1}})
    call("client-metrics empty", "POST", "/api/client-metrics", {"records": None})
    client = (logs_dir / "cs-server.client.jsonl").read_text()
    client = re.sub(r'"rx_ts":"[0-9T:-]+"', '"rx_ts":"<t>"', client)
    client = re.sub(r'"rel":"[0-9a-f]+"', '"rel":"<rel>"', client)
    obs.append(("client log lines", client))

    # kill
    call("kill empty", "POST", "/api/kill-session", {"session": " "})
    call("kill unsafe", "POST", "/api/kill-session", {"session": "a;b"})
    call("kill s3", "POST", "/api/kill-session", {"session": s3})
    call("kill s3 again", "POST", "/api/kill-session", {"session": s3})
    obs.append(("tmux sessions after kill", side.norm(" ".join(sorted(
        side.tmux("list-sessions", "-F", "#{session_name}").stdout.split())))))

    # entity-logo (native path: fresh catalog in the sandbox HOME)
    logo = side.home / "logo.png"
    logo.write_bytes(png)
    (side.home / ".baton").mkdir(exist_ok=True)
    (side.home / ".baton/entities.json").write_text(json.dumps([
        {"id": "org:A", "type": "org", "logo_path": str(logo), "image_url": None},
        {"id": "org:B", "type": "org", "logo_path": None, "image_url": None},
        {"id": "org:C", "type": "org", "logo_path": str(side.home / "missing.svg"), "image_url": ""},
    ]))
    for eid in ("org:A", "org:B", "org:C", "org:Z"):
        st, hd, b = req(port, "GET", f"/api/entity-logo?id={eid}")
        obs.append((f"entity-logo {eid}", f"{st} {hd.get('content-type')} {hd.get('cache-control')} "
                    f"{b if st != 200 else len(b)} daemon-header={'x-baton-daemon' in hd}"))
    time.sleep(0.5)

    # AppleScript that would have run
    scripts = sorted(side.norm(p.read_text()) for p in side.osa.iterdir())
    obs.append(("osascript calls", "\n----\n".join(scripts)))
    return obs


def part_b(a, tmp):
    print("\nPart B — write endpoints in sandboxes (Python in-process vs daemon)")
    fakes = tmp / "fakes"
    fakes.mkdir()
    for name, body in FAKES.items():
        (fakes / name).write_text(body)
        (fakes / name).chmod(0o755)
    py, rs = Side("py", tmp, 19886), Side("rs", tmp, 19885)
    rs_logs = tmp / "rs-logs"
    rs_devices = tmp / "rs-devices.json"
    for s in (py, rs):  # each sandbox's tmux server, started with the sandbox env
        env = s.env(fakes)
        (s.tmuxdir / f"tmux-{os.getuid()}").mkdir(parents=True, mode=0o700, exist_ok=True)
        subprocess.run([str(s.shim / "tmux"), "-f", "/dev/null", "new-session", "-d", "-s", "csm-bench-ctl-keep"],
                       env=env, check=True)
    server = ("import sys; sys.path.insert(0, sys.argv[1]); import api_server as a; "
              "a.ThreadedHTTPServer(('127.0.0.1', 19886), a.APIHandler).serve_forever()")
    py.proc = subprocess.Popen([PY311, "-c", server, str(Path(a.ref) / "server")], env=py.env(fakes),
                               stdout=subprocess.DEVNULL, stderr=open(tmp / "py.err", "w"))
    renv = rs.env(fakes)
    renv.update(BATON_LOG_DIR=str(rs_logs), BATON_DEVICES_FILE=str(rs_devices),
                BATON_SESSION_PREFIX="csm-bench-ctl-")
    rs.proc = subprocess.Popen([a.bin, "serve", "--dev", "--listen", "127.0.0.1:19885",
                                "--upstream", "http://127.0.0.1:19886"], env=renv,
                               stdout=subprocess.DEVNULL, stderr=open(tmp / "rs.err", "w"))
    try:
        if not (wait_port(19886) and wait_port(19885)):
            check("sandbox servers start", False, (tmp / "py.err").read_text()[-500:] + (tmp / "rs.err").read_text()[-500:])
            return
        py.events, rs.events = Events(19886, "/api/events"), Events(19885, "/api/bench/events")
        for s in (py, rs):
            s.events.start()
            s.events.ready.wait(5)
        obs_py = run_script(py, py.home / "Library/Logs", py.home / ".config/baton/devices.json")
        obs_rs = run_script(rs, rs_logs, rs_devices)
        for (lp, vp), (lr, vr) in zip(obs_py, obs_rs):
            detail = f"py: {vp[:600]!r}\n       rs: {vr[:600]!r}"
            label = lp.removesuffix(" → response")
            if lp.endswith(" → response") and label in INTENDED:
                inc, st_py, st_rs = INTENDED[label]
                body = vr.split(" ", 2)[2] if vr.count(" ") >= 2 else ""
                try:
                    err_ok = isinstance(json.loads(body).get("error"), str)
                except Exception:
                    err_ok = False
                check(f"{lp}: Python {st_py}, daemon {st_rs} + JSON error ({inc}, intended)",
                      vp.startswith(f"{st_py} ") and vr.startswith(f"{st_rs} ") and err_ok, detail)
            elif lp == "resume unknown → tmux":
                check(f"{lp}: Python spawned a pane, daemon none (INC-012, intended)",
                      vp != "no pane" and vr == "no pane", detail)
            elif lp == "tmux sessions after kill":
                # Python's extra pane is the one its unknown-id resume spawned.
                check(f"{lp}: same, plus Python's unknown-id pane (INC-012, intended)",
                      vp.split().count("<S:>") == vr.split().count("<S:>") + 1
                      and [x for x in vp.split() if x != "<S:>"] == [x for x in vr.split() if x != "<S:>"], detail)
            elif lp == "osascript calls":
                from collections import Counter
                cp, cr = Counter(vp.split("\n----\n")), Counter(vr.split("\n----\n"))
                extra = list((cp - cr).elements())
                check(f"{lp}: same, plus Python's Terminal for the unknown-id pane (INC-012, intended)",
                      not (cr - cp) and len(extra) == 1 and 'tmux attach -t <S:>"' in extra[0], detail)
            else:
                check(lp, (lp, vp) == (lr, vr), detail)

        # broadcast events (compare as sets of canonical JSON, volatile fields
        # dropped). Not compared: Python's events for its unknown-id resume
        # (INC-012, the daemon answers 404), and `sessions_update` — the list
        # group's tmux deltas on the daemon's shared bus, timing-dependent and
        # not a control broadcast (already differing before fix/control).
        def canon(side):
            out = []
            for e in side.events.items:
                if e.get("type") == "sessions_update" or e.get("claude_session_id") == UNKNOWN_SID:
                    continue
                e = {k: v for k, v in e.items() if k not in ("timestamp", "duration_ms")}
                out.append(side.norm(json.dumps(e, sort_keys=True)))
            return sorted(out)
        ep, er = canon(py), canon(rs)
        check(f"broadcast events ({len(ep)})", ep == er, f"py-only={sorted(set(ep) - set(er))}\n       rs-only={sorted(set(er) - set(ep))}")
        ts = [e for e in rs.events.items if e.get("type") == "Stop"]
        check("hook-event timestamp is a float", bool(ts) and isinstance(ts[0].get("timestamp"), float))

        # structured logs: request lines joined by client request id
        def lines(p):
            return [json.loads(l) for l in p.read_text().splitlines()] if p.exists() else []
        rq_py = {r["crid"]: r for r in lines(py.home / "Library/Logs/cs-server.requests.jsonl") if r.get("crid")}
        rq_rs = {r["crid"]: r for r in lines(rs_logs / "cs-server.requests.jsonl") if r.get("crid")}
        keys_ok = all(list(r) == list(rq_py[c]) for c, r in rq_rs.items() if c in rq_py)
        # Intended divergences: only the status differs, and it's the one
        # INTENDED names.
        diverging = {rs.crids[k]: v[2] for k, v in INTENDED.items()}
        fields_ok = all((r["m"], py.norm(r["path"]), r["gz"]) == (rq_py[c]["m"], py.norm(rq_py[c]["path"]), rq_py[c]["gz"])
                        and r["st"] == diverging.get(c, rq_py[c]["st"])
                        for c, r in rq_rs.items() if c in rq_py)
        check(f"request log: {len(rq_rs)} lines, same keys/order and m/path/st/gz per crid (st as INTENDED where it diverges)",
              len(rq_rs) == len(rq_py) and keys_ok and fields_ok,
              f"py={len(rq_py)} rs={len(rq_rs)} keys_ok={keys_ok} fields_ok={fields_ok}")
        # Python logs an error for each of its INTENDED 500s; the daemon
        # answers those with a 400 and logs none.
        py_500s = sum(1 for v in INTENDED.values() if v[1] == 500)
        er_py = lines(py.home / "Library/Logs/cs-server.errors.jsonl")
        er_rs = lines(rs_logs / "cs-server.errors.jsonl")
        check(f"error log: {len(er_rs)} entries (Python {len(er_py)}, {py_500s} of them the intended 400s), same keys/order",
              len(er_py) - py_500s == len(er_rs) and all(list(x) == list(y) for x, y in zip(er_py, er_rs)),
              f"py={[list(x) for x in er_py[:1]]} rs={[list(x) for x in er_rs[:1]]}")
        ev_py = [e for e in lines(py.home / "Library/Logs/cs-server.events.jsonl") if e["event"] == "request_error"]
        ev_rs = [e for e in lines(rs_logs / "cs-server.events.jsonl") if e["event"] == "request_error"]
        check(f"events log: {len(ev_rs)} request_error (Python {len(ev_py)}, {py_500s} of them the intended 400s), same keys",
              len(ev_py) - py_500s == len(ev_rs) and all(list(x) == list(y) for x, y in zip(ev_py, ev_rs)))
        check("daemon wrote nothing under the sandbox HOME's real log dir",
              not (rs.home / "Library/Logs").exists())
    finally:
        for s in (py, rs):
            if s.proc:
                s.proc.send_signal(signal.SIGTERM)
                s.proc.wait(10)
            s.tmux("kill-server")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ref", default=str(REF_DEFAULT))
    ap.add_argument("--bin", default=os.environ.get("BATON_BIN", str(
        Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton")))
    ap.add_argument("--live", default="http://127.0.0.1:19877")
    ap.add_argument("--keep", action="store_true", help="keep the temp dir")
    ap.add_argument("--only", choices=["a", "b"])
    a = ap.parse_args()
    # Short path: tmux sockets live under it (sun_path is ~104 bytes).
    tmp = Path(tempfile.mkdtemp(prefix="csm-bench-ctl-", dir="/tmp"))
    try:
        if a.only in (None, "a"):
            part_a(a, tmp)
        if a.only in (None, "b"):
            part_b(a, tmp)
    finally:
        if a.keep:
            print(f"\nkept {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print(f"\n{len(PASSES)} passed, {len(FAILS)} failed")
    if FAILS:
        print("failed: " + "; ".join(FAILS))
        sys.exit(1)


if __name__ == "__main__":
    main()
