// Helpers are used by the group modules as they land.
#![allow(dead_code)]
//! Shared HTTP helpers for native handlers — same conventions as the
//! Python reference (docs/api.md): bearer auth, JSON with CORS headers,
//! gzip over ~1400 bytes when accepted, ETag/304.

use crate::proxy::Body;
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

/// Read a JSON request body (POST handlers). Invalid/empty → `{}` like Python's read_body.
pub async fn read_json(req: Request<Incoming>) -> Value {
    match req.into_body().collect().await {
        Ok(c) => serde_json::from_slice(&c.to_bytes()).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    }
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
