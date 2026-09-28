//! `sessions_core.list_children(session_id)`: Claude subagents (from
//! `.meta.json`, no transcript parsing) and Codex subagent threads
//! (`session_meta.parent_thread_id`), newest first.

use super::paths::{glob, rglob};
use super::{isoformat_utc, py_type_name, PyErr, ReadConfig};
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

pub fn list_children(cfg: &ReadConfig, session_id: &str) -> Result<Vec<Value>, PyErr> {
    let mut out: Vec<Value> = Vec::new();
    let dirs = glob(&cfg.claude_projects, &format!("*/{session_id}/subagents")).map_err(PyErr)?;
    for d in dirs {
        let files = glob(&d, "*.jsonl").map_err(PyErr)?;
        for jf in files {
            let mut meta = Map::new();
            let mf = with_suffix_meta(&jf);
            if mf.exists() {
                if let Ok(bytes) = std::fs::read(&mf) {
                    let text = String::from_utf8(bytes).map_err(|e| {
                        PyErr(super::messages::utf8_error_message(
                            e.as_bytes(),
                            &e.utf8_error(),
                        ))
                    })?;
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        match v {
                            Value::Object(o) => meta = o,
                            other => {
                                return Err(PyErr(format!(
                                    "'{}' object has no attribute 'get'",
                                    py_type_name(&other)
                                )))
                            }
                        }
                    }
                }
            }
            let mut first_ts = Value::Null;
            if let Ok(lines) = head_lines(&jf, 1) {
                let line = lines.into_iter().next().unwrap_or_default();
                let src = if line.is_empty() {
                    "{}".to_string()
                } else {
                    line
                };
                if let Ok(v) = serde_json::from_str::<Value>(&src) {
                    match v {
                        Value::Object(o) => {
                            first_ts = o.get("timestamp").cloned().unwrap_or(Value::Null)
                        }
                        other => {
                            return Err(PyErr(format!(
                                "'{}' object has no attribute 'get'",
                                py_type_name(&other)
                            )))
                        }
                    }
                }
            }
            let st = std::fs::metadata(&jf).map_err(|e| PyErr(e.to_string()))?;
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
            let Ok(head) = head_lines(&jf, 5) else {
                continue;
            };
            for line in head {
                if !line.contains("\"session_meta\"") {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line.trim_end_matches('\n')) else {
                    break;
                };
                let Some(o) = v.as_object() else {
                    return Err(PyErr(format!(
                        "'{}' object has no attribute 'get'",
                        py_type_name(&v)
                    )));
                };
                let p = match o.get("payload") {
                    Some(x) if truthy(x) => x.clone(),
                    _ => json!({}),
                };
                let Some(p) = p.as_object() else {
                    return Err(PyErr(format!(
                        "'{}' object has no attribute 'get'",
                        py_type_name(&p)
                    )));
                };
                if p.get("parent_thread_id").and_then(Value::as_str) == Some(session_id) {
                    let st = std::fs::metadata(&jf).map_err(|e| PyErr(e.to_string()))?;
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
                break;
            }
        }
    }
    let key = |c: &Value| match c.get("last_ts") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => String::new(),
    };
    out.sort_by_key(|c| std::cmp::Reverse(key(c)));
    Ok(out)
}
