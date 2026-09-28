//! The grouped list the clients show — a port of
//! `sessions_core.group_for_remote` and the server's payload split
//! (`_rebuild_ls_payload`: rows without `search_index`, plus the search
//! index as its own document), encoded byte-identically to Python's
//! `json.dumps` so ETags match the reference server.

use crate::listbuild::{ListBuilder, Session};
use crate::pyfmt::{head, split_ws, strip, truthy};
use crate::pyjson;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the optional data lives. `csm_dir` is the Python app directory
/// (`session_scores.json`, `area_map.json` sit next to `sessions_core.py`).
#[derive(Clone, Debug)]
pub struct Config {
    pub home: PathBuf,
    pub csm_dir: Option<PathBuf>,
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn object(v: Option<Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// Everything `group_for_remote` reads besides the sessions.
pub struct Inputs {
    pub scores: Map<String, Value>,
    pub classifications: Map<String, Value>,
    pub area_map: HashMap<String, Value>,
    pub entity_index: Map<String, Value>,
    pub catalog_by_id: HashMap<String, Value>,
    pub lineage: Map<String, Value>,
    pub tmux: crate::tmuxmap::TmuxInfo,
}

impl Inputs {
    pub fn load(cfg: &Config, claude_dir: &Path) -> Inputs {
        let home = &cfg.home;
        let user = home
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let prefix = format!("-Users-{user}-Documents-");
        let settings = crate::settings::get();
        let mut area_map: HashMap<String, Value> = HashMap::new();
        area_map.insert(format!("-Users-{user}"), Value::from("Home"));
        // `project_groups` from the config (docs/config.md).
        for (k, v) in &settings.project_groups {
            area_map.insert(crate::settings::group_key(k, home), Value::from(v.as_str()));
        }
        let mut scores = Map::new();
        if let Some(dir) = &cfg.csm_dir {
            if let Some(Value::Object(m)) = read_json(&dir.join("area_map.json")) {
                for (k, v) in m {
                    let key = k
                        .replace("{home}", &format!("-Users-{user}"))
                        .replace("{prefix}", &prefix);
                    area_map.insert(key, v);
                }
            }
            scores = object(read_json(&dir.join("session_scores.json")));
        }
        let tmux = crate::tmuxmap::detect(home, claude_dir);
        // The entity catalog is an opt-in integration (`entities = true`).
        let entities_on = settings.entities;
        let catalog_by_id =
            match read_json(&home.join(".baton/entities.json")).filter(|_| entities_on) {
                Some(Value::Array(list)) => {
                    let mut m = HashMap::new();
                    let mut ok = true;
                    for e in list {
                        match e.get("id") {
                            Some(Value::String(id)) => {
                                m.insert(id.clone(), e);
                            }
                            Some(_) => {}
                            None => {
                                ok = false; // KeyError: the whole comprehension fails
                                break;
                            }
                        }
                    }
                    if ok {
                        m
                    } else {
                        HashMap::new()
                    }
                }
                _ => HashMap::new(),
            };
        Inputs {
            scores,
            classifications: load_classifications(home),
            area_map,
            entity_index: object(
                read_json(&home.join(".baton/session_entities.json")).filter(|_| entities_on),
            ),
            catalog_by_id,
            lineage: object(read_json(&home.join(".baton/session_lineage.json"))),
            tmux,
        }
    }
}

/// `load_cache`: the title/folder classifications, folders normalised; any
/// malformed entry makes Python fall back to `{}`.
fn load_classifications(home: &Path) -> Map<String, Value> {
    let Some(Value::Object(mut cache)) =
        read_json(&crate::settings::claude_dir(home).join("session_classifications.json"))
    else {
        return Map::new();
    };
    for (_, entry) in cache.iter_mut() {
        let Some(e) = entry.as_object_mut() else {
            return Map::new();
        };
        match e.get("folder") {
            None => {}
            Some(Value::String(f)) => {
                let nf = normalize_folder(f);
                e.insert("folder".into(), Value::String(nf));
            }
            Some(_) => return Map::new(),
        }
    }
    cache
}

pub fn normalize_folder(folder: &str) -> String {
    if folder == "other" {
        return "other".into();
    }
    let f = folder.trim_start_matches('/');
    if f.starts_with("work/") {
        f.to_string()
    } else {
        format!("work/{f}")
    }
}

pub fn folder_to_group(folder: &str) -> String {
    if folder == "other" {
        return "Other".into();
    }
    let f = normalize_folder(folder);
    let f = f.strip_prefix("work/").unwrap_or(&f);
    let parts: Vec<&str> = f.split('/').collect();
    if parts.len() >= 2 {
        let sub = parts[1].to_lowercase();
        if sub == "portfolio" || sub == "pipeline" {
            let mut c = sub.chars();
            let cap: String = c
                .next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default();
            return format!("{} {}", parts[0], cap);
        }
    }
    parts[0].to_string()
}

const WS: &str = r"[\s\x1c-\x1f]";

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(&pat.replace("\\s", WS)).unwrap())
}

