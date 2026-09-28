//! POSIX path helpers with CPython 3.11 semantics: `os.path.realpath`
//! (non-strict), `expanduser`, `splitext`, and `pathlib` glob matching
//! (scandir order, wildcard components matched case-sensitively, literal
//! components checked with a stat — case-insensitive on APFS, like Python).

use std::path::{Path, PathBuf};

/// `os.path.expanduser` for `~` and `~<current user>`; other `~name`
/// forms are left unchanged (as Python does for unknown users).
pub fn expanduser(path: &str, home: &str) -> String {
    if !path.starts_with('~') {
        return path.to_string();
    }
    let i = path[1..].find('/').map(|i| i + 1).unwrap_or(path.len());
    let userhome = if i == 1 {
        home.to_string()
    } else {
        let name = &path[1..i];
        let me = std::env::var("USER").unwrap_or_default();
        let home_name = Path::new(home)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if name == me || name == home_name {
            home.to_string()
        } else {
            return path.to_string();
        }
    };
    let userhome = userhome.trim_end_matches('/');
    let out = format!("{userhome}{}", &path[i..]);
    if out.is_empty() {
        "/".into()
    } else {
        out
    }
}

fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `posixpath.split`.
fn split(p: &str) -> (String, String) {
    let i = p.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (head, tail) = p.split_at(i);
    let mut head = head.to_string();
    if !head.is_empty() && head != "/".repeat(head.len()) {
        head = head.trim_end_matches('/').to_string();
    }
    (head, tail.to_string())
}

/// `posixpath.normpath`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial = if path.starts_with('/') {
        if path.starts_with("//") && !path.starts_with("///") {
            2
        } else {
            1
        }
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let s = comps.join("/");
    let s = format!("{}{}", "/".repeat(initial), s);
    if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

fn is_link(p: &str) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

/// `posixpath._joinrealpath` (strict=False).
fn joinrealpath(
    mut path: String,
    rest: &str,
    seen: &mut std::collections::HashMap<String, Option<String>>,
) -> (String, bool) {
    let mut rest = rest.to_string();
    if rest.starts_with('/') {
        rest = rest[1..].to_string();
        path = "/".into();
    }
    while !rest.is_empty() {
        let (name, r) = match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i + 1..].to_string()),
            None => (rest.clone(), String::new()),
        };
        rest = r;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            if !path.is_empty() {
                let (p, n) = split(&path);
                path = p;
                if n == ".." {
                    path = join(&join(&path, ".."), "..");
                }
            } else {
                path = "..".into();
            }
            continue;
        }
        let newpath = join(&path, &name);
        if !is_link(&newpath) {
            path = newpath;
            continue;
        }
        if let Some(v) = seen.get(&newpath) {
            match v {
                Some(resolved) => {
                    path = resolved.clone();
                    continue;
                }
                None => return (join(&newpath, &rest), false),
            }
        }
        seen.insert(newpath.clone(), None);
        let target = match std::fs::read_link(&newpath) {
            Ok(t) => t.to_string_lossy().into_owned(),
            Err(_) => {
                path = newpath;
                continue;
            }
        };
        let (p, ok) = joinrealpath(path, &target, seen);
        path = p;
        if !ok {
            return (join(&path, &rest), false);
        }
        seen.insert(newpath, Some(path.clone()));
    }
    (path, true)
}

/// `os.path.realpath(path)`; `None` where Python raised (embedded NUL).
pub fn realpath(path: &str) -> Option<String> {
    if path.contains('\0') {
        return None;
    }
    let (p, _) = joinrealpath(String::new(), path, &mut Default::default());
    Some(abspath(&p))
}

/// `os.path.abspath`.
pub fn abspath(p: &str) -> String {
    if p.starts_with('/') {
        normpath(p)
    } else {
        let cwd = std::env::current_dir()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".into());
        normpath(&join(&cwd, p))
    }
}

/// `os.path.splitext(p)[1]`.
pub fn splitext_ext(p: &str) -> &str {
    let sep = p.rfind('/').map(|i| i as isize).unwrap_or(-1);
    let Some(dot) = p.rfind('.') else {
        return "";
    };
    if dot as isize <= sep {
        return "";
    }
    let start = (sep + 1) as usize;
    if p[start..dot].bytes().any(|b| b != b'.') {
        &p[dot..]
    } else {
        ""
    }
}

pub fn basename(p: &str) -> &str {
    &p[p.rfind('/').map(|i| i + 1).unwrap_or(0)..]
}

fn is_wildcard(pat: &str) -> bool {
    pat.contains(['*', '?', '['])
}

