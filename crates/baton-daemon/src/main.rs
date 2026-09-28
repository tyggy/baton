//! `baton` — the Baton daemon (`baton serve`); TUI and setup come later
//! (PLAN.md). Native endpoints live in `api/`; anything they don't claim is
//! forwarded to the Python reference server until the migration is done.

mod api;
mod cmd;
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

const USAGE: &str = "usage: baton serve [--listen 127.0.0.1:19880] [--upstream http://127.0.0.1:19877] [--relay URL] [--config FILE]\n       baton sessions   (session list as JSON)";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// `~/…` → `$HOME/…`. launchd doesn't expand `~`, and the Mac app's
/// LaunchAgent plist can't know the user's home at build time.
pub fn expand_home(p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
        }
        None => std::path::PathBuf::from(p),
    }
}

/// `--config FILE`: `key=value` lines (`listen`, `upstream`), written by the
/// Mac app so its bundled LaunchAgent can keep fixed arguments. A missing
/// file means defaults. Explicit flags win over the file.
fn config_value(args: &[String], key: &str) -> Option<String> {
    let path = flag(args, "--config")?;
    let text = std::fs::read_to_string(expand_home(&path)).ok()?;
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
        .rfind(|v| !v.is_empty())
}

fn arg(args: &[String], name: &str, default: &str) -> String {
    flag(args, name)
        .or_else(|| config_value(args, name.trim_start_matches("--")))
        .unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("sessions") => {
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
        }
        Some("serve") => serve(&args).await,
        _ => {
            eprintln!("baton {}\n{USAGE}", env!("CARGO_PKG_VERSION"));
            std::process::exit(2);
        }
    }
}

async fn serve(args: &[String]) {
    let listen: SocketAddr = arg(args, "--listen", "127.0.0.1:19880")
        .parse()
        .expect("--listen host:port");
    let upstream_url = arg(args, "--upstream", "http://127.0.0.1:19877");
    // Push relay: BATON_RELAY_URL, else --relay / `relay=` in --config.
    let relay = Some(arg(args, "--relay", "")).filter(|r| !r.is_empty());
    let state = Arc::new(state::State::new(
        proxy::Upstream::new(&upstream_url),
        relay,
    ));
    let listener = TcpListener::bind(listen).await.expect("bind");
    tokio::spawn(api::list::prewarm(state.clone()));
    tokio::spawn(state.push.clone().run(state.events.subscribe()));
    eprintln!(
        "baton serve on {listen} (upstream {upstream_url}, auth {})",
        if state.token.is_empty() { "off" } else { "on" }
    );
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(&state, req).await) }
            });
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

async fn handle(
    state: &Arc<state::State>,
    req: hyper::Request<hyper::body::Incoming>,
) -> hyper::Response<proxy::Body> {
    let path = req.uri().path().to_string();
    if let Some(denied) = http::check_auth(req.headers(), &path, &state.token) {
        return denied;
    }
    match api::route(state, req).await {
        Ok(resp) => resp,
        Err(req) => state.upstream.forward(req).await,
    }
}
