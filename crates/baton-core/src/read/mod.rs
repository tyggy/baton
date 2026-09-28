//! The read group (`/api/session`, `/api/messages`, `/api/tail`,
//! `/api/asset`, `/api/children`) — ports of the Python reference
//! server's transcript lookup and readers, byte-for-byte where the
//! response depends on them. HTTP lives in the daemon; this is the logic.

pub mod assets;
pub mod children;
pub mod messages;
pub mod paths;
pub mod preview;

use assets::AssetPolicy;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Python `int(s)` for a str; None where it raised ValueError.
pub fn py_int(s: &str) -> Option<i64> {
    preview::py_int_str(s)
}

/// An exception message as Python's `str(e)` would render it.
#[derive(Debug, Clone)]
pub struct PyErr(pub String);

/// `type(v).__name__` for a JSON value.
pub fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// The body `_send_500_if_possible` sends for a crashed handler.
pub fn internal_error() -> Value {
    let rid = format!(
        "{:08x}",
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            & 0xffff_ffff) as u32
    );
    json!({"error": "internal error", "rid": rid})
}

/// `st.st_mtime` as Python computes it.
pub fn mtime_f64(m: &std::fs::Metadata) -> f64 {
    m.mtime() as f64 + m.mtime_nsec() as f64 * 1e-9
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `datetime.fromtimestamp(st.st_mtime, timezone.utc).isoformat()
/// .replace("+00:00", "Z")`.
pub fn isoformat_utc(m: &std::fs::Metadata) -> String {
    let t = mtime_f64(m);
    let mut secs = t.floor();
    let mut us = ((t - secs) * 1e6).round_ties_even();
    if us >= 1e6 {
        secs += 1.0;
        us -= 1e6;
    }
    let secs = secs as i64;
    let us = us as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let frac = if us != 0 {
        format!(".{us:06}")
    } else {
        String::new()
    };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}Z")
}

pub struct ReadConfig {
    pub home: String,
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    /// Markdown exports of sessions (`<kb>/sessions.nosync`), used as a
    /// preview cache when present. `None` without a knowledge base.
    pub sessions_cache_dir: Option<PathBuf>,
    pub assets: AssetPolicy,
}

/// `os.environ` as `csm_config` sees it: the process environment, then
/// `CSM_ENV_FILE` files (earlier files win, existing vars are kept).
fn csm_env(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        return Some(v);
    }
    let files = std::env::var("CSM_ENV_FILE").ok()?;
    let home = std::env::var("HOME").unwrap_or_default();
    for piece in files.split(':') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let path = paths::expanduser(piece, &home);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || !line.contains('=') {
                continue;
            }
            let line = line
                .strip_prefix("export ")
                .map(str::trim_start)
                .unwrap_or(line);
            let (k, v) = line.split_once('=').expect("contains =");
            if k.trim() == name {
                return Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    None
}

impl ReadConfig {
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let env_path = |name: &str, default: &str| -> String {
            let v = csm_env(name).unwrap_or_else(|| default.to_string());
            assets::pathlib_str(&paths::expanduser(&v, &home))
        };
        let settings = crate::settings::get();
        // Env (the Python server's names) wins over the config file.
        let base = csm_env("CSM_BASE_PATH")
            .map(|v| env_path("CSM_BASE_PATH", &v))
            .or_else(|| {
                settings
                    .kb_path
                    .as_ref()
                    .map(|p| assets::pathlib_str(&p.to_string_lossy()))
            });
        let cache = match csm_env("CSM_SESSIONS_CACHE_DIR") {
            Some(v) => Some(env_path("CSM_SESSIONS_CACHE_DIR", &v)),
            None => base.map(|b| format!("{b}/sessions.nosync")),
        };
        let assets = match csm_env("CSM_ASSET_ROOTS").filter(|r| !r.trim().is_empty()) {
            Some(r) => AssetPolicy::new(&home, Some(&r)),
            None => AssetPolicy::with_roots(&home, settings.asset_roots.clone()),
        };
        let home_p = Path::new(&home);
        ReadConfig {
            claude_projects: crate::settings::claude_dir(home_p).join("projects"),
            codex_sessions: crate::settings::codex_dir(home_p).join("sessions"),
            sessions_cache_dir: cache.map(PathBuf::from),
            assets,
            home,
        }
    }

    /// `_find_claude_jsonl(session_id)`: the largest copy across project
    /// dirs, else a subagent transcript `agent-<id>.jsonl`.
    /// Invalid ids (see [`valid_session_id`]) never reach the glob.
    pub fn find_claude_jsonl(&self, session_id: &str) -> Option<PathBuf> {
        if !valid_session_id(session_id) || !self.claude_projects.exists() {
            return None;
        }
        let matches = paths::glob(&self.claude_projects, &format!("*/{session_id}.jsonl")).ok()?;
        let mut best: Option<(u64, PathBuf)> = None;
        for p in matches {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if best.as_ref().is_none_or(|(s, _)| size > *s) {
                best = Some((size, p));
            }
        }
        if let Some((_, p)) = best {
            return Some(p);
        }
        if session_id.starts_with("agent-") {
            return paths::glob(
                &self.claude_projects,
                &format!("*/*/subagents/{session_id}.jsonl"),
            )
            .ok()?
            .into_iter()
            .next();
        }
        None
    }

    /// `_find_codex_rollout(session_id)`.
    pub fn find_codex_rollout(&self, session_id: &str) -> Option<PathBuf> {
        if session_id.len() != 36 || !is_uuid(session_id) || !self.codex_sessions.exists() {
            return None;
        }
        paths::rglob(
            &self.codex_sessions,
            &format!("rollout-*{session_id}.jsonl"),
            true,
        )
        .into_iter()
        .next()
    }
}

