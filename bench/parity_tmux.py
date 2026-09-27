#!/usr/bin/env python3
"""Parity + latency gate for the tmux group (PLAN.md slice 4).

Compares the Rust daemon (`baton serve`, default 127.0.0.1:19883) with the
Python reference server (127.0.0.1:19877) on throwaway tmux sessions named
`csm-bench-tmux-*` that this script creates and kills. It never sends keys
to, scrolls, resizes or kills any other session; `/api/sessions` lists the
user's real sessions read-only.

  python3 bench/parity_tmux.py [--rs-port 19883] [--py-port 19877]
                               [--bin path/to/baton] [--n-scroll 100]

Starts the daemon if nothing listens on --rs-port (and stops it at the end).
Exit status 0 = every parity check passed.
"""
import argparse
import http.client
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
APP = HERE / "tmux_bench_app.py"
PREFIX = "csm-bench-tmux-"


def token():
    if os.environ.get("BATON_BEARER"):
        return os.environ["BATON_BEARER"].strip()
    cfg = Path.home() / ".claude-sessions" / "config"
    tok = ""
    for line in cfg.read_text().splitlines() if cfg.exists() else []:
        line = line.strip()
        if line.startswith("export "):
            line = line[7:].strip()
        if line.startswith("BATON_BEARER="):
            tok = line.split("=", 1)[1].strip().strip("\"'")
    return tok


TOKEN = token()
H = {"Authorization": "Bearer " + TOKEN}


# ── tmux (bench sessions only) ────────────────────────────────────────────

def tmux(*args):
    # Every -t target this script passes is one of its own sessions (own()).
    return subprocess.run(["tmux", *args], capture_output=True, text=True)


def own(name):
    assert name.startswith(PREFIX), name
    return name


def new_session(name, cmd, cols=100, rows=30):
    own(name)
    tmux("kill-session", "-t", "=" + name)
    r = tmux("new-session", "-d", "-s", name, "-x", str(cols), "-y", str(rows), cmd)
    assert r.returncode == 0, r.stderr


def set_size(name, cols, rows):
    own(name)
    tmux("resize-window", "-t", "=" + name + ":", "-x", str(cols), "-y", str(rows))
    tmux("set-option", "-t", "=" + name, "window-size", "latest")


def win_size(name):
    return tmux("display-message", "-p", "-t", "=" + own(name) + ":",
                "#{window_width}x#{window_height}").stdout.strip()


def capture(name):
    return tmux("capture-pane", "-p", "-e", "-t", "=" + own(name) + ":").stdout


def wait_quiet(name, quiet=0.25, timeout=3.0):
    """Wait until the pane content stops changing."""
    t0 = time.time()
    last, since = capture(name), time.time()
    while time.time() - t0 < timeout:
        time.sleep(0.03)
        cur = capture(name)
        if cur != last:
            last, since = cur, time.time()
        elif time.time() - since >= quiet:
            return


# ── HTTP ──────────────────────────────────────────────────────────────────

def request(port, method, path, body=None, headers=None, conn=None):
    c = conn or http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    hdrs = dict(H)
    hdrs.update(headers or {})
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        hdrs["Content-Type"] = "application/json"
    c.request(method, path, body=data, headers=hdrs)
    r = c.getresponse()
    payload = r.read()
    if conn is None:
        c.close()
    return r.status, r.getheader("Content-Type"), payload


class Stream:
    """SSE client: events as raw bytes (keepalive comments kept apart)."""

    def __init__(self, port, name, cols=None, rows=None):
        self.t_open = time.perf_counter()
        self.conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
        hdrs = dict(H)
        if cols:
            hdrs["X-Pane-Cols"], hdrs["X-Pane-Rows"] = str(cols), str(rows)
        self.conn.request("GET", "/api/pane-stream/" + name, headers=hdrs)
        self.resp = self.conn.getresponse()
        self.status = self.resp.status
        self.ctype = self.resp.getheader("Content-Type")
        self.buf = b""

    def next_event(self, include_keepalive=False):
        """(t, raw_event) — raw_event ends with the blank line."""
        while True:
            i = self.buf.find(b"\n\n")
            if i >= 0:
                ev, self.buf = self.buf[:i + 2], self.buf[i + 2:]
                if ev == b": keepalive\n\n" and not include_keepalive:
                    continue
                return time.perf_counter(), ev
            chunk = self.resp.read1(65536)
            if not chunk:
                return time.perf_counter(), None
            self.buf += chunk

    def close(self):
        try:
            self.conn.sock.shutdown(socket.SHUT_RDWR)
        except Exception:
            pass
        self.conn.close()


