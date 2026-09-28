//! Group `control`: session control (spawn, resume + readiness watcher,
//! kill, respond), hook events, device registration, pasted images, voice,
//! client metrics, system info (health, temp, browse, orphans, entity
//! logos) and the dev-only phone-bench queue.
//!
//! Parity reference: claude-session-manager/server/api_server.py — bodies,
//! status codes, files written and events broadcast match it byte for byte
//! (JSON through `pyjson`, Python's encoding), except where the incident
//! catalog (docs/incidents.md) asks for the fix: malformed bodies and wrong
//! field types are 400 (INC-041/045), tmux targets are exact and validated
//! (INC-049/050), `/api/resume` resumes by origin and rejects unknown or
//! glob-like ids (INC-012/047), and voice without a backend is 503
//! (INC-039). Not native here:
//! `/api/plugins` and `/api/plugin/*` (plugins are Python modules the
//! reference server imports), `/api/entity-logo` when the entity catalog
//! must be refreshed or an Airtable image fetched, and `/api/bench/*`
//! unless the daemon runs with `--dev` (the POSTs are still validated here
//! first, so a malformed one is a 400 rather than Python's 500).
//!
//! Environment (tests point these at a sandbox): `HOME` (lineage, pastes,
//! calibration, classifications, catalog), `BATON_LOG_DIR`,
//! `BATON_DEVICES_FILE`, `BATON_SESSION_PREFIX` (tmux names, default
//! `claude-`).

use super::Routed;
use crate::cmd;
use crate::http::{self, bytes_response, BadRequest, Fields};
use crate::logs::{self, RequestLog};
use crate::proxy::Body;
use crate::pyjson::{head, Py, Style};
use crate::state::State;
use crate::tmux_target as target;
use baton_core::pyfmt::strip;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

pub const API_VERSION: u64 = 1;
pub const MIN_CLIENT: u64 = 1;

#[derive(Debug)]
enum Fail {
    /// A handler failure Python would have raised → 500 `internal error`.
    Internal {
        typ: &'static str,
        msg: String,
        at: &'static str,
    },
    /// The request is unusable (body, field types) → 400 with the reason.
    Bad(BadRequest),
}

fn fail(typ: &'static str, msg: impl Into<String>, at: &'static str) -> Fail {
    Fail::Internal {
        typ,
        msg: msg.into(),
        at,
    }
}

impl From<BadRequest> for Fail {
    fn from(b: BadRequest) -> Self {
        Fail::Bad(b)
    }
}

type HResult = Result<Response<Body>, Fail>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ep {
    Health,
    Pairing,
    Temp,
    Browse,
    Orphans,
    EntityLogo,
    BenchJobGet,
    BenchJobs,
    BenchEvents,
    Spawn,
    Resume,
    Kill,
    Respond,
    HookEvent,
    RegisterDevice,
    PasteImage,
    Voice,
    ClientMetrics,
    BenchJobPost,
    BenchDone,
    /// `/api/bench/{job,done}` without `--dev`: validated, then proxied.
    BenchProxy,
}

fn dev_mode() -> bool {
    static DEV: OnceLock<bool> = OnceLock::new();
    *DEV.get_or_init(|| std::env::args().any(|a| a == "--dev"))
}

fn classify(method: &Method, path: &str) -> Option<Ep> {
    let dev = dev_mode();
    if method == Method::GET {
        return match path {
            "/api/health" => Some(Ep::Health),
            "/api/pairing" => Some(Ep::Pairing),
            "/api/temp" => Some(Ep::Temp),
            "/api/browse" => Some(Ep::Browse),
            "/api/orphans" => Some(Ep::Orphans),
            "/api/entity-logo" => Some(Ep::EntityLogo),
            "/api/bench/job" if dev => Some(Ep::BenchJobGet),
            "/api/bench/jobs" if dev => Some(Ep::BenchJobs),
            "/api/bench/events" if dev => Some(Ep::BenchEvents),
            _ => None,
        };
    }
    if method == Method::POST {
        return match path {
            "/api/spawn" => Some(Ep::Spawn),
            "/api/resume" => Some(Ep::Resume),
            "/api/kill-session" => Some(Ep::Kill),
            "/api/respond" => Some(Ep::Respond),
            "/api/hook-event" => Some(Ep::HookEvent),
            "/api/register-device" => Some(Ep::RegisterDevice),
            "/api/paste-image" => Some(Ep::PasteImage),
            "/api/voice" => Some(Ep::Voice),
            "/api/client-metrics" => Some(Ep::ClientMetrics),
            "/api/bench/job" if dev => Some(Ep::BenchJobPost),
            "/api/bench/done" if dev => Some(Ep::BenchDone),
            "/api/bench/job" | "/api/bench/done" => Some(Ep::BenchProxy),
            _ => None,
        };
    }
    None
}

pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
    let Some(ep) = classify(req.method(), req.uri().path()) else {
        return Err(req);
    };
    let q = parse_qs(req.uri().query());
    if ep == Ep::EntityLogo && !logo_is_local(state, q.get("id").map(String::as_str)) {
        return Err(req); // catalog refresh / Airtable fetch stay in Python
    }
    if ep == Ep::BenchProxy {
        return Ok(bench_proxy(state, req).await);
    }
    let log = RequestLog::begin(&req);
    let path = req.uri().path().to_string();
    let method = req.method().as_str().to_string();
    let headers = req.headers().clone();
    let crid = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let peer = req
        .extensions()
        .get::<std::net::SocketAddr>()
        .map(|a| a.ip().to_string());
    let res = dispatch(state, ep, req, &q, &headers, peer).await;
    let resp = match res {
        Ok(r) => r,
        Err(Fail::Bad(b)) => b.response(&headers),
        Err(Fail::Internal { typ, msg, at }) => {
            logs::event(
                "request_error",
                vec![
                    ("rid", Py::str(log.rid.as_str())),
                    ("path", Py::str(path.as_str())),
                    ("error", Py::str(head(&format!("{typ}({msg:?})"), 300))),
                ],
            );
            logs::error(
                "request",
                typ,
                &msg,
                vec![format!("control.rs:0 {at}")],
                vec![
                    ("rid", Py::str(log.rid.as_str())),
                    ("method", Py::str(method.as_str())),
                    ("path", Py::str(path.as_str())),
                    ("crid", Py::str(crid.as_str())),
                ],
            );
            send_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &Py::dict()
                    .with("error", "internal error")
                    .with("rid", log.rid.as_str()),
                &headers,
            )
        }
    };
    Ok(log.finish(resp))
}

async fn dispatch(
    state: &Arc<State>,
    ep: Ep,
    req: Request<Incoming>,
    q: &std::collections::HashMap<String, String>,
    h: &HeaderMap,
    peer: Option<String>,
) -> HResult {
    match ep {
        Ep::Health => Ok(send_json(StatusCode::OK, &health().await, h)),
        Ep::Pairing => Ok(pairing(state, h)),
        Ep::Temp => Ok(send_json(StatusCode::OK, &thermal().await, h)),
        Ep::Browse => browse(state, q, h).await,
        Ep::Orphans => orphans(state, h).await,
        Ep::EntityLogo => entity_logo(state, q.get("id").map(String::as_str), h),
        Ep::BenchJobGet => Ok(bench_claim(peer, h)),
        Ep::BenchJobs => Ok(bench_list(h)),
        Ep::BenchEvents => Ok(events_tap(state)),
        Ep::PasteImage => paste_image(state, req, h).await,
        Ep::Voice => voice(state, req, h).await,
        Ep::BenchProxy => unreachable!("proxied in route"),
        _ => {
            let body = http::read_object(req).await?;
            match ep {
                Ep::Spawn => spawn(state, body, h).await,
                Ep::Resume => resume(state, body, h).await,
                Ep::Kill => kill(state, body, h).await,
                Ep::Respond => respond(state, body, h).await,
                Ep::HookEvent => hook_event(state, body, h),
                Ep::RegisterDevice => register_device(state, body, h),
                Ep::ClientMetrics => client_metrics(body, h),
                Ep::BenchJobPost => bench_create(body, h),
                Ep::BenchDone => bench_done(body, h),
                _ => unreachable!("handled above"),
            }
        }
    }
}

// ── HTTP plumbing (Python's send_json / read_body / parse_qs) ──────────

fn send_json(status: StatusCode, v: &Py, h: &HeaderMap) -> Response<Body> {
    bytes_response(
        status,
        "application/json",
        v.dumps(Style::Default).into_bytes(),
        h,
        None,
    )
}

fn err_json(status: StatusCode, msg: &str, h: &HeaderMap) -> Response<Body> {
    send_json(status, &Py::dict().with("error", msg), h)
}

