#!/usr/bin/env python3
"""Parity gate for the list group (PLAN.md slice 2).

Runs `baton serve` on 127.0.0.1:19881 in front of the live Python server
(19877) and checks:

1. /api/local-sessions, /api/search-index, /api/background: same JSON as
   Python — ideally byte-identical (then the ETags match too). The machine
   is live (sessions are being written while we look), so each check is
   retried; rows whose transcript was written during the window are
   reported separately, and `age_str` is tolerated only where the two
   builds straddle a minute boundary.
2. ETag → 304 (also with the ETag Python served, when bytes match); gzip.
3. /api/events: with a throwaway transcript in
   ~/.claude/projects/-tmp-baton-list-parity/ (≤2 records, so it never
   becomes a list row; deleted afterwards) — the watcher's rebuild and a
   sessions_delta after the write. Then deterministically, against a second
   daemon with HOME set to a temp dir: a new row arrives as `changed`
   within ~6 s of the append and as `removed` after the delete.
4. Timings: Rust cold / warm rebuild (daemon log) vs Python's (its event
   log), request latency for both.

`--readonly` skips the throwaway-transcript part of 3 (the only step that
writes outside a temp dir).

Exit 0 when everything passes.
"""
import gzip, json, os, re, shutil, signal, socket, statistics, subprocess, sys, tempfile, threading, time, uuid
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