def is_frame(ev):
    return ev is not None and ev.startswith(b"data: ")


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def fmt_ms(xs):
    return "p50 %.1f ms  p95 %.1f ms  (n=%d)" % (pct(xs, 50) * 1e3, pct(xs, 95) * 1e3, len(xs))


# ── checks ────────────────────────────────────────────────────────────────

class Gate:
    def __init__(self):
        self.results = []

    def check(self, name, ok, detail=""):
        self.results.append((name, bool(ok), detail))
        print("  %s  %s%s" % ("PASS" if ok else "FAIL", name, ("  — " + detail) if detail else ""))
        return ok


def first_events(port, name, cols=None, rows=None):
    s = Stream(port, name, cols, rows)
    t_dims, dims = s.next_event()
    t_frame, frame = s.next_event()
    s.close()
    return {"status": s.status, "ctype": s.ctype, "dims": dims, "frame": frame,
            "first_frame_s": t_frame - s.t_open}


def winch_count(log):
    try:
        return len(Path(log).read_text().splitlines())
    except FileNotFoundError:
        return 0


def run(args):
    g = Gate()
    tmp = Path(tempfile.mkdtemp(prefix="csm-bench-tmux-"))
    py, rs = args.py_port, args.rs_port
    sess = {
        "seq": PREFIX + "seq",
        "winch": PREFIX + "winch",
        "in_py": PREFIX + "input-py",
        "in_rs": PREFIX + "input-rs",
    }
    logs = {k: str(tmp / (k + ".log")) for k in sess}
    new_session(sess["seq"], "seq 1 3000; exec sleep 100000")
    new_session(sess["winch"], "%s %s winch %s" % (sys.executable, APP, logs["winch"]))
    new_session(sess["in_py"], "%s %s input %s" % (sys.executable, APP, logs["in_py"]))
    new_session(sess["in_rs"], "%s %s input %s" % (sys.executable, APP, logs["in_rs"]))
    for s in sess.values():
        wait_quiet(s)
    try:
        print("pane-stream: same size (100x30)")
        for name in (sess["seq"], sess["winch"]):
            before_w = winch_count(logs["winch"])
            a = first_events(py, name, 100, 30)
            b = first_events(rs, name, 100, 30)
            g.check("same-size %s: dims event identical" % name, a["dims"] == b["dims"] == b"event: dims\ndata: 100x30\n\n", repr(b["dims"]))
            g.check("same-size %s: first frame byte-identical" % name, a["frame"] == b["frame"] and is_frame(a["frame"]),
                    "%d vs %d bytes" % (len(a["frame"] or b""), len(b["frame"] or b"")))
            g.check("same-size %s: status/content-type" % name,
                    (a["status"], a["ctype"]) == (b["status"], b["ctype"]) == (200, "text/event-stream"))
            if name == sess["winch"]:
                g.check("same-size: no resize, no redraw", winch_count(logs["winch"]) == before_w and win_size(name) == "100x30")
        print("pane-stream: no size headers")
        a, b = first_events(py, sess["seq"]), first_events(rs, sess["seq"])
        g.check("no-headers: dims + first frame identical", (a["dims"], a["frame"]) == (b["dims"], b["frame"]))

        print("pane-stream: resize 100x30 → 80x24 (tmux-only path, no client attached)")
        res = {}
        for label, port in (("py", py), ("rs", rs)):
            set_size(sess["winch"], 100, 30)
            wait_quiet(sess["winch"])
            n0 = winch_count(logs["winch"])
            ev = first_events(port, sess["winch"], 80, 24)
            time.sleep(0.3)
            ev["redraws"] = winch_count(logs["winch"]) - n0
            res[label] = ev
        a, b = res["py"], res["rs"]
        g.check("resize: dims event identical and final", a["dims"] == b["dims"] == b"event: dims\ndata: 80x24\n\n", repr(b["dims"]))
        g.check("resize: first frame byte-identical", a["frame"] == b["frame"])
        g.check("resize: first frame shows final size", b"bottom 80x24" in (b["frame"] or b"") and b"WINCH 80x24" in (b["frame"] or b""))
        g.check("resize: exactly one redraw", a["redraws"] == b["redraws"] == 1, "py %d, rs %d" % (a["redraws"], b["redraws"]))
        print("pane-stream: clamps (X-Pane-Cols 5 → 20, X-Pane-Rows 999 → 200)")
        res = {}
        for label, port in (("py", py), ("rs", rs)):
            set_size(sess["winch"], 100, 30)
            wait_quiet(sess["winch"])
            ev = first_events(port, sess["winch"], 5, 999)
            res[label] = (ev["dims"], ev["frame"], win_size(sess["winch"]))
        g.check("clamped resize identical", res["py"] == res["rs"], repr(res["rs"][2]))
        set_size(sess["winch"], 100, 30)
        wait_quiet(sess["winch"])

        print("pane-stream: invalid / gone")
        # (Not "a;b": Python's urlparse drops ";b" as path params and would
        # target tmux session "a" — see the report; the daemon answers 400.)
        for path in ("/api/pane-stream/bad%20name", "/api/pane-stream/", "/api/pane-stream/a%3Bb"):
            g.check("GET %s identical" % path, request(py, "GET", path) == request(rs, "GET", path))
        gone = PREFIX + "gone"
        a = first_events(py, gone)
        b = first_events(rs, gone)
        g.check("missing session: 'pane gone' error event identical",
                a["dims"] == b["dims"] == b"event: error\ndata: pane gone\n\n" and a["frame"] == b["frame"] is None)

        print("/api/pane")
        for q in ("", "?history=10", "?history=", "?history=x", "?history=0"):
            a = request(py, "GET", "/api/pane/" + sess["seq"] + q)
            b = request(rs, "GET", "/api/pane/" + sess["seq"] + q)
            g.check("/api/pane%s identical" % q, a == b, "%d bytes" % len(b[2]))
        for name in ("bad%20name", gone):
            g.check("/api/pane/%s identical" % name,
                    request(py, "GET", "/api/pane/" + name) == request(rs, "GET", "/api/pane/" + name),
                    request(rs, "GET", "/api/pane/" + name)[2][:80].decode())

        print("scroll / send / send-key effects (twin panes, one per server)")
        twins = {"py": (py, sess["in_py"], logs["in_py"]), "rs": (rs, sess["in_rs"], logs["in_rs"])}
        ops = [
            ("POST", "/api/scroll", lambda s: {"session": s, "direction": "up", "ticks": 3}),
            ("POST", "/api/scroll", lambda s: {"session": s, "direction": "UP ", "ticks": "2"}),
            ("POST", "/api/scroll", lambda s: {"session": " " + s, "direction": "down", "ticks": 1.9}),
            ("POST", "/api/scroll", lambda s: {"session": s, "direction": "up", "ticks": 999}),
            ("POST", "/api/scroll", lambda s: {"session": s, "direction": "down", "ticks": None}),
            ("POST", "/api/send", lambda s: {"session": s, "message": "hello wörld\nline2 ●"}),
            ("POST", "/api/send-key", lambda s: {"session": s, "keys": ["Up", "C-c", "Escape", "M-x", "BSpace"]}),
            ("POST", "/api/send-key", lambda s: {"session": s, "key": "Enter"}),
            ("POST", "/api/send-key", lambda s: {"session": s, "keys": "", "key": "Tab"}),
        ]
        initial = capture(sess["in_rs"])
        g.check("input panes start identical and non-empty", initial == capture(sess["in_py"]) and "top=" in initial)
        for m, path, mk in ops:
            out = {}
            for label, (port, name, log) in twins.items():
                st, ct, body = request(port, m, path, mk(name))
                time.sleep(0.12)
                wait_quiet(name, quiet=0.15)
                out[label] = (st, ct, body, capture(name), Path(log).read_text())
            a, b = out["py"], out["rs"]
            desc = "%s %s" % (path, json.dumps(mk("S"), ensure_ascii=False))
            g.check("%s: response identical" % desc, a[:3] == b[:3], b[2].decode()[:80])
            g.check("%s: pane identical" % desc, a[3] == b[3] and "top=" in b[3])
            if path == "/api/scroll" and initial is not None:
                g.check("scroll moved the view", b[3] != initial)
                initial = None
            g.check("%s: input bytes identical" % desc, a[4] == b[4])

        print("POST validation errors")
        bad = [
            ("/api/scroll", {"session": sess["in_rs"], "direction": "left"}),
            ("/api/scroll", {"direction": "up"}),
            ("/api/scroll", {"session": "bad name", "direction": "up"}),
            ("/api/scroll", {"session": PREFIX + "gone", "direction": "up"}),
            ("/api/send-key", {"session": sess["in_rs"]}),
            ("/api/send-key", {"keys": ["Up"]}),
            ("/api/send-key", {"session": "", "keys": 5}),
            ("/api/send-key", {"session": PREFIX + "gone", "keys": ["Up"]}),
        ]
        for path, body in bad:
            g.check("%s %s identical" % (path, json.dumps(body)), request(py, "POST", path, body) == request(rs, "POST", path, body),
                    request(rs, "POST", path, body)[2].decode()[:80])
        for path, body in [("/api/scroll", {"session": 5, "direction": "up"}), ("/api/send-key", {"session": sess["in_rs"], "keys": 5})]:
            a, b = request(py, "POST", path, body), request(rs, "POST", path, body)
            g.check("%s %s → 500 internal error both" % (path, json.dumps(body)),
                    a[0] == b[0] == 500 and json.loads(a[2])["error"] == json.loads(b[2])["error"] == "internal error")

        print("/api/sessions (the user's live tmux sessions, read-only)")
        same, diffs = False, None
        for _ in range(8):
            got = {}

            def fetch(label, port):
                got[label] = request(port, "GET", "/api/sessions")
            ts = [threading.Thread(target=fetch, args=a) for a in (("py", py), ("rs", rs))]
            [t.start() for t in ts]
            [t.join() for t in ts]
            if got["py"] == got["rs"]:
                same = True
                break
            pa, pb = json.loads(got["py"][2]), json.loads(got["rs"][2])
            diffs = [x["name"] for x, y in zip(pa, pb) if x != y] + (["<len>"] if len(pa) != len(pb) else [])
            time.sleep(2.1)
        g.check("/api/sessions byte-identical (status, content-type, body)", same,
                "" if same else "live panes still differing: %s" % diffs)

        # ── latencies ─────────────────────────────────────────────────────
        print("\nlatency (daemon vs Python)")
        lat = {}
        for label, port in (("py", py), ("rs", rs)):
            ff = []
            for _ in range(args.n_first):
                ff.append(first_events(port, sess["seq"], 100, 30)["first_frame_s"])
            lat[label, "first"] = ff
        for label, (port, name, _) in twins.items():
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
            xs = []
            for i in range(args.n_scroll):
                d = "up" if (i // 5) % 2 == 0 else "down"
                t0 = time.perf_counter()
                st, _, _ = request(port, "POST", "/api/scroll", {"session": name, "direction": d, "ticks": 1}, conn=conn)
                xs.append(time.perf_counter() - t0)
                assert st == 200
                time.sleep(0.02)
            conn.close()
            lat[label, "scroll"] = xs
            time.sleep(2.2)   # let the adaptive fast-poll window lapse
            s = Stream(port, name, 100, 30)
            s.next_event()
            s.next_event()
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
            sf = []
            for i in range(args.n_sf):
                time.sleep(0.4 if i % 2 else 2.2)  # alternate idle (350 ms) / active (50 ms) poll
                d = "up" if i % 2 == 0 else "down"
                t0 = time.perf_counter()
                request(port, "POST", "/api/scroll", {"session": name, "direction": d, "ticks": 1}, conn=conn)
                while True:
                    t, ev = s.next_event()
                    if ev is None or is_frame(ev):
                        break
                sf.append(t - t0)
            conn.close()
            s.close()
            lat[label, "scroll_frame"] = sf
        for key, title in (("first", "first frame (same size)"), ("scroll", "POST /api/scroll"),
                           ("scroll_frame", "scroll → next frame")):
            print("  %-26s python %s\n  %-26s daemon %s" % (title, fmt_ms(lat["py", key]), "", fmt_ms(lat["rs", key])))
    finally:
        for s in list(sess.values()):
            tmux("kill-session", "-t", "=" + own(s))
        shutil.rmtree(tmp, ignore_errors=True)
    fails = [r for r in g.results if not r[1]]
    print("\n%d/%d checks passed" % (len(g.results) - len(fails), len(g.results)))
    return 0 if not fails else 1


def listening(port):
    with socket.socket() as s:
        s.settimeout(0.3)
        return s.connect_ex(("127.0.0.1", port)) == 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--py-port", type=int, default=19877)
    ap.add_argument("--rs-port", type=int, default=19883)
    ap.add_argument("--bin", default=os.environ.get("BATON_BIN", str(
        Path(os.environ.get("CARGO_TARGET_DIR", HERE.parent / "target")) / "release" / "baton")))
    ap.add_argument("--n-first", type=int, default=20)
    ap.add_argument("--n-scroll", type=int, default=100)
    ap.add_argument("--n-sf", type=int, default=30)
    args = ap.parse_args()
    proc = None
    if not listening(args.rs_port):
        proc = subprocess.Popen([args.bin, "serve", "--listen", "127.0.0.1:%d" % args.rs_port,
                                 "--upstream", "http://127.0.0.1:%d" % args.py_port],
                                stderr=subprocess.DEVNULL)
        for _ in range(50):
            if listening(args.rs_port):
                break
            time.sleep(0.1)
    try:
        sys.exit(run(args))
    finally:
        if proc:
            proc.terminate()


if __name__ == "__main__":
    main()
