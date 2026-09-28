//! Shared HTTP helpers for native handlers — same conventions as the
//! Python reference (docs/api.md): bearer auth, JSON with CORS headers,
//! gzip over ~1400 bytes when accepted, ETag/304.

use crate::proxy::Body;
use crate::pyjson::{Py, Style};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{HeaderMap, Request, Response, StatusCode};
use serde_json::Value;
use std::io::Write;

/// Paths reachable without a token (Python: OPEN_PATHS + anything outside /api/).
pub fn is_open(path: &str) -> bool {
    matches!(path, "/api/health" | "/api/entity-logo") || !path.starts_with("/api/")
}

/// `None` = authorized; `Some(401 response)` otherwise. Empty token = dev mode.
pub fn check_auth(headers: &HeaderMap, path: &str, token: &str) -> Option<Response<Body>> {
    if token.is_empty() || is_open(path) {
        return None;
    }
    let h = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(provided) = h.strip_prefix("Bearer ") else {
        return Some(json(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({"error": "missing bearer token"}),
            headers,
        ));
    };
    if !constant_time_eq(provided.trim().as_bytes(), token.as_bytes()) {
        return Some(json(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({"error": "invalid bearer token"}),
            headers,
        ));
    }
    None
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"))
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(5));
    e.write_all(data).expect("in-memory gzip");
    e.finish().expect("in-memory gzip")
}

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

/// JSON response like Python's `send_json` (gzip when accepted and >1400 B).
pub fn json(status: StatusCode, value: &Value, req_headers: &HeaderMap) -> Response<Body> {
    bytes_response(
        status,
        "application/json",
        value.to_string().into_bytes(),
        req_headers,
        None,
    )
}

/// Body with optional gzip and ETag; `etag` given + matching If-None-Match → 304.
pub fn bytes_response(
    status: StatusCode,
    content_type: &str,
    body: Vec<u8>,
    req_headers: &HeaderMap,
    etag: Option<&str>,
) -> Response<Body> {
    let mut b = Response::builder()
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET, POST, OPTIONS")
        .header(
            "access-control-allow-headers",
            "Content-Type, Authorization",
        );
    if let Some(tag) = etag {
        b = b.header("etag", tag);
        if req_headers
            .get("if-none-match")
            .and_then(|v| v.to_str().ok())
            == Some(tag)
        {
            return b
                .status(StatusCode::NOT_MODIFIED)
                .body(full(Bytes::new()))
                .expect("304");
        }
    }
    b = b.status(status).header("content-type", content_type);
    let body = if body.len() >= 1400 && accepts_gzip(req_headers) {
        b = b.header("content-encoding", "gzip");
        gzip(&body)
    } else {
        body
    };
    b.header("content-length", body.len())
        .body(full(body))
        .expect("response")
}

// ── POST bodies ────────────────────────────────────────────────────────
//
// One parser for every JSON POST endpoint (INC-045, INC-041). Python's
// `read_body` handed `json.loads` whatever arrived and every handler then
// called `body.get(...).strip()`, so a malformed body or a wrong field type
// was an uncaught exception and a 500. Here those are 400s with a JSON error.

/// A request the handler can't act on → 400 `{"error": msg}`.
#[derive(Debug, Clone, PartialEq)]
pub struct BadRequest(pub String);

impl BadRequest {
    pub fn new(msg: impl Into<String>) -> Self {
        BadRequest(msg.into())
    }

    pub fn response(&self, req_headers: &HeaderMap) -> Response<Body> {
        bytes_response(
            StatusCode::BAD_REQUEST,
            "application/json",
            Py::dict()
                .with("error", self.0.as_str())
                .dumps(Style::Default)
                .into_bytes(),
            req_headers,
            None,
        )
    }
}

/// `Content-Length`: absent or empty → 0; anything else must be a number.
pub fn content_length(h: &HeaderMap) -> Result<u64, BadRequest> {
    match h.get("content-length").map(|v| v.to_str()) {
        None => Ok(0),
        Some(Ok(s)) if s.trim().is_empty() => Ok(0),
        Some(Ok(s)) => s
            .trim()
            .parse()
            .map_err(|_| BadRequest::new("invalid Content-Length")),
        Some(Err(_)) => Err(BadRequest::new("invalid Content-Length")),
    }
}

/// The whole request body.
pub async fn body_bytes(req: Request<Incoming>) -> Result<Bytes, BadRequest> {
    req.into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|_| BadRequest::new("could not read request body"))
}

/// A POST body as a JSON object. Empty → `{}` (Python's `read_body`);
/// invalid JSON, invalid UTF-8 or any other JSON type → 400.
pub fn parse_object(bytes: &[u8]) -> Result<Py, BadRequest> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Py::dict());
    }
    let v = Py::loads(bytes).map_err(|e| BadRequest::new(format!("invalid JSON body: {e}")))?;
    if !v.is_dict() {
        return Err(BadRequest::new(format!(
            "JSON body must be an object, not {}",
            json_type(&v)
        )));
    }
    Ok(v)
}

/// [`body_bytes`] then [`parse_object`].
pub async fn read_object(req: Request<Incoming>) -> Result<Py, BadRequest> {
    parse_object(&body_bytes(req).await?)
}

fn json_type(v: &Py) -> &'static str {
    match v {
        Py::None => "null",
        Py::Bool(_) => "a boolean",
        Py::Int(_) | Py::Float(_) => "a number",
        Py::Str(_) => "a string",
        Py::List(_) => "an array",
        Py::Dict(_) => "an object",
    }
}

/// Typed field access on a parsed body: a missing or `null` field is
/// `None`, a field of the wrong type is a 400 naming the field.
pub struct Fields<'a>(pub &'a Py);

