//! The daemon's configuration (docs/config.md).
//!
//! Sources, highest first: command-line flags, environment variables, the
//! config file(s), built-in defaults. Files:
//!
//! 1. `$XDG_CONFIG_HOME/baton/config.toml` (default `~/.config/baton/`),
//!    written by `baton setup`;
//! 2. `--config FILE` (or `BATON_CONFIG`), layered on top — TOML, or the
//!    `key=value` lines the Mac app writes to `daemon.conf`.
//!
//! Nothing personal is built in: every path defaults to something derived
//! from `$HOME`, and personal integrations (knowledge base, entity catalog)
//! stay off until configured.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The one default port. Phones paired with the Python prototype talk to
/// this port, so it stays.
pub const DEFAULT_PORT: u16 = 19877;
/// Where the daemon listens when it runs beside the Python server (which
/// then keeps [`DEFAULT_PORT`]); the Mac app's default until the cut-over.
pub const SIDECAR_PORT: u16 = 19880;

pub fn default_listen() -> String {
    format!("127.0.0.1:{DEFAULT_PORT}")
}

pub const KEYS: [&str; 9] = [
    "listen",
    "upstream",
    "relay",
    "claude_dir",
    "codex_dir",
    "asset_roots",
    "kb_path",
    "project_groups",
    "entities",
];

/// Where a value came from (shown by `baton doctor`).
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Default,
    File(PathBuf),
    Env(&'static str),
    Flag,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Default => write!(f, "default"),
            Source::File(p) => write!(f, "{}", home_relative(p)),
            Source::Env(v) => write!(f, "${v}"),
            Source::Flag => write!(f, "flag"),
        }
    }
}

/// One layer's values; `None` = not set here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    pub listen: Option<String>,
    /// `Some("")` = explicitly none.
    pub upstream: Option<String>,
    pub relay: Option<String>,
    pub claude_dir: Option<String>,
    pub codex_dir: Option<String>,
    pub asset_roots: Option<Vec<String>>,
    pub kb_path: Option<String>,
    pub project_groups: Option<Vec<(String, String)>>,
    pub entities: Option<bool>,
    /// Keys this layer didn't understand (reported, never fatal).
    pub unknown: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: String,
    pub upstream: Option<String>,
    pub relay: Option<String>,
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    /// `None` = the default roots (see `baton_core::read::assets`).
    pub asset_roots: Option<Vec<String>>,
    pub kb_path: Option<PathBuf>,
    pub project_groups: Vec<(String, String)>,
    pub entities: bool,
    pub sources: BTreeMap<&'static str, Source>,
    /// Config files that were read.
    pub files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `~/…` → `$HOME/…`.
pub fn expand(p: &str) -> PathBuf {
    let home = home();
    if p == "~" {
        return home;
    }
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(p),
    }
}

/// `$HOME/x` → `~/x` for display (and bug-report bundles).
pub fn home_relative(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) if !h.as_os_str().is_empty() => {
            if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            }
        }
        _ => p.display().to_string(),
    }
}

/// `$XDG_CONFIG_HOME/baton`, else `~/.config/baton`.
pub fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() && Path::new(&x).is_absolute() => PathBuf::from(x).join("baton"),
        _ => home().join(".config/baton"),
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// The bearer token file `baton setup` creates (0600).
pub fn token_path() -> PathBuf {
    config_dir().join("token")
}

