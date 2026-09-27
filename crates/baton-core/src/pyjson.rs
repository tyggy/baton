//! `json.dumps(value)` with Python's defaults (`ensure_ascii=True`,
//! `", "` / `": "` separators, `repr` floats), so payloads are
//! byte-identical to the Python server's — same bytes, same ETag.
//! Object keys are written in insertion order (serde_json `preserve_order`).

use serde_json::Value;
use std::fmt::Write;

pub fn dumps(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

/// md5 hex digest — Python's `hashlib.md5(b).hexdigest()` (ETags).
pub fn md5_hex(b: &[u8]) -> String {
    use md5::{Digest, Md5};
    let d = Md5::digest(b);
    let mut s = String::with_capacity(32);
    for x in d {
        let _ = write!(s, "{x:02x}");
    }
    s
}

pub fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else {
                float_repr(out, n.as_f64().unwrap_or(0.0));
            }
        }
        Value::String(s) => write_str(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, x);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(out, k);
                out.push_str(": ");
                write_value(out, x);
            }
            out.push('}');
        }
    }
}

/// `json.encoder.py_encode_basestring_ascii`.
pub fn write_str(out: &mut String, s: &str) {
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
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// `float.__repr__`: shortest round-trip digits; fixed notation when the
/// decimal exponent is in (-4, 16], scientific (`1e-05`, `1.5e+16`) otherwise.
pub fn float_repr(out: &mut String, x: f64) {
    if x.is_nan() {
        out.push_str("NaN");
        return;
    }
    if x.is_infinite() {
        out.push_str(if x > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    let sci = format!("{:e}", x.abs()); // e.g. "1.2345e8", "0e0"
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1;
    if x.is_sign_negative() {
        out.push('-');
    }
    if -4 < decpt && decpt <= 16 {
        let n = digits.len() as i32;
        if decpt <= 0 {
            out.push_str("0.");
            for _ in 0..(-decpt) {
                out.push('0');
            }
            out.push_str(&digits);
        } else if decpt >= n {
            out.push_str(&digits);
            for _ in 0..(decpt - n) {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt as usize]);
            out.push('.');
            out.push_str(&digits[decpt as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        let _ = write!(out, "e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(x: f64) -> String {
        let mut s = String::new();
        float_repr(&mut s, x);
        s
    }

    #[test]
    fn floats_like_python() {
        assert_eq!(f(1e16), "1e+16");
        assert_eq!(f(1e15), "1000000000000000.0");
        assert_eq!(f(0.0001), "0.0001");
        assert_eq!(f(0.00001), "1e-05");
        assert_eq!(f(1.5e300), "1.5e+300");
        assert_eq!(f(123456789.123), "123456789.123");
        assert_eq!(f(3.0), "3.0");
        assert_eq!(f(0.0), "0.0");
        assert_eq!(f(-0.0), "-0.0");
        assert_eq!(f(23.71), "23.71");
        assert_eq!(f(-2.5e-7), "-2.5e-07");
    }

    #[test]
    fn dumps_like_python() {
        let v = json!({"b": 1, "a": [true, null, 0.5, "é\u{1F600}\"\\\n\u{7f}"], "c": {}});
        assert_eq!(
            dumps(&v),
            "{\"b\": 1, \"a\": [true, null, 0.5, \"\\u00e9\\ud83d\\ude00\\\"\\\\\\n\\u007f\"], \"c\": {}}"
        );
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }
}
