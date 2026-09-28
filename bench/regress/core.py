"""Shared plumbing for the regression suite and bench/ab.py: HTTP client,
SSE reader, check registry and result bookkeeping. Standard library only."""
import http.client
import json
import os
import socket
import subprocess
import threading
import time
from pathlib import Path

HOME = Path.home()


# ── auth ────────────────────────────────────────────────────────────────

def live_token():
    """BATON_BEARER from the environment or ~/.claude-sessions/config."""
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


# ── HTTP ────────────────────────────────────────────────────────────────

class Resp:
    __slots__ = ("status", "headers", "body", "ms", "error")

    def __init__(self, status, headers, body, ms, error=None):
        self.status, self.headers, self.body, self.ms, self.error = status, headers, body, ms, error

    def json(self):
        try:
            return json.loads(self.body)
        except Exception:
            return None

    def __repr__(self):
        return f"<{self.status} {len(self.body or b'')}B {self.ms:.0f}ms{' ' + self.error if self.error else ''}>"


class Client:
    """Tiny HTTP client bound to one server. Every call returns a Resp;
    transport errors come back as status 0 with `error` set (never raised),
    so a hang or a reset is a check failure, not a crash of the runner."""

    def __init__(self, host, port, token=""):
        self.host, self.port, self.token = host, port, token

    def request(self, method, path, body=None, raw=None, headers=None, timeout=15.0, auth=True):
        h = dict(headers or {})
        if auth and self.token:
            h.setdefault("Authorization", "Bearer " + self.token)
        data = raw
        if body is not None:
            data = json.dumps(body).encode()
            h.setdefault("Content-Type", "application/json")
        t0 = time.perf_counter()
        c = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
        try:
            c.request(method, path, body=data, headers=h)
            r = c.getresponse()
            b = r.read()
            return Resp(r.status, {k.lower(): v for k, v in r.getheaders()}, b,
                        (time.perf_counter() - t0) * 1000)
        except Exception as e:  # timeout, reset, refused
            return Resp(0, {}, b"", (time.perf_counter() - t0) * 1000, f"{type(e).__name__}: {e}")
        finally:
            c.close()

    def get(self, path, **kw):
        return self.request("GET", path, **kw)

    def post(self, path, body=None, **kw):
        return self.request("POST", path, body=body, **kw)

    def sse(self, path, headers=None, timeout=30.0):
        return SSE(self, path, headers, timeout)


class SSE:
    """Background SSE reader: keeps raw events (`data:`/`event:` blocks,
    keepalives dropped) with their arrival time."""

    def __init__(self, client, path, headers=None, timeout=30.0):
        self.events = []          # (t, raw bytes)
        self.status = None
        self.ctype = None
        self.error = None
        self.bytes = 0
        self.t_open = time.perf_counter()
        self.ready = threading.Event()
        self._conn = None
        self._closed = False
        h = dict(headers or {})
        if client.token:
            h["Authorization"] = "Bearer " + client.token
        self._t = threading.Thread(target=self._run, args=(client, path, h, timeout), daemon=True)
        self._t.start()

    def _run(self, client, path, h, timeout):
        buf = b""
        try:
            self._conn = http.client.HTTPConnection(client.host, client.port, timeout=timeout)
            self._conn.request("GET", path, headers=h)
            r = self._conn.getresponse()
            self.status, self.ctype = r.status, r.getheader("Content-Type")
            self.ready.set()
            if r.status != 200:
                self.bytes += len(r.read())
                return
            while not self._closed:
                chunk = r.read1(65536)
                if not chunk:
                    return
                self.bytes += len(chunk)
                buf += chunk
                while b"\n\n" in buf:
                    ev, buf = buf.split(b"\n\n", 1)
                    if ev.strip() and not ev.startswith(b":"):
                        self.events.append((time.perf_counter(), ev))
        except Exception as e:
            if not self._closed:
                self.error = f"{type(e).__name__}: {e}"
        finally:
            self.ready.set()

    def data(self):
        """Decoded `data:` payloads (JSON when it parses, else the string)."""
        out = []
        for _, ev in self.events:
            for line in ev.split(b"\n"):
                if line.startswith(b"data: ") or line.startswith(b"data:"):
                    s = line.split(b":", 1)[1].lstrip(b" ").decode("utf-8", "replace")
                    try:
                        out.append(json.loads(s))
                    except Exception:
                        out.append(s)
        return out

    def wait(self, pred, secs):
        end = time.time() + secs
        while time.time() < end:
            if any(pred(d) for d in self.data()):
                return True
            time.sleep(0.1)
        return False

    def close(self, rst=False):
        self._closed = True
        c = self._conn
        if c is not None and c.sock is not None:
            try:
                if rst:  # abortive close: RST instead of FIN
                    import struct
                    c.sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                    c.sock.close()
                else:
                    c.sock.shutdown(socket.SHUT_RDWR)
            except Exception:
                pass
        try:
            if c is not None:
                c.close()
        except Exception:
            pass


