//! Group `tmux`: /api/pane-stream/{name}, /api/pane/{name}, /api/scroll,
//! /api/send, /api/send-key, /api/sessions, resize.
//! Parity reference: claude-session-manager/server/api_server.py
//! (`_stream_pane`, `_send_pane_snapshot`, `_maybe_resize_pane`,
//! `_await_pane_settle`, `get_sessions`, the POST handlers). Same bytes on
//! the wire; `text` holds the pure transforms. Intended divergences from
//! Python: every tmux target is exact (`=name`, INC-050), every name goes
//! through `tmux_target::is_safe` (INC-049), POST bodies go through the
//! shared parser (400 instead of 500, INC-041/045), and a missing pane is a
//! 404 on scroll / send / send-key (INC-031, INC-015).

mod sse;
pub mod text;

use super::Routed;
use crate::http;
use crate::proxy::Body;
use crate::pyjson::Py;
use crate::state::State;
use crate::tmux_target as target;
use baton_core::pyfmt::strip;
use bytes::Bytes;
use http::{BadRequest, Fields};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use text::J;
use tokio::sync::Notify;

pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
    let path = req.uri().path().to_string();
    match *req.method() {
        Method::GET => {
            if let Some(name) = path.strip_prefix("/api/pane-stream/") {
                return Ok(stream_pane(name, req.headers()).await);
            }
            if let Some(name) = path.strip_prefix("/api/pane/") {
                let history = http::query(req.uri())
                    .get("history")
                    .filter(|v| !v.is_empty()) // parse_qs drops blank values
                    .map(|v| text::py_int_str(v).unwrap_or(500))
                    .unwrap_or(500);
                return Ok(pane_snapshot(name, history, req.headers()).await);
            }
            if path == "/api/sessions" {
                let body = cached_get_sessions().await;
                return Ok(http::bytes_response(
                    StatusCode::OK,
                    "application/json",
                    body.to_vec(),
                    req.headers(),
                    None,
                ));
            }
            Err(req)
        }
        Method::POST if matches!(path.as_str(), "/api/scroll" | "/api/send" | "/api/send-key") => {
            let headers = req.headers().clone();
            let body = match http::read_object(req).await {
                Ok(b) => b,
                Err(e) => return Ok(e.response(&headers)),
            };
            let resp = match path.as_str() {
                "/api/scroll" => scroll(state, &body, &headers).await,
                "/api/send" => send(state, &body, &headers).await,
                _ => send_key(state, &body, &headers).await,
            };
            Ok(resp.unwrap_or_else(|e| e.response(&headers)))
        }
        _ => Err(req),
    }
}

// ── Shared per-session state ──────────────────────────────────────────────

/// `threading.Event` semantics: level-triggered flag plus wakeup.
struct Wake {
    flag: Mutex<bool>,
    notify: Notify,
}

impl Wake {
    fn set(&self) {
        *self.flag.lock().expect("wake") = true;
        self.notify.notify_waiters();
    }
    fn clear(&self) {
        *self.flag.lock().expect("wake") = false;
    }
    async fn wait(&self, timeout: Duration) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if *self.flag.lock().expect("wake") {
            return;
        }
        let _ = tokio::time::timeout(timeout, notified).await;
    }
}

#[derive(Default)]
struct Activity {
    /// `_session_activity_ts`
    ts: HashMap<String, f64>,
    /// `_session_wake_events`
    wakes: HashMap<String, Arc<Wake>>,
}

/// (cached at, (cols, rows))
type SizeEntry = (f64, (i64, i64));

struct Shared {
    activity: Mutex<Activity>,
    /// `_pane_size_cache`: session → (ts, (cols, rows))
    sizes: Mutex<HashMap<String, SizeEntry>>,
    /// `_get_sessions_cache` (encoded body); the async lock is held across
    /// the build, so a burst of callers coalesces into one tmux round.
    sessions: tokio::sync::Mutex<(f64, Option<Bytes>)>,
    log: Mutex<()>,
}

fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| Shared {
        activity: Mutex::new(Activity::default()),
        sizes: Mutex::new(HashMap::new()),
        sessions: tokio::sync::Mutex::new((0.0, None)),
        log: Mutex::new(()),
    })
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `_bump_session_activity` (+ the daemon's live-list input mark).
fn bump_session_activity(state: &State, name: &str) {
    if name.is_empty() {
        return;
    }
    state.mark_input();
    let mut a = shared().activity.lock().expect("activity");
    a.ts.insert(name.to_string(), now());
    if let Some(w) = a.wakes.get(name) {
        w.set();
    }
}

/// `_adaptive_pane_sleep`: 50 ms within 2 s of input, else 350 ms.
fn adaptive_pane_sleep(name: &str) -> Duration {
    let last = shared()
        .activity
        .lock()
        .expect("activity")
        .ts
        .get(name)
        .copied()
        .unwrap_or(0.0);
    Duration::from_millis(if now() - last < 2.0 { 50 } else { 350 })
}

/// `_session_wake_event`
fn session_wake_event(name: &str) -> Arc<Wake> {
    shared()
        .activity
        .lock()
        .expect("activity")
        .wakes
        .entry(name.to_string())
        .or_insert_with(|| {
            Arc::new(Wake {
                flag: Mutex::new(false),
                notify: Notify::new(),
            })
        })
        .clone()
}

// ── Subprocesses ──────────────────────────────────────────────────────────

/// `run_cmd(args, timeout)` → (stdout, stderr, returncode); timeout →
/// ("", "timeout", 1); missing binary → ("", "command not found: …", 1).
async fn run_cmd(args: &[&str], timeout: f64) -> (String, String, i32) {
    let mut cmd = tokio::process::Command::new(args[0]);
    cmd.args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return (String::new(), format!("command not found: {}", args[0]), 1),
    };
    match tokio::time::timeout(Duration::from_secs_f64(timeout), child.wait_with_output()).await {
        Ok(Ok(out)) => (
            text::decode_text(&out.stdout),
            text::decode_text(&out.stderr),
            out.status.code().unwrap_or(-1),
        ),
        Ok(Err(e)) => (String::new(), e.to_string(), 1),
        Err(_) => (String::new(), "timeout".into(), 1),
    }
}

