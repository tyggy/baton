#!/usr/bin/env python3
"""Parity: `baton sessions` vs sessions_core.load_all_sessions(None) — same
sessions, same order, identical fields (the Python parse cache is bypassed
so both parse from the transcripts)."""
import json, os, subprocess, sys, time
from pathlib import Path

ref = os.environ.get("BATON_REF", str(Path.home() / "GitHub/workbench-exporter-master/claude-session-manager"))
bin_ = os.environ.get("BATON_BIN", str(Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton"))
sys.path.insert(0, ref)
import sessions_cache
sessions_cache.get_cached = lambda p: None
sessions_cache.store = lambda p, r: None
import sessions_core as sc

t = time.perf_counter(); py = sc.load_all_sessions(None); t_py = time.perf_counter() - t
py_headless = sc._headless_cache.get("__all__", [])
t = time.perf_counter(); r = subprocess.run([bin_, "sessions"], capture_output=True, text=True); t_rs = time.perf_counter() - t
rs = json.loads(r.stdout)
norm = lambda xs: json.loads(json.dumps(xs))
a, b = norm(py), rs["sessions"]
print(f"python {len(a)} sessions in {t_py:.1f}s | rust {len(b)} in {t_rs:.1f}s ({r.stderr.strip()})")
ok = True
if [s["id"] for s in a] != [s["id"] for s in b]:
    ok = False
    ai, bi = [s["id"] for s in a], [s["id"] for s in b]
    print("ORDER/SET differs: only python", len(set(ai) - set(bi)), "only rust", len(set(bi) - set(ai)))
    for i, (x, y) in enumerate(zip(ai, bi)):
        if x != y:
            print(f"  first difference at {i}: {x} vs {y}"); break
bad = [(x["id"], sorted(k for k in set(x) | set(y) if x.get(k) != y.get(k))) for x, y in zip(a, b) if x != y]
print(f"identical rows: {len(a) - len(bad)}/{len(a)}")
for sid, ks in bad[:10]:
    print("  MISMATCH", sid[:8], ks[:8])
hp, hr = norm(py_headless), rs["headless"]
print(f"headless: python {len(hp)} rust {len(hr)} identical={hp == hr}")
sys.exit(0 if ok and not bad and hp == hr else 1)
