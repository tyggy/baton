//! `baton setup` — interactive, idempotent first-run: config, token,
//! LaunchAgent, tool checks, pairing QR. `baton pair` — the QR alone.
//!
//! Safe to re-run: existing values are kept, files are only rewritten when
//! their content changes, an existing token is never replaced (unless
//! `--new-token`).

use super::migrate;
use super::pairing::{self, Tailscale};
use super::{account_home, ask, confirm, flag_value, has_flag, run, which, write_if_changed};
use crate::config::{
    self, config_path, home, home_relative, mac_app_conf, token_path, Config, Layer, DEFAULT_PORT,
};
use crate::state::{load_token, TokenSource};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LABEL: &str = "com.baton.daemon";

pub const SETUP_USAGE: &str = "usage: baton setup [--yes] [--dry-run] [--skip-launchctl] [--no-launchd] [--no-qr]
                   [--listen ADDR] [--upstream URL|none] [--relay URL] [--kb-path DIR]
                   [--claude-dir DIR] [--codex-dir DIR] [--bin PATH] [--migrate|--no-migrate] [--new-token]

  --yes             accept the defaults, no questions (also used when stdin isn't a terminal)
  --dry-run         show what would be written and run; change nothing
  --skip-launchctl  write the LaunchAgent plist but don't call launchctl (prints the commands)
  --no-launchd      don't install the LaunchAgent (e.g. you run `baton serve` yourself)
  --migrate         import the Python server's settings even when a config already exists";

struct Opts {
    yes: bool,
    dry_run: bool,
    skip_launchctl: bool,
    no_launchd: bool,
    no_qr: bool,
    new_token: bool,
    migrate: Option<bool>,
    bin: Option<PathBuf>,
    overrides: Layer,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, String> {
        const VALUED: [&str; 8] = [
            "--listen",
            "--upstream",
            "--relay",
            "--kb-path",
            "--claude-dir",
            "--codex-dir",
            "--bin",
            "--config",
        ];
        const BARE: [&str; 11] = [
            "--yes",
            "-y",
            "--dry-run",
            "--skip-launchctl",
            "--no-launchd",
            "--no-qr",
            "--new-token",
            "--migrate",
            "--no-migrate",
            "--help",
            "-h",
        ];
        let mut i = 2;
        while i < args.len() {
            let a = args[i].as_str();
            if VALUED.contains(&a) {
                if flag_value(args, a).is_none() {
                    return Err(format!("{a} needs a value"));
                }
                i += 2;
            } else if BARE.contains(&a) {
                i += 1;
            } else {
                return Err(format!("unknown argument {a}"));
            }
        }
        let mut overrides = Layer::from_flags(args);
        overrides.relay = flag_value(args, "--relay");
        Ok(Opts {
            yes: has_flag(args, "--yes") || has_flag(args, "-y"),
            dry_run: has_flag(args, "--dry-run"),
            skip_launchctl: has_flag(args, "--skip-launchctl"),
            no_launchd: has_flag(args, "--no-launchd"),
            no_qr: has_flag(args, "--no-qr"),
            new_token: has_flag(args, "--new-token"),
            migrate: if has_flag(args, "--migrate") {
                Some(true)
            } else if has_flag(args, "--no-migrate") {
                Some(false)
            } else {
                None
            },
            bin: flag_value(args, "--bin").map(|p| config::expand(&p)),
            overrides,
        })
    }
}

fn step(n: usize, title: &str) {
    println!("\n[{n}/5] {title}");
}

fn ok(msg: &str) {
    println!("  ✓ {msg}");
}

fn warn(msg: &str) {
    println!("  ! {msg}");
}

fn info(msg: &str) {
    println!("    {msg}");
}

/// `baton setup`.
pub fn main(args: &[String]) -> i32 {
    if has_flag(args, "--help") || has_flag(args, "-h") {
        println!("{SETUP_USAGE}");
        return 0;
    }
    let o = match Opts::parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("baton setup: {e}\n{SETUP_USAGE}");
            return 2;
        }
    };
    let interactive = !o.yes && super::stdin_is_tty();
    let home = home();
    let cfg_path = config_path();
    println!(
        "Baton setup (baton {}){}",
        env!("CARGO_PKG_VERSION"),
        if o.dry_run {
            " — dry run, nothing is changed"
        } else {
            ""
        }
    );

    // ── what's already here ────────────────────────────────────────────
    let existing = match Layer::from_file(&cfg_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("baton setup: {e}\nFix or move the file, then run setup again.");
            return 1;
        }
    };
    let legacy = migrate::detect(&home);
    let mac_app = mac_app_conf().exists();
    info(&format!(
        "config    {} ({})",
        home_relative(&cfg_path),
        if existing.is_some() { "exists" } else { "new" }
    ));
    if let Some(l) = &legacy {
        match (&l.plist, l.port) {
            (Some(_), port) => info(&format!(
                "legacy    Python server LaunchAgent {} (port {})",
                migrate::LEGACY_LABEL,
                port.map(|p| p.to_string()).unwrap_or("?".into())
            )),
            (None, _) => info("legacy    ~/.claude-sessions/config (bearer token)"),
        }
    }
    if mac_app {
        info(&format!(
            "mac app   Baton.app hosts the daemon ({})",
            home_relative(&mac_app_conf())
        ));
    }

    // ── 1. config ──────────────────────────────────────────────────────
    step(1, "Config");
    let migrate_now = legacy.as_ref().is_some_and(|l| l.plist.is_some())
        && o.migrate.unwrap_or(existing.is_none());
    let mut layer = existing.clone().unwrap_or_default();
    if migrate_now {
        let (ml, notes) = migrate::layer(legacy.as_ref().expect("checked"));
        fill_missing(&mut layer, ml);
        ok("imported the Python server's settings (docs/migrating.md)");
        for n in notes {
            info(&n);
        }
    }
    if existing.is_none() && layer.listen.is_none() {
        let d = format!("0.0.0.0:{DEFAULT_PORT}");
        let listen = ask(
            "  Listen address (0.0.0.0 = reachable from your phone over LAN/Tailscale)",
            &d,
            interactive,
        );
        layer.listen = Some(listen);
    }
    if existing.is_none() && layer.relay.is_none() && interactive {
        let r = ask(
            "  Push relay URL for notifications (blank = none)",
            "",
            true,
        );
        if !r.is_empty() {
            layer.relay = Some(r);
        }
    }
    overlay_set(&mut layer, o.overrides.clone());
    if let Some(l) = &layer.listen {
        if l.parse::<std::net::SocketAddr>().is_err() {
            eprintln!("baton setup: listen `{l}` is not host:port");
            return 2;
        }
    }
    let text = config::render(&layer);
    let config_changed = existing.as_ref() != Some(&layer);
    if !config_changed {
        ok(&format!("{} unchanged", home_relative(&cfg_path)));
    } else if o.dry_run {
        ok(&format!("would write {}:", home_relative(&cfg_path)));
        for l in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            info(l);
        }
    } else {
        if existing.is_some() {
            let bak = cfg_path.with_extension("toml.bak");
            let _ = std::fs::copy(&cfg_path, &bak);
            info(&format!("previous file saved as {}", home_relative(&bak)));
        }
        match write_if_changed(&cfg_path, &text, Some(0o644)) {
            Ok(_) => ok(&format!("wrote {}", home_relative(&cfg_path))),
            Err(e) => {
                eprintln!("baton setup: {}: {e}", cfg_path.display());
                return 1;
            }
        }
    }
    let effective = Config::from_layer(
        layer.clone(),
        Default::default(),
        vec![cfg_path.clone()],
        vec![],
    );
    info(&format!(
        "listen {} · upstream {} · relay {}",
        effective.listen,
        effective.upstream.as_deref().unwrap_or("none"),
        effective.relay.as_deref().unwrap_or("none")
    ));

    // ── 2. token ───────────────────────────────────────────────────────
    step(2, "Bearer token");
    let (mut token, source) = load_token(&home);
    let tpath = token_path();
    match (&source, o.new_token) {
        (TokenSource::File(p), false) => {
            let _ = (!o.dry_run).then(|| super::set_mode(p, 0o600));
            ok(&format!("existing token in {} kept", home_relative(p)));
        }
        (TokenSource::Legacy(p), false) => ok(&format!(
            "existing token in {} kept (paired phones keep working)",
            home_relative(p)
        )),
        (TokenSource::Env, false) if tpath.exists() => {
            ok("token from $BATON_BEARER (overrides the token file)")
        }
        _ => {
            if o.dry_run {
                ok(&format!("would generate {} (0600)", home_relative(&tpath)));
                token = "<new token>".into();
            } else {
                let t = match new_token() {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("baton setup: token: {e}");
                        return 1;
                    }
                };
                if let Err(e) = write_if_changed(&tpath, &format!("{t}\n"), Some(0o600)) {
                    eprintln!("baton setup: {}: {e}", tpath.display());
                    return 1;
                }
                ok(&format!("generated {} (0600)", home_relative(&tpath)));
                if source == TokenSource::Env {
                    info("note: $BATON_BEARER is set in this shell and overrides the file");
                }
                token = t; // what the LaunchAgent's daemon will use
            }
        }
    }

    // ── 3. LaunchAgent ─────────────────────────────────────────────────
    step(3, "Login service");
    if mac_app {
        ok("Baton.app manages the daemon — skipped (restart it from the app to apply the config)");
    } else if o.no_launchd {
        ok("skipped (--no-launchd); run `baton serve` yourself");
    } else if !confirm(
        "  Start the daemon at login (LaunchAgent)?",
        true,
        interactive,
    ) {
        ok("skipped");
    } else {
        launch_agent(&o, &home, config_changed);
    }

    // ── 4. tools ───────────────────────────────────────────────────────
    step(4, "Tools on PATH");
    for (name, need, hint) in [
        ("tmux", true, "brew install tmux"),
        (
            "claude",
            false,
            "https://docs.anthropic.com/en/docs/claude-code",
        ),
        ("codex", false, "npm install -g @openai/codex"),
    ] {
        match which(name) {
            Some(p) => ok(&format!("{name:<7} {}", home_relative(&p))),
            None if need => warn(&format!("{name:<7} not found — required ({hint})")),
            None => warn(&format!("{name:<7} not found — optional ({hint})")),
        }
    }

    // ── 5. pairing ─────────────────────────────────────────────────────
    step(5, "Pair your iPhone");
    let ts = Tailscale::load();
    pair_output(&effective, &token, &ts, !o.no_qr, !o.dry_run && !mac_app);
    next_steps(&effective, &ts);
    0
}

