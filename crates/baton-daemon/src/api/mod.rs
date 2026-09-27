//! Native endpoints, one module per migration group (PLAN.md). Each
//! `route` returns `Ok(response)` for endpoints it implements and hands the
//! request back (`Err(req)`) otherwise; unclaimed requests go to the Python
//! reference server through the proxy.

pub mod control;
pub mod list;
pub mod read;
pub mod tmux;

use crate::proxy::Body;
use crate::state::State;
use hyper::body::Incoming;
use hyper::{Request, Response};
use std::sync::Arc;

pub type Routed = Result<Response<Body>, Request<Incoming>>;

pub async fn route(state: &Arc<State>, req: Request<Incoming>) -> Routed {
    let req = match list::route(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    let req = match read::route(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    let req = match tmux::route(state, req).await {
        Ok(r) => return Ok(r),
        Err(req) => req,
    };
    control::route(state, req).await
}
