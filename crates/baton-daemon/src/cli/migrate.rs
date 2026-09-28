//! Detect the Python prototype's install (the `com.workbench.cs-server`
//! LaunchAgent, `~/.claude-sessions/config`) and derive the daemon config
//! that behaves the same (docs/migrating.md). Read-only: the plist is read
//! with `plutil -convert json -o -`, nothing is modified or unloaded.
//!
//! The prototype's built-in defaults (asset roots, knowledge-base path,
//! project groups) are read from its own source next to the server script
//! the plist runs, so none of them are baked into this binary.

use crate::config::{home_relative, Layer, DEFAULT_PORT, SIDECAR_PORT};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LEGACY_LABEL: &str = "com.workbench.cs-server";

#[derive(Debug, Default)]
pub struct Legacy {
    pub plist: Option<PathBuf>,
    pub port: Option<u16>,
    /// `claude-session-manager/` (the dir holding `server/api_server.py`).
    pub ref_dir: Option<PathBuf>,
    /// `CSM_*` values the server sees (plist env, then `CSM_ENV_FILE`s).
    pub env: BTreeMap<String, String>,
    /// `~/.claude-sessions/config` holds `BATON_BEARER` (the daemon reads it
    /// as a fallback; setup keeps that token instead of making a new one).
    pub token_file: Option<PathBuf>,
}

pub fn legacy_plist(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LEGACY_LABEL}.plist"))
}

/// `None` when there's no trace of the Python server.
pub fn detect(home: &Path) -> Option<Legacy> {
    let mut l = Legacy::default();
    let plist = legacy_plist(home);
    if plist.exists() {
        let out = super::run(
            "/usr/bin/plutil",
            &["-convert", "json", "-o", "-", &plist.to_string_lossy()],
            Duration::from_secs(5),
        );
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out.stdout) {
            let args: Vec<String> = v["ProgramArguments"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            l.port = parse_port(&args);
            l.ref_dir = args
                .iter()
                .find(|a| a.ends_with("api_server.py"))
                .and_then(|s| Path::new(s).parent()?.parent().map(Path::to_path_buf));
            if let Some(env) = v["EnvironmentVariables"].as_object() {
                for (k, x) in env {
                    if let Some(s) = x.as_str() {
                        l.env.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
        l.plist = Some(plist);
        // CSM_ENV_FILE: earlier files win, the process env wins over all.
        if let Some(files) = l.env.get("CSM_ENV_FILE").cloned() {
            for piece in files.split(':').map(str::trim).filter(|p| !p.is_empty()) {
                let path = expand_in(piece, home);
                for (k, v) in env_file(&path) {
                    l.env.entry(k).or_insert(v);
                }
            }
        }
    }
    let legacy_cfg = crate::state::legacy_config(home);
    if crate::state::legacy_value(&legacy_cfg, "BATON_BEARER").is_some_and(|t| !t.is_empty()) {
        l.token_file = Some(legacy_cfg);
    }
    (l.plist.is_some() || l.token_file.is_some()).then_some(l)
}

fn expand_in(p: &str, home: &Path) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(p),
    }
}

/// `--port N`, else a bare number argument (the old positional form).
fn parse_port(args: &[String]) -> Option<u16> {
    if let Some(i) = args.iter().position(|a| a == "--port") {
        return args.get(i + 1)?.parse().ok();
    }
    args.iter().skip(1).find_map(|a| a.parse().ok())
}

/// `KEY=value` lines of a shell env file (`export`, quotes stripped).
fn env_file(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let l = l.strip_prefix("export ").unwrap_or(l).trim_start();
            let (k, v) = l.split_once('=')?;
            Some((
                k.trim().to_string(),
                v.trim().trim_matches('"').trim_matches('\'').to_string(),
            ))
        })
        .collect()
}

