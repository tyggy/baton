//! Process-wide settings the engine reads (docs/config.md). The daemon
//! resolves them once at start-up (flags > env > config file > defaults)
//! and installs them here; library callers and tests that never install
//! anything get the defaults, which derive every path from `$HOME`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Claude Code's data dir (`projects/`, `tty_session_map.json`, …).
    /// `None` = `$HOME/.claude`.
    pub claude_dir: Option<PathBuf>,
    /// Codex's data dir (`sessions/`). `None` = `$HOME/.codex`.
    pub codex_dir: Option<PathBuf>,
    /// `/api/asset` roots. `None` = the defaults (the home dir minus hidden
    /// top-level entries and `~/Library`, plus Baton's own paste / plan dirs;
    /// secrets are always refused).
    pub asset_roots: Option<Vec<String>>,
    /// Optional personal knowledge base (holds `sessions.nosync/` markdown
    /// exports). Off unless set.
    pub kb_path: Option<PathBuf>,
    /// Project dir name (as under `~/.claude/projects`) → group label.
    pub project_groups: Vec<(String, String)>,
    /// Read the entity catalog (`~/.baton/entities.json`,
    /// `~/.baton/session_entities.json`). Off unless enabled.
    pub entities: bool,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Install the resolved settings (first call wins).
pub fn install(s: Settings) {
    let _ = SETTINGS.set(s);
}

pub fn get() -> &'static Settings {
    static DEFAULT: OnceLock<Settings> = OnceLock::new();
    SETTINGS
        .get()
        .unwrap_or_else(|| DEFAULT.get_or_init(Settings::default))
}

/// The Claude data dir for `home` (configured, else `home/.claude`).
pub fn claude_dir(home: &Path) -> PathBuf {
    get()
        .claude_dir
        .clone()
        .unwrap_or_else(|| home.join(".claude"))
}

/// The Codex data dir for `home` (configured, else `home/.codex`).
pub fn codex_dir(home: &Path) -> PathBuf {
    get()
        .codex_dir
        .clone()
        .unwrap_or_else(|| home.join(".codex"))
}

/// A cwd as Claude Code names its project dir: every character other than
/// ASCII letters, digits and `-` becomes `-` (`/Users/me/x.y` →
/// `-Users-me-x-y`).
pub fn project_dir_name(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A `project_groups` key as a project dir name: paths (`~/…`, `/…`) are
/// expanded and encoded; anything else is taken as a dir name already,
/// with the `area_map.json` placeholders `{home}` (`-Users-<user>`) and
/// `{prefix}` (`-Users-<user>-Documents-`) filled in.
pub fn group_key(key: &str, home: &Path) -> String {
    if key.contains('{') {
        let user = home
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        return key
            .replace("{home}", &format!("-Users-{user}"))
            .replace("{prefix}", &format!("-Users-{user}-Documents-"));
    }
    if let Some(rest) = key.strip_prefix("~/") {
        project_dir_name(&home.join(rest).to_string_lossy())
    } else if key == "~" {
        project_dir_name(&home.to_string_lossy())
    } else if key.starts_with('/') {
        project_dir_name(key)
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let home = Path::new("/Users/me");
        assert_eq!(group_key("~/GitHub/x.y", home), "-Users-me-GitHub-x-y");
        assert_eq!(group_key("/opt/a_b", home), "-opt-a-b");
        assert_eq!(group_key("-Users-me-proj", home), "-Users-me-proj");
        assert_eq!(group_key("~", home), "-Users-me");
        assert_eq!(
            group_key("{prefix}GitHub-a-b", home),
            "-Users-me-Documents-GitHub-a-b"
        );
    }
}