/// The Mac app's config (fixed `--config` argument of its LaunchAgent).
pub fn mac_app_conf() -> PathBuf {
    home().join("Library/Application Support/Baton/daemon.conf")
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Normalise an upstream value: `""`, `none`, `off` = explicitly none.
fn upstream_value(v: &str) -> String {
    let v = v.trim();
    match v.to_ascii_lowercase().as_str() {
        "none" | "off" | "false" => String::new(),
        _ => v.to_string(),
    }
}

impl Layer {
    /// A TOML document (`config.toml`).
    pub fn from_toml(text: &str) -> Result<Layer, String> {
        let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut l = Layer::default();
        let s = |v: &toml::Value| -> Result<String, String> {
            v.as_str()
                .map(String::from)
                .ok_or_else(|| format!("expected a string, got {v}"))
        };
        for (k, v) in &table {
            match k.as_str() {
                "listen" => l.listen = Some(s(v)?),
                "upstream" => l.upstream = Some(upstream_value(&s(v)?)),
                "relay" => l.relay = Some(s(v)?),
                "claude_dir" => l.claude_dir = Some(s(v)?),
                "codex_dir" => l.codex_dir = Some(s(v)?),
                "kb_path" => l.kb_path = Some(s(v)?),
                "entities" => {
                    l.entities = Some(
                        v.as_bool()
                            .ok_or_else(|| "entities: expected true or false".to_string())?,
                    )
                }
                "asset_roots" => {
                    let arr = v
                        .as_array()
                        .ok_or_else(|| "asset_roots: expected a list of paths".to_string())?;
                    l.asset_roots = Some(arr.iter().map(s).collect::<Result<_, _>>()?);
                }
                "project_groups" => {
                    let t = v.as_table().ok_or_else(|| {
                        "project_groups: expected a table of \"path\" = \"Group\"".to_string()
                    })?;
                    l.project_groups = Some(
                        t.iter()
                            .map(|(k, v)| s(v).map(|v| (k.clone(), v)))
                            .collect::<Result<_, _>>()?,
                    );
                }
                other => l.unknown.push(other.to_string()),
            }
        }
        Ok(l)
    }

    /// `key=value` lines (`daemon.conf`, the pre-TOML `--config` format).
    /// Lists are comma-separated; groups are `project_groups.<key>=Label`.
    /// Empty values are ignored, except `upstream=` (explicitly none).
    pub fn from_kv(text: &str) -> Layer {
        let mut l = Layer::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            if v.is_empty() && k != "upstream" {
                continue;
            }
            match k {
                "listen" => l.listen = Some(v.into()),
                "upstream" if v.is_empty() => {} // unset, as before
                "upstream" => l.upstream = Some(upstream_value(v)),
                "relay" => l.relay = Some(v.into()),
                "claude_dir" => l.claude_dir = Some(v.into()),
                "codex_dir" => l.codex_dir = Some(v.into()),
                "kb_path" => l.kb_path = Some(v.into()),
                "asset_roots" => l.asset_roots = Some(split_list(v)),
                "entities" => l.entities = parse_bool(v),
                _ => match k.strip_prefix("project_groups.") {
                    Some(key) if !key.is_empty() => l
                        .project_groups
                        .get_or_insert_with(Vec::new)
                        .push((key.to_string(), v.to_string())),
                    _ => l.unknown.push(k.to_string()),
                },
            }
        }
        l
    }

    /// A config file: TOML when it parses as TOML (or is named `*.toml`),
    /// else `key=value` lines.
    pub fn from_file(path: &Path) -> Result<Option<Layer>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        match Layer::from_toml(&text) {
            Ok(l) => Ok(Some(l)),
            Err(e) if path.extension().is_some_and(|x| x == "toml") => {
                Err(format!("{}: {e}", path.display()))
            }
            Err(_) => Ok(Some(Layer::from_kv(&text))),
        }
    }

    /// `BATON_*` environment variables (plus the Python server's
    /// `CSM_ASSET_ROOTS` / `CSM_BASE_PATH`, which are also honoured by the
    /// read path directly).
    pub fn from_env() -> (Layer, BTreeMap<&'static str, &'static str>) {
        let mut l = Layer::default();
        let mut names = BTreeMap::new();
        let get = |n: &str| std::env::var(n).ok();
        let mut set_str = |key: &'static str, vars: &[&'static str], slot: &mut Option<String>| {
            for &var in vars {
                if let Some(v) = get(var) {
                    if !v.trim().is_empty() || key == "upstream" {
                        *slot = Some(v.trim().to_string());
                        names.insert(key, var);
                        return;
                    }
                }
            }
        };
        set_str("listen", &["BATON_LISTEN"], &mut l.listen);
        set_str("upstream", &["BATON_UPSTREAM"], &mut l.upstream);
        set_str("relay", &["BATON_RELAY_URL"], &mut l.relay);
        set_str("claude_dir", &["BATON_CLAUDE_DIR"], &mut l.claude_dir);
        set_str("codex_dir", &["BATON_CODEX_DIR"], &mut l.codex_dir);
        set_str(
            "kb_path",
            &["BATON_KB_PATH", "CSM_BASE_PATH"],
            &mut l.kb_path,
        );
        let mut roots = None;
        set_str(
            "asset_roots",
            &["BATON_ASSET_ROOTS", "CSM_ASSET_ROOTS"],
            &mut roots,
        );
        l.asset_roots = roots.map(|r| split_list(&r));
        if let Some(u) = &mut l.upstream {
            *u = upstream_value(u);
        }
        if let Some(v) = get("BATON_ENTITIES") {
            l.entities = parse_bool(&v);
            if l.entities.is_some() {
                names.insert("entities", "BATON_ENTITIES");
            }
        }
        (l, names)
    }

    /// `baton serve` flags.
    pub fn from_flags(args: &[String]) -> Layer {
        Layer {
            listen: flag(args, "--listen"),
            upstream: flag(args, "--upstream").map(|u| upstream_value(&u)),
            relay: flag(args, "--relay"),
            claude_dir: flag(args, "--claude-dir"),
            codex_dir: flag(args, "--codex-dir"),
            kb_path: flag(args, "--kb-path"),
            ..Layer::default()
        }
    }

    /// Which keys this layer sets.
    pub fn keys(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let set = [
            self.listen.is_some(),
            self.upstream.is_some(),
            self.relay.is_some(),
            self.claude_dir.is_some(),
            self.codex_dir.is_some(),
            self.asset_roots.is_some(),
            self.kb_path.is_some(),
            self.project_groups.is_some(),
            self.entities.is_some(),
        ];
        for (k, on) in KEYS.iter().zip(set) {
            if on {
                out.push(*k);
            }
        }
        out
    }

    /// `other` on top of `self`.
    fn overlay(&mut self, other: Layer) {
        macro_rules! take {
            ($($f:ident),*) => {$( if other.$f.is_some() { self.$f = other.$f; } )*};
        }
        take!(
            listen,
            upstream,
            relay,
            claude_dir,
            codex_dir,
            asset_roots,
            kb_path,
            project_groups,
            entities
        );
    }
}

