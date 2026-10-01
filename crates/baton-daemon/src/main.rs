//! `baton` — the Baton daemon (`baton serve`) and its setup tools
//! (`setup`, `pair`, `doctor`; PLAN.md M3). Native endpoints live in `api/`;
//! anything they don't claim goes to the optional Python upstream, or gets a
//! 501 when none is configured.

mod api;
use pyjson::Py;
mod cli;
mod cmd;
mod config;
mod http;
mod logs;
mod proxy;
mod push;
mod pyjson;
mod state;
mod tmux_target;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

const USAGE: &str = "usage: baton <command>

  serve    run the daemon
           [--listen ADDR] [--upstream URL|none] [--relay URL] [--config FILE]
           [--claude-dir DIR] [--codex-dir DIR] [--kb-path DIR]
  setup    first-run setup: config, token, login service, pairing QR (--help)
  pair     print the pairing QR code and code again
  doctor   diagnose the install; --bundle writes a redacted bug-report archive
  config   print the effective config and where each value comes from
  sessions session list as JSON

Config: ~/.config/baton/config.toml (docs/config.md). Precedence: flags > env > file > defaults.";

/// `~/…` → `$HOME/…`. launchd doesn't expand `~`, and the Mac app's
/// LaunchAgent plist can't know the user's home at build time.
pub fn expand_home(p: &str) -> std::path::PathBuf {
    config::expand(p)
}

fn load_config(args: &[String]) -> config::Config {
    let cfg = config::Config::load(args);
    for w in &cfg.warnings {
        eprintln!("baton: {w}");
    }
    cfg.install();
    cfg
}

/// launchd starts agents without LANG/LC_*. tmux then treats its output as
/// ASCII and prints the tab in `-F '#{a}\t#{b}'` formats as `_`, so every
/// pane-list parse came back empty and all tmux sessions looked dormant.
/// Children (tmux, ps, claude, codex) inherit this.
fn ensure_utf8_locale() {
    let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| {
            let v = v.to_ascii_uppercase();
            v.contains("UTF-8") || v.contains("UTF8")
        });
    if !utf8 {
        // Single-threaded here: before the tokio runtime or any spawn.
        std::env::set_var("LANG", "en_US.UTF-8");
        std::env::remove_var("LC_ALL");
        std::env::remove_var("LC_CTYPE");
    }
}

/// launchd gives agents a soft limit of 256 open files. Each phone
/// connection, SSE stream, upstream socket and tmux child pipe uses one;
/// raise the soft limit to the hard limit (capped) at start.
fn raise_nofile_limit() {
    unsafe {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
            let want = rl.rlim_max.min(10_240);
            if rl.rlim_cur < want {
                rl.rlim_cur = want;
                libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
            }
        }
    }
}

fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0)
}

/// Stall detector. A task on the runtime bumps a heartbeat every second; a
/// plain thread checks it. If the runtime makes no progress for 15 s, save
/// a stack sample next to the logs (the evidence a hang otherwise takes
/// with it) and abort, so launchd restarts the daemon instead of it
/// answering nothing until someone notices.
fn spawn_stall_watchdog(beat: Arc<std::sync::atomic::AtomicU64>) {
    use std::sync::atomic::Ordering;
    std::thread::Builder::new()
        .name("stall-watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let last = beat.load(Ordering::Relaxed);
            if last == 0 {
                continue;
            }
            let now = logs::unix_now() as u64;
            if now.saturating_sub(last) < 15 {
                continue;
            }
            let dir = logs::dir();
            let _ = std::fs::create_dir_all(&dir);
            let file = dir.join(format!("hang-{}.txt", logs::local_ts().replace(':', "")));
            eprintln!(
                "baton: runtime stalled {}s (open fds {}); sampling to {} and aborting",
                now - last,
                open_fds(),
                file.display()
            );
            let _ = std::process::Command::new("/usr/bin/sample")
                .args([&std::process::id().to_string(), "3", "-file"])
                .arg(&file)
                .output();
            logs::error(
                "stall",
                "RuntimeStall",
                &format!("no progress for {}s; sample {}", now - last, file.display()),
                vec![],
                vec![("open_fds", Py::Int(open_fds() as i128))],
            );
            std::process::abort();
        })
        .expect("stall watchdog thread");
}