/// `fallback_title`: a readable title from the first message.
pub fn fallback_title(first_msg: &str) -> String {
    static PATH: OnceLock<Regex> = OnceLock::new();
    static PLAN1: OnceLock<Regex> = OnceLock::new();
    static PLAN2: OnceLock<Regex> = OnceLock::new();
    static HASH: OnceLock<Regex> = OnceLock::new();
    static CTX: OnceLock<Regex> = OnceLock::new();
    static LEAD: OnceLock<Regex> = OnceLock::new();
    if first_msg.is_empty() || first_msg == "(no message)" {
        return "untitled session".into();
    }
    let path_re = re(
        &PATH,
        r"(?s)^((?:/[\w.@,-]+(?:\\[ ][\w.@,()'-]+)*)+/?)\s*(.*)",
    );
    let mut clean = first_msg.to_string();
    while let Some(c) = path_re.captures(&clean) {
        let g1 = c.get(1).map(|m| m.as_str()).unwrap_or("");
        let g2 = strip(c.get(2).map(|m| m.as_str()).unwrap_or("")).to_string();
        if g2.is_empty() {
            clean = g1
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .replace('\\', "");
            break;
        }
        clean = g2;
    }
    clean = re(&PLAN1, r"^Implement the following plan:\s*")
        .replace(&clean, "")
        .into_owned();
    clean = re(&PLAN2, r"^#+ Plan:\s*").replace(&clean, "").into_owned();
    clean = re(&HASH, r"^#+ ").replace(&clean, "").into_owned();
    clean = re(&CTX, r"(?s)\n##\s+Context.*")
        .replace(&clean, "")
        .into_owned();
    clean = clean.replace('\\', "");
    clean = re(&LEAD, r"^[\s,/()]+").replace(&clean, "").into_owned();
    clean = split_ws(&clean).collect::<Vec<_>>().join(" ");
    if crate::pyfmt::len(&clean) > 70 {
        clean = format!("{}...", head(&clean, 67));
    }
    let mut chars = clean.chars();
    if let Some(c0) = chars.next() {
        if c0.is_lowercase() {
            clean = c0.to_uppercase().chain(chars).collect();
        }
    }
    clean
}