/// Fire-and-forget `osascript -e script` (Python: `subprocess.Popen`).
fn spawn_osascript(script: &str) {
    let _ = std::process::Command::new("osascript")
        .args(["-e", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|mut c| {
            // Reap it off-thread so it doesn't linger as a zombie.
            std::thread::spawn(move || c.wait());
        });
}

/// `_pane_window_size`: (cols, rows) of the session's current window.
async fn pane_window_size(name: &str) -> Option<(i64, i64)> {
    let (out, _, _) = run_cmd(
        &[
            "tmux",
            "display-message",
            "-p",
            "-t",
            &target::pane(name),
            "#{window_width} #{window_height}",
        ],
        2.0,
    )
    .await;
    let parts: Vec<&str> = baton_core::pyfmt::split_ws(&out).collect();
    match parts.as_slice() {
        [w, h] => Some((text::py_int_str(w)? as i64, text::py_int_str(h)? as i64)),
        _ => None,
    }
}

const PANE_SIZE_TTL: f64 = 5.0;

/// `_pane_window_size_cached`: no tmux call per wheel burst.
async fn pane_window_size_cached(name: &str) -> Option<(i64, i64)> {
    if let Some(&(ts, size)) = shared().sizes.lock().expect("sizes").get(name) {
        if now() - ts < PANE_SIZE_TTL {
            return Some(size);
        }
    }
    let size = pane_window_size(name).await;
    if let Some(s) = size {
        shared()
            .sizes
            .lock()
            .expect("sizes")
            .insert(name.to_string(), (now(), s));
    }
    size
}

/// `_list_session_clients`: ttys of the Mac terminals attached.
async fn list_session_clients(name: &str) -> Vec<String> {
    let (out, _, rc) = run_cmd(
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
    if rc != 0 {
        return Vec::new();
    }
    strip(&out)
        .lines()
        .map(strip)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// `_resize_terminal_app_window`: AppleScript resize of the Terminal tab.
async fn resize_terminal_app_window(tty: &str, cols: i64, rows: i64) -> bool {
    let script = text::resize_terminal_script(tty, cols, rows);
    let (out, _, _) = run_cmd(&["osascript", "-e", &script], 3.0).await;
    strip(&out) == "ok"
}

/// `_resize_tmux_window`: resize, then back to `window-size latest`.
async fn resize_tmux_window(name: &str, cols: i64, rows: i64) {
    let (c, r) = (cols.to_string(), rows.to_string());
    let t = target::pane(name);
    run_cmd(
        &["tmux", "resize-window", "-t", &t, "-x", &c, "-y", &r],
        2.0,
    )
    .await;
    run_cmd(
        &["tmux", "set-option", "-t", &t, "window-size", "latest"],
        2.0,
    )
    .await;
}

/// `_maybe_resize_pane`: compares with the pane's ACTUAL size; returns the
/// target when a resize was started.
async fn maybe_resize_pane(name: &str, cols: i64, rows: i64) -> Option<(i64, i64)> {
    let t0 = now();
    let before = pane_window_size(name).await;
    if before == Some((cols, rows)) {
        return None;
    }
    let mac_clients = list_session_clients(name).await;
    let width_changed = before.is_none_or(|b| b.0 != cols);
    let mut action = None;
    if !mac_clients.is_empty() {
        if width_changed {
            if resize_terminal_app_window(&mac_clients[0], cols, rows).await {
                action = Some("terminal_app");
            } else {
                resize_tmux_window(name, cols, rows).await;
                action = Some("tmux_fallback");
            }
        }
    } else {
        resize_tmux_window(name, cols, rows).await;
        action = Some("tmux");
    }
    shared().sizes.lock().expect("sizes").remove(name);
    log_event(
        "pane_resize",
        vec![
            ("session", J::Str(name.into())),
            (
                "before",
                before.map_or(J::Null, |(c, r)| J::Arr(vec![J::Int(c), J::Int(r)])),
            ),
            ("target", J::Arr(vec![J::Int(cols), J::Int(rows)])),
            (
                "action",
                J::Str(action.unwrap_or("ignored_height_only").into()),
            ),
            ("mac_clients", J::Int(mac_clients.len() as i64)),
            ("ms", J::Int(((now() - t0) * 1000.0) as i64)),
        ],
    );
    action.map(|_| (cols, rows))
}

/// `_await_pane_settle`: wait (≤0.8 s) for the target size, then 60 ms
/// for the app's redraw. Returns ms waited.
async fn await_pane_settle(name: &str, target: (i64, i64)) -> i64 {
    let t0 = now();
    while now() - t0 < 0.8 {
        if pane_window_size(name).await == Some(target) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    ((now() - t0) * 1000.0) as i64
}

// ── Logs (same files and line format as the Python server) ────────────────

fn local_ts(t: f64) -> String {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r writes only into the tm we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

const REL: &str = concat!("baton-", env!("CARGO_PKG_VERSION"));

fn append_jsonl(file: &str, record: J) {
    use std::io::Write;
    let line = text::dumps_compact(&record) + "\n";
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = std::path::Path::new(&home).join("Library/Logs").join(file);
    let _g = shared().log.lock();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 50 * 1024 * 1024) {
        let _ = std::fs::rename(&path, path.with_file_name(format!("{file}.1")));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `_log_event(event, **fields)` → cs-server.events.jsonl
fn log_event(event: &'static str, fields: Vec<(&'static str, J)>) {
    let mut rec = vec![
        ("ts", J::Str(local_ts(now()))),
        ("event", J::Str(event.into())),
        ("rel", J::Str(REL.into())),
    ];
    rec.extend(fields);
    append_jsonl("cs-server.events.jsonl", J::Obj(rec));
}

/// The pane stream's one `_log_request("SSE", …#first-frame, …)` line.
fn log_first_frame(name: &str, started: f64) {
    let ms = ((now() - started) * 1000.0) as i64;
    let path: String = format!("/api/pane-stream/{name}#first-frame")
        .chars()
        .take(160)
        .collect();
    append_jsonl(
        "cs-server.requests.jsonl",
        J::Obj(vec![
            ("ts", J::Str(local_ts(started))),
            ("rid", J::Str(String::new())),
            ("crid", J::Null),
            ("m", J::Str("SSE".into())),
            ("path", J::Str(path)),
            ("st", J::Int(200)),
            ("ms", J::Int(ms)),
            ("bytes", J::Int(0)),
            ("gz", J::Null),
            ("client", J::Str(format!("first-frame-after-{ms}ms"))),
            ("rel", J::Str(REL.into())),
        ]),
    );
}

// ── Responses ─────────────────────────────────────────────────────────────

fn json(status: StatusCode, v: &J, headers: &HeaderMap) -> Response<Body> {
    http::bytes_response(
        status,
        "application/json",
        text::dumps(v).into_bytes(),
        headers,
        None,
    )
}

fn error(status: StatusCode, msg: &str, headers: &HeaderMap) -> Response<Body> {
    json(
        status,
        &J::Obj(vec![("error", J::Str(msg.into()))]),
        headers,
    )
}

fn ok(headers: &HeaderMap) -> Response<Body> {
    json(
        StatusCode::OK,
        &J::Obj(vec![("ok", J::Bool(true))]),
        headers,
    )
}

// ── GET /api/pane/{name} ──────────────────────────────────────────────────

async fn pane_snapshot(name: &str, history: i128, headers: &HeaderMap) -> Response<Body> {
    if !target::is_safe(name) {
        return error(StatusCode::BAD_REQUEST, "invalid session name", headers);
    }
    let t = target::pane(name);
    run_cmd(&["tmux", "send-keys", "-t", &t, "-X", "cancel"], 2.0).await;
    let start = format!("-{history}");
    let (out, err, rc) = run_cmd(
        &[
            "tmux",
            "capture-pane",
            "-t",
            &t,
            "-p",
            "-e",
            "-J",
            "-S",
            &start,
        ],
        5.0,
    )
    .await;
    if rc != 0 {
        if target::is_missing(&err) {
            return error(StatusCode::NOT_FOUND, "no such session", headers);
        }
        let msg = if err.is_empty() {
            "capture-pane failed"
        } else {
            &err
        };
        return error(StatusCode::NOT_FOUND, msg, headers);
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; charset=utf-8")
        .header("access-control-allow-origin", "*")
        .header("content-length", out.len())
        .body(http::full(out))
        .expect("pane response")
}

// ── GET /api/pane-stream/{name} ───────────────────────────────────────────

fn header_int(headers: &HeaderMap, name: &str) -> Option<Result<i128, ()>> {
    let v = headers.get(name)?;
    let s = v.to_str().unwrap_or("\u{0}");
    if s.is_empty() {
        return None; // falsy header → no resize
    }
    Some(text::py_int_str(s).ok_or(()))
}

async fn stream_pane(name: &str, headers: &HeaderMap) -> Response<Body> {
    if !target::is_safe(name) {
        return error(StatusCode::BAD_REQUEST, "invalid session name", headers);
    }
    if let (Some(c), Some(r)) = (
        header_int(headers, "x-pane-cols"),
        header_int(headers, "x-pane-rows"),
    ) {
        if let (Ok(c), Ok(r)) = (c, r) {
            let cols = c.clamp(20, 500) as i64;
            let rows = r.clamp(8, 200) as i64;
            if let Some(target) = maybe_resize_pane(name, cols, rows).await {
                // Hold the first frame until the pane is at the new size and
                // the app has redrawn (no intermediate SIGWINCH flicker).
                let ms = await_pane_settle(name, target).await;
                log_event(
                    "pane_settle",
                    vec![("session", J::Str(name.into())), ("ms", J::Int(ms))],
                );
            }
        }
    }
    // One-time copy-mode exit on connect (not repeated: it would yank the
    // Mac terminal back to the bottom every frame).
    run_cmd(
        &[
            "tmux",
            "send-keys",
            "-t",
            &target::pane(name),
            "-X",
            "cancel",
        ],
        2.0,
    )
    .await;

    let (tx, body) = sse::channel();
    tokio::spawn(frame_loop(name.to_string(), tx));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("access-control-allow-origin", "*")
        .header("connection", "close")
        .body(body.boxed())
        .expect("sse response")
}

/// The frame loop: one tmux call per frame (window size ; capture), a
/// `dims` event when the size changes, a frame when the pane changed, a
/// keepalive otherwise; then sleep 50/350 ms or until scroll/send wakes it.
async fn frame_loop(name: String, tx: tokio::sync::mpsc::Sender<Bytes>) {
    let wake = session_wake_event(&name);
    let mut last_payload: Option<String> = None;
    let mut last_dims: Option<String> = None;
    let mut first_frame_sent = false;
    let started = now();
    let t = target::pane(&name);
    loop {
        let (both, _, rc) = run_cmd(
            &[
                "tmux",
                "display-message",
                "-t",
                &t,
                "-p",
                "#{window_width}x#{window_height}",
                ";",
                "capture-pane",
                "-t",
                &t,
                "-p",
                "-e",
                "-J",
                "-S",
                "0",
            ],
            5.0,
        )
        .await;
        let (dims_line, captured) = both.split_once('\n').unwrap_or((both.as_str(), ""));
        let cur_dims = if rc == 0 { strip(dims_line) } else { "" };
        if !cur_dims.is_empty() && last_dims.as_deref() != Some(cur_dims) {
            let ev = format!("event: dims\ndata: {cur_dims}\n\n");
            if tx.send(Bytes::from(ev)).await.is_err() {
                return;
            }
            last_dims = Some(cur_dims.to_string());
        }
        if rc != 0 {
            let _ = tx
                .send(Bytes::from_static(b"event: error\ndata: pane gone\n\n"))
                .await;
            return;
        }
        let event = if last_payload.as_deref() != Some(captured) {
            if !first_frame_sent {
                first_frame_sent = true;
                log_first_frame(&name, started);
            }
            let ev = text::frame_event(captured);
            last_payload = Some(captured.to_string());
            Bytes::from(ev)
        } else {
            Bytes::from_static(b": keepalive\n\n")
        };
        if tx.send(event).await.is_err() {
            return; // client gone
        }
        wake.wait(adaptive_pane_sleep(&name)).await;
        wake.clear();
    }
}

// ── GET /api/sessions ─────────────────────────────────────────────────────

const GET_SESSIONS_TTL: f64 = 2.0;

/// The tmux session list as JSON for the `sessions_update` events (list
/// group's `/api/events`), from the same 2 s cache as `/api/sessions`.
pub async fn sessions_value() -> serde_json::Value {
    serde_json::from_slice(&cached_get_sessions().await).unwrap_or_else(|_| serde_json::json!([]))
}

/// `_cached_get_sessions`: single-flight, 2 s TTL.
async fn cached_get_sessions() -> Bytes {
    let mut cache = shared().sessions.lock().await;
    if let Some(b) = &cache.1 {
        if now() - cache.0 < GET_SESSIONS_TTL {
            return b.clone();
        }
    }
    let data = Bytes::from(text::dumps(&get_sessions().await));
    *cache = (now(), Some(data.clone()));
    data
}

fn marker() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(now().to_bits());
    format!("\x1eCSM-PANE-{:08x}\x1e", h.finish() as u32)
}

/// `_capture_panes`: every pane in ONE tmux invocation, split by a marker;
/// per-session fallback if a session vanished mid-batch.
async fn capture_panes(names: &[String]) -> Vec<String> {
    let marker = marker();
    let targets: Vec<String> = names.iter().map(|n| target::pane(n)).collect();
    let mut args: Vec<&str> = vec!["tmux"];
    for n in &targets {
        args.extend([
            "capture-pane",
            "-p",
            "-t",
            n,
            ";",
            "display-message",
            "-p",
            &marker,
            ";",
        ]);
    }
    args.pop();
    let (out, _, rc) = run_cmd(&args, 10.0).await;
    let sep = format!("{marker}\n");
    let chunks: Vec<&str> = out.split(sep.as_str()).collect();
    if rc == 0 && chunks.len() == names.len() + 1 {
        return chunks[..names.len()]
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    let mut res = Vec::with_capacity(names.len());
    for n in &targets {
        res.push(
            run_cmd(&["tmux", "capture-pane", "-t", n, "-p"], 10.0)
                .await
                .0,
        );
    }
    res
}

/// `get_sessions()`: tmux sessions sorted by name with status + preview.
async fn get_sessions() -> J {
    let (stdout, _, rc) = run_cmd(
        &[
            "tmux",
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_activity}",
        ],
        10.0,
    )
    .await;
    if rc != 0 {
        return J::Arr(Vec::new());
    }
    let mut activity: HashMap<String, i64> = HashMap::new();
    for line in strip(&stdout).split('\n') {
        let (name, act) = line.split_once('\t').unwrap_or((line, ""));
        if !name.is_empty() {
            let a = strip(act);
            let v = if !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()) {
                text::py_int_str(a).unwrap_or(0) as i64
            } else {
                0
            };
            activity.insert(name.to_string(), v);
        }
    }
    let mut ordered: Vec<String> = activity.keys().cloned().collect();
    ordered.sort();
    let panes = if ordered.is_empty() {
        Vec::new()
    } else {
        capture_panes(&ordered).await
    };
    let rows = ordered
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let content = text::last_lines(panes.get(i).map_or("", |s| s), 15);
            J::Obj(vec![
                ("index", J::Int(i as i64 + 1)),
                ("name", J::Str(name.clone())),
                ("status", J::Str(text::pane_status(&content).into())),
                ("preview", J::Str(text::tail_chars(&content, 800).into())),
                ("activity", J::Int(activity[name])),
            ])
        })
        .collect();
    J::Arr(rows)
}

// ── POST /api/scroll, /api/send, /api/send-key ────────────────────────────

type Handled = Result<Response<Body>, BadRequest>;

/// A tmux command on a target failed: 404 when the pane isn't there,
/// else a 500 carrying tmux's message.
fn tmux_failure(err: &str, fallback: &str, headers: &HeaderMap) -> Response<Body> {
    if target::is_missing(err) {
        return error(StatusCode::NOT_FOUND, "no such session", headers);
    }
    let msg = if err.is_empty() { fallback } else { err };
    error(StatusCode::INTERNAL_SERVER_ERROR, msg, headers)
}

/// `int(body.get("ticks", 1))` clamped to 1..50; unparsable → 1 (as the
/// Python handler's fallback); a non-finite float → 400.
fn ticks_of(v: Option<&Py>) -> Result<i64, BadRequest> {
    let n: i128 = match v {
        None | Some(Py::None) => 1,
        Some(Py::Bool(b)) => *b as i128,
        Some(Py::Int(i)) => *i,
        Some(Py::Float(f)) if f.is_finite() => f.trunc().clamp(-1e30, 1e30) as i128,
        Some(Py::Float(_)) => return Err(BadRequest::new("field 'ticks' must be finite")),
        Some(Py::Str(s)) => text::py_int_str(s).unwrap_or(1),
        Some(_) => 1,
    };
    Ok(n.clamp(1, 50) as i64)
}

/// The `session` of a tmux POST: a string naming a session (400 otherwise).
fn session_of<'a>(f: &Fields<'a>) -> Result<&'a str, BadRequest> {
    let session = strip(f.str_or("session", "")?);
    if session.is_empty() {
        return Err(BadRequest::new("session required"));
    }
    if !target::is_safe(session) {
        return Err(BadRequest::new("invalid session name"));
    }
    Ok(session)
}

