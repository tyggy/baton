#!/usr/bin/env python3
"""A/B profiler: the Python reference server vs the Rust daemon, side by side.

    python3 bench/ab.py [--live http://127.0.0.1:19877] [--workload checks,replay,cold]
                        [--reps 3] [--load N] [--conc N] [--json out.json]

Both servers run at the same time and get identical requests, interleaved
per request (A then B, order flipped every pair) so drift in machine load
hits both equally. Reported per endpoint: p50/p95/p99 for each side, the
daemon/python ratio of medians with a bootstrap 95% CI over the pairs, and
how many pairs differed in status or (normalised) JSON body. Per server
process: CPU seconds used during the run and peak RSS (sampled with
`ps -o rss=,time=` every 0.25 s).

Modes
  sandbox (default)  a fresh fake world (bench/regress/world.py): Python on a
                     spare port, the daemon on another with its own sandboxed
                     Python upstream (so proxy work isn't billed to the Python
                     side). Private tmux server, temp HOME, nothing live.
  --live URL         Python = the running server at URL (GET only); the daemon
                     is started on a spare port with --upstream URL against the
                     real HOME (read-only; its logs go to a temp dir).

Workloads
  checks  the read requests the regression checks issue (health, list,
          search-index, background, session/messages/children per row,
          sessions, orphans; sandbox also asset + pane)
  replay  the endpoint mix and concurrency of real phone traffic from
          ~/Library/Logs/cs-server.requests.jsonl (non-local clients, GET,
          no streams, no /api/bench); ids are real in live mode and mapped to
          fixture ids in the sandbox
  cold    spawn → first 200 on /api/health and first full /api/local-sessions
          (sandbox: both sides, fresh process per rep, alternating; live: the
          daemon only — a second Python would log into the real HOME)

--load N runs N busy-loop CPU hogs for the whole run (like bench/run.py's
pressure).
"""
import argparse
import json
import math
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "regress"))

from core import Client, live_token, pct, proc_stats, spare_port, wait_http  # noqa: E402
from world import (BIN_DEFAULT, REF_DEFAULT, World, capture_first_list,  # noqa: E402
                   start_daemon, start_python)

REQ_LOG = Path.home() / "Library/Logs/cs-server.requests.jsonl"
VOLATILE = {"age_str", "rid", "uptime", "pid", "started", "idle_seconds", "daemon", "api_version", "min_client",
            "release", "src", "files", "python", "memory_pressure", "cpu_temp", "port"}


# ── sampling ────────────────────────────────────────────────────────────

class Sampler(threading.Thread):
    """Peak RSS and CPU time of a set of pids, via ps."""

    def __init__(self, pids):
        super().__init__(daemon=True)
        self.pids = dict(pids)          # label → pid
        self.first, self.last, self.peak = {}, {}, defaultdict(int)
        self.stop_ev = threading.Event()

    def sample(self):
        for label, pid in self.pids.items():
            st = proc_stats(pid)
            if st is None:
                continue
            rss, cpu = st
            self.first.setdefault(label, cpu)
            self.last[label] = cpu
            self.peak[label] = max(self.peak[label], rss)

    def run(self):
        while not self.stop_ev.is_set():
            self.sample()
            self.stop_ev.wait(0.25)

    def stop(self):
        self.stop_ev.set()
        self.join(2)
        self.sample()
        return {l: {"cpu_s": round(self.last.get(l, 0) - self.first.get(l, 0), 2),
                    "peak_rss_mb": round(self.peak[l] / 1024, 1)} for l in self.pids}


def hogs(n):
    return [subprocess.Popen([sys.executable, "-c", "while True: pass"]) for _ in range(n)]


# ── comparison ──────────────────────────────────────────────────────────

def norm_body(b):
    try:
        j = json.loads(b)
    except Exception:
        return b

    def strip(x):
        if isinstance(x, dict):
            return {k: strip(v) for k, v in x.items() if k not in VOLATILE}
        if isinstance(x, list):
            return [strip(v) for v in x]
        return x
    return json.dumps(strip(j), sort_keys=True)


def first_diff(a, b):
    try:
        ja, jb = json.loads(a), json.loads(b)
    except Exception:
        return f"bytes differ ({len(a)} vs {len(b)})"

    def walk(x, y, path):
        if type(x) is not type(y):
            return f"{path}: {str(x)[:60]} vs {str(y)[:60]}"
        if isinstance(x, dict):
            for k in sorted(set(x) | set(y)):
                if k in VOLATILE:
                    continue
                if k not in x or k not in y:
                    return f"{path}.{k}: only in {'python' if k in x else 'daemon'}"
                d = walk(x[k], y[k], f"{path}.{k}")
                if d:
                    return d
            return None
        if isinstance(x, list):
            if len(x) != len(y):
                return f"{path}: len {len(x)} vs {len(y)}"
            for i, (u, v) in enumerate(zip(x, y)):
                d = walk(u, v, f"{path}[{i}]")
                if d:
                    return d
            return None
        if x == y:
            return None
        if isinstance(x, str) and isinstance(y, str):
            i = next((k for k, (u, v) in enumerate(zip(x, y)) if u != v), min(len(x), len(y)))
            return f"{path} @{i}: {x[max(0, i - 30):i + 40]!r} vs {y[max(0, i - 30):i + 40]!r}"
        return f"{path}: {str(x)[:60]!r} vs {str(y)[:60]!r}"
    return walk(ja, jb, "$")