/// `urllib.parse.parse_qs` first values (blank values dropped).
fn parse_qs(q: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
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
                    Some(v) => {
                        out.push(v);
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

/// `(d.get(key) or "").strip()` on a local JSON file (classifications).
fn get_str_or_empty(body: &Py, key: &str, at: &'static str) -> Result<String, Fail> {
    match body.get(key) {
        Some(v) if v.truthy() => match v {
            Py::Str(s) => Ok(strip(s).to_string()),
            other => Err(fail(
                "AttributeError",
                format!("'{}' object has no attribute 'strip'", other.type_name()),
                at,
            )),
        },
        _ => Ok(String::new()),
    }
}

fn no_get(body: &Py, at: &'static str) -> Fail {
    fail(
        "AttributeError",
        format!("'{}' object has no attribute 'get'", body.type_name()),
        at,
    )
}

fn broadcast(state: &State, ev: Py) {
    let _ = state.events.send(ev.to_value());
}

fn home(state: &State) -> &Path {
    &state.home
}

fn session_prefix() -> String {
    std::env::var("BATON_SESSION_PREFIX").unwrap_or_else(|_| "claude-".to_string())
}

fn expanduser(p: &str, home: &Path) -> String {
    if p == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("{}/{}", home.to_string_lossy().trim_end_matches('/'), rest)
    } else {
        p.to_string()
    }
}

/// `_sh_quote`.
fn sh_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn tmux_apply_options(name: &str) {
    let t = target::pane(name);
    for (opt, val) in [
        ("status", "off"),
        ("mouse", "on"),
        ("copy-command", "pbcopy"),
        ("window-size", "latest"),
    ] {
        cmd::run(&["tmux", "set-option", "-t", &t, opt, val], 3.0).await;
    }
}

fn osa_open_attached(tmux_name: &str, title: &str) -> String {
    format!(
        "tell application \"Terminal\"\n  activate\n  set newWin to do script \"\"\n  do script \"tmux attach -t {tmux_name}\" in newWin\n  tell newWin to set custom title of selected tab to \"{title}\"\nend tell"
    )
}

// ── /api/health ────────────────────────────────────────────────────────

struct HealthCache {
    at: Option<Instant>,
    tmux: String,
    uptime: String,
    hostname: String,
}

async fn health() -> Py {
    static CACHE: tokio::sync::Mutex<HealthCache> = tokio::sync::Mutex::const_new(HealthCache {
        at: None,
        tmux: String::new(),
        uptime: String::new(),
        hostname: String::new(),
    });
    let mut c = CACHE.lock().await;
    if c.at.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
        c.tmux = strip(&cmd::run(&["tmux", "-V"], 2.0).await.stdout).to_string();
        c.uptime = strip(&cmd::run(&["uptime"], 2.0).await.stdout).to_string();
        c.hostname = strip(&cmd::run(&["uname", "-n"], 2.0).await.stdout).to_string();
        c.at = Some(Instant::now());
    }
    let rel = logs::release();
    let exe_name = Path::new(&rel.exe)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Py::dict()
        .with("status", "ok")
        .with("hostname", c.hostname.as_str())
        .with("version", "1.0.0")
        .with("release", rel.id.as_str())
        .with("src", rel.exe.as_str())
        .with("files", Py::dict().with(&exe_name, rel.mtime.as_str()))
        .with("pid", std::process::id() as u64)
        .with("started", started().await.as_str())
        .with("tmux", c.tmux.as_str())
        .with("uptime", c.uptime.as_str())
        .with("daemon", env!("CARGO_PKG_VERSION"))
        .with("api_version", API_VERSION)
        .with("min_client", MIN_CLIENT)
}

/// `GET /api/pairing` (auth): the addresses a paired phone should use, as
/// last written by the Mac app (`pairing.json`, no token inside). Phones
/// re-read it after pairing, so pairing before or after remote access is
/// turned on ends with the same addresses (docs/api.md).
fn pairing(state: &Arc<State>, h: &HeaderMap) -> Response<Body> {
    let path = match std::env::var_os("BATON_PAIRING_FILE") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => state
            .home
            .join("Library/Application Support/Baton/pairing.json"),
    };
    match std::fs::read(&path).ok().and_then(|b| Py::loads(&b).ok()) {
        Some(v @ Py::Dict(_)) => send_json(StatusCode::OK, &v, h),
        _ => err_json(StatusCode::NOT_FOUND, "no pairing info", h),
    }
}

/// Process start time (local, `%Y-%m-%dT%H:%M:%S`), from `ps` once.
async fn started() -> String {
    static STARTED: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    STARTED
        .get_or_init(|| async {
            let pid = std::process::id().to_string();
            let o = cmd::run(&["ps", "-o", "lstart=", "-p", &pid], 2.0).await;
            let s = o.stdout.split_whitespace().collect::<Vec<_>>().join(" ");
            chrono::NaiveDateTime::parse_from_str(&s, "%a %b %d %H:%M:%S %Y")
                .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string())
                .unwrap_or_else(|_| logs::local_ts())
        })
        .await
        .clone()
}

// ── /api/temp ──────────────────────────────────────────────────────────

async fn thermal() -> Py {
    static PCT: OnceLock<regex::Regex> = OnceLock::new();
    static STATE: OnceLock<regex::Regex> = OnceLock::new();
    static PRESSURE: OnceLock<regex::Regex> = OnceLock::new();
    let mut info = Py::dict()
        .with("cpu_temp", Py::None)
        .with("battery_percent", Py::None)
        .with("battery_state", Py::None);
    let batt = cmd::run(&["pmset", "-g", "batt"], 10.0).await.stdout;
    if !batt.is_empty() {
        let pct = PCT.get_or_init(|| regex::Regex::new(r"(\d+)%").expect("re"));
        if let Some(n) = pct.captures(&batt).and_then(|c| c[1].parse::<i128>().ok()) {
            info.set("battery_percent", Py::Int(n));
        }
        let st = STATE.get_or_init(|| {
            regex::Regex::new(r"(AC Power|Battery Power|charging|discharging|charged)").expect("re")
        });
        if let Some(c) = st.captures(&batt) {
            info.set("battery_state", Py::str(&c[1]));
        }
    }
    let mp = cmd::run(&["memory_pressure"], 10.0).await.stdout;
    if !mp.is_empty() {
        let re = PRESSURE.get_or_init(|| regex::Regex::new(r"System-wide.*?(\d+)%").expect("re"));
        if let Some(n) = re.captures(&mp).and_then(|c| c[1].parse::<i128>().ok()) {
            info.set("memory_pressure", Py::Int(n));
        }
    }
    info
}

// ── /api/browse ────────────────────────────────────────────────────────

fn split_lines_max(s: &str, max: usize) -> Py {
    Py::List(strip(s).split('\n').take(max).map(Py::str).collect())
}

async fn browse(
    state: &State,
    q: &std::collections::HashMap<String, String>,
    h: &HeaderMap,
) -> HResult {
    let default_dir = expanduser("~", home(state));
    let dir = q.get("dir").cloned().unwrap_or(default_dir);
    if let Some(query) = q.get("q") {
        let o = cmd::run(&["mdfind", "-onlyin", &dir, query], 15.0).await;
        if o.rc == 0 && !o.stdout.is_empty() {
            return Ok(send_json(
                StatusCode::OK,
                &split_lines_max(&o.stdout, 50),
                h,
            ));
        }
        let pat = format!("*{query}*");
        let o = cmd::run(&["find", &dir, "-iname", &pat, "-maxdepth", "5"], 15.0).await;
        let v = if strip(&o.stdout).is_empty() {
            Py::List(vec![])
        } else {
            split_lines_max(&o.stdout, 50)
        };
        return Ok(send_json(StatusCode::OK, &v, h));
    }
    let v = tokio::task::spawn_blocking(move || list_dir(&dir))
        .await
        .map_err(|e| fail("RuntimeError", e.to_string(), "browse_files"))??;
    Ok(send_json(StatusCode::OK, &v, h))
}

fn list_dir(dir: &str) -> Result<Py, Fail> {
    let rd = match std::fs::read_dir(if dir.is_empty() { "\0" } else { dir }) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(Py::List(vec![])),
        Err(e) => {
            let typ = match e.kind() {
                std::io::ErrorKind::NotFound => "FileNotFoundError",
                std::io::ErrorKind::NotADirectory => "NotADirectoryError",
                _ => "OSError",
            };
            return Err(fail(typ, format!("{e}: '{dir}'"), "browse_files"));
        }
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let out = names
        .into_iter()
        .take(50)
        .map(|name| {
            let full = if dir.ends_with('/') {
                format!("{dir}{name}")
            } else {
                format!("{dir}/{name}")
            };
            let meta = std::fs::metadata(&full);
            let is_dir = meta.as_ref().is_ok_and(|m| m.is_dir());
            let size = if is_dir {
                0
            } else {
                meta.map(|m| m.len()).unwrap_or(0)
            };
            Py::dict()
                .with("name", name.as_str())
                .with("path", full.as_str())
                .with("is_dir", is_dir)
                .with("size", size)
        })
        .collect();
    Ok(Py::List(out))
}

// ── /api/orphans ───────────────────────────────────────────────────────