/// `datetime.fromisoformat(ts.replace("Z", "+00:00"))` → UTC microseconds;
/// None when Python would raise or the value is naive (the subtraction
/// from an aware `now` raises too).
pub fn parse_iso_us(ts: &str) -> Option<i64> {
    let s = ts.replace('Z', "+00:00");
    let c: Vec<char> = s.chars().collect();
    let num = |a: usize, n: usize| -> Option<i64> {
        let part = c.get(a..a + n)?;
        if part.iter().all(|ch| ch.is_ascii_digit()) {
            part.iter().collect::<String>().parse().ok()
        } else {
            None
        }
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if c.get(4) != Some(&'-') || c.get(7) != Some(&'-') {
        return None;
    }
    let dim = [
        31,
        if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            29
        } else {
            28
        },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&mo) || d < 1 || d > dim[(mo - 1) as usize] || y < 1 {
        return None;
    }
    if c.len() <= 10 {
        return None; // date only: naive
    }
    // time: HH[:MM[:SS[.f+]]] then offset
    let mut i = 11;
    let hh = num(i, 2)?;
    i += 2;
    let (mut mm, mut ss, mut us) = (0, 0, 0i64);
    if c.get(i) == Some(&':') {
        mm = num(i + 1, 2)?;
        i += 3;
        if c.get(i) == Some(&':') {
            ss = num(i + 1, 2)?;
            i += 3;
            if matches!(c.get(i), Some('.') | Some(',')) {
                i += 1;
                let start = i;
                while c.get(i).is_some_and(|ch| ch.is_ascii_digit()) {
                    i += 1;
                }
                let n = i - start;
                if n == 0 {
                    return None;
                }
                let digits: String = c[start..start + n.min(6)].iter().collect();
                us = format!("{digits:0<6}").parse().ok()?;
            }
        }
    }
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let off_sign = match c.get(i) {
        Some('+') => 1,
        Some('-') => -1,
        _ => return None, // naive (or trailing junk)
    };
    let o: String = c[i + 1..].iter().collect();
    let (oh, om, os) = {
        let p: Vec<&str> = o.split(':').collect();
        match p.as_slice() {
            [h] if h.len() == 4 => (h[..2].parse::<i64>().ok()?, h[2..].parse::<i64>().ok()?, 0),
            [h] if h.len() == 2 => (h.parse::<i64>().ok()?, 0, 0),
            [h, m] => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?, 0),
            [h, m, s] => (
                h.parse::<i64>().ok()?,
                m.parse::<i64>().ok()?,
                s.split('.').next()?.parse::<i64>().ok()?,
            ),
            _ => return None,
        }
    };
    if oh > 23 || om > 59 {
        return None;
    }
    let days = crate::listbuild::days_from_civil(y, mo, d);
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss - off_sign * (oh * 3600 + om * 60 + os);
    Some(secs * 1_000_000 + us)
}

pub fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// `age_str`
pub fn age_str(ts: Option<&str>, now_us: i64) -> String {
    let Some(dt) = ts.and_then(parse_iso_us) else {
        return "?".into();
    };
    let diff = now_us - dt;
    let day_us = 86_400_000_000i64;
    let days = diff.div_euclid(day_us);
    let secs = diff.rem_euclid(day_us) / 1_000_000;
    let hours = secs / 3600;
    if days > 30 {
        format!("{}mo", days / 30)
    } else if days > 0 {
        format!("{days}d")
    } else if hours > 0 {
        format!("{hours}h")
    } else {
        format!("{}m", secs / 60)
    }
}

/// `int(v or 0)` (None/garbage → 0).
fn int0(v: Option<&Value>) -> i64 {
    crate::pyfmt::py_int(v).unwrap_or(0)
}

fn num(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as u8 as f64,
        _ => 0.0,
    }
}

fn mtime_secs(path: Option<&str>) -> Option<f64> {
    let m = std::fs::metadata(path?).ok()?;
    use std::os::unix::fs::MetadataExt;
    Some(m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9)
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python `round(x, 2)`.
fn round2(x: f64) -> f64 {
    format!("{x:.2}").parse().unwrap_or(x)
}

struct Item<'a> {
    s: &'a Session,
    title: String,
    folder: String,
    group: String,
    score: f64,
}

/// A row as `/api/local-sessions` serves it (no `search_index`) plus that
/// session's search text.
pub struct Row {
    pub id: String,
    pub row: Map<String, Value>,
    pub search: String,
}

pub struct Grouped {
    pub recent: Vec<Row>,
    pub momentum: Vec<Row>,
    pub areas: Vec<(String, Vec<Row>)>,
}

