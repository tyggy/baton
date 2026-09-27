//! Group `read`: /api/session/{id}, /api/messages/{id}, /api/tail/{id}, /api/asset, /api/children/{id}.
//! Parity reference: claude-session-manager/server/api_server.py.

use super::Routed;
use crate::state::State;
use hyper::body::Incoming;
use hyper::Request;
use std::sync::Arc;

pub async fn route(_state: &Arc<State>, req: Request<Incoming>) -> Routed {
    Err(req)
}
