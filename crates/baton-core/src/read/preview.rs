//! `/api/session/{id}`: `session_extractor.extract_session` (Claude),
//! the pre-extracted cache read, and `_send_codex_markdown` — the same
//! payload `{id, title, markdown, html, turn_count, started_at,
//! last_message_at}` with the HTML rendered by the Python-Markdown port.

use super::messages::universal_lines;
use super::{mtime_f64, py_type_name, PyErr, ReadConfig};
use crate::pyfmt::{head, len, strip, truthy};
use crate::pyjson_read::{dumps_unicode_compact, py_repr, py_str};
use serde_json::{json, Value};
use std::path::Path;

const PREFERRED_TOOL_ARGS: [&str; 8] = [
    "command",
    "file_path",
    "path",
    "pattern",
    "query",
    "url",
    "prompt",
    "description",
];

/// `abbreviate_tool_input`.
fn abbreviate_tool_input(inp: Option<&Value>) -> String {
    let Some(Value::Object(o)) = inp else {
        let s = match inp {
            None => "None".to_string(),
            Some(v) => py_str(v),
        };
        return head(&s, 100).to_string();
    };
    for key in PREFERRED_TOOL_ARGS {
        if let Some(v) = o.get(key).filter(|v| !v.is_null()) {
            let s = py_str(v).replace('\n', " ");
            let s = strip(&s);
            if !s.is_empty() {
                return head(s, 100).to_string();
            }
        }
    }
    let s = dumps_unicode_compact(inp.expect("object"));
    head(&s, 100).to_string()
}

/// `render_block`: `Ok(None)` = dropped; a non-str "text" value is
/// returned as-is (the later join raises on it).
fn render_block(block: &Value) -> Option<Value> {
    let b = block.as_object()?;
    match b.get("type").and_then(Value::as_str) {
        Some("text") => b.get("text").filter(|v| truthy(v)).cloned(),
        Some("thinking") => {
            let t = b.get("thinking").filter(|v| truthy(v))?;
            let (cropped, marker) = match t {
                Value::String(s) => (
                    head(s, 1000).to_string(),
                    if len(s) > 1000 { "..." } else { "" },
                ),
                Value::Array(a) => (
                    py_repr(&Value::Array(a.iter().take(1000).cloned().collect())),
                    if a.len() > 1000 { "..." } else { "" },
                ),
                other => (py_str(other), ""),
            };
            Some(json!(format!("<thinking>\n{cropped}{marker}\n</thinking>")))
        }
        Some("tool_use") => {
            let name = match b.get("name") {
                Some(v) if truthy(v) => py_str(v),
                _ => "?".into(),
            };
            Some(json!(format!(
                "[{name}] {}",
                abbreviate_tool_input(b.get("input"))
            )))
        }
        _ => None,
    }
}

/// `render_content`.
fn render_content(content: Option<&Value>) -> Result<Option<String>, PyErr> {
    match content {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let s = strip(s);
            Ok((!s.is_empty()).then(|| s.to_string()))
        }
        Some(Value::Array(blocks)) => {
            let parts: Vec<Value> = blocks.iter().filter_map(render_block).collect();
            if parts.is_empty() {
                return Ok(None);
            }
            let mut strs = Vec::with_capacity(parts.len());
            for (i, p) in parts.iter().enumerate() {
                match p {
                    Value::String(s) => strs.push(s.as_str()),
                    other => {
                        return Err(PyErr(format!(
                            "sequence item {i}: expected str instance, {} found",
                            py_type_name(other)
                        )))
                    }
                }
            }
            Ok(Some(strs.join("\n\n")))
        }
        Some(other) => {
            let s = py_str(other);
            let s = strip(&s);
            Ok((!s.is_empty()).then(|| s.to_string()))
        }
    }
}