def spare_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def port_open(port, host="127.0.0.1"):
    try:
        socket.create_connection((host, port), 0.2).close()
        return True
    except OSError:
        return False


def wait_http(client, secs, path="/api/health"):
    """Seconds until `path` answers 200, or None."""
    t0 = time.time()
    while time.time() - t0 < secs:
        r = client.get(path, timeout=2)
        if r.status == 200:
            return time.time() - t0
        time.sleep(0.05)
    return None


def proc_stats(pid):
    """(rss_kb, cpu_seconds) of one process via ps; None when gone."""
    try:
        out = subprocess.run(["ps", "-o", "rss=,time=", "-p", str(pid)],
                             capture_output=True, text=True, timeout=5).stdout.split()
    except Exception:
        return None
    if len(out) < 2:
        return None
    rss = int(out[0])
    t = out[1]  # [[dd-]hh:]mm:ss.cc
    days = 0
    if "-" in t:
        d, t = t.split("-", 1)
        days = int(d)
    secs = 0.0
    for part in t.split(":"):
        secs = secs * 60 + float(part)
    return rss, secs + days * 86400


def pct(xs, p):
    if not xs:
        return float("nan")
    xs = sorted(xs)
    k = (len(xs) - 1) * p / 100
    f = int(k)
    c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)


# ── checks ──────────────────────────────────────────────────────────────

CHECKS = []

SANDBOX, ANY = "sandbox", "any"


def check(incidents, *, mode=ANY, xfail=None, name=None, slow=False):
    """Register a check. `incidents`: "INC-001,INC-011". `mode`: ANY checks
    are GET-only (safe on a live server); SANDBOX checks need the fake
    world. `xfail`: {impl: "INC-xxx why"} (impl = python | daemon | *) —
    the whole check is a known failure there."""
    def deco(fn):
        CHECKS.append({
            "fn": fn, "name": name or fn.__name__.replace("_", "-"),
            "incidents": [i.strip() for i in incidents.split(",") if i.strip()],
            "mode": mode, "xfail": xfail or {}, "slow": slow,
            "doc": (fn.__doc__ or "").strip().split("\n")[0],
        })
        return fn
    return deco


class Skip(Exception):
    pass


class Abort(Exception):
    """Stop the check here (a hard failure was already recorded)."""


def known_for(spec, impl):
    """spec: None | "INC-x" (all impls) | {"python": "INC-x", ...}."""
    if not spec:
        return None
    if isinstance(spec, str):
        return spec
    return spec.get(impl) or spec.get("*")


class Ctx:
    """What a check sees: the server under test (client, impl), the sandbox
    world (None in live mode), and soft assertions."""

    def __init__(self, target, world):
        self.target = target
        self.c = target.client
        self.impl = target.impl
        self.world = world
        self.live = world is None
        self.fails, self.xfails, self.xpasses, self.notes = [], [], [], []

    def expect(self, cond, msg, known=None):
        """Soft assertion. `known`: incident(s) where this is a known bug
        (for this impl it counts as XFAIL when it fails, XPASS when not)."""
        k = known_for(known, self.impl)
        if cond:
            if k:
                self.xpasses.append(f"{msg} [{k}]")
        elif k:
            self.xfails.append(f"{msg} [{k}]")
        else:
            self.fails.append(msg)
        return bool(cond)

    def require(self, cond, msg, known=None):
        if not self.expect(cond, msg, known):
            raise Abort()

    def note(self, msg):
        self.notes.append(msg)

    def skip(self, msg):
        raise Skip(msg)
