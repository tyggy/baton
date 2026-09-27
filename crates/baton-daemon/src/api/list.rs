//! Group `list`: grouping + /api/local-sessions, /api/search-index, /api/background, /api/events (sessions_delta).
//! Parity reference: claude-session-manager/server/api_server.py.
//!
//! The payload cache follows `_local_sessions_payload`: an entry younger
//! than 30 s is served as is; an older one is served immediately while a
//! background rebuild runs (stale-while-revalidate); only the very first
//! request waits for a build. The live watcher (`live_list_watcher`)
//! fingerprints the session files every 2 s and rebuilds at once on a
//! change, pushing `sessions_delta` to `/api/events` subscribers.
//!
//! During the migration `/api/events` also relays the events the Python
//! server still produces (hook events, resume progress, …) — everything
//! except the list deltas, which are computed here — and publishes
//! `sessions_update` from the upstream `/api/sessions` every 5 s (until the
//! tmux group serves that natively).

use super::Routed;
use crate::http;
use crate::proxy::Body;
use crate::state::State;
use baton_core::grouping::{self, Config, Inputs, Payload};
use baton_core::listbuild::{ListBuilder, Session};
use baton_core::pyjson;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

const LS_TTL: Duration = Duration::from_secs(30);
const LOAD_ALL_TTL: Duration = Duration::from_secs(60);
const LIVE_INTERVAL: Duration = Duration::from_secs(2);
const LIVE_MIN_REBUILD_GAP_MS: u64 = 3000;
const LIVE_IDLE_AFTER_MS: u64 = 60_000;
const LIVE_QUIET_AFTER_INPUT_MS: u64 = 3000;
const SESSIONS_UPDATE_EVERY: Duration = Duration::from_secs(5);
const KEEPALIVE: Duration = Duration::from_secs(15);
/// The Python server runs its optional background jobs (entity classifier,
/// title-drift check, tmux window retitles) off `/api/local-sessions` hits.
/// While those stay in Python, a native hit forwards a request upstream at
/// most this often so they keep running.
const PLUGIN_KICK_EVERY_MS: u64 = 300_000;
/// Event types computed natively — not relayed from the upstream stream.
const NATIVE_EVENTS: [&str; 2] = ["sessions_delta", "sessions_update"];

type Key = Option<String>;
/// Headless sessions per filter, with when they were listed.
type Headless = HashMap<Key, (Instant, Arc<Vec<Session>>)>;

struct Entry {
    built: Instant,
    bytes: Bytes,
    etag: String,
    search_bytes: Bytes,
    search_etag: String,
    gz: OnceLock<Bytes>,
    search_gz: OnceLock<Bytes>,
    rows: Vec<(String, Value, Vec<String>)>,
}

struct Engine {
    lb: ListBuilder,
    home: PathBuf,
    csm_dir: Mutex<(Option<PathBuf>, Option<Instant>)>,
    upstream: Upstream,
    build_lock: Mutex<()>,
    cache: Mutex<HashMap<Key, Arc<Entry>>>,
    inflight: Mutex<HashSet<Key>>,
    built: tokio::sync::Notify,
    headless: Mutex<Headless>,
    last_poll_ms: AtomicU64,
    last_kick_ms: AtomicU64,
}

static ENGINE: tokio::sync::OnceCell<Arc<Engine>> = tokio::sync::OnceCell::const_new();

pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
    let claimed = req.method() == hyper::Method::GET
        && matches!(
            req.uri().path(),
            "/api/local-sessions" | "/api/search-index" | "/api/background" | "/api/events"
        );
    if !claimed {
        return Err(req);
    }
    let eng = engine(state).await;
    let path = req.uri().path().to_string();
    let filter: Key = http::query(req.uri())
        .remove("project")
        .filter(|p| !p.is_empty());
    let h = req.headers();
    Ok(match path.as_str() {
        "/api/local-sessions" => {
            let now = State::now_ms();
            eng.last_poll_ms.store(now, Ordering::Relaxed);
            if now.saturating_sub(eng.last_kick_ms.load(Ordering::Relaxed)) > PLUGIN_KICK_EVERY_MS {
                eng.last_kick_ms.store(now, Ordering::Relaxed);
                let e = eng.clone();
                tokio::spawn(async move { e.upstream.drain("/api/local-sessions").await });
            }
            match eng.payload(filter).await {
                Some(e) => serve_payload(h, &e.bytes, &e.etag, &e.gz),
                None => error_500(h, "local-sessions payload build failed"),
            }
        }
        "/api/search-index" => match eng.payload(filter).await {
            Some(e) => serve_payload(h, &e.search_bytes, &e.search_etag, &e.search_gz),
            None => error_500(h, "local-sessions payload build failed"),
        },
        "/api/background" => {
            let hl = eng.background().await;
            let rows: Vec<Value> = hl
                .iter()
                .take(100)
                .map(|s| {
                    let v = &s.v;
                    json!({
                        "id": v["id"],
                        "title": baton_core::pyfmt::head(v["first_msg"].as_str().unwrap_or(""), 120),
                        "last_ts": v.get("last_ts").cloned().unwrap_or(Value::Null),
                        "msg_count": v.get("msg_count").cloned().unwrap_or(json!(0)),
                        "origin": v.get("origin").filter(|o| baton_core::pyfmt::truthy(o)).cloned().unwrap_or(json!("claude")),
                    })
                })
                .collect();
            let body = pyjson::dumps(&json!({ "sessions": rows }));
            http::bytes_response(
                StatusCode::OK,
                "application/json",
                body.into_bytes(),
                h,
                None,
            )
        }
        _ => events(state),
    })
}

async fn engine(state: &Arc<State>) -> Arc<Engine> {
    ENGINE
        .get_or_init(|| async {
            let eng = Arc::new(Engine {
                lb: ListBuilder::new(baton_core::sessions::Dirs::default_home()),
                home: state.home.clone(),
                csm_dir: Mutex::new((None, None)),
                upstream: Upstream::new(state.token.clone()),
                build_lock: Mutex::new(()),
                cache: Mutex::default(),
                inflight: Mutex::default(),
                built: tokio::sync::Notify::new(),
                headless: Mutex::default(),
                last_poll_ms: AtomicU64::new(0),
                last_kick_ms: AtomicU64::new(0),
            });
            eng.resolve_csm_dir().await;
            tokio::spawn(live_watcher(eng.clone(), state.clone()));
            tokio::spawn(sessions_update(eng.clone(), state.clone()));
            tokio::spawn(relay_upstream_events(eng.clone(), state.clone()));
            eng
        })
        .await
        .clone()
}

impl Engine {
    /// The Python app dir holding `session_scores.json` / `area_map.json`:
    /// `BATON_CSM_DIR`, else the `src` the upstream server reports.
    async fn resolve_csm_dir(&self) {
        {
            let g = self.csm_dir.lock().unwrap();
            if g.0.as_ref().is_some_and(|d| d.exists())
                || g.1.is_some_and(|t| t.elapsed() < Duration::from_secs(60))
            {
                return;
            }
        }
        let dir = match std::env::var_os("BATON_CSM_DIR") {
            Some(d) => Some(PathBuf::from(d)),
            None => self
                .upstream
                .get_json("/api/health")
                .await
                .and_then(|v| v["src"].as_str().map(PathBuf::from)),
        };
        *self.csm_dir.lock().unwrap() = (dir, Some(Instant::now()));
    }

    fn config(&self) -> Config {
        Config {
            home: self.home.clone(),
            csm_dir: self.csm_dir.lock().unwrap().0.clone(),
        }
    }