/// `datetime.fromisoformat(ts.replace("Z", "+00:00"))` formatted with
/// `%Y-%m-%d %H:%M:%S`, or None where Python raised.
pub fn parse_timestamp(ts: Option<&Value>) -> Option<String> {
    let ts = ts?;
    if !truthy(ts) {
        return None;
    }
    let s = ts.as_str()?.replace('Z', "+00:00");
    fromisoformat(&s)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

fn digits(s: &[u8]) -> Option<i64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// A subset of Python 3.11's `datetime.fromisoformat` (calendar dates,
/// basic/extended times, fractions, offsets).
fn fromisoformat(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if !s.is_ascii() || b.len() < 7 {
        return None;
    }
    // date: YYYY-MM-DD or YYYYMMDD
    let (y, m, d, date_len) = if b.len() >= 10 && b[4] == b'-' {
        if b[7] != b'-' {
            return None;
        }
        (digits(&b[0..4])?, digits(&b[5..7])?, digits(&b[8..10])?, 10)
    } else if b.len() >= 8 {
        (digits(&b[0..4])?, digits(&b[4..6])?, digits(&b[6..8])?, 8)
    } else {
        return None;
    };
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    let (mut hh, mut mm, mut ss) = (0, 0, 0);
    if b.len() > date_len {
        // any single separator char, then the time
        let t = &b[date_len + 1..];
        let tz_at = t.iter().position(|&c| c == b'+' || c == b'-');
        let (time, tz) = match tz_at {
            Some(i) => (&t[..i], Some(&t[i + 1..])),
            None => (t, None),
        };
        let (main, frac) = match time.iter().position(|&c| c == b'.' || c == b',') {
            Some(i) => (&time[..i], Some(&time[i + 1..])),
            None => (time, None),
        };
        if let Some(f) = frac {
            digits(f)?;
        }
        let parts: Vec<&[u8]> = if main.contains(&b':') {
            main.split(|&c| c == b':').collect()
        } else {
            main.chunks(2).collect()
        };
        if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
            return None;
        }
        if frac.is_some() && parts.len() != 3 {
            return None;
        }
        hh = digits(parts[0])?;
        if parts.len() > 1 {
            mm = digits(parts[1])?;
        }
        if parts.len() > 2 {
            ss = digits(parts[2])?;
        }
        if hh > 23 || mm > 59 || ss > 59 {
            return None;
        }
        if let Some(tz) = tz {
            let (tmain, tfrac) = match tz.iter().position(|&c| c == b'.') {
                Some(i) => (&tz[..i], Some(&tz[i + 1..])),
                None => (tz, None),
            };
            if let Some(f) = tfrac {
                digits(f)?;
            }
            let tparts: Vec<&[u8]> = if tmain.contains(&b':') {
                tmain.split(|&c| c == b':').collect()
            } else {
                tmain.chunks(2).collect()
            };
            if tparts.is_empty() || tparts.len() > 3 || tparts.iter().any(|p| p.len() != 2) {
                return None;
            }
            let th = digits(tparts[0])?;
            let tm = if tparts.len() > 1 {
                digits(tparts[1])?
            } else {
                0
            };
            if th > 23 || tm > 59 {
                return None;
            }
        }
    }
    Some(format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}"))
}

pub struct Extracted {
    pub first_user_message: String,
    pub started_at: String,
    pub last_message_at: String,
    pub turn_count: usize,
    pub body: String,
}

/// `session_extractor.extract_session` as a resumable state: transcripts
/// are append-only, so an active one is re-read only from where the last
/// call stopped. Errors are sticky (Python raised on the whole file).
#[derive(Clone, Default)]
pub struct ExtractState {
    turns: Vec<(bool, Option<String>, String)>,
    first_user: Option<String>,
    started: Option<String>,
    ended: Option<String>,
    error: Option<String>,
}

impl ExtractState {
    fn feed_line(&mut self, line: &str) {
        if self.error.is_some() {
            return;
        }
        if let Err(e) = self.feed_inner(line) {
            self.error = Some(e.0);
        }
    }

