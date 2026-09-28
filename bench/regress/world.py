"""The sandbox ("fake world") the suite runs servers in, and the servers.

A World is a temp HOME with synthetic Claude transcripts and Codex rollouts,
a private tmux server (TMUX_TMPDIR, $TMUX unset) holding `csm-bench-*`
panes, fake `claude` / `codex` / `osascript` binaries first on PATH (they
record argv + cwd and stay alive), optional counting shims for `tmux` / `ps`,
and a fake Codex app-server (no tty, holds a rollout open) so the bare-`codex`
→ thread mapping can be exercised. Nothing outside the temp dirs is written;
the user's tmux server, ~/.claude, ~/.codex and ~/Library/Logs are never
touched.

Servers under test:
  * python — `python3.11 <ref>/server/api_server.py --port P --host 127.0.0.1`
             with HOME = the sandbox (so its logs land there too);
  * daemon — `<bin> serve --listen 127.0.0.1:D --upstream http://127.0.0.1:P`
             in front of a sandboxed python, BATON_LOG_DIR in the sandbox.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

from core import Client, spare_port, HOME

HERE = Path(__file__).resolve().parent
BENCH = HERE.parent
REF_DEFAULT = HOME / "GitHub/workbench-exporter-master/claude-session-manager"
BIN_DEFAULT = Path(os.environ.get("CARGO_TARGET_DIR", "/Volumes/ResearchSSD/xcode/cargo-baton-regress")) / "release/baton"
PY311 = shutil.which("python3.11") or "/opt/homebrew/bin/python3.11"
REAL_TMUX = shutil.which("tmux") or "/opt/homebrew/bin/tmux"


def U(n):
    """Deterministic, valid lowercase UUID per fixture."""
    h = f"{n:08x}"
    return f"{h}-{h[:4]}-4{h[5:8]}-8{h[1:4]}-{h}{h[:4]}"


IDS = {
    "CL_RUN": U(0xa1a1a1a1),     # claude --resume, pane csm-bench-claude
    "CL_FLAG": U(0xa2a2a2a2),    # claude --chrome ... --resume, pane csm-bench-chrome
    "CL_STALE": U(0xa3a3a3a3),   # stale tty-map entry points the codex pane at it
    "CL_BIG": U(0xa4a4a4a4),     # 1000 records (tail / since)
    "CL_DUP": U(0xa5a5a5a5),     # same id in two project dirs
    "CL_HEADLESS": U(0xa6a6a6a6),  # entrypoint sdk-cli
    "CL_BADUTF": U(0xa7a7a7a7),  # an invalid UTF-8 line
    "CL_MALFORMED": U(0xa8a8a8a8),  # message: 123 + truncated line
    "CL_MD": U(0xa9a9a9a9),      # unpaired <b> / <details>, 500-char token
    "CL_IDLE": U(0xaaaaaaaa),    # plain, not running (resume target)
    "CX_RUN": U(0xc1c1c1c1),     # codex resume, pane csm-bench-codex
    "CX_BARE": U(0xc2c2c2c2),    # bare codex, pane csm-bench-codexbare, app-server holds rollout
    "CX_IDLE": U(0xc3c3c3c3),    # codex, not running (resume target)
    "CX_SUB": U(0xc4c4c4c4),     # codex subagent of CX_RUN
}

PANES = {
    "claude": "csm-bench-claude",
    "chrome": "csm-bench-chrome",
    "codex": "csm-bench-codex",
    "codexbare": "csm-bench-codexbare",
    "input": "csm-bench-input",
    "send": "csm-bench-send",
    "ab": "csm-bench-ab",
    "seq": "csm-bench-seq",
}


def iso(dt):
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{dt.microsecond // 1000:03d}Z"


def claude_records(sid, cwd, n, start, tag, entrypoint="cli", step=1.0):
    out = []
    for i in range(n):
        ts = iso(start + timedelta(seconds=i * step))
        if i % 2 == 0:
            out.append({"type": "user", "sessionId": sid, "cwd": cwd, "entrypoint": entrypoint,
                        "timestamp": ts, "uuid": str(uuid.uuid4()),
                        "message": {"role": "user", "content": f"{tag} user message {i}"}})
        else:
            out.append({"type": "assistant", "sessionId": sid, "cwd": cwd, "timestamp": ts,
                        "uuid": str(uuid.uuid4()),
                        "message": {"role": "assistant", "model": "claude-opus-5",
                                    "content": [{"type": "text", "text": f"{tag} assistant reply {i}"}]}})
    return out


def codex_records(sid, cwd, n, start, tag, source="cli", parent=None):
    meta = {"id": sid, "cwd": cwd, "source": source, "timestamp": iso(start), "originator": "codex_cli_rs"}
    if parent:
        meta["parent_thread_id"] = parent
        meta["agent_nickname"] = "regress-helper"
    out = [{"type": "session_meta", "timestamp": iso(start), "payload": meta}]
    for i in range(n):
        ts = iso(start + timedelta(seconds=i + 1))
        role = "user" if i % 2 == 0 else "assistant"
        kind = "input_text" if role == "user" else "output_text"
        out.append({"type": "response_item", "timestamp": ts,
                    "payload": {"type": "message", "role": role,
                                "content": [{"type": kind, "text": f"{tag} codex {role} {i}"}]}})
    return out


def compact(r):
    """Transcripts are written compact (`"type":"assistant"`); the servers'
    byte-level prefilters depend on that."""
    return json.dumps(r, separators=(",", ":"), ensure_ascii=False).encode()


def dump(path, recs, extra=b""):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"".join(compact(r) + b"\n" for r in recs) + extra)


def rollout_path(home, sid, day):
    return home / f".codex/sessions/{day:%Y/%m/%d}/rollout-{day:%Y-%m-%dT%H-%M-%S}-{sid}.jsonl"


FAKE_CLAUDE = """#!/bin/bash
printf 'claude\\t%s\\t%s\\n' "$PWD" "$*" >> "{argv}"
echo "fake-claude $*"
echo "? for shortcuts"
while :; do sleep 3600; done
"""
FAKE_CODEX = """#!/bin/bash
printf 'codex\\t%s\\t%s\\n' "$PWD" "$*" >> "{argv}"
if [ $# -eq 0 ]; then exec -a codex /bin/sleep 86400; fi
echo "fake-codex $*"
while :; do sleep 3600; done
"""
FAKE_OSA = """#!/bin/sh
printf '%s\\n----\\n' "$*" >> "{osa}"
"""
# Every tmux call in the sandbox is pinned to an explicit socket (-S). With
# only TMUX_TMPDIR, tmux silently falls back to /tmp/tmux-$UID/default when
# that dir is missing — i.e. the user's real server (a `kill-server` there
# killed every live session on 2026-09-27). -S never falls back.
SHIM_TMUX = """#!/bin/sh
[ -d "{tdir}" ] || {{ echo "regress: sandbox tmux dir gone, refusing" >&2; exit 97; }}
printf 'tmux %s\\n' "$*" >> "{log}"
exec "{real}" -S "{sock}" "$@"
"""
SHIM_PS = """#!/bin/bash
printf 'ps %s\\n' "$*" >> "{log}"
if [ -f "{poison}" ]; then /bin/ps "$@"; printf '99999 1 /usr/bin/bad-\\xff\\xfe-arg\\n'; exit 0; fi
exec /bin/ps "$@"
"""


class World:
    def __init__(self, shims=True, keep=False):
        self.keep = keep
        # HOME lives outside /tmp (an allowed asset root) so root checks are
        # meaningful; tmux sockets need a short path, so they go to /tmp.
        self.root = Path(os.path.realpath(tempfile.mkdtemp(prefix="baton-regress-")))
        self.tdir = Path(os.path.realpath(tempfile.mkdtemp(prefix="csm-bench-rg-", dir="/tmp")))
        self.sock = self.tdir / "tmux.sock"
        self.home = self.root / "home"
        self.fakes = self.root / "fakes"
        self.argv_log = self.root / "argv.log"
        self.spawn_log = self.root / "spawns.log"
        self.osa_log = self.root / "osa.log"
        self.poison = self.root / "ps-poison"
        self.token = "regress-" + uuid.uuid4().hex[:12]
        self.ids = dict(IDS)
        self.panes = dict(PANES)
        self.procs = []           # helper processes (app-server)
        self.shims = shims
        self.proj = self.home / "proj"
        self.bare_cwd = self.home / "proj-bare"
        for d in (self.home, self.fakes, self.proj, self.bare_cwd):
            d.mkdir(parents=True, exist_ok=True)
        self.argv_log.touch()
        self.spawn_log.touch()
        self._fakes()
        self._fixtures()
        self._assets()
        self._tmux_world()

    # ── environment ──────────────────────────────────────────────────
    def env(self, **extra):
        keep = ("LANG", "LC_ALL", "LC_CTYPE", "USER", "LOGNAME", "TERM")
        env = {k: os.environ[k] for k in keep if k in os.environ}
        env.update(HOME=str(self.home), TMUX_TMPDIR=str(self.tdir), SHELL="/bin/sh",
                   TMPDIR=str(self.root) + "/", PYTHONDONTWRITEBYTECODE="1",
                   PATH=f"{self.fakes}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                   BATON_DISABLE_CLASSIFY="1", REGRESS_ARGV_LOG=str(self.argv_log), HISTFILE="/dev/null",
                   # no mlx-whisper backend (it would download a model into the sandbox HOME)
                   CS_MLX_PYTHON=str(self.root / "no-mlx-python"))
        # Python's `markdown` lives in the user site-packages, which Python
        # finds through HOME; keep it visible with the sandbox HOME.
        ub = HOME / "Library/Python/3.11"
        if ub.is_dir():
            env["PYTHONUSERBASE"] = str(ub)
        env.setdefault("LANG", "en_US.UTF-8")
        env.update(extra)
        return env

    def tmux(self, *args, check=False):
        if not self.tdir.is_dir() or not self.tdir.name.startswith("csm-bench-rg-"):
            raise RuntimeError(f"sandbox tmux dir {self.tdir} missing; refusing to run tmux {args}")
        env = self.env()
        return subprocess.run([REAL_TMUX, "-S", str(self.sock), *args], env=env,
                              capture_output=True, text=True, check=check)

    def argv(self):
        """[(prog, cwd, args)] recorded by the fake binaries."""
        out = []
        for line in self.argv_log.read_text(errors="replace").splitlines():
            p = line.split("\t")
            if len(p) == 3:
                out.append(tuple(p))
        return out

    def spawns(self):
        return self.spawn_log.read_text(errors="replace").splitlines()

    # ── construction ─────────────────────────────────────────────────
    def _fakes(self):
        files = {"claude": FAKE_CLAUDE.format(argv=self.argv_log),
                 "codex": FAKE_CODEX.format(argv=self.argv_log),
                 "osascript": FAKE_OSA.format(osa=self.osa_log)}
        # The tmux shim is always installed (it pins the socket); it only
        # counts calls when shims are on.
        files["tmux"] = SHIM_TMUX.format(log=self.spawn_log if self.shims else "/dev/null",
                                         real=REAL_TMUX, tdir=self.tdir, sock=self.sock)
        if self.shims:
            files["ps"] = SHIM_PS.format(log=self.spawn_log, poison=self.poison)
        for name, body in files.items():
            p = self.fakes / name
            p.write_text(body)
            p.chmod(0o755)

    def claude_file(self, sid, project=None):
        project = project or "-" + str(self.proj).replace("/", "-").lstrip("-")
        return self.home / ".claude/projects" / project / f"{sid}.jsonl"

    def _fixtures(self):
        now = datetime.now(timezone.utc)
        I, cwd = self.ids, str(self.proj)
        t = lambda mins: now - timedelta(minutes=mins)  # noqa: E731
        dump(self.claude_file(I["CL_RUN"]), claude_records(I["CL_RUN"], cwd, 24, t(50), "running-claude"))
        dump(self.claude_file(I["CL_FLAG"]), claude_records(I["CL_FLAG"], cwd, 22, t(48), "flagged-claude"))
        dump(self.claude_file(I["CL_STALE"]), claude_records(I["CL_STALE"], cwd, 21, t(46), "stale-ttymap"))
        big = claude_records(I["CL_BIG"], cwd, 1000, t(200), "big", step=0.5)
        dump(self.claude_file(I["CL_BIG"]), big)
        self.big_ts = [r["timestamp"] for r in big]
        # Same id, two project dirs (project moved): the 30-message copy wins.
        dump(self.claude_file(I["CL_DUP"], "-old-location-proj"), claude_records(I["CL_DUP"], cwd, 4, t(44), "dup-stub"))
        dump(self.claude_file(I["CL_DUP"]), claude_records(I["CL_DUP"], cwd, 30, t(43), "dup-full"))
        dump(self.claude_file(I["CL_HEADLESS"]),
             claude_records(I["CL_HEADLESS"], cwd, 24, t(42), "headless-run", entrypoint="sdk-cli"))
        dump(self.claude_file(I["CL_BADUTF"]), claude_records(I["CL_BADUTF"], cwd, 21, t(40), "badutf"),
             extra=b'{"type":"user","message":{"role":"user","content":"bad \xff\xfe bytes"}}\n'
             + b"".join(compact(r) + b"\n" for r in claude_records(I["CL_BADUTF"], cwd, 2, t(39), "badutf-after")))
        # Clean at start; the INC-037 check appends the malformed lines (and
        # removes them again) so one bad record can't take the whole run down.
        dump(self.claude_file(I["CL_MALFORMED"]), claude_records(I["CL_MALFORMED"], cwd, 21, t(38), "malformed"))
        md = claude_records(I["CL_MD"], cwd, 21, t(36), "markdown")
        md[1]["message"]["content"][0]["text"] = "a <b> zzAFTERBOLD c"
        md[3]["message"]["content"][0]["text"] = "<details>zzAFTERDETAILS"
        md[5]["message"]["content"][0]["text"] = "long " + "x" * 500 + " zzLONGTOKEN"
        dump(self.claude_file(I["CL_MD"]), md)
        dump(self.claude_file(I["CL_IDLE"]), claude_records(I["CL_IDLE"], cwd, 22, t(34), "idle-claude"))

        day = now - timedelta(hours=1)
        self.rollouts = {}
        for key, n, tag, cwd_, src, parent in (
                ("CX_RUN", 22, "running-codex", cwd, "cli", None),
                ("CX_BARE", 22, "bare-codex", str(self.bare_cwd), "cli", None),
                ("CX_IDLE", 22, "idle-codex", cwd, "cli", None),
                ("CX_SUB", 6, "sub-codex", cwd, {"subagent": {"thread_spawn": {"parent_thread_id": I["CX_RUN"], "depth": 1}}}, I["CX_RUN"])):
            p = rollout_path(self.home, I[key], day)
            dump(p, codex_records(I[key], cwd_, n, t(30), tag, source=src, parent=parent))
            self.rollouts[key] = p
        (self.home / ".claude/plans").mkdir(parents=True, exist_ok=True)

    def _assets(self):
        h = self.home
        # The daemon's default asset roots are the home dir minus private
        # dirs; the Python reference has a fixed list. The sandbox daemon
        # gets that list through its config file — what `baton setup`
        # migrates for an existing install (docs/migrating.md) — so the
        # "outside every root" cases below stay meaningful for both.
        cfg = h / ".config/baton/config.toml"
        cfg.parent.mkdir(parents=True, exist_ok=True)
        cfg.write_text('asset_roots = ["~/Documents", "~/Downloads", "~/Desktop", "/tmp", "~/GitHub",\n'
                       '               "~/Superapp-Projects", "~/.baton/pastes", "~/.claude/plans"]\n')
        self.asset_ok = {}
        for root in ("Documents", "Downloads", "Desktop", "GitHub/repo", "Superapp-Projects",
                     ".baton/pastes", ".claude/plans"):
            p = h / root / f"ok-{root.replace('/', '-').strip('.')}.txt"
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(f"allowed {root}\n")
            self.asset_ok[root] = p
        tmpf = self.tdir / "ok-tmp.txt"
        tmpf.write_text("allowed tmp\n")
        self.asset_ok["/tmp"] = tmpf
        repo = h / "GitHub/repo"
        self.asset_secret = {}
        for rel in (".env", ".env.local", "AuthKey_X.p8", "server.pem", "id_ed25519", ".git/config",
                    "auth.json", ".credentials.json", "secrets.yaml"):
            p = repo / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text("SECRET=1\n")
            self.asset_secret[rel] = p
        (h / ".ssh").mkdir(exist_ok=True)
        (h / ".ssh/id_x").write_text("PRIVATE KEY\n")
        (h / "outside.txt").write_text("not under any root\n")
        os.symlink(h / "outside.txt", repo / "link-to-outside.txt")
        (h / ".claude/.credentials.json").write_text('{"token": "x"}')
        (h / ".codex").mkdir(exist_ok=True)
        (h / ".codex/auth.json").write_text('{"token": "x"}')
        (h / "Documents/café é.txt").write_text("unicode name\n")
        (h / "Documents/big.md").write_text("# big\n" + "line of text\n" * 4000)

    def _tmux_world(self):
        P, I = self.panes, self.ids
        app = f"{sys.executable} {BENCH / 'tmux_bench_app.py'}"
        self.app_log = {k: self.root / f"app-{k}.log" for k in ("input", "send", "ab")}
        specs = [
            (P["claude"], str(self.proj), f"claude --resume {I['CL_RUN']}"),
            (P["chrome"], str(self.proj), f"claude --chrome --dangerously-skip-permissions --resume {I['CL_FLAG']}"),
            (P["codex"], str(self.proj), f"codex resume {I['CX_RUN']}"),
            (P["codexbare"], str(self.bare_cwd), "codex"),
            (P["input"], str(self.proj), f"{app} input {self.app_log['input']}"),
            (P["send"], str(self.proj), f"{app} input {self.app_log['send']}"),
            (P["ab"], str(self.proj), f"{app} input {self.app_log['ab']}"),
            (P["seq"], str(self.proj), "seq 1 3000; exec sleep 100000"),
        ]
        first = True
        for name, cwd, cmd in specs:
            args = ["new-session", "-d", "-s", name, "-x", "100", "-y", "30", "-c", cwd, cmd]
            if first:
                args = ["-f", "/dev/null"] + args
                first = False
            self.tmux(*args, check=True)
        # window-size manual so a server's -x/-y probe doesn't fight clients
        # Fake Codex app-server: no controlling tty, holds CX_BARE's rollout.
        rollout = str(self.rollouts["CX_BARE"])
        self.procs.append(subprocess.Popen(
            ["/bin/bash", "-c", 'exec 3<"$1"; exec -a "codex app-server" /bin/sleep 86400', "_", rollout],
            env=self.env(), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, start_new_session=True))
        # Stale tty-map entry: the Codex pane's tty → a third (Claude) session.
        tty = self.tmux("display", "-p", "-t", "=" + P["codex"] + ":", "#{pane_tty}").stdout.strip()
        self.codex_tty = tty
        tm = self.home / ".claude/tty_session_map.json"
        tm.write_text(json.dumps({tty: {"session_id": I["CL_STALE"], "ts": time.time()}}))
        # Let fakes write their argv lines and the input apps draw.
        deadline = time.time() + 5
        while time.time() < deadline:
            progs = [a[0] for a in self.argv()]
            if progs.count("claude") >= 2 and progs.count("codex") >= 2:
                break
            time.sleep(0.05)
        time.sleep(0.3)
        self.argv_baseline = len(self.argv())

    def pane_names(self):
        return self.tmux("list-sessions", "-F", "#{session_name}").stdout.split()

    def close(self):
        for p in self.procs:
            try:
                p.kill()
                p.wait(3)
            except Exception:
                pass
        if self.tdir.is_dir():
            self.tmux("kill-server")
            time.sleep(0.3)     # let the panes' shells exit before the tree goes
        if not self.keep:
            shutil.rmtree(self.root, ignore_errors=True)
            shutil.rmtree(self.tdir, ignore_errors=True)


# ── servers ─────────────────────────────────────────────────────────────

class Server:
    def __init__(self, label, proc, port, err_path):
        self.label, self.proc, self.port, self.err_path = label, proc, port, err_path

    def stop(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(5)


def start_python(world, ref, port=None, tag="py", extra_env=None):
    port = port or spare_port()
    err = world.root / f"{tag}-{port}.err"
    env = world.env(BATON_BEARER=world.token, **(extra_env or {}))
    p = subprocess.Popen([PY311, str(Path(ref) / "server/api_server.py"), "--port", str(port), "--host", "127.0.0.1"],
                         env=env, cwd=str(world.home), stdin=subprocess.DEVNULL,
                         stdout=open(world.root / f"{tag}-{port}.out", "w"), stderr=open(err, "w"))
    return Server(tag, p, port, err)


def start_daemon(world, binary, upstream_port, port=None, tag="daemon", extra_env=None):
    port = port or spare_port()
    err = world.root / f"{tag}-{port}.err"
    env = world.env(BATON_BEARER=world.token, BATON_LOG_DIR=str(world.root / f"{tag}-logs"),
                    BATON_DEVICES_FILE=str(world.root / f"{tag}-devices.json"), **(extra_env or {}))
    p = subprocess.Popen([str(binary), "serve", "--listen", f"127.0.0.1:{port}",
                          "--upstream", f"http://127.0.0.1:{upstream_port}"],
                         env=env, cwd=str(world.home), stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=open(err, "w"))
    return Server(tag, p, port, err)


def capture_first_list(client, proc, secs=60):
    """Poll until the server answers /api/local-sessions with 200; returns
    (seconds since call, first-200 Resp, first-health seconds). Connection
    refused / 503 while starting are retried; the FIRST 200 is kept — that's
    what a phone reconnecting right after a restart gets (prewarm race)."""
    t0 = time.time()
    health_s = None
    while time.time() - t0 < secs:
        if proc.poll() is not None:
            return None, None, health_s
        if health_s is None:
            h = client.get("/api/health", timeout=2)
            if h.status == 200:
                health_s = time.time() - t0
            else:
                time.sleep(0.02)
                continue
        r = client.get("/api/local-sessions", timeout=60)
        if r.status == 200:
            return time.time() - t0, r, health_s
        time.sleep(0.05)
    return None, None, health_s


class Target:
    """A server under test. impl: python | daemon (what the xfail maps key on)."""

    def __init__(self, name, impl, client, servers=(), world=None):
        self.name, self.impl, self.client = name, impl, client
        self.servers = list(servers)
        self.world = world
        self.first_list = None
        self.first_list_s = None
        self.first_health_s = None

    @property
    def main(self):
        return self.servers[-1] if self.servers else None

    def log_dir(self):
        if self.impl == "daemon":
            return self.world.root / "daemon-logs"
        return self.world.home / "Library/Logs"

    def stop(self):
        for s in reversed(self.servers):
            s.stop()


def sandbox_target(kind, world, ref, binary):
    py = start_python(world, ref)
    servers = [py]
    if kind == "python":
        main = py
    else:
        # The upstream must be up first; the daemon's prewarm then starts
        # at the daemon's own process start.
        up = Client("127.0.0.1", py.port, world.token)
        from core import wait_http
        if wait_http(up, 60) is None:
            raise RuntimeError("sandbox python upstream did not start: " + py.err_path.read_text()[-800:])
        main = start_daemon(world, binary, py.port)
        servers.append(main)
    client = Client("127.0.0.1", main.port, world.token)
    t = Target(kind, kind, client, servers, world)
    t.first_list_s, t.first_list, t.first_health_s = capture_first_list(client, main.proc)
    if t.first_list is None:
        t.stop()
        raise RuntimeError(f"{kind} did not serve /api/local-sessions: " + main.err_path.read_text()[-1500:])
    return t


def live_target(url, token):
    from urllib.parse import urlparse
    u = urlparse(url)
    client = Client(u.hostname, u.port or 80, token)
    h = client.get("/api/health", timeout=10)
    j = h.json() or {}
    impl = "daemon" if j.get("daemon") else "python"
    return Target(url, impl, client)
