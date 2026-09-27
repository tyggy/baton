//! `baton` — the Baton daemon (`baton serve`); TUI and setup come later
//! (PLAN.md). During the migration the daemon forwards every endpoint it
//! doesn't implement natively to the Python reference server.

mod proxy;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::TcpListener;

const USAGE: &str =
    "usage: baton serve [--listen 127.0.0.1:19880] [--upstream http://127.0.0.1:19877]";

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("serve") {
        eprintln!("baton {}\n{USAGE}", env!("CARGO_PKG_VERSION"));
        std::process::exit(2);
    }
    let listen: SocketAddr = arg(&args, "--listen", "127.0.0.1:19880")
        .parse()
        .expect("--listen host:port");
    let upstream = proxy::Upstream::new(&arg(&args, "--upstream", "http://127.0.0.1:19877"));
    let listener = TcpListener::bind(listen).await.expect("bind");
    eprintln!(
        "baton serve on {listen} (upstream {})",
        arg(&args, "--upstream", "http://127.0.0.1:19877")
    );
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let up = upstream.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let up = up.clone();
                async move { Ok::<_, std::convert::Infallible>(up.forward(req).await) }
            });
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}
