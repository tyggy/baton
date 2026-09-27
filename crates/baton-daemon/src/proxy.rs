//! Pass-through to the Python reference server for every endpoint the
//! daemon doesn't implement yet (PLAN.md: strangler migration). Bodies are
//! streamed both ways, so SSE endpoints (events, tail, pane-stream) work
//! unchanged and nothing is buffered.

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;

pub type Body = BoxBody<Bytes, hyper::Error>;

/// Hop-by-hop headers are per connection, never forwarded (RFC 9110 §7.6.1).
const HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone)]
pub struct Upstream {
    client: Client<HttpConnector, Incoming>,
    base: String,
}

impl Upstream {
    pub fn new(base: &str) -> Self {
        let mut conn = HttpConnector::new();
        conn.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build(conn);
        Upstream {
            client,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub async fn forward(&self, mut req: Request<Incoming>) -> Response<Body> {
        let pq = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let uri: Uri = match format!("{}{}", self.base, pq).parse() {
            Ok(u) => u,
            Err(_) => return error(StatusCode::BAD_REQUEST, "bad uri"),
        };
        *req.uri_mut() = uri;
        strip_hop(req.headers_mut());
        match self.client.request(req).await {
            Ok(resp) => {
                let (mut parts, body) = resp.into_parts();
                strip_hop(&mut parts.headers);
                parts.headers.insert(
                    HeaderName::from_static("x-baton-daemon"),
                    HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
                );
                Response::from_parts(parts, body.boxed())
            }
            Err(e) => error(StatusCode::BAD_GATEWAY, &format!("upstream: {e}")),
        }
    }
}

fn strip_hop(h: &mut hyper::HeaderMap) {
    for name in HOP {
        h.remove(name);
    }
}

pub fn error(status: StatusCode, msg: &str) -> Response<Body> {
    let body = serde_json::json!({"error": msg}).to_string();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("static response")
}