async fn orphans(state: &Arc<State>, h: &HeaderMap) -> HResult {
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
        return Ok(send_orphans(vec![], h));
    }
    let st = state.clone();
    let known = tokio::task::spawn_blocking(move || {
        let mut tracker = st.tracker.lock().unwrap_or_else(|p| p.into_inner());
        let (sessions, _) = baton_core::sessions::build_all(&st.dirs, Some(&mut tracker));
        sessions
            .iter()
            .filter_map(|s| s["id"].as_str().map(String::from))
            .collect::<std::collections::HashSet<String>>()
    })
    .await
    .map_err(|e| fail("RuntimeError", e.to_string(), "get_orphan_tmux_sessions"))?;

    let tty_map = load_tty_map(&state.home.join(".claude/tty_session_map.json"));
    let now = logs::unix_now();
    let mut seen = std::collections::HashSet::new();
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
    Ok(send_orphans(out.into_iter().map(|(_, v)| v).collect(), h))
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
fn load_tty_map(path: &Path) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
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
                    _ => break, // unhashable/mismatched ids: Python's set lookup differs
                }
            }
        }
    }
    out
}

// ── /api/entity-logo ───────────────────────────────────────────────────

const CATALOG_TTL: f64 = 3600.0;

/// The cached catalog when it is fresh and well-formed (else Python must
/// refresh it: `None`).
fn fresh_catalog(state: &State) -> Option<Vec<Py>> {
    let path = state.home.join(".baton/entities.json");
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
    let age = SystemTime::now()
        .duration_since(mtime)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if age >= CATALOG_TTL {
        return None;
    }
    match Py::loads(&std::fs::read(&path).ok()?) {
        Ok(Py::List(items)) if items.iter().all(|e| e.get("id").is_some()) => Some(items),
        _ => None,
    }
}

fn find_entity<'a>(catalog: &'a [Py], id: &str) -> Option<&'a Py> {
    catalog.iter().find(|e| e.get("id") == Some(&Py::str(id)))
}

fn logo_path_of(e: &Py) -> Option<String> {
    match e.get("logo_path") {
        Some(Py::Str(p)) if !p.is_empty() && Path::new(p).exists() => Some(p.clone()),
        _ => None,
    }
}

/// Native unless Python has to refresh the catalog or fetch from Airtable.
fn logo_is_local(state: &State, id: Option<&str>) -> bool {
    let Some(id) = id else {
        return true; // 400 id required
    };
    let Some(catalog) = fresh_catalog(state) else {
        return false;
    };
    match find_entity(&catalog, id) {
        None => true,
        Some(e) => logo_path_of(e).is_some() || !e.get("image_url").is_some_and(Py::truthy),
    }
}

fn entity_logo(state: &State, id: Option<&str>, h: &HeaderMap) -> HResult {
    let Some(id) = id else {
        return Ok(err_json(StatusCode::BAD_REQUEST, "id required", h));
    };
    let catalog = fresh_catalog(state).unwrap_or_default();
    let Some(entity) = find_entity(&catalog, id) else {
        return Ok(err_json(StatusCode::NOT_FOUND, "entity not found", h));
    };
    let Some(path) = logo_path_of(entity) else {
        return Ok(err_json(StatusCode::NOT_FOUND, "no image", h));
    };
    let ext = Path::new(&path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let ctype = match ext.as_str() {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".svg" => "image/svg+xml",
        ".webp" => "image/webp",
        _ => "application/octet-stream",
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            return Ok(err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("read failed: {e}"),
                h,
            ))
        }
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", ctype)
        .header("content-length", data.len())
        .header("cache-control", "public, max-age=86400")
        .header("access-control-allow-origin", "*")
        .body(crate::http::full(data))
        .expect("logo response"))
}

// ── /api/spawn ─────────────────────────────────────────────────────────

async fn spawn(state: &Arc<State>, body: Py, h: &HeaderMap) -> HResult {
    let f = Fields(&body);
    let cwd_raw = match f.str("cwd")? {
        Some(c) if !c.is_empty() => c,
        _ => "~",
    };
    let cwd = expanduser(cwd_raw, home(state));
    let prompt = strip(f.str_or("prompt", "")?).to_string();
    let model = f.str_or("model", "")?;
    let open_terminal = body.get("open_terminal").is_none_or(Py::truthy);
    let name = format!("{}{}", session_prefix(), logs::rand_hex(8));

    let o = cmd::run(
        &["tmux", "new-session", "-d", "-s", &name, "-c", &cwd],
        10.0,
    )
    .await;
    if o.rc != 0 {
        let msg = if o.stderr.is_empty() {
            "tmux failed"
        } else {
            &o.stderr
        };
        return Ok(err_json(StatusCode::INTERNAL_SERVER_ERROR, msg, h));
    }
    tmux_apply_options(&name).await;
    let line = if !prompt.is_empty() {
        let model_part = if !model.is_empty() {
            format!(" --model {model}")
        } else {
            String::new()
        };
        format!("claude{model_part} -p {}", sh_quote(&prompt))
    } else if !model.is_empty() {
        format!("claude --model {model}")
    } else {
        "claude".to_string()
    };
    cmd::run(
        &[
            "tmux",
            "send-keys",
            "-t",
            &target::pane(&name),
            &target::literal(&line),
            "Enter",
        ],
        3.0,
    )
    .await;
    if open_terminal {
        cmd::popen(&["osascript", "-e", &osa_open_attached(&name, &name)]);
    }
    let body_parent = strip(f.str_or("parent_id", "")?).to_string();
    let header_parent = h
        .get("x-spawned-by")
        .and_then(|v| v.to_str().ok())
        .map(|s| strip(s).to_string())
        .unwrap_or_default();
    let parent: Option<String> = [body_parent, header_parent]
        .into_iter()
        .find(|s| !s.is_empty());
    let kind = strip(f.str_or("spawn_kind", "")?).to_string();
    let kind = if !kind.is_empty() {
        kind
    } else if parent.is_some() {
        "task".into()
    } else {
        "ios".into()
    };
    record_spawn(
        &state.home.join(".baton/session_lineage.json"),
        &name,
        parent.as_deref(),
        &kind,
    );
    broadcast(
        state,
        Py::dict()
            .with("type", "spawn")
            .with("session", name.as_str())
            .with("parent_id", parent.clone()),
    );
    Ok(send_json(
        StatusCode::OK,
        &Py::dict()
            .with("session_id", name.as_str())
            .with("parent_id", parent),
        h,
    ))
}

