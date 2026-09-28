//! Native endpoints, one module per migration group (PLAN.md). Each
//! `route` returns `Ok(response)` for endpoints it implements and hands the
//! request back (`Err(req)`) otherwise; unclaimed requests go to the Python
//! reference server through the proxy.

pub mod control;
pub mod list;
pub mod read;
pub mod sysread;
pub mod tmux;

use crate::proxy::Body;
use crate::state::State;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use std::sync::Arc;

pub type Routed = Result<Response<Body>, Request<Incoming>>;

pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
    let req = match push(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    let req = match list::route(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    let req = match read::route(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    let req = match sysread::route(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    let req = match tmux::route(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    control::route(state, req).await
}

/// `POST /api/push/register` / `POST /api/push/unregister` (docs/relay.md).
async fn push(state: &Arc<State>, req: Request<Incoming>) -> Routed {
    let path = req.uri().path();
    if req.method() != Method::POST
        || !matches!(path, "/api/push/register" | "/api/push/unregister")
    {
        return Err(req);
    }
    let register = path == "/api/push/register";
    let headers = req.headers().clone();
    let (status, body) = match req.into_body().collect().await {
        Ok(b) if register => state.push.register(&b.to_bytes()),
        Ok(b) => state.push.unregister(&b.to_bytes()),
        Err(_) => (400, serde_json::json!({"error": "unreadable body"})),
    };
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    Ok(crate::http::json(status, &body, &headers))
}
