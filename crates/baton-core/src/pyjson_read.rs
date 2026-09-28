//! Python's `json.dumps` and `str()`/`repr()` of JSON-shaped values, so
//! responses and previews come out byte-identical to the reference server.
//! Objects keep insertion order (serde_json `preserve_order`), like dicts.

use serde_json::{Number, Value};
use std::fmt::Write;

/// `json.dumps(v)` (`ensure_ascii=True`, separators `", "`/`": "`).
pub fn dumps(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, true, ", ", ": ");
    out
}

/// `json.dumps(v, ensure_ascii=False)`.
pub fn dumps_unicode(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, false, ", ", ": ");
    out
}

/// `json.dumps(v, ensure_ascii=False, separators=(",", ":"))`.
pub fn dumps_unicode_compact(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, false, ",", ":");
    out
}

fn write_value(out: &mut String, v: &Value, ascii: bool, item_sep: &str, key_sep: &str) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&number_repr(n)),
        Value::String(s) => write_str(out, s, ascii),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_value(out, x, ascii, item_sep, key_sep);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_str(out, k, ascii);
                out.push_str(key_sep);
                write_value(out, x, ascii, item_sep, key_sep);
            }
            out.push('}');
        }
    }
}

/// A JSON string literal as `json.dumps` writes it.
pub fn write_str(out: &mut String, s: &str, ascii: bool) {
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
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ascii && !(' '..='~').contains(&c) => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{:04x}", u);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `repr(int)` / `repr(float)` for a JSON number.
pub fn number_repr(n: &Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    float_repr(n.as_f64().unwrap_or(0.0))
}

/// `repr(float)`: shortest round-trip digits; fixed notation when the
/// decimal exponent is in [-4, 16), else `d.ddde±XX`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sci = format!("{f:e}"); // e.g. "-1.5e-5", "1e16", "0e0"
    let (mant, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("exponent");
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                for _ in digits.len()..int_len {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    out
}

/// `str(v)` for a value parsed by `json.loads`: strings as-is, everything
/// else as its Python repr (`True`, `None`, `{'a': 1}`, …).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

pub fn py_repr(v: &Value) -> String {
    let mut out = String::new();
    write_repr(&mut out, v);
    out
}

fn write_repr(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                out.push_str(&n.to_string());
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.is_nan() {
                    out.push_str("nan");
                } else if f.is_infinite() {
                    out.push_str(if f > 0.0 { "inf" } else { "-inf" });
                } else {
                    out.push_str(&float_repr(f));
                }
            }
        }
        Value::String(s) => out.push_str(&str_repr(s)),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr(out, x);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&str_repr(k));
                out.push_str(": ");
                write_repr(out, x);
            }
            out.push('}');
        }
    }
}

/// `repr(str)`.
pub fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if (c as u32) < 0x7f => out.push(c),
            c if is_printable(c) => out.push(c),
            c => {
                let u = c as u32;
                let _ = if u < 0x100 {
                    write!(out, "\\x{u:02x}")
                } else if u < 0x10000 {
                    write!(out, "\\u{u:04x}")
                } else {
                    write!(out, "\\U{u:08x}")
                };
            }
        }
    }
    out.push(quote);
    out
}

/// `str.isprintable()` for one non-ASCII char: false for control,
/// format, separator (other than space), surrogate and private-use code
/// points. (Unassigned code points are treated as printable.)
fn is_printable(c: char) -> bool {
    let u = c as u32;
    !matches!(u,
        0x80..=0xa0
        | 0xad
        | 0x600..=0x605
        | 0x61c
        | 0x6dd
        | 0x70f
        | 0x890..=0x891
        | 0x8e2
        | 0x1680
        | 0x180e
        | 0x2000..=0x200f
        | 0x2028..=0x202f
        | 0x205f..=0x2064
        | 0x2066..=0x206f
        | 0x3000
        | 0xd800..=0xf8ff
        | 0xfeff
        | 0xfff9..=0xfffb
        | 0x110bd
        | 0x110cd
        | 0x13430..=0x1343f
        | 0x1bca0..=0x1bca3
        | 0x1d173..=0x1d17a
        | 0xe0001
        | 0xe0020..=0xe007f
        | 0xf0000..=0x10ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_like_python() {
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(100.0), "100.0");
        assert_eq!(float_repr(0.1), "0.1");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1.5e-5), "1.5e-05");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(123456789012345.6), "123456789012345.6");
        assert_eq!(float_repr(-2.5), "-2.5");
        assert_eq!(float_repr(0.0), "0.0");
    }

    #[test]
    fn dumps_like_python() {
        let v = json!({"b": [1, 2.5, null, true], "a": "é\n\u{1F600}"});
        let bs = '\\';
        assert_eq!(
            dumps(&v),
            format!("{{\"b\": [1, 2.5, null, true], \"a\": \"{bs}u00e9{bs}n{bs}ud83d{bs}ude00\"}}")
        );
        assert_eq!(
            dumps_unicode_compact(&v),
            "{\"b\":[1,2.5,null,true],\"a\":\"é\\n\u{1F600}\"}"
        );
        assert_eq!(
            py_repr(&json!({"a": [1, "it's", null]})),
            r#"{'a': [1, "it's", None]}"#
        );
    }
}