fn sort_keys(v: &Py) -> Py {
    match v {
        Py::Dict(items) => {
            let mut items: Vec<(String, Py)> = items
                .iter()
                .map(|(k, v)| (k.clone(), sort_keys(v)))
                .collect();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            Py::Dict(items)
        }
        Py::List(l) => Py::List(l.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// `lineage.record_spawn` (errors swallowed, as the spawn handler does).
fn record_spawn(path: &Path, key: &str, parent: Option<&str>, kind: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut data = match std::fs::read(path) {
        Ok(b) => Py::loads(&b).unwrap_or_else(|_| Py::dict()),
        Err(_) => Py::dict(),
    };
    if !data.is_dict() {
        return;
    }
    data.set(
        key,
        Py::dict()
            .with("parent_id", parent.map(str::to_string))
            .with("spawned_at", logs::unix_now() as i64)
            .with("spawn_kind", kind),
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, sort_keys(&data).dumps(Style::Indent(2)));
}

// ── /api/resume ────────────────────────────────────────────────────────

/// Markers Claude's TUI draws once loaded (`_RESUME_READY_MARKERS`).
const READY_MARKERS: [&str; 6] = [
    "⏵⏵",
    "? for shortcuts",
    "auto mode",
    "plan mode",
    "bypass permissions",
    "/clear to save",
];

static CALIB_LOCK: Mutex<()> = Mutex::new(());

fn calib_file(home: &Path) -> PathBuf {
    home.join(".claude/resume_calibration.json")
}

fn num(v: Option<&Py>) -> Option<f64> {
    match v? {
        Py::Int(i) => Some(*i as f64),
        Py::Float(f) => Some(*f),
        Py::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    }
}

/// `_resume_estimate_ms`: median calibrated bytes/ms, default ≈5 MB/s,
/// clamped to [1.5 s, 90 s].
fn resume_estimate_ms(home: &Path, jsonl_bytes: u64) -> i64 {
    let mut rate = 5.0 * 1024.0 * 1024.0 / 1000.0;
    let rates = (|| -> Option<Vec<f64>> {
        let _g = CALIB_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let data = Py::loads(&std::fs::read(calib_file(home)).ok()?).ok()?;
        let Py::List(recs) = data.get("records")? else {
            return None;
        };
        let mut rates = Vec::new();
        for r in recs {
            if !r.is_dict() {
                return None;
            }
            if !r.get("ms").is_some_and(Py::truthy) {
                continue;
            }
            let b = num(r.get("bytes"))?;
            let ms = num(r.get("ms"))?;
            rates.push(b / ms.max(1.0));
        }
        Some(rates)
    })();
    if let Some(mut rates) = rates {
        if !rates.is_empty() {
            rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            rate = rates[rates.len() / 2];
        }
    }
    let est = jsonl_bytes as f64 / rate.max(1.0);
    est.clamp(1500.0, 90_000.0) as i64
}

/// `_record_resume_duration`: keep the last 50 (bytes, ms) pairs.
fn record_resume_duration(home: &Path, jsonl_bytes: u64, ms: u64) {
    let _g = CALIB_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = calib_file(home);
    let mut data = std::fs::read(&path)
        .ok()
        .and_then(|b| Py::loads(&b).ok())
        .unwrap_or_else(|| Py::dict().with("records", Py::List(vec![])));
    let Some(Py::List(recs)) = data.get("records") else {
        return;
    };
    let mut recs = recs.clone();
    recs.push(Py::dict().with("bytes", jsonl_bytes).with("ms", ms));
    let start = recs.len().saturating_sub(50);
    data.set("records", Py::List(recs.split_off(start)));
    let _ = std::fs::write(&path, data.dumps(Style::Default));
}

/// First `<projects>/*/<id>.jsonl` (glob order: directory order).
fn find_session_jsonl(projects: &Path, session_id: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(projects).ok()?.flatten() {
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let p = e.path().join(format!("{session_id}.jsonl"));
        if std::fs::symlink_metadata(&p).is_ok() {
            return Some(p);
        }
    }
    None
}

/// `_resolve_session_cwd`: the first `cwd` in the first 51 records.
fn resolve_session_cwd(projects: &Path, session_id: &str, home: &Path) -> Option<String> {
    if session_id.is_empty() || !projects.exists() {
        return None;
    }
    let path = find_session_jsonl(projects, session_id)?;
    let data = std::fs::read(path).ok()?;
    for (i, raw) in data.split(|b| *b == b'\n').enumerate() {
        if i > 50 {
            break;
        }
        let line = std::str::from_utf8(raw).ok()?;
        let line = strip(line);
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = Py::loads(line.as_bytes()) else {
            continue;
        };
        if !rec.is_dict() {
            return None; // rec.get raised → except → None
        }
        if let Some(Py::Str(cwd)) = rec.get("cwd") {
            if !cwd.is_empty() {
                let expanded = expanduser(cwd, home);
                return Some(if Path::new(&expanded).is_dir() {
                    expanded
                } else {
                    cwd.clone()
                });
            }
        }
    }
    None
}

/// `_haiku_title_for` ("" when unclassified).
fn haiku_title_for(home: &Path, session_id: &str) -> Result<String, Fail> {
    let at = "_haiku_title_for";
    let Some(cache) = std::fs::read(home.join(".claude/session_classifications.json"))
        .ok()
        .and_then(|b| Py::loads(&b).ok())
    else {
        return Ok(String::new());
    };
    if !cache.is_dict() {
        return Err(no_get(&cache, at));
    }
    let entry = match cache.get(session_id) {
        Some(e) if e.truthy() => e.clone(),
        _ => Py::dict(),
    };
    if !entry.is_dict() {
        return Err(no_get(&entry, at));
    }
    let title = strip(&get_str_or_empty(&entry, "title", at)?).to_string();
    let drift = strip(&get_str_or_empty(&entry, "drift_topic", at)?).to_string();
    Ok(if !title.is_empty() && !drift.is_empty() {
        format!("{title} → {drift}")
    } else {
        title
    })
}

/// Resume ids are Claude / Codex session ids (UUIDs, `agent-…`). A strict
/// charset means an id can never act as a glob or a path (INC-047).
fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    Claude,
    Codex,
}

/// Which agent wrote the session (INC-012): a Claude transcript under
/// `~/.claude/projects` wins, else a Codex rollout under `~/.codex/sessions`.
fn locate_transcript(home: &Path, id: &str) -> Option<(Origin, PathBuf)> {
    if let Some(p) = find_session_jsonl(&home.join(".claude/projects"), id) {
        return Some((Origin::Claude, p));
    }
    find_codex_rollout(&home.join(".codex/sessions"), id).map(|p| (Origin::Codex, p))
}

/// `<root>/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`, newest directory first.
fn find_codex_rollout(root: &Path, id: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: u8, suffix: &str) -> Option<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort_unstable_by(|a, b| b.cmp(a));
        for p in entries {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            let Some(name) = name.filter(|n| !n.starts_with('.')) else {
                continue;
            };
            if p.is_dir() {
                if depth < 3 {
                    if let Some(f) = walk(&p, depth + 1, suffix) {
                        return Some(f);
                    }
                }
            } else if name.starts_with("rollout-") && name.ends_with(suffix) {
                return Some(p);
            }
        }
        None
    }
    walk(root, 0, &format!("-{id}.jsonl"))
}

/// The rollout's `session_meta.payload.cwd`, when that directory exists.
fn codex_cwd(rollout: &Path, home: &Path) -> Option<String> {
    use std::io::{BufRead, Read};
    let mut line = Vec::new();
    std::io::BufReader::new(std::fs::File::open(rollout).ok()?.take(1 << 20))
        .read_until(b'\n', &mut line)
        .ok()?;
    let rec = Py::loads(&line).ok()?;
    let cwd = rec.get("payload")?.get("cwd")?.as_str()?;
    let cwd = expanduser(cwd, home);
    Path::new(&cwd).is_dir().then_some(cwd)
}

/// What an existing tmux session named for this id holds.
#[derive(Debug, PartialEq, Eq)]
enum PaneState {
    Missing,
    /// Something runs in it (the agent, or anything else): reuse, send nothing.
    Busy,
    /// Only a shell at its prompt (the agent exited), or a dead pane.
    Idle,
}

const SHELLS: [&str; 9] = [
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "nu",
];

/// Idle means: the pane's first process is a shell, it owns the terminal's
/// foreground, and it has no children. Anything unclear counts as busy, so
/// the only pane ever respawned is one with nothing in it to lose.
async fn pane_state(name: &str) -> PaneState {
    if !cmd::run(&["tmux", "has-session", "-t", &target::session(name)], 3.0)
        .await
        .ok()
    {
        return PaneState::Missing;
    }
    let o = cmd::run(
        &[
            "tmux",
            "display-message",
            "-p",
            "-t",
            &target::pane(name),
            "#{pane_pid} #{pane_dead}",
        ],
        3.0,
    )
    .await;
    let fields: Vec<&str> = strip(&o.stdout).split(' ').collect();
    let (pid, dead) = match (o.ok(), fields.as_slice()) {
        (true, [pid, dead]) => (pid.parse::<i64>().ok(), *dead == "1"),
        _ => return PaneState::Busy,
    };
    if dead {
        return PaneState::Idle;
    }
    let Some(pid) = pid else {
        return PaneState::Busy;
    };
    let ps = cmd::run(&["ps", "-A", "-o", "pid=,ppid=,tpgid=,comm="], 5.0).await;
    if !ps.ok() {
        return PaneState::Busy;
    }
    let mut own: Option<(i64, String)> = None;
    for line in ps.stdout.lines() {
        let mut it = line.split_whitespace();
        let (Some(p), Some(pp), Some(tp)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(p), Ok(pp), Ok(tp)) = (p.parse::<i64>(), pp.parse::<i64>(), tp.parse::<i64>())
        else {
            continue;
        };
        if pp == pid {
            return PaneState::Busy; // the shell has a child
        }
        if p == pid {
            own = Some((tp, it.collect::<Vec<_>>().join(" ")));
        }
    }
    match own {
        Some((tpgid, comm)) if tpgid == pid => {
            let base = comm
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches('-');
            if SHELLS.contains(&base) {
                PaneState::Idle
            } else {
                PaneState::Busy
            }
        }
        _ => PaneState::Busy,
    }
}

/// The shell line that resumes the session.
fn resume_line(origin: Origin, sid: &str, cwd: Option<&str>) -> String {
    let q = sh_quote(sid);
    let run = match origin {
        Origin::Claude => format!("claude --resume {q}"),
        Origin::Codex => format!("codex resume {q}"),
    };
    match cwd {
        Some(c) => format!("cd {} && {run}", sh_quote(c)),
        None => run,
    }
}