    /// `_local_sessions_payload`
    async fn payload(self: &Arc<Self>, key: Key) -> Option<Arc<Entry>> {
        loop {
            let waiter = self.built.notified();
            let stale = {
                let cache = self.cache.lock().unwrap();
                let mut inflight = self.inflight.lock().unwrap();
                let ent = cache.get(&key).cloned();
                if let Some(e) = &ent {
                    if e.built.elapsed() < LS_TTL {
                        return ent;
                    }
                }
                if inflight.contains(&key) {
                    if ent.is_some() {
                        return ent; // rebuild running — serve stale
                    }
                    None
                } else {
                    inflight.insert(key.clone());
                    Some(ent)
                }
            };
            match stale {
                None => waiter.await, // first build in flight: wait for it
                Some(Some(ent)) => {
                    let eng = self.clone();
                    tokio::spawn(async move { eng.rebuild(key).await });
                    return Some(ent);
                }
                Some(None) => return self.rebuild(key).await,
            }
        }
    }

    /// Build and swap in a payload; the caller holds the inflight mark.
    async fn rebuild(self: &Arc<Self>, key: Key) -> Option<Arc<Entry>> {
        self.resolve_csm_dir().await;
        let eng = self.clone();
        let k = key.clone();
        let res = tokio::task::spawn_blocking(move || eng.build(k.as_deref())).await;
        let ent = res.ok().map(Arc::new);
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(e) = &ent {
                cache.insert(key.clone(), e.clone());
            }
            self.inflight.lock().unwrap().remove(&key);
        }
        self.built.notify_waiters();
        ent
    }

    /// `_rebuild_ls_payload` body (blocking).
    fn build(&self, filter: Option<&str>) -> Entry {
        let _g = self.build_lock.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = Instant::now();
        let (sessions, headless) = self.lb.build(filter);
        self.headless.lock().unwrap().insert(
            filter.map(str::to_string),
            (Instant::now(), Arc::new(headless)),
        );
        let t_load = Instant::now();
        let inputs = Inputs::load(&self.config(), &self.lb.dirs.claude);
        let t_inputs = Instant::now();
        let grouped = grouping::group_for_remote(&sessions, &inputs, &self.lb);
        let t_group = Instant::now();
        let p: Payload = grouping::encode(grouped);
        let t_end = Instant::now();
        let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
        eprintln!(
            "ls_rebuild filter={:?} sessions={} load_ms={:.1} inputs_ms={:.1} group_ms={:.1} encode_ms={:.1} total_ms={:.1} bytes={} search_bytes={} etag={}",
            filter,
            sessions.len(),
            ms(t0, t_load),
            ms(t_load, t_inputs),
            ms(t_inputs, t_group),
            ms(t_group, t_end),
            ms(t0, t_end),
            p.bytes.len(),
            p.search_bytes.len(),
            &p.etag[..8]
        );
        Entry {
            built: Instant::now(),
            bytes: Bytes::from(p.bytes),
            etag: p.etag,
            search_bytes: Bytes::from(p.search_bytes),
            search_etag: p.search_etag,
            gz: OnceLock::new(),
            search_gz: OnceLock::new(),
            rows: p.rows,
        }
    }

    /// `_force_ls_rebuild(None)`: None when a rebuild is already running.
    async fn force_rebuild(self: &Arc<Self>) -> Option<Arc<Entry>> {
        if !self.inflight.lock().unwrap().insert(None) {
            return None;
        }
        self.rebuild(None).await
    }

    /// `background_sessions(None)`: headless sessions from a list at most
    /// 60 s old (`load_all_sessions`' TTL).
    async fn background(self: &Arc<Self>) -> Arc<Vec<Session>> {
        if let Some((t, h)) = self.headless.lock().unwrap().get(&None) {
            if t.elapsed() < LOAD_ALL_TTL {
                return h.clone();
            }
        }
        let eng = self.clone();
        tokio::task::spawn_blocking(move || {
            let _g = eng.build_lock.lock().unwrap_or_else(|e| e.into_inner());
            let (_, h) = eng.lb.build(None);
            let h = Arc::new(h);
            eng.headless
                .lock()
                .unwrap()
                .insert(None, (Instant::now(), h.clone()));
            h
        })
        .await
        .unwrap_or_default()
    }
}

