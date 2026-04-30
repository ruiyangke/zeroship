//! In-VM agent (PID 1) for zeroship sandbox microVMs.
//!
//! See [`crate::handlers`] for the HTTP surface, [`crate::files`] for
//! the path-safe workspace ops, [`crate::exec`] for shell execution,
//! [`crate::auth`] for the bearer-token check, and [`crate::reap`]
//! for the PID 1 zombie reaper.
//!
//! Built on the same compio + ntex stack the rest of the workspace
//! uses, so we have one async story end-to-end.

pub mod audit;
pub mod auth;
pub mod exec;
pub mod files;
pub mod handlers;
pub mod reap;
pub mod version;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub use handlers::AppState;

/// Default workspace mount path inside the VM. Matches the controller's
/// bind-mount path so creator code that hard-codes `/workspace` works
/// the same in either backend.
pub const DEFAULT_WORKSPACE: &str = "/workspace";

/// Default port. Controller talks to the agent on this. Kept stable
/// so the cluster NetworkPolicy can name a single port.
pub const DEFAULT_PORT: u16 = 7777;

/// Convenience: build the default state from env + the given workspace.
/// Reads the token from `SANDBOX_AGENT_TOKEN_FILE` (default
/// [`auth::DEFAULT_TOKEN_PATH`]); the file is unlinked after read.
pub fn state_from_env(workspace_path: PathBuf) -> Result<AppState, String> {
    let token_path = std::env::var("SANDBOX_AGENT_TOKEN_FILE")
        .unwrap_or_else(|_| auth::DEFAULT_TOKEN_PATH.to_string());
    let token = auth::Token::from_path(std::path::Path::new(&token_path))?;
    let workspace = files::Workspace::open(&workspace_path)?;
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(AppState {
        token: Arc::new(token),
        workspace: Arc::new(workspace),
        draining: Arc::new(AtomicBool::new(false)),
        started_at_unix: started_at,
    })
}