/// `POST /api/resume {session_id}`.
///
/// 1. The id must look like a session id (400) and name a transcript on
///    disk (404 otherwise; nothing is spawned).
/// 2. The origin comes from the transcript: `claude --resume <id>` in
///    `claude-<short>`, or `codex resume <id>` in `codex-<short>`.
/// 3. Already running: a pane the live mapping (process walk, Codex tty /
///    app-server cwd mapping — the same map `/api/local-sessions` uses)
///    ties to this id, or a pane with this id's name that has something
///    running in it, is returned as is (`reused`) and nothing is typed.
/// 4. A Codex session running outside tmux → 409 (a second `codex resume`
///    would write to the same thread).
/// 5. A pane with this id's name holding only an idle shell (the agent
///    exited — e.g. the `claude-<codex id>` panes Python's origin-blind
///    resume left behind) is respawned (`respawn-pane -k`, which only kills
///    that idle shell) and the command started in it (`respawned`). Python
///    returned such a pane silently and the phone opened a bare prompt.
/// 6. Otherwise a new session.
///
/// Claude responses and events keep Python's shape; Codex ones add
/// `"origin": "codex"`.
async fn resume(state: &Arc<State>, body: Py, h: &HeaderMap) -> HResult {
    let session_id = strip(Fields(&body).str_or("session_id", "")?).to_string();
    if session_id.is_empty() {
        return Ok(err_json(StatusCode::BAD_REQUEST, "session_id required", h));
    }
    if !is_valid_session_id(&session_id) {
        return Ok(err_json(StatusCode::BAD_REQUEST, "invalid session_id", h));
    }
    let located = {
        let (hm, sid) = (state.home.clone(), session_id.clone());
        tokio::task::spawn_blocking(move || locate_transcript(&hm, &sid))
            .await
            .unwrap_or(None)
    };
    let Some((origin, transcript)) = located else {
        return Ok(err_json(StatusCode::NOT_FOUND, "unknown session", h));
    };
    let short = head(session_id.split('-').next().unwrap_or(""), 8);
    let names = match origin {
        Origin::Claude => {
            let prefix = session_prefix();
            vec![
                format!("{prefix}{short}"),
                format!("{prefix}resume-{short}"),
            ]
        }
        Origin::Codex => vec![format!("codex-{short}")],
    };

    // 3. already running
    let mapped = {
        let (hm, claude, sid) = (
            state.home.clone(),
            state.dirs.claude.clone(),
            session_id.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let info = baton_core::tmuxmap::detect(&hm, &claude);
            info.tmux_for
                .get(&sid)
                .filter(|_| !info.zombies.contains(&sid))
                .cloned()
        })
        .await
        .unwrap_or(None)
    };
    let mut idle: Option<String> = None;
    for name in mapped.iter().chain(names.iter()) {
        match pane_state(name).await {
            PaneState::Busy => return Ok(resume_reused(state, name, &session_id, origin, h)),
            PaneState::Idle if names.contains(name) && idle.is_none() => idle = Some(name.clone()),
            _ => {}
        }
    }

    // 4. a Codex CLI on a terminal outside tmux
    if origin == Origin::Codex {
        let sid = session_id.clone();
        let tty = tokio::task::spawn_blocking(move || {
            baton_core::tmuxmap::codex_tty_map()
                .into_iter()
                .find(|(s, _)| *s == sid)
                .map(|(_, t)| t)
        })
        .await
        .unwrap_or(None);
        if let Some(tty) = tty {
            return Ok(send_json(
                StatusCode::CONFLICT,
                &Py::dict()
                    .with("error", "codex session is running outside tmux")
                    .with("tty", tty),
                h,
            ));
        }
    }

    let cwd = match origin {
        Origin::Claude => {
            let (p, s, hm) = (
                state.home.join(".claude/projects"),
                session_id.clone(),
                state.home.clone(),
            );
            tokio::task::spawn_blocking(move || resolve_session_cwd(&p, &s, &hm))
                .await
                .unwrap_or(None)
        }
        Origin::Codex => codex_cwd(&transcript, &state.home),
    };

    // 5. / 6. start the agent: in the idle pane, or in a new session
    let respawned = idle.is_some();
    let tmux_name = match idle {
        Some(name) => {
            let t = target::pane(&name);
            let mut args = vec!["tmux", "respawn-pane", "-k", "-t", t.as_str()];
            if let Some(c) = &cwd {
                args.extend(["-c", c.as_str()]);
            }
            let o = cmd::run(&args, 10.0).await;
            if o.rc != 0 {
                let msg = if o.stderr.is_empty() {
                    "tmux respawn-pane failed"
                } else {
                    &o.stderr
                };
                return Ok(err_json(StatusCode::INTERNAL_SERVER_ERROR, msg, h));
            }
            name
        }
        None => {
            let name = names[0].clone();
            let mut args = vec!["tmux", "new-session", "-d", "-s", name.as_str()];
            if let Some(c) = &cwd {
                args.extend(["-c", c.as_str()]);
            }
            let o = cmd::run(&args, 10.0).await;
            if o.rc != 0 {
                let msg = if o.stderr.is_empty() {
                    "tmux failed"
                } else {
                    &o.stderr
                };
                return Ok(err_json(StatusCode::INTERNAL_SERVER_ERROR, msg, h));
            }
            tmux_apply_options(&name).await;
            name
        }
    };
    let line = resume_line(origin, &session_id, cwd.as_deref());
    cmd::run(
        &[
            "tmux",
            "send-keys",
            "-t",
            &target::pane(&tmux_name),
            &line,
            "Enter",
        ],
        3.0,
    )
    .await;

    if !respawned || attached_ttys(&tmux_name).await.is_empty() {
        let haiku = haiku_title_for(&state.home, &session_id)?;
        let tab_title = if haiku.is_empty() {
            tmux_name.clone()
        } else {
            format!("{tmux_name} · {}", head(&haiku, 60).replace('"', "'"))
        };
        cmd::popen(&[
            "osascript",
            "-e",
            &osa_open_attached(&tmux_name, &tab_title),
        ]);
    }

    let jsonl_bytes = std::fs::metadata(&transcript).map(|m| m.len()).unwrap_or(0);
    let est_ms = resume_estimate_ms(&state.home, jsonl_bytes);
    tokio::spawn(watch_resume_ready(
        state.clone(),
        tmux_name.clone(),
        session_id.clone(),
        jsonl_bytes,
        origin,
    ));
    let mut ev = Py::dict()
        .with("type", "resume")
        .with("session", tmux_name.as_str())
        .with("claude_session_id", session_id.as_str());
    let mut resp = Py::dict()
        .with("tmux_session", tmux_name.as_str())
        .with("claude_session_id", session_id.as_str())
        .with("jsonl_bytes", jsonl_bytes)
        .with("estimated_ms", est_ms);
    if origin == Origin::Codex {
        ev.set("origin", Py::str("codex"));
        resp.set("origin", Py::str("codex"));
    }
    if respawned {
        ev.set("respawned", Py::Bool(true));
        resp.set("respawned", Py::Bool(true));
    }
    broadcast(state, ev);
    Ok(send_json(StatusCode::OK, &resp, h))
}

fn resume_reused(
    state: &State,
    tmux_name: &str,
    session_id: &str,
    origin: Origin,
    h: &HeaderMap,
) -> Response<Body> {
    let mut ev = Py::dict()
        .with("type", "resume")
        .with("session", tmux_name)
        .with("claude_session_id", session_id);
    let mut resp = Py::dict()
        .with("tmux_session", tmux_name)
        .with("claude_session_id", session_id);
    if origin == Origin::Codex {
        ev.set("origin", Py::str("codex"));
        resp.set("origin", Py::str("codex"));
        resp.set("reused", Py::Bool(true));
    }
    ev.set("reused", Py::Bool(true));
    broadcast(state, ev);
    send_json(StatusCode::OK, &resp, h)
}

/// Codex's TUI footer once a resumed thread is loaded.
const CODEX_READY_MARKERS: [&str; 3] = ["? for shortcuts", "context left", "⏎ send"];