def bootstrap_ratio(a, b, n=1000, seed=7):
    """Ratio of medians b/a with a 95% CI, resampling the pairs."""
    if len(a) < 3:
        return float("nan"), float("nan"), float("nan")
    rnd = random.Random(seed)
    pairs = list(zip(a, b))
    base = pct(b, 50) / max(pct(a, 50), 1e-6)
    rs = []
    for _ in range(n):
        s = [pairs[rnd.randrange(len(pairs))] for _ in pairs]
        rs.append(pct([y for _, y in s], 50) / max(pct([x for x, _ in s], 50), 1e-6))
    rs.sort()
    return base, rs[int(0.025 * n)], rs[int(0.975 * n) - 1]


def endpoint_of(path):
    p = path.split("?")[0]
    return re.sub(r"^(/api/(session|messages|tail|children|pane|pane-stream))/.*", r"\1/{id}", p)


class Run:
    def __init__(self, a, b):
        self.a, self.b = a, b                       # Clients: python, daemon
        self.lat = defaultdict(lambda: ([], []))    # endpoint → (python ms, daemon ms)
        self.diffs = defaultdict(list)
        self.errors = Counter()
        self.flip = 0
        self.lock = threading.Lock()

    def get(self, c, path):
        for _ in range(4):
            r = c.get(path, timeout=30)
            if r.status or not re.search(r"reset|Broken pipe|Invalid argument", r.error or ""):
                return r
            with self.lock:
                self.errors["reset " + ("python" if c is self.a else "daemon")] += 1
            time.sleep(0.05)
        return r

    def pair(self, path, headers=None):
        with self.lock:
            self.flip ^= 1
            ab = self.flip
        if ab:
            ra = self.get(self.a, path)
            rb = self.get(self.b, path)
        else:
            rb = self.get(self.b, path)
            ra = self.get(self.a, path)
        ep = endpoint_of(path)
        with self.lock:
            if ra.status == 0 or rb.status == 0:
                self.errors[f"{ep}: transport error"] += 1
                return
            la, lb = self.lat[ep]
            la.append(ra.ms)
            lb.append(rb.ms)
            if ra.status != rb.status:
                self.diffs[ep].append(f"{path}: status {ra.status} vs {rb.status}")
            elif ra.status == 200 and norm_body(ra.body) != norm_body(rb.body):
                self.diffs[ep].append(f"{path}: {first_diff(ra.body, rb.body)}")

    def table(self, title):
        rows = []
        print(f"\n{title}")
        print(f"  {'endpoint':<24} {'n':>5}  {'python p50/p95/p99 ms':>24}  {'daemon p50/p95/p99 ms':>24}  "
              f"{'ratio d/p (95% CI)':>22}  diffs")
        for ep in sorted(self.lat, key=lambda e: -len(self.lat[e][0])):
            a, b = self.lat[ep]
            r, lo, hi = bootstrap_ratio(a, b)
            f = lambda xs: "%6.1f %6.1f %6.1f" % (pct(xs, 50), pct(xs, 95), pct(xs, 99))  # noqa: E731
            nd = len(self.diffs.get(ep, []))
            print(f"  {ep:<24} {len(a):>5}  {f(a):>24}  {f(b):>24}  {r:6.2f} ({lo:.2f}–{hi:.2f})  {nd}")
            rows.append({"endpoint": ep, "n": len(a),
                         "python": {"p50": pct(a, 50), "p95": pct(a, 95), "p99": pct(a, 99)},
                         "daemon": {"p50": pct(b, 50), "p95": pct(b, 95), "p99": pct(b, 99)},
                         "ratio_p50": r, "ci95": [lo, hi], "diffs": nd,
                         "diff_examples": self.diffs.get(ep, [])[:3]})
        for ep, ds in self.diffs.items():
            print(f"  diff {ep}: {len(ds)}× e.g. {ds[0][:220]}")
        if self.errors:
            print(f"  errors: {dict(self.errors)}")
        return rows


# ── workloads ───────────────────────────────────────────────────────────

