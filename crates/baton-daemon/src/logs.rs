//! Structured JSONL logs, line-compatible with the Python server's
//! (`~/Library/Logs/cs-server.{requests,events,errors,client}.jsonl`) so the
//! bench tools can join client and server timings by request id.
//!
//! Directory: `$BATON_LOG_DIR`, else `~/Library/Logs`. Each file rotates to
//! `<name>.1` past 50 MB, like `_append_jsonl`. Lines are written with
//! Python's compact `json.dumps(…, separators=(",", ":"))` encoding.
//!
//! For other groups:
//! - `logs::RequestLog::begin(&req)` before handling, then
//!   `log.finish(response)` — the request line is written when the body has
//!   been sent (or dropped), with the bytes actually sent; works for SSE.
//! - `logs::event("name", vec![("field", Py::from(..)), ..])`.
//! - `logs::error("request", "TypeName", "message", frames, ctx)`.
#![allow(dead_code)] // API for the other groups

use crate::proxy::Body;
use crate::pyjson::{Py, Style};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use hyper::{Request, Response};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

pub const REQUESTS: &str = "cs-server.requests.jsonl";
pub const EVENTS: &str = "cs-server.events.jsonl";
pub const ERRORS: &str = "cs-server.errors.jsonl";
pub const CLIENT: &str = "cs-server.client.jsonl";
const MAX_BYTES: u64 = 50 * 1024 * 1024;

static LOG_LOCK: Mutex<()> = Mutex::new(());

pub fn dir() -> PathBuf {
    let d = match std::env::var_os("BATON_LOG_DIR") {
        Some(d) if !d.is_empty() => crate::expand_home(&d.to_string_lossy()),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Logs"),
    };
    let _ = std::fs::create_dir_all(&d);
    d
}

/// `_append_jsonl`: one compact line, rotate past 50 MB, never fails loudly.
pub fn append(file: &str, record: &Py) {
    let line = record.dumps(Style::Compact) + "\n";
    let path = dir().join(file);
    let _g = LOG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_file_name(format!("{file}.1")));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `time.strftime("%Y-%m-%dT%H:%M:%S")` (local time).
pub fn local_ts() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub fn local_ts_of(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
}

/// Unix seconds as a float (`time.time()`).
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `n` random lowercase hex chars (`uuid.uuid4().hex[:n]`).
pub fn rand_hex(n: usize) -> String {
    let mut buf = vec![0u8; n.div_ceil(2)];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok();
    if !ok {
        use std::hash::{BuildHasher, Hasher};
        for chunk in buf.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(Instant::now().elapsed().as_nanos());
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    let mut s: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    s.truncate(n);
    s
}

/// FNV-1a 64 — stable fingerprints (no crypto needed).
pub fn fnv64(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

/// The daemon's release id (the Python server's `rel`): a fingerprint of
/// the running executable, stable for the process lifetime.
pub fn release() -> &'static Release {
    static REL: OnceLock<Release> = OnceLock::new();
    REL.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_default();
        let data = std::fs::read(&exe).unwrap_or_default();
        let mtime = std::fs::metadata(&exe)
            .and_then(|m| m.modified())
            .map(|t| {
                chrono::DateTime::<chrono::Local>::from(t)
                    .format("%Y-%m-%dT%H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        Release {
            id: format!("{:016x}", fnv64(&data))[..12].to_string(),
            exe: exe.to_string_lossy().into_owned(),
            mtime,
        }
    })
}

pub struct Release {
    pub id: String,
    pub exe: String,
    pub mtime: String,
}

/// `_log_event`.
pub fn event(name: &str, fields: Vec<(&str, Py)>) {
    let mut rec = Py::dict()
        .with("ts", local_ts())
        .with("event", name)
        .with("rel", release().id.as_str());
    for (k, v) in fields {
        rec.set(k, v);
    }
    append(EVENTS, &rec);
}

/// `_log_error`: `frames` are "file:line fn" strings (innermost last).
pub fn error(source: &str, typ: &str, msg: &str, frames: Vec<String>, ctx: Vec<(&str, Py)>) {
    let top = frames.last().cloned().unwrap_or_else(|| "?".into());
    let fp = format!("{:016x}", fnv64(format!("{typ}|{top}").as_bytes()))[..10].to_string();
    let mut rec = Py::dict()
        .with("ts", local_ts())
        .with("rel", release().id.as_str())
        .with("pid", std::process::id() as u64)
        .with("source", source)
        .with("type", typ)
        .with("msg", crate::pyjson::head(msg, 500))
        .with("fp", fp)
        .with(
            "frames",
            Py::List(
                frames
                    .iter()
                    .rev()
                    .take(8)
                    .rev()
                    .map(|f| Py::str(f.as_str()))
                    .collect(),
            ),
        );
    for (k, v) in ctx {
        rec.set(k, v);
    }
    append(ERRORS, &rec);
}

/// One request's log line (`_log_request`), written when its body is done.
pub struct RequestLog {
    pub rid: String,
    crid: Option<String>,
    method: String,
    path: String,
    client: String,
    started_wall: std::time::SystemTime,
    started: Instant,
}

impl RequestLog {
    pub fn begin<B>(req: &Request<B>) -> RequestLog {
        let pq = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "?".into());
        RequestLog {
            rid: rand_hex(8),
            crid: req
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(String::from),
            method: req.method().as_str().to_string(),
            path: pq,
            // The peer address is available once the accept loop stores it
            // in the request extensions; "-" until then.
            client: req
                .extensions()
                .get::<SocketAddr>()
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".into()),
            started_wall: std::time::SystemTime::now(),
            started: Instant::now(),
        }
    }

    /// Wrap the response body; the line is written when it is dropped
    /// (fully sent or the client went away).
    pub fn finish(self, resp: Response<Body>) -> Response<Body> {
        let status = resp.status().as_u16();
        let gz = resp.headers().contains_key("content-encoding");
        // Python counts every byte written: status line + headers + body.
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n",
            status,
            resp.status().canonical_reason().unwrap_or("")
        )
        .len()
            + "date: Sun, 27 Sep 2026 00:00:00 GMT\r\n".len()
            + 2;
        for (k, v) in resp.headers() {
            head += k.as_str().len() + 2 + v.len() + 2;
        }
        let (parts, body) = resp.into_parts();
        let logged = LoggedBody {
            inner: body,
            sent: head as u64,
            status,
            gz,
            log: Some(self),
        };
        Response::from_parts(parts, logged.boxed())
    }

    fn write(&self, status: u16, nbytes: u64, gz: bool) {
        append(
            REQUESTS,
            &Py::dict()
                .with("ts", local_ts_of(self.started_wall))
                .with("rid", self.rid.as_str())
                .with("crid", self.crid.clone())
                .with("m", self.method.as_str())
                .with("path", crate::pyjson::head(&self.path, 160))
                .with("st", status as u64)
                .with("ms", self.started.elapsed().as_millis() as u64)
                .with("bytes", nbytes)
                .with("gz", if gz { Py::Bool(true) } else { Py::None })
                .with("client", self.client.as_str())
                .with("rel", release().id.as_str()),
        );
    }
}

struct LoggedBody {
    inner: Body,
    sent: u64,
    status: u16,
    gz: bool,
    log: Option<RequestLog>,
}

impl HttpBody for LoggedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let r = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(f))) = &r {
            if let Some(d) = f.data_ref() {
                self.sent += d.len() as u64;
            }
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        if let Some(log) = self.log.take() {
            log.write(self.status, self.sent, self.gz);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rand_hex_len() {
        let a = rand_hex(8);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, rand_hex(8));
    }
}