/// `_watch_resume_ready`: poll the pane until the agent's prompt UI appears
/// (≤180 s), record the duration for calibration (Claude only — the
/// estimate is Claude's load rate), broadcast resume-ready.
async fn watch_resume_ready(
    state: Arc<State>,
    tmux_name: String,
    sid: String,
    bytes: u64,
    origin: Origin,
) {
    let markers: &[&str] = match origin {
        Origin::Claude => &READY_MARKERS,
        Origin::Codex => &CODEX_READY_MARKERS,
    };
    let t = target::pane(&tmux_name);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(180) {
        let o = cmd::run(&["tmux", "capture-pane", "-t", &t, "-p", "-S", "0"], 3.0).await;
        if o.rc != 0 {
            return; // pane gone
        }
        if markers.iter().any(|m| o.stdout.contains(m)) {
            let ms = t0.elapsed().as_millis() as u64;
            if origin == Origin::Claude {
                let home = state.home.clone();
                let _ =
                    tokio::task::spawn_blocking(move || record_resume_duration(&home, bytes, ms))
                        .await;
            }
            let mut ev = Py::dict()
                .with("type", "resume-ready")
                .with("session", tmux_name.as_str())
                .with("claude_session_id", sid.as_str())
                .with("duration_ms", ms);
            if origin == Origin::Codex {
                ev.set("origin", Py::str("codex"));
            }
            broadcast(&state, ev);
            return;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

// ── /api/kill-session, /api/respond ────────────────────────────────────

async fn kill(state: &Arc<State>, body: Py, h: &HeaderMap) -> HResult {
    let session = strip(Fields(&body).str_or("session", "")?).to_string();
    if session.is_empty() {
        return Ok(err_json(StatusCode::BAD_REQUEST, "session required", h));
    }
    if !target::is_safe(&session) {
        return Ok(err_json(StatusCode::BAD_REQUEST, "invalid session name", h));
    }
    let o = cmd::run(
        &["tmux", "kill-session", "-t", &target::session(&session)],
        5.0,
    )
    .await;
    let mut already_gone = false;
    if o.rc != 0 {
        // Also "no server running" (INC-043): nothing left to kill.
        if target::is_missing(&o.stderr) {
            already_gone = true;
        } else {
            let msg = if o.stderr.is_empty() {
                "kill-session failed"
            } else {
                &o.stderr
            };
            return Ok(err_json(StatusCode::INTERNAL_SERVER_ERROR, msg, h));
        }
    }
    let osa = format!(
        "tell application \"Terminal\"\n  set toClose to {{}}\n  repeat with w in windows\n    repeat with t in tabs of w\n      try\n        if custom title of t contains \"{session}\" then\n          set end of toClose to w\n          exit repeat\n        end if\n      end try\n    end repeat\n  end repeat\n  repeat with w in toClose\n    close w saving no\n  end repeat\nend tell"
    );
    cmd::popen(&["osascript", "-e", &osa]);
    broadcast(
        state,
        Py::dict()
            .with("type", "kill")
            .with("session", session.as_str()),
    );
    Ok(send_json(
        StatusCode::OK,
        &Py::dict()
            .with("ok", true)
            .with("already_gone", already_gone),
        h,
    ))
}

async fn respond(state: &Arc<State>, body: Py, h: &HeaderMap) -> HResult {
    let f = Fields(&body);
    let s = f.str_or("session", "")?;
    let k = f.str_or("key", "y")?;
    if s.is_empty() {
        return Ok(err_json(StatusCode::BAD_REQUEST, "session required", h));
    }
    if !target::is_safe(s) {
        return Ok(err_json(StatusCode::BAD_REQUEST, "invalid session name", h));
    }
    let o = cmd::run(
        &[
            "tmux",
            "send-keys",
            "-t",
            &target::pane(s),
            &target::literal(k),
            "Enter",
        ],
        10.0,
    )
    .await;
    if o.rc != 0 && target::is_missing(&o.stderr) {
        return Ok(err_json(StatusCode::NOT_FOUND, "no such session", h));
    }
    focus_terminal_tab(s).await;
    broadcast(
        state,
        Py::dict()
            .with("type", "respond")
            .with("session", s)
            .with("key", k),
    );
    Ok(send_json(StatusCode::OK, &Py::dict().with("ok", true), h))
}

/// ttys of the terminals attached to the session.
async fn attached_ttys(name: &str) -> Vec<String> {
    let o = cmd::run(
        &[
            "tmux",
            "list-clients",
            "-t",
            &target::session(name),
            "-F",
            "#{client_tty}",
        ],
        2.0,
    )
    .await;
    if o.rc != 0 {
        return Vec::new();
    }
    strip(&o.stdout)
        .split('\n')
        .map(|t| strip(t).to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// `_focus_terminal_tab`: select the Terminal tab attached to the session
/// (matched by client tty), without activating Terminal.
async fn focus_terminal_tab(name: &str) {
    if name.is_empty() {
        return;
    }
    let tty = attached_ttys(name)
        .await
        .into_iter()
        .next()
        .unwrap_or_default();
    if tty.is_empty() {
        return;
    }
    let osa = format!(
        "tell application \"Terminal\"\n  repeat with w in windows\n    repeat with t in tabs of w\n      try\n        if (tty of t) is \"{tty}\" then\n          set selected of t to true\n          set index of w to 1\n          return\n        end if\n      end try\n    end repeat\n  end repeat\nend tell"
    );
    cmd::popen(&["osascript", "-e", &osa]);
}

// ── /api/hook-event, /api/register-device ──────────────────────────────

fn hook_event(state: &Arc<State>, mut body: Py, h: &HeaderMap) -> HResult {
    body.set("timestamp", Py::Float(logs::unix_now()));
    broadcast(state, body);
    Ok(send_json(StatusCode::OK, &Py::dict().with("ok", true), h))
}

fn devices_file(home: &Path) -> PathBuf {
    match std::env::var_os("BATON_DEVICES_FILE") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home.join(".config/baton/devices.json"),
    }
}

fn register_device(state: &Arc<State>, body: Py, h: &HeaderMap) -> HResult {
    let token = strip(Fields(&body).str_or("token", "")?).to_string();
    if token.is_empty() {
        return Ok(err_json(StatusCode::BAD_REQUEST, "token required", h));
    }
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let path = devices_file(&state.home);
    let res = (|| -> Result<usize, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut devices = if path.exists() {
            std::fs::read(&path)
                .ok()
                .and_then(|b| Py::loads(&b).ok())
                .unwrap_or_else(Py::dict)
        } else {
            Py::dict()
        };
        match &devices {
            Py::Dict(_) => {}
            Py::List(_) => return Err("list indices must be integers or slices, not str".into()),
            other => {
                return Err(format!(
                    "'{}' object does not support item assignment",
                    other.type_name()
                ))
            }
        }
        devices.set(
            &token,
            Py::dict()
                .with("last_seen", Py::Float(logs::unix_now()))
                .with(
                    "platform",
                    body.get("platform").cloned().unwrap_or(Py::str("ios")),
                )
                .with(
                    "bundle_id",
                    body.get("bundle_id").cloned().unwrap_or(Py::str("")),
                ),
        );
        std::fs::write(&path, devices.dumps(Style::Indent(2))).map_err(|e| e.to_string())?;
        Ok(match &devices {
            Py::Dict(d) => d.len(),
            _ => 0,
        })
    })();
    Ok(match res {
        Ok(n) => send_json(
            StatusCode::OK,
            &Py::dict().with("ok", true).with("devices", n),
            h,
        ),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e, h),
    })
}

// ── /api/paste-image ───────────────────────────────────────────────────

async fn paste_image(state: &Arc<State>, req: Request<Incoming>, h: &HeaderMap) -> HResult {
    let n = http::content_length(h)?;
    if n == 0 || n > 25 * 1024 * 1024 {
        return Ok(err_json(
            StatusCode::BAD_REQUEST,
            "image required (<=25MB)",
            h,
        ));
    }
    let mut data = http::body_bytes(req).await?.to_vec();
    data.truncate(n as usize);
    let ctype = h
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| strip(&s.to_lowercase()).to_string())
        .unwrap_or_default();
    let ext = match ctype.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "image/heif" => "heif",
        _ => "png",
    };
    let dir = state.home.join(".baton/pastes");
    std::fs::create_dir_all(&dir).map_err(|e| fail("OSError", e.to_string(), "paste-image"))?;
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let target = dir.join(format!("{date}_{}.{ext}", logs::rand_hex(8)));
    std::fs::write(&target, &data).map_err(|e| fail("OSError", e.to_string(), "paste-image"))?;
    Ok(send_json(
        StatusCode::OK,
        &Py::dict()
            .with("path", target.to_string_lossy().as_ref())
            .with("bytes", data.len()),
        h,
    ))
}

// ── /api/voice ─────────────────────────────────────────────────────────

/// Env var, else the `~/.claude-sessions/config` value the Python server
/// gets when `cs server` sources that file.
fn env_or_config(home: &Path, name: &str) -> String {
    if let Ok(v) = std::env::var(name) {
        return v;
    }
    let cfg = std::fs::read_to_string(home.join(".claude-sessions/config")).unwrap_or_default();
    let pfx = format!("{name}=");
    cfg.lines()
        .map(|l| l.trim().trim_start_matches("export ").trim())
        .filter_map(|l| l.strip_prefix(pfx.as_str()))
        .next_back()
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .unwrap_or_default()
}

/// What `transcribe` produced.
enum Transcript {
    Text(String),
    /// No backend could run (INC-039) → 503.
    Unavailable,
    /// The configured API answered with an error → 502, nothing typed.
    Upstream(String),
}

const NO_BACKEND: &str = "no transcription backend";

async fn voice(state: &Arc<State>, req: Request<Incoming>, h: &HeaderMap) -> HResult {
    let n = http::content_length(h)?;
    let session = h
        .get("x-session")
        .and_then(|v| v.to_str().ok())
        .map(|s| strip(s).to_string())
        .unwrap_or_default();
    if !session.is_empty() && !target::is_safe(&session) {
        return Ok(err_json(StatusCode::BAD_REQUEST, "invalid session name", h));
    }
    let mut data = http::body_bytes(req).await?.to_vec();
    data.truncate(n as usize);
    let tmp = std::env::temp_dir().join(format!("tmp{}.wav", logs::rand_hex(8)));
    std::fs::write(&tmp, &data).map_err(|e| fail("OSError", e.to_string(), "voice"))?;
    let res = transcribe(&state.home, &tmp).await;
    let _ = std::fs::remove_file(&tmp);
    let text = match res? {
        Transcript::Text(t) => t,
        Transcript::Unavailable => {
            return Ok(err_json(StatusCode::SERVICE_UNAVAILABLE, NO_BACKEND, h))
        }
        Transcript::Upstream(e) => return Ok(err_json(StatusCode::BAD_GATEWAY, &e, h)),
    };
    let mut sent_to = Py::None;
    if !session.is_empty() && !text.is_empty() {
        let t = target::pane(&session);
        let o = cmd::run(
            &["tmux", "send-keys", "-t", &t, "-l", &target::literal(&text)],
            10.0,
        )
        .await;
        if o.ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cmd::run(&["tmux", "send-keys", "-t", &t, "Enter"], 10.0).await;
            sent_to = Py::Str(session);
        }
    } else if !session.is_empty() {
        sent_to = Py::Str(session);
    }
    Ok(send_json(
        StatusCode::OK,
        &Py::dict().with("text", text).with("sent_to", sent_to),
        h,
    ))
}

