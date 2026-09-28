//! macOS privacy-protected folders (TCC): Desktop, Documents, Downloads,
//! iCloud Drive and removable volumes.
//!
//! Opening a file there from a process that hasn't been granted access
//! doesn't fail — `open()` blocks until the user answers the system prompt,
//! which for a background daemon can be never (a phone request for a
//! transcript that mentions `~/Downloads/x.csv` hung for 30 s+). `stat` is
//! not gated, so the list and path checks still work.
//!
//! [`open_ok`] answers without blocking: a path outside the protected roots
//! is always fine; inside one, the root is probed once on a helper thread
//! and the answer cached. Callers skip the optional read (previews) or
//! answer an error (assets) instead of waiting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Granted,
    /// Denied, or a prompt is pending (the probe didn't return in time).
    Blocked,
}

struct Entry {
    access: Access,
    at: Instant,
    probing: bool,
}

/// How long the caller waits for a first probe of a root.
const PROBE_WAIT: Duration = Duration::from_millis(300);
/// Re-probe a blocked root after this long (the user may have allowed it).
const RETRY_BLOCKED: Duration = Duration::from_secs(60);

fn table() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static T: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The protected root containing `path`, if any.
pub fn protected_root(path: &Path, home: &Path) -> Option<PathBuf> {
    for sub in [
        "Desktop",
        "Documents",
        "Downloads",
        "Library/Mobile Documents",
    ] {
        let root = home.join(sub);
        if path.starts_with(&root) {
            return Some(root);
        }
    }
    let mut c = path.components();
    if let (Some(std::path::Component::RootDir), Some(v), Some(name)) =
        (c.next(), c.next(), c.next())
    {
        if v.as_os_str() == "Volumes" {
            return Some(Path::new("/Volumes").join(name.as_os_str()));
        }
    }
    None
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// True when reading `path` won't block on a privacy prompt.
pub fn open_ok(path: &Path) -> bool {
    match protected_root(path, &home()) {
        None => true,
        Some(root) => access(&root) == Access::Granted,
    }
}

/// The protected root of `path` when reading it would block (not granted).
pub fn blocked_root(path: &Path) -> Option<PathBuf> {
    protected_root(path, &home()).filter(|r| access(r) != Access::Granted)
}

/// Cached access for a protected root; probes it (waiting at most
/// `PROBE_WAIT`) when unknown or when a blocked answer is stale.
pub fn access(root: &Path) -> Access {
    {
        let t = table().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = t.get(root) {
            let fresh = e.access == Access::Granted || e.at.elapsed() < RETRY_BLOCKED;
            if fresh || e.probing {
                return e.access;
            }
        }
    }
    probe(root)
}

fn probe(root: &Path) -> Access {
    {
        let mut t = table().lock().unwrap_or_else(|p| p.into_inner());
        let e = t.entry(root.to_path_buf()).or_insert(Entry {
            access: Access::Blocked,
            at: Instant::now(),
            probing: false,
        });
        if e.probing {
            return e.access;
        }
        e.probing = true;
    }
    let (tx, rx) = mpsc::channel();
    let r = root.to_path_buf();
    // The probe may block for as long as a prompt is up; it records its own
    // answer when it returns, so a later call sees it.
    std::thread::spawn(move || {
        let access = match std::fs::read_dir(&r) {
            Ok(mut d) => {
                let _ = d.next();
                Access::Granted
            }
            // Missing folder or a real error: nothing to wait for, but
            // nothing to read either.
            Err(_) => Access::Blocked,
        };
        let mut t = table().lock().unwrap_or_else(|p| p.into_inner());
        t.insert(
            r,
            Entry {
                access,
                at: Instant::now(),
                probing: false,
            },
        );
        let _ = tx.send(access);
    });
    rx.recv_timeout(PROBE_WAIT).unwrap_or(Access::Blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots() {
        let h = Path::new("/Users/u");
        assert_eq!(
            protected_root(Path::new("/Users/u/Downloads/a.csv"), h),
            Some(PathBuf::from("/Users/u/Downloads"))
        );
        assert_eq!(
            protected_root(Path::new("/Users/u/Library/Mobile Documents/x/y.md"), h),
            Some(PathBuf::from("/Users/u/Library/Mobile Documents"))
        );
        assert_eq!(
            protected_root(Path::new("/Volumes/Ext/f.txt"), h),
            Some(PathBuf::from("/Volumes/Ext"))
        );
        assert_eq!(protected_root(Path::new("/Users/u/GitHub/a.rs"), h), None);
        assert_eq!(protected_root(Path::new("/Users/u/DownloadsX/a"), h), None);
        assert_eq!(protected_root(Path::new("/tmp/a"), h), None);
    }

    #[test]
    fn unprotected_is_ok_and_readable_root_is_granted() {
        assert!(open_ok(Path::new("/tmp/whatever.txt")));
        let d = std::env::temp_dir().join(format!("tcc-probe-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(access(&d), Access::Granted);
        assert_eq!(access(&d.join("missing")), Access::Blocked);
        std::fs::remove_dir_all(&d).ok();
    }
}