/// `fnmatch.fnmatchcase(name, pat)`.
pub fn fnmatch(name: &str, pat: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    fn class_end(p: &[char], i: usize) -> Option<usize> {
        // p[i] == '['; returns index of the closing ']'
        let mut j = i + 1;
        if j < p.len() && p[j] == '!' {
            j += 1;
        }
        if j < p.len() && p[j] == ']' {
            j += 1;
        }
        while j < p.len() && p[j] != ']' {
            j += 1;
        }
        (j < p.len()).then_some(j)
    }
    fn class_match(p: &[char], i: usize, end: usize, c: char) -> bool {
        let mut j = i + 1;
        let neg = p[j] == '!';
        if neg {
            j += 1;
        }
        let mut hit = false;
        while j < end {
            if j + 2 < end && p[j + 1] == '-' {
                hit |= p[j] <= c && c <= p[j + 2];
                j += 3;
            } else {
                hit |= p[j] == c;
                j += 1;
            }
        }
        hit != neg
    }
    fn rec(n: &[char], p: &[char]) -> bool {
        let Some(&pc) = p.first() else {
            return n.is_empty();
        };
        match pc {
            '*' => (0..=n.len()).any(|k| rec(&n[k..], &p[1..])),
            '?' => !n.is_empty() && rec(&n[1..], &p[1..]),
            '[' => match class_end(p, 0) {
                Some(end) => {
                    !n.is_empty() && class_match(p, 0, end, n[0]) && rec(&n[1..], &p[end + 1..])
                }
                None => n.first() == Some(&'[') && rec(&n[1..], &p[1..]),
            },
            c => n.first() == Some(&c) && rec(&n[1..], &p[1..]),
        }
    }
    rec(&n, &p)
}

fn scandir(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect()
}

/// `Path(base).glob(pattern)` in Python 3.11 order. `Err` where pathlib
/// raised (`**` inside a component).
pub fn glob(base: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let parts: Vec<&str> = pattern
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    for p in &parts {
        if p.contains("**") && *p != "**" {
            return Err("Invalid pattern: '**' can only be an entire path component".into());
        }
    }
    let mut out = Vec::new();
    select(base, &parts, &mut out);
    Ok(out)
}

fn select(dir: &Path, parts: &[&str], out: &mut Vec<PathBuf>) {
    let Some(&pat) = parts.first() else {
        out.push(dir.to_path_buf());
        return;
    };
    let last = parts.len() == 1;
    if pat == "**" {
        let mut seen = std::collections::HashSet::new();
        let mut dirs = Vec::new();
        iterate_dirs(dir, &mut dirs);
        for d in dirs {
            let mut sub = Vec::new();
            select(&d, &parts[1..], &mut sub);
            for p in sub {
                if seen.insert(p.clone()) {
                    out.push(p);
                }
            }
        }
        return;
    }
    if is_wildcard(pat) {
        for (name, path) in scandir(dir) {
            if !last && !path.is_dir() {
                continue;
            }
            if fnmatch(&name, pat) {
                select(&path, &parts[1..], out);
            }
        }
    } else {
        let path = dir.join(pat);
        let ok = if last { path.exists() } else { path.is_dir() };
        if ok {
            select(&path, &parts[1..], out);
        }
    }
}

fn iterate_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    out.push(dir.to_path_buf());
    for (_, path) in scandir(dir) {
        let is_dir = path.is_dir();
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if is_dir && !is_link {
            iterate_dirs(&path, out);
        }
    }
}

/// `Path(base).rglob(pattern)` for a single-component pattern, stopping
/// at the first match when `first` is set.
pub fn rglob(base: &Path, pattern: &str, first: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    rglob_into(base, pattern, first, &mut out);
    out
}

fn rglob_into(dir: &Path, pattern: &str, first: bool, out: &mut Vec<PathBuf>) -> bool {
    let entries = scandir(dir);
    for (name, path) in &entries {
        if fnmatch(name, pattern) {
            out.push(path.clone());
            if first {
                return true;
            }
        }
    }
    for (_, path) in &entries {
        let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        if path.is_dir() && !is_link && rglob_into(path, pattern, first, out) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_semantics() {
        assert_eq!(normpath("/a//b/./c/../d/"), "/a/b/d");
        assert_eq!(normpath("//a"), "//a");
        assert_eq!(normpath("/../x"), "/x");
        assert_eq!(splitext_ext("/a/b.tar.gz"), ".gz");
        assert_eq!(splitext_ext("/a/.bashrc"), "");
        assert_eq!(splitext_ext("/a.b/c"), "");
        assert_eq!(expanduser("~/x", "/Users/me"), "/Users/me/x");
        assert!(fnmatch("rollout-2026-abc.jsonl", "rollout-*abc.jsonl"));
        assert!(fnmatch("a.jsonl", "*.jsonl"));
        assert!(!fnmatch("a.json", "*.jsonl"));
        assert!(fnmatch("x1", "x[0-9]"));
        assert!(fnmatch("xa", "x[!0-9]"));
    }
}