/// A session id the read endpoints accept: a UUID (Claude sessions,
/// Codex threads) or a subagent transcript id `agent-<alnum/_/->`.
/// Anything else — glob metacharacters (`*`, `?`, `[`), path separators,
/// `..`, percent-escapes, non-ASCII — is rejected before any lookup, so an
/// id can never select another session's file (INC-048; Python globbed
/// the raw id).
pub fn valid_session_id(s: &str) -> bool {
    if s.len() == 36 && is_uuid(s) {
        return true;
    }
    match s.strip_prefix("agent-") {
        Some(rest) => {
            !rest.is_empty()
                && rest.len() <= 128
                && rest
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        }
        None => false,
    }
}

/// `_UUID_RE.match` (`$` also matches before a trailing newline).
fn is_uuid(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The last lines of a file: one buffer plus line ranges (no per-line
/// copies).
#[derive(Default)]
pub struct TailLines {
    buf: Vec<u8>,
    ranges: Vec<(usize, usize)>,
}

impl TailLines {
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.ranges.iter().map(|&(a, b)| &self.buf[a..b])
    }
}

/// `_tail_jsonl_lines(path, target_lines)`: raw lines (split on b"\n",
/// the first dropped when the read didn't reach the start). The region is
/// located by counting newlines backwards in 64 KB chunks, then read once
/// into a single buffer.
pub fn tail_jsonl_lines(path: &Path, target_lines: usize) -> TailLines {
    const CHUNK: u64 = 64 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return TailLines::default();
    };
    let Ok(end) = f.seek(SeekFrom::End(0)) else {
        return TailLines::default();
    };
    if end == 0 {
        return TailLines::default();
    }
    let mut chunk = vec![0u8; CHUNK as usize];
    let mut newlines = 0usize;
    let mut pos = end;
    while pos > 0 && newlines <= target_lines {
        let read_size = CHUNK.min(pos);
        pos -= read_size;
        let c = &mut chunk[..read_size as usize];
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(c).is_err() {
            return TailLines::default();
        }
        newlines += c.iter().filter(|&&b| b == b'\n').count();
    }
    drop(chunk);
    let mut buf = vec![0u8; (end - pos) as usize];
    if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(&mut buf).is_err() {
        return TailLines::default();
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            ranges.push((start, i));
            start = i + 1;
        }
    }
    ranges.push((start, buf.len()));
    if pos > 0 {
        ranges.remove(0);
    }
    if ranges.len() > target_lines {
        ranges.drain(..ranges.len() - target_lines);
    }
    TailLines { buf, ranges }
}

pub enum CodexErr {
    /// OSError: "session file unreadable"
    Io,
}

/// `_codex_messages(path, limit)`.
pub fn codex_messages(path: &Path, limit: i64) -> Result<Vec<Value>, CodexErr> {
    std::fs::metadata(path).map_err(|_| CodexErr::Io)?;
    let mut tail: usize = 2000usize.max((limit.max(0) as usize).saturating_mul(40));
    let mut msgs;
    loop {
        let lines = tail_jsonl_lines(path, tail);
        msgs = Vec::new();
        for l in lines.iter() {
            // A malformed record is skipped, not fatal (Python raised).
            if let Ok(Some(m)) = messages::codex_line_to_message(l) {
                msgs.push(m);
            }
        }
        if msgs.len() as i64 >= limit || lines.len() < tail || tail >= 400_000 {
            break;
        }
        tail *= 4;
    }
    if limit > 0 && msgs.len() > limit as usize {
        msgs.drain(..msgs.len() - limit as usize);
    }
    Ok(msgs)
}