/// `_serve_payload`: ETag/304, gzip (compressed once per entry).
fn serve_payload(
    h: &hyper::HeaderMap,
    body: &Bytes,
    etag: &str,
    gz: &OnceLock<Bytes>,
) -> Response<Body> {
    let inm = h
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let b = Response::builder()
        .header("etag", etag)
        .header("access-control-allow-origin", "*");
    if !inm.is_empty() && inm == etag {
        return b
            .status(StatusCode::NOT_MODIFIED)
            .body(http::full(Bytes::new()))
            .expect("304");
    }
    let b = b
        .status(StatusCode::OK)
        .header("content-type", "application/json");
    let (b, body) = if http::accepts_gzip(h) {
        let z = gz.get_or_init(|| Bytes::from(http::gzip(body))).clone();
        (b.header("content-encoding", "gzip"), z)
    } else {
        (b, body.clone())
    };
    b.header("content-length", body.len())
        .body(http::full(body))
        .expect("response")
}

fn error_500(h: &hyper::HeaderMap, msg: &str) -> Response<Body> {
    http::json(
        StatusCode::INTERNAL_SERVER_ERROR,
        &json!({ "error": msg }),
        h,
    )
}

// ── /api/events ────────────────────────────────────────────────────────

/// SSE body fed by a channel.
struct ChanBody(tokio::sync::mpsc::Receiver<Bytes>);

impl hyper::body::Body for ChanBody {
    type Data = Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        self.0.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b))))
    }
}

fn events(state: &Arc<State>) -> Response<Body> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let mut sub = state.events.subscribe();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let msg = tokio::select! {
                _ = tx.closed() => break,
                r = tokio::time::timeout(KEEPALIVE, sub.recv()) => match r {
                    Ok(Ok(ev)) => format!("data: {}\n\n", pyjson::dumps(&ev)),
                    Ok(Err(RecvError::Lagged(_))) => continue,
                    Ok(Err(RecvError::Closed)) => break,
                    Err(_) => ": keepalive\n\n".to_string(),
                },
            };
            if tx.send(Bytes::from(msg)).await.is_err() {
                break;
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("access-control-allow-origin", "*")
        .body(ChanBody(rx).boxed())
        .expect("sse")
}

fn listeners(state: &State) -> bool {
    state.events.receiver_count() > 0
}

/// `live_list_watcher`
async fn live_watcher(eng: Arc<Engine>, state: Arc<State>) {
    let mut prev_sig: Option<String> = None;
    let mut prev_keys: Option<Vec<(String, String)>> = None;
    let mut last_rebuild = 0u64;
    loop {
        tokio::time::sleep(LIVE_INTERVAL).await;
        let now = State::now_ms();
        if !listeners(&state)
            && now.saturating_sub(eng.last_poll_ms.load(Ordering::Relaxed)) > LIVE_IDLE_AFTER_MS
        {
            continue;
        }
        if now.saturating_sub(last_rebuild) < LIVE_MIN_REBUILD_GAP_MS {
            continue;
        }
        if now.saturating_sub(state.last_input_ms.load(Ordering::Relaxed))
            < LIVE_QUIET_AFTER_INPUT_MS
        {
            continue;
        }
        let e2 = eng.clone();
        let Ok(sig) = tokio::task::spawn_blocking(move || e2.lb.activity_signature()).await else {
            continue;
        };
        if prev_sig.as_deref() == Some(sig.as_str()) {
            continue;
        }
        let t0 = Instant::now();
        let Some(ent) = eng.force_rebuild().await else {
            continue; // a rebuild is already running; next tick
        };
        prev_sig = Some(sig);
        last_rebuild = State::now_ms();
        let keys: Vec<(String, String)> = ent
            .rows
            .iter()
            .map(|(id, row, groups)| (id.clone(), grouping::row_key(row, groups)))
            .collect();
        if let Some(prev) = &prev_keys {
            if listeners(&state) {
                let old: HashMap<&str, &str> =
                    prev.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                let new: HashSet<&str> = keys.iter().map(|(a, _)| a.as_str()).collect();
                let changed: Vec<Value> = ent
                    .rows
                    .iter()
                    .zip(&keys)
                    .filter(|(_, (id, k))| old.get(id.as_str()) != Some(&k.as_str()))
                    .map(|((_, row, groups), _)| {
                        let mut r = row.clone();
                        r["groups"] = json!(groups);
                        r
                    })
                    .collect();
                let removed: Vec<&str> = prev
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .filter(|id| !new.contains(id))
                    .collect();
                if !changed.is_empty() || !removed.is_empty() {
                    eprintln!(
                        "live_delta changed={} removed={} ms={}",
                        changed.len(),
                        removed.len(),
                        t0.elapsed().as_millis()
                    );
                    let _ = state.events.send(json!({
                        "type": "sessions_delta",
                        "etag": ent.etag,
                        "changed": changed,
                        "removed": removed,
                    }));
                }
            }
        }
        prev_keys = Some(keys);
    }
}