fn main() {
    ensure_utf8_locale();
    raise_nofile_limit();
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("sessions") => {
            load_config(&args);
            let t0 = std::time::Instant::now();
            let (sessions, headless) =
                baton_core::sessions::build_all(&baton_core::sessions::Dirs::default_home(), None);
            eprintln!(
                "{} sessions, {} headless in {} ms",
                sessions.len(),
                headless.len(),
                t0.elapsed().as_millis()
            );
            println!(
                "{}",
                serde_json::json!({"sessions": sessions, "headless": headless})
            );
            0
        }
        Some("serve") => {
            let cfg = load_config(&args);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(serve(cfg));
            0
        }
        Some("setup") => cli::setup::main(&args),
        Some("pair") => cli::setup::pair(&args),
        Some("doctor") => cli::doctor::main(&args),
        Some("config") => {
            let cfg = config::Config::load(&args);
            print!("{}", cfg.to_toml());
            println!("\n# sources:");
            for (k, s) in &cfg.sources {
                println!("#   {k:<15} {s}");
            }
            for w in &cfg.warnings {
                println!("# warning: {w}");
            }
            0
        }
        Some("--version" | "-V" | "version") => {
            println!("baton {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help" | "-h" | "help") => {
            println!("baton {}\n{USAGE}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            eprintln!("baton {}\n{USAGE}", env!("CARGO_PKG_VERSION"));
            2
        }
    };
    std::process::exit(code);
}

async fn serve(cfg: config::Config) {
    let listen: SocketAddr = match cfg.listen.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("baton: listen `{}` is not host:port", cfg.listen);
            std::process::exit(2);
        }
    };
    let upstream = cfg.upstream.clone();
    let state = Arc::new(state::State::new(cfg));
    let listener = TcpListener::bind(listen).await.expect("bind");
    let beat = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let beat = beat.clone();
        tokio::spawn(async move {
            loop {
                beat.store(
                    logs::unix_now() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }
    spawn_stall_watchdog(beat);
    tokio::spawn(api::list::prewarm(state.clone()));
    tokio::spawn(state.push.clone().run(state.events.subscribe()));
    eprintln!(
        "baton serve on {listen} (upstream {}, auth {})",
        upstream.as_deref().unwrap_or("none"),
        if state.token.is_empty() { "off" } else { "on" }
    );
    if state.token.is_empty() && !listen.ip().is_loopback() {
        eprintln!("baton: warning: no bearer token and listening on {listen}; run `baton setup`");
    }
    let mut accept_errors = 0u32;
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(c) => {
                accept_errors = 0;
                c
            }
            Err(e) => {
                // EMFILE and friends fail again immediately: retrying at
                // once spun a core and served nothing. Back off and say so.
                accept_errors += 1;
                if accept_errors == 1 || accept_errors.is_multiple_of(100) {
                    eprintln!("baton: accept failed ({e}); open fds {}", open_fds());
                    logs::error(
                        "accept",
                        "AcceptError",
                        &e.to_string(),
                        vec![],
                        vec![("open_fds", Py::Int(open_fds() as i128))],
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(&state, req, peer).await) }
            });
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

/// Every request is logged here, once: native, proxied, refused. Handlers
/// that need the request id (error records) read `logs::Rid`.
async fn handle(
    state: &Arc<state::State>,
    mut req: hyper::Request<hyper::body::Incoming>,
    peer: SocketAddr,
) -> hyper::Response<proxy::Body> {
    req.extensions_mut().insert(peer);
    let log = logs::RequestLog::begin(&req);
    req.extensions_mut().insert(logs::Rid(log.rid.clone()));
    let path = req.uri().path().to_string();
    if let Some(denied) = http::check_auth(req.headers(), &path, &state.token) {
        return log.finish(denied);
    }
    let resp = match api::route(state, req).await {
        Ok(resp) => resp,
        Err(req) => state.upstream.forward(req).await,
    };
    log.finish(resp)
}
