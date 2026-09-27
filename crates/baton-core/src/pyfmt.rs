//! Python string semantics the reference parser relies on. The transcript
//! parser must produce exactly what `sessions_core.py` produces, so these
//! mirror Python's `str.isspace`, `strip`, `split`, slicing by code point,
//! truthiness and `round(x, 1)`.

use serde_json::Value;

/// `str.isspace()` — Rust's `char::is_whitespace` plus the ASCII
/// information separators (U+001C..U+001F) that Python also counts.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.split()` with no separator.
pub fn split_ws(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|t| !t.is_empty())
}

/// `s[:n]` — by code point.
pub fn head(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// `len(s)` — code points.
pub fn len(s: &str) -> usize {
    s.chars().count()
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `round(x, 1)` for a finite float: correctly rounded to one decimal
/// (Rust's formatter and CPython both round the exact binary value,
/// ties to even).
pub fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// Python's `int()` would have raised on this value.
#[derive(Debug)]
pub struct NotAnInt;

/// `int(v or 0)` as used for token counts: numbers truncate, integer
/// strings parse (surrounding whitespace allowed), falsy → 0. Anything
/// else raised in Python — the whole parse then returned None.
pub fn py_int(v: Option<&Value>) -> Result<i64, NotAnInt> {
    let v = match v {
        None => return Ok(0),
        Some(v) if !truthy(v) => return Ok(0),
        Some(v) => v,
    };
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .ok_or(NotAnInt),
        Value::String(s) => {
            let t = strip(s).replace('_', "");
            t.parse::<i64>().map_err(|_| NotAnInt)
        }
        _ => Err(NotAnInt),
    }
}

/// Python `\w` for the word-boundary checks below.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `re.sub(r"<([A-Za-z][A-Za-z0-9_-]*)\b[^>]*>[\s\S]*?</\1>", " ", s)` —
/// written out as a scan (the regex's back-reference needs a backtracking
/// engine, which hit its limit on very long messages). Same semantics:
/// leftmost match; the tag name is tried longest-first and must end at a
/// word boundary; `[^>]*>` runs to the first `>`; the body is lazy up to
/// the first `</name>`.
fn strip_paired_tags(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        if b[i] == b'<' && i + 1 < b.len() && b[i + 1].is_ascii_alphabetic() {
            let mut end = i + 2;
            while end < b.len()
                && (b[end].is_ascii_alphanumeric() || b[end] == b'_' || b[end] == b'-')
            {
                end += 1;
            }
            let mut matched = None;
            let mut name_end = end;
            while name_end > i + 1 {
                let prev = b[name_end - 1] as char;
                let next = s[name_end..].chars().next();
                let boundary = is_word(prev) != next.is_some_and(is_word);
                if boundary {
                    if let Some(gt) = s[name_end..].find('>') {
                        let body = name_end + gt + 1;
                        let close = format!("</{}>", &s[i + 1..name_end]);
                        if let Some(c) = s[body..].find(&close) {
                            matched = Some(body + c + close.len());
                            break;
                        }
                    }
                }
                name_end -= 1;
            }
            if let Some(m) = matched {
                out.push_str(&s[last..i]);
                out.push(' ');
                i = m;
                last = m;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    out
}

/// `re.sub(r"<[^>]+>", " ", s)`.
fn strip_standalone_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        match rest[lt + 1..].find('>') {
            Some(0) => {
                out.push_str(&rest[..lt + 1]);
                rest = &rest[lt + 1..];
            }
            Some(gt) => {
                out.push_str(&rest[..lt]);
                out.push(' ');
                rest = &rest[lt + 1 + gt + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// `sessions_core.is_meaningful_message`: would this user message make a
/// useful row preview (not only markup, not a bare path, ≥3 chars)?
pub fn is_meaningful_message(content: &str) -> bool {
    if content.is_empty() {
        return false;
    }
    let s = strip_paired_tags(content);
    let s = strip_standalone_tags(&s);
    let s = split_ws(&s).collect::<Vec<_>>().join(" ");
    if len(&s) < 3 {
        return false;
    }
    !(!s.contains(' ') && (s.starts_with('/') || s.starts_with("~/") || s.starts_with("./")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_like_python() {
        assert_eq!(
            strip_paired_tags("a <system-reminder x=1>hi</system-reminder> b"),
            "a   b"
        );
        assert_eq!(strip_paired_tags("<a-b>x</a>"), " "); // shorter name at a '-' boundary
        assert_eq!(strip_paired_tags("<abc>no close"), "<abc>no close");
        assert_eq!(
            strip_standalone_tags("x <br> y <> z <open"),
            "x   y <> z <open"
        );
        assert!(!is_meaningful_message("<command-name>/x</command-name>"));
        assert!(!is_meaningful_message("/Users/me/file.md"));
        assert!(is_meaningful_message("fix the build please"));
    }
}
