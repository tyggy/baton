//! Python-compatible JSON values for byte-parity with the reference server.
//!
//! `serde_json::Value` sorts object keys and writes compact UTF-8; Python's
//! `json.dumps` keeps insertion order, escapes non-ASCII (`ensure_ascii`)
//! and, by default, separates with `", "` / `": "`. Responses, log lines
//! and files the Python server writes (devices.json, lineage, calibration)
//! are produced through [`Py`] so the daemon's bytes match.
#![allow(dead_code)] // part of the API other groups can call

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::fmt;

/// A JSON value with Python semantics: dicts keep insertion order, ints and
/// floats stay distinct (`1` vs `1.0`).
#[derive(Clone, Debug, PartialEq)]
pub enum Py {
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<Py>),
    Dict(Vec<(String, Py)>),
}

/// Output style: Python's defaults, `separators=(",", ":")`, or `indent=N`.
#[derive(Clone, Copy)]
pub enum Style {
    Default,
    Compact,
    Indent(usize),
}

impl Py {
    pub fn str(s: impl Into<String>) -> Py {
        Py::Str(s.into())
    }

    pub fn dict() -> Py {
        Py::Dict(Vec::new())
    }

    /// Builder: `Py::dict().with("a", 1)`.
    pub fn with(mut self, k: &str, v: impl Into<Py>) -> Py {
        self.set(k, v.into());
        self
    }

    /// `d[k] = v`: replaces in place (keeping the key's position) or appends.
    pub fn set(&mut self, k: &str, v: Py) {
        if let Py::Dict(items) = self {
            if let Some(slot) = items.iter_mut().find(|(key, _)| key == k) {
                slot.1 = v;
            } else {
                items.push((k.to_string(), v));
            }
        }
    }

    /// `d.get(k)` (None when absent or not a dict).
    pub fn get(&self, k: &str) -> Option<&Py> {
        match self {
            Py::Dict(items) => items.iter().find(|(key, _)| key == k).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Py::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_dict(&self) -> bool {
        matches!(self, Py::Dict(_))
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Py::None => false,
            Py::Bool(b) => *b,
            Py::Int(i) => *i != 0,
            Py::Float(f) => *f != 0.0,
            Py::Str(s) => !s.is_empty(),
            Py::List(l) => !l.is_empty(),
            Py::Dict(d) => !d.is_empty(),
        }
    }

    /// Python type name (for error messages mirroring Python's).
    pub fn type_name(&self) -> &'static str {
        match self {
            Py::None => "NoneType",
            Py::Bool(_) => "bool",
            Py::Int(_) => "int",
            Py::Float(_) => "float",
            Py::Str(_) => "str",
            Py::List(_) => "list",
            Py::Dict(_) => "dict",
        }
    }

    /// `json.dumps(self, …)`.
    pub fn dumps(&self, style: Style) -> String {
        let mut out = String::new();
        write_value(&mut out, self, style, 0);
        out
    }

    /// `json.loads` of a request body / file. Errors carry serde's message.
    pub fn loads(bytes: &[u8]) -> Result<Py, String> {
        serde_json::from_slice::<Py>(bytes).map_err(|e| e.to_string())
    }

    /// `str(v)`: strings as-is, everything else as Python's repr.
    pub fn py_str(&self) -> String {
        match self {
            Py::Str(s) => s.clone(),
            other => other.repr(),
        }
    }

