//! Which tmux session runs which transcript — a port of
//! `sessions_core.get_tmux_for_sessions` (tmux panes → process walk for
//! `claude --resume <id>` / `codex resume <id>` → Codex app-server thread
//! mapping by cwd → `~/.claude/tty_session_map.json` fallback) and
//! `get_zombie_session_ids`.

use crate::pyfmt::strip;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub struct TmuxInfo {
    /// session id → tmux session name (freshest pane wins).
    pub tmux_for: HashMap<String, String>,
    pub zombies: HashSet<String>,
}

/// Run a command, stdout as text; None when it can't run or exits non-zero.
fn run(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(resolve(prog)).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `prog` from PATH, else the usual Homebrew / system locations (a daemon
/// started by launchd may have a minimal PATH).
fn resolve(prog: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for d in std::env::split_paths(&path).chain(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/usr/sbin",
            "/bin",
        ]
        .map(PathBuf::from),
    ) {
        let p = d.join(prog);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from(prog)
}

/// Python `s.split(None, maxsplit)`.
fn split_max(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s.trim_start_matches(crate::pyfmt::is_space);
    while !rest.is_empty() {
        if out.len() == max {
            out.push(rest);
            break;
        }
        let end = rest.find(crate::pyfmt::is_space).unwrap_or(rest.len());
        out.push(&rest[..end]);
        rest = rest[end..].trim_start_matches(crate::pyfmt::is_space);
    }
    out
}

/// Python `int(s)` for the simple decimal case.
fn py_int(s: &str) -> Option<i64> {
    strip(s).replace('_', "").parse().ok()
}

/// `tty_session_map.json` → {tty: session_id} for entries under 7 days old.
/// A malformed `ts` ends the scan (Python raised there, keeping what it had).
fn tty_map(home: &Path) -> Option<HashMap<String, String>> {
    let text = std::fs::read_to_string(home.join(".claude/tty_session_map.json")).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut out = HashMap::new();
    for (tty, info) in data.as_object()? {
        let Some(info) = info.as_object() else {
            continue;
        };
        let ts = match info.get("ts") {
            None => 0.0,
            Some(Value::Bool(b)) => *b as u8 as f64,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            Some(_) => break,
        };
        if now - ts < 7.0 * 86400.0 {
            if let Some(sid) = info.get("session_id").and_then(Value::as_str) {
                if !sid.is_empty() {
                    out.insert(tty.clone(), sid.to_string());
                }
            }
        }
    }
    Some(out)
}

fn resume_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:\bclaude\b[^\n;]*?--resume|\bcodex resume) ([0-9a-f-]{36})").unwrap()
    })
}

fn rollout_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"rollout-\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl",
        )
        .unwrap()
    })
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Mapping and zombies in one pass (one `tmux list-panes`, one `ps`).
pub fn detect(home: &Path, claude_dir: &Path) -> TmuxInfo {
    let mut info = TmuxInfo {
        tmux_for: HashMap::new(),
        zombies: HashSet::new(),
    };
    let Some(panes) = run(
        "tmux",
        &[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_tty}\t#{pane_pid}\t#{session_activity}",
        ],
    ) else {
        return info;
    };

    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut cmd_of: HashMap<i64, String> = HashMap::new();
    if let Some(ps) = run("ps", &["-eo", "pid,ppid,command"]) {
        for line in ps.split('\n').skip(1) {
            let parts = split_max(line, 2);
            if parts.len() < 3 {
                continue;
            }
            let (Some(pid), Some(ppid)) = (py_int(parts[0]), py_int(parts[1])) else {
                continue;
            };
            cmd_of.insert(pid, parts[2].to_string());
            children.entry(ppid).or_default().push(pid);
        }
    }

    // Depth-first over the pane's process subtree, same visiting order as
    // the Python stack walk.
    let walk = |root: i64, f: &mut dyn FnMut(&str) -> bool| -> bool {
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur) {
                continue;
            }
            if f(cmd_of.get(&cur).map(String::as_str).unwrap_or("")) {
                return true;
            }
            if let Some(c) = children.get(&cur) {
                stack.extend(c);
            }
        }
        false
    };

    let tty_to_session = tty_map(home).unwrap_or_default();
    let mut candidates: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    let mut codex_by_tty: Option<HashMap<String, String>> = None;
    let mut zombie_lines: Vec<String> = Vec::new();
    for line in panes.lines() {
        // The zombie check's own list-panes format (session_name, tty, activity).
        let raw: Vec<&str> = line.split('\t').collect();
        if raw.len() == 4 {
            zombie_lines.push(format!("{}\t{}\t{}", raw[0], raw[1], raw[3]));
        }
        let parts: Vec<&str> = strip(line).split('\t').collect();
        if parts.len() != 4 {
            continue;
        }
        let (name, tty) = (parts[0], parts[1]);
        let ts = py_int(parts[3]).unwrap_or(0);
        let ppid = py_int(parts[2]).unwrap_or(0);
        let mut sid: Option<String> = None;
        if ppid != 0 {
            walk(ppid, &mut |cmd| {
                if let Some(c) = resume_re().captures(cmd) {
                    sid = Some(c[1].to_string());
                    return true;
                }
                false
            });
        }
        if sid.is_none()
            && ppid != 0
            && walk(ppid, &mut |cmd| {
                basename(crate::pyfmt::split_ws(cmd).next().unwrap_or("")) == "codex"
            })
        {
            let map = codex_by_tty.get_or_insert_with(|| {
                let mut inv = HashMap::new();
                for (s, t) in codex_tty_map() {
                    inv.insert(t, s);
                }
                inv
            });
            sid = map.get(tty).cloned();
        }
        if sid.is_none() {
            sid = tty_to_session.get(tty).cloned();
        }
        if let Some(sid) = sid {
            candidates
                .entry(sid)
                .or_default()
                .push((name.to_string(), ts));
        }
    }
    for (sid, opts) in candidates {
        // max() keeps the first of equal maxima.
        let mut best = &opts[0];
        for o in &opts[1..] {
            if o.1 > best.1 {
                best = o;
            }
        }
        info.tmux_for.insert(sid, best.0.clone());
    }

    // get_zombie_session_ids: panes whose tty-map session was written >30 min
    // after the pane last saw activity.
    if let Some(tty_to_session) = tty_map(home).filter(|m| !m.is_empty()) {
        for line in &zombie_lines {
            let parts: Vec<&str> = strip(line).split('\t').collect();
            if parts.len() != 3 {
                continue;
            }
            let Some(sid) = tty_to_session.get(parts[1]) else {
                continue;
            };
            let Some(activity) = py_int(parts[2]) else {
                continue;
            };
            let Some(mtime) = jsonl_mtime(claude_dir, sid) else {
                continue;
            };
            if mtime - activity as f64 > 30.0 * 60.0 {
                info.zombies.insert(sid.clone());
            }
        }
    }
    info
}