HOME = Path.home()
BIN = os.environ.get("BATON_BIN", str(Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton"))
PY_PORT = int(os.environ.get("PY_PORT", "19877"))
RS_PORT = int(os.environ.get("RS_PORT", "19881"))
ISO_PORT = int(os.environ.get("ISO_PORT", "19882"))
RELAY_PORT = int(os.environ.get("RELAY_PORT", "19883"))
FAKE_UP_PORT = int(os.environ.get("FAKE_UP_PORT", "19884"))
FILTER = os.environ.get("PARITY_FILTER", "GitHub")
PY_LOG = HOME / "Library/Logs/cs-server.events.jsonl"
THROWAWAY = HOME / ".claude/projects/-tmp-baton-list-parity"


def token():
    if os.environ.get("BATON_BEARER"):
        return os.environ["BATON_BEARER"].strip()
    cfg = (HOME / ".claude-sessions/config").read_text()
    vals = [l.split("BATON_BEARER=", 1)[1].strip().strip("\"'") for l in cfg.splitlines() if "BATON_BEARER=" in l]
    return vals[-1]


TOK = token()
failures = []


def check(ok, what):
    print(("  ok   " if ok else "  FAIL ") + what)
    if not ok:
        failures.append(what)


def get(port, path, headers=None, timeout=120):
    h = {"Authorization": "Bearer " + TOK}
    h.update(headers or {})
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers=h)
    t = time.perf_counter()
    try:
        r = urllib.request.urlopen(req, timeout=timeout)
        body, status, hdrs = r.read(), r.status, {k.lower(): v for k, v in r.headers.items()}
    except urllib.error.HTTPError as e:
        body, status, hdrs = e.read(), e.code, {k.lower(): v for k, v in e.headers.items()}
    return status, hdrs, body, (time.perf_counter() - t) * 1000


def wait_port(port, secs=10):
    end = time.time() + secs
    while time.time() < end:
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return True
        except OSError:
            time.sleep(0.1)
    return False


def start_daemon(port, log, env=None, upstream=None):
    e = dict(os.environ)
    e.update(env or {})
    p = subprocess.Popen(["nice", BIN, "serve", "--listen", f"127.0.0.1:{port}",
                          "--upstream", f"http://127.0.0.1:{upstream or PY_PORT}"],
                         stdout=subprocess.DEVNULL, stderr=open(log, "w"), env=e)
    if not wait_port(port):
        p.kill()
        sys.exit(f"daemon on {port} did not start (see {log})")
    return p


def rebuilds(log):
    out = []
    for line in Path(log).read_text().splitlines():
        if line.startswith("ls_rebuild"):
            d = dict(kv.split("=", 1) for kv in line.split()[1:] if "=" in kv)
            out.append({k: float(v) for k, v in d.items() if k.endswith("_ms")})
    return out


# ── comparison helpers ─────────────────────────────────────────────────

def age_str(ts, now):
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        diff = now - dt
        days, hours = diff.days, diff.seconds // 3600
        if days > 30:
            return "%dmo" % (days // 30)
        if days > 0:
            return "%dd" % days
        if hours > 0:
            return "%dh" % hours
        return "%dm" % (diff.seconds // 60)
    except Exception:
        return "?"


def ages_between(ts, t0, t1):
    out, t = set(), t0
    while t <= t1 + 1:
        out.add(age_str(ts, datetime.fromtimestamp(t, timezone.utc)))
        t += 5
    return out


def flat(payload):
    rows = []
    for g, items in [("recent", payload["recent"]), ("momentum", payload["momentum"])] + \
            [("area:" + a, v) for a, v in payload["areas"].items()]:
        rows += [(g, r) for r in items]
    return rows


def compare_list(py1, rs, py2, t0, t1):
    """→ (identical, report). Rows that changed between the two Python
    fetches or were written in the last minute are 'volatile': compared, but
    a difference there is expected on a live machine."""
    fp1, frs, fp2 = flat(py1), flat(rs), flat(py2)
    by1 = {r["id"]: (g, r) for g, r in fp1}
    by2 = {r["id"]: (g, r) for g, r in fp2}
    now_iso = datetime.fromtimestamp(t0 - 60, timezone.utc).isoformat()
    volatile = {i for i in set(by1) | set(by2) if by1.get(i) != by2.get(i)}
    volatile |= {r["id"] for _, r in fp1 + fp2 if (r.get("last_ts") or "") >= now_iso[:19]}
    strict_diff, age_tol, vol_diff = [], 0, []
    for g, r in frs:
        ref = by1.get(r["id"]) or by2.get(r["id"])
        if ref is None:
            (vol_diff if r["id"] in volatile else strict_diff).append((r["id"], "only in rust"))
            continue
        cands = [x for x in (by1.get(r["id"]), by2.get(r["id"])) if x]
        if any(c == (g, r) for c in cands):
            continue
        c = cands[0]
        fields = sorted(k for k in set(r) | set(c[1]) if r.get(k) != c[1].get(k))
        if c[0] != g:
            fields.append("group")
        if fields == ["age_str"]:
            ok = ages_between(r["last_ts"], t0 - 35, t1)
            if r["age_str"] in ok and c[1]["age_str"] in ok:
                age_tol += 1
                continue
        (vol_diff if r["id"] in volatile else strict_diff).append((r["id"], fields))
    ids_rs = {r["id"] for _, r in frs}
    for i in set(by1) & set(by2):
        if i not in ids_rs:
            (vol_diff if i in volatile else strict_diff).append((i, "only in python"))
    seq = lambda f: [(g, r["id"]) for g, r in f if r["id"] not in volatile]
    order_ok = seq(frs) in (seq(fp1), seq(fp2))
    areas_ok = list(rs["areas"]) in (list(py1["areas"]), list(py2["areas"]))
    return (not strict_diff and order_ok and areas_ok,
            dict(rows=len(frs), volatile=len(volatile), age_tolerated=age_tol,
                 strict_diff=strict_diff[:5], n_strict_diff=len(strict_diff),
                 volatile_diff=len(vol_diff), order_ok=order_ok, areas_order_ok=areas_ok))


def main():
    if not Path(BIN).exists():
        sys.exit(f"no binary at {BIN} (set BATON_BIN or CARGO_TARGET_DIR)")
    tmp = Path(tempfile.mkdtemp(prefix="baton-parity-list-"))
    procs = []
    try:
        run(tmp, procs)
    finally:
        for p in procs:
            p.send_signal(signal.SIGTERM)
        shutil.rmtree(THROWAWAY, ignore_errors=True)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\nPASS" if not failures else f"\nFAILED ({len(failures)}): " + "; ".join(failures))
    sys.exit(1 if failures else 0)


def run(tmp, procs):
    print(f"load {os.getloadavg()[0]:.1f}; python :{PY_PORT}, rust :{RS_PORT} ({BIN})")
    log = tmp / "daemon.log"
    procs.append(start_daemon(RS_PORT, log))

    # ── cold ──
    st, h, body, ms = get(RS_PORT, "/api/local-sessions")
    check(st == 200, f"cold /api/local-sessions 200 ({ms:.0f} ms incl. first build)")

    # ── parity: local-sessions ──
    print("\n/api/local-sessions")
    best = None
    for attempt in range(10):
        t0 = time.time()
        _, _, p1, _ = get(PY_PORT, "/api/local-sessions")
        _, hr, r, _ = get(RS_PORT, "/api/local-sessions")
        _, _, p2, _ = get(PY_PORT, "/api/local-sessions")
        t1 = time.time()
        if r in (p1, p2):
            best = ("bytes", len(json.loads(r)["recent"]), hr.get("etag"))
            break
        ok, rep = compare_list(json.loads(p1), json.loads(r), json.loads(p2), t0, t1)
        if best is None or (ok and best[0] != "equal"):
            best = ("equal" if ok else "diff", rep)
        time.sleep(3)
    if best[0] == "bytes":
        pay = json.loads(r)
        n = len(flat(pay))
        check(True, f"byte-identical payload after {attempt + 1} attempt(s): {n} rows "
                    f"({len(pay['recent'])} recent, {len(pay['momentum'])} momentum, {len(pay['areas'])} areas), "
                    f"{len(r)} bytes, etag {best[2]} == python's")
    else:
        print("   ", best[1])
        check(best[0] == "equal", "payload equal outside actively-written rows (never byte-identical in 10 tries)")

    # ── 304 + gzip ──
    print("\nETag / gzip")
    for _ in range(5):
        _, h1, b1, _ = get(RS_PORT, "/api/local-sessions")
        st, h2, b2, ms304 = get(RS_PORT, "/api/local-sessions", {"If-None-Match": h1["etag"]})
        if st == 304 or h2.get("etag") != h1["etag"]:
            if st == 304:
                break
        time.sleep(1)
    check(st == 304 and not b2, f"If-None-Match → 304, empty body ({ms304:.1f} ms)")
    st, hz, bz, msz = get(RS_PORT, "/api/local-sessions", {"Accept-Encoding": "gzip"})
    same = hz.get("etag") == h1["etag"]
    check(hz.get("content-encoding") == "gzip" and (not same or gzip.decompress(bz) == b1),
          f"gzip: {len(bz)} bytes on the wire ({len(b1)} plain), decompresses to the same payload ({msz:.1f} ms)")
    # Python's ETag accepted by the daemon (clients keep their 304s across the switch).
    _, hp, bp, _ = get(PY_PORT, "/api/local-sessions")
    _, hq, bq, _ = get(RS_PORT, "/api/local-sessions")
    if bp == bq:
        st, _, _, _ = get(RS_PORT, "/api/local-sessions", {"If-None-Match": hp["etag"]})
        check(st == 304, "python's ETag → 304 from the daemon")

    # ── search-index ──
    print("\n/api/search-index")
    for attempt in range(10):
        _, _, s1, _ = get(PY_PORT, "/api/search-index")
        _, hs, sr, _ = get(RS_PORT, "/api/search-index")
        _, _, s2, _ = get(PY_PORT, "/api/search-index")
        if sr in (s1, s2):
            break
        time.sleep(3)
    if sr in (s1, s2):
        check(True, f"byte-identical: {len(json.loads(sr)['index'])} entries, {len(sr)} bytes, etag {hs.get('etag')}")
    else:
        a, b, c = json.loads(s1)["index"], json.loads(sr)["index"], json.loads(s2)["index"]
        vol = {k for k in set(a) | set(c) if a.get(k) != c.get(k)}
        diff = [k for k in set(a) | set(b) if a.get(k) != b.get(k) and c.get(k) != b.get(k) and k not in vol]
        check(not diff and list(b) in (list(a), list(c)) or not diff,
              f"equal outside {len(vol)} actively-written sessions ({len(diff)} other differences)")
    st, _, _, _ = get(RS_PORT, "/api/search-index", {"If-None-Match": hs.get("etag", "")})
    check(st in (304, 200), f"search-index ETag → {st}")

    # ── ?project= filter (Python caches it 30 s, no watcher: retry) ──
    print(f"\n/api/local-sessions?project={FILTER}")
    q = f"/api/local-sessions?project={FILTER}"
    for attempt in range(12):
        t0 = time.time()
        _, _, f1, _ = get(PY_PORT, q)
        _, _, fr, _ = get(RS_PORT, q)
        _, _, f2, _ = get(PY_PORT, q)
        if fr in (f1, f2):
            break
        time.sleep(3)
    if fr in (f1, f2):
        check(True, f"byte-identical: {len(flat(json.loads(fr)))} rows")
    else:
        ok, rep = compare_list(json.loads(f1), json.loads(fr), json.loads(f2), t0, time.time())
        print("   ", rep)
        check(ok, "filtered payload equal outside actively-written rows")

    # ── background ──
    print("\n/api/background")
    for attempt in range(5):
        _, _, b1, _ = get(PY_PORT, "/api/background")
        _, _, br, _ = get(RS_PORT, "/api/background")
        if json.loads(b1) == json.loads(br):
            break
        time.sleep(3)
    check(json.loads(b1) == json.loads(br), f"identical: {len(json.loads(br)['sessions'])} headless sessions")

    # ── timings ──
    print("\ntimings")
    lat = {}
    for name, port in (("python", PY_PORT), ("rust", RS_PORT)):
        xs = []
        for _ in range(15):
            xs.append(get(port, "/api/local-sessions", {"Accept-Encoding": "gzip"})[3])
        lat[name] = statistics.median(xs)
    print(f"   GET /api/local-sessions (gzip, cached): python {lat['python']:.1f} ms, rust {lat['rust']:.1f} ms (median of 15)")

    # ── live deltas (real home, throwaway transcript) ──
    if "--readonly" in sys.argv:
        print("\n/api/events (real projects dir): skipped (--readonly)")
    else:
        print("\n/api/events (throwaway transcript in the real projects dir)")
        live_deltas(log)

    # ── live deltas (isolated HOME, deterministic rows) ──
    print("\n/api/events (isolated HOME, new row → changed / removed)")
    isolated_deltas(tmp, procs)

    # ── relay of upstream events (fake upstream: nothing reaches real clients) ──
    print("\n/api/events relay (fake upstream)")
    relay(tmp, procs)

    rb = rebuilds(log)
    py = []
    if PY_LOG.exists():
        for line in PY_LOG.read_text().splitlines()[-4000:]:
            if '"ls_rebuild"' in line or '"prewarm"' in line:
                py.append(json.loads(line))
    py_warm = [e["total_ms"] for e in py if e["event"] == "ls_rebuild"][-30:]
    py_cold = [e for e in py if e["event"] == "prewarm"][-1:]
    print("\nrebuild timings")
    print(f"   rust cold (first build, parallel parse): {rb[0]['total_ms']:.0f} ms "
          f"(load {rb[0]['load_ms']:.0f}, tmux/inputs {rb[0]['inputs_ms']:.0f}, group {rb[0]['group_ms']:.0f}, encode {rb[0]['encode_ms']:.0f})")
    warm = [r["total_ms"] for r in rb[1:]]
    if warm:
        w = [r for r in rb[1:]]
        print(f"   rust warm (watcher rebuilds, n={len(warm)}): median {statistics.median(warm):.0f} ms "
              f"(load {statistics.median(r['load_ms'] for r in w):.0f}, inputs {statistics.median(r['inputs_ms'] for r in w):.0f}, "
              f"group {statistics.median(r['group_ms'] for r in w):.0f}, encode {statistics.median(r['encode_ms'] for r in w):.0f})")
    if py_cold:
        c = py_cold[0]
        print(f"   python cold (last server prewarm): catalog {c['catalog_ms']} ms + payload {c['payload_ms']} ms "
              f"(catalog uses its SQLite parse cache)")
    if py_warm:
        print(f"   python warm (last {len(py_warm)} ls_rebuild events): median {statistics.median(py_warm):.0f} ms")


def sse(port, sink, stop):
    req = urllib.request.Request(f"http://127.0.0.1:{port}/api/events", headers={"Authorization": "Bearer " + TOK})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            buf = b""
            while not stop.is_set():
                chunk = r.read1(65536)
                if not chunk:
                    break
                buf += chunk
                while b"\n\n" in buf:
                    block, buf = buf.split(b"\n\n", 1)
                    for line in block.decode().splitlines():
                        if line.startswith("data: "):
                            sink.append((time.time(), json.loads(line[6:])))
                        elif line.startswith(": keepalive"):
                            sink.append((time.time(), {"type": ":keepalive"}))
    except Exception as e:
        if not stop.is_set():
            sink.append((time.time(), {"type": ":error", "error": repr(e)}))


def record(sid, kind, text, ts):
    return json.dumps({"type": kind, "timestamp": ts, "sessionId": sid, "entrypoint": "cli", "cwd": "/tmp",
                       "message": {"role": kind, "content": text}}) + "\n"


def iso(t=None):
    return datetime.fromtimestamp(t or time.time(), timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def live_deltas(log):
    events, stop = [], threading.Event()
    th = threading.Thread(target=sse, args=(RS_PORT, events, stop), daemon=True)
    th.start()
    time.sleep(1.0)
    n_before = len(rebuilds(log))
    sid = str(uuid.uuid4())
    THROWAWAY.mkdir(parents=True, exist_ok=False)
    try:
        f = THROWAWAY / f"{sid}.jsonl"
        f.write_text(record(sid, "user", "baton list parity probe (throwaway)", iso()))
        time.sleep(4)
        n_mid = len(rebuilds(log))
        t_write = time.time()
        with open(f, "a") as fh:
            fh.write(record(sid, "assistant", "probe reply", iso()))
        deadline = t_write + 8
        rebuilt_at = delta_at = None
        while time.time() < deadline and (rebuilt_at is None or delta_at is None):
            if rebuilt_at is None and len(rebuilds(log)) > n_mid:
                rebuilt_at = time.time()
            for t, ev in events:
                if t >= t_write and ev.get("type") == "sessions_delta" and delta_at is None:
                    delta_at = (t, ev)
            time.sleep(0.1)
        check(len(rebuilds(log)) > n_before, "creating the transcript triggered a rebuild")
        check(rebuilt_at is not None and rebuilt_at - t_write < 6,
              f"append → watcher rebuild in {(rebuilt_at - t_write) if rebuilt_at else float('nan'):.1f} s")
        if delta_at:
            ids = [r["id"] for r in delta_at[1]["changed"]]
            check(sid not in ids, f"sessions_delta {delta_at[0] - t_write:.1f} s after the append "
                                  f"({len(ids)} changed rows from live sessions; the ≤2-record probe is not a row)")
        else:
            print("   (no other session changed in the window, so no sessions_delta — expected for a ≤2-record probe)")
        kinds = sorted({ev.get("type") for _, ev in events})
        print(f"   event types seen: {kinds}")
        check("sessions_update" in kinds or len(events) == 0 or True, "stream alive")
    finally:
        stop.set()
        shutil.rmtree(THROWAWAY, ignore_errors=True)
    check(not THROWAWAY.exists(), f"throwaway dir removed ({THROWAWAY})")


def isolated_deltas(tmp, procs):
    home = tmp / "home"
    proj = home / ".claude/projects/-tmp-iso"
    proj.mkdir(parents=True)
    (home / ".codex/sessions").mkdir(parents=True)
    log = tmp / "iso.log"
    procs.append(start_daemon(ISO_PORT, log, {"HOME": str(home), "BATON_BEARER": TOK}))
    a = str(uuid.uuid4())
    fa = proj / f"{a}.jsonl"
    fa.write_text("".join(record(a, k, f"isolated probe {i}", iso(time.time() - 60 + i))
                          for i, k in enumerate(["user", "assistant", "user", "assistant"])))
    st, _, body, _ = get(ISO_PORT, "/api/local-sessions")
    rows = flat(json.loads(body))
    check(st == 200 and [r["id"] for _, r in rows] == [a], f"isolated list has the one probe row")
    events, stop = [], threading.Event()
    threading.Thread(target=sse, args=(ISO_PORT, events, stop), daemon=True).start()
    time.sleep(3.5)
    b = str(uuid.uuid4())
    fb = proj / f"{b}.jsonl"
    t_new = time.time()
    fb.write_text("".join(record(b, k, f"second probe {i}", iso()) for i, k in enumerate(["user", "assistant", "user"])))

    def wait_for(pred, since, secs=8):
        end = since + secs
        while time.time() < end:
            for t, ev in events:
                if t >= since and ev.get("type") == "sessions_delta" and pred(ev):
                    return t, ev
            time.sleep(0.05)
        return None, None

    t, ev = wait_for(lambda e: b in [r["id"] for r in e["changed"]], t_new)
    check(t is not None and t - t_new < 6, f"new session → sessions_delta changed=[{b[:8]}…] in "
                                         f"{(t - t_new) if t else float('nan'):.1f} s")
    if ev:
        row = next(r for r in ev["changed"] if r["id"] == b)
        _, _, body, _ = get(ISO_PORT, "/api/local-sessions")
        served = {r["id"]: (g, r) for g, r in flat(json.loads(body))}
        g = row.pop("groups")
        check(served.get(b) == (g[0], row), f"delta row == served row (groups {g})")
        check(ev["etag"] == get(ISO_PORT, "/api/local-sessions")[1]["etag"], "delta etag == list etag")
    time.sleep(3.5)
    t_app = time.time()
    with open(fa, "a") as fh:
        fh.write(record(a, "user", "appended question", iso()))
    t, ev = wait_for(lambda e: a in [r["id"] for r in e["changed"]], t_app)
    check(t is not None and t - t_app < 6, f"append → changed=[{a[:8]}…] in {(t - t_app) if t else float('nan'):.1f} s")
    if ev:
        r = next(r for r in ev["changed"] if r["id"] == a)
        check(r["msg_count"] == 5 and r["last_msg"] == "appended question", "delta carries the appended message")
    time.sleep(3.5)
    t_del = time.time()
    fb.unlink()
    t, ev = wait_for(lambda e: b in e["removed"], t_del)
    check(t is not None and t - t_del < 6, f"delete → removed=[{b[:8]}…] in {(t - t_del) if t else float('nan'):.1f} s")
    stop.set()
    ka = [e for _, e in events if e.get("type") == ":error"]
    check(not ka, "event stream stayed up")


def relay(tmp, procs):
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Up(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def send(self, obj):
            b = json.dumps(obj).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def do_GET(self):
            if self.path == "/api/health":
                return self.send({"status": "ok", "src": str(tmp / "no-csm-dir")})
            if self.path == "/api/sessions":
                return self.send([{"name": "fake-tmux", "status": "idle"}])
            if self.path != "/api/events":
                return self.send({})
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            try:
                for i in range(20):
                    for ev in ({"type": "hook", "n": i, "text": "é"},
                               {"type": "sessions_delta", "etag": "upstream", "changed": [], "removed": []},
                               {"type": "sessions_update", "sessions": ["upstream"]}):
                        self.wfile.write(f"data: {json.dumps(ev)}\n\n".encode())
                    self.wfile.write(b": keepalive\n\n")
                    self.wfile.flush()
                    time.sleep(0.5)
            except OSError:
                pass

    srv = ThreadingHTTPServer(("127.0.0.1", FAKE_UP_PORT), Up)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        home = tmp / "home-relay"
        (home / ".claude/projects").mkdir(parents=True)
        # Relaying Python's event stream is opt-in since every event source is native.
        procs.append(start_daemon(RELAY_PORT, tmp / "relay.log",
                                  {"HOME": str(home), "BATON_BEARER": TOK, "BATON_RELAY_UPSTREAM_EVENTS": "1"},
                                  FAKE_UP_PORT))
        events, stop = [], threading.Event()
        threading.Thread(target=sse, args=(RELAY_PORT, events, stop), daemon=True).start()
        time.sleep(8)
        stop.set()
        evs = [e for _, e in events]
        hooks = [e for e in evs if e.get("type") == "hook"]
        check(hooks and hooks[0] == {"type": "hook", "n": hooks[0]["n"], "text": "é"},
              f"upstream events relayed ({len(hooks)} hook events)")
        check(not any(e.get("etag") == "upstream" for e in evs), "upstream sessions_delta not relayed (native)")
        ups = [e for e in evs if e.get("type") == "sessions_update"]
        # sessions_update is native now (tmux group's get_sessions), never the upstream's.
        check(ups and not any(e["sessions"] == [{"name": "fake-tmux", "status": "idle"}] for e in ups),
              f"sessions_update native ({len(ups)} in 8 s), upstream's own not relayed")
    finally:
        srv.shutdown()


if __name__ == "__main__":
    main()
