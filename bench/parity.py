#!/usr/bin/env python3
"""Parity: Rust baton-core vs the Python reference (sessions_core.py).

    python3 bench/parity.py [--ref ~/GitHub/workbench-exporter-master/claude-session-manager]
                            [--bin <baton-parse>] [--cuts 6] [--limit N]

Parses every Claude transcript and Codex rollout on this machine with both
implementations and compares the resulting dicts field by field (the Python
parse cache is bypassed). Also runs the Rust incremental-vs-full check at
random cut points on the largest files. Exit code 1 on any mismatch — the
slice-1 promotion gate.
"""
import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

HOME = Path.home()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ref", default=str(HOME / "GitHub/workbench-exporter-master/claude-session-manager"))
    ap.add_argument("--bin", default=os.environ.get("BATON_PARSE",
                                                    str(Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton-parse")))
    ap.add_argument("--cuts", type=int, default=6)
    ap.add_argument("--cut-files", type=int, default=12)
    ap.add_argument("--limit", type=int, default=0)
    a = ap.parse_args()

    sys.path.insert(0, a.ref)
    import sessions_cache
    sessions_cache.get_cached = lambda p: None
    sessions_cache.store = lambda p, r: None
    import sessions_core as sc

    files = [("claude", p) for p in sorted(HOME.glob(".claude/projects/*/*.jsonl")) if not p.stem.startswith("agent-")]
    files += [("codex", p) for p in sorted(HOME.glob(".codex/sessions/**/rollout-*.jsonl"))]
    if a.limit:
        files = files[:a.limit]
    total_mb = sum(p.stat().st_size for _, p in files) / 1e6
    print(f"{len(files)} transcripts, {total_mb:.0f} MB")

    t = time.perf_counter()
    ref = {}
    for kind, p in files:
        ref[str(p)] = sc.parse_session(p) if kind == "claude" else sc.parse_codex_session(p)
    t_py = time.perf_counter() - t

    stdin = "".join(f"{k}\t{p}\n" for k, p in files)
    t = time.perf_counter()
    r = subprocess.run([a.bin], input=stdin, capture_output=True, text=True)
    t_rs = time.perf_counter() - t
    if r.returncode != 0:
        sys.exit("baton-parse failed: " + r.stderr[-800:])
    rust = {}
    for line in r.stdout.split("\n"):
        if not line:
            continue
        d = json.loads(line)
        rust[d["path"]] = d["result"]

    mism = []
    for kind, p in files:
        x, y = ref.get(str(p)), rust.get(str(p))
        # Compare through JSON so both sides use the same value model.
        if json.loads(json.dumps(x)) != y:
            fields = sorted(k for k in set((x or {}).keys()) | set((y or {}).keys()) if (x or {}).get(k) != (y or {}).get(k))
            mism.append((kind, p, fields if x and y else ["whole result: py=%s rust=%s" % (x is None, y is None)]))
    print(f"full parse parity: {len(files) - len(mism)}/{len(files)} identical   "
          f"python {t_py:.1f}s  rust {t_rs:.1f}s  ({t_py / max(t_rs, 1e-9):.1f}x)")
    for kind, p, fields in mism[:15]:
        print(f"  MISMATCH {kind} {p.name}: {fields[:6]}")

    ok = not mism
    if a.cuts:
        biggest = sorted(files, key=lambda kp: -kp[1].stat().st_size)[:a.cut_files]
        stdin = "".join(f"{k}\t{p}\n" for k, p in biggest)
        r = subprocess.run([a.bin, "--cuts", str(a.cuts)], input=stdin, capture_output=True, text=True)
        good = tot = 0
        for line in r.stdout.split("\n"):
            if not line:
                continue
            d = json.loads(line)
            g, n = d["incr"]
            good += g
            tot += n
        print(f"incremental == full: {good}/{tot} cut points ({len(biggest)} largest files)")
        ok = ok and good == tot
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
