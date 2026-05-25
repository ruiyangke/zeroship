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
pub mod dropuser;
pub(crate) mod error_envelope;
pub mod exec;
pub mod files;
pub mod handlers;
pub mod metrics;
pub mod proxy;
pub mod proxy_ws;
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

/// Same value, different name — this is the "controller-side" alias
/// for the agent's listening port. Anywhere a backend formats an
/// agent URL (e.g. `http://<vm-ip>:7777`) should reference
/// [`AGENT_PORT`] rather than hard-coding 7777, so a future port
/// change is one edit.
pub const AGENT_PORT: u16 = DEFAULT_PORT;

/// Build [`AppState`] from explicit paths. Tests use this directly
/// to avoid mutating `SANDBOX_AGENT_PUBKEY_FILE` (which races with
/// other parallel tests).
pub fn state_with_paths(
    pubkey_path: &std::path::Path,
    workspace_path: &std::path::Path,
) -> Result<AppState, String> {
    let pubkey = auth::load_pubkey_from_path(pubkey_path)
        .map_err(|e| format!("load pubkey {}: {e}", pubkey_path.display()))?;
    let verifier = sig::Verifier::new(pubkey);
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
/// Reads the public key from `SANDBOX_AGENT_PUBKEY_FILE` (default
/// [`auth::DEFAULT_PUBKEY_PATH`]). The file persists after read —
/// the pubkey is non-secret and the mount is read-only anyway.
pub fn state_from_env(workspace_path: PathBuf) -> Result<AppState, String> {
    let pubkey_path = std::env::var("SANDBOX_AGENT_PUBKEY_FILE")
        .unwrap_or_else(|_| auth::DEFAULT_PUBKEY_PATH.to_string());
    state_with_paths(std::path::Path::new(&pubkey_path), &workspace_path)
}

/// Bind the agent's boot-time `sandbox_id` from `SANDBOX_AGENT_SANDBOX_ID`
/// env (preferred) or `/run/keys/sandbox-id` file (fallback). The
/// `/_clock_resync` handler (R7-S1) matches controller-signed bodies
/// against this id, so calling this exactly once before `ntex::run` is
/// mandatory for cluster-wake correctness — without it the handler
/// 500s every request and the controller surfaces that as a backend
/// failure on the restore path.
///
/// **The canonical entry point invoked by the binary's `main.rs`; not
/// for any other consumer.** This crate's `[lib]`/`[bin]` split forces
/// the binary to compile against the lib's public API, so the
/// underlying [`handlers::init_sandbox_id_from_env`] stays
/// `pub(crate)` and this wrapper is the single named surface bin code
/// reaches through. Tests use `handlers::test_set_sandbox_id` directly.
///
/// Idempotent: a second call after the OnceLock is set returns `Ok`
/// without re-reading. Subsequent calls with a DIFFERENT id return
/// `Ok` but do NOT overwrite — `OnceLock::set` is write-once.
pub fn boot_init_sandbox_id() -> Result<(), String> {
    handlers::init_sandbox_id_from_env()
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

    fn write_pubkey_in(dir: &std::path::Path) -> PathBuf {
        // Deterministic Ed25519 keypair for tests. The agent only
        // sees the public side; nothing in the agent code path
        // ever touches a SigningKey.
        use ed25519_dalek::SigningKey;
        let pk = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        let p = dir.join("controller-pubkey");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(pk.as_bytes()).unwrap();
        p
    }

    #[test]
    fn state_with_paths_succeeds_with_valid_inputs() {
        let dir = unique_dir("ok");
        let pubkey_path = write_pubkey_in(&dir);
        let ws_path = dir.join("ws");
        let state = state_with_paths(&pubkey_path, &ws_path).unwrap();
        // Pubkey file persists — read-only mount, non-secret.
        assert!(pubkey_path.exists());
        // Workspace dir should have been created.
        assert!(ws_path.exists() && ws_path.is_dir());
        // Default state values.
        assert!(!state.is_draining());
        assert!(state.started_at_unix > 0);
    }

    #[test]
    fn state_with_paths_errors_on_missing_pubkey() {
        let dir = unique_dir("missing");
        let pubkey_path = dir.join("nope");
        let ws_path = dir.join("ws");
        let r = state_with_paths(&pubkey_path, &ws_path);
        assert!(r.is_err());
    }

    #[test]
    fn state_with_paths_errors_on_garbage_pubkey() {
        let dir = unique_dir("garbage");
        let pubkey_path = dir.join("controller-pubkey");
        // 40 bytes that aren't 32-raw and aren't valid base64.
        let bad: Vec<u8> = (0..40).map(|i| i as u8).collect();
        std::fs::write(&pubkey_path, bad).unwrap();
        let r = state_with_paths(&pubkey_path, &dir.join("ws"));
        assert!(r.is_err());
    }

    #[test]
    fn state_with_paths_started_at_is_recent() {
        let dir = unique_dir("ts");
        let pubkey_path = write_pubkey_in(&dir);
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let state = state_with_paths(&pubkey_path, &dir.join("ws")).unwrap();
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
        let pubkey_path = write_pubkey_in(&dir);
        let state = state_with_paths(&pubkey_path, &dir.join("ws")).unwrap();
        assert!(!state.is_draining());
        state.mark_draining();
        assert!(state.is_draining());
        // Idempotent.
        state.mark_draining();
        assert!(state.is_draining());
    }
}
