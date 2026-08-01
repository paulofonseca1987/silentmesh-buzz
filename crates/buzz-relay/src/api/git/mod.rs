//! Git hosting — Smart HTTP transport, permission hooks, and policy engine.
//!
//! # Module structure
//!
//! - `transport` — Smart HTTP protocol (info/refs, upload-pack, receive-pack)
//! - `hook` — Pre-receive hook script and injection
//! - `policy` — Internal policy endpoint (HMAC-authenticated callback from hook)

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use tower_http::limit::RequestBodyLimitLayer;

use crate::state::AppState;

pub mod canonicalize;
pub mod cas_publish;
pub mod hook;
pub mod hydrate;
pub mod manifest;
pub mod manifest_event;
pub mod pack_cache;
pub mod policy;
pub mod promote;
pub mod store;
pub mod transport;

pub use transport::git_router;

/// Marker inserted by the UDS server so [`require_localhost`] can recognize a
/// request that arrived over the Unix socket.
///
/// It must be a *positive* signal rather than inferred from a missing
/// `ConnectInfo`: absence is also what a misconfigured TCP listener looks
/// like, and treating "I don't know where this came from" as "local" is the
/// one interpretation that fails open.
#[derive(Clone, Copy, Debug)]
pub struct LocalTransport;

/// Middleware that rejects requests that did not come from this host.
///
/// Defense-in-depth on top of the body HMAC: the internal policy endpoint is
/// only ever called by the pre-receive hook, which the relay spawns on its
/// own machine.
///
/// Two things count as this host:
///
/// - a **loopback peer** over TCP, and
/// - a request carrying [`LocalTransport`], which only the **Unix socket**
///   server attaches.
///
/// The socket case is not a relaxation — a Unix socket cannot be connected to
/// from another machine at all, so it is a stronger locality proof than a
/// loopback IP. It is required because a relay bound to one specific
/// non-loopback address is not listening on loopback, so the hook has no
/// loopback endpoint to call and every push would be refused here.
async fn require_localhost(req: Request<Body>, next: Next) -> Response {
    let over_uds = req.extensions().get::<LocalTransport>().is_some();
    let is_loopback = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false);

    if !over_uds && !is_loopback {
        return (StatusCode::FORBIDDEN, "internal endpoint: localhost only").into_response();
    }

    next.run(req).await
}

/// Build the internal git policy router.
///
/// Mounted at `/internal/git/policy` — only accessible from localhost.
/// The pre-receive hook calls this to authorize pushes.
/// Body limit: 1 MB (500 refs × ~200 bytes each = ~100 KB typical; 1 MB is generous).
pub fn git_policy_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/internal/git/policy", post(policy::hook_policy_check))
        .layer(RequestBodyLimitLayer::new(1024 * 1024)) // 1 MB
        .layer(middleware::from_fn(require_localhost))
        .with_state(state)
}
