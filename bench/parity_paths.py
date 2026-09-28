#!/usr/bin/env python3
"""Parity: baton-core's attachment path scanners vs the reference server's
`_PATH_RE` / `_REL_PATH_RE` (api_server.py), on text from recent
transcripts plus fuzz. Exit 1 on any mismatch.

    python3 bench/parity_paths.py [--files 150] [--fuzz 20000]
"""
import argparse
import json
import os
import random
import re
import subprocess
import sys
from pathlib import Path

HOME = Path.home()
EXT = r"(md|markdown|txt|png|jpg|jpeg|gif|webp|heic|pdf|csv|json|yaml|yml|toml|html|htm|swift|py|sh|js|ts|tsx|jsx|css)"
PATH_RE = re.compile(r"(?<![A-Za-z0-9_])(~/|/[A-Za-z0-9_\-./]+/|/)[A-Za-z0-9_\-./]+?\." + EXT + r"(?![A-Za-z0-9])")
REL_RE = re.compile(r"(?<![A-Za-z0-9_~./-])((?:[A-Za-z0-9_][A-Za-z0-9_\-.]*/)+[A-Za-z0-9_\-.]+\.(?:"
                    + EXT[1:-1] + r"))(?![A-Za-z0-9])")


def texts_from(path):
    out = []
    for line in open(path, "rb"):
        if b'"type":"user"' not in line and b'"type":"assistant"' not in line:
            continue
        try:
            d = json.loads(line)
        except Exception:
            continue
        c = (d.get("message") or {}).get("content")
        if isinstance(c, str):
            out.append(c)
        elif isinstance(c, list):
            for b in c:
                if not isinstance(b, dict):
                    continue
                for k in ("text", "thinking"):
                    if isinstance(b.get(k), str):
                        out.append(b[k])
                if isinstance(b.get("input"), dict):
                    out += [v for v in b["input"].values() if isinstance(v, str)]
                tc = b.get("content")
                if isinstance(tc, str):
                    out.append(tc)
                elif isinstance(tc, list):
                    out += [x.get("text") for x in tc if isinstance(x, dict) and isinstance(x.get("text"), str)]
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--files", type=int, default=150)
    ap.add_argument("--fuzz", type=int, default=20000)
    ap.add_argument("--bin", default=str(Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton-pymd"))
    a = ap.parse_args()
    files = sorted(HOME.glob(".claude/projects/*/*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True)[:a.files]
    texts = [t for p in files for t in texts_from(p)]
    rnd = random.Random(3)
    atoms = ["/", "~/", "~", ".", "..", "a", "Z9", "_", "-", ".md", ".ts", ".tsx", ".jsonl", ".json", ".py", "x.md.bak",
             " ", "\n", "é", "src/", "../", "./", "(", ")", ",", "'", "\"", ":", "/tmp/", "foo/bar"]
    texts += ["".join(rnd.choice(atoms) for _ in range(rnd.randint(1, 30))) for _ in range(a.fuzz)]
    ref = [[[m.group(0) for m in PATH_RE.finditer(t)], [m.group(1) for m in REL_RE.finditer(t)]] for t in texts]
    r = subprocess.run([a.bin, "--paths"], input="".join(json.dumps(t) + "\n" for t in texts),
                       capture_output=True, text=True)
    if r.returncode:
        sys.exit(r.stderr[-2000:])
    got = [json.loads(l) for l in r.stdout.split("\n") if l]
    bad = [(t, x, y) for t, x, y in zip(texts, ref, got) if x != y]
    print(f"{len(texts)} texts ({sum(len(x[0]) + len(x[1]) for x in ref)} matches); mismatches: {len(bad)}")
    for t, x, y in bad[:5]:
        print(repr(t[:200]), x, y)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
