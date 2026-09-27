//! Pure text helpers for the tmux group — each mirrors a Python function in
//! `api_server.py` byte for byte (frame transforms, status detection,
//! `int()` parsing, `json.dumps` formatting, the AppleScript sources).

use baton_core::pyfmt::{is_space, strip};

/// `_is_safe_tmux_name`: `^[A-Za-z0-9._-]+$`, at most 200 code points.
/// (Python's `$` also matches before one trailing `\n`.)
pub fn is_safe_tmux_name(name: &str) -> bool {
    if name.is_empty() || name.chars().count() > 200 {
        return false;
    }
    let body = name.strip_suffix('\n').unwrap_or(name);
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Python `int(s)` for a string: surrounding whitespace, optional sign,
/// ASCII digits with single `_` between digits. `None` = ValueError.
pub fn py_int_str(s: &str) -> Option<i128> {
    let t = strip(s);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    let mut v: i128 = 0;
    let mut prev_us = false;
    for b in digits.bytes() {
        match b {
            b'_' if !prev_us => prev_us = true,
            b'0'..=b'9' => {
                prev_us = false;
                v = v.saturating_mul(10).saturating_add((b - b'0') as i128);
            }
            _ => return None,
        }
    }
    Some(if neg { -v } else { v })
}

/// Universal-newline decoding of subprocess output (`text=True`).
pub fn decode_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s.into_owned()
    }
}

// ── Frame transforms (`_stream_pane`) ─────────────────────────────────────

const PALETTE_BASE: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (170, 0, 0),
    (0, 170, 0),
    (170, 85, 0),
    (0, 0, 170),
    (170, 0, 170),
    (0, 170, 170),
    (170, 170, 170),
    (85, 85, 85),
    (255, 85, 85),
    (85, 255, 85),
    (255, 255, 85),
    (85, 85, 255),
    (255, 85, 255),
    (85, 255, 255),
    (255, 255, 255),
];

/// `_XTERM_256_PALETTE[n]`.
pub fn xterm_rgb(n: usize) -> (u8, u8, u8) {
    const L: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match n {
        0..=15 => PALETTE_BASE[n],
        16..=231 => {
            let i = n - 16;
            (L[i / 36], L[(i / 6) % 6], L[i % 6])
        }
        _ => {
            let g = (8 + (n - 232) * 10) as u8;
            (g, g, g)
        }
    }
}

/// `_convert_256_to_truecolor`: `\e[(38|48);5;(\d+)m` → `\e[K;2;R;G;Bm`.
pub fn convert_256_to_truecolor(s: &str) -> String {
    use std::fmt::Write;
    let b = s.as_bytes();
    if !s.contains(";5;") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    let mut last = 0; // start of the not-yet-copied run
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b
            && b.len() >= i + 7
            && b[i + 1] == b'['
            && (b[i + 2] == b'3' || b[i + 2] == b'4')
            && &b[i + 3..i + 7] == b"8;5;"
        {
            let mut j = i + 7;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 7 && j < b.len() && b[j] == b'm' {
                let n = s[i + 7..j].parse::<u64>().unwrap_or(u64::MAX);
                if n < 256 {
                    out.push_str(&s[last..i]);
                    let (r, g, bl) = xterm_rgb(n as usize);
                    let kind = if b[i + 2] == b'3' { "38" } else { "48" };
                    let _ = write!(out, "\x1b[{kind};2;{r};{g};{bl}m");
                    last = j + 1;
                }
                // Match consumed (replaced or kept verbatim) — no overlap.
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    out
}

/// `_FORCE_TEXT_VARIANT` keys: each gets U+FE0E appended.
fn needs_vs15(c: char) -> bool {
    matches!(
        c,
        '\u{25cf}'
            | '\u{25ef}'
            | '\u{25c9}'
            | '\u{25ce}'
            | '\u{26ab}'
            | '\u{26aa}'
            | '\u{23fa}'
            | '\u{2713}'
            | '\u{2717}'
            | '\u{2733}'
            | '\u{231b}'
            | '\u{23f1}'
            | '\u{23f3}'
    )
}

/// `_force_text_glyphs`.
pub fn force_text_glyphs(s: &str) -> String {
    if !s.chars().any(needs_vs15) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 64);
    for c in s.chars() {
        out.push(c);
        if needs_vs15(c) {
            out.push('\u{fe0e}');
        }
    }
    out
}

/// Length of an `_ANSI_STRIP` match at `b[i]` (an ESC), if any:
/// `\e\[[\d;]*[mGKHJ]` | `\e\][^\x07]*\x07` | `\e\\`.
fn ansi_match(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i + 1)? {
        b'[' => {
            let mut j = i + 2;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            matches!(b.get(j)?, b'm' | b'G' | b'K' | b'H' | b'J').then_some(j + 1 - i)
        }
        b']' => {
            let rel = b[i + 2..].iter().position(|&c| c == 0x07)?;
            Some(rel + 3)
        }
        b'\\' => Some(2),
        _ => None,
    }
}

