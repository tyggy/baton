//! The session list for the live daemon: same rows as
//! [`crate::sessions::build_all`] (== `sessions_core._build_all_sessions`),
//! rebuilt cheaply. Mirrors the Python server's per-process memos: a
//! transcript is re-parsed only when its (mtime, size) moved — and then
//! incrementally, through sharded [`Tracker`]s so a cold start still parses
//! in parallel — project dirs are re-listed only when their mtime changes,
//! entrypoints and subagent counts are cached. Also: `activity_signature`
//! and `last_reply_info`.

use crate::pyfmt::{split_ws, strip};
use crate::sessions::Dirs;
use crate::{Kind, Tracker};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// (mtime_ns, size) — Python's memo key.
pub type FileKey = (i128, u64);

pub fn file_key(m: &std::fs::Metadata) -> FileKey {
    (
        m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
        m.len(),
    )
}

/// One list row's source: the parse result (shared, never copied) plus the
/// fields `_build_all_sessions` annotates Claude sessions with.
#[derive(Clone)]
pub struct Session {
    pub v: Arc<Value>,
    pub claude: bool,
    pub entrypoint: Option<String>,
    pub subagent_count: u64,
}

impl Session {
    pub fn id(&self) -> &str {
        self.v["id"].as_str().unwrap_or("")
    }
    /// `s["last_ts"] or ""`
    pub fn last_ts(&self) -> &str {
        self.v["last_ts"].as_str().unwrap_or("")
    }
    pub fn msg_count(&self) -> u64 {
        self.v["msg_count"].as_u64().unwrap_or(0)
    }
    pub fn get(&self, k: &str) -> Option<&Value> {
        self.v.get(k)
    }
    /// The dict `load_all_sessions` returns for this session.
    pub fn to_value(&self) -> Value {
        let mut v = (*self.v).clone();
        if self.claude {
            v["entrypoint"] = json!(self.entrypoint);
            v["subagent_count"] = json!(self.subagent_count);
        }
        v
    }
    /// `s.get("subagent_count", 0)`
    pub fn subagents(&self) -> u64 {
        if self.claude {
            self.subagent_count
        } else {
            self.v["subagent_count"].as_u64().unwrap_or(0)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub last_reply: Option<String>,
    pub last_role: Option<String>,
}

type Parsed = Option<Arc<Value>>;
/// Project dir → (dir mtime_ns, its `*.jsonl` paths).
type Listings = HashMap<PathBuf, (i128, Arc<Vec<PathBuf>>)>;

pub struct ListBuilder {
    pub dirs: Dirs,
    shards: Vec<Mutex<Tracker>>,
    parsed: Mutex<HashMap<PathBuf, (FileKey, Parsed)>>,
    listings: Mutex<Listings>,
    entrypoints: Mutex<HashMap<PathBuf, (u64, Option<String>)>>,
    subagents: Mutex<HashMap<PathBuf, (i128, u64)>>,
    replies: Mutex<HashMap<String, (FileKey, Reply)>>,
}

const ENTRYPOINT_WINDOW: u64 = 16 * 65536;
const REPLY_TAIL_BYTES: u64 = 256 << 10;

fn dir_mtime(p: &Path) -> Option<i128> {
    std::fs::metadata(p).ok().map(|m| file_key(&m).0)
}

fn lower_contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Same walk as `sessions::codex_paths` (and pathlib's rglob for the
/// YYYY/MM/DD layout, where files only sit in leaf dirs).
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

impl ListBuilder {
    pub fn new(dirs: Dirs) -> Self {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8);
        ListBuilder {
            dirs,
            shards: (0..n).map(|_| Mutex::new(Tracker::new())).collect(),
            parsed: Mutex::default(),
            listings: Mutex::default(),
            entrypoints: Mutex::default(),
            subagents: Mutex::default(),
            replies: Mutex::default(),
        }
    }

    /// Project dirs under ~/.claude/projects (readdir order), filtered like
    /// `_build_all_sessions`.
    pub fn project_dirs(&self, filter: Option<&str>) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(&self.dirs.claude) else {
            return Vec::new();
        };
        rd.filter_map(Result::ok)
            .filter(|e| filter.is_none_or(|f| lower_contains(&e.file_name().to_string_lossy(), f)))
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect()
    }

    /// `_jsonl_paths_in`: `*.jsonl` (not hidden), re-listed only when the
    /// dir's mtime changes.
    pub fn jsonl_paths_in(&self, dir: &Path) -> Arc<Vec<PathBuf>> {
        let Some(mt) = dir_mtime(dir) else {
            return Arc::default();
        };
        if let Some((m, paths)) = self.listings.lock().unwrap().get(dir) {
            if *m == mt {
                return paths.clone();
            }
        }
        let paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n.ends_with(".jsonl") && !n.starts_with('.')
                })
                .map(|e| e.path())
                .collect(),
            Err(_) => Vec::new(),
        };
        let paths = Arc::new(paths);
        self.listings
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), (mt, paths.clone()));
        paths
    }

    fn shard_of(&self, p: &Path) -> usize {
        let mut h = DefaultHasher::new();
        p.hash(&mut h);
        (h.finish() as usize) % self.shards.len()
    }

    /// Parse results for `items`, re-parsing (incrementally, in parallel
    /// across shards) only files whose (mtime, size) changed.
    fn parse_many(&self, items: &[(PathBuf, Kind)]) -> Vec<Parsed> {
        let mut out: Vec<Parsed> = vec![None; items.len()];
        let mut todo: Vec<Vec<(usize, FileKey)>> = vec![Vec::new(); self.shards.len()];
        {
            let memo = self.parsed.lock().unwrap();
            for (i, (p, kind)) in items.iter().enumerate() {
                if *kind == Kind::Claude
                    && p.file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.starts_with("agent-"))
                {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(p) else {
                    continue;
                };
                let key = file_key(&meta);
                match memo.get(p) {
                    Some((k, v)) if *k == key => out[i] = v.clone(),
                    _ => todo[self.shard_of(p)].push((i, key)),
                }
            }
        }
        let results: Vec<(usize, FileKey, Parsed)> = std::thread::scope(|s| {
            let handles: Vec<_> = todo
                .iter()
                .enumerate()
                .filter(|(_, t)| !t.is_empty())
                .map(|(shard, t)| {
                    s.spawn(move || {
                        let mut tracker = self.shards[shard].lock().unwrap();
                        t.iter()
                            .map(|&(i, key)| {
                                let (p, kind) = &items[i];
                                let v = tracker.parse(p, *kind);
                                (i, key, (!v.is_null()).then(|| Arc::new(v)))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });
        let mut memo = self.parsed.lock().unwrap();
        for (i, key, v) in results {
            memo.insert(items[i].0.clone(), (key, v.clone()));
            out[i] = v;
        }
        out
    }

    /// `session_entrypoint`, cached: a found value never changes; "none
    /// found" is final once the file covers the whole 1 MB scan window.
    fn entrypoint(&self, path: &Path) -> Option<String> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if let Some((sz, ep)) = self.entrypoints.lock().unwrap().get(path) {
            if ep.is_some() || *sz >= ENTRYPOINT_WINDOW || *sz == size {
                return ep.clone();
            }
        }
        let ep = crate::sessions::entrypoint(path);
        self.entrypoints
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (size, ep.clone()));
        ep
    }

    /// `_subagent_count_memo`
    fn subagent_count(&self, v: &Value) -> u64 {
        let (Some(p), Some(id)) = (v["path"].as_str(), v["id"].as_str()) else {
            return 0;
        };
        let Some(dir) = Path::new(p).parent().map(|d| d.join(id).join("subagents")) else {
            return 0;
        };
        let Some(mt) = dir_mtime(&dir) else {
            return 0;
        };
        if let Some((m, n)) = self.subagents.lock().unwrap().get(&dir) {
            if *m == mt {
                return *n;
            }
        }
        let n = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
                .count() as u64,
            Err(_) => 0,
        };
        self.subagents.lock().unwrap().insert(dir, (mt, n));
        n
    }

    /// `load_all_sessions(filter)` + the headless sessions it hides.
    pub fn build(&self, filter: Option<&str>) -> (Vec<Session>, Vec<Session>) {
        if !self.dirs.claude.exists() {
            return (Vec::new(), Vec::new());
        }
        let mut items: Vec<(PathBuf, Kind)> = Vec::new();
        for d in self.project_dirs(filter) {
            items.extend(
                self.jsonl_paths_in(&d)
                    .iter()
                    .map(|p| (p.clone(), Kind::Claude)),
            );
        }
        let n_claude = items.len();
        let mut xpaths = Vec::new();
        codex_paths(&self.dirs.codex, &mut xpaths);
        items.extend(xpaths.into_iter().map(|p| (p, Kind::Codex)));
        let parsed = self.parse_many(&items);
        if filter.is_none() {
            let live: HashSet<&PathBuf> = items.iter().map(|(p, _)| p).collect();
            self.parsed.lock().unwrap().retain(|p, _| live.contains(p));
        }

        let mut sessions: Vec<Session> = Vec::new();
        let mut headless: Vec<Session> = Vec::new();
        for v in parsed[..n_claude].iter().flatten() {
            if v["msg_count"].as_u64().unwrap_or(0) <= 2 {
                continue;
            }
            let ep = v["path"]
                .as_str()
                .and_then(|p| self.entrypoint(Path::new(p)));
            let s = Session {
                subagent_count: self.subagent_count(v),
                v: v.clone(),
                claude: true,
                entrypoint: ep,
            };
            if s.entrypoint.as_deref() == Some("sdk-cli") {
                headless.push(s);
            } else {
                sessions.push(s);
            }
        }
        headless.sort_by(|a, b| b.last_ts().cmp(a.last_ts()));
        for v in parsed[n_claude..].iter().flatten() {
            if let Some(f) = filter {
                if !lower_contains(v["cwd"].as_str().unwrap_or(""), f) {
                    continue;
                }
            }
            if v["msg_count"].as_u64().unwrap_or(0) > 0 {
                sessions.push(Session {
                    v: v.clone(),
                    claude: false,
                    entrypoint: None,
                    subagent_count: 0,
                });
            }
        }

        // One row per id: most messages, then latest (first seen on ties).
        let mut order: Vec<String> = Vec::new();
        let mut best: HashMap<String, Session> = HashMap::new();
        for s in sessions {
            let id = s.id().to_string();
            match best.get(&id) {
                None => {
                    order.push(id.clone());
                    best.insert(id, s);
                }
                Some(cur) if (s.msg_count(), s.last_ts()) > (cur.msg_count(), cur.last_ts()) => {
                    best.insert(id, s);
                }
                _ => {}
            }
        }
        let mut sessions: Vec<Session> = order
            .into_iter()
            .filter_map(|id| best.remove(&id))
            .collect();
        sessions.sort_by(|a, b| b.last_ts().cmp(a.last_ts()));

        let mut seen: HashSet<String> = HashSet::new();
        let deduped = sessions
            .into_iter()
            .filter(|s| {
                if s.msg_count() > 5 {
                    return true;
                }
                let fm = crate::pyfmt::head(s.v["first_msg"].as_str().unwrap_or(""), 100);
                fm.is_empty() || seen.insert(fm.to_string())
            })
            .collect();
        (deduped, headless)
    }

    /// `activity_signature(None)`: md5 over every session file's
    /// (path, mtime_ns, size) plus the Codex rollouts of the last 3 local
    /// days. Stats only; listings reused.
    pub fn activity_signature(&self) -> String {
        let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
        use std::io::Write;
        for d in self.project_dirs(None) {
            for p in self.jsonl_paths_in(&d).iter() {
                if let Ok(m) = std::fs::metadata(p) {
                    let (mt, sz) = file_key(&m);
                    let _ = writeln!(buf, "{} {} {}", p.display(), mt, sz);
                }
            }
        }
        let today = local_days_now();
        for back in 0..3 {
            let (y, mo, d) = civil_from_days(today - back);
            let day = self
                .dirs
                .codex
                .join(format!("{y:04}"))
                .join(format!("{mo:02}"))
                .join(format!("{d:02}"));
            let Ok(rd) = std::fs::read_dir(&day) else {
                continue;
            };
            for e in rd.filter_map(Result::ok) {
                if let Ok(m) = std::fs::metadata(e.path()) {
                    let (mt, sz) = file_key(&m);
                    let _ = writeln!(buf, "{} {} {}", e.file_name().to_string_lossy(), mt, sz);
                }
            }
        }
        crate::pyjson::md5_hex(&buf)
    }

    /// `last_reply_info`, memoised on (mtime, size).
    pub fn last_reply(&self, s: &Session) -> Reply {
        let none = Reply {
            last_reply: None,
            last_role: None,
        };
        let Some(path) = s.v["path"].as_str().filter(|p| !p.is_empty()) else {
            return none;
        };
        let Ok(meta) = std::fs::metadata(path) else {
            return none;
        };
        let key = file_key(&meta);
        if let Some((k, r)) = self.replies.lock().unwrap().get(path) {
            if *k == key {
                return r.clone();
            }
        }
        let r = read_last_reply(
            Path::new(path),
            key.1,
            s.v["origin"].as_str() == Some("codex"),
        );
        self.replies
            .lock()
            .unwrap()
            .insert(path.to_string(), (key, r.clone()));
        r
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `_reply_text`
fn reply_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => strip(s).to_string(),
        Some(Value::Array(blocks)) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| {
                    matches!(
                        b.get("type").and_then(Value::as_str),
                        Some("text" | "output_text" | "input_text")
                    )
                })
                .map(|b| strip(b.get("text").and_then(Value::as_str).unwrap_or("")))
                .collect();
            strip(&parts.join("\n")).to_string()
        }
        _ => String::new(),
    }
}