/// `_jsonl_mtime_for_session`: mtime of `<project>/<sid>.jsonl`, the last
/// project dir (readdir order) holding it winning.
fn jsonl_mtime(claude_dir: &Path, sid: &str) -> Option<f64> {
    if sid.starts_with('.') || sid.contains('/') {
        return None;
    }
    let mut found = None;
    for e in std::fs::read_dir(claude_dir).ok()?.filter_map(Result::ok) {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if let Ok(m) = std::fs::metadata(p.join(format!("{sid}.jsonl"))) {
            if m.is_file() {
                use std::os::unix::fs::MetadataExt;
                found = Some(m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9);
            }
        }
    }
    found
}

/// `_ps_snapshot`: (pid, ppid, tty, command); tty from the raw device
/// (pty slaves are major 16, named ttys%03d by minor).
fn ps_snapshot() -> Vec<(i64, String, String)> {
    let Some(out) = run("ps", &["-ax", "-o", "pid=,ppid=,tdev=,command="]) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for line in out.lines() {
        let parts = split_max(line, 3);
        if parts.len() < 3 {
            continue;
        }
        let (Some(pid), Some(_ppid)) = (py_int(parts[0]), py_int(parts[1])) else {
            continue;
        };
        let mut tty = "??".to_string();
        if let Some((major, minor)) = parts[2].split_once('/') {
            if major == "16" && !minor.is_empty() && minor.chars().all(|c| c.is_ascii_digit()) {
                tty = format!("ttys{:03}", minor.parse::<u64>().unwrap_or(0));
            }
        }
        rows.push((pid, tty, parts.get(3).unwrap_or(&"").to_string()));
    }
    rows
}

/// `_codex_rollout_meta`: (source, cwd) from a rollout's first line.
fn rollout_meta(path: &str) -> Option<(String, Value)> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut line = Vec::new();
    std::io::BufReader::new(f)
        .read_until(b'\n', &mut line)
        .ok()?;
    let d: Value = serde_json::from_slice(&line).ok()?;
    let d = d.as_object()?;
    let p = d.get("payload").filter(|p| crate::pyfmt::truthy(p));
    let empty = serde_json::Map::new();
    let p = match p {
        Some(Value::Object(m)) => m,
        Some(_) => return None,
        None => &empty,
    };
    let src = match p.get("source") {
        Some(Value::String(s)) => s.clone(),
        _ => "subagent".to_string(),
    };
    Some((src, p.get("cwd").cloned().unwrap_or(Value::Null)))
}

/// Ordered map helper (Python dict semantics: re-insert keeps position).
#[derive(Default)]
struct Ordered<V> {
    keys: Vec<String>,
    map: HashMap<String, V>,
}

impl<V> Ordered<V> {
    fn insert(&mut self, k: String, v: V) {
        if !self.map.contains_key(&k) {
            self.keys.push(k.clone());
        }
        self.map.insert(k, v);
    }
    fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.keys.iter().map(|k| (k, &self.map[k]))
    }
}