async fn whisper_compatible(
    audio: &str,
    url: &str,
    key: &str,
    model: &str,
) -> Result<String, String> {
    let auth = format!("Authorization: Bearer {key}");
    let file = format!("file=@{audio}");
    let model = format!("model={model}");
    let o = cmd::run(
        &[
            "curl", "-sS", "-X", "POST", url, "-H", &auth, "-F", &file, "-F", &model,
        ],
        30.0,
    )
    .await;
    if o.stderr == "timeout" && o.rc == 1 && o.stdout.is_empty() {
        return Err("Command timed out after 30 seconds".into());
    }
    if o.rc != 0 {
        return Err(format!("curl failed: {}", strip(&o.stderr)));
    }
    let data = Py::loads(o.stdout.as_bytes()).map_err(|e| e.to_string())?;
    if !data.is_dict() {
        return Err(format!(
            "argument of type '{}' is not iterable",
            data.type_name()
        ));
    }
    match data.get("text") {
        Some(t) => Ok(t.py_str()),
        None => Err(format!("unexpected response: {}", head(&o.stdout, 300))),
    }
}

/// `transcribe_audio`: Groq → OpenAI → whisper-cpp → mlx-whisper → whisper.
async fn transcribe(home: &Path, audio: &Path) -> Result<Transcript, Fail> {
    let audio_s = audio.to_string_lossy().into_owned();
    let size = std::fs::metadata(audio).map(|m| m.len()).unwrap_or(0);
    let mut dur: Option<f64> = None;
    if cmd::status(&["which", "ffprobe"]).await == Some(0) {
        let pr = cmd::run(
            &[
                "ffprobe",
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
                &audio_s,
            ],
            10.0,
        )
        .await;
        if !(pr.rc == 1 && pr.stderr == "timeout") {
            dur = strip(&pr.stdout).parse::<f64>().ok();
        }
    }
    eprintln!(
        "[voice] upload {size} bytes, duration {}s",
        dur.map(|d| crate::pyjson::float_repr((d * 100.0).round() / 100.0))
            .unwrap_or_else(|| "None".into())
    );
    if size < 4096 || dur.is_some_and(|d| d < 0.3) {
        let d = dur.map(|d| format!(", {d:.2}s")).unwrap_or_default();
        return Ok(Transcript::Text(format!(
            "Recording too short ({size} bytes{d}) — hold the mic button while speaking, then release"
        )));
    }
    let groq = env_or_config(home, "CS_GROQ_API_KEY");
    if !groq.is_empty() {
        match whisper_compatible(
            &audio_s,
            "https://api.groq.com/openai/v1/audio/transcriptions",
            &groq,
            "whisper-large-v3",
        )
        .await
        {
            Ok(t) => return Ok(Transcript::Text(t)),
            Err(e) => eprintln!("[voice] Groq error, falling through: {e}"),
        }
    }
    let openai = env_or_config(home, "CS_OPENAI_API_KEY");
    if !openai.is_empty() {
        // Python returned the error as the transcript (typed into the pane).
        return Ok(
            match whisper_compatible(
                &audio_s,
                "https://api.openai.com/v1/audio/transcriptions",
                &openai,
                "whisper-1",
            )
            .await
            {
                Ok(t) => Transcript::Text(t),
                Err(e) => Transcript::Upstream(format!("OpenAI API error: {e}")),
            },
        );
    }
    let wcpp = std::env::var("CS_WHISPER_CPP").unwrap_or_default();
    if !wcpp.is_empty() || cmd::status(&["which", "whisper-cpp"]).await == Some(0) {
        let c = if wcpp.is_empty() {
            "whisper-cpp"
        } else {
            &wcpp
        };
        let o = cmd::run(&[c, "-f", &audio_s, "--no-timestamps"], 60.0).await;
        return Ok(Transcript::Text(strip(&o.stdout).to_string()));
    }
    let mlx_py = std::env::var("CS_MLX_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    if Path::new(&mlx_py).exists()
        && cmd::status(&[&mlx_py, "-c", "import mlx_whisper"]).await == Some(0)
    {
        let code = "import sys, mlx_whisper; r = mlx_whisper.transcribe(sys.argv[1], path_or_hf_repo=sys.argv[2]); print(r['text'].strip())";
        let model = std::env::var("CS_MLX_WHISPER_MODEL")
            .unwrap_or_else(|_| "mlx-community/whisper-large-v3-mlx".into());
        let o = cmd::run(&[&mlx_py, "-c", code, &audio_s, &model], 180.0).await;
        if o.rc == 0 && !strip(&o.stdout).is_empty() {
            return Ok(Transcript::Text(strip(&o.stdout).to_string()));
        }
        let tail: String = {
            let ch: Vec<char> = o.stderr.chars().collect();
            ch[ch.len().saturating_sub(300)..].iter().collect()
        };
        eprintln!("[voice] mlx-whisper failed rc={}: {tail}", o.rc);
    }
    match cmd::status(&["whisper", "--help"]).await {
        // Python's subprocess.run raised here when `whisper` was missing → 500.
        None => Ok(Transcript::Unavailable),
        Some(0) => {
            let td = std::env::temp_dir().join(format!("tmp{}", logs::rand_hex(8)));
            let _ = std::fs::create_dir_all(&td);
            let tds = td.to_string_lossy().into_owned();
            cmd::run(
                &[
                    "whisper",
                    &audio_s,
                    "--model",
                    "base",
                    "--output_format",
                    "txt",
                    "--output_dir",
                    &tds,
                ],
                120.0,
            )
            .await;
            let txt = std::fs::read_dir(&td).ok().and_then(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .find(|p| p.to_string_lossy().ends_with(".txt"))
            });
            let text = txt.and_then(|p| std::fs::read_to_string(p).ok());
            let _ = std::fs::remove_dir_all(&td);
            // Python: the text "No Whisper backend available" as a 200.
            Ok(match text {
                Some(t) => Transcript::Text(strip(&t).to_string()),
                None => Transcript::Unavailable,
            })
        }
        Some(_) => Ok(Transcript::Unavailable),
    }
}

// ── /api/client-metrics ────────────────────────────────────────────────

fn client_metrics(body: Py, h: &HeaderMap) -> HResult {
    let records = match body.get("records") {
        Some(v) if v.truthy() => v.clone(),
        _ => Py::List(vec![]),
    };
    let Py::List(records) = records else {
        return Ok(err_json(
            StatusCode::BAD_REQUEST,
            "records must be a list",
            h,
        ));
    };
    let device = head(
        &body.get("device").cloned().unwrap_or(Py::str("")).py_str(),
        40,
    );
    let build = head(
        &body
            .get("app_build")
            .cloned()
            .unwrap_or(Py::str(""))
            .py_str(),
        40,
    );
    let now = logs::local_ts();
    let rel = logs::release().id.as_str();
    for r in records.iter().take(500) {
        let Py::Dict(items) = r else {
            continue;
        };
        let mut rec = Py::dict();
        for (k, v) in items.iter().take(40) {
            rec.set(
                k,
                match v {
                    Py::Str(s) => Py::Str(head(s, 200)),
                    other => other.clone(),
                },
            );
        }
        rec.set("rx_ts", Py::str(now.as_str()));
        rec.set("device", Py::str(device.as_str()));
        rec.set("app_build", Py::str(build.as_str()));
        rec.set("rel", Py::str(rel));
        logs::append(logs::CLIENT, &rec);
    }
    Ok(send_json(
        StatusCode::OK,
        &Py::dict()
            .with("ok", true)
            .with("stored", records.len().min(500)),
        h,
    ))
}

// ── /api/bench/* (dev only: `baton serve --dev`) ───────────────────────

static BENCH: Mutex<Vec<(String, Py)>> = Mutex::new(Vec::new());

fn created_of(j: &Py) -> String {
    j.get("created")
        .and_then(Py::as_str)
        .unwrap_or("")
        .to_string()
}

struct BenchJob {
    reps: i128,
    note: String,
    ports: Vec<Py>,
    tmux: String,
}

/// The fields of `POST /api/bench/job` (Python: `int(reps)`, `[int(p) for
/// p in ports]` — a bad value raised; here it's a 400).
fn bench_job_fields(body: &Py) -> Result<BenchJob, BadRequest> {
    let f = Fields(body);
    let reps = f.int("reps")?.unwrap_or(5).clamp(1, 50);
    let note = head(
        &body.get("note").cloned().unwrap_or(Py::str("")).py_str(),
        200,
    );
    let ports: Vec<Py> = match body.get("ports") {
        Some(Py::List(l)) => {
            let mut out = Vec::new();
            for p in l {
                let n = http::int_of(p)
                    .ok_or_else(|| BadRequest::new("field 'ports' must hold integers"))?;
                out.push(Py::Int(n));
            }
            out
        }
        Some(v) if v.truthy() => {
            return Err(BadRequest::new("field 'ports' must be an array"));
        }
        _ => vec![],
    };
    let ports: Vec<Py> = ports.into_iter().take(4).collect();
    let tmux = match body.get("tmux") {
        Some(v) if v.truthy() => head(&v.py_str(), 60),
        _ => String::new(),
    };
    Ok(BenchJob {
        reps,
        note,
        ports,
        tmux,
    })
}