fn read_last_reply(path: &Path, size: u64, codex: bool) -> Reply {
    let mut reply = None;
    let mut role: Option<String> = None;
    let mut data = Vec::new();
    let ok = std::fs::File::open(path).and_then(|mut f| {
        f.seek(SeekFrom::Start(size.saturating_sub(REPLY_TAIL_BYTES)))?;
        f.read_to_end(&mut data)
    });
    if ok.is_ok() {
        for raw in data.split(|&b| b == b'\n').rev() {
            if codex {
                if !contains(raw, b"\"response_item\"") || !contains(raw, b"\"message\"") {
                    continue;
                }
            } else if !contains(raw, b"\"type\":\"assistant\"")
                && !contains(raw, b"\"type\":\"user\"")
            {
                continue;
            }
            let Ok(d) = serde_json::from_slice::<Value>(raw) else {
                continue;
            };
            if !d.is_object() {
                continue;
            }
            let (r, text) = if codex {
                let p = d.get("payload").filter(|p| crate::pyfmt::truthy(p));
                let Some(p) = p.filter(|p| p.is_object()) else {
                    continue;
                };
                if p.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                (p.get("role").cloned(), reply_text(p.get("content")))
            } else {
                let msg = d.get("message").filter(|m| crate::pyfmt::truthy(m));
                let empty = json!({});
                let msg = match msg {
                    Some(m) if m.is_object() => m,
                    Some(_) => continue,
                    None => &empty,
                };
                let r = msg
                    .get("role")
                    .filter(|r| crate::pyfmt::truthy(r))
                    .or_else(|| d.get("type"))
                    .cloned();
                (r, reply_text(msg.get("content")))
            };
            let r = match r.as_ref().and_then(Value::as_str) {
                Some(r @ ("user" | "assistant")) => r.to_string(),
                _ => continue,
            };
            if text.is_empty() {
                continue;
            }
            if r == "user" && (text.starts_with('<') || text.starts_with("[Request interrupted")) {
                continue;
            }
            if role.is_none() {
                role = Some(r.clone());
            }
            if r == "assistant" {
                let joined = split_ws(&text).collect::<Vec<_>>().join(" ");
                reply = Some(crate::pyfmt::head(&joined, 280).to_string());
                break;
            }
        }
    }
    Reply {
        last_reply: reply,
        last_role: role,
    }
}