async fn scroll(state: &State, body: &Py, headers: &HeaderMap) -> Handled {
    let f = Fields(body);
    let session = strip(f.str_or("session", "")?);
    let direction = strip(f.str_or("direction", "")?).to_lowercase();
    let ticks = ticks_of(body.get("ticks"))?;
    if session.is_empty() || !matches!(direction.as_str(), "up" | "down") {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "session + direction(up|down) required",
            headers,
        ));
    }
    if !target::is_safe(session) {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "invalid session name",
            headers,
        ));
    }
    let btn = if direction == "up" { 64 } else { 65 };
    // SGR wheel at the pane centre (Codex routes the wheel by position).
    let (cols, rows) = pane_window_size_cached(session).await.unwrap_or((80, 24));
    let one = format!(
        "\x1b[<{};{};{}M",
        btn,
        (cols.div_euclid(2)).max(1),
        (rows.div_euclid(2)).max(1)
    );
    let seq = one.repeat(ticks as usize);
    let (_, err, rc) = run_cmd(
        &[
            "tmux",
            "send-keys",
            "-t",
            &target::pane(session),
            "-l",
            &seq,
        ],
        3.0,
    )
    .await;
    if rc != 0 {
        return Ok(tmux_failure(&err, "scroll send failed", headers));
    }
    bump_session_activity(state, session);
    Ok(json(
        StatusCode::OK,
        &J::Obj(vec![
            ("ok", J::Bool(true)),
            ("ticks", J::Int(ticks)),
            ("direction", J::Str(direction)),
        ]),
        headers,
    ))
}