pub enum MessagesErr {
    NotFound,
    Unreadable,
    /// `send_json({"error": str(e)}, 500)`
    Error(String),
}

pub struct Messages {
    pub body: Value,
    /// `m<int(mtime)>-<size>-<limit>` (Claude transcripts only)
    pub etag: Option<String>,
}

/// ETag for `/api/messages` of a Claude transcript, without parsing
/// (answers If-None-Match early).
pub fn messages_etag(path: &Path, limit: i64) -> Option<String> {
    let st = std::fs::metadata(path).ok()?;
    Some(format!(
        "m{}-{}-{}",
        mtime_f64(&st).trunc() as i64,
        st.len(),
        limit
    ))
}

/// `_send_session_messages(session_id, limit)` (the cold path; Python's
/// incremental cache returns the same messages).
pub fn session_messages(
    cfg: &ReadConfig,
    session_id: &str,
    limit: i64,
) -> Result<Messages, MessagesErr> {
    let Some(path) = cfg.find_claude_jsonl(session_id) else {
        let Some(codex) = cfg.find_codex_rollout(session_id) else {
            return Err(MessagesErr::NotFound);
        };
        let out = match codex_messages(&codex, limit) {
            Ok(m) => m,
            Err(CodexErr::Io) => return Err(MessagesErr::Unreadable),
        };
        let n = out.len();
        return Ok(Messages {
            body: json!({"id": session_id, "messages": out, "count": n}),
            etag: None,
        });
    };
    let Some(etag) = messages_etag(&path, limit) else {
        return Err(MessagesErr::Unreadable);
    };
    let policy = &cfg.assets;
    let mut tail_size: usize = 500usize.max((limit.max(0) as usize).saturating_mul(10));
    let mut messages;
    loop {
        let lines = tail_jsonl_lines(&path, tail_size);
        messages = messages::parse_claude_lines(policy, lines.iter());
        let consumed_whole = lines.len() < tail_size;
        if limit <= 0 || messages.len() as i64 >= limit || consumed_whole {
            break;
        }
        if tail_size >= 200_000 {
            let bytes = std::fs::read(&path).map_err(|e| MessagesErr::Error(e.to_string()))?;
            let text = String::from_utf8_lossy(&bytes);
            messages = messages::parse_claude_lines(
                policy,
                messages::universal_lines(&text).map(str::as_bytes),
            );
            break;
        }
        tail_size *= 4;
    }
    if messages.len() > 600 {
        messages.drain(..messages.len() - 600);
    }
    if limit > 0 && messages.len() > limit as usize {
        messages.drain(..messages.len() - limit as usize);
    }
    let n = messages.len();
    Ok(Messages {
        body: json!({"id": session_id, "messages": messages, "count": n}),
        etag: Some(etag),
    })
}

/// `_TS_RE = rb'"timestamp"\s*:\s*"([^"]+)"'`: first match's value.
fn ts_value(line: &[u8]) -> Option<&[u8]> {
    const KEY: &[u8] = b"\"timestamp\"";
    let ws = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let mut from = 0;
    while let Some(off) = line[from..].windows(KEY.len()).position(|w| w == KEY) {
        let k = from + off;
        let mut j = k + KEY.len();
        while j < line.len() && ws(line[j]) {
            j += 1;
        }
        if j < line.len() && line[j] == b':' {
            j += 1;
            while j < line.len() && ws(line[j]) {
                j += 1;
            }
            if j < line.len() && line[j] == b'"' {
                let v0 = j + 1;
                if let Some(len) = line[v0..].iter().position(|&c| c == b'"') {
                    if len > 0 {
                        return Some(&line[v0..v0 + len]);
                    }
                }
            }
        }
        from = k + 1;
    }
    None
}

pub const TAIL_SINCE_SCAN: u64 = 4 << 20;

