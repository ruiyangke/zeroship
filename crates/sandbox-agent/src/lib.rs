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
pub mod sig;
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

/// Build [`AppState`] from explicit paths. Tests use this directly
/// to avoid mutating `SANDBOX_AGENT_TOKEN_FILE` (which races with
/// other parallel tests).
pub fn state_with_paths(
    token_path: &std::path::Path,
    workspace_path: &std::path::Path,
) -> Result<AppState, String> {
    let key = auth::load_key_from_path(token_path)?;
    let verifier = sig::Verifier::new(key);
    let workspace = files::Workspace::open(workspace_path)?;
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(AppState {
        verifier: Arc::new(verifier),
        workspace: Arc::new(workspace),
        draining: Arc::new(AtomicBool::new(false)),
        started_at_unix: started_at,
    })
}

/// Convenience: build the default state from env + the given workspace.
/// Reads the token from `SANDBOX_AGENT_TOKEN_FILE` (default
/// [`auth::DEFAULT_TOKEN_PATH`]); the file is unlinked after read.
pub fn state_from_env(workspace_path: PathBuf) -> Result<AppState, String> {
    let token_path = std::env::var("SANDBOX_AGENT_TOKEN_FILE")
        .unwrap_or_else(|_| auth::DEFAULT_TOKEN_PATH.to_string());
    state_with_paths(std::path::Path::new(&token_path), &workspace_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(label: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let p = std::env::temp_dir().join(format!("zsbx-libtest-{label}-{pid}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_token_in(dir: &std::path::Path) -> PathBuf {
        let p = dir.join("token");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"abcdefghijklmnopqrstuvwxyz0123456789").unwrap();
        p
    }

    #[test]
    fn state_with_paths_succeeds_with_valid_inputs() {
        let dir = unique_dir("ok");
        let token_path = write_token_in(&dir);
        let ws_path = dir.join("ws");
        let state = state_with_paths(&token_path, &ws_path).unwrap();
        // The token file should have been unlinked.
        assert!(!token_path.exists());
        // Workspace dir should have been created.
        assert!(ws_path.exists() && ws_path.is_dir());
        // Default state values.
        assert!(!state.is_draining());
        assert!(state.started_at_unix > 0);
    }

    #[test]
    fn state_with_paths_errors_on_missing_token() {
        let dir = unique_dir("missing");
        let token_path = dir.join("nope");
        let ws_path = dir.join("ws");
        let r = state_with_paths(&token_path, &ws_path);
        assert!(r.is_err());
    }

    #[test]
    fn state_with_paths_errors_on_short_token() {
        let dir = unique_dir("short");
        let token_path = dir.join("token");
        std::fs::write(&token_path, b"too-short").unwrap();
        let r = state_with_paths(&token_path, &dir.join("ws"));
        assert!(r.is_err());
        // Per Round 4 fix: file unlinked even on validation failure.
        assert!(!token_path.exists());
    }

    #[test]
    fn state_with_paths_started_at_is_recent() {
        let dir = unique_dir("ts");
        let token_path = write_token_in(&dir);
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let state = state_with_paths(&token_path, &dir.join("ws")).unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(state.started_at_unix >= before);
        assert!(state.started_at_unix <= after);
    }

    #[test]
    fn app_state_draining_helpers() {
        let dir = unique_dir("draining");
        let token_path = write_token_in(&dir);
        let state = state_with_paths(&token_path, &dir.join("ws")).unwrap();
        assert!(!state.is_draining());
        state.mark_draining();
        assert!(state.is_draining());
        // Idempotent.
        state.mark_draining();
        assert!(state.is_draining());
    }
}