/// `_focus_terminal_tab`: select the Terminal tab attached to this session
/// (fire-and-forget; no-op without an attached client).
async fn focus_terminal_tab(name: String) {
    if name.is_empty() {
        return;
    }
    let (out, _, rc) = run_cmd(
        &[
            "tmux",
            "list-clients",
            "-t",
            &target::session(&name),
            "-F",
            "#{client_tty}",
        ],
        2.0,
    )
    .await;
    if rc != 0 {
        return;
    }
    let tty = strip(strip(&out).split('\n').next().unwrap_or(""));
    if tty.is_empty() {
        return;
    }
    spawn_osascript(&text::focus_tab_script(tty));
}

/// Claude Code's background-agent/shell picker has focus: "↑/↓ to select"
/// with the "⏺ main" row near the bottom (the list grows with each running
/// sub-agent). Narrow on purpose: an Esc on other screens would cancel a
/// permission prompt or interrupt a running turn.
async fn in_agent_picker(t: &str) -> bool {
    let (out, _, rc) = run_cmd(&["tmux", "capture-pane", "-p", "-t", t], 3.0).await;
    rc == 0 && picker_focused(&out)
}

fn picker_focused(screen: &str) -> bool {
    let tail: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(30)
        .collect();
    tail.iter().any(|l| l.contains("↑/↓ to select")) && tail.iter().any(|l| l.contains("⏺ main"))
}