    /// Python `repr(v)` for JSON-shaped values.
    pub fn repr(&self) -> String {
        match self {
            Py::None => "None".into(),
            Py::Bool(true) => "True".into(),
            Py::Bool(false) => "False".into(),
            Py::Int(i) => i.to_string(),
            Py::Float(f) => float_repr(*f),
            Py::Str(s) => str_repr(s),
            Py::List(l) => format!(
                "[{}]",
                l.iter().map(Py::repr).collect::<Vec<_>>().join(", ")
            ),
            Py::Dict(d) => format!(
                "{{{}}}",
                d.iter()
                    .map(|(k, v)| format!("{}: {}", str_repr(k), v.repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Lossy conversion for `State::events` (a `serde_json::Value` channel).
    pub fn to_value(&self) -> Value {
        match self {
            Py::None => Value::Null,
            Py::Bool(b) => Value::Bool(*b),
            Py::Int(i) => i64::try_from(*i)
                .map(Value::from)
                .or_else(|_| u64::try_from(*i).map(Value::from))
                .unwrap_or_else(|_| Value::from(*i as f64)),
            Py::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Py::Str(s) => Value::String(s.clone()),
            Py::List(l) => Value::Array(l.iter().map(Py::to_value).collect()),
            Py::Dict(d) => {
                Value::Object(d.iter().map(|(k, v)| (k.clone(), v.to_value())).collect())
            }
        }
    }
}

impl From<&str> for Py {
    fn from(s: &str) -> Py {
        Py::Str(s.to_string())
    }
}
impl From<String> for Py {
    fn from(s: String) -> Py {
        Py::Str(s)
    }
}
impl From<bool> for Py {
    fn from(b: bool) -> Py {
        Py::Bool(b)
    }
}
impl From<i64> for Py {
    fn from(i: i64) -> Py {
        Py::Int(i as i128)
    }
}
impl From<u64> for Py {
    fn from(i: u64) -> Py {
        Py::Int(i as i128)
    }
}
impl From<usize> for Py {
    fn from(i: usize) -> Py {
        Py::Int(i as i128)
    }
}
impl From<i32> for Py {
    fn from(i: i32) -> Py {
        Py::Int(i as i128)
    }
}
impl From<f64> for Py {
    fn from(f: f64) -> Py {
        Py::Float(f)
    }
}
impl<T: Into<Py>> From<Option<T>> for Py {
    fn from(o: Option<T>) -> Py {
        o.map(Into::into).unwrap_or(Py::None)
    }
}
impl From<Vec<Py>> for Py {
    fn from(v: Vec<Py>) -> Py {
        Py::List(v)
    }
}

// ── json.dumps ─────────────────────────────────────────────────────────

fn write_value(out: &mut String, v: &Py, style: Style, depth: usize) {
    match v {
        Py::None => out.push_str("null"),
        Py::Bool(true) => out.push_str("true"),
        Py::Bool(false) => out.push_str("false"),
        Py::Int(i) => out.push_str(&i.to_string()),
        Py::Float(f) => out.push_str(&float_json(*f)),
        Py::Str(s) => write_str(out, s),
        Py::List(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                sep(out, style, depth + 1, i == 0);
                write_value(out, item, style, depth + 1);
            }
            close(out, style, depth);
            out.push(']');
        }
        Py::Dict(items) => {
            if items.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in items.iter().enumerate() {
                sep(out, style, depth + 1, i == 0);
                write_str(out, k);
                out.push_str(match style {
                    Style::Compact => ":",
                    _ => ": ",
                });
                write_value(out, item, style, depth + 1);
            }
            close(out, style, depth);
            out.push('}');
        }
    }
}

fn sep(out: &mut String, style: Style, depth: usize, first: bool) {
    match style {
        Style::Default => {
            if !first {
                out.push_str(", ");
            }
        }
        Style::Compact => {
            if !first {
                out.push(',');
            }
        }
        Style::Indent(n) => {
            if !first {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(n * depth));
        }
    }
}

fn close(out: &mut String, style: Style, depth: usize) {
    if let Style::Indent(n) = style {
        out.push('\n');
        out.push_str(&" ".repeat(n * depth));
    }
}

/// `ensure_ascii=True` string encoding.
fn write_str(out: &mut String, s: &str) {
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
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
        }
    }
    out.push('"');
}

fn float_json(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        float_repr(f)
    }
}

/// Python `repr(float)`: shortest round-trip digits; fixed notation for
/// decimal exponents in [-4, 16), scientific (`1e+16`, `1.5e-05`) otherwise.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let sci = format!("{:e}", f); // e.g. "-1.790549973380621e9"
    let (mant, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("exponent");
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if neg { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let n = digits.len() as i32;
        let body = if exp < 0 {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
        } else if exp + 1 >= n {
            format!("{}{}.0", digits, "0".repeat((exp + 1 - n) as usize))
        } else {
            let (a, b) = digits.split_at((exp + 1) as usize);
            format!("{a}.{b}")
        };
        format!("{sign}{body}")
    } else {
        let m = if digits.len() > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            digits
        };
        let es = if exp < 0 { "-" } else { "+" };
        format!("{sign}{m}e{es}{:02}", exp.abs())
    }
}

/// Python `repr(str)`.
pub fn str_repr(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// `s[:n]` by code point.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ── json.loads (order-preserving) ──────────────────────────────────────

impl<'de> Deserialize<'de> for Py {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Py, D::Error> {
        d.deserialize_any(PyVisitor)
    }
}

struct PyVisitor;

impl<'de> Visitor<'de> for PyVisitor {
    type Value = Py;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Py, E> {
        Ok(Py::None)
    }
    fn visit_none<E: de::Error>(self) -> Result<Py, E> {
        Ok(Py::None)
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Py, E> {
        Ok(Py::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Py, E> {
        Ok(Py::Int(v as i128))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Py, E> {
        Ok(Py::Int(v as i128))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Py, E> {
        Ok(Py::Float(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Py, E> {
        Ok(Py::Str(v.to_string()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Py, E> {
        Ok(Py::Str(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Py, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element::<Py>()? {
            out.push(v);
        }
        Ok(Py::List(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Py, A::Error> {
        let mut out = Py::dict();
        while let Some((k, v)) = map.next_entry::<String, Py>()? {
            out.set(&k, v); // duplicate keys: last value, first position
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_like_python() {
        for (f, s) in [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1790549973.380621, "1790549973.380621"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345.6, "123456789012345.6"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5e-7, "-2.5e-07"),
            (5242.88, "5242.88"),
            (100.0, "100.0"),
        ] {
            assert_eq!(float_repr(f), s, "{f}");
        }
    }

    #[test]
    fn dumps_like_python() {
        let v =
            Py::loads("{\"b\": 1, \"a\": [1.0, \"é\\u0001\", null, true], \"c\": {}}".as_bytes())
                .unwrap();
        assert_eq!(
            v.dumps(Style::Default),
            r#"{"b": 1, "a": [1.0, "\u00e9\u0001", null, true], "c": {}}"#
        );
        assert_eq!(
            v.dumps(Style::Compact),
            r#"{"b":1,"a":[1.0,"\u00e9\u0001",null,true],"c":{}}"#
        );
        assert_eq!(
            Py::dict()
                .with("x", Py::dict().with("y", 2))
                .dumps(Style::Indent(2)),
            "{\n  \"x\": {\n    \"y\": 2\n  }\n}"
        );
        assert_eq!(Py::str("😀").dumps(Style::Default), r#""\ud83d\ude00""#);
    }

    #[test]
    fn repr_like_python() {
        assert_eq!(Py::None.py_str(), "None");
        assert_eq!(Py::str("it's").repr(), "\"it's\"");
        assert_eq!(
            Py::loads(br#"{"a": [1, true, "x"]}"#).unwrap().repr(),
            "{'a': [1, True, 'x']}"
        );
    }

    #[test]
    fn duplicate_keys_keep_first_position() {
        let v = Py::loads(br#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(v.dumps(Style::Compact), r#"{"a":3,"b":2}"#);
    }
}
