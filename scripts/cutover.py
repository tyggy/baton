#!/usr/bin/env python3
"""Move the author's Mac from the Python server to the Rust daemon, with an
automatic rollback.

    scripts/cutover.py start      # daemon takes :19877, Python moves to :19878
    scripts/cutover.py status
    scripts/cutover.py rollback   # Python back on :19877, daemon back to :19880
    scripts/cutover.py watch      # one watchdog tick (run by launchd)

Before: Python (com.workbench.cs-server) on 0.0.0.0:19877 serves the phone;
the daemon (Baton.app's com.torwood.baton.daemon) shadows on 127.0.0.1:19880.
After `start`: the daemon listens on 0.0.0.0:19877 (same address the phone
and hooks use) and proxies what it doesn't implement to Python, now on
127.0.0.1:19878. A watchdog LaunchAgent checks every 20 s and rolls back
after 3 failed ticks in a row (≈1 min), with a notification.

Every step is logged to ~/Library/Logs/Baton/cutover.log. The original
Python plist and app settings are saved first and restored byte-for-byte.
"""
import json
import os
import plistlib
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

HOME = Path.home()
UID = os.getuid()
PY_LABEL = "com.workbench.cs-server"
PY_PLIST = HOME / f"Library/LaunchAgents/{PY_LABEL}.plist"
DAEMON_LABEL = "com.torwood.baton.daemon"
APP_DOMAIN = "com.torwood.baton.mac"
DAEMON_CONF = HOME / "Library/Application Support/Baton/daemon.conf"
WATCH_LABEL = "com.baton.cutover-watchdog"
WATCH_PLIST = HOME / f"Library/LaunchAgents/{WATCH_LABEL}.plist"
STATE_DIR = HOME / "Library/Application Support/Baton/cutover"
STATE = STATE_DIR / "state.json"
LOG = HOME / "Library/Logs/Baton/cutover.log"

LIVE, PY_NEW, SIDECAR = 19877, 19878, 19880
FAILS_TO_ROLLBACK = 3

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "bench/regress"))
from core import live_token  # noqa: E402


def log(msg):
    LOG.parent.mkdir(parents=True, exist_ok=True)
    line = f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}"
    with open(LOG, "a") as f:
        f.write(line + "\n")
    print(line, flush=True)


def run(*args, check=False):
    r = subprocess.run(list(args), capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(args)} → {r.returncode}: {r.stderr.strip()[:200]}")
    return r


def _get(port, path, timeout):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}",
                                 headers={"Authorization": "Bearer " + live_token()})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, r.read()


def health(port, timeout=3.0):
    """Parsed /api/health on 127.0.0.1:port, or None."""
    try:
        return json.loads(_get(port, "/api/health", timeout)[1])
    except Exception:
        return None


def get_ok(port, path, timeout=10.0):
    try:
        return _get(port, path, timeout)[0] == 200
    except Exception:
        return False


def is_daemon(h):
    return bool(h) and "daemon" in h


def wait(pred, secs, step=0.3):
    end = time.time() + secs
    while time.time() < end:
        if pred():
            return True
        time.sleep(step)
    return False


