//! `sandbox-agent` — PID 1 in zeroship sandbox microVMs.
//!
//! Boots a tokio runtime, starts the SIGCHLD reaper, and serves the
//! axum HTTP API on `:7777` until the host sends SIGTERM (graceful)
//! or SIGINT (graceful, dev-friendly).

use std::path::PathBuf;
use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};

use zeroship_sandbox_agent::{router, state_from_env, DEFAULT_PORT, DEFAULT_WORKSPACE};

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(e) = run().await {
        eprintln!("[agent] fatal: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<(), String> {
    let workspace = PathBuf::from(
        std::env::var("SANDBOX_AGENT_WORKSPACE")
            .unwrap_or_else(|_| DEFAULT_WORKSPACE.to_string()),
    );
    let port: u16 = match std::env::var("SANDBOX_AGENT_PORT") {
        Ok(s) => s.parse().map_err(|e| format!("SANDBOX_AGENT_PORT: {e}"))?,
        Err(_) => DEFAULT_PORT,
    };

    // Make sure /workspace exists. In production it'll be a virtio-fs
    // mount or PVC; in dev it might not be — we mkdir for ergonomics.
    std::fs::create_dir_all(&workspace)
        .map_err(|e| format!("create workspace {}: {e}", workspace.display()))?;

    let state = state_from_env(workspace.clone())?;
    let app = router(state);

    // PID 1 zombie reaper. No-op when not PID 1; harmless either way.
    zeroship_sandbox_agent::reap::spawn();

    let bind = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("bind {bind}: {e}"))?;
    eprintln!("[agent] listening on http://{bind}, workspace={}", workspace.display());

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("serve: {e}"))
}

/// Resolve when SIGTERM (graceful container stop) or SIGINT (Ctrl-C)
/// arrives. Lets in-flight requests drain before the process exits.
async fn shutdown_signal() {
    let term = async {
        let mut s = signal(SignalKind::terminate()).expect("install SIGTERM");
        s.recv().await;
    };
    let int = async {
        let mut s = signal(SignalKind::interrupt()).expect("install SIGINT");
        s.recv().await;
    };
    tokio::select! {
        _ = term => eprintln!("[agent] SIGTERM received, draining"),
        _ = int  => eprintln!("[agent] SIGINT received, draining"),
    }
}
