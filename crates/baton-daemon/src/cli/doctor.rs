//! `baton doctor` — what's installed, how it's configured, whether the
//! daemon answers, Tailscale state (read-only), permission hints, the tmux
//! socket and recent structured errors. `--bundle` writes the same report
//! plus redacted log tails to a `.tar.gz` for bug reports: no tokens, no
//! transcript text, paths home-relative, query values hashed.

use super::pairing::Tailscale;
use super::{find_tool, flag_value, has_flag, http_get, run, tail_lines, which};
use crate::config::{home, home_relative, mac_app_conf, Config, Source};
use crate::state::{load_token, TokenSource};
use regex::Regex;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub const DOCTOR_USAGE: &str = "usage: baton doctor [--config FILE] [--bundle [FILE.tar.gz]]";

struct Report {
    text: String,
    problems: usize,
}

impl Report {
    fn section(&mut self, t: &str) {
        let _ = writeln!(self.text, "\n{t}");
    }
    fn line(&mut self, k: &str, v: impl AsRef<str>) {
        let _ = writeln!(self.text, "  {k:<14} {}", v.as_ref());
    }
    fn bad(&mut self, k: &str, v: impl AsRef<str>) {
        self.problems += 1;
        let _ = writeln!(self.text, "! {k:<14} {}", v.as_ref());
    }
    fn hint(&mut self, v: impl AsRef<str>) {
        let _ = writeln!(self.text, "  {:<14} → {}", "", v.as_ref());
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

fn version_of(tool: &Path, args: &[&str]) -> String {
    let o = run(&tool.to_string_lossy(), args, Duration::from_secs(5));
    let v = first_line(if o.stdout.trim().is_empty() {
        &o.stderr
    } else {
        &o.stdout
    });
    if v.is_empty() {
        "?".into()
    } else {
        v
    }
}

/// Where structured logs may be: `$BATON_LOG_DIR`, the LaunchAgents' dir
/// (`~/Library/Logs/Baton`), the default (`~/Library/Logs`).
fn log_dirs() -> Vec<PathBuf> {
    let h = home();
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("BATON_LOG_DIR").filter(|d| !d.is_empty()) {
        v.push(crate::config::expand(&d.to_string_lossy()));
    }
    for d in [h.join("Library/Logs/Baton"), h.join("Library/Logs")] {
        if !v.contains(&d) {
            v.push(d);
        }
    }
    v.into_iter().filter(|d| d.is_dir()).collect()
}

fn stderr_logs() -> Vec<PathBuf> {
    let h = home();
    [
        h.join("Library/Logs/Baton/daemon.err.log"),
        h.join("Library/Logs/baton-daemon.err.log"),
        PathBuf::from("/tmp/baton-daemon.err.log"),
    ]
    .into_iter()
    .filter(|p| p.is_file())
    .collect()
}

fn build(cfg: &Config) -> Report {
    let mut r = Report {
        text: String::new(),
        problems: 0,
    };
    let h = home();
    let _ = writeln!(
        r.text,
        "Baton doctor — {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z")
    );

    // ── versions ───────────────────────────────────────────────────────
    r.section("Versions");
    r.line(
        "baton",
        format!(
            "{} ({})",
            env!("CARGO_PKG_VERSION"),
            std::env::current_exe()
                .map(|p| home_relative(&p))
                .unwrap_or_default()
        ),
    );
    let sw = run(
        "/usr/bin/sw_vers",
        &["-productVersion"],
        Duration::from_secs(3),
    );
    r.line(
        "macOS",
        format!("{} ({})", first_line(&sw.stdout), std::env::consts::ARCH),
    );
    let mut tmux_path = None;
    for (name, args, required) in [
        ("tmux", &[][..], true),
        ("claude", &["--version"][..], false),
        ("codex", &["--version"][..], false),
    ] {
        match which(name) {
            // tmux itself is never run here; its version comes from the daemon.
            Some(p) if name == "tmux" => {
                r.line(name, home_relative(&p));
                tmux_path = Some(p);
                let _ = args;
            }
            Some(p) => r.line(
                name,
                format!("{} ({})", version_of(&p, args), home_relative(&p)),
            ),
            None if required => {
                r.bad(name, "not on PATH");
                r.hint("brew install tmux");
            }
            None => r.line(name, "not on PATH (optional)"),
        }
    }
    let ts_cli = find_tool("tailscale", &super::pairing::TAILSCALE_CANDIDATES);
    match &ts_cli {
        Some(p) => r.line(
            "tailscale",
            format!("{} ({})", version_of(p, &["version"]), p.display()),
        ),
        None => r.line("tailscale", "not installed (optional: remote access)"),
    }

    // ── config ─────────────────────────────────────────────────────────
    r.section("Config");
    if cfg.files.is_empty() {
        r.line("files", "none (defaults) — run `baton setup`");
    } else {
        r.line(
            "files",
            cfg.files
                .iter()
                .map(|p| home_relative(p))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    let src = |k: &str| {
        cfg.sources
            .get(k)
            .cloned()
            .unwrap_or(Source::Default)
            .to_string()
    };
    let show = |r: &mut Report, k: &str, v: String| r.line(k, format!("{v}   [{}]", src(k)));
    show(&mut r, "listen", cfg.listen.clone());
    show(
        &mut r,
        "upstream",
        cfg.upstream
            .clone()
            .unwrap_or("none (unimplemented endpoints → 501)".into()),
    );
    show(
        &mut r,
        "relay",
        cfg.relay.clone().unwrap_or("none (no push)".into()),
    );
    show(&mut r, "claude_dir", home_relative(&cfg.claude_dir));
    show(&mut r, "codex_dir", home_relative(&cfg.codex_dir));
    show(
        &mut r,
        "asset_roots",
        match &cfg.asset_roots {
            Some(v) => v.join(", "),
            None => "default (~ minus hidden dirs and ~/Library; ~/.baton/pastes, ~/.claude/plans)"
                .into(),
        },
    );
    show(
        &mut r,
        "kb_path",
        cfg.kb_path
            .as_deref()
            .map(home_relative)
            .unwrap_or("off".into()),
    );
    show(
        &mut r,
        "entities",
        if cfg.entities { "on" } else { "off" }.into(),
    );
    show(
        &mut r,
        "project_groups",
        if cfg.project_groups.is_empty() {
            "none".into()
        } else {
            cfg.project_groups
                .iter()
                .map(|(k, v)| format!("{k} → {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    for w in &cfg.warnings {
        r.bad("warning", w);
    }
    let (token, tsrc) = load_token(&h);
    match &tsrc {
        TokenSource::Env => r.line("token", "set ($BATON_BEARER)"),
        TokenSource::File(p) | TokenSource::Legacy(p) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0);
            let legacy = matches!(tsrc, TokenSource::Legacy(_));
            r.line(
                "token",
                format!(
                    "set ({}, mode {mode:o}{})",
                    home_relative(p),
                    if legacy { ", Python server's file" } else { "" }
                ),
            );
            if mode & 0o077 != 0 {
                r.bad("token", "readable by other users");
                r.hint(format!("chmod 600 {}", home_relative(p)));
            }
        }
        TokenSource::None => {
            r.bad("token", "none — the API is open to anyone who can reach it");
            r.hint("baton setup (generates one)");
        }
    }

    // ── daemon ─────────────────────────────────────────────────────────
    r.section("Daemon");
    let base = cfg.local_base();
    let mut daemon_tmux = None;
    match http_get(&base, "/api/health", Duration::from_secs(3)) {
        Ok((200, body)) => {
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            match v["daemon"].as_str() {
                Some(ver) => r.line(
                    "health",
                    format!(
                        "{base} → ok · baton {ver} · api {} · pid {} · started {}",
                        v["api_version"],
                        v["pid"],
                        v["started"].as_str().unwrap_or("?")
                    ),
                ),
                None => {
                    r.bad(
                        "health",
                        format!("{base} answers, but it is not the baton daemon (Python server?)"),
                    );
                }
            }
            daemon_tmux = v["tmux"].as_str().map(String::from);
        }
        Ok((code, _)) => r.bad("health", format!("{base}/api/health → HTTP {code}")),
        Err(e) => {
            r.bad("health", format!("{base} not answering ({e})"));
            r.hint("start it: `baton serve`, or re-run `baton setup` to install the LaunchAgent");
        }
    }
    if let Some(up) = &cfg.upstream {
        match http_get(up, "/api/health", Duration::from_secs(3)) {
            Ok((200, _)) => r.line("upstream", format!("{up} → ok")),
            Ok((c, _)) => r.bad("upstream", format!("{up} → HTTP {c}")),
            Err(e) => r.bad("upstream", format!("{up} not answering ({e})")),
        }
    }
    let plist = super::setup::plist_path(&h);
    if mac_app_conf().exists() {
        r.line(
            "service",
            format!("Baton.app ({})", home_relative(&mac_app_conf())),
        );
    } else if plist.exists() {
        r.line(
            "service",
            format!(
                "LaunchAgent {} ({})",
                super::setup::LABEL,
                home_relative(&plist)
            ),
        );
    } else {
        r.line("service", "no LaunchAgent (run `baton setup`)");
    }
    if let Some(t) = daemon_tmux.filter(|t| !t.is_empty()) {
        r.line("tmux", t);
    }

    // ── tailscale (read-only) ──────────────────────────────────────────
    r.section("Tailscale");
    if ts_cli.is_some() {
        let ts = Tailscale::load();
        match (&ts.error, ts.backend_state.as_deref()) {
            (Some(e), _) => r.bad("status", first_line(e)),
            (None, st) => r.line(
                "status",
                format!(
                    "{} · {} · {}",
                    st.unwrap_or("?"),
                    ts.dns_name.as_deref().unwrap_or("no MagicDNS name"),
                    ts.ipv4.as_deref().unwrap_or("no IPv4")
                ),
            ),
        }
        if ts.running() {
            if ts.serves_port(cfg.port()) {
                r.line(
                    "serve",
                    format!(
                        "https://{} → :{} ✓",
                        ts.dns_name.as_deref().unwrap_or("?"),
                        cfg.port()
                    ),
                );
            } else {
                r.line(
                    "serve",
                    if ts.serve_targets.is_empty() {
                        "not serving anything".to_string()
                    } else {
                        format!(
                            "serving {} (not :{})",
                            ts.serve_targets.join(", "),
                            cfg.port()
                        )
                    },
                );
                r.hint(format!(
                    "tailscale serve --bg --https=443 http://127.0.0.1:{}",
                    cfg.port()
                ));
            }
            if ts.cert_domains.is_empty() {
                r.hint("HTTPS certificates are off for this tailnet (admin console → DNS → HTTPS)");
            }
        }
    } else {
        r.line("status", "not installed");
    }

    // ── permissions ────────────────────────────────────────────────────
    r.section("Permissions");
    let check_dir = |r: &mut Report, k: &str, p: &Path, required: bool| match std::fs::read_dir(p) {
        Ok(it) => r.line(
            k,
            format!("{} readable ({} entries)", home_relative(p), it.count()),
        ),
        Err(e)
            if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(1) =>
        {
            r.bad(k, format!("{} not readable ({e})", home_relative(p)));
            r.hint("macOS privacy: allow Baton / your terminal under System Settings → Privacy & Security → Files and Folders (or Full Disk Access)");
        }
        Err(_) if !required => r.line(k, format!("{} absent", home_relative(p))),
        Err(e) => r.bad(k, format!("{} ({e})", home_relative(p))),
    };
    check_dir(&mut r, "claude", &cfg.claude_dir.join("projects"), true);
    check_dir(&mut r, "codex", &cfg.codex_dir.join("sessions"), false);
    if let Some(kb) = &cfg.kb_path {
        check_dir(&mut r, "kb_path", kb, true);
    }
    r.line(
        "automation",
        "opening sessions in Terminal/iTerm uses AppleScript: approve the Automation prompt on first use",
    );

    // ── tmux socket (never runs tmux) ──────────────────────────────────
    r.section("tmux");
    let uid = unsafe { libc::getuid() };
    let tdir = std::env::var_os("TMUX_TMPDIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let sock = tdir.join(format!("tmux-{uid}/default"));
    {
        use std::os::unix::fs::FileTypeExt;
        match std::fs::metadata(&sock) {
            Ok(m) if m.file_type().is_socket() => r.line(
                "socket",
                format!("{} (server socket present)", sock.display()),
            ),
            Ok(_) => r.bad(
                "socket",
                format!("{} exists but is not a socket", sock.display()),
            ),
            Err(_) => r.line(
                "socket",
                format!("{} absent (no tmux server running yet)", sock.display()),
            ),
        }
    }
    if tmux_path.is_none() {
        r.hint("sessions are driven through tmux; install it first");
    }

    // ── recent errors ──────────────────────────────────────────────────
    r.section("Recent errors");
    let mut errors: Vec<(String, String)> = Vec::new();
    for d in log_dirs() {
        for line in tail_lines(&d.join(crate::logs::ERRORS), 200) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                let ts = v["ts"].as_str().unwrap_or("?").to_string();
                let msg = format!(
                    "{} {} {}: {}",
                    ts,
                    v["source"].as_str().unwrap_or("?"),
                    v["type"].as_str().unwrap_or("?"),
                    crate::pyjson::head(&redact_text(v["msg"].as_str().unwrap_or(""), &token), 140)
                );
                errors.push((ts, msg));
            }
        }
    }
    errors.sort();
    if errors.is_empty() {
        r.line("errors", "none logged");
    } else {
        for (_, m) in errors.iter().rev().take(8).rev() {
            let _ = writeln!(r.text, "  {m}");
        }
    }
    let _ = writeln!(
        r.text,
        "\n{}",
        if r.problems == 0 {
            "No problems found.".to_string()
        } else {
            format!("{} problem(s) — see the lines marked `!`.", r.problems)
        }
    );
    r.text = redact_text(&r.text, &token);
    r
}

/// `baton doctor`.
pub fn main(args: &[String]) -> i32 {
    if has_flag(args, "--help") || has_flag(args, "-h") {
        println!("{DOCTOR_USAGE}");
        return 0;
    }
    let cfg = Config::load(args);
    let r = build(&cfg);
    print!("{}", r.text);
    if has_flag(args, "--bundle") {
        let out = flag_value(args, "--bundle")
            .map(|p| crate::config::expand(&p))
            .unwrap_or_else(|| {
                PathBuf::from(format!(
                    "baton-doctor-{}.tar.gz",
                    chrono::Local::now().format("%Y%m%d-%H%M%S")
                ))
            });
        match bundle(&cfg, &r.text, &out) {
            Ok(n) => println!(
                "\nBundle: {} ({n} files, redacted — check before sharing)",
                out.display()
            ),
            Err(e) => {
                eprintln!("baton doctor: bundle: {e}");
                return 1;
            }
        }
    }
    0
}

// ── bundle + redaction ─────────────────────────────────────────────────

fn bundle(cfg: &Config, report: &str, out: &Path) -> Result<usize, String> {
    let (token, _) = load_token(&home());
    let dir = std::env::temp_dir().join(format!("baton-doctor-{}", std::process::id()));
    let root = dir.join("baton-doctor");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(root.join("logs")).map_err(|e| e.to_string())?;
    let mut n = 0;
    let mut put = |rel: &str, text: &str| -> Result<(), String> {
        std::fs::write(root.join(rel), text).map_err(|e| format!("{rel}: {e}"))?;
        n += 1;
        Ok(())
    };
    put("doctor.txt", report)?;
    put("config.toml", &redact_text(&cfg.to_toml(), &token))?;
    for (i, d) in log_dirs().iter().enumerate() {
        for (file, keep) in [
            (crate::logs::ERRORS, 300),
            (crate::logs::REQUESTS, 300),
            (crate::logs::EVENTS, 300),
            (crate::logs::CLIENT, 100),
        ] {
            let lines = tail_lines(&d.join(file), keep);
            if lines.is_empty() {
                continue;
            }
            let red: String = lines
                .iter()
                .map(|l| redact_json_line(l, &token) + "\n")
                .collect();
            put(&format!("logs/{i}-{file}"), &red)?;
        }
    }
    for (i, p) in stderr_logs().iter().enumerate() {
        let red: String = tail_lines(p, 300)
            .iter()
            .map(|l| redact_text(l, &token) + "\n")
            .collect();
        put(&format!("logs/stderr-{i}.log"), &red)?;
    }
    put(
        "README.txt",
        "baton doctor bundle. Redacted: bearer/device tokens, message and transcript text\n\
         (replaced by lengths), query-string values (hashed), home directory (shown as ~).\n",
    )?;
    let out_abs = if out.is_absolute() {
        out.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(out)
    };
    let tar = run(
        "/usr/bin/tar",
        &[
            "-czf",
            &out_abs.to_string_lossy(),
            "-C",
            &dir.to_string_lossy(),
            "baton-doctor",
        ],
        Duration::from_secs(60),
    );
    let _ = std::fs::remove_dir_all(&dir);
    if !tar.ok {
        return Err(format!("tar: {}", tar.stderr.trim()));
    }
    Ok(n)
}

/// Keys whose values are credentials.
const SECRET_KEYS: [&str; 9] = [
    "token",
    "device_token",
    "authorization",
    "bearer",
    "key",
    "secret",
    "password",
    "cookie",
    "push_key",
];
/// Keys whose values may carry transcript or user text.
const CONTENT_KEYS: [&str; 16] = [
    "text",
    "content",
    "message",
    "prompt",
    "body",
    "preview",
    "title",
    "first_user_message",
    "last_message",
    "summary",
    "input",
    "output",
    "lines",
    "screen",
    "data",
    "keys",
];

fn redact_value(k: Option<&str>, v: &mut Value, token: &str) {
    let lk = k.map(str::to_ascii_lowercase);
    let lk = lk.as_deref();
    if lk.is_some_and(|k| SECRET_KEYS.contains(&k) || k.ends_with("_token")) {
        *v = Value::String("<redacted>".into());
        return;
    }
    if lk.is_some_and(|k| CONTENT_KEYS.contains(&k)) {
        *v = match v {
            Value::String(s) => Value::String(format!("<{} chars>", s.chars().count())),
            Value::Null => Value::Null,
            _ => Value::String("<redacted>".into()),
        };
        return;
    }
    match v {
        Value::String(s) => {
            let mut t = redact_text(s, token);
            if matches!(lk, Some("path" | "url" | "uri" | "referer")) {
                t = hash_query(&t);
            }
            *s = t;
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact_value(None, x, token)),
        Value::Object(m) => {
            for (kk, x) in m.iter_mut() {
                redact_value(Some(kk), x, token);
            }
        }
        _ => {}
    }
}

fn redact_json_line(line: &str, token: &str) -> String {
    match serde_json::from_str::<Value>(line) {
        Ok(mut v) => {
            redact_value(None, &mut v, token);
            v.to_string()
        }
        Err(_) => redact_text(line, token),
    }
}

/// `?a=1&b=x` → `?a=h:…&b=h:…` (8 hex of SHA-256; equal values stay equal).
fn hash_query(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let Some((path, q)) = s.split_once('?') else {
        return s.to_string();
    };
    let q = q
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if !v.is_empty() => {
                let d = Sha256::digest(v.as_bytes());
                format!("{k}=h:{:02x}{:02x}{:02x}{:02x}", d[0], d[1], d[2], d[3])
            }
            _ => kv.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{path}?{q}")
}

/// Plain-text redaction: the token, `Bearer …`, `"token":"…"`, home → `~`.
pub fn redact_text(s: &str, token: &str) -> String {
    static RE: OnceLock<[Regex; 2]> = OnceLock::new();
    let [bearer, kv] = RE.get_or_init(|| {
        [
            Regex::new(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{6,}").expect("regex"),
            Regex::new(r#"(?i)("(?:[a-z_]*token|authorization|secret|password)"\s*:\s*)"[^"]*""#)
                .expect("regex"),
        ]
    });
    let mut t = s.to_string();
    if token.len() >= 6 {
        t = t.replace(token, "<token>");
    }
    t = bearer.replace_all(&t, "${1}<redacted>").into_owned();
    t = kv.replace_all(&t, "${1}\"<redacted>\"").into_owned();
    let h = home();
    let hs = h.to_string_lossy();
    if hs.len() > 1 {
        t = t.replace(hs.as_ref(), "~");
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_are_redacted() {
        let line = r#"{"ts":"t","path":"/api/asset?path=/x/y.md&n=2","token":"abc","body":{"text":"secret words"},"msg":"Bearer abcdefgh1234 failed","device_token":"d"}"#;
        let out = redact_json_line(line, "zzzzzzzz");
        assert!(!out.contains("secret words") && !out.contains("abcdefgh1234"));
        assert!(!out.contains("\"abc\"") && !out.contains("y.md"));
        assert!(out.contains("path=h:") && out.contains("n=h:"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn text_is_redacted() {
        let t = redact_text(
            "tok=SuperSecretToken1 Authorization: Bearer SuperSecretToken1 {\"token\":\"x\"}",
            "SuperSecretToken1",
        );
        assert!(!t.contains("SuperSecretToken1"), "{t}");
        assert!(t.contains("\"token\":\"<redacted>\""));
    }
}
