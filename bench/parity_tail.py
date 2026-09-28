#!/usr/bin/env python3
"""Parity for /api/tail on a live-changing transcript: Python's
`_stream_tail` (run in-process against a fixture) vs a baton daemon whose
HOME points at the same fixture. Exercises growth in pieces, a partial
line completed later, CRLF endings, Codex conversion, truncation and the
file disappearing (`event: error`). Nothing outside a temp dir is touched.

    python3 bench/parity_tail.py [--bin <baton>] [--ref <claude-session-manager>]
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

HOME = Path.home()


class FakeWfile:
    def __init__(self):
        self.buf = bytearray()
        self.lock = threading.Lock()

    def write(self, b):
        with self.lock:
            self.buf.extend(b)

    def flush(self):
        pass


class FakeHandler:
    def __init__(self):
        self.wfile = FakeWfile()
        self.headers = {}

    def send_response(self, *a):
        pass

    def send_header(self, *a):
        pass

    def end_headers(self):
        pass

    def send_json(self, data, status=200):
        self.wfile.write(json.dumps(data).encode())


def sse_reader(port, path, out, stop):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    c.request("GET", path)
    r = c.getresponse()
    sock = c.sock or getattr(getattr(r.fp, "raw", None), "_sock", None)
    stop.append(sock)
    try:
        while True:
            chunk = r.read1(65536)
            if not chunk:
                return
            out.extend(chunk)
    except Exception:
        return


def events(b):
    return [e for e in bytes(b).split(b"\n\n") if e and e != b": keepalive"]


def rec(i, typ="user", ts=None):
    ts = ts or f"2026-09-27T10:00:{i:02d}.000Z"
    return json.dumps({"type": typ, "timestamp": ts, "message": {"role": typ, "content": f"line {i} é"}}).encode()


def codex_rec(i):
    return json.dumps({"type": "response_item", "timestamp": f"2026-09-27T11:00:{i:02d}Z",
                       "payload": {"type": "message", "role": "assistant",
                                   "content": [{"type": "output_text", "text": f"codex {i} ✓"}]}}).encode()


def codex_call(i):
    return json.dumps({"type": "response_item", "timestamp": f"2026-09-27T11:01:{i:02d}Z",
                       "payload": {"type": "function_call", "name": "shell", "call_id": f"c{i}",
                                   "arguments": json.dumps({"command": ["bash", "-lc", f"echo {i}"]})}}).encode()


def scenario(a, A, kind, path, url_suffix, steps):
    """Run both streams over the same file while `steps` mutate it."""
    h = FakeHandler()
    sid = path.stem if kind == "claude" else path.stem[-36:]
    py_thread = threading.Thread(target=lambda: _safe(A.APIHandler._stream_tail, h, sid, *url_suffix[1]),
                                 daemon=True)
    rs_out, stop = bytearray(), []
    rs_thread = threading.Thread(target=sse_reader, args=(a.port, f"/api/tail/{sid}{url_suffix[0]}", rs_out, stop),
                                 daemon=True)
    py_thread.start()
    rs_thread.start()
    for delay, action in steps:
        time.sleep(delay)
        action()
    time.sleep(2.6)
    for s in stop:
        try:
            s.shutdown(socket.SHUT_RDWR)
        except Exception:
            pass
    rs_thread.join(3)
    return events(h.wfile.buf), events(rs_out)


def _safe(fn, *args):
    try:
        fn(*args)
    except Exception:
        pass


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=str(Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton"))
    ap.add_argument("--ref", default=str(HOME / "GitHub/workbench-exporter-master/claude-session-manager"))
    ap.add_argument("--port", type=int, default=19884)
    a = ap.parse_args()

    fake = Path(tempfile.mkdtemp(prefix="baton-tail-"))
    proj = fake / ".claude/projects/-tmp-proj"
    proj.mkdir(parents=True)
    codex_dir = fake / ".codex/sessions/2026/09/27"
    codex_dir.mkdir(parents=True)

    sys.path.insert(0, a.ref)
    sys.path.insert(0, a.ref + "/server")
    import api_server as A
    A.CLAUDE_PROJECTS_DIR = fake / ".claude/projects"
    A.CODEX_SESSIONS_DIR = fake / ".codex/sessions"

    env = dict(os.environ, HOME=str(fake))
    env.pop("BATON_BEARER", None)
    proc = subprocess.Popen([a.bin, "serve", "--listen", f"127.0.0.1:{a.port}"], env=env,
                            stderr=subprocess.DEVNULL)
    time.sleep(0.8)
    results = []
    try:
        # 1. default mode: starts at EOF (mid-record partial kept), growth in pieces, CRLF, truncation, deletion
        f1 = proj / "11111111-1111-4111-8111-111111111111.jsonl"
        f1.write_bytes(rec(1) + b"\n" + rec(2) + b"\n" + rec(3)[:20])

        def append(p, b):
            with open(p, "ab") as fh:
                fh.write(b)

        steps = [
            (0.4, lambda: append(f1, rec(3)[20:] + b"\n" + rec(4)[:10])),
            (1.3, lambda: append(f1, rec(4)[10:] + b"\n" + rec(5) + b"\r\n" + rec(6) + b"\n")),
            (1.3, lambda: append(f1, b"   \n\n" + rec(7) + b"\n")),
            (1.3, lambda: f1.write_bytes(rec(8) + b"\n")),          # rewritten smaller: follow the new end
            (1.3, lambda: append(f1, rec(9) + b"\n")),
            (1.3, lambda: f1.unlink()),                              # event: error
        ]
        results.append(("default + growth/truncate/delete", *scenario(a, A, "claude", f1, ("", (None, False)), steps)))

        # 2. ?since= replays newer records, then follows growth
        f2 = proj / "22222222-2222-4222-8222-222222222222.jsonl"
        f2.write_bytes(b"".join(rec(i) + b"\n" for i in range(1, 9)) + b'{"type":"summary"}\n')
        steps = [(1.2, lambda: append(f2, rec(10) + b"\n"))]
        results.append(("since", *scenario(a, A, "claude", f2, ("?since=2026-09-27T10:00:05.000Z",
                                                                 ("2026-09-27T10:00:05.000Z", False)), steps)))

        # 3. ?from=0 full replay
        results.append(("from=0", *scenario(a, A, "claude", f2, ("?from=0", (None, True)), [])))

        # 4. Codex rollout: converted records, non-message lines skipped
        f3 = codex_dir / "rollout-2026-09-27T11-00-00-33333333-3333-4333-8333-333333333333.jsonl"
        f3.write_bytes(json.dumps({"type": "session_meta", "payload": {"id": "x"}}).encode() + b"\n"
                       + codex_rec(1) + b"\n" + codex_call(2) + b"\n")
        steps = [(0.5, lambda: append(f3, codex_rec(3) + b"\n" + codex_call(4) + b"\n"
                                      + json.dumps({"type": "event_msg", "payload": {}}).encode() + b"\n"))]
        results.append(("codex from=0 + growth", *scenario(a, A, "codex", f3, ("?from=0", (None, True)), steps)))
    finally:
        proc.terminate()
        shutil.rmtree(fake, ignore_errors=True)

    bad = 0
    for name, py, rs in results:
        same = py == rs
        bad += not same
        print(f"{name:34} py {len(py):>2} events, daemon {len(rs):>2} -> {'equal' if same else 'DIFF'}")
        if not same:
            for x, y in zip(py + [b""] * 20, rs + [b""] * 20):
                if x != y:
                    print("   py:", x[:160])
                    print("   rs:", y[:160])
                    break
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