/// `_ANSI_STRIP.sub("", line).strip() == ""`.
fn visually_blank(line: &str) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            if let Some(n) = ansi_match(b, i) {
                i += n;
                continue;
            }
            return false;
        }
        let c = line[i..].chars().next().expect("char boundary");
        if !is_space(c) {
            return false;
        }
        i += c.len_utf8();
    }
    true
}

/// `_pin_content_to_bottom`: trailing blank rows move to the head.
pub fn pin_content_to_bottom(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let trailing = lines.iter().rev().take_while(|l| visually_blank(l)).count();
    if trailing == 0 || trailing >= lines.len() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + trailing);
    for _ in 0..trailing {
        out.push('\n');
    }
    out.push_str(&lines[..lines.len() - trailing].join("\n"));
    out
}

const ROW_RESET: &str = "\x1b[39m\x1b[49m";
const HIDE_CARET: &str = "\x1b[?25l";

/// The whole wire transform of one captured pane: 256→truecolor, VS15,
/// pin to bottom, per-row fg/bg reset, hidden caret, SSE `data:` framing.
pub fn frame_event(captured: &str) -> String {
    let s = convert_256_to_truecolor(captured);
    let s = force_text_glyphs(&s);
    let s = pin_content_to_bottom(&s);
    let n = s.split('\n').count();
    let mut out = String::with_capacity(s.len() + n * (6 + ROW_RESET.len() + 1) + 16);
    for (k, line) in s.split('\n').enumerate() {
        out.push_str("data: ");
        out.push_str(line);
        if k + 1 < n {
            out.push_str(ROW_RESET);
            out.push('\n');
        }
    }
    out.push_str(HIDE_CARET);
    out.push_str("\n\n");
    out
}

// ── /api/sessions helpers ─────────────────────────────────────────────────

/// `_last_lines(text, n)`.
pub fn last_lines(text: &str, n: usize) -> String {
    let mut lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    while lines.last().is_some_and(|l| strip(l).is_empty()) {
        lines.pop();
    }
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    if !lines.is_empty() {
        out.push('\n');
    }
    out
}

/// `_pane_status(content)`.
pub fn pane_status(content: &str) -> &'static str {
    let lower = content.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| lower.contains(w));
    if has(&["allow", "deny", "y/n", "permission", "approve"]) {
        return "waiting";
    }
    if has(&["error", "failed", "fatal", "panic:"]) {
        return "error";
    }
    if strip(content).ends_with(['$', '>', '%', '~']) {
        return "idle";
    }
    "active"
}

/// `s[-n:]` by code point.
pub fn tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let (i, _) = s.char_indices().nth(count - n).expect("in range");
    &s[i..]
}

// ── json.dumps (default separators, ensure_ascii) ─────────────────────────

