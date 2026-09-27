#!/usr/bin/env python3
"""Bench pane app for bench/parity_tmux.py (runs inside a throwaway
`csm-bench-tmux-*` tmux session, never anywhere else).

Modes:
  winch LOG      redraw on every SIGWINCH and append "<cols>x<rows>" to LOG
  input LOG      raw-mode, SGR-mouse viewer over lines 1..3000: wheel events
                 scroll the view (3 lines per notch); every input byte is
                 appended (hex) to LOG so send / send-key effects compare
"""
import os
import re
import signal
import sys
import termios
import tty

mode, log = sys.argv[1], sys.argv[2]


def size():
    s = os.get_terminal_size()
    return s.columns, s.lines


def out(s):
    os.write(1, s.encode())


def winch_main():
    def draw(*_):
        c, r = size()
        with open(log, "a") as f:
            f.write("%dx%d\n" % (c, r))
        rows = ["WINCH %dx%d" % (c, r)]
        rows += ["row %d ●✓ \x1b[38;5;%dmcolor\x1b[0m" % (i, i % 256) for i in range(2, r)]
        rows += ["bottom %dx%d" % (c, r)]
        out("\x1b[H\x1b[2J" + "\r\n".join(rows))

    signal.signal(signal.SIGWINCH, draw)
    draw()
    while True:
        signal.pause()


def input_main():
    lines = ["%d \x1b[38;5;%dmline\x1b[0m %s" % (i, i % 256, "●" if i % 7 == 0 else "") for i in range(1, 3001)]
    state = {"top": None, "status": ""}

    def draw(*_):
        c, r = size()
        body = r - 1
        if state["top"] is None:
            state["top"] = len(lines) - body
        state["top"] = max(0, min(state["top"], len(lines) - body))
        view = lines[state["top"]:state["top"] + body]
        out("\x1b[H\x1b[2J" + "\r\n".join(view) + "\r\n" + ("top=%d %s" % (state["top"], state["status"]))[:c])

    fd = sys.stdin.fileno()
    tty.setraw(fd)
    out("\x1b[?1000h\x1b[?1006h")
    signal.signal(signal.SIGWINCH, draw)
    draw()
    wheel = re.compile(rb"\x1b\[<(\d+);(\d+);(\d+)M")
    while True:
        try:
            data = os.read(fd, 4096)
        except InterruptedError:
            continue
        with open(log, "a") as f:
            f.write(data.hex() + "\n")
        moved = False
        for m in wheel.finditer(data):
            btn = int(m.group(1))
            if btn in (64, 65):
                state["top"] += -3 if btn == 64 else 3
                state["status"] = "wheel %s@%s;%s" % (btn, m.group(2).decode(), m.group(3).decode())
                moved = True
        if not moved:
            state["status"] = "in %s" % data.hex()[:40]
        draw()


if __name__ == "__main__":
    termios  # imported for tty.setraw
    {"winch": winch_main, "input": input_main}[mode]()