/// `_tail_start_offset(path, since)`.
pub fn tail_start_offset(path: &Path, since: Option<&str>) -> std::io::Result<u64> {
    let size = std::fs::metadata(path)?.len();
    if size == 0 {
        return Ok(0);
    }
    let mut f = std::fs::File::open(path)?;
    let Some(since) = since.filter(|s| !s.is_empty()) else {
        let back = size.min(1 << 20);
        f.seek(SeekFrom::Start(size - back))?;
        let mut data = vec![0u8; back as usize];
        f.read_exact(&mut data)?;
        return Ok(match data.iter().rposition(|&b| b == b'\n') {
            Some(nl) if nl == data.len() - 1 => size,
            Some(nl) => size - back + nl as u64 + 1,
            None if back == size => 0,
            None => size,
        });
    };
    let lo = size.saturating_sub(TAIL_SINCE_SCAN);
    f.seek(SeekFrom::Start(lo))?;
    let mut data = vec![0u8; (size - lo) as usize];
    f.read_exact(&mut data)?;
    let mut end = data.len();
    if !data.ends_with(b"\n") {
        end = data
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
    }
    let mut start = end;
    let mut pos = end;
    while pos > 0 {
        let prev = data[..pos - 1]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if prev == 0 && lo > 0 {
            break;
        }
        let line = messages::bstrip(&data[prev..pos]);
        if let Some(v) = ts_value(line) {
            match std::str::from_utf8(v) {
                Ok(s) if s <= since => break,
                Ok(_) => {}
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "timestamp not utf-8",
                    ))
                }
            }
        }
        start = prev;
        pos = prev;
    }
    Ok(lo + start as u64)
}

/// One tick of `_stream_tail`: given the pending partial line and new
/// bytes, the SSE payload to write (`data: <record>\n\n` per complete
/// line) and the new pending tail. Malformed Codex records are skipped
/// (Python's stream died on them).
pub fn tail_events(pending: &mut Vec<u8>, chunk: &[u8], codex: bool) -> Vec<u8> {
    pending.extend_from_slice(chunk);
    let Some(cut) = pending.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    let complete: Vec<u8> = pending[..cut].to_vec();
    pending.drain(..cut + 1);
    let mut out = Vec::new();
    // bytes.splitlines(): \n, \r\n, \r
    let b = &complete;
    let mut lines: Vec<&[u8]> = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        if b[i] == b'\n' || b[i] == b'\r' {
            lines.push(&b[start..i]);
            if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < b.len() {
        lines.push(&b[start..]);
    }
    for line in lines {
        let raw = messages::bstrip(line);
        if raw.is_empty() {
            continue;
        }
        if codex {
            // A malformed record is skipped; the stream lives on.
            if let Ok(Some(rec)) = messages::codex_line_to_tail_record(raw) {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(rec.as_bytes());
                out.extend_from_slice(b"\n\n");
            }
        } else {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(raw);
            out.extend_from_slice(b"\n\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_and_ts() {
        assert!(is_uuid("019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8"));
        assert!(!is_uuid("x19a1b2c-3d4e-5f60-7182-93a4b5c6d7e8"));
        assert_eq!(
            ts_value(br#"{"a":1,"timestamp" : "2026-01-01T00:00:00Z"}"#),
            Some(&b"2026-01-01T00:00:00Z"[..])
        );
        let mut pending = Vec::new();
        let out = tail_events(&mut pending, b"{\"a\":1}\n{\"b\"", false);
        assert_eq!(out, b"data: {\"a\":1}\n\n");
        assert_eq!(pending, b"{\"b\"");
    }

    #[test]
    fn session_id_validation() {
        assert!(valid_session_id("019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8"));
        assert!(valid_session_id("agent-a1b2c3d4"));
        for bad in [
            "",
            "*",
            "a*",
            "0*",
            "[a-f]*",
            "?",
            "agent-",
            "agent-*",
            "../x",
            "a;b",
            "%2A",
            "019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8\n",
            "019a1b2c-3d4e-5f60-7182-93a4b5c6d7e*",
        ] {
            assert!(!valid_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn bad_lines_are_skipped() {
        let policy = AssetPolicy::new("/nonexistent", None);
        let good = br#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        let lines: Vec<&[u8]> = vec![
            b"{\"type\":\"user\",\"message\":{\"content\":\"\xff\"}}",
            br#"{"type":"assistant","message":123}"#,
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":5}]}}"#,
            br#"{"type":"user","message":{"role":"us"#,
            good,
        ];
        let out = messages::parse_claude_lines(&policy, lines);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["blocks"][0]["text"], "hi");
        let mut pending = Vec::new();
        assert!(tail_events(
            &mut pending,
            b"{\"type\":\"response_item\",\"payload\":7}\n",
            true
        )
        .is_empty());
    }
}