    fn feed_inner(&mut self, line: &str) -> Result<(), PyErr> {
        let line = strip(line);
        if line.is_empty() {
            return Ok(());
        }
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            return Ok(());
        };
        let Some(obj) = d.as_object() else {
            return Err(PyErr(format!(
                "'{}' object has no attribute 'get'",
                py_type_name(&d)
            )));
        };
        let lt = match obj.get("type") {
            Some(Value::String(s)) if s == "user" || s == "assistant" => s.clone(),
            Some(v @ (Value::Array(_) | Value::Object(_))) if truthy(v) => {
                return Err(PyErr(format!("unhashable type: '{}'", py_type_name(v))))
            }
            _ => return Ok(()),
        };
        let empty = json!({});
        let msg = match obj.get("message") {
            Some(v) if truthy(v) => v,
            _ => &empty,
        };
        let Some(msg) = msg.as_object() else {
            return Err(PyErr(format!(
                "'{}' object has no attribute 'get'",
                py_type_name(msg)
            )));
        };
        let Some(text) = render_content(msg.get("content"))? else {
            return Ok(());
        };
        let role = match msg.get("role") {
            Some(v) if truthy(v) => match v {
                Value::String(s) => s.to_lowercase(),
                other => {
                    return Err(PyErr(format!(
                        "'{}' object has no attribute 'lower'",
                        py_type_name(other)
                    )))
                }
            },
            _ => lt.clone(),
        };
        let role = if role == "user" || role == "assistant" {
            role
        } else {
            lt
        };
        let ts = parse_timestamp(obj.get("timestamp"));
        let is_user = role == "user";
        if is_user && self.first_user.is_none() {
            let snippet = strip(&text);
            if !snippet.is_empty() {
                self.first_user = Some(head(snippet, 200).to_string());
            }
        }
        if self.started.is_none() && ts.is_some() {
            self.started = ts.clone();
        }
        if ts.is_some() {
            self.ended = ts.clone();
        }
        self.turns.push((is_user, ts, text));
        Ok(())
    }

    fn finish(&self) -> Result<Option<Extracted>, PyErr> {
        if let Some(e) = &self.error {
            return Err(PyErr(e.clone()));
        }
        if self.turns.is_empty() {
            return Ok(None);
        }
        let total: usize = self.turns.iter().map(|t| t.2.len() + 40).sum();
        let mut body = String::with_capacity(total);
        for (i, (is_user, ts, text)) in self.turns.iter().enumerate() {
            if i > 0 {
                body.push_str("\n\n");
            }
            body.push_str(if *is_user {
                "## User ["
            } else {
                "## Assistant ["
            });
            body.push_str(ts.as_deref().unwrap_or(""));
            body.push_str("]\n\n");
            body.push_str(text);
        }
        Ok(Some(Extracted {
            first_user_message: self.first_user.clone().unwrap_or_default(),
            started_at: self.started.clone().unwrap_or_default(),
            last_message_at: self.ended.clone().unwrap_or_default(),
            turn_count: self.turns.len(),
            body,
        }))
    }
}

/// `session_extractor.extract_session(path)`; `Ok(None)` when there are
/// no turns.
pub fn extract_session(path: &Path) -> Result<Option<Extracted>, PyErr> {
    let bytes = std::fs::read(path).map_err(|e| PyErr(e.to_string()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut st = ExtractState::default();
    for line in universal_lines(&text) {
        st.feed_line(line);
    }
    st.finish()
}

struct Tracked {
    ino: u64,
    offset: u64,
    state: ExtractState,
    used: u64,
}

/// Incremental `extract_session` over many transcripts (bounded LRU).
#[derive(Default)]
pub struct Extractor {
    files: std::collections::HashMap<std::path::PathBuf, Tracked>,
    clock: u64,
}

impl Extractor {
    pub fn extract(&mut self, path: &Path) -> Result<Option<Extracted>, PyErr> {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).map_err(|e| PyErr(e.to_string()))?;
        self.clock += 1;
        let fresh = match self.files.get(path) {
            Some(t) => t.ino != meta.ino() || meta.len() < t.offset,
            None => true,
        };
        if fresh {
            if self.files.len() >= 12 {
                if let Some(k) = self
                    .files
                    .iter()
                    .min_by_key(|(_, t)| t.used)
                    .map(|(k, _)| k.clone())
                {
                    self.files.remove(&k);
                }
            }
            self.files.insert(
                path.to_path_buf(),
                Tracked {
                    ino: meta.ino(),
                    offset: 0,
                    state: ExtractState::default(),
                    used: 0,
                },
            );
        }
        let t = self.files.get_mut(path).expect("inserted");
        t.used = self.clock;
        let mut f = std::fs::File::open(path).map_err(|e| PyErr(e.to_string()))?;
        f.seek(SeekFrom::Start(t.offset))
            .map_err(|e| PyErr(e.to_string()))?;
        let mut rest = Vec::new();
        f.read_to_end(&mut rest).map_err(|e| PyErr(e.to_string()))?;
        let cut = rest
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if cut > 0 {
            let text = String::from_utf8_lossy(&rest[..cut]);
            for line in universal_lines(&text) {
                t.state.feed_line(line);
            }
            t.offset += cut as u64;
        }
        if cut == rest.len() {
            return t.state.finish();
        }
        // The unterminated last line counts too (Python reads to EOF).
        let mut tmp = t.state.clone();
        let text = String::from_utf8_lossy(&rest[cut..]);
        for line in universal_lines(&text) {
            tmp.feed_line(line);
        }
        tmp.finish()
    }
}

/// `str.splitlines()`.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let brk = matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if brk {
            out.push(&s[start..i]);
            let mut next = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = it.peek() {
                    it.next();
                    next = j + 1;
                }
            }
            start = next;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Python `int(str)` (whitespace allowed, underscores between digits);
