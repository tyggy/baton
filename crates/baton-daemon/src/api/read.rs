//! Group `read`: /api/session/{id}, /api/messages/{id}, /api/tail/{id}, /api/asset, /api/children/{id}.
//! Parity reference: claude-session-manager/server/api_server.py
//! (`_send_session_markdown`, `_send_session_messages`, `_stream_tail`,
//! `_send_asset`, `sessions_core.list_children`). Logic lives in
//! `baton_core::read`; this module is the HTTP surface with Python's
//! status codes, headers and bodies.

use super::Routed;
use crate::http::{accepts_gzip, full, gzip};
use crate::proxy::Body;
use crate::state::State;
use baton_core::pyjson_read as pyjson;
use baton_core::read::{self as core, assets, preview, ReadConfig};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Frame, Incoming};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

fn cfg() -> &'static ReadConfig {
    static C: OnceLock<ReadConfig> = OnceLock::new();
    C.get_or_init(ReadConfig::from_env)
}

pub async fn route(_state: &Arc<State>, req: Request<Incoming>) -> Routed {
    if req.method() != Method::GET {
        return Err(req);
    }
    let path = url_path(req.uri().path());
    let params = parse_qs(req.uri().query().unwrap_or(""));
    let headers = req.headers().clone();
    // INC-048: ids are validated before any lookup (Python globbed them,
    // so `*` / `a*` / `[a-f]*` served some other session). Ids with glob
    // or path syntax → 400; other ids that can't name a session get
    // Python's answer for an unknown id without a lookup.
    for prefix in [
        "/api/children/",
        "/api/tail/",
        "/api/session/",
        "/api/messages/",
    ] {
        let Some(sid) = path.strip_prefix(prefix) else {
            continue;
        };
        if core::valid_session_id(sid) {
            break;
        }
        let resp = if sid.contains(['*', '?', '[', ']', '/', '\\']) {
            send_json(400, &json!({"error": "invalid session id"}), &headers)
        } else if prefix == "/api/children/" {
            send_json(200, &json!({"id": sid, "children": []}), &headers)
        } else {
            send_json(404, &json!({"error": "session not found"}), &headers)
        };
        return Ok(resp);
    }
    if let Some(sid) = path.strip_prefix("/api/children/") {
        return Ok(children(sid.to_string(), &headers).await);
    }
    if path == "/api/asset" {
        return Ok(asset(params.get("path").cloned(), &headers).await);
    }
    if let Some(sid) = path.strip_prefix("/api/tail/") {
        let since = params.get("since").cloned();
        let from_start = params.get("from").map(String::as_str) == Some("0");
        return Ok(tail(sid.to_string(), since, from_start, &headers));
    }
    if let Some(sid) = path.strip_prefix("/api/session/") {
        let limit = limit_param(&params);
        return Ok(session(sid.to_string(), limit, &headers).await);
    }
    if let Some(sid) = path.strip_prefix("/api/messages/") {
        let limit = limit_param(&params);
        return Ok(messages(sid.to_string(), limit, &headers).await);
    }
    Err(req)
}

/// `urlparse(path).path`: `;params` of the last segment removed.
fn url_path(p: &str) -> String {
    let last_slash = p.rfind('/').unwrap_or(0);
    match p[last_slash..].find(';') {
        Some(i) => p[..last_slash + i].to_string(),
        None => p.to_string(),
    }
}