def check_paths(list_payload, world):
    rows = [r for g in ("recent", "momentum") for r in list_payload.get(g, [])]
    rows += [r for v in (list_payload.get("areas") or {}).values() for r in v]
    ids = list(dict.fromkeys(r["id"] for r in rows))
    paths = ["/api/health", "/api/local-sessions", "/api/search-index", "/api/background", "/api/sessions",
             "/api/orphans"]
    for sid in ids:
        paths += [f"/api/session/{sid}", f"/api/messages/{sid}?limit=50", f"/api/children/{sid}"]
    if world:
        from urllib.parse import quote
        paths += [f"/api/asset?path={quote(str(p))}" for p in list(world.asset_ok.values())[:4]]
        paths += [f"/api/pane/{world.panes['seq']}"]
    return paths


def replay_shape(max_lines=60000):
    """(weighted path list, concurrency) from the live request log: GETs by
    non-local clients (the phone), no streams, no /api/bench."""
    lines = REQ_LOG.read_text(errors="replace").splitlines()[-max_lines:]
    reqs, per_sec = [], defaultdict(float)
    for l in lines:
        try:
            r = json.loads(l)
        except Exception:
            continue
        if r.get("m") != "GET" or str(r.get("client", "")).startswith("127.0.0.1"):
            continue
        p = r.get("path", "")
        if p.startswith(("/api/events", "/api/tail/", "/api/pane-stream/", "/api/bench")):
            continue
        reqs.append(p)
        per_sec[r.get("ts")] += (r.get("ms") or 0) / 1000.0     # busy-seconds started in that second
    busy = sorted(per_sec.values())
    conc = max(1, min(6, math.ceil(pct(busy, 95)))) if busy else 1
    return reqs, conc


def map_to_sandbox(paths, world, ids):
    from urllib.parse import quote
    out = []
    assets = [str(p) for p in world.asset_ok.values()]
    rnd = random.Random(3)
    for p in paths:
        ep = endpoint_of(p)
        q = ("?" + p.split("?", 1)[1]) if "?" in p else ""
        if ep in ("/api/session/{id}", "/api/messages/{id}", "/api/children/{id}"):
            out.append(ep.replace("{id}", rnd.choice(ids)) + (q if "limit" in q else ""))
        elif ep == "/api/pane/{id}":
            out.append(f"/api/pane/{world.panes['seq']}")
        elif ep == "/api/asset":
            out.append("/api/asset?path=" + quote(rnd.choice(assets)))
        elif ep in ("/api/health", "/api/local-sessions", "/api/sessions", "/api/search-index", "/api/background",
                    "/api/orphans", "/api/temp", "/api/plugins"):
            out.append(ep)
        elif ep == "/api/browse":
            out.append("/api/browse?dir=" + quote(str(world.home / "Documents")))
    return out


def run_pairs(run, paths, conc):
    if conc <= 1:
        for p in paths:
            run.pair(p)
        return
    with ThreadPoolExecutor(conc) as pool:
        list(pool.map(run.pair, paths))


