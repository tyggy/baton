#!/usr/bin/env python3
"""Parity gate for the `read` group: Python reference server vs baton daemon.

    python3 bench/parity_read.py [--py 19877] [--rs 19882] [--n 40] [--codex 5] [--assets 200]

Needs both servers running (the daemon e.g. with
`baton serve --listen 127.0.0.1:19882 --upstream http://127.0.0.1:19877`).
For the most recent sessions (+ a few Codex ones) compares, as parsed JSON:
/api/session/{id}?limit=50, /api/messages/{id}?limit=50 and ?limit=200,
/api/children/{id}. Then /api/asset for files the messages reference
(bytes, content type, ETag, gzip, 304) plus edge cases (missing, secret,
outside the roots, >100 MB → 413 without reading, no path → 400), and
/api/tail first events (default, ?since=, ?from=0, Codex conversion).
Prints per-endpoint latency for both servers. Exit 1 on any mismatch.
"""
import argparse
import gzip
import http.client
import json
import os
import statistics
import subprocess
import time
import urllib.parse
from collections import defaultdict
from pathlib import Path

HOME = Path.home()


def bearer():
    for line in (HOME / ".claude-sessions" / "config").read_text().splitlines():
        line = line.strip().removeprefix("export ")
        if line.startswith("BATON_BEARER="):
            return line.split("=", 1)[1].strip().strip("'\"")
    return ""


TOKEN = bearer()
LAT = defaultdict(list)          # (label, endpoint) -> [ms]
FAIL = []


def get(port, path, headers=None, label=None, ep=None):
    h = {"Authorization": f"Bearer {TOKEN}"}
    h.update(headers or {})
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
    t0 = time.perf_counter()
    c.request("GET", path, headers=h)
    r = c.getresponse()
    body = r.read()
    ms = (time.perf_counter() - t0) * 1000
    if label and ep:
        LAT[(label, ep)].append(ms)
    hdrs = {k.lower(): v for k, v in r.getheaders()}
    c.close()
    if hdrs.get("content-encoding") == "gzip":
        body = gzip.decompress(body)
    return r.status, hdrs, body


def jparse(b):
    try:
        return json.loads(b)
    except Exception:
        return ("<non-json>", b[:200])


def diff_json(a, b, path="$"):
    """First differing path between two JSON values."""
    if type(a) != type(b):
        return f"{path}: type {type(a).__name__} vs {type(b).__name__}"
    if isinstance(a, dict):
        for k in list(a) + [k for k in b if k not in a]:
            if k not in a or k not in b:
                return f"{path}.{k}: missing on {'py' if k not in a else 'daemon'}"
            d = diff_json(a[k], b[k], f"{path}.{k}")
            if d:
                return d
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path}: len {len(a)} vs {len(b)}"
        for i, (x, y) in enumerate(zip(a, b)):
            d = diff_json(x, y, f"{path}[{i}]")
            if d:
                return d
        return None
    if a != b:
        sa, sb = str(a), str(b)
        i = next((k for k in range(min(len(sa), len(sb))) if sa[k] != sb[k]), min(len(sa), len(sb)))
        return f"{path}: {sa[max(0, i - 60):i + 60]!r} vs {sb[max(0, i - 60):i + 60]!r}"
    return None


def compare_json(a, name, path, ep, retries=2):
    """Python first, then daemon; on a mismatch retry (active sessions
    grow; Python's first preview writes its cache)."""
    for attempt in range(retries + 1):
        sp, _, bp = get(a.py, path, {"Accept-Encoding": "gzip"}, "py", ep)
        sr, _, br = get(a.rs, path, {"Accept-Encoding": "gzip"}, "daemon", ep)
        jp, jr = jparse(bp), jparse(br)
        if sp == sr and jp == jr:
            return True, jp
        if attempt < retries:
            time.sleep(0.3)
    d = diff_json(jp, jr) if sp == sr else f"status {sp} vs {sr}"
    FAIL.append(f"{name} {path}: {d}")
    return False, jp


