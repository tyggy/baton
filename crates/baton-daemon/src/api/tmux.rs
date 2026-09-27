//! Group `tmux`: /api/pane-stream/{name}, /api/pane/{name}, /api/scroll, /api/send, /api/send-key, /api/sessions, resize.
//! Parity reference: claude-session-manager/server/api_server.py.

use super::Routed;
use crate::state::State;
use hyper::body::Incoming;
use hyper::Request;
use std::sync::Arc;

pub async fn route(_state: &Arc<State>, req: Request<Incoming>) -> Routed {
    Err(req)
}