/// Days since 1970-01-01 of today's local date.
fn local_days_now() -> i64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let tt = t as libc::time_t;
    // SAFETY: localtime_r writes into the zeroed `tm` we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&tt, &mut tm).is_null() };
    if !ok {
        return t.div_euclid(86400);
    }
    days_from_civil(
        tm.tm_year as i64 + 1900,
        tm.tm_mon as i64 + 1,
        tm.tm_mday as i64,
    )
}

/// Howard Hinnant's days_from_civil.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 9, 27), (2024, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn line(kind: &str, ts: &str, text: &str) -> String {
        format!(
            "{{\"type\":\"{kind}\",\"timestamp\":\"{ts}\",\"sessionId\":\"x\",\"entrypoint\":\"cli\",\"message\":{{\"role\":\"{kind}\",\"content\":\"{text}\"}}}}\n"
        )
    }

    /// Same list as the full, uncached `sessions::build_all`, before and
    /// after an append; last reply read from the tail.
    #[test]
    fn matches_build_all_incrementally() {
        let root = std::env::temp_dir().join(format!("baton-listbuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dirs = || Dirs {
            claude: root.join("projects"),
            codex: root.join("codex"),
        };
        let a = root.join("projects/-p1/aaaaaaaa-0000-0000-0000-000000000001.jsonl");
        let b = root.join("projects/-p2/bbbbbbbb-0000-0000-0000-000000000002.jsonl");
        let mut sa = String::new();
        for i in 0..3 {
            sa += &line(
                "user",
                &format!("2026-01-01T00:0{i}:00.000Z"),
                "hello there friend",
            );
            sa += &line(
                "assistant",
                &format!("2026-01-01T00:0{i}:30.000Z"),
                "reply  one",
            );
        }
        write(&a, &sa);
        write(
            &b,
            &(line("user", "2026-01-02T00:00:00.000Z", "other topic here").repeat(4)),
        );
        let lb = ListBuilder::new(dirs());
        let check = |lb: &ListBuilder| {
            let (s, h) = lb.build(None);
            let (rs, rh) = crate::sessions::build_all(&dirs(), None);
            let s: Vec<Value> = s.iter().map(Session::to_value).collect();
            let h: Vec<Value> = h.iter().map(Session::to_value).collect();
            assert_eq!(s, rs);
            assert_eq!(h, rh);
            s.len()
        };
        assert_eq!(check(&lb), 2);
        let mut f = std::fs::OpenOptions::new().append(true).open(&a).unwrap();
        use std::io::Write;
        f.write_all(line("assistant", "2026-01-03T00:00:00.000Z", "newest\\nline").as_bytes())
            .unwrap();
        drop(f);
        assert_eq!(check(&lb), 2);
        let (s, _) = lb.build(None);
        let r = lb.last_reply(&s[0]);
        assert_eq!(r.last_reply.as_deref(), Some("newest line"));
        assert_eq!(r.last_role.as_deref(), Some("assistant"));
        assert_eq!(lb.activity_signature().len(), 32);
        let _ = std::fs::remove_dir_all(&root);
    }
}
