//! The session list: every Claude transcript and Codex rollout on the
//! machine, filtered and deduplicated — a port of
//! `sessions_core._build_all_sessions` (output must be identical).

use crate::{parse_file, Kind, Tracker};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Dirs {
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl Dirs {
    pub fn default_home() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Dirs {
            claude: home.join(".claude/projects"),
            codex: home.join(".codex/sessions"),
        }
    }
}

/// `session_entrypoint`: `"entrypoint": "<value>"` in the first ≤1 MB,
/// scanned in 64 KB chunks with a 64-byte overlap (same window as Python,
/// so a field straddling a chunk boundary beyond the overlap is missed the
/// same way).
pub fn entrypoint(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 65536];
    for _ in 0..16 {
        let mut n = 0;
        while n < chunk.len() {
            match f.read(&mut chunk[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return None,
            }
        }
        if n == 0 {
            break;
        }
        let mut window = tail.clone();
        window.extend_from_slice(&chunk[..n]);
        if let Some(v) = find_entrypoint(&window) {
            return Some(v);
        }
        tail = chunk[n.saturating_sub(64)..n].to_vec();
    }
    None
}

/// `rb'"entrypoint"\s*:\s*"([^"]+)"'` — first match.
fn find_entrypoint(b: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"\"entrypoint\"";
    let ws = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let mut from = 0;
    while let Some(off) = b[from..].windows(KEY.len()).position(|w| w == KEY) {
        let mut i = from + off + KEY.len();
        while i < b.len() && ws(b[i]) {
            i += 1;
        }
        if i < b.len() && b[i] == b':' {
            i += 1;
            while i < b.len() && ws(b[i]) {
                i += 1;
            }
            if i < b.len() && b[i] == b'"' {
                let start = i + 1;
                if let Some(len) = b[start..].iter().position(|&c| c == b'"') {
                    if len > 0 {
                        return Some(String::from_utf8_lossy(&b[start..start + len]).into_owned());
                    }
                }
            }
        }
        from = from + off + 1;
    }
    None
}

fn subagent_count(session: &Value) -> u64 {
    let (Some(p), Some(id)) = (session["path"].as_str(), session["id"].as_str()) else {
        return 0;
    };
    let dir = Path::new(p).parent().map(|d| d.join(id).join("subagents"));
    match dir.and_then(|d| std::fs::read_dir(d).ok()) {
        Some(rd) => rd
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .count() as u64,
        None => 0,
    }
}

fn claude_paths(dirs: &Dirs) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dirs.claude) else {
        return out;
    };
    for proj in rd.filter_map(Result::ok) {
        if !proj.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(proj.path()) {
            for f in files.filter_map(Result::ok) {
                let n = f.file_name().to_string_lossy().into_owned();
                if n.ends_with(".jsonl") && !n.starts_with('.') {
                    out.push(f.path());
                }
            }
        }
    }
    out
}

fn codex_paths(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => codex_paths(&p, out),
            Ok(_) => {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("rollout-") && n.ends_with(".jsonl") {
                    out.push(p);
                }
            }
            Err(_) => {}
        }
    }
}

/// Parse many files in parallel (cold start: no per-file state yet).
fn parse_all(paths: &[PathBuf], kind: Kind) -> Vec<Value> {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let chunk = paths.len().div_ceil(n).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|c| s.spawn(move || c.iter().map(|p| parse_file(p, kind)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// The list as `load_all_sessions(None)` returns it, plus the headless
/// (`sdk-cli`) sessions it hides. `tracker`: incremental parsing across
/// calls (None = full parse of every file, in parallel).
pub fn build_all(dirs: &Dirs, tracker: Option<&mut Tracker>) -> (Vec<Value>, Vec<Value>) {
    let cpaths = claude_paths(dirs);
    let mut xpaths = Vec::new();
    codex_paths(&dirs.codex, &mut xpaths);

    let (claude, codex) = match tracker {
        Some(t) => (
            cpaths
                .iter()
                .map(|p| t.parse(p, Kind::Claude))
                .collect::<Vec<_>>(),
            xpaths
                .iter()
                .map(|p| t.parse(p, Kind::Codex))
                .collect::<Vec<_>>(),
        ),
        None => (
            parse_all(&cpaths, Kind::Claude),
            parse_all(&xpaths, Kind::Codex),
        ),
    };

    let mut sessions: Vec<Value> = Vec::new();
    let mut headless: Vec<Value> = Vec::new();
    for mut s in claude
        .into_iter()
        .filter(|s| s["msg_count"].as_u64().unwrap_or(0) > 2)
    {
        let ep = s["path"].as_str().and_then(|p| entrypoint(Path::new(p)));
        s["entrypoint"] = json!(ep);
        s["subagent_count"] = json!(subagent_count(&s));
        if ep.as_deref() == Some("sdk-cli") {
            headless.push(s);
        } else {
            sessions.push(s);
        }
    }
    let last_ts = |s: &Value| s["last_ts"].as_str().unwrap_or("").to_string();
    headless.sort_by_key(|s| std::cmp::Reverse(last_ts(s)));
    sessions.extend(
        codex
            .into_iter()
            .filter(|s| s["msg_count"].as_u64().unwrap_or(0) > 0),
    );

    // One row per id: keep the copy with the most messages (then latest).
    let mut order: Vec<String> = Vec::new();
    let mut best: HashMap<String, Value> = HashMap::new();
    for s in sessions {
        let id = s["id"].as_str().unwrap_or("").to_string();
        let key = |v: &Value| (v["msg_count"].as_u64().unwrap_or(0), last_ts(v));
        match best.get(&id) {
            None => {
                order.push(id.clone());
                best.insert(id, s);
            }
            Some(cur) if key(&s) > key(cur) => {
                best.insert(id, s);
            }
            _ => {}
        }
    }
    let mut sessions: Vec<Value> = order
        .into_iter()
        .filter_map(|id| best.remove(&id))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(last_ts(s)));

    // Short sessions with the same opening message are duplicates.
    let mut seen: HashSet<String> = HashSet::new();
    let deduped = sessions
        .into_iter()
        .filter(|s| {
            if s["msg_count"].as_u64().unwrap_or(0) > 5 {
                return true;
            }
            let fm = crate::pyfmt::head(s["first_msg"].as_str().unwrap_or(""), 100).to_string();
            if fm.is_empty() {
                return true;
            }
            seen.insert(fm)
        })
        .collect();
    (deduped, headless)
}