/// `urllib.parse.unquote(s.replace('+', ' '))`.
fn unquote_plus(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let h = (b[i + 1] as char).to_digit(16);
                let l = (b[i + 2] as char).to_digit(16);
                match (h, l) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `parse_qs(query)` keeping the first value of each key (blank values
/// and entries without '=' are dropped, like Python's defaults).
fn parse_qs(q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for nv in q.split('&') {
        if nv.is_empty() {
            continue;
        }
        let Some((k, v)) = nv.split_once('=') else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        out.entry(unquote_plus(k))
            .or_insert_with(|| unquote_plus(v));
    }
    out
}

/// `int(params.get("limit", ["50"])[0])`, ValueError → 50.
fn limit_param(params: &HashMap<String, String>) -> i64 {
    match params.get("limit") {
        None => 50,
        Some(v) => core::py_int(v).unwrap_or(50),
    }
}

// ---- responses (Python's header sets) ----

const ACAM: &str = "GET, POST, OPTIONS";
const ACAH: &str = "Content-Type, Authorization";

/// `send_json(data, status)`.
fn send_json(status: u16, v: &Value, req: &HeaderMap) -> Response<Body> {
    let body = pyjson::dumps(v).into_bytes();
    let (body, gz) = gzip_body(body, req);
    let mut b = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json");
    if gz {
        b = b.header("content-encoding", "gzip");
    }
    b.header("content-length", body.len())
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", ACAM)
        .header("access-control-allow-headers", ACAH)
        .body(full(body))
        .expect("response")
}

/// `_gzip_body`: JSON bodies of ≥1400 bytes when the client accepts gzip.
fn gzip_body(body: Vec<u8>, req: &HeaderMap) -> (Vec<u8>, bool) {
    if body.len() < 1400 || !accepts_gzip(req) {
        (body, false)
    } else {
        (gzip(&body), true)
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

// ---- /api/children/{id} ----

async fn children(sid: String, req: &HeaderMap) -> Response<Body> {
    let sid2 = sid.clone();
    match blocking(move || core::children::list_children(cfg(), &sid2)).await {
        Some(Ok(list)) => send_json(200, &json!({"id": sid, "children": list}), req),
        _ => send_json(500, &core::internal_error(), req),
    }
}

// ---- /api/session/{id} ----

async fn session(sid: String, limit: i64, req: &HeaderMap) -> Response<Body> {
    let out = blocking(move || {
        let c = cfg();
        match c.find_claude_jsonl(&sid) {
            Some(p) => preview_cached(c, &sid, &p, limit),
            None => match c.find_codex_rollout(&sid) {
                Some(codex) => preview::codex_preview(&sid, &codex, limit),
                None => preview::Preview::Err(404, json!({"error": "session not found"})),
            },
        }
    })
    .await;
    match out {
        Some(preview::Preview::Ok(v)) => send_json(200, &v, req),
        Some(preview::Preview::Err(status, v)) => send_json(status, &v, req),
        None => send_json(500, &core::internal_error(), req),
    }
}

/// Claude previews memoized on (path, size, mtime, cache-file state,
/// limit): an unchanged transcript costs no re-extraction.
fn preview_cached(c: &ReadConfig, sid: &str, path: &PathBuf, limit: i64) -> preview::Preview {
    type Key = (PathBuf, u64, i64, i64, String, i64);
    static MEMO: OnceLock<Mutex<Vec<(Key, Value)>>> = OnceLock::new();
    use std::os::unix::fs::MetadataExt;
    let key = std::fs::metadata(path).ok().map(|m| {
        let cache_state = core::paths::glob(&c.sessions_cache_dir, &format!("*/{sid}.md"))
            .ok()
            .and_then(|v| v.into_iter().next())
            .and_then(|p| std::fs::metadata(&p).ok().map(|m| (p, m)))
            .map(|(p, m)| {
                format!(
                    "{}:{}:{}:{}",
                    p.display(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec()
                )
            })
            .unwrap_or_default();
        (
            path.clone(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            format!("{sid}\u{0}{cache_state}"),
            limit,
        )
    });
    let memo = MEMO.get_or_init(|| Mutex::new(Vec::new()));
    if let Some(k) = &key {
        if let Some((_, v)) = memo.lock().expect("memo").iter().find(|(kk, _)| kk == k) {
            return preview::Preview::Ok(v.clone());
        }
    }
    static EX: OnceLock<Mutex<preview::Extractor>> = OnceLock::new();
    let ex = EX.get_or_init(|| Mutex::new(preview::Extractor::default()));
    let out = preview::claude_preview(c, ex, sid, path, limit);
    if let (Some(k), preview::Preview::Ok(v)) = (key, &out) {
        let mut m = memo.lock().expect("memo");
        if m.len() >= 16 {
            m.remove(0);
        }
        m.push((k, v.clone()));
    }
    out
}

// ---- /api/messages/{id} ----

async fn messages(sid: String, limit: i64, req: &HeaderMap) -> Response<Body> {
    let inm = req
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let out = blocking(move || {
        let c = cfg();
        // The 304 check comes before any parsing, like Python's.
        if let Some(p) = c.find_claude_jsonl(&sid) {
            if let Some(etag) = core::messages_etag(&p, limit) {
                if etag == inm {
                    return Err(Some(etag));
                }
            }
        }
        Ok(messages_cached(c, &sid, limit))
    })
    .await;
    match out {
        None => send_json(500, &core::internal_error(), req),
        Some(Err(Some(etag))) => Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header("etag", etag)
            .header("access-control-allow-origin", "*")
            .body(full(Bytes::new()))
            .expect("304"),
        Some(Err(None)) => send_json(500, &core::internal_error(), req),
        Some(Ok(Err(e))) => match e {
            core::MessagesErr::NotFound => {
                send_json(404, &json!({"error": "session not found"}), req)
            }
            core::MessagesErr::Unreadable => {
                send_json(500, &json!({"error": "session file unreadable"}), req)
            }
            core::MessagesErr::Error(msg) => send_json(500, &json!({"error": msg}), req),
        },
        Some(Ok(Ok(m))) => match m.etag {
            None => send_json(200, &m.body, req),
            Some(etag) => {
                let body = pyjson::dumps(&m.body).into_bytes();
                let (body, gz) = gzip_body(body, req);
                let mut b = Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .header("etag", etag);
                if gz {
                    b = b.header("content-encoding", "gzip");
                }
                b.header("content-length", body.len())
                    .header("access-control-allow-origin", "*")
                    .body(full(body))
                    .expect("response")
            }
        },
    }
}

/// `/api/messages` results memoized while the transcript is unchanged
/// (Python keeps a parsed-message cache per session too).
fn messages_cached(
    c: &ReadConfig,
    sid: &str,
    limit: i64,
) -> Result<core::Messages, core::MessagesErr> {
    use std::os::unix::fs::MetadataExt;
    type Key = (String, PathBuf, u64, i64, i64, i64);
    type Entry = (Key, Value, Option<String>);
    static MEMO: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    let path = c
        .find_claude_jsonl(sid)
        .or_else(|| c.find_codex_rollout(sid));
    let key = path.as_ref().and_then(|p| {
        let m = std::fs::metadata(p).ok()?;
        Some((
            sid.to_string(),
            p.clone(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            limit,
        ))
    });
    let memo = MEMO.get_or_init(|| Mutex::new(Vec::new()));
    if let Some(k) = &key {
        if let Some((_, body, etag)) = memo.lock().expect("memo").iter().find(|(kk, ..)| kk == k) {
            return Ok(core::Messages {
                body: body.clone(),
                etag: etag.clone(),
            });
        }
    }
    let out = core::session_messages(c, sid, limit)?;
    if let Some(k) = key {
        let mut m = memo.lock().expect("memo");
        if m.len() >= 32 {
            m.remove(0);
        }
        m.push((k, out.body.clone(), out.etag.clone()));
    }
    Ok(out)
}

// ---- streaming bodies ----

/// A response body fed by a channel (SSE streams, large files).
struct ChannelBody {
    rx: mpsc::Receiver<Bytes>,
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn channel_body(cap: usize) -> (mpsc::Sender<Bytes>, Body) {
    let (tx, rx) = mpsc::channel(cap);
    (tx, ChannelBody { rx }.boxed())
}

// ---- /api/tail/{id} ----

/// Poll period; Python sleeps 1 s between checks. Keepalives keep
/// Python's cadence: one per second without new data.
const TAIL_TICK: Duration = Duration::from_millis(250);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(1);

fn tail(sid: String, since: Option<String>, from_start: bool, req: &HeaderMap) -> Response<Body> {
    let c = cfg();
    let (path, codex) = match c.find_claude_jsonl(&sid) {
        Some(p) => (p, false),
        None => match c.find_codex_rollout(&sid) {
            Some(p) => (p, true),
            None => return send_json(404, &json!({"error": "session not found"}), req),
        },
    };
    let (tx, body) = channel_body(16);
    tokio::spawn(async move {
        let p2 = path.clone();
        let since2 = since.clone();
        let offset = if from_start {
            Ok(0)
        } else {
            tokio::task::spawn_blocking(move || core::tail_start_offset(&p2, since2.as_deref()))
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("join")))
        };
        let mut offset = match offset {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(_) => return,
        };
        let mut pending: Vec<u8> = Vec::new();
        let mut last_write: Option<Instant> = None;
        loop {
            let size = match tokio::fs::metadata(&path).await {
                Ok(m) => m.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = tx
                        .send(Bytes::from_static(b"event: error\ndata: file gone\n\n"))
                        .await;
                    return;
                }
                Err(_) => return,
            };
            if size < offset {
                offset = size;
                pending.clear();
            }
            if size > offset {
                let chunk = match read_range(&path, offset, size).await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                offset = size;
                let events = core::tail_events(&mut pending, &chunk, codex);
                if !events.is_empty() {
                    if tx.send(Bytes::from(events)).await.is_err() {
                        return;
                    }
                    last_write = Some(Instant::now());
                }
            } else if last_write.is_none_or(|t| t.elapsed() >= KEEPALIVE_EVERY) {
                if tx
                    .send(Bytes::from_static(b": keepalive\n\n"))
                    .await
                    .is_err()
                {
                    return;
                }
                last_write = Some(Instant::now());
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(TAIL_TICK).await;
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("access-control-allow-origin", "*")
        .body(body)
        .expect("sse")
}

async fn read_range(path: &PathBuf, from: u64, to: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(from)).await?;
    let mut buf = vec![0u8; (to - from) as usize];
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..]).await?;
        if k == 0 {
            break;
        }
        n += k;
    }
    buf.truncate(n);
    Ok(buf)
}

// ---- /api/asset ----

async fn asset(path: Option<String>, req: &HeaderMap) -> Response<Body> {
    let Some(requested) = path else {
        return send_json(400, &json!({"error": "path required"}), req);
    };
    let accepts = accepts_gzip(req);
    let inm = req
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    enum Plan {
        Json(u16, Value),
        NotModified(String),
        Gzip {
            ctype: &'static str,
            etag: String,
            name: String,
            body: Vec<u8>,
        },
        Stream {
            ctype: &'static str,
            etag: String,
            name: String,
            file: std::fs::File,
            size: u64,
        },
    }
    let plan = blocking(move || {
        use std::os::unix::fs::MetadataExt;
        let c = cfg();
        let Some(safe) = c.assets.path_under_allowed_root(&requested) else {
            return Plan::Json(404, json!({"error": "not found"}));
        };
        match std::fs::metadata(&safe) {
            Ok(m) if m.is_file() => {}
            _ => return Plan::Json(404, json!({"error": "not found"})),
        }
        let ext = core::paths::splitext_ext(&safe).to_lowercase();
        let ctype = assets::content_type(&ext);
        let st = match std::fs::metadata(&safe) {
            Ok(st) => st,
            Err(e) => return Plan::Json(500, json!({"error": format!("read failed: {}", os_error(&e))})),
        };
        if st.len() > assets::ASSET_MAX_BYTES {
            return Plan::Json(
                413,
                json!({"error": "file too large to open on the phone", "size": st.len(), "max": assets::ASSET_MAX_BYTES}),
            );
        }
        let mtime_ns = st.mtime() as i128 * 1_000_000_000 + st.mtime_nsec() as i128;
        let etag = format!("\"{mtime_ns:x}-{:x}\"", st.len());
        if inm == etag {
            return Plan::NotModified(etag);
        }
        let textual = ctype.starts_with("text/") || ctype.starts_with("application/json");
        let gz = textual && (1400..=assets::ASSET_GZIP_MAX).contains(&st.len()) && accepts;
        let file = match std::fs::File::open(&safe) {
            Ok(f) => f,
            Err(e) => return Plan::Json(500, json!({"error": format!("read failed: {}", os_error(&e))})),
        };
        let name = core::paths::basename(&safe).to_string();
        if gz {
            use std::io::Read;
            let mut data = Vec::with_capacity(st.len() as usize);
            let mut f = file;
            if let Err(e) = f.read_to_end(&mut data) {
                return Plan::Json(500, json!({"error": format!("read failed: {}", os_error(&e))}));
            }
            Plan::Gzip {
                ctype,
                etag,
                name,
                body: gzip(&data),
            }
        } else {
            Plan::Stream {
                ctype,
                etag,
                name,
                file,
                size: st.len(),
            }
        }
    })
    .await;
    let Some(plan) = plan else {
        return send_json(500, &core::internal_error(), req);
    };
    let disposition = |name: &str| -> Option<hyper::header::HeaderValue> {
        // Python encodes header values as latin-1; names outside it made
        // the reference server fail the request.
        let latin1: Option<Vec<u8>> = format!("inline; filename=\"{name}\"")
            .chars()
            .map(|c| u8::try_from(c as u32).ok())
            .collect();
        hyper::header::HeaderValue::from_bytes(&latin1?).ok()
    };
    match plan {
        Plan::Json(status, v) => send_json(status, &v, req),
        Plan::NotModified(etag) => Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header("etag", etag)
            .header("cache-control", "private, no-cache")
            .body(full(Bytes::new()))
            .expect("304"),
        Plan::Gzip {
            ctype,
            etag,
            name,
            body,
        } => {
            let Some(disp) = disposition(&name) else {
                return send_json(500, &core::internal_error(), req);
            };
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", ctype)
                .header("content-length", body.len())
                .header("content-encoding", "gzip")
                .header("etag", etag)
                .header("cache-control", "private, no-cache")
                .header("access-control-allow-origin", "*")
                .header("content-disposition", disp)
                .body(full(body))
                .expect("asset")
        }
        Plan::Stream {
            ctype,
            etag,
            name,
            file,
            size,
        } => {
            let Some(disp) = disposition(&name) else {
                return send_json(500, &core::internal_error(), req);
            };
            let (tx, body) = channel_body(4);
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let mut f = file;
                let mut remaining = size;
                while remaining > 0 {
                    let mut buf = vec![0u8; (1u64 << 20).min(remaining) as usize];
                    let n = match f.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.truncate(n);
                    if tx.blocking_send(Bytes::from(buf)).is_err() {
                        break;
                    }
                    remaining -= n as u64;
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", ctype)
                .header("content-length", size)
                .header("etag", etag)
                .header("cache-control", "private, no-cache")
                .header("access-control-allow-origin", "*")
                .header("content-disposition", disp)
                .body(body)
                .expect("asset")
        }
    }
}

/// `str(OSError)`: `[Errno N] <strerror>: '<path>'`-less form is enough
/// here (the path is not part of `os.stat`'s message in this position).
fn os_error(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => {
            let msg = e.to_string();
            let msg = msg.split(" (os error").next().unwrap_or(&msg).to_string();
            format!("[Errno {code}] {msg}")
        }
        None => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_like_python() {
        let q = parse_qs("path=%2Ftmp%2Fa+b.md&limit=&x&limit=7&limit=9");
        assert_eq!(q.get("path").map(String::as_str), Some("/tmp/a b.md"));
        assert_eq!(q.get("limit").map(String::as_str), Some("7"));
        assert_eq!(limit_param(&q), 7);
        assert_eq!(url_path("/api/session/abc;x"), "/api/session/abc");
        assert_eq!(url_path("/api/session/a;b/c"), "/api/session/a;b/c");
    }
}
