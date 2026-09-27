//! Daemon-wide state shared by the native handlers.
#![allow(dead_code)] // fields are read by the group modules as they land

use baton_core::sessions::Dirs;
use baton_core::Tracker;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct State {
    /// Bearer token (empty = dev mode, no auth) — from `BATON_BEARER` or
    /// `~/.claude-sessions/config`, the same source the Python server uses.
    pub token: String,
    pub home: PathBuf,
    pub dirs: Dirs,
    /// Incremental transcript parser shared by the list/read handlers.
    pub tracker: Mutex<Tracker>,
    pub upstream: crate::proxy::Upstream,
    /// Live events for `/api/events` subscribers (served by the list group;
    /// tmux/control publish hook events, resume-ready, …). Payloads are the
    /// JSON objects the Python server sends as `data:` lines.
    pub events: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// Unix ms of the last terminal input (scroll/send/send-key). The live
    /// list watcher skips rebuilds for 3 s after it (they compete with the
    /// terminal for CPU) — same rule as the Python server.
    pub last_input_ms: AtomicU64,
}

impl State {
    pub fn new(upstream: crate::proxy::Upstream) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let (events, _) = tokio::sync::broadcast::channel(256);
        State {
            token: load_token(&home),
            dirs: Dirs::default_home(),
            home,
            tracker: Mutex::new(Tracker::new()),
            upstream,
            events,
            last_input_ms: AtomicU64::new(0),
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

fn load_token(home: &std::path::Path) -> String {
    if let Ok(t) = std::env::var("BATON_BEARER") {
        return t.trim().to_string();
    }
    let cfg = std::fs::read_to_string(home.join(".claude-sessions/config")).unwrap_or_default();
    cfg.lines()
        .map(|l| l.trim().trim_start_matches("export ").trim())
        .filter_map(|l| l.strip_prefix("BATON_BEARER="))
        .next_back()
        .map(|v| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .unwrap_or_default()
}
