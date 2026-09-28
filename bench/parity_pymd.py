#!/usr/bin/env python3
"""Parity: baton-core's Python-Markdown port vs Python-Markdown itself.

    python3 bench/parity_pymd.py [--limit N] [--max-kb 400] [--bin <baton-pymd>]

Renders every cached session markdown (~/Documents/Claude/sessions.nosync,
the same text /api/session previews render) plus synthetic edge cases with
`markdown.markdown(..., extensions=["tables", "fenced_code", "nl2br"],
output_format="html5")` and with the Rust port, and compares the HTML.
Exit code 1 on any mismatch.
"""
import argparse
import json
import os
import random
import subprocess
import sys
import time
from pathlib import Path

import markdown

HOME = Path.home()

SYNTH = [
    "x </y z\n\nmore text", "a &#zz; b &#q; c <b>bold</b>", "[x](href)", "<div>\nraw\n</div>\ntail",
    "a\nb  \nc", "```py\ncode <x>\n```\n", "1. a\n2. b\n\n    code?\n\n- c", "***a** b*", "**a *b***",
    "_a_ __b__ ___c___ snake_case_word", "`a` ``b`c`` ```d", "\\*not em\\* \\| \\q", "<http://x.y/z?a&b>",
    "<me@example.com>", "[ref][1]\n\n[1]: http://x 'T'", "![img](a.png \"t\")", "[a](<b c> 'd')",
    "[a](b (c) \"d\")", "| a | b |\n|---|:-:|\n| 1 | `|` |", "a | b\n--|--\n1 | 2", "# H #\ntext\n## H2",
    "Title\n=====\n\nSub\n---", "- - -\n***\n___", "> q\n> > nested\n\nafter", "    code\n\n    more",
    "* a\n    * b\n        * c\n* d", "1. x\n\n    para\n\n2. y", "<!-- c --> x", "<?php x ?>", "<![CDATA[x]]>",
    "<!DOCTYPE html>", "<script>\nif (a < b) {}\n</script>\nx", "&amp; &copy; &#169; &#xA9; & x",
    "~~~\ntilde\n~~~", "```{.python #id .x}\nc\n```", "```python hl_lines=\"1\"\nc\n```", "a  \n  b",
    "<thinking>\nhi *x*\n</thinking>", "<command-name>/x</command-name>\n<command-args></command-args>",
    "\t tab\tin\ttext", "trailing   ", "line\r\nwin\rmac", "[x](a) [y](b \"c\") [z]", "*a **b** c*",
    "**bold**text**more**", "__init__.py and __main__", "2 * 3 * 4", "a_b_c", "<a href='x'>l</a>",
    "</>", "<a`b>", "&#x1g; &#12a; &#", "text <b\nattr>x</b>", "<div a=b>\n\n*md*\n\n</div>",
    "<hr>\n<hr/>\npara", "x <br/> y", "<p>para</p>\n\nnext", "- [ ] task\n- [x] done",
    "| a |\n| - |", "|a|\n|-|\n|b|", "\\\\|x|\\\\\n|-|-|", "a\n\n\n\nb", "#no space", "####### seven",
    "## Assistant [2026-01-01 10:00:00]\n\ntext\n\n## User []\n\nmore", "* * *", "+ plus\n+ list",
    "1) paren", "10. ten\n11. eleven", "  1. indented", "text\n* not list?", "a <em>b</em> c",
    "<span>x", "</span>", "<!-- unclosed", "a <!-- mid --> b", "<? mid", "x & y && z", "&foo;bar",
]


def fuzz(n, seed=7):
    rnd = random.Random(seed)
    atoms = ["*", "**", "_", "__", "`", "``", "[", "]", "(", ")", "<", ">", "</", "&", "#", "\n", "\n\n",
             "  \n", "    ", "|", "-", "!", "\\", "\"", "'", "a", "b c", "<div>", "</div>", "<b>", "</b>",
             "http://x.y", "&amp;", "&#1;", "1. ", "- ", "> ", "```", "~~~", "---", "===", ":", "é", "😀"]
    return ["".join(rnd.choice(atoms) for _ in range(rnd.randint(1, 40))) for _ in range(n)]


def render_py(src):
    try:
        return markdown.markdown(src, extensions=["tables", "fenced_code", "nl2br"], output_format="html5")
    except Exception:
        return ""


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=str(Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "release/baton-pymd"))
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--max-kb", type=int, default=400)
    ap.add_argument("--fuzz", type=int, default=3000)
    ap.add_argument("--show", type=int, default=3)
    a = ap.parse_args()

    docs = [("synth", s) for s in SYNTH] + [("fuzz", s) for s in fuzz(a.fuzz)]
    files = sorted((HOME / "Documents/Claude/sessions.nosync").glob("*/*.md"),
                   key=lambda p: p.stat().st_mtime, reverse=True)
    files = [p for p in files if p.stat().st_size <= a.max_kb * 1024]
    if a.limit:
        files = files[:a.limit]
    for p in files:
        try:
            raw = p.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        docs.append((str(p), raw))
    print(f"{len(docs)} documents ({len(files)} session files ≤ {a.max_kb} KB)")

    t = time.perf_counter()
    ref = [render_py(s) for _, s in docs]
    t_py = time.perf_counter() - t
    t = time.perf_counter()
    r = subprocess.run([a.bin], input="".join(json.dumps(s) + "\n" for _, s in docs),
                       capture_output=True, text=True)
    t_rs = time.perf_counter() - t
    if r.returncode != 0:
        sys.exit("baton-pymd failed: " + r.stderr[-2000:])
    got = [json.loads(line) for line in r.stdout.split("\n") if line]
    bad = [(name, src, x, y) for (name, src), x, y in zip(docs, ref, got) if x != y]
    print(f"python {t_py:.1f}s, rust {t_rs:.1f}s; mismatches: {len(bad)}")
    for name, src, x, y in bad[:a.show]:
        i = next((k for k in range(min(len(x), len(y))) if x[k] != y[k]), min(len(x), len(y)))
        print(f"--- {name} (first diff at {i})")
        print("src:", repr(src[:300]) if len(src) < 2000 else f"<{len(src)} chars>")
        print("py :", repr(x[max(0, i - 150):i + 150]))
        print("rs :", repr(y[max(0, i - 150):i + 150]))
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
