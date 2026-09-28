"""Shadow check: replay the phone's recent GETs against the live Python
server and the shadow daemon, and log every real difference.

    python3 bench/shadow.py [--py http://127.0.0.1:19877] [--daemon http://127.0.0.1:19880]
                            [--n 150] [--log ~/Library/Logs/Baton/shadow.jsonl]

Read-only: GETs only, no streams. Each path is fetched python → daemon →
python; a daemon answer matching either python answer is "same", and one
differing from both while python itself changed between the two fetches is
"live" (data moved underneath). Only the rest are "diff" — the cut-over gate
(docs/migrating.md) is zero diffs over a day of shadow runs.

Run by the com.baton.shadow LaunchAgent every 15 minutes during the shadow
period; exit code 1 when any diff was found.
"""
import argparse
import json
import sys
import time
from collections import Counter
from pathlib import Path
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).parent / "regress"))
sys.path.insert(0, str(Path(__file__).parent))
from core import Client, live_token  # noqa: E402
from ab import REQ_LOG, endpoint_of, first_diff, norm_body  # noqa: E402

# Endpoints where the daemon differs from Python on purpose (docs/incidents.md):
# /api/pairing is daemon-only, /api/browse answers 4xx/200 where Python 500s.
INTENDED = ("/api/pairing", "/api/browse")

SKIP = ("/api/events", "/api/tail/", "/api/pane-stream/", "/api/bench", "/api/voice")


def recent_paths(n, log=None):
    """The n most recent distinct phone GET paths (non-local clients)."""
    seen, out = set(), []
    for line in reversed(Path(log or REQ_LOG).read_text(errors="replace").splitlines()[-40000:]):
        try:
            r = json.loads(line)
        except Exception:
            continue
        p = r.get("path", "")
        if (r.get("m") != "GET" or str(r.get("client", "")).startswith("127.0.0.1")
                or p.startswith(SKIP) or p in seen):
            continue
        seen.add(p)
        out.append(p)
        if len(out) >= n:
            break
    return out


def coarse(body):
    """Normalised body with floats rounded to one decimal: time-based scores
    (importance, momentum) drift between two fetches a few ms apart."""
    try:
        j = json.loads(norm_body(body))
    except Exception:
        return body

    def r(x):
        if isinstance(x, float):
            return round(x, 1)
        if isinstance(x, dict):
            return {k: r(v) for k, v in x.items()}
        if isinstance(x, list):
            return [r(v) for v in x]
        return x
    return json.dumps(r(j), sort_keys=True)


def client(url, token):
    u = urlsplit(url)
    return Client(u.hostname, u.port or 80, token)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--py", default="http://127.0.0.1:19877")
    ap.add_argument("--daemon", default="http://127.0.0.1:19880")
    ap.add_argument("--n", type=int, default=150)
    ap.add_argument("--requests", help="request log to replay (default: the Python server's; "
                    "after the cut-over, ~/Library/Logs/Baton/cs-server.requests.jsonl)")
    ap.add_argument("--log", default=str(Path.home() / "Library/Logs/Baton/shadow.jsonl"))
    a = ap.parse_args()
    token = live_token()
    py, dm = client(a.py, token), client(a.daemon, token)
    if dm.get("/api/health", timeout=3).status != 200:
        print("shadow daemon not answering", file=sys.stderr)
        return 2
    counts, diffs, ms = Counter(), [], {"py": [], "daemon": []}
    for p in recent_paths(a.n, a.requests):
        r1, rd, r2 = py.get(p, timeout=20), dm.get(p, timeout=20), py.get(p, timeout=20)
        ms["py"].append(r1.ms)
        ms["daemon"].append(rd.ms)
        n1, nd, n2 = (norm_body(r.body or b"") for r in (r1, rd, r2))
        same = lambda r, n: rd.status == r.status and nd == n  # noqa: E731
        if same(r1, n1) or same(r2, n2):
            counts["same"] += 1
        elif p.startswith(INTENDED):
            counts["intended"] += 1
        elif rd.status == r1.status and coarse(rd.body or b"") == coarse(r1.body or b""):
            counts["same"] += 1
        elif n1 != n2 or r1.status != r2.status:
            counts["live"] += 1
        else:
            counts["diff"] += 1
            detail = (f"status {r1.status} vs {rd.status}" if r1.status != rd.status
                      else first_diff(r1.body or b"", rd.body or b""))
            diffs.append({"endpoint": endpoint_of(p), "path": p[:200], "detail": str(detail)[:300]})
    med = lambda v: sorted(v)[len(v) // 2] if v else None  # noqa: E731
    rec = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S"), **counts,
           "py_p50_ms": round(med(ms["py"]) or 0, 1), "daemon_p50_ms": round(med(ms["daemon"]) or 0, 1),
           "diffs": diffs[:20]}
    Path(a.log).parent.mkdir(parents=True, exist_ok=True)
    with open(a.log, "a") as f:
        f.write(json.dumps(rec) + "\n")
    print(f"same {counts['same']}  intended {counts['intended']}  live {counts['live']}  diff {counts['diff']}  "
          f"p50 py {rec['py_p50_ms']} ms / daemon {rec['daemon_p50_ms']} ms")
    for d in diffs[:10]:
        print(f"  DIFF {d['endpoint']}: {d['detail']}")
    return 1 if diffs else 0


if __name__ == "__main__":
    sys.exit(main())