/// values beyond i64 saturate (Python ints don't overflow).
pub fn py_int_str(s: &str) -> Option<i64> {
    let t = strip(s);
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if t.is_empty() || t.starts_with('_') || t.ends_with('_') || t.contains("__") {
        return None;
    }
    let mut v: i64 = 0;
    for c in t.chars().filter(|&c| c != '_') {
        let d = c.to_digit(10)? as i64;
        v = v.saturating_mul(10).saturating_add(d);
    }
    Some(if neg { -v } else { v })
}

struct CacheRead {
    frontmatter: std::collections::HashMap<String, String>,
    body: String,
    turn_count: i64,
}

/// The fast path of `_send_session_markdown`: the pre-extracted `.md`.
fn read_cached(path: &Path) -> Option<CacheRead> {
    let bytes = std::fs::read(path).ok()?;
    let raw = String::from_utf8(bytes).ok()?;
    let raw = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut fm = std::collections::HashMap::new();
    let mut raw = raw.as_str();
    if raw.starts_with("---") {
        if let Some(end) = raw.get(3..).and_then(|r| r.find("\n---\n")).map(|i| i + 3) {
            let fm_block = raw.get(4..end).unwrap_or("");
            for line in py_splitlines(fm_block) {
                if let Some((k, v)) = line.split_once(':') {
                    fm.insert(strip(k).to_string(), strip(v).trim_matches('"').to_string());
                }
            }
            raw = &raw[end + 5..];
        }
    }
    let mut it = raw.splitn(3, '\n');
    let l0 = it.next().unwrap_or("");
    let l1 = it.next();
    let l2 = it.next();
    let body = match (l1, l2) {
        (Some(_), Some(rest)) if l0.starts_with("# ") => rest.to_string(),
        _ => raw.to_string(),
    };
    let turn_count = fm
        .get("turn_count")
        .map(|v| py_int_str(v).unwrap_or(0))
        .unwrap_or(0);
    Some(CacheRead {
        frontmatter: fm,
        body,
        turn_count,
    })
}

pub enum Preview {
    Ok(Value),
    /// status, body
    Err(u16, Value),
}

fn crop_title(t: &str) -> String {
    let t = t.replace('\n', " ");
    let t = strip(&t);
    if len(t) > 120 {
        format!("{}...", head(t, 117))
    } else {
        t.to_string()
    }
}

/// `_send_session_markdown(session_id, limit)` for a Claude transcript.
pub fn claude_preview(
    cfg: &ReadConfig,
    ex: &std::sync::Mutex<Extractor>,
    session_id: &str,
    jsonl: &Path,
    limit: i64,
) -> Preview {
    let mut body: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut started_at = String::new();
    let mut last_at = String::new();
    let mut turn_count: i64 = 0;
    let cached = super::paths::glob(&cfg.sessions_cache_dir, &format!("*/{session_id}.md"))
        .ok()
        .and_then(|v| v.into_iter().next());
    if let Some(c) = cached {
        let fresh = match (std::fs::metadata(&c), std::fs::metadata(jsonl)) {
            (Ok(a), Ok(b)) => mtime_f64(&a) >= mtime_f64(&b),
            _ => false,
        };
        if fresh {
            if let Some(cr) = read_cached(&c) {
                first_user = cr.frontmatter.get("first_user_message").cloned();
                started_at = cr
                    .frontmatter
                    .get("started_at")
                    .cloned()
                    .unwrap_or_default();
                last_at = cr
                    .frontmatter
                    .get("last_message_at")
                    .cloned()
                    .unwrap_or_default();
                turn_count = cr.turn_count;
                body = Some(cr.body);
            }
        }
    }
    if body.is_none() {
        let res = ex.lock().map(|mut e| e.extract(jsonl));
        let res = res.unwrap_or_else(|_| extract_session(jsonl));
        match res {
            Err(e) => return Preview::Err(500, json!({"error": e.0})),
            Ok(None) => return Preview::Err(404, json!({"error": "empty session"})),
            Ok(Some(x)) => {
                first_user = Some(x.first_user_message);
                started_at = x.started_at;
                last_at = x.last_message_at;
                turn_count = x.turn_count as i64;
                body = Some(x.body);
            }
        }
    }
    let mut body = body.expect("set above");
    if limit > 0 {
        let starts = turn_starts(&body);
        if starts.len() as i64 > limit {
            let cut = starts[starts.len() - limit as usize];
            body = format!(
                "_(showing last {limit} of {} turns)_\n\n{}",
                starts.len(),
                &body[cut..]
            );
        }
    }
    let title = match first_user {
        Some(t) if !t.is_empty() => t,
        _ => session_id.to_string(),
    };
    let title = crop_title(&title);
    let markdown_text = format!("# {title}\n\n{body}");
    let html = crate::pymd::markdown(&markdown_text);
    Preview::Ok(json!({
        "id": session_id,
        "title": title,
        "markdown": markdown_text,
        "html": html,
        "turn_count": turn_count,
        "started_at": started_at,
        "last_message_at": last_at,
    }))
}