impl<'a> Fields<'a> {
    pub fn get(&self, key: &str) -> Option<&'a Py> {
        match self.0.get(key) {
            None | Some(Py::None) => None,
            Some(v) => Some(v),
        }
    }

    fn wrong(key: &str, want: &str, got: &Py) -> BadRequest {
        BadRequest::new(format!(
            "field '{key}' must be {want}, not {}",
            json_type(got)
        ))
    }

    /// A string field. NUL can't be passed to a subprocess, so it's a 400 too.
    pub fn str(&self, key: &str) -> Result<Option<&'a str>, BadRequest> {
        match self.get(key) {
            None => Ok(None),
            Some(Py::Str(s)) if s.contains('\0') => Err(BadRequest::new(format!(
                "field '{key}' must not contain NUL"
            ))),
            Some(Py::Str(s)) => Ok(Some(s)),
            Some(v) => Err(Self::wrong(key, "a string", v)),
        }
    }

    /// A string field, `default` when missing or null.
    pub fn str_or(&self, key: &str, default: &'a str) -> Result<&'a str, BadRequest> {
        Ok(self.str(key)?.unwrap_or(default))
    }

    /// An integer field: JSON integers, booleans, integral floats and
    /// decimal strings (what Python's `int()` accepted).
    pub fn int(&self, key: &str) -> Result<Option<i128>, BadRequest> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => int_of(v)
                .map(Some)
                .ok_or_else(|| Self::wrong(key, "an integer", v)),
        }
    }

    /// A string or a list of scalars (each as Python's `str(x)`).
    pub fn strs(&self, key: &str) -> Result<Option<Vec<String>>, BadRequest> {
        let out = match self.get(key) {
            None => return Ok(None),
            Some(Py::Str(s)) => vec![s.clone()],
            Some(Py::List(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it {
                        Py::List(_) | Py::Dict(_) | Py::None => {
                            return Err(BadRequest::new(format!(
                                "field '{key}' must hold strings, not {}",
                                json_type(it)
                            )))
                        }
                        other => out.push(other.py_str()),
                    }
                }
                out
            }
            Some(v) => return Err(Self::wrong(key, "a string or an array of strings", v)),
        };
        if out.iter().any(|s| s.contains('\0')) {
            return Err(BadRequest::new(format!(
                "field '{key}' must not contain NUL"
            )));
        }
        Ok(Some(out))
    }
}

/// Python `int(v)` for a JSON value: integers, booleans, finite floats
/// (truncated) and decimal strings; `None` where Python raised.
pub fn int_of(v: &Py) -> Option<i128> {
    match v {
        Py::Int(i) => Some(*i),
        Py::Bool(b) => Some(*b as i128),
        Py::Float(f) if f.is_finite() && f.abs() < 1e30 => Some(f.trunc() as i128),
        Py::Str(s) => strip_int(s),
        _ => None,
    }
}

/// Python `int(s)` on a string: surrounding whitespace, optional sign,
/// digits with single `_` between them.
fn strip_int(s: &str) -> Option<i128> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let n: i128 = digits.replace('_', "").parse().ok()?;
    Some(if neg { -n } else { n })
}

/// Query parameters (first value per key), percent-decoded.
pub fn query(req_uri: &hyper::Uri) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = req_uri.query() {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.entry(decode(k)).or_insert_with(|| decode(v));
        }
    }
    out
}

fn decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies() {
        assert_eq!(parse_object(b"").unwrap(), Py::dict());
        assert_eq!(parse_object(b" \n").unwrap(), Py::dict());
        assert!(parse_object(b"{\"a\": 1}").unwrap().is_dict());
        for bad in [
            &b"{not json"[..],
            b"[]",
            b"5",
            b"\"x\"",
            b"null",
            b"{\"a\":\"\xff\"}",
        ] {
            assert!(parse_object(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn fields() {
        let b = parse_object(
            br#"{"s": "x", "n": 1, "f": 2.9, "t": true, "z": null, "ns": " 4_0 ", "l": ["Up", 3], "bad": [[1]], "nul": "a\u0000"}"#,
        )
        .unwrap();
        let f = Fields(&b);
        assert_eq!(f.str("s").unwrap(), Some("x"));
        assert_eq!(f.str("z").unwrap(), None);
        assert_eq!(f.str("missing").unwrap(), None);
        assert_eq!(f.str_or("missing", "d").unwrap(), "d");
        assert!(f.str("n").is_err());
        assert!(f.str("nul").is_err());
        assert_eq!(f.int("n").unwrap(), Some(1));
        assert_eq!(f.int("f").unwrap(), Some(2));
        assert_eq!(f.int("t").unwrap(), Some(1));
        assert_eq!(f.int("ns").unwrap(), Some(40));
        assert!(f.int("s").is_err());
        assert!(f.int("l").is_err());
        assert_eq!(f.strs("s").unwrap(), Some(vec!["x".to_string()]));
        assert_eq!(
            f.strs("l").unwrap(),
            Some(vec!["Up".to_string(), "3".to_string()])
        );
        assert!(f.strs("n").is_err());
        assert!(f.strs("bad").is_err());
        assert_eq!(
            f.str("n").unwrap_err().0,
            "field 'n' must be a string, not a number"
        );
    }

    #[test]
    fn content_lengths() {
        let mut h = HeaderMap::new();
        assert_eq!(content_length(&h), Ok(0));
        h.insert("content-length", "12".parse().unwrap());
        assert_eq!(content_length(&h), Ok(12));
        h.insert("content-length", "x".parse().unwrap());
        assert!(content_length(&h).is_err());
    }
}
