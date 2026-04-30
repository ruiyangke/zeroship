//! In-VM agent (PID 1) for zeroship sandbox microVMs.
//!
//! See [`crate::handlers`] for the HTTP surface, [`crate::files`] for
//! the path-safe workspace ops, [`crate::exec`] for shell execution,
//! [`crate::auth`] for the bearer-token check, and [`crate::reap`]
//! for the PID 1 zombie reaper.

pub mod auth;
pub mod exec;
pub mod files;
pub mod handlers;
pub mod reap;

use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{delete, get, post, put};
use axum::Router;

pub use handlers::AppState;

/// Default workspace mount path inside the VM. Matches the controller's
/// bind-mount path so creator code that hard-codes `/workspace` works
/// the same in either backend.
pub const DEFAULT_WORKSPACE: &str = "/workspace";

/// Default port. Controller talks to the agent on this. Kept stable
/// so the cluster NetworkPolicy can name a single port.
pub const DEFAULT_PORT: u16 = 7777;

/// Build the axum Router. `/healthz` is unauthenticated (cold-start
/// polling); everything else goes through the bearer-token middleware.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/exec", post(handlers::exec_cmd))
        .route("/tree", get(handlers::file_tree))
        .route("/files/*path", get(handlers::read_file))
        .route("/files/*path", put(handlers::write_file))
        .route("/files/*path", delete(handlers::delete_file))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            handlers::require_token,
        ));

    Router::new()
        .route("/healthz", get(handlers::healthz))
        .merge(protected)
        .with_state(state)
}

/// Convenience: build the default state from env + the given workspace.
pub fn state_from_env(workspace: PathBuf) -> Result<AppState, String> {
    Ok(AppState {
        token: Arc::new(auth::Token::from_env()?),
        workspace,
    })
}