impl Config {
    /// Resolve from `args` (flags and `--config`), the environment and the
    /// config files.
    pub fn load(args: &[String]) -> Config {
        let mut merged = Layer::default();
        let mut sources: BTreeMap<&'static str, Source> =
            KEYS.iter().map(|k| (*k, Source::Default)).collect();
        let mut files = Vec::new();
        let mut warnings = Vec::new();
        let mut file_layers = vec![config_path()];
        if let Some(p) = flag(args, "--config").or_else(|| std::env::var("BATON_CONFIG").ok()) {
            if !p.is_empty() {
                file_layers.push(expand(&p));
            }
        }
        for path in file_layers {
            match Layer::from_file(&path) {
                Ok(Some(l)) => {
                    for k in &l.unknown {
                        warnings.push(format!("{}: unknown key `{k}`", home_relative(&path)));
                    }
                    for k in l.keys() {
                        sources.insert(k, Source::File(path.clone()));
                    }
                    merged.overlay(l);
                    files.push(path);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("config ignored: {e}")),
            }
        }
        let (env, names) = Layer::from_env();
        for k in env.keys() {
            sources.insert(k, Source::Env(names.get(k).copied().unwrap_or("?")));
        }
        merged.overlay(env);
        let flags = Layer::from_flags(args);
        for k in flags.keys() {
            sources.insert(k, Source::Flag);
        }
        merged.overlay(flags);
        Config::from_layer(merged, sources, files, warnings)
    }