impl Inputs {
    fn title(&self, s: &Session) -> String {
        let c = self.classifications.get(s.id()).filter(|c| truthy(c));
        let mut title = match c.and_then(|c| c.get("title")).filter(|t| truthy(t)) {
            Some(t) => {
                let mut t = t
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| t.to_string());
                if let Some(d) = c.and_then(|c| c.get("drift_topic")).filter(|d| truthy(d)) {
                    t += " → ";
                    t += d.as_str().unwrap_or("");
                }
                t
            }
            None => fallback_title(s.v["first_msg"].as_str().unwrap_or("")),
        };
        if s.v["origin"].as_str() == Some("codex") {
            title = format!("⌥ {title}");
        }
        title
    }

    fn folder(&self, s: &Session) -> String {
        if let Some(c) = self.classifications.get(s.id()).filter(|c| truthy(c)) {
            if let Some(f) = c.get("folder").and_then(Value::as_str) {
                if !f.is_empty() && f != "other" {
                    return normalize_folder(f);
                }
            }
        }
        if let Some(Value::String(area)) = self.area_map.get(s.v["project"].as_str().unwrap_or(""))
        {
            if area != "Main" && area != "Home" {
                return format!("work/{area}");
            }
        }
        "other".into()
    }

    fn score_of(&self, id: &str) -> Option<&Map<String, Value>> {
        self.scores.get(id).and_then(Value::as_object)
    }

    fn momentum(&self, id: &str) -> i64 {
        int0(self.score_of(id).and_then(|m| m.get("momentum")))
    }

    /// `importance_score`
    fn importance(&self, s: &Session, now: i64) -> f64 {
        let size = (s.msg_count().max(1) as f64).log2();
        let days_ago = match s.v["last_ts"].as_str().and_then(parse_iso_us) {
            Some(dt) => (now - dt) as f64 / 1e6 / 86400.0,
            None => 30.0,
        };
        let recency = if days_ago < 0.5 {
            1.0
        } else if days_ago < 1.0 {
            0.9
        } else if days_ago < 3.0 {
            0.75
        } else if days_ago < 7.0 {
            0.6
        } else if days_ago < 14.0 {
            0.45
        } else {
            0.3
        };
        let mut base = size * recency;
        if let Some(sc) = self.score_of(s.id()).filter(|m| !m.is_empty()) {
            let m = num(sc.get("momentum"));
            let r = num(sc.get("rc"));
            let multi = sc.get("type").and_then(Value::as_str) == Some("multi");
            base += m * if multi { 1.5 } else { 0.5 } + r * 0.5;
        }
        base
    }

    /// `session_state`
    fn state(&self, s: &Session, mtime: Option<f64>, now: f64) -> &'static str {
        let sid = s.id();
        if self.tmux.zombies.contains(sid) {
            return "zombie";
        }
        if self.tmux.tmux_for.contains_key(sid) {
            return if mtime.is_some_and(|m| now - m < 30.0) {
                "live"
            } else {
                "tmux"
            };
        }
        if mtime.is_some_and(|m| now - m < 600.0) {
            "recent"
        } else {
            "dormant"
        }
    }

    fn to_row(&self, it: &Item, lb: &ListBuilder, now_us: i64) -> Row {
        let s = it.s;
        let sid = s.id();
        let v = &s.v;
        let get = |k: &str| v.get(k).cloned().unwrap_or(Value::Null);
        let mut entity = Value::Null;
        let mut entities = Value::Array(Vec::new());
        if let Some(ce) = self.entity_index.get(sid).filter(|c| truthy(c)) {
            if let Some(es) = ce.get("entities").filter(|e| truthy(e)) {
                entities = es.clone();
            }
            if let Some(pid) = ce
                .get("primary")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            {
                if let Some(e) = self.catalog_by_id.get(pid) {
                    let f = |k: &str| e.get(k).cloned().unwrap_or(Value::Null);
                    let mut m = Map::new();
                    m.insert("id".into(), f("id"));
                    m.insert("type".into(), f("type"));
                    m.insert("name".into(), f("name"));
                    m.insert("parent".into(), f("parent"));
                    m.insert(
                        "has_logo".into(),
                        Value::Bool(e.get("has_logo").is_some_and(truthy)),
                    );
                    m.insert("symbol".into(), f("symbol"));
                    entity = Value::Object(m);
                }
            }
        }
        let tmux = self.tmux.tmux_for.get(sid);
        let mtime = mtime_secs(v["path"].as_str());
        let now = now_secs();
        let status = if tmux.is_some() || mtime.is_some_and(|m| now - m < 600.0) {
            "active"
        } else {
            "idle"
        };
        let parent_by = |k: &str| {
            self.lineage
                .get(k)
                .filter(|l| truthy(l))
                .and_then(|l| l.get("parent_id"))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let p1 = parent_by(tmux.map(String::as_str).unwrap_or(""));
        let parent_id = if truthy(&p1) { p1 } else { parent_by(sid) };
        let reply = lb.last_reply(s);
        let text = |k: &str, n: usize| {
            Value::String(head(v.get(k).and_then(Value::as_str).unwrap_or(""), n).to_string())
        };
        let or0 = |k: &str| v.get(k).cloned().unwrap_or(Value::from(0));

        let mut r = Map::new();
        r.insert("id".into(), Value::from(sid));
        r.insert("title".into(), Value::from(it.title.clone()));
        r.insert("folder".into(), Value::from(it.folder.clone()));
        r.insert("area".into(), Value::from(it.group.clone()));
        r.insert("msg_count".into(), get("msg_count"));
        r.insert("user_msg_count".into(), or0("user_msg_count"));
        r.insert("assistant_msg_count".into(), or0("assistant_msg_count"));
        r.insert("last_ts".into(), get("last_ts"));
        r.insert("last_assistant_ts".into(), get("last_assistant_ts"));
        r.insert(
            "age_str".into(),
            Value::from(age_str(v["last_ts"].as_str(), now_us)),
        );
        r.insert("momentum".into(), Value::from(self.momentum(sid)));
        r.insert("importance".into(), Value::from(round2(it.score)));
        r.insert("status".into(), Value::from(status));
        r.insert("state".into(), Value::from(self.state(s, mtime, now)));
        r.insert(
            "drift_topic".into(),
            self.classifications
                .get(sid)
                .and_then(|c| c.get("drift_topic"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        r.insert(
            "tmux_session".into(),
            tmux.map(|t| Value::from(t.as_str())).unwrap_or(Value::Null),
        );
        r.insert("first_msg".into(), text("first_msg", 1000));
        r.insert("last_msg".into(), text("last_msg", 1000));
        r.insert(
            "last_reply".into(),
            reply.last_reply.map(Value::from).unwrap_or(Value::Null),
        );
        r.insert(
            "last_role".into(),
            reply.last_role.map(Value::from).unwrap_or(Value::Null),
        );
        r.insert("entity".into(), entity);
        r.insert("entities".into(), entities);
        r.insert("parent_id".into(), parent_id);
        r.insert(
            "origin".into(),
            v.get("origin")
                .filter(|o| truthy(o))
                .cloned()
                .unwrap_or(Value::from("claude")),
        );
        r.insert("subagent_count".into(), Value::from(s.subagents()));
        Row {
            id: sid.to_string(),
            row: r,
            search: head(
                v.get("search_index").and_then(Value::as_str).unwrap_or(""),
                10000,
            )
            .to_string(),
        }
    }
}

/// `group_for_remote(project_filter, 300, 200, 20)`.
pub fn group_for_remote(sessions: &[Session], inp: &Inputs, lb: &ListBuilder) -> Grouped {
    const GROUP_CAP: usize = 300;
    const RECENT_CAP: usize = 200;
    const MOMENTUM_CAP: usize = 20;
    let now = now_us();
    let enriched: Vec<Item> = sessions
        .iter()
        .map(|s| {
            let folder = inp.folder(s);
            Item {
                s,
                title: inp.title(s),
                group: folder_to_group(&folder),
                folder,
                score: inp.importance(s, now),
            }
        })
        .collect();

    let mut by_activity: Vec<&Item> = enriched.iter().collect();
    by_activity
        .sort_by(|a, b| (b.s.last_ts(), b.s.msg_count()).cmp(&(a.s.last_ts(), a.s.msg_count())));
    let recent_ok = |it: &Item| {
        let s = it.s;
        if inp.tmux.tmux_for.contains_key(s.id()) || s.msg_count() >= 20 {
            return true;
        }
        if s.msg_count() <= 2 {
            return false;
        }
        match s.v["last_ts"].as_str().and_then(parse_iso_us) {
            Some(dt) => (now - dt) as f64 / 1e6 / 3600.0 <= 24.0,
            None => false,
        }
    };
    let recent: Vec<&Item> = by_activity
        .into_iter()
        .filter(|it| recent_ok(it))
        .take(RECENT_CAP)
        .collect();
    let recent_ids: HashSet<&str> = recent.iter().map(|it| it.s.id()).collect();

    let mut msorted: Vec<&Item> = enriched
        .iter()
        .filter(|it| !recent_ids.contains(it.s.id()))
        .collect();
    msorted.sort_by_key(|it| std::cmp::Reverse(inp.momentum(it.s.id())));
    let momentum: Vec<&Item> = msorted
        .into_iter()
        .filter(|it| inp.momentum(it.s.id()) >= 5)
        .take(MOMENTUM_CAP)
        .collect();
    let momentum_ids: HashSet<&str> = momentum.iter().map(|it| it.s.id()).collect();

    let mut group_order: Vec<&str> = Vec::new();
    let mut by_group: HashMap<&str, Vec<&Item>> = HashMap::new();
    for it in &enriched {
        let sid = it.s.id();
        if recent_ids.contains(sid) || momentum_ids.contains(sid) {
            continue;
        }
        by_group
            .entry(&it.group)
            .or_insert_with(|| {
                group_order.push(&it.group);
                Vec::new()
            })
            .push(it);
    }
    for g in by_group.values_mut() {
        g.sort_by(|a, b| b.s.last_ts().cmp(a.s.last_ts()));
    }
    let newest = |g: &str| {
        by_group[g]
            .iter()
            .map(|it| it.s.last_ts())
            .max()
            .unwrap_or("")
    };
    let mut sorted_groups = group_order.clone();
    sorted_groups.sort_by(|a, b| newest(b).cmp(newest(a)));

    let rows =
        |items: &[&Item]| -> Vec<Row> { items.iter().map(|it| inp.to_row(it, lb, now)).collect() };
    Grouped {
        areas: sorted_groups
            .iter()
            .map(|g| {
                let items = &by_group[g];
                (g.to_string(), rows(&items[..items.len().min(GROUP_CAP)]))
            })
            .collect(),
        recent: rows(&recent),
        momentum: rows(&momentum),
    }
}

/// The encoded list: what `/api/local-sessions` and `/api/search-index`
/// serve, with their ETags (md5 of the bytes, as in Python).
pub struct Payload {
    pub bytes: Vec<u8>,
    pub etag: String,
    pub search_bytes: Vec<u8>,
    pub search_etag: String,
    /// `_payload_rows`: id → (row, groups), in payload order — for deltas.
    pub rows: Vec<(String, Value, Vec<String>)>,
}

pub fn encode(g: Grouped) -> Payload {
    let mut out = String::with_capacity(1 << 20);
    let mut search = String::with_capacity(1 << 20);
    let mut seen_search: HashSet<String> = HashSet::new();
    let mut rows: Vec<(String, Value, Vec<String>)> = Vec::new();
    let mut row_ix: HashMap<String, usize> = HashMap::new();
    search.push_str("{\"index\": {");
    let mut first_search = true;

    let mut section = |out: &mut String, group: String, items: Vec<Row>| {
        out.push('[');
        for (i, r) in items.into_iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let row = Value::Object(r.row);
            pyjson::write_value(out, &row);
            if !r.search.is_empty() && seen_search.insert(r.id.clone()) {
                if !first_search {
                    search.push_str(", ");
                }
                first_search = false;
                pyjson::write_str(&mut search, &r.id);
                search.push_str(": ");
                pyjson::write_str(&mut search, &r.search);
            }
            match row_ix.get(&r.id) {
                Some(&ix) => rows[ix].2.push(group.clone()),
                None => {
                    row_ix.insert(r.id.clone(), rows.len());
                    rows.push((r.id, row, vec![group.clone()]));
                }
            }
        }
        out.push(']');
    };
    out.push_str("{\"recent\": ");
    section(&mut out, "recent".into(), g.recent);
    out.push_str(", \"momentum\": ");
    section(&mut out, "momentum".into(), g.momentum);
    out.push_str(", \"areas\": {");
    for (i, (name, items)) in g.areas.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        pyjson::write_str(&mut out, &name);
        out.push_str(": ");
        section(&mut out, format!("area:{name}"), items);
    }
    out.push_str("}}");
    search.push_str("}}");
    let bytes = out.into_bytes();
    let search_bytes = search.into_bytes();
    Payload {
        etag: pyjson::md5_hex(&bytes),
        search_etag: pyjson::md5_hex(&search_bytes),
        bytes,
        search_bytes,
        rows,
    }
}

/// `_row_key`: what counts as a change for the live delta (the volatile
/// `age_str` excluded).
pub fn row_key(row: &Value, groups: &[String]) -> String {
    let mut r = row.clone();
    if let Some(m) = r.as_object_mut() {
        m.shift_remove("age_str");
    }
    let mut g = groups.to_vec();
    g.sort();
    format!("{}|{}", pyjson::dumps(&r), g.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_like_python() {
        assert_eq!(fallback_title("(no message)"), "untitled session");
        // Expected values from sessions_core.fallback_title.
        assert_eq!(fallback_title("/Users/me/x/file.md"), "File.md");
        assert_eq!(
            fallback_title("/Users/me/x/ fix the thing"),
            "Fix the thing"
        );
        assert_eq!(
            fallback_title(
                "Implement the following plan:\n\n# Plan: make it fast\n\n## Context\nlots"
            ),
            "Make it fast"
        );
        assert_eq!(fallback_title("## Big  header\ntext"), "Big header text");
        assert_eq!(
            fallback_title(&"word ".repeat(30)),
            "Word word word word word word word word word word word word word wo..."
        );
        assert_eq!(fallback_title("/a/b\\ c/d.txt rest of it"), "Rest of it");
        assert_eq!(fallback_title("/x,y/@z/ "), "@z");
        assert_eq!(fallback_title("ßtraße"), "SStraße");
        assert_eq!(fallback_title("(/path) hello"), "Path) hello");
    }

    #[test]
    fn groups_and_ages() {
        assert_eq!(
            folder_to_group("work/Torwood/portfolio/X"),
            "Torwood Portfolio"
        );
        assert_eq!(folder_to_group("/Network/a"), "Network");
        assert_eq!(folder_to_group("other"), "Other");
        let t = parse_iso_us("2026-09-27T22:59:11.953Z").unwrap();
        assert_eq!(
            age_str(Some("2026-09-27T22:59:11.953Z"), t + 59_000_000),
            "0m"
        );
        assert_eq!(
            age_str(Some("2026-09-27T22:59:11.953Z"), t + 3_600_000_000),
            "1h"
        );
        assert_eq!(
            age_str(Some("2026-09-27T22:59:11.953Z"), t + 86_400_000_000 * 62),
            "2mo"
        );
        assert_eq!(
            age_str(Some("2026-09-27T22:59:11.953Z"), t - 1_000_000),
            "23h"
        );
        assert_eq!(age_str(Some("2026-09-27T22:59:11"), t), "?");
        assert_eq!(age_str(None, t), "?");
        assert_eq!(
            parse_iso_us("2026-09-27T22:59:11.953+02:00").unwrap(),
            t - 7_200_000_000
        );
        assert_eq!(parse_iso_us("1970-01-01T00:00:00Z"), Some(0));
    }
}
