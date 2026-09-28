//! `/api/asset` policy: allowed roots (`csm_config.asset_allowed_roots`),
//! realpath + secret-name deny (`_path_under_allowed_root`,
//! `_is_secret_path`) and content types (`_send_asset`).

use super::paths::{expanduser, realpath};
use fancy_regex::Regex;
use std::sync::OnceLock;

pub const ASSET_MAX_BYTES: u64 = 100 << 20;
pub const ASSET_GZIP_MAX: u64 = 32 << 20;

/// Default roots: the home dir (minus its hidden top-level entries and
/// `~/Library`, see [`AssetPolicy::hide_home_private`]) plus the two
/// hidden dirs Baton itself writes attachments to. Secrets are refused
/// under every root. The Python reference had a fixed personal list here;
/// that list now lives in its user's config (`asset_roots`).
const DEFAULT_ROOTS: [&str; 3] = ["~", "~/.baton/pastes", "~/.claude/plans"];

#[derive(Clone, Debug)]
pub struct AssetPolicy {
    /// `str(Path(p).expanduser())` of each root.
    pub roots: Vec<String>,
    pub home: String,
    /// Default roots only: a path reached through the home-dir root must not
    /// sit under a hidden top-level entry (`~/.ssh`, `~/.config`, …) or
    /// `~/Library` (keychains, mail, cookies).
    pub hide_home_private: bool,
}

/// `str(PurePosixPath(p))`: collapse repeated slashes and "." parts, drop
/// a trailing slash (a leading "//" is kept).
pub fn pathlib_str(p: &str) -> String {
    let lead = if p.starts_with("//") && !p.starts_with("///") {
        "//"
    } else if p.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let s = format!("{lead}{}", parts.join("/"));
    if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

impl AssetPolicy {
    /// Roots from `CSM_ASSET_ROOTS` (comma-separated) or the defaults.
    pub fn new(home: &str, env_roots: Option<&str>) -> Self {
        let raw = env_roots.map(str::trim).unwrap_or("");
        let list: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Self::with_roots(home, (!list.is_empty()).then_some(list))
    }

    /// Explicit roots (`asset_roots` in the config), or the defaults.
    pub fn with_roots(home: &str, roots: Option<Vec<String>>) -> Self {
        let hide_home_private = roots.is_none();
        let list = roots.unwrap_or_else(|| DEFAULT_ROOTS.iter().map(|s| s.to_string()).collect());
        let roots = list
            .iter()
            .map(|r| {
                let p = pathlib_str(r);
                let p = if p.starts_with('~') {
                    let first = p.split('/').next().unwrap_or("~");
                    let expanded = expanduser(first, home);
                    pathlib_str(&format!("{expanded}{}", &p[first.len()..]))
                } else {
                    p
                };
                p
            })
            .collect();
        AssetPolicy {
            roots,
            home: home.to_string(),
            hide_home_private,
        }
    }

    /// `_path_under_allowed_root(path)`.
    pub fn path_under_allowed_root(&self, path: &str) -> Option<String> {
        let expanded = expanduser(path, &self.home);
        let resolved = realpath(&expanded)?;
        if is_secret_path(&resolved) {
            return None;
        }
        let home = if self.hide_home_private {
            realpath(&self.home)
        } else {
            None
        };
        for rroot in self.resolved_roots() {
            if resolved == rroot || resolved.starts_with(&format!("{rroot}/")) {
                if home.as_deref() == Some(rroot.as_str()) {
                    let first = resolved[rroot.len()..]
                        .trim_start_matches('/')
                        .split('/')
                        .next()
                        .unwrap_or("");
                    if first.starts_with('.') || first == "Library" {
                        continue;
                    }
                }
                return Some(resolved);
            }
        }
        None
    }

    /// `os.path.realpath(root)` of every root, re-resolved at most every
    /// 2 s (Python resolves per call; roots are symlinked rarely, and a
    /// request checks hundreds of candidate paths).
    fn resolved_roots(&self) -> Vec<String> {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        type Resolved = Option<(Instant, Vec<String>, Vec<String>)>;
        static CACHE: Mutex<Resolved> = Mutex::new(None);
        let mut c = CACHE.lock().expect("roots cache");
        if let Some((at, roots, resolved)) = c.as_ref() {
            if at.elapsed() < Duration::from_secs(2) && *roots == self.roots {
                return resolved.clone();
            }
        }
        let resolved: Vec<String> = self.roots.iter().filter_map(|r| realpath(r)).collect();
        *c = Some((Instant::now(), self.roots.clone(), resolved.clone()));
        resolved
    }
}

/// `_is_secret_path(resolved)`.
pub fn is_secret_path(resolved: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\A(\.env(\..*)?|\.envrc|\.netrc|\.npmrc|\.pypirc|auth\.json|credentials(\..*)?|",
            r"\.credentials\.json|secrets?(\..*)?|id_(rsa|dsa|ecdsa|ed25519)(\.pub)?|",
            r".*\.(p8|p12|pfx|pem|key|keystore|jks|mobileprovision|kdbx))\n?\z"
        ))
        .expect("secret regex")
    });
    let parts: Vec<&str> = resolved.split('/').collect();
    let (last, dirs) = parts.split_last().expect("split yields one part");
    if dirs.contains(&".git") || parts.contains(&".ssh") || parts.contains(&".gnupg") {
        return true;
    }
    re.is_match(last).unwrap_or(false)
}

