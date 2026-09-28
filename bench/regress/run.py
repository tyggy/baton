#!/usr/bin/env python3
"""Baton server regression suite (checks from docs/incidents.md).

    python3 bench/regress/run.py --target python|daemon|<url> [--only INC-012,pane-row-mapping]
                                 [--json out.json] [--bin <baton>] [--ref <claude-session-manager>]
                                 [--skip-slow] [--keep] [--list]

Targets
  python   sandbox: Python api_server.py started by the runner on a spare port
  daemon   sandbox: `baton serve --upstream <sandboxed python>` on a spare port
  <url>    live-readonly (e.g. http://127.0.0.1:19877): only GET-safe (ANY)
           checks; never POSTs, never touches tmux

The sandbox is a temp HOME with synthetic Claude transcripts and Codex
rollouts, a private tmux server pinned by socket (-S) holding csm-bench-*
panes, fake claude/codex/osascript binaries and a fake Codex app-server —
see world.py. A fresh world is built per target.

Statuses: PASS, FAIL (unexpected — exit 1), XFAIL (known incident, still
broken), XPASS (known incident now passing — update the xfail), SKIP.
"""
import argparse
import json
import os
import sys
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import checks  # noqa: E402,F401  (registers the checks)
from core import CHECKS, ANY, Abort, Ctx, Skip, live_token  # noqa: E402
from world import BIN_DEFAULT, REF_DEFAULT, World, live_target, sandbox_target  # noqa: E402


def selected(only, live):
    out = []
    want = [x.strip() for x in (only or "").split(",") if x.strip()]
    for c in CHECKS:
        if live and c["mode"] != ANY:
            continue
        if want and not any(w == c["name"] or w in c["incidents"] for w in want):
            continue
        out.append(c)
    return out


def run_check(c, target, world):
    ctx = Ctx(target, world)
    t0 = time.time()
    status, detail = None, ""
    try:
        c["fn"](ctx)
    except Skip as e:
        status, detail = "SKIP", str(e)
    except Abort:
        pass
    except Exception as e:
        ctx.fails.append(f"check crashed: {type(e).__name__}: {e}")
        ctx.notes.append(traceback.format_exc(limit=3))
    dt = time.time() - t0
    if status is None:
        if ctx.fails:
            status = "FAIL"
        elif ctx.xfails:
            status = "XFAIL"
        elif ctx.xpasses:
            status = "XPASS"
        else:
            status = "PASS"
    return {"check": c["name"], "incidents": c["incidents"], "mode": c["mode"], "status": status,
            "seconds": round(dt, 2), "fails": ctx.fails, "xfails": ctx.xfails, "xpasses": ctx.xpasses,
            "notes": ctx.notes, "skip": detail, "doc": c["doc"]}


def print_table(target_name, results):
    print(f"\n== {target_name}")
    w = max(len(r["check"]) for r in results) if results else 10
    for r in results:
        inc = ",".join(i.replace("INC-", "") for i in r["incidents"])
        print(f"  {r['status']:<5}  {r['check']:<{w}}  {r['seconds']:6.1f}s  INC {inc}")
        for f in r["fails"]:
            print(f"           FAIL  {f}")
        for f in r["xfails"]:
            print(f"           xfail {f}")
        for f in r["xpasses"]:
            print(f"           XPASS {f}")
        if r["skip"]:
            print(f"           skip  {r['skip']}")
        for n in r["notes"]:
            if not n.startswith("Traceback"):
                print(f"           note  {n}")
            else:
                print("           " + n.strip().replace("\n", "\n           "))
    counts = {}
    for r in results:
        counts[r["status"]] = counts.get(r["status"], 0) + 1
    print("  " + "  ".join(f"{k} {counts.get(k, 0)}" for k in ("PASS", "FAIL", "XFAIL", "XPASS", "SKIP")))
    return counts


def run_target(name, a):
    live = name not in ("python", "daemon")
    todo = selected(a.only, live)
    if not todo:
        print(f"{name}: no checks selected")
        return [], {}
    if live:
        target = live_target(name, live_token())
        world = None
        results = [run_check(c, target, world) for c in todo]
        return results, print_table(f"{name} (live-readonly, {target.impl})", results)
    world = World(keep=a.keep)
    target = None
    try:
        target = sandbox_target(name, world, a.ref, a.bin)
        target.binary, target.ref = a.bin, a.ref
        # the pane mapping check reads the first list; run it first
        todo.sort(key=lambda c: (c["name"] != "pane-row-mapping", c["slow"]))
        results = []
        for c in todo:
            if a.skip_slow and c["slow"]:
                continue
            results.append(run_check(c, target, world))
            if target.main.proc.poll() is not None:
                results.append({"check": "server-alive", "incidents": [], "mode": "sandbox", "status": "FAIL",
                                "seconds": 0, "fails": [f"{name} exited (rc {target.main.proc.returncode}) after {c['name']}"],
                                "xfails": [], "xpasses": [], "notes": [], "skip": "", "doc": ""})
                break
        return results, print_table(f"{name} (sandbox, port {target.main.port})", results)
    finally:
        if target:
            target.stop()
        world.close()
        if a.keep:
            print(f"kept sandbox {world.root}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target", action="append", required=False,
                    help="python | daemon | http://host:port (repeatable; default python + daemon)")
    ap.add_argument("--only", help="comma list of check names or incident ids")
    ap.add_argument("--json", help="write results here")
    ap.add_argument("--bin", default=os.environ.get("BATON_BIN", str(BIN_DEFAULT)))
    ap.add_argument("--ref", default=str(REF_DEFAULT))
    ap.add_argument("--skip-slow", action="store_true")
    ap.add_argument("--keep", action="store_true", help="keep the sandbox dirs")
    ap.add_argument("--list", action="store_true", help="list checks and exit")
    a = ap.parse_args()
    if a.list:
        for c in CHECKS:
            print(f"{c['mode']:<8} {c['name']:<22} {','.join(c['incidents']):<40} {c['doc']}")
        return 0
    all_results, bad = {}, False
    for t in a.target or ["python", "daemon"]:
        try:
            results, counts = run_target(t, a)
        except Exception as e:
            print(f"\n== {t}: could not run: {e}")
            results, counts = [], {"FAIL": 1}
        all_results[t] = results
        bad |= counts.get("FAIL", 0) > 0
    if a.json:
        Path(a.json).write_text(json.dumps(all_results, indent=1))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