def sse_events(port, path, seconds):
    """Events received within `seconds` (a reader thread; the socket is
    shut down when the window ends)."""
    import socket
    import threading
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=seconds + 5)
    c.request("GET", path, headers={"Authorization": f"Bearer {TOKEN}"})
    r = c.getresponse()
    if r.status != 200:
        body = r.read()
        c.close()
        return r.status, [body]
    sock = c.sock or getattr(getattr(r.fp, "raw", None), "_sock", None)
    buf = bytearray()

    def reader():
        try:
            while True:
                chunk = r.read1(65536)
                if not chunk:
                    return
                buf.extend(chunk)
        except Exception:
            return

    t = threading.Thread(target=reader, daemon=True)
    t.start()
    time.sleep(seconds)
    try:
        sock.shutdown(socket.SHUT_RDWR)
    except Exception:
        pass
    t.join(2)
    c.close()
    return 200, [e for e in bytes(buf).split(b"\n\n") if e]


def live_preview(a, ids, counts):
    """The preview's live-extraction path: Python's handler called
    in-process with an empty cache dir vs a second daemon whose cache dir
    is empty (the first comparison mostly hits the KB cache Python wrote)."""
    import dataclasses
    import sys
    import tempfile
    sys.path.insert(0, a.ref)
    sys.path.insert(0, a.ref + "/server")
    import api_server as A

    class H:
        headers = {}

        def send_json(self, data, status=200):
            self.out = (status, data)

    empty = tempfile.mkdtemp(prefix="baton-parity-cache-")
    env = dict(os.environ, CSM_SESSIONS_CACHE_DIR=empty)
    port = a.rs + 1
    proc = subprocess.Popen([a.bin, "serve", "--listen", f"127.0.0.1:{port}", "--upstream",
                             f"http://127.0.0.1:{a.py}"], env=env, stderr=subprocess.DEVNULL)
    time.sleep(0.8)
    ok = bad = 0
    try:
        for sid in ids:
            for limit in (50, 200):
                for attempt in range(3):
                    tmp = tempfile.mkdtemp(prefix="baton-parity-py-")
                    A.config = dataclasses.replace(A.config, sessions_cache_dir=Path(tmp))
                    h = H()
                    t0 = time.perf_counter()
                    A.APIHandler._send_session_markdown(h, sid, limit)
                    LAT[("py", "session (live)")].append((time.perf_counter() - t0) * 1000)
                    st, _, body = get(port, f"/api/session/{sid}?limit={limit}", {"Accept-Encoding": "gzip"},
                                      "daemon", "session (live)")
                    same = h.out[0] == st and h.out[1] == jparse(body)
                    if same or attempt == 2:
                        break
                    time.sleep(0.3)
                ok += same
                bad += not same
                if not same:
                    FAIL.append(f"live preview {sid} limit={limit}: "
                                + (diff_json(h.out[1], jparse(body)) or f"status {h.out[0]} vs {st}"))
    finally:
        proc.terminate()
    counts["session (live path)"] = [ok, bad]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--py", type=int, default=19877)
    ap.add_argument("--rs", type=int, default=19882)
    ap.add_argument("--n", type=int, default=40)
    ap.add_argument("--codex", type=int, default=5)
    ap.add_argument("--assets", type=int, default=200)
    ap.add_argument("--live", type=int, default=80,
                    help="sessions whose preview is also compared on the live-extraction path")
    ap.add_argument("--bin", default=str(Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton"))
    ap.add_argument("--ref", default=str(HOME / "GitHub/workbench-exporter-master/claude-session-manager"))
    a = ap.parse_args()

    _, _, body = get(a.py, "/api/local-sessions", {"Accept-Encoding": "gzip"})
    d = json.loads(body)
    rows, seen = [], set()
    for s in d.get("recent", []) + d.get("momentum", []) + [x for v in d.get("areas", {}).values() for x in v]:
        if s["id"] not in seen:
            seen.add(s["id"])
            rows.append(s)
    ids = [s["id"] for s in rows[:a.n]]
    codex = [s["id"] for s in rows if (s.get("origin") == "codex") and s["id"] not in ids][:a.codex]
    ids += codex
    origin = {s["id"]: s.get("origin") or "claude" for s in rows}
    print(f"{len(ids)} sessions ({sum(origin[i] == 'codex' for i in ids)} codex)")

    counts = defaultdict(lambda: [0, 0])
    attachments = []
    msgs200 = {}
    for sid in ids:
        for ep, path in [("session", f"/api/session/{sid}?limit=50"),
                         ("messages50", f"/api/messages/{sid}?limit=50"),
                         ("messages200", f"/api/messages/{sid}?limit=200"),
                         ("children", f"/api/children/{sid}")]:
            ok, j = compare_json(a, sid, path, ep)
            counts[ep][0 if ok else 1] += 1
            if ep == "messages200" and isinstance(j, dict):
                msgs200[sid] = j
                for m in j.get("messages", []):
                    for att in m.get("attachments", []):
                        attachments.append(att["path"])
    # edge ids
    for ep, path in [("session", "/api/session/does-not-exist"), ("messages50", "/api/messages/does-not-exist"),
                     ("children", "/api/children/does-not-exist"), ("session", "/api/session/..%2Fx"),
                     ("messages50", f"/api/messages/{ids[0]}?limit=abc"), ("session", f"/api/session/{ids[0]}?limit=-1")]:
        ok, _ = compare_json(a, "edge", path, ep)
        counts[ep + " (edge)"][0 if ok else 1] += 1

    # ETag 304 on /api/messages (Claude)
    claude_ids = [i for i in ids if origin.get(i) != "codex"]
    ok304 = 0
    for sid in claude_ids[:10]:
        res = []
        for port in (a.py, a.rs):
            st, h, _ = get(port, f"/api/messages/{sid}?limit=50")
            et = h.get("etag")
            st2, h2, b2 = get(port, f"/api/messages/{sid}?limit=50", {"If-None-Match": et or ""})
            res.append((et, st2, b2))
        (ep_, sp, bp), (er, sr, br) = res
        if ep_ == er and sp == sr and bp == br:
            ok304 += 1
        else:
            FAIL.append(f"messages etag {sid}: {ep_} {sp} vs {er} {sr}")
    counts["messages etag/304"] = [ok304, min(10, len(claude_ids)) - ok304]

    # ---- assets ----
    uniq = list(dict.fromkeys(attachments))[:a.assets]
    asset_ok = 0
    for p in uniq:
        q = "/api/asset?path=" + urllib.parse.quote(p)
        rp = get(a.py, q, {"Accept-Encoding": "gzip"}, "py", "asset")
        rr = get(a.rs, q, {"Accept-Encoding": "gzip"}, "daemon", "asset")
        same = (rp[0] == rr[0] and rp[2] == rr[2]
                and all(rp[1].get(k) == rr[1].get(k) for k in ("content-type", "etag", "cache-control",
                                                               "content-encoding", "content-disposition")))
        if same and rp[0] == 200:
            et = rp[1]["etag"]
            n1 = get(a.py, q, {"If-None-Match": et})
            n2 = get(a.rs, q, {"If-None-Match": et})
            same = n1[0] == n2[0] == 304 and n1[1].get("etag") == n2[1].get("etag")
        if same:
            asset_ok += 1
        else:
            FAIL.append(f"asset {p}: {rp[0]} {rp[1].get('content-type')} vs {rr[0]} {rr[1].get('content-type')}")
    counts["asset (referenced)"] = [asset_ok, len(uniq) - asset_ok]

    def find(cmd):
        try:
            out = subprocess.run(["bash", "-c", cmd], capture_output=True, text=True, timeout=60).stdout.split("\n")
            return next((x for x in out if x), None)
        except Exception:
            return None

    big = find("find ~/Documents -size +100M -name '*.jsonl' 2>/dev/null | head -1")
    # fixtures under /tmp (an allowed root): secrets by name, a latin-1 name
    fx = Path(f"/tmp/baton-parity-{os.getpid()}")
    (fx / "repo/.git").mkdir(parents=True, exist_ok=True)
    for name, data in [("key.p8", "k"), (".env", "A=1"), ("id_rsa", "k"), ("repo/.git/config", "[x]"),
                       ("café.md", "# café\n" * 400), ("notes.txt", "hello\n" * 10), ("pic.png", "\x89PNG")]:
        (fx / name).write_text(data)
    p8 = find("find ~/Documents ~/GitHub -name '*.p8' -not -path '*/node_modules/*' 2>/dev/null | head -1") or str(fx / "key.p8")
    envf = next((str(x) for x in [HOME / "GitHub/workbench-exporter-master/.env"] if x.exists()), str(fx / ".env"))
    gitcfg = next((str(x) for x in [HOME / "GitHub/workbench-exporter-master/.git/config"] if x.exists()), None)
    text_big = next((x for x in uniq if x.endswith((".md", ".py", ".json", ".swift")) and os.path.getsize(x) > 1400), None)
    edge = [("missing", "~/Documents/definitely-missing-baton-parity.md", 404),
            ("outside roots", "/etc/hosts", 404),
            ("no path", None, 400),
            ("secret .env", envf, 404),
            ("secret .p8", p8, 404),
            ("secret .git/config", gitcfg, 404),
            ("over 100 MB", big, 413),
            ("relative", "Documents/x.md", 404),
            ("tilde dir", "~/.claude/plans", 404),
            ("tmp secret .env", str(fx / ".env"), 404),
            ("tmp id_rsa", str(fx / "id_rsa"), 404),
            ("tmp .git/config", str(fx / "repo/.git/config"), 404),
            ("tmp latin-1 name", str(fx / "café.md"), 200),
            ("tmp small text", str(fx / "notes.txt"), 200),
            ("tmp png", str(fx / "pic.png"), 200),
            ("dotdot", str(fx / "repo/../notes.txt"), 200)]
    edge_ok = 0
    for name, p, want in edge:
        if name != "no path" and not p:
            print(f"  (edge '{name}': no such file on this machine, skipped)")
            continue
        q = "/api/asset" if p is None else "/api/asset?path=" + urllib.parse.quote(p)
        rp = get(a.py, q, label="py", ep="asset edge")
        t0 = time.perf_counter()
        rr = get(a.rs, q, label="daemon", ep="asset edge")
        ms = (time.perf_counter() - t0) * 1000
        good = rp[0] == rr[0] == want and (rp[2] == rr[2] if want == 200 else jparse(rp[2]) == jparse(rr[2]))
        if want == 200:
            good = good and all(rp[1].get(k) == rr[1].get(k) for k in ("content-type", "etag", "content-disposition"))
        if name == "over 100 MB":
            good = good and ms < 200
            print(f"  413 for {os.path.getsize(p) >> 20} MB file in {ms:.1f} ms (daemon)")
        edge_ok += good
        if not good:
            FAIL.append(f"asset edge {name}: py {rp[0]} {rp[2][:120]!r} vs daemon {rr[0]} {rr[2][:120]!r}")
    counts["asset (edge cases)"] = [edge_ok, sum(1 for n, p, _ in edge if p or n == "no path") - edge_ok]
    if text_big:
        q = "/api/asset?path=" + urllib.parse.quote(text_big)
        gp = get(a.py, q, {"Accept-Encoding": "gzip"})
        gr = get(a.rs, q, {"Accept-Encoding": "gzip"})
        gz_ok = gp[1].get("content-encoding") == gr[1].get("content-encoding") == "gzip" and gp[2] == gr[2]
        counts["asset gzip"] = [int(gz_ok), int(not gz_ok)]
        if not gz_ok:
            FAIL.append(f"asset gzip {text_big}")

    # ---- tail ----
    tail_ok, tail_n = 0, 0

    def tail_case(label, path, seconds=2.5, cmp="data"):
        nonlocal tail_ok, tail_n
        tail_n += 1
        (sp, ep), (sr, er) = sse_events(a.py, path, seconds), sse_events(a.rs, path, seconds)
        if cmp == "data":
            dp = [e for e in ep if e.startswith(b"data: ")]
            dr = [e for e in er if e.startswith(b"data: ")]
            k = min(len(dp), len(dr))
            good = sp == sr and len(dp) == len(dr) and dp[:k] == dr[:k] and (k > 0 or not dp)
        else:
            good = sp == sr and ep[:1] == er[:1]
        tail_ok += good
        print(f"  tail {label}: {sum(e.startswith(b'data: ') for e in ep)} data / {len(ep)} events (py), "
              f"{sum(e.startswith(b'data: ') for e in er)} / {len(er)} (daemon) -> {'ok' if good else 'DIFF'}")
        if not good:
            FAIL.append(f"tail {label}: py {sp} {len(ep)} events {ep[:2]!r} vs daemon {sr} {len(er)} {er[:2]!r}")
        return ep, er

    idle = [s for s in rows if s["id"] in ids and origin.get(s["id"]) != "codex"]
    idle = sorted(idle, key=lambda s: s.get("last_ts") or "")[:3]
    for s in idle:
        sid = s["id"]
        tail_case(f"default {sid}", f"/api/tail/{sid}", 1.5, cmp="first")
        m = msgs200.get(sid, {}).get("messages", [])
        if len(m) > 4:
            since = m[-4]["ts"]
            tail_case(f"since {sid}", f"/api/tail/{sid}?since={urllib.parse.quote(since)}")
    small = [s for s in rows if origin.get(s["id"]) != "codex" and s.get("msg_count", 99) < 15][:1]
    for s in small:
        tail_case(f"from=0 {s['id']}", f"/api/tail/{s['id']}?from=0", 3)
    for sid in codex[:2]:
        m = msgs200.get(sid, {}).get("messages", [])
        if len(m) > 4:
            tail_case(f"codex since {sid}", f"/api/tail/{sid}?since={urllib.parse.quote(m[-4]['ts'])}")
        tail_case(f"codex default {sid}", f"/api/tail/{sid}", 1.5, cmp="first")
    tail_case("unknown", "/api/tail/does-not-exist", 1, cmp="first")
    counts["tail"] = [tail_ok, tail_n - tail_ok]

    if a.live:
        live_preview(a, [s["id"] for s in rows if origin.get(s["id"]) != "codex"][:a.live], counts)

    print(f"\n{'endpoint':24} {'equal':>7} {'differ':>7}")
    for ep, (ok, bad) in counts.items():
        print(f"{ep:24} {ok:>7} {bad:>7}")
    print(f"\n{'latency (ms)':24} {'py p50':>8} {'py p95':>8} {'rs p50':>8} {'rs p95':>8}")
    for ep in dict.fromkeys(e for _, e in LAT):
        def q(label, f):
            xs = sorted(LAT[(label, ep)])
            return xs[min(len(xs) - 1, int(round(f * (len(xs) - 1))))] if xs else 0
        print(f"{ep:24} {q('py', .5):>8.1f} {q('py', .95):>8.1f} {q('daemon', .5):>8.1f} {q('daemon', .95):>8.1f}")
    if FAIL:
        print(f"\n{len(FAIL)} MISMATCHES:")
        for f in FAIL[:40]:
            print("  " + f)
        raise SystemExit(1)
    print("\nPARITY OK")


if __name__ == "__main__":
    main()