/// `_send_asset`'s Content-Type by (lowercased) extension.
pub fn content_type(ext: &str) -> &'static str {
    match ext {
        ".md" | ".markdown" => "text/markdown; charset=utf-8",
        ".txt" => "text/plain; charset=utf-8",
        ".csv" => "text/csv; charset=utf-8",
        ".json" => "application/json; charset=utf-8",
        ".yaml" | ".yml" => "text/yaml; charset=utf-8",
        ".html" | ".htm" => "text/html; charset=utf-8",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".heic" => "image/heic",
        ".pdf" => "application/pdf",
        ".swift" => "text/x-swift; charset=utf-8",
        ".py" => "text/x-python; charset=utf-8",
        ".sh" => "text/x-shellscript; charset=utf-8",
        ".js" => "text/javascript; charset=utf-8",
        ".ts" | ".tsx" => "text/typescript; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets() {
        for p in [
            "/a/.env",
            "/a/.env.local",
            "/a/.git/config",
            "/a/x.P8",
            "/a/id_rsa.pub",
            "/a/.ssh/known",
            "/a/secrets.yaml",
            "/a/credentials",
        ] {
            assert!(is_secret_path(p), "{p}");
        }
        for p in ["/a/.git", "/a/env.md", "/a/notes.md", "/a/keys.md"] {
            assert!(!is_secret_path(p), "{p}");
        }
        assert_eq!(pathlib_str("/tmp/"), "/tmp");
        assert_eq!(pathlib_str("~/a//./b"), "~/a/b");
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn default_roots_are_home_minus_private() {
        let base = std::env::temp_dir().join(format!("baton-assets-{}", std::process::id()));
        let home = base.join("home");
        let files = [
            "Documents/a.txt",
            "Music/b.txt",
            ".ssh/c.txt",
            ".config/baton/token.txt",
            "Library/Keychains/k.txt",
            ".baton/pastes/p.txt",
            ".claude/plans/q.md",
            "proj/.env",
        ];
        for f in files {
            let p = home.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
        }
        std::fs::write(base.join("outside.txt"), "x").unwrap();
        let h = home.to_string_lossy().into_owned();
        let pol = AssetPolicy::with_roots(&h, None);
        let ok = |f: &str| pol.path_under_allowed_root(&format!("{h}/{f}")).is_some();
        assert!(ok("Documents/a.txt") && ok("Music/b.txt"));
        assert!(ok(".baton/pastes/p.txt") && ok(".claude/plans/q.md"));
        assert!(!ok(".ssh/c.txt") && !ok(".config/baton/token.txt"));
        assert!(!ok("Library/Keychains/k.txt") && !ok("proj/.env"));
        assert!(pol
            .path_under_allowed_root(&base.join("outside.txt").to_string_lossy())
            .is_none());
        // Explicit roots: exactly those (secrets still refused).
        let pol = AssetPolicy::with_roots(&h, Some(vec!["~/Documents".into(), "~/.config".into()]));
        let ok = |f: &str| pol.path_under_allowed_root(&format!("{h}/{f}")).is_some();
        assert!(ok("Documents/a.txt") && ok(".config/baton/token.txt"));
        assert!(!ok("Music/b.txt"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
