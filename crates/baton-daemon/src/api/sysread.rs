//! Read-side fixes for two system endpoints that otherwise live in the
//! control group: `/api/browse` input validation (INC-040) and
//! `/api/orphans` reading the known-session set from the list engine's
//! cached build (like Python's `load_all_sessions` cache) instead of a
//! separate full transcript parse per request. Routed before `control`,
//! which keeps the unchanged listing and search code for valid browse
//! requests.

use super::Routed;
use crate::cmd;
use crate::http::bytes_response;
use crate::logs::{self, RequestLog};
use crate::proxy::Body;
use crate::pyjson::{Py, Style};
use crate::state::State;
use baton_core::pyfmt::strip;
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
    if req.method() != Method::GET {
        return Err(req);
    }
    match req.uri().path() {
        "/api/browse" => browse(state, req).await,
        "/api/orphans" => {
            let log = RequestLog::begin(&req);
            let resp = orphans(state, req.headers()).await;
            Ok(log.finish(resp))
        }
        _ => Err(req),
    }
}

fn send_json(status: StatusCode, v: &Py, h: &HeaderMap) -> Response<Body> {
    bytes_response(
        status,
        "application/json",
        v.dumps(Style::Default).into_bytes(),
        h,
        None,
    )
}

/// `parse_qs(query)` first values (blank values dropped, like Python).
fn parse_qs(q: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in q.unwrap_or("").split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = unquote_plus(v);
        if v.is_empty() {
            continue;
        }
        out.entry(unquote_plus(k)).or_insert(v);
    }
    out
}

fn unquote_plus(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hx = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|x| u8::from_str_radix(x, 16).ok());
                match hx {
                    Some(c) => {
                        out.push(c);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

// ── /api/browse (INC-040) ──────────────────────────────────────────────

/// `~` / `~/x` expanded (Python passed it to `os.scandir` verbatim).
fn expanduser(p: &str, home: &Path) -> Option<String> {
    let home = home.to_string_lossy();
    if p == "~" {
        Some(home.into_owned())
    } else {
        p.strip_prefix("~/")
            .map(|rest| format!("{}/{}", home.trim_end_matches('/'), rest))
    }
}

/// A missing directory → 404, a file → 400, `~` → the home listing; every
/// other request goes on to the control group's listing unchanged.
/// Python answered 500 for all three.
async fn browse(state: &Arc<State>, mut req: Request<Incoming>) -> Routed {
    let q = parse_qs(req.uri().query());
    let Some(dir) = q.get("dir") else {
        return Err(req);
    };
    let expanded = expanduser(dir, &state.home);
    let target = expanded.clone().unwrap_or_else(|| dir.clone());
    if !q.contains_key("q") {
        let err = match std::fs::metadata(&target) {
            Ok(m) if m.is_dir() => None,
            Ok(_) => Some((StatusCode::BAD_REQUEST, "not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Some((StatusCode::NOT_FOUND, "no such directory"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {
                Some((StatusCode::NOT_FOUND, "no such directory"))
            }
            Err(_) => None, // permission errors: control answers [] like Python
        };
        if let Some((status, msg)) = err {
            let log = RequestLog::begin(&req);
            let resp = send_json(
                status,
                &Py::dict().with("error", msg).with("dir", dir.as_str()),
                req.headers(),
            );
            return Ok(log.finish(resp));
        }
    }
    if let Some(exp) = expanded {
        // Hand control the expanded directory (other params kept).
        let rest: Vec<String> = req
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|p| {
                !p.is_empty()
                    && p.split_once('=').map(|(k, _)| unquote_plus(k)) != Some("dir".into())
            })
            .map(String::from)
            .collect();
        let mut query = format!("dir={}", quote(&exp));
        for p in rest {
            query.push('&');
            query.push_str(&p);
        }
        if let Ok(uri) = format!("{}?{query}", req.uri().path()).parse() {
            *req.uri_mut() = uri;
        }
    }
    Err(req)
}

// ── /api/orphans ───────────────────────────────────────────────────────

async fn orphans(state: &Arc<State>, h: &HeaderMap) -> Response<Body> {
    let o = cmd::run(
        &[
            "tmux",
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_tty}\t#{session_activity}",
        ],
        3.0,
    )
    .await;
    if o.rc != 0 {
        return send_orphans(vec![], h);
    }
    let Some(known) = super::list::known_session_ids(state).await else {
        return send_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &Py::dict()
                .with("error", "internal error")
                .with("rid", logs::rand_hex(8)),
            h,
        );
    };
    let tty_map = load_tty_map(&state.home.join(".claude/tty_session_map.json"));
    let now = logs::unix_now();
    let mut seen = HashSet::new();
    let mut out: Vec<(i64, Py)> = Vec::new();
    for line in o.stdout.lines() {
        let parts: Vec<&str> = strip(line).split('\t').collect();
        if parts.len() != 3 {
            continue;
        }
        let (name, tty, activity) = (parts[0], parts[1], parts[2]);
        if !name.starts_with("claude-") || !seen.insert(name.to_string()) {
            continue;
        }
        let mapped = tty_map.get(tty).cloned();
        let orphan = match &mapped {
            None => true,
            Some(sid) => !known.contains(sid),
        };
        if !orphan {
            continue;
        }
        let (act, age) = match strip(activity).parse::<i64>() {
            Ok(a) => (a, (now - a as f64) as i64),
            Err(_) => (0, 0),
        };
        out.push((
            age,
            Py::dict()
                .with("tmux_name", name)
                .with("mapped_session_id", mapped.clone())
                .with(
                    "reason",
                    if mapped.is_none() {
                        "no mapping"
                    } else {
                        "session not in catalog"
                    },
                )
                .with("pane_activity_ts", act)
                .with("idle_seconds", age),
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    send_orphans(out.into_iter().map(|(_, v)| v).collect(), h)
}

fn send_orphans(list: Vec<Py>, h: &HeaderMap) -> Response<Body> {
    let n = list.len();
    send_json(
        StatusCode::OK,
        &Py::dict().with("orphans", Py::List(list)).with("count", n),
        h,
    )
}

/// `~/.claude/tty_session_map.json` entries younger than 7 days. Like the
/// Python loop, a malformed entry stops the scan (its `except: pass`).
fn load_tty_map(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(data) = std::fs::read(path) else {
        return out;
    };
    let Ok(Py::Dict(items)) = Py::loads(&data) else {
        return out;
    };
    let now = logs::unix_now();
    for (tty, info) in items {
        if !info.is_dict() {
            continue;
        }
        let ts = match info.get("ts") {
            None => 0.0,
            Some(Py::Int(i)) => *i as f64,
            Some(Py::Float(f)) => *f,
            Some(Py::Bool(b)) => *b as u8 as f64,
            Some(_) => break,
        };
        if now - ts < 7.0 * 86400.0 {
            if let Some(sid) = info.get("session_id").filter(|v| v.truthy()) {
                match sid {
                    Py::Str(s) => {
                        out.insert(tty, s.clone());
                    }
                    _ => break,
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_params() {
        let q = parse_qs(Some("dir=%7E%2Fx&dir=y&q="));
        assert_eq!(q.get("dir").map(String::as_str), Some("~/x"));
        assert!(!q.contains_key("q"));
        let home = Path::new("/Users/me/");
        assert_eq!(expanduser("~", home).as_deref(), Some("/Users/me/"));
        assert_eq!(expanduser("~/x", home).as_deref(), Some("/Users/me/x"));
        assert_eq!(expanduser("/tmp", home), None);
        assert_eq!(quote("/a b/é"), "/a%20b/%C3%A9");
    }
}
