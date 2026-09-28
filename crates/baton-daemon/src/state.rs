//! Daemon-wide state shared by the native handlers.
#![allow(dead_code)] // fields are read by the group modules as they land

use baton_core::sessions::Dirs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct State {
    /// Bearer token (empty = dev mode, no auth) — see [`load_token`].
    pub token: String,
    pub config: crate::config::Config,
    pub home: PathBuf,
    pub dirs: Dirs,
    pub upstream: crate::proxy::Upstream,
    /// Live events for `/api/events` subscribers (served by the list group;
    /// tmux/control publish hook events, resume-ready, …). Payloads are the
    /// JSON objects the Python server sends as `data:` lines.
    pub events: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// Unix ms of the last terminal input (scroll/send/send-key). The live
    /// list watcher skips rebuilds for 3 s after it (they compete with the
    /// terminal for CPU) — same rule as the Python server.
    pub last_input_ms: AtomicU64,
    /// Push devices + relay client (push.rs, docs/relay.md).
    pub push: std::sync::Arc<crate::push::Push>,
}

impl State {
    pub fn new(config: crate::config::Config) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let (events, _) = tokio::sync::broadcast::channel(256);
        let push = std::sync::Arc::new(crate::push::Push::from_env(&home, config.relay.clone()));
        let upstream = crate::proxy::Upstream::new(config.upstream.as_deref());
        State {
            token: load_token(&home).0,
            config,
            dirs: Dirs::default_home(),
            home,
            upstream,
            events,
            last_input_ms: AtomicU64::new(0),
            push,
        }
    }

    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Record terminal input (tmux group calls this on scroll/send/send-key).
    pub fn mark_input(&self) {
        self.last_input_ms.store(Self::now_ms(), Ordering::Relaxed);
    }
}

/// Where the bearer token came from.
#[derive(Clone, Debug, PartialEq)]
pub enum TokenSource {
    Env,
    File(PathBuf),
    /// `BATON_BEARER=` in `~/.claude-sessions/config` (the Python server's).
    Legacy(PathBuf),
    None,
}

/// The bearer token: `BATON_BEARER`, else the token file `baton setup`
/// writes (`~/.config/baton/token`), else the Python server's
/// `~/.claude-sessions/config`. Empty = auth off.
pub fn load_token(home: &std::path::Path) -> (String, TokenSource) {
    if let Ok(t) = std::env::var("BATON_BEARER") {
        return (t.trim().to_string(), TokenSource::Env);
    }
    let file = crate::config::token_path();
    if let Ok(t) = std::fs::read_to_string(&file) {
        let t = t.trim();
        if !t.is_empty() {
            return (t.to_string(), TokenSource::File(file));
        }
    }
    let legacy = legacy_config(home);
    match legacy_value(&legacy, "BATON_BEARER") {
        Some(t) if !t.is_empty() => (t, TokenSource::Legacy(legacy)),
        _ => (String::new(), TokenSource::None),
    }
}

/// The Python server's env file.
pub fn legacy_config(home: &std::path::Path) -> PathBuf {
    home.join(".claude-sessions/config")
}

/// `NAME=value` (optionally `export`ed, quoted) from a shell env file.
pub fn legacy_value(path: &std::path::Path, name: &str) -> Option<String> {
    let cfg = std::fs::read_to_string(path).ok()?;
    let pfx = format!("{name}=");
    cfg.lines()
        .map(|l| l.trim().trim_start_matches("export ").trim())
        .filter_map(|l| l.strip_prefix(pfx.as_str()))
        .next_back()
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
}