/// Values from `from` for keys `into` doesn't set.
fn fill_missing(into: &mut Layer, from: Layer) {
    let mut merged = from;
    overlay_set(&mut merged, into.clone());
    merged.unknown = std::mem::take(&mut into.unknown);
    *into = merged;
}

/// `over`'s set keys on top of `base`.
fn overlay_set(base: &mut Layer, over: Layer) {
    macro_rules! take {
        ($($f:ident),*) => {$( if over.$f.is_some() { base.$f = over.$f; } )*};
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

/// 32 random bytes, URL-safe base64 (43 chars).
pub fn new_token() -> std::io::Result<String> {
    use base64::Engine;
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

pub fn plist_path(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent: `<bin> serve`, which reads `config.toml` itself.
pub fn plist_text(bin: &Path, home: &Path, path_env: &str) -> String {
    let logs = home.join("Library/Logs/Baton");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `baton setup`. Settings live in ~/.config/baton/config.toml. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{bin}</string>
		<string>serve</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>{path}</string>
		<key>BATON_LOG_DIR</key>
		<string>{logs}</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>5</integer>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>{out}</string>
	<key>StandardErrorPath</key>
	<string>{err}</string>
</dict>
</plist>
"#,
        bin = xml(&bin.to_string_lossy()),
        path = xml(path_env),
        logs = xml(&logs.to_string_lossy()),
        out = xml(&logs.join("daemon.out.log").to_string_lossy()),
        err = xml(&logs.join("daemon.err.log").to_string_lossy()),
    )
}

/// PATH for the agent: where tmux/claude/codex were found, then the usual dirs.
fn agent_path_env() -> String {
    let mut dirs: Vec<String> = Vec::new();
    for t in ["tmux", "claude", "codex"] {
        if let Some(d) = which(t).and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    for d in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        if !dirs.iter().any(|x| x == d) {
            dirs.push(d.into());
        }
    }
    dirs.join(":")
}

fn launch_agent(o: &Opts, home: &Path, config_changed: bool) {
    let bin = o
        .bin
        .clone()
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("baton"));
    let plist = plist_path(home);
    let text = plist_text(&bin, home, &agent_path_env());
    let unchanged = std::fs::read_to_string(&plist).is_ok_and(|t| t == text);
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let target = format!("{domain}/{LABEL}");
    let mut cmds: Vec<Vec<String>> = Vec::new();
    if unchanged {
        ok(&format!("{} unchanged", home_relative(&plist)));
        if config_changed {
            cmds.push(vec!["kickstart".into(), "-k".into(), target.clone()]);
        }
    } else {
        if o.dry_run {
            ok(&format!(
                "would write {} (runs {} serve)",
                home_relative(&plist),
                bin.display()
            ));
        } else {
            let _ = std::fs::create_dir_all(home.join("Library/Logs/Baton"));
            match write_if_changed(&plist, &text, Some(0o644)) {
                Ok(_) => ok(&format!(
                    "wrote {} (runs {} serve)",
                    home_relative(&plist),
                    bin.display()
                )),
                Err(e) => {
                    warn(&format!("{}: {e}", plist.display()));
                    return;
                }
            }
        }
        cmds.push(vec!["bootout".into(), target.clone()]);
        cmds.push(vec![
            "bootstrap".into(),
            domain.clone(),
            plist.to_string_lossy().into_owned(),
        ]);
    }
    if cmds.is_empty() {
        return;
    }
    // Never load an agent for a sandboxed HOME into the real launchd.
    let sandboxed = account_home().is_some_and(|h| h != home);
    let launchctl = std::env::var("BATON_LAUNCHCTL").unwrap_or_else(|_| "/bin/launchctl".into());
    let print_only = o.dry_run || o.skip_launchctl || sandboxed;
    if print_only {
        let why = if o.dry_run {
            "dry run"
        } else if o.skip_launchctl {
            "--skip-launchctl"
        } else {
            "HOME is not this account's home"
        };
        info(&format!("launchctl not called ({why}); would run:"));
        for c in &cmds {
            info(&format!("  {launchctl} {}", c.join(" ")));
        }
        return;
    }
    for c in &cmds {
        let args: Vec<&str> = c.iter().map(String::as_str).collect();
        let out = run(&launchctl, &args, Duration::from_secs(15));
        // bootout of an agent that isn't loaded fails harmlessly.
        if !out.ok && c[0] != "bootout" {
            warn(&format!("launchctl {}: {}", c.join(" "), out.stderr.trim()));
            return;
        }
    }
    ok(&format!("loaded {LABEL}"));
}

/// The pairing QR + copyable code; optionally publishes `pairing.json`.
pub fn pair_output(cfg: &Config, token: &str, ts: &Tailscale, qr: bool, publish: bool) -> bool {
    if token.is_empty() {
        warn("no bearer token — run `baton setup` first");
        return false;
    }
    let lan = pairing::lan_ipv4();
    let Some(a) = pairing::addresses(ts, lan.as_deref(), cfg.port(), cfg.listens_publicly()) else {
        warn(&format!(
            "no address a phone can reach: the daemon listens on {} only and Tailscale isn't up.",
            cfg.listen
        ));
        info(&format!(
            "Set listen = \"0.0.0.0:{}\" in {} or start Tailscale, then run `baton pair`.",
            cfg.port(),
            home_relative(&config_path())
        ));
        return false;
    };
    let code = pairing::payload(&a, token);
    if qr {
        match pairing::qr_text(&code, super::stdout_is_tty()) {
            Some(q) => {
                info("Scan with the Baton app on your iPhone:");
                println!("{q}");
            }
            None => warn("could not render a QR code"),
        }
    }
    info("Or paste this pairing code into the app:");
    println!("{code}");
    info(&format!(
        "url {}{}{}",
        a.url,
        a.fallback
            .as_deref()
            .map(|f| format!(" · fallback {f}"))
            .unwrap_or_default(),
        a.lan
            .as_deref()
            .map(|l| format!(" · lan {l}"))
            .unwrap_or_default()
    ));
    if publish {
        let p = pairing::pairing_file();
        let remote = ts.serves_port(cfg.port());
        let body = pairing::published(&a, remote);
        // Unchanged apart from the timestamp → leave it.
        let strip = |s: &str| {
            s.split(",\"updated\"")
                .next()
                .unwrap_or_default()
                .to_string()
        };
        let same = std::fs::read_to_string(&p).is_ok_and(|t| strip(&t) == strip(&body));
        if !same {
            if let Err(e) = write_if_changed(&p, &body, Some(0o644)) {
                warn(&format!("{}: {e}", p.display()));
            }
        }
    }
    true
}

fn next_steps(cfg: &Config, ts: &Tailscale) {
    println!();
    if ts.cli.is_none() {
        info("Optional: install Tailscale to reach this Mac from anywhere over HTTPS,");
        info(&format!(
            "then run: tailscale serve --bg --https=443 http://127.0.0.1:{}",
            cfg.port()
        ));
    } else if !ts.serves_port(cfg.port()) {
        info("Optional remote access over HTTPS (valid cert on your *.ts.net name):");
        info(&format!(
            "  tailscale serve --bg --https=443 http://127.0.0.1:{}",
            cfg.port()
        ));
    }
    info("Check everything with `baton doctor`.");
}

/// `baton pair`.
pub fn pair(args: &[String]) -> i32 {
    let cfg = Config::load(args);
    let (token, _) = load_token(&home());
    let ts = Tailscale::load();
    let publish = !mac_app_conf().exists();
    if pair_output(&cfg, &token, &ts, !has_flag(args, "--no-qr"), publish) {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_well_formed() {
        let t = plist_text(
            Path::new("/opt/x & y/baton"),
            Path::new("/Users/me"),
            "/usr/bin",
        );
        assert!(t.contains("<string>/opt/x &amp; y/baton</string>"));
        assert!(
            t.contains("<string>serve</string>") && t.contains("<key>KeepAlive</key>\n\t<true/>")
        );
        assert!(!t.contains("19877"));
    }

    #[test]
    fn token_shape() {
        let t = new_token().unwrap();
        assert_eq!(t.len(), 43);
        assert_ne!(t, new_token().unwrap());
    }

    #[test]
    fn fill_missing_keeps_existing() {
        let mut have = Layer {
            listen: Some("0.0.0.0:1".into()),
            ..Layer::default()
        };
        fill_missing(
            &mut have,
            Layer {
                listen: Some("127.0.0.1:2".into()),
                kb_path: Some("~/kb".into()),
                ..Layer::default()
            },
        );
        assert_eq!(have.listen.as_deref(), Some("0.0.0.0:1"));
        assert_eq!(have.kb_path.as_deref(), Some("~/kb"));
    }
}