/// `_codex_tty_map`: {sid: /dev/ttysNNN} for live Codex CLIs, in insertion
/// order.
pub fn codex_tty_map() -> Vec<(String, String)> {
    let mut out: Ordered<String> = Ordered::default();
    let mut codex_pids: Vec<(i64, String)> = Vec::new();
    let mut daemon_pids: Vec<i64> = Vec::new();
    for (pid, tty, cmd) in ps_snapshot() {
        if tty == "??" {
            if cmd.contains("codex app-server") && !cmd.contains("pid-update-loop") {
                daemon_pids.push(pid);
            }
            continue;
        }
        if cmd.starts_with("codex") {
            match codex_pids.iter_mut().find(|(p, _)| *p == pid) {
                Some(e) => e.1 = format!("/dev/{tty}"),
                None => codex_pids.push((pid, format!("/dev/{tty}"))),
            }
        }
    }
    if codex_pids.is_empty() {
        return Vec::new();
    }
    let tty_of = |pid: i64| {
        codex_pids
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, t)| t.clone())
    };
    let pids: Vec<String> = codex_pids
        .iter()
        .map(|(p, _)| p.to_string())
        .chain(daemon_pids.iter().map(i64::to_string))
        .collect();
    // lsof exits 1 when some pid has vanished; its output is still valid.
    let Ok(r) = Command::new(resolve("lsof"))
        .args(["-p", &pids.join(","), "-Fpfn"])
        .output()
    else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&r.stdout);
    let mut cli_cwd: Ordered<String> = Ordered::default(); // pid → cwd
    let mut daemon_rollouts: Ordered<String> = Ordered::default(); // sid → path
    let (mut cur_pid, mut cur_fd): (Option<i64>, Option<String>) = (None, None);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            cur_pid = py_int(rest);
            cur_fd = None;
        } else if let Some(rest) = line.strip_prefix('f') {
            cur_fd = Some(rest.to_string());
        } else if let (Some(name), Some(pid)) = (line.strip_prefix('n'), cur_pid) {
            if cur_fd.as_deref() == Some("cwd") && tty_of(pid).is_some() {
                cli_cwd.insert(pid.to_string(), name.to_string());
                continue;
            }
            let Some(c) = rollout_re().captures(name) else {
                continue;
            };
            let sid = c[1].to_string();
            if let Some(tty) = tty_of(pid) {
                if !out.map.contains_key(&sid) {
                    out.insert(sid, tty);
                }
            } else if name.ends_with(".jsonl") {
                daemon_rollouts.insert(sid, name.to_string());
            }
        }
    }
    let mut by_cwd_threads: Ordered<Vec<String>> = Ordered::default();
    for (sid, path) in daemon_rollouts.iter() {
        if out.map.contains_key(sid) {
            continue;
        }
        let Some((src, cwd)) = rollout_meta(path) else {
            continue;
        };
        if src != "cli" || !crate::pyfmt::truthy(&cwd) {
            continue;
        }
        let Some(cwd) = cwd.as_str() else {
            continue; // non-string cwd never equals a CLI's cwd
        };
        let mut v = by_cwd_threads.map.get(cwd).cloned().unwrap_or_default();
        v.push(sid.clone());
        by_cwd_threads.insert(cwd.to_string(), v);
    }
    let taken: HashSet<String> = out.map.values().cloned().collect();
    let mut by_cwd_clis: HashMap<String, Vec<i64>> = HashMap::new();
    for (pid, cwd) in cli_cwd.iter() {
        let pid: i64 = pid.parse().unwrap_or(0);
        if !tty_of(pid).is_some_and(|t| taken.contains(&t)) {
            by_cwd_clis.entry(cwd.clone()).or_default().push(pid);
        }
    }
    let mut assign = Vec::new();
    for (cwd, sids) in by_cwd_threads.iter() {
        let pids = by_cwd_clis.get(cwd).map(Vec::as_slice).unwrap_or(&[]);
        if sids.len() == 1 && pids.len() == 1 {
            if let Some(t) = tty_of(pids[0]) {
                assign.push((sids[0].clone(), t));
            }
        }
    }
    for (s, t) in assign {
        out.insert(s, t);
    }
    out.keys
        .iter()
        .map(|k| (k.clone(), out.map[k].clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_like_python() {
        assert_eq!(
            split_max("  1   0  /sbin/launchd -x  ", 2),
            vec!["1", "0", "/sbin/launchd -x  "]
        );
        assert_eq!(split_max(" 5 3", 2), vec!["5", "3"]);
        assert!(split_max("   ", 2).is_empty());
    }

    #[test]
    fn resume_regex() {
        let id = "d4039fa0-af2a-456f-b6bd-8c655b3cf238";
        let m = |s: &str| resume_re().captures(s).map(|c| c[1].to_string());
        assert_eq!(
            m(&format!("claude --chrome --resume {id}")),
            Some(id.into())
        );
        assert_eq!(
            m(&format!("/usr/bin/codex resume {id} --x")),
            Some(id.into())
        );
        assert_eq!(m(&format!("claude; x --resume {id}")), None);
        assert_eq!(m(&format!("claudex --resume {id}")), None);
    }
}