/// `periodic_session_broadcast`: the legacy tmux list every 5 s, fetched
/// from the upstream server for now (the tmux group ports it).
async fn sessions_update(eng: Arc<Engine>, state: Arc<State>) {
    loop {
        tokio::time::sleep(SESSIONS_UPDATE_EVERY).await;
        if !listeners(&state) {
            continue;
        }
        if let Some(v) = eng.upstream.get_json("/api/sessions").await {
            let _ = state
                .events
                .send(json!({"type": "sessions_update", "sessions": v}));
        }
    }
}

/// Forward the upstream server's own events (hooks, resume progress, …)
/// while anyone listens here; list events are native and skipped.
async fn relay_upstream_events(eng: Arc<Engine>, state: Arc<State>) {
    loop {
        if !listeners(&state) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let Some(mut body) = eng.upstream.open("/api/events").await else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let frame = match tokio::time::timeout(Duration::from_secs(20), body.frame()).await {
                Err(_) => {
                    if listeners(&state) {
                        continue;
                    }
                    break;
                }
                Ok(Some(Ok(f))) => f,
                Ok(_) => break,
            };
            if let Ok(data) = frame.into_data() {
                buf.extend_from_slice(&data);
            }
            while let Some(i) = buf.windows(2).position(|w| w == b"\n\n") {
                let block: Vec<u8> = buf.drain(..i + 2).collect();
                for line in String::from_utf8_lossy(&block).lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let Ok(ev) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    let t = ev.get("type").and_then(Value::as_str).unwrap_or("");
                    if !NATIVE_EVENTS.contains(&t) {
                        let _ = state.events.send(ev);
                    }
                }
            }
            if !listeners(&state) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── upstream client (GETs the daemon makes itself) ─────────────────────

struct Upstream {
    client: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        Empty<Bytes>,
    >,
    base: String,
    token: String,
}

impl Upstream {
    fn new(token: String) -> Self {
        // Same flag `baton serve` reads (main.rs); default matches it.
        let args: Vec<String> = std::env::args().collect();
        let base = args
            .iter()
            .position(|a| a == "--upstream")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:19877".into());
        Upstream {
            client: hyper_util::client::legacy::Client::builder(
                hyper_util::rt::TokioExecutor::new(),
            )
            .build_http(),
            base: base.trim_end_matches('/').to_string(),
            token,
        }
    }

    async fn open(&self, path: &str) -> Option<Incoming> {
        let mut b = Request::get(format!("{}{}", self.base, path));
        if !self.token.is_empty() {
            b = b.header("authorization", format!("Bearer {}", self.token));
        }
        let req = b.body(Empty::new()).ok()?;
        let resp = tokio::time::timeout(Duration::from_secs(10), self.client.request(req))
            .await
            .ok()?
            .ok()?;
        resp.status().is_success().then(|| resp.into_body())
    }

    /// GET and discard the body (for the request's side effects upstream).
    async fn drain(&self, path: &str) {
        if let Some(body) = self.open(path).await {
            let _ = tokio::time::timeout(Duration::from_secs(60), body.collect()).await;
        }
    }

    async fn get_json(&self, path: &str) -> Option<Value> {
        let body = self.open(path).await?;
        let bytes = tokio::time::timeout(Duration::from_secs(10), body.collect())
            .await
            .ok()?
            .ok()?
            .to_bytes();
        serde_json::from_slice(&bytes).ok()
    }
}