def cold_start(world, ref, binary, reps, upstream_port, live_url=None):
    out = {"python": {"health_s": [], "list_s": []}, "daemon": {"health_s": [], "list_s": []}}
    sides = ["daemon"] if live_url else ["python", "daemon"]
    for i in range(reps):
        order = sides if i % 2 == 0 else list(reversed(sides))
        for side in order:
            port = spare_port()
            t0 = time.time()
            if side == "python":
                srv = start_python(world, ref, port=port, tag=f"cold{i}")
            elif live_url:
                logs = Path(tempfile.mkdtemp(prefix="baton-ab-logs-"))
                env = dict(os.environ, BATON_LOG_DIR=str(logs), BATON_DEVICES_FILE=str(logs / "devices.json"))
                p = subprocess.Popen([str(binary), "serve", "--listen", f"127.0.0.1:{port}", "--upstream", live_url],
                                     env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                from world import Server
                srv = Server("daemon", p, port, logs / "err")
            else:
                srv = start_daemon(world, binary, upstream_port, port=port, tag=f"cold{i}")
            c = Client("127.0.0.1", port, world.token if world and not live_url else live_token())
            list_s, _, health_s = capture_first_list(c, srv.proc, secs=120)
            gap = time.time() - t0 - (list_s or 0)     # spawn → first poll (Popen etc.)
            gap = max(0.0, gap) if list_s else 0.0
            out[side]["health_s"].append(round((health_s or float("nan")) + gap, 3))
            out[side]["list_s"].append(round((list_s or float("nan")) + gap, 3))
            srv.stop()
    return out


# ── main ────────────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--live", help="compare against a running Python server (GET only)")
    ap.add_argument("--workload", default="checks,replay,cold")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--load", type=int, default=0, help="CPU hog processes during the run")
    ap.add_argument("--conc", type=int, default=0, help="override replay concurrency")
    ap.add_argument("--replay-n", type=int, default=600, help="requests per replay rep")
    ap.add_argument("--bin", default=os.environ.get("BATON_BIN", str(BIN_DEFAULT)))
    ap.add_argument("--ref", default=str(REF_DEFAULT))
    ap.add_argument("--json")
    a = ap.parse_args()
    work = set(a.workload.split(","))
    result = {"mode": "live" if a.live else "sandbox", "reps": a.reps, "load": a.load}
    world, servers, hog = None, [], []
    try:
        if a.live:
            from urllib.parse import urlparse
            u = urlparse(a.live)
            tok = live_token()
            py = Client(u.hostname, u.port, tok)
            health = py.get("/api/health").json() or {}
            ppid = health.get("pid")
            logs = Path(tempfile.mkdtemp(prefix="baton-ab-logs-"))
            dport = spare_port()
            env = dict(os.environ, BATON_LOG_DIR=str(logs), BATON_DEVICES_FILE=str(logs / "devices.json"))
            dproc = subprocess.Popen([a.bin, "serve", "--listen", f"127.0.0.1:{dport}", "--upstream", a.live],
                                     env=env, stdout=subprocess.DEVNULL, stderr=open(logs / "daemon.err", "w"))
            servers.append(dproc)
            dm = Client("127.0.0.1", dport, tok)
            if wait_http(dm, 30) is None:
                sys.exit("daemon did not start")
            dm.get("/api/local-sessions", timeout=120)
            pids = {"python (live)": ppid, "daemon": dproc.pid}
        else:
            world = World()
            p = start_python(world, a.ref, tag="ab-py")
            u = start_python(world, a.ref, tag="ab-upstream")
            servers += [p, u]
            py = Client("127.0.0.1", p.port, world.token)
            for c in (py, Client("127.0.0.1", u.port, world.token)):
                if wait_http(c, 60) is None:
                    sys.exit("sandbox python did not start")
            d = start_daemon(world, a.bin, u.port, tag="ab-daemon")
            servers.append(d)
            dm = Client("127.0.0.1", d.port, world.token)
            capture_first_list(dm, d.proc)
            capture_first_list(py, p.proc)
            pids = {"python": p.proc.pid, "daemon": d.proc.pid, "daemon's python upstream": u.proc.pid}
        hog = hogs(a.load)
        payload = py.get("/api/local-sessions", timeout=120).json() or {}
        result["workloads"] = {}

        for wl in ("checks", "replay"):
            if wl not in work:
                continue
            run = Run(py, dm)
            if wl == "checks":
                paths, conc = check_paths(payload, world), 1
            else:
                reqs, conc = replay_shape()
                if a.conc:
                    conc = a.conc
                if world:
                    rows = [r for g in ("recent", "momentum") for r in payload.get(g, [])]
                    rows += [r for v in (payload.get("areas") or {}).values() for r in v]
                    reqs = map_to_sandbox(reqs, world, [r["id"] for r in rows])
                rnd = random.Random(11)
                paths = [rnd.choice(reqs) for _ in range(a.replay_n)] if reqs else []
            sampler = Sampler(pids)
            sampler.start()
            t0 = time.time()
            for _ in range(a.reps):
                run_pairs(run, paths, conc)
            wall = time.time() - t0
            procs = sampler.stop()
            title = f"workload {wl}: {len(paths)} requests × {a.reps} reps, concurrency {conc}, {wall:.1f}s wall" + \
                (f", {a.load} CPU hogs" if a.load else "")
            rows = run.table(title)
            for label, st in procs.items():
                print(f"  {label:<26} cpu {st['cpu_s']:6.2f}s   peak rss {st['peak_rss_mb']:7.1f} MB")
            result["workloads"][wl] = {"requests": len(paths), "concurrency": conc, "wall_s": round(wall, 1),
                                       "endpoints": rows, "processes": procs, "errors": dict(run.errors)}

        if "cold" in work:
            cw = world or World()
            try:
                cs = cold_start(cw, a.ref, a.bin, a.reps, servers[1].port if world else None,
                                live_url=a.live if a.live else None)
            finally:
                if cw is not world:
                    cw.close()
            print("\ncold start (spawn → first 200)")
            for side, v in cs.items():
                if v["health_s"]:
                    print(f"  {side:<8} health p50 {pct(v['health_s'], 50) * 1000:7.0f} ms   "
                          f"full list p50 {pct(v['list_s'], 50) * 1000:7.0f} ms   (runs: {v['list_s']})")
            result["cold"] = cs
    finally:
        for h in hog:
            h.kill()
        for s in servers:
            if hasattr(s, "stop"):
                s.stop()
            else:
                s.terminate()
                try:
                    s.wait(5)
                except subprocess.TimeoutExpired:
                    s.kill()
        if world:
            world.close()
    if a.json:
        Path(a.json).write_text(json.dumps(result, indent=1, default=str))


if __name__ == "__main__":
    main()