/// Insertion-ordered JSON value serialized exactly like Python's
/// `json.dumps(obj)`.
pub enum J {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

pub fn dumps(v: &J) -> String {
    let mut out = String::new();
    write_j(&mut out, v, (", ", ": "));
    out
}

/// `json.dumps(obj, separators=(",", ":"))` — the log-line format.
pub fn dumps_compact(v: &J) -> String {
    let mut out = String::new();
    write_j(&mut out, v, (",", ":"));
    out
}

fn write_j(out: &mut String, v: &J, sep: (&str, &str)) {
    match v {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Int(n) => out.push_str(&n.to_string()),
        J::Str(s) => write_str(out, s),
        J::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep.0);
                }
                write_j(out, x, sep);
            }
            out.push(']');
        }
        J::Obj(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep.0);
                }
                write_str(out, k);
                out.push_str(sep.1);
                write_j(out, x, sep);
            }
            out.push('}');
        }
    }
}

pub fn write_str(out: &mut String, s: &str) {
    use std::fmt::Write;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", u);
                }
            }
        }
    }
    out.push('"');
}

/// Python `str(v)` for a JSON-decoded value (send-key arguments).
pub fn py_str(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).into(),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) => py_float_repr(f),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

fn py_float_repr(f: f64) -> String {
    if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

// ── AppleScript sources ───────────────────────────────────────────────────

/// `_resize_terminal_app_window` script.
pub fn resize_terminal_script(client_tty: &str, cols: i64, rows: i64) -> String {
    format!(
        "tell application \"Terminal\"\n    set wc to count of windows\n    repeat with i from 1 to wc\n        try\n            if tty of selected tab of window i is \"{client_tty}\" then\n                set number of columns of selected tab of window i to {cols}\n                set number of rows of selected tab of window i to {rows}\n                return \"ok\"\n            end if\n        end try\n    end repeat\n    return \"no_match\"\nend tell"
    )
}

/// `_focus_terminal_tab` script.
pub fn focus_tab_script(tty: &str) -> String {
    format!(
        "tell application \"Terminal\"\n  repeat with w in windows\n    repeat with t in tabs of w\n      try\n        if (tty of t) is \"{tty}\" then\n          set selected of t to true\n          set index of w to 1\n          return\n        end if\n      end try\n    end repeat\n  end repeat\nend tell"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert!(is_safe_tmux_name("claude-1d693e35"));
        assert!(is_safe_tmux_name("a.b_c-d"));
        assert!(is_safe_tmux_name("abc\n"));
        assert!(!is_safe_tmux_name(""));
        assert!(!is_safe_tmux_name("\n"));
        assert!(!is_safe_tmux_name("a b"));
        assert!(!is_safe_tmux_name("a;b"));
        assert!(!is_safe_tmux_name("é"));
        assert!(!is_safe_tmux_name(&"a".repeat(201)));
        assert!(is_safe_tmux_name(&"a".repeat(200)));
    }

    #[test]
    fn ints() {
        assert_eq!(py_int_str(" 80 "), Some(80));
        assert_eq!(py_int_str("+8_0"), Some(80));
        assert_eq!(py_int_str("-3"), Some(-3));
        assert_eq!(py_int_str("8__0"), None);
        assert_eq!(py_int_str("_8"), None);
        assert_eq!(py_int_str("8.0"), None);
        assert_eq!(py_int_str(""), None);
    }

    #[test]
    fn palette() {
        assert_eq!(xterm_rgb(16), (0, 0, 0));
        assert_eq!(xterm_rgb(78), (95, 215, 135));
        assert_eq!(xterm_rgb(231), (255, 255, 255));
        assert_eq!(xterm_rgb(232), (8, 8, 8));
        assert_eq!(xterm_rgb(255), (238, 238, 238));
        assert_eq!(xterm_rgb(9), (255, 85, 85));
    }

    #[test]
    fn truecolor() {
        assert_eq!(
            convert_256_to_truecolor("a\x1b[38;5;78mb\x1b[48;5;244mc\x1b[38;5;256md\x1b[38;5;m"),
            "a\x1b[38;2;95;215;135mb\x1b[48;2;128;128;128mc\x1b[38;5;256md\x1b[38;5;m"
        );
        assert_eq!(
            convert_256_to_truecolor("\x1b[1;38;5;9mx"),
            "\x1b[1;38;5;9mx"
        );
        assert_eq!(
            convert_256_to_truecolor("\x1b[38;5;0009m"),
            "\x1b[38;2;255;85;85m"
        );
    }

    #[test]
    fn glyphs() {
        assert_eq!(force_text_glyphs("● ok ✓"), "●\u{fe0e} ok ✓\u{fe0e}");
        assert_eq!(force_text_glyphs("plain"), "plain");
    }

    #[test]
    fn pin() {
        assert_eq!(pin_content_to_bottom("a\nb\n\n \x1b[0m"), "\n\na\nb");
        assert_eq!(pin_content_to_bottom("a\nb"), "a\nb");
        assert_eq!(pin_content_to_bottom("\n\n"), "\n\n");
        assert_eq!(pin_content_to_bottom("a\n\x1b]0;t\x07"), "\na");
        assert_eq!(pin_content_to_bottom("a\n\x1b[?25l"), "a\n\x1b[?25l");
        assert_eq!(pin_content_to_bottom("a\n\u{1f}"), "\na");
    }

    #[test]
    fn frame() {
        assert_eq!(
            frame_event("x\ny\n"),
            "data: \x1b[39m\x1b[49m\ndata: x\x1b[39m\x1b[49m\ndata: y\x1b[?25l\n\n"
        );
    }

    #[test]
    fn lines_and_status() {
        assert_eq!(last_lines("a\nb\n\n  \n", 15), "a\nb\n");
        assert_eq!(last_lines("", 15), "");
        assert_eq!(last_lines("1\n2\n3\n", 2), "2\n3\n");
        assert_eq!(pane_status("Do you want to Allow?"), "waiting");
        assert_eq!(pane_status("Fatal"), "error");
        assert_eq!(pane_status("user@host ~ $ \n"), "idle");
        assert_eq!(pane_status("working"), "active");
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(tail_chars("hi", 3), "hi");
    }

    #[test]
    fn json_dumps() {
        let v = J::Arr(vec![J::Obj(vec![
            ("index", J::Int(1)),
            ("ok", J::Bool(true)),
            ("s", J::Str("a\"\\\n\u{1}\u{7f}✻😀".into())),
        ])]);
        assert_eq!(
            dumps(&v),
            r#"[{"index": 1, "ok": true, "s": "a\"\\\n\u0001\u007f\u273b\ud83d\ude00"}]"#
        );
    }

    #[test]
    fn scripts() {
        // Exact texts produced by the Python functions (captured, not run).
        assert_eq!(
            resize_terminal_script("/dev/ttys003", 80, 24),
            "tell application \"Terminal\"\n    set wc to count of windows\n    repeat with i from 1 to wc\n        try\n            if tty of selected tab of window i is \"/dev/ttys003\" then\n                set number of columns of selected tab of window i to 80\n                set number of rows of selected tab of window i to 24\n                return \"ok\"\n            end if\n        end try\n    end repeat\n    return \"no_match\"\nend tell"
        );
        assert_eq!(
            focus_tab_script("/dev/ttys9"),
            "tell application \"Terminal\"\n  repeat with w in windows\n    repeat with t in tabs of w\n      try\n        if (tty of t) is \"/dev/ttys9\" then\n          set selected of t to true\n          set index of w to 1\n          return\n        end if\n      end try\n    end repeat\n  end repeat\nend tell"
        );
    }
}
