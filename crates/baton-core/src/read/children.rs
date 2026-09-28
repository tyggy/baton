//! `sessions_core.list_children(session_id)`: Claude subagents (from
//! `.meta.json`, no transcript parsing) and Codex subagent threads
//! (`session_meta.parent_thread_id`), newest first.

use super::paths::{glob, rglob};
use super::{isoformat_utc, PyErr, ReadConfig};
use crate::pyfmt::truthy;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;

/// First `n` text-mode lines (with terminators, `errors="replace"`).
fn head_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut out: Vec<String> = Vec::new();
    let mut breaks = 0;
    loop {
        let k = f.read(&mut chunk)?;
        if k == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..k]);
        breaks += chunk[..k]
            .iter()
            .filter(|&&b| b == b'\n' || b == b'\r')
            .count();
        if breaks > n {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    // readline() keeps the terminator; universal newlines turn it into "\n"
    let b = text.as_bytes();
    let mut i = 0;
    while out.len() < n {
        if i >= b.len() {
            out.push(String::new());
            continue;
        }
        let start = i;
        while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
            i += 1;
        }
        let mut line = text[start..i].to_string();
        if i < b.len() {
            line.push('\n');
            if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
        }
        out.push(line);
    }
    Ok(out)
}

fn with_suffix_meta(p: &Path) -> std::path::PathBuf {
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let suffix = match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    };
    let stem = &name[..name.len() - suffix.len()];
    p.with_file_name(format!("{stem}.meta.json"))
}

fn stem(p: &Path) -> String {
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

fn or_else(v: Option<&Value>, dflt: Value) -> Value {
    match v {
        Some(x) if truthy(x) => x.clone(),
        _ => dflt,
    }
}

/// The `session_meta` payload fields `list_children` reads from a Codex
/// rollout (None: no usable `session_meta` in the first 5 lines).
type Meta = Option<Map<String, Value>>;

/// `session_meta` of a rollout, memoized per (path, inode): the record is
/// the rollout's first line and never changes once written, so repeated
/// `/api/children` calls don't re-read every rollout head. Only settled
/// answers are kept (a meta line that parsed, or 5 complete lines
/// without one); a half-written head is re-read next time.
fn codex_meta(path: &Path) -> Meta {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Mutex, OnceLock};
    static MEMO: OnceLock<Mutex<HashMap<std::path::PathBuf, (u64, Meta)>>> = OnceLock::new();
    let ino = std::fs::metadata(path).ok()?.ino();
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((i, m)) = memo.lock().ok()?.get(path) {
        if *i == ino {
            return m.clone();
        }
    }
    let head = head_lines(path, 5).ok()?;
    let mut settled = head.iter().all(|l| l.ends_with('\n'));
    let mut meta = None;
    for line in &head {
        if !line.contains("\"session_meta\"") {
            continue;
        }
        match serde_json::from_str::<Value>(line.trim_end_matches('\n')) {
            Ok(v) => {
                settled = true;
                // Wrong shapes are skipped (Python raised → 500).
                meta = v.get("payload").and_then(Value::as_object).map(|p| {
                    [
                        "parent_thread_id",
                        "id",
                        "agent_nickname",
                        "agent_role",
                        "timestamp",
                    ]
                    .iter()
                    .filter_map(|k| p.get(*k).map(|v| (k.to_string(), v.clone())))
                    .collect()
                });
            }
            Err(_) => settled = line.ends_with('\n'),
        }
        break;
    }
    if settled {
        if let Ok(mut m) = memo.lock() {
            if m.len() > 20_000 {
                m.clear();
            }
            m.insert(path.to_path_buf(), (ino, meta.clone()));
        }
    }
    meta
}

pub fn list_children(cfg: &ReadConfig, session_id: &str) -> Result<Vec<Value>, PyErr> {
    let mut out: Vec<Value> = Vec::new();
    let dirs = glob(&cfg.claude_projects, &format!("*/{session_id}/subagents")).map_err(PyErr)?;
    for d in dirs {
        let files = glob(&d, "*.jsonl").map_err(PyErr)?;
        for jf in files {
            let mut meta = Map::new();
            let mf = with_suffix_meta(&jf);
            // A sidecar that isn't UTF-8 or isn't a JSON object is ignored
            // (Python raised and the whole `/api/children` answered 500).
            if let Ok(bytes) = std::fs::read(&mf) {
                if let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(&bytes) {
                    meta = o;
                }
            }
            let mut first_ts = Value::Null;
            if let Ok(lines) = head_lines(&jf, 1) {
                if let Some(Ok(Value::Object(o))) =
                    lines.first().map(|l| serde_json::from_str::<Value>(l))
                {
                    first_ts = o.get("timestamp").cloned().unwrap_or(Value::Null);
                }
            }
            let Ok(st) = std::fs::metadata(&jf) else {
                continue; // vanished since the glob
            };
            let st_stem = stem(&jf);
            out.push(json!({
                "id": st_stem,
                "kind": "subagent",
                "agent_type": or_else(meta.get("agentType"), json!("")),
                "title": or_else(meta.get("description"), json!(st_stem)),
                "background": meta.get("requestShape") == Some(&json!("background")),
                "first_ts": first_ts,
                "last_ts": isoformat_utc(&st),
                "size": st.len(),
            }));
        }
    }
    if cfg.codex_sessions.exists() {
        for jf in rglob(&cfg.codex_sessions, "rollout-*.jsonl", false) {
            let Some(p) = codex_meta(&jf) else {
                continue;
            };
            if p.get("parent_thread_id").and_then(Value::as_str) != Some(session_id) {
                continue;
            }
            let Ok(st) = std::fs::metadata(&jf) else {
                continue;
            };
            let s = stem(&jf);
            let tail36: String = {
                let chars: Vec<char> = s.chars().collect();
                chars[chars.len().saturating_sub(36)..].iter().collect()
            };
            out.push(json!({
                "id": or_else(p.get("id"), json!(tail36)),
                "kind": "codex-subagent",
                "agent_type": "codex",
                "title": or_else(p.get("agent_nickname"), or_else(p.get("agent_role"), json!("Codex subagent"))),
                "background": true,
                "first_ts": p.get("timestamp").cloned().unwrap_or(Value::Null),
                "last_ts": isoformat_utc(&st),
                "size": st.len(),
            }));
        }
    }
    let key = |c: &Value| match c.get("last_ts") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => String::new(),
    };
    out.sort_by_key(|c| std::cmp::Reverse(key(c)));
    Ok(out)
}