/// Without `--dev` the queue stays in the upstream Python server; the body
/// is checked here first so a malformed one is a 400, not Python's 500.
async fn bench_proxy(state: &Arc<State>, req: Request<Incoming>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return BadRequest::new("could not read request body").response(&parts.headers),
    };
    let checked = http::parse_object(&bytes).and_then(|b| {
        if parts.uri.path() == "/api/bench/job" {
            bench_job_fields(&b).map(|_| ())
        } else {
            Ok(())
        }
    });
    match checked {
        Err(e) => e.response(&parts.headers),
        Ok(()) => state.upstream.forward_bytes(parts, bytes).await,
    }
}

fn bench_create(body: Py, h: &HeaderMap) -> HResult {
    let BenchJob {
        reps,
        note,
        ports,
        tmux,
    } = bench_job_fields(&body)?;
    let id = logs::rand_hex(8);
    let job = Py::dict()
        .with("id", id.as_str())
        .with("reps", Py::Int(reps))
        .with("note", note)
        .with("status", "pending")
        .with(
            "ports",
            if ports.is_empty() {
                Py::None
            } else {
                Py::List(ports)
            },
        )
        .with(
            "tmux",
            if tmux.is_empty() {
                Py::None
            } else {
                Py::Str(tmux)
            },
        )
        .with("created", logs::local_ts());
    BENCH
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((id, job.clone()));
    Ok(send_json(StatusCode::OK, &job, h))
}

fn bench_claim(peer: Option<String>, h: &HeaderMap) -> Response<Body> {
    let mut jobs = BENCH.lock().unwrap_or_else(|p| p.into_inner());
    let mut pending: Vec<usize> = (0..jobs.len())
        .filter(|&i| jobs[i].1.get("status") == Some(&Py::str("pending")))
        .collect();
    pending.sort_by_key(|&i| created_of(&jobs[i].1));
    let Some(&i) = pending.first() else {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(crate::http::full(Bytes::new()))
            .expect("204");
    };
    let job = &mut jobs[i].1;
    job.set("status", Py::str("running"));
    job.set("claimed", Py::str(logs::local_ts()));
    job.set("claimed_by", Py::str(peer.unwrap_or_else(|| "-".into())));
    send_json(StatusCode::OK, job, h)
}

fn bench_list(h: &HeaderMap) -> Response<Body> {
    let jobs = BENCH.lock().unwrap_or_else(|p| p.into_inner());
    let mut list: Vec<Py> = jobs.iter().map(|(_, j)| j.clone()).collect();
    list.sort_by_key(created_of);
    send_json(StatusCode::OK, &Py::List(list), h)
}

fn bench_done(body: Py, h: &HeaderMap) -> HResult {
    let id = body.get("id").cloned().unwrap_or(Py::str("")).py_str();
    let mut jobs = BENCH.lock().unwrap_or_else(|p| p.into_inner());
    let found = jobs.iter_mut().find(|(k, _)| *k == id);
    let ok = found.is_some();
    if let Some((_, job)) = found {
        let err = body.get("error").cloned().unwrap_or(Py::None);
        job.set(
            "status",
            Py::str(if err.truthy() { "failed" } else { "done" }),
        );
        job.set("error", err);
        job.set("finished", Py::str(logs::local_ts()));
    }
    Ok(send_json(StatusCode::OK, &Py::dict().with("ok", ok), h))
}

/// Dev tap on the daemon's own event bus (`data: <json>` per event), so
/// the parity harness can check what control broadcasts.
fn events_tap(state: &Arc<State>) -> Response<Body> {
    let mut rx = state.events.subscribe();
    let (tx, out) = tokio::sync::mpsc::channel::<Bytes>(64);
    tokio::spawn(async move {
        if tx.send(Bytes::from_static(b": tap\n\n")).await.is_err() {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(v) => {
                    let line = format!("data: {v}\n\n");
                    if tx.send(Bytes::from(line)).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("access-control-allow-origin", "*")
        .body(ChannelBody(out).boxed())
        .expect("sse")
}

struct ChannelBody(tokio::sync::mpsc::Receiver<Bytes>);

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|o| o.map(|b| Ok(hyper::body::Frame::data(b))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qs_like_python() {
        let q = parse_qs(Some("dir=%2Ftmp&q=&x&dir=second&a+b=c+d"));
        assert_eq!(q.get("dir").map(String::as_str), Some("/tmp"));
        assert!(!q.contains_key("q"));
        assert!(!q.contains_key("x"));
        assert_eq!(q.get("a b").map(String::as_str), Some("c d"));
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn estimate_and_calibration() {
        let dir = std::env::temp_dir().join(format!("baton-ctl-test-{}", logs::rand_hex(6)));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        // no history: 5 MB/s, clamped
        assert_eq!(resume_estimate_ms(&dir, 0), 1500);
        assert_eq!(resume_estimate_ms(&dir, 100 * 1024 * 1024), 20000);
        assert_eq!(resume_estimate_ms(&dir, u64::MAX / 2), 90000);
        record_resume_duration(&dir, 10_000_000, 1000);
        record_resume_duration(&dir, 30_000_000, 1000);
        record_resume_duration(&dir, 20_000_000, 1000);
        // median rate 20000 bytes/ms
        assert_eq!(resume_estimate_ms(&dir, 100_000_000), 5000);
        let text = std::fs::read_to_string(calib_file(&dir)).unwrap();
        assert!(text.starts_with("{\"records\": [{\"bytes\": 10000000, \"ms\": 1000}, "));
        for _ in 0..60 {
            record_resume_duration(&dir, 1, 1);
        }
        let v = Py::loads(&std::fs::read(calib_file(&dir)).unwrap()).unwrap();
        match v.get("records") {
            Some(Py::List(l)) => assert_eq!(l.len(), 50),
            _ => panic!("records"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lineage_file_format() {
        let dir = std::env::temp_dir().join(format!("baton-ctl-test-{}", logs::rand_hex(6)));
        let f = dir.join(".baton/session_lineage.json");
        record_spawn(&f, "claude-b", None, "ios");
        record_spawn(&f, "claude-a", Some("p"), "task");
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.starts_with("{\n  \"claude-a\": {\n    \"parent_id\": \"p\",\n    \"spawn_kind\": \"task\",\n    \"spawned_at\": "));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resume_ids_and_origin() {
        for ok in [
            "a1a1a1a1-a1a1-4a1a-8a1a-a1a1a1a1a1a1",
            "agent-abc_1",
            "deadbeef-0000",
        ] {
            assert!(is_valid_session_id(ok), "{ok}");
        }
        for bad in ["", "*", "../x", "[a-f]*", "a b", "-x", "a/b", "a.b", "a;b"] {
            assert!(!is_valid_session_id(bad), "{bad:?}");
        }
        assert!(!is_valid_session_id(&"a".repeat(129)));

        let dir = std::env::temp_dir().join(format!("baton-ctl-test-{}", logs::rand_hex(6)));
        let (cl, cx) = (
            "c1c1c1c1-0000-4000-8000-000000000001",
            "d2d2d2d2-0000-4000-8000-000000000002",
        );
        let proj = dir.join(".claude/projects/-p");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(format!("{cl}.jsonl")), "{}\n").unwrap();
        let day = dir.join(".codex/sessions/2026/09/27");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join(format!("rollout-2026-09-27T10-00-00-{cx}.jsonl"));
        std::fs::write(
            &rollout,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{cx}\",\"cwd\":\"{}\"}}}}\n",
                dir.display()
            ),
        )
        .unwrap();
        assert_eq!(
            locate_transcript(&dir, cl).map(|t| t.0),
            Some(Origin::Claude)
        );
        assert_eq!(
            locate_transcript(&dir, cx),
            Some((Origin::Codex, rollout.clone()))
        );
        assert_eq!(locate_transcript(&dir, "nope-0000"), None);
        assert_eq!(
            codex_cwd(&rollout, &dir),
            Some(dir.to_string_lossy().into_owned())
        );
        assert_eq!(
            resume_line(Origin::Codex, cx, Some("/p q")),
            format!("cd '/p q' && codex resume '{cx}'")
        );
        assert_eq!(
            resume_line(Origin::Claude, cl, None),
            format!("claude --resume '{cl}'")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cwd_resolution() {
        let dir = std::env::temp_dir().join(format!("baton-ctl-test-{}", logs::rand_hex(6)));
        let proj = dir.join("projects/-x");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-1.jsonl"),
            format!(
                "\n{{\"type\":\"x\"}}\nnot json\n{{\"cwd\":\"{}\"}}\n",
                dir.display()
            ),
        )
        .unwrap();
        assert_eq!(
            resolve_session_cwd(&dir.join("projects"), "sid-1", &dir),
            Some(dir.to_string_lossy().into_owned())
        );
        assert_eq!(
            resolve_session_cwd(&dir.join("projects"), "nope", &dir),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