/// `re.finditer(r"^## ", body, re.MULTILINE)` starts.
fn turn_starts(body: &str) -> Vec<usize> {
    let mut out = Vec::new();
    if body.starts_with("## ") {
        out.push(0);
    }
    for (i, _) in body.match_indices("\n## ") {
        out.push(i + 1);
    }
    out
}

/// `_send_codex_markdown(session_id, path, limit)`.
pub fn codex_preview(session_id: &str, path: &Path, limit: i64) -> Preview {
    let msgs = match super::codex_messages(path, limit.max(1) * 3) {
        Ok(m) => m,
        Err(super::CodexErr::Io) => {
            return Preview::Err(500, json!({"error": "session file unreadable"}))
        }
        Err(super::CodexErr::Crash) => return Preview::Err(500, super::internal_error()),
    };
    let mut turns: Vec<&Value> = msgs
        .iter()
        .filter(|m| m["blocks"][0]["type"] == "text")
        .collect();
    if turns.is_empty() {
        return Preview::Err(404, json!({"error": "empty session"}));
    }
    if limit > 0 && turns.len() > limit as usize {
        turns = turns.split_off(turns.len() - limit as usize);
    }
    let mut parts = Vec::new();
    for m in &turns {
        let who = if m["role"] == "user" { "User" } else { "Codex" };
        parts.push(format!(
            "## {who}\n\n{}",
            m["blocks"][0]["text"].as_str().unwrap_or("")
        ));
    }
    let first_user = turns
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["blocks"][0]["text"].as_str())
        .unwrap_or(session_id);
    let title = crop_title(first_user);
    let markdown_text = format!("# {title}\n\n{}", parts.join("\n\n"));
    let html = crate::pymd::markdown(&markdown_text);
    Preview::Ok(json!({
        "id": session_id,
        "title": title,
        "markdown": markdown_text,
        "html": html,
        "turn_count": turns.len(),
        "started_at": turns[0]["ts"],
        "last_message_at": turns[turns.len() - 1]["ts"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isoformat() {
        let f = |s: &str| fromisoformat(s);
        assert_eq!(
            f("2026-09-27T10:11:12.345+00:00").as_deref(),
            Some("2026-09-27 10:11:12")
        );
        assert_eq!(
            f("2026-09-27T10:11:12.123456789+00:00").as_deref(),
            Some("2026-09-27 10:11:12")
        );
        assert_eq!(
            f("2026-09-27 10:11").as_deref(),
            Some("2026-09-27 10:11:00")
        );
        assert_eq!(f("20260927T101112").as_deref(), Some("2026-09-27 10:11:12"));
        assert_eq!(f("2026-09-27").as_deref(), Some("2026-09-27 00:00:00"));
        assert_eq!(
            f("2026-09-27T10:11:12,5+05:30").as_deref(),
            Some("2026-09-27 10:11:12")
        );
        assert_eq!(
            f("2026-09-27T10:11:12.345+0530").as_deref(),
            Some("2026-09-27 10:11:12")
        );
        assert_eq!(f("2026-09-27T10").as_deref(), Some("2026-09-27 10:00:00"));
        assert_eq!(f("2026-02-30T00:00:00"), None);
        assert_eq!(f("2026-09-27T24:00:00"), None);
        assert_eq!(
            py_splitlines("a\u{b}b\u{1c}c\u{2028}d\r\ne"),
            vec!["a", "b", "c", "d", "e"]
        );
    }
}