async fn send(state: &State, body: &Py, headers: &HeaderMap) -> Handled {
    let f = Fields(body);
    let session = session_of(&f)?;
    let message = f.str_or("message", "")?;
    let t = target::pane(session);
    if in_agent_picker(&t).await {
        // Focus is on Claude Code's background-agent/shell list (↓ on an
        // empty prompt opens it): typed text and Enter would go to the
        // list and the message would be lost. Esc returns to the input.
        run_cmd(&["tmux", "send-keys", "-t", &t, "Escape"], 10.0).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        eprintln!(
            "send_refocus session={session} still_in_picker={}",
            in_agent_picker(&t).await
        );
    }
    // Text then Enter as two calls: a batched paste can swallow the Enter.
    let (_, err, rc) = run_cmd(
        &[
            "tmux",
            "send-keys",
            "-t",
            &t,
            "-l",
            &target::literal(message),
        ],
        10.0,
    )
    .await;
    if rc != 0 {
        return Ok(tmux_failure(&err, "send failed", headers));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    run_cmd(&["tmux", "send-keys", "-t", &t, "Enter"], 10.0).await;
    tokio::spawn(focus_terminal_tab(session.to_string()));
    bump_session_activity(state, session);
    let _ = state
        .events
        .send(serde_json::json!({"type": "send", "session": session}));
    Ok(ok(headers))
}

async fn send_key(state: &State, body: &Py, headers: &HeaderMap) -> Handled {
    let f = Fields(body);
    // `body.get("keys") or body.get("key")`
    let keys_field = if f.get("keys").is_some_and(Py::truthy) {
        "keys"
    } else {
        "key"
    };
    let session = strip(f.str_or("session", "")?);
    if session.is_empty() || !f.get(keys_field).is_some_and(Py::truthy) {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "session and keys required",
            headers,
        ));
    }
    if !target::is_safe(session) {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "invalid session name",
            headers,
        ));
    }
    let keys = f.strs(keys_field)?.unwrap_or_default();
    let t = target::pane(session);
    let keys: Vec<std::borrow::Cow<str>> = keys.iter().map(|k| target::literal(k)).collect();
    let mut args: Vec<&str> = vec!["tmux", "send-keys", "-t", &t];
    args.extend(keys.iter().map(|k| k.as_ref()));
    let (_, err, rc) = run_cmd(&args, 5.0).await;
    if rc != 0 {
        return Ok(tmux_failure(&err, "send-keys failed", headers));
    }
    bump_session_activity(state, session);
    Ok(ok(headers))
}

#[cfg(test)]
mod picker_tests {
    use super::picker_focused;

    #[test]
    fn focused_vs_listed() {
        let rows: String = (0..12)
            .map(|i| format!("  ◯ general-purpose  task{i}\n"))
            .collect();
        let focused = format!("x\n❯ \n────\n  ↑/↓ to select\n❯ ⏺ main\n{rows}");
        let listed = format!("x\n❯ \n────\n  ⏵⏵ auto mode on · esc to interrupt\n  ⏺ main\n{rows}");
        assert!(picker_focused(&focused));
        assert!(!picker_focused(&listed));
        assert!(!picker_focused("❯ 1. Yes\n  2. No\n  Esc to cancel\n"));
    }
}
