//! Group `control`: /api/spawn, /api/resume, /api/kill-session, /api/respond, /api/hook-event, /api/register-device, /api/paste-image, /api/health (native), Codex thread mapping.
//! Parity reference: claude-session-manager/server/api_server.py.

use super::Routed;
use crate::state::State;
use hyper::body::Incoming;
use hyper::Request;
use std::sync::Arc;

pub async fn route(_state: &Arc<State>, req: Request<Incoming>) -> Routed {
    Err(req)
}