def port_free(port):
    s = socket.socket()
    try:
        s.bind(("0.0.0.0", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def set_daemon(listen, upstream):
    run("defaults", "write", APP_DOMAIN, "daemon.listen", listen, check=True)
    run("defaults", "write", APP_DOMAIN, "daemon.upstream", upstream, check=True)
    DAEMON_CONF.parent.mkdir(parents=True, exist_ok=True)
    DAEMON_CONF.write_text("# Written by Baton.app — edit in the app (Onboarding → Daemon).\n"
                           f"listen={listen}\nupstream={upstream}\n")
    run("launchctl", "kickstart", "-k", f"gui/{UID}/{DAEMON_LABEL}", check=True)


def restart_python(plist_bytes):
    run("launchctl", "bootout", f"gui/{UID}/{PY_LABEL}")
    wait(lambda: run("launchctl", "print", f"gui/{UID}/{PY_LABEL}").returncode != 0, 15)
    PY_PLIST.write_bytes(plist_bytes)
    run("launchctl", "bootstrap", f"gui/{UID}", str(PY_PLIST), check=True)


def load_state():
    return json.loads(STATE.read_text()) if STATE.exists() else None


def save_state(st):
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    STATE.write_text(json.dumps(st, indent=2))


def notify(msg):
    run("osascript", "-e", f'display notification "{msg}" with title "Baton"')


# ── commands ────────────────────────────────────────────────────────────

def start():
    if load_state():
        print("cut-over already active (scripts/cutover.py status / rollback)")
        return 1
    ph, dh = health(LIVE), health(SIDECAR)
    if not ph or is_daemon(ph):
        print(f"preflight: :{LIVE} is not the Python server ({ph})")
        return 1
    if not is_daemon(dh):
        print(f"preflight: daemon not answering on :{SIDECAR} ({dh})")
        return 1
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    backup = STATE_DIR / f"{PY_LABEL}.plist.orig"
    shutil.copy2(PY_PLIST, backup)
    prev = {k: run("defaults", "read", APP_DOMAIN, k).stdout.strip() or None
            for k in ("daemon.listen", "daemon.upstream")}
    save_state({"started": time.time(), "plist_backup": str(backup), "defaults": prev,
                "fails": 0, "phase": "starting"})
    log(f"start: python :{LIVE} → :{PY_NEW}, daemon :{SIDECAR} → :{LIVE}")
    try:
        d = plistlib.loads(backup.read_bytes())
        a = d["ProgramArguments"]
        a[a.index("--port") + 1] = str(PY_NEW)
        a[a.index("--host") + 1] = "127.0.0.1"
        restart_python(plistlib.dumps(d))
        if not wait(lambda: health(PY_NEW) is not None and port_free(LIVE), 45):
            raise RuntimeError(f"python didn't come up on :{PY_NEW} / free :{LIVE}")
        log(f"python up on :{PY_NEW}")
        set_daemon(f"0.0.0.0:{LIVE}", f"http://127.0.0.1:{PY_NEW}")
        if not wait(lambda: is_daemon(health(LIVE)), 30):
            raise RuntimeError(f"daemon didn't come up on :{LIVE}")
        if not get_ok(LIVE, "/api/local-sessions", 30):
            raise RuntimeError("daemon /api/local-sessions failed")
        log(f"daemon serving :{LIVE} (upstream :{PY_NEW})")
    except Exception as e:
        log(f"start failed: {e} — rolling back")
        rollback()
        return 1
    WATCH_PLIST.write_bytes(plistlib.dumps({
        "Label": WATCH_LABEL,
        "ProgramArguments": [sys.executable, str(Path(__file__).resolve()), "watch"],
        "StartInterval": 20, "RunAtLoad": False, "ProcessType": "Background",
        "EnvironmentVariables": {"LANG": "en_US.UTF-8"},
        "StandardOutPath": str(LOG), "StandardErrorPath": str(LOG),
    }))
    run("launchctl", "bootstrap", f"gui/{UID}", str(WATCH_PLIST), check=True)
    st = load_state()
    st["phase"] = "live"
    save_state(st)
    log("cut-over live; watchdog armed")
    return 0


def rollback():
    st = load_state() or {}
    log("rollback: python back to :19877, daemon back to :19880")
    run("launchctl", "bootout", f"gui/{UID}/{WATCH_LABEL}")
    WATCH_PLIST.unlink(missing_ok=True)
    backup = Path(st.get("plist_backup") or STATE_DIR / f"{PY_LABEL}.plist.orig")
    prev = st.get("defaults") or {}
    # Daemon off the live port first, then Python back on it.
    set_daemon(prev.get("daemon.listen") or f"127.0.0.1:{SIDECAR}",
               prev.get("daemon.upstream") or f"http://127.0.0.1:{LIVE}")
    wait(lambda: not is_daemon(health(LIVE)), 20)
    if backup.exists():
        restart_python(backup.read_bytes())
    ok = wait(lambda: (h := health(LIVE)) is not None and not is_daemon(h), 45)
    log(f"rollback {'done' if ok else 'INCOMPLETE — check launchctl'}: python :{LIVE} "
        f"{'up' if ok else 'down'}, daemon :{SIDECAR} {'up' if is_daemon(health(SIDECAR)) else 'down'}")
    if ok:
        STATE.unlink(missing_ok=True)
    return 0 if ok else 1


def watch():
    st = load_state()
    if not st or st.get("phase") != "live":
        return 0
    problems = []
    h = health(LIVE, timeout=5)
    if not is_daemon(h):
        problems.append(f":{LIVE} health {'answered by python' if h else 'failed'}")
    elif not get_ok(LIVE, "/api/local-sessions", 20):
        problems.append("/api/local-sessions failed")
    if health(PY_NEW, timeout=5) is None:
        problems.append(f"python :{PY_NEW} down")
    if not problems:
        if st.get("fails"):
            log("watchdog: healthy again")
        st["fails"] = 0
        save_state(st)
        return 0
    st["fails"] = st.get("fails", 0) + 1
    save_state(st)
    log(f"watchdog: {'; '.join(problems)} ({st['fails']}/{FAILS_TO_ROLLBACK})")
    if st["fails"] >= FAILS_TO_ROLLBACK:
        notify("Rust daemon unhealthy — rolled back to the Python server")
        return rollback()
    return 0


def status():
    st = load_state()
    ph, lh, sh = health(PY_NEW), health(LIVE), health(SIDECAR)
    who = "daemon" if is_daemon(lh) else ("python" if lh else "nothing")
    print(f"cut-over: {st['phase'] if st else 'not active'}"
          + (f" since {time.strftime('%H:%M', time.localtime(st['started']))}, fails {st.get('fails', 0)}" if st else ""))
    print(f"  :{LIVE}  {who}")
    print(f"  :{PY_NEW}  {'python' if ph else '-'}")
    print(f"  :{SIDECAR}  {'daemon' if is_daemon(sh) else '-'}")
    return 0


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "status"
    fn = {"start": start, "rollback": rollback, "watch": watch, "status": status}.get(cmd)
    if not fn:
        print(__doc__)
        sys.exit(2)
    sys.exit(fn())