    pub fn from_layer(
        l: Layer,
        sources: BTreeMap<&'static str, Source>,
        files: Vec<PathBuf>,
        mut warnings: Vec<String>,
    ) -> Config {
        let home = home();
        let mut listen = l.listen.unwrap_or_else(default_listen);
        if listen.parse::<std::net::SocketAddr>().is_err() {
            warnings.push(format!(
                "listen `{listen}` is not host:port; using {}",
                default_listen()
            ));
            listen = default_listen();
        }
        let upstream = l
            .upstream
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string());
        let relay = l.relay.filter(|r| !r.trim().is_empty());
        Config {
            listen,
            upstream,
            relay,
            claude_dir: l
                .claude_dir
                .map(|p| expand(&p))
                .unwrap_or_else(|| home.join(".claude")),
            codex_dir: l
                .codex_dir
                .map(|p| expand(&p))
                .unwrap_or_else(|| home.join(".codex")),
            asset_roots: l.asset_roots,
            kb_path: l
                .kb_path
                .filter(|p| !p.trim().is_empty())
                .map(|p| expand(&p)),
            project_groups: l.project_groups.unwrap_or_default(),
            entities: l.entities.unwrap_or(false),
            sources,
            files,
            warnings,
        }
    }

    /// Hand the engine settings to baton-core (once, at start-up).
    pub fn install(&self) {
        baton_core::settings::install(baton_core::settings::Settings {
            claude_dir: Some(self.claude_dir.clone()),
            codex_dir: Some(self.codex_dir.clone()),
            asset_roots: self.asset_roots.clone(),
            kb_path: self.kb_path.clone(),
            project_groups: self.project_groups.clone(),
            entities: self.entities,
        });
    }

    pub fn port(&self) -> u16 {
        self.listen
            .parse::<std::net::SocketAddr>()
            .map(|a| a.port())
            .unwrap_or(DEFAULT_PORT)
    }

    /// Reachable from other devices (not loopback-only)?
    pub fn listens_publicly(&self) -> bool {
        self.listen
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|a| !a.ip().is_loopback())
    }

    /// `http://127.0.0.1:<port>` style base for local clients.
    pub fn local_base(&self) -> String {
        match self.listen.parse::<std::net::SocketAddr>() {
            Ok(a) if a.ip().is_unspecified() => format!("http://127.0.0.1:{}", a.port()),
            Ok(a) => format!("http://{a}"),
            Err(_) => format!("http://127.0.0.1:{DEFAULT_PORT}"),
        }
    }

    /// The resolved config as TOML (for `baton doctor` and `setup`).
    pub fn to_toml(&self) -> String {
        render(&Layer {
            listen: Some(self.listen.clone()),
            upstream: Some(self.upstream.clone().unwrap_or_default()),
            relay: self.relay.clone(),
            claude_dir: Some(home_relative(&self.claude_dir)),
            codex_dir: Some(home_relative(&self.codex_dir)),
            asset_roots: self.asset_roots.clone(),
            kb_path: self.kb_path.as_deref().map(home_relative),
            project_groups: Some(self.project_groups.clone()).filter(|g| !g.is_empty()),
            entities: Some(self.entities),
            unknown: vec![],
        })
    }
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// A layer as a commented `config.toml` (what `baton setup` writes). Keys
/// that are unset are written as comments showing the default.
pub fn render(l: &Layer) -> String {
    let mut o = String::new();
    o.push_str("# Baton daemon config — see docs/config.md.\n");
    o.push_str("# Precedence: command-line flags > BATON_* env vars > this file > defaults.\n\n");
    let line = |o: &mut String, doc: &str, key: &str, val: Option<String>, default: &str| {
        o.push_str(&format!("# {doc}\n"));
        match val {
            Some(v) => o.push_str(&format!("{key} = {v}\n\n")),
            None => o.push_str(&format!("# {key} = {default}\n\n")),
        }
    };
    line(
        &mut o,
        "Address the daemon listens on. 0.0.0.0 lets phones on your LAN/tailnet connect directly.",
        "listen",
        l.listen.as_deref().map(q),
        &q(&default_listen()),
    );
    line(
        &mut o,
        "Python reference server for endpoints the daemon doesn't implement yet (\"\" = none: those return 501).",
        "upstream",
        l.upstream.as_deref().map(q),
        "\"\"",
    );
    line(
        &mut o,
        "Push relay for notifications (docs/relay.md). Unset = no push.",
        "relay",
        l.relay.as_deref().map(q),
        "\"https://relay.example.com\"",
    );
    line(
        &mut o,
        "Claude Code data dir.",
        "claude_dir",
        l.claude_dir.as_deref().map(q),
        "\"~/.claude\"",
    );
    line(
        &mut o,
        "Codex data dir.",
        "codex_dir",
        l.codex_dir.as_deref().map(q),
        "\"~/.codex\"",
    );
    line(
        &mut o,
        "Directories /api/asset may serve files from (secrets are always refused).\n# Default: your home dir minus hidden dirs and ~/Library, plus ~/.baton/pastes and ~/.claude/plans.",
        "asset_roots",
        l.asset_roots.as_ref().map(|r| {
            if r.is_empty() {
                "[]".to_string()
            } else {
                format!(
                    "[\n{}]",
                    r.iter().map(|x| format!("  {},\n", q(x))).collect::<String>()
                )
            }
        }),
        "[\"~\", \"~/.baton/pastes\", \"~/.claude/plans\"]",
    );
    line(
        &mut o,
        "Optional personal knowledge base (session markdown exports in <kb_path>/sessions.nosync). Off unless set.",
        "kb_path",
        l.kb_path.as_deref().map(q),
        "\"~/Notes\"",
    );
    line(
        &mut o,
        "Optional entity catalog integration (~/.baton/entities.json).",
        "entities",
        l.entities.map(|b| b.to_string()),
        "false",
    );
    o.push_str("# Group labels for projects: a path (or a ~/.claude/projects dir name) → label.\n");
    match &l.project_groups {
        Some(g) if !g.is_empty() => {
            o.push_str("[project_groups]\n");
            for (k, v) in g {
                o.push_str(&format!("{} = {}\n", q(k), q(v)));
            }
        }
        _ => o.push_str("# [project_groups]\n# \"~/code/my-app\" = \"My App\"\n"),
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_layer() {
        let l = Layer::from_toml(
            r#"
listen = "0.0.0.0:19877"
upstream = "none"
asset_roots = ["~/Documents", "/tmp"]
kb_path = "~/Notes"
entities = true
bogus = 1
[project_groups]
"~/GitHub/app" = "App"
"#,
        )
        .unwrap();
        assert_eq!(l.listen.as_deref(), Some("0.0.0.0:19877"));
        assert_eq!(l.upstream.as_deref(), Some(""));
        assert_eq!(l.asset_roots.as_ref().unwrap().len(), 2);
        assert_eq!(l.entities, Some(true));
        assert_eq!(
            l.project_groups.unwrap(),
            vec![("~/GitHub/app".to_string(), "App".to_string())]
        );
        assert_eq!(l.unknown, vec!["bogus"]);
        assert!(Layer::from_toml("asset_roots = \"x\"").is_err());
    }

    #[test]
    fn kv_layer_is_the_old_format() {
        let l = Layer::from_kv(
            "# Written by Baton.app\nlisten=127.0.0.1:19880\nupstream=http://127.0.0.1:19877\nrelay=\n\
             asset_roots=~/a, /tmp\nproject_groups.-Users-x-proj=Proj\nentities=yes\n",
        );
        assert_eq!(l.listen.as_deref(), Some("127.0.0.1:19880"));
        assert_eq!(l.upstream.as_deref(), Some("http://127.0.0.1:19877"));
        assert_eq!(l.relay, None);
        assert_eq!(l.asset_roots.unwrap(), vec!["~/a", "/tmp"]);
        assert_eq!(l.entities, Some(true));
        assert_eq!(l.project_groups.unwrap()[0].1, "Proj");
        // daemon.conf lines are not valid TOML, so from_file falls back.
        assert!(Layer::from_toml("listen=127.0.0.1:19880").is_err());
    }

    #[test]
    fn precedence_and_defaults() {
        let mut base = Layer::from_toml("listen = \"127.0.0.1:1\"\nrelay = \"https://r\"").unwrap();
        base.overlay(Layer::from_kv("listen=127.0.0.1:2"));
        base.overlay(Layer::from_flags(&[
            "serve".into(),
            "--listen".into(),
            "127.0.0.1:3".into(),
        ]));
        let c = Config::from_layer(base, BTreeMap::new(), vec![], vec![]);
        assert_eq!(c.listen, "127.0.0.1:3");
        assert_eq!(c.relay.as_deref(), Some("https://r"));
        assert_eq!(c.upstream, None);
        assert!(!c.entities);
        let d = Config::from_layer(Layer::default(), BTreeMap::new(), vec![], vec![]);
        assert_eq!(d.listen, format!("127.0.0.1:{DEFAULT_PORT}"));
        assert_eq!(d.port(), DEFAULT_PORT);
        assert!(!d.listens_publicly());
    }

    #[test]
    fn render_round_trips() {
        let l = Layer {
            listen: Some("0.0.0.0:19877".into()),
            upstream: Some("http://127.0.0.1:19878".into()),
            asset_roots: Some(vec!["~/Documents".into(), "/tmp".into()]),
            kb_path: Some("~/Documents/Notes".into()),
            project_groups: Some(vec![("~/code/a b".into(), "A \"B\"".into())]),
            entities: Some(true),
            ..Layer::default()
        };
        let back = Layer::from_toml(&render(&l)).unwrap();
        assert_eq!(back, l);
        // An empty layer renders to a file of comments that parses empty.
        assert_eq!(
            Layer::from_toml(&render(&Layer::default())).unwrap(),
            Layer::default()
        );
    }
}