/// The prototype's `_env_list("CSM_ASSET_ROOTS", [...])` default.
fn source_asset_roots(csm_config: &str) -> Option<Vec<String>> {
    let re = Regex::new(r#"(?s)_env_list\(\s*"CSM_ASSET_ROOTS"\s*,\s*\[(.*?)\]\s*\)"#).ok()?;
    let body = re.captures(csm_config)?.get(1)?.as_str();
    let item = Regex::new(r#""([^"]+)""#).ok()?;
    let roots: Vec<String> = body
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(|l| {
            item.captures_iter(l)
                .map(|c| c[1].to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    (!roots.is_empty()).then_some(roots)
}

/// The prototype's `_env_path("CSM_BASE_PATH", "...")` default.
fn source_base_path(csm_config: &str) -> Option<String> {
    let re = Regex::new(r#"_env_path\(\s*"CSM_BASE_PATH"\s*,\s*"([^"]+)"\s*\)"#).ok()?;
    Some(re.captures(csm_config)?[1].to_string())
}

/// The prototype's hard-coded `AREA_MAP` entries, as `project_groups` keys
/// with the `{home}` / `{prefix}` placeholders (home-independent).
fn source_area_map(sessions_core: &str) -> Vec<(String, String)> {
    let Some(start) = sessions_core.find("AREA_MAP = {") else {
        return vec![];
    };
    // The dict ends at a `}` on its own line (`{_HOME_PREFIX}` has braces).
    let rest = &sessions_core[start..];
    let block = &rest[..rest.find("\n}").unwrap_or(rest.len())];
    let re = Regex::new(r#"f?"([^"]+)"\s*:\s*"([^"]+)""#).expect("static regex");
    re.captures_iter(block)
        .map(|c| {
            let key = c[1]
                .replace("{_HOME_PREFIX}", "{prefix}")
                .replace("-Users-{_HOME_USER}", "{home}");
            (key, c[2].to_string())
        })
        .filter(|(k, v)| !(k == "{home}" && v == "Home")) // built in
        .filter(|(k, _)| !k.contains("{_")) // placeholders we can't map
        .collect()
}

/// The config that reproduces the prototype's behaviour, plus notes for
/// the user. The daemon runs beside the Python server (which keeps its
/// port) until the cut-over described in docs/migrating.md.
pub fn layer(l: &Legacy) -> (Layer, Vec<String>) {
    let mut notes = Vec::new();
    let mut out = Layer::default();
    if l.plist.is_none() {
        return (out, notes);
    }
    let port = l.port.unwrap_or(DEFAULT_PORT);
    out.upstream = Some(format!("http://127.0.0.1:{port}"));
    let listen_port = if port == SIDECAR_PORT {
        DEFAULT_PORT
    } else {
        SIDECAR_PORT
    };
    out.listen = Some(format!("127.0.0.1:{listen_port}"));
    notes.push(format!(
        "Python server keeps :{port}; the daemon listens on 127.0.0.1:{listen_port} and proxies to it (cut-over: docs/migrating.md)"
    ));
    let read = |name: &str| {
        l.ref_dir
            .as_ref()
            .and_then(|d| std::fs::read_to_string(d.join(name)).ok())
    };
    let csm_config = read("csm_config.py");
    let sessions_core = read("sessions_core.py");
    if l.ref_dir.is_some() && csm_config.is_none() {
        notes.push("could not read the Python server's csm_config.py; asset roots / kb path left at the daemon defaults".into());
    }
    out.asset_roots = match l
        .env
        .get("CSM_ASSET_ROOTS")
        .filter(|v| !v.trim().is_empty())
    {
        Some(v) => Some(
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        ),
        None => csm_config.as_deref().and_then(source_asset_roots),
    };
    out.kb_path = l
        .env
        .get("CSM_BASE_PATH")
        .filter(|v| !v.trim().is_empty())
        .cloned()
        .or_else(|| csm_config.as_deref().and_then(source_base_path));
    if l.env.contains_key("CSM_SESSIONS_CACHE_DIR") {
        notes.push(
            "CSM_SESSIONS_CACHE_DIR is set for the Python server; export it for the daemon too (it is still honoured)".into(),
        );
    }
    // The prototype always read the entity catalog when present.
    out.entities = Some(true);
    let groups = sessions_core
        .as_deref()
        .map(source_area_map)
        .unwrap_or_default();
    if !groups.is_empty() {
        out.project_groups = Some(groups);
    }
    if let Some(t) = &l.token_file {
        notes.push(format!(
            "bearer token: kept in {} (already paired phones keep working)",
            home_relative(t)
        ));
    }
    (out, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_prototype_defaults_from_source() {
        let cfg = r#"
    base_path: Path = field(default_factory=lambda: _env_path("CSM_BASE_PATH", "~/Notes/KB"))
            for p in _env_list("CSM_ASSET_ROOTS",
                               ["~/Documents", "/tmp",
                                # comment "~/not-this"
                                "~/code", "~/.baton/pastes"])
"#;
        assert_eq!(
            source_asset_roots(cfg).unwrap(),
            vec!["~/Documents", "/tmp", "~/code", "~/.baton/pastes"]
        );
        assert_eq!(source_base_path(cfg).unwrap(), "~/Notes/KB");
        let core = r#"
_HOME_PREFIX = f"-Users-{_HOME_USER}-Documents-"
AREA_MAP = {
    f"{_HOME_PREFIX}GitHub-some-repo": "Repo",
    f"-Users-{_HOME_USER}": "Home",
}
"#;
        assert_eq!(
            source_area_map(core),
            vec![("{prefix}GitHub-some-repo".to_string(), "Repo".to_string())]
        );
        assert_eq!(
            parse_port(&["py".into(), "x.py".into(), "--port".into(), "19877".into()]),
            Some(19877)
        );
        assert_eq!(
            parse_port(&["py".into(), "x.py".into(), "19877".into()]),
            Some(19877)
        );
    }
}
