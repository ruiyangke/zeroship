//! `sandbox-agent` — PID 1 in zeroship sandbox microVMs.
//!
//! Boots ntex on the compio reactor, installs the SIGCHLD reaper
//! (pre-bind so blocking SIGCHLD process-wide is the first thing we
//! do), and serves the agent HTTP API on `:7777` until ntex's own
//! signal handling drains us on SIGTERM / SIGINT.

use std::path::PathBuf;
use std::process::ExitCode;

use ntex::web::middleware::DefaultHeaders;
use ntex::web::{self, HttpResponse};
use tracing::{error, info};

use zeroship_sandbox_agent::{
    dropuser, handlers, proxy, proxy_ws, reap, state_from_env, version, DEFAULT_PORT,
    DEFAULT_WORKSPACE,
};

#[ntex::main]
async fn main() -> ExitCode {
    // SIGCHLD MUST be blocked before any code that might fork(). The
    // reaper installs the process-wide block + a dedicated waitpid
    // thread, so put it ahead of everything (env reads, logging
    // setup, ntex startup). Idempotent + safe even when not PID 1.
    reap::install();
    init_tracing();
    if let Err(e) = run().await {
        error!(error = %e, "agent fatal");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Init the workspace-wide tracing subscriber.
///
/// Format pluggable via `ZEROSHIP_LOG_FORMAT={pretty|compact|json|logfmt|bunyan}`;
/// auto-detects TTY for the default (json off-TTY, pretty on-TTY).
///
/// Filter resolution: `RUST_LOG` (standard tracing-subscriber env) wins. If
/// unset, fall back to the long-standing `SANDBOX_AGENT_LOG` env var so
/// existing operator runbooks keep working. If neither is set, default to
/// `info`.
fn init_tracing() {
    let default_filter = std::env::var("SANDBOX_AGENT_LOG")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "info,sandbox_agent=debug".to_string());
    zeroship_core::observability::init_tracing(&default_filter);
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

    std::fs::create_dir_all(&workspace)
        .map_err(|e| format!("create workspace {}: {e}", workspace.display()))?;

    // Chown the workspace to the drop user so /exec children
    // (which run as nobody:nogroup when we're PID 1 root) can
    // actually write into it. No-op when not running as root.
    dropuser::chown_workspace(&workspace);

    // Per-user home dir — the conventional $HOME for /exec
    // children. When the controller mounts a per-user PVC at
    // this path the dir already exists (provisioned by k8s);
    // we still ensure it's there + chown'd so first-time
    // sessions without a PVC also work. Fresh-PVC root-owned
    // mountpoint gets re-owned to nobody so caches are writable.
    let user_home = std::path::Path::new(dropuser::USER_HOME);
    if let Err(e) = std::fs::create_dir_all(user_home) {
        // Don't fail boot — without a writable HOME the agent
        // still works, just without the per-user cache benefit.
        tracing::warn!(path = %user_home.display(), error = %e, "create user home failed");
    }
    dropuser::chown_user_home(user_home);

    // (Reaper installed at the very top of `main` so SIGCHLD is
    // already blocked process-wide by the time we reach this point.)
    let state = state_from_env(workspace.clone())?;

    // R7-S1: bind the agent's own sandbox_id before serving so the
    // `/_clock_resync` handler can match controller-signed bodies
    // against it. Hard error if neither `SANDBOX_AGENT_SANDBOX_ID`
    // env nor `/run/keys/sandbox-id` mount provides it — without
    // this, the handler would refuse every cluster wake (500) and
    // the controller would surface that as a backend failure on the
    // restore path. The wrapper script must inject one of these
    // before launching the agent (rootfs v5 + controller v18 land
    // the wire change together).
    handlers::init_sandbox_id_from_env().map_err(|e| {
        format!(
            "R7-S1: cannot bind sandbox_id for /_clock_resync: {e}. \
             Set SANDBOX_AGENT_SANDBOX_ID env or mount /run/keys/sandbox-id."
        )
    })?;

    let bind = format!("0.0.0.0:{port}");
    info!(bind = %bind, workspace = %workspace.display(), "agent listening");

    // WebSocket Upgrade handler — Phase 2. Bound on a separate port
    // (default 7778; configurable via `SANDBOX_AGENT_WS_PORT`) and
    // serves ONLY `/proxy/{port}/{path*}` Upgrade requests verified
    // under the V1_1_Ws canonical (ED25519-V1.1-WS domain tag). The
    // controller learns the port via the `proxy.ws-v1` capability.
    //
    // Implementation note: ntex's connection-hijack surface
    // (`HttpRequest::head().take_io()`) doesn't compose cleanly with
    // a compio TcpStream upstream splice; the proposal explicitly
    // permits a separate compio listener for the WS path
    // (`docs/proposals/sandbox-preview-urls.md` Phase 2 plan, "If
    // ntex's surface is too awkward, fall back to a compio raw-socket
    // path"). We take that path.
    let ws_port = proxy_ws::ws_port_from_env();
    let ws_state = state.clone();
    compio::runtime::spawn(async move {
        if let Err(e) = proxy_ws::serve(ws_state, ws_port).await {
            tracing::error!(error = %e, ws_port, "[sandbox-agent] proxy_ws serve exited with error");
        }
    })
    .detach();
    info!(ws_port, "agent ws-upgrade listener");

    // Per-route payload caps — applied at the ntex extractor so
    // oversize bodies are rejected before handlers run (no
    // buffering huge requests just to fail). The previous global
    // cap of `MAX_BYTES + 1 MiB` applied to /exec too, which let
    // an authenticated-or-not client send ~6 MiB of body per
    // /exec request and burn agent CPU on SHA-256 + signature
    // verify before rejecting. /exec only needs a JSON envelope
    // (cmd + cwd + timeout_ms); 64 KiB is generous.
    //
    // The `/files/{path}*` write path is the only legitimate
    // bulk-upload route; it gets the full file-size budget plus
    // serialization slack.
    let exec_limit: usize = 64 * 1024;
    let files_limit: usize =
        zeroship_sandbox_agent::files::MAX_BYTES + 1024 * 1024;
    // /shutdown takes no body; everything else is GET. A small
    // generic cap protects miscellaneous unsigned probe routes
    // from being abused as a CPU sink.
    let small_limit: usize = 4 * 1024;
    // /proxy/{port}/{path*} body cap: matches the proxy module's
    // DEFAULT_MAX_BODY_BYTES (100 MiB) plus 1 MiB serialization slack.
    // Larger requests are 413'd in the handler before the body is
    // read to completion (see `proxy_http`'s leading body-cap check).
    let proxy_limit: usize = proxy::DEFAULT_MAX_BODY_BYTES + 1024 * 1024;

    let proto = version::PROTOCOL_VERSION.to_string();
    // AppState is already cheap-clone (`Arc<Token>`, `Arc<Workspace>`,
    // `Arc<AtomicBool>`), so we capture by move and clone per-worker
    // inside the factory. No outer Arc<AppState> needed.
    web::server(async move || {
        let state = state.clone();
        let proto = proto.clone();
        web::App::new()
            .state(state)
            // Default payload cap covers any route that doesn't set its
            // own. We override per-resource below for /exec and PUT /files.
            .state(web::types::PayloadConfig::default().limit(small_limit))
            // X-Sbx-Protocol on every response — controller checks
            // this to verify it's talking to a supported agent version.
            .middleware(DefaultHeaders::new().header("X-Sbx-Protocol", &proto))
            // Liveness / readiness / version (unauthenticated)
            .service(web::resource("/livez").route(web::get().to(handlers::livez)))
            .service(web::resource("/readyz").route(web::get().to(handlers::readyz)))
            .service(web::resource("/version").route(web::get().to(handlers::version_info)))
            // Prometheus scrape — unauthenticated; cluster NetworkPolicy
            // governs who can reach :7777 to scrape.
            .service(web::resource("/metrics").route(web::get().to(handlers::metrics)))
            // /healthz preserved as an alias for /livez (back-compat).
            .service(web::resource("/healthz").route(web::get().to(handlers::livez)))
            // Auth-gated endpoints — per-resource state overrides the
            // app-level default. Bytes/Json extractors look this up.
            .service(
                web::resource("/exec")
                    .state(web::types::PayloadConfig::default().limit(exec_limit))
                    .route(web::post().to(handlers::exec_cmd)),
            )
            .service(web::resource("/tree").route(web::get().to(handlers::file_tree)))
            .service(web::resource("/shutdown").route(web::post().to(handlers::shutdown)))
            // Bug #22 fix: POST /_clock_resync — controller calls
            // this once after CH `--restore` to repair the guest's
            // frozen-at-snapshot `CLOCK_REALTIME`. The handler
            // verifies the signature WITHOUT applying the strict
            // 5-second skew window (the very window that the broken
            // clock would otherwise blow through). Body is a tiny
            // JSON `{"ts": <unix_secs>}`; cap at the small_limit
            // generic budget.
            .service(
                web::resource("/_clock_resync")
                    .route(web::post().to(handlers::clock_resync)),
            )
            .service(
                web::resource("/files/{path}*")
                    .state(web::types::PayloadConfig::default().limit(files_limit))
                    .route(web::get().to(handlers::read_file))
                    .route(web::put().to(handlers::write_file))
                    .route(web::delete().to(handlers::delete_file)),
            )
            // Sandbox preview proxy (preview-URL § II.1, Phase 1).
            // ANY /proxy/{port}/{path*} forwards to 127.0.0.1:{port}
            // inside the VM. Verify is via v1.1 canonical (path+query,
            // ED25519-V1.1 domain tag); the dispatcher in
            // `handlers::canonical_kind_for` keys off the `/proxy/`
            // prefix. Body cap matches the proxy module's
            // DEFAULT_MAX_BODY_BYTES (100 MiB).
            //
            // `{path}*` — tail-match for multi-segment paths
            // (Vite-style `/assets/main-abc.js`). ntex's `{path:.*}`
            // only matches a single segment.
            .service(
                web::resource("/proxy/{port}/{path}*")
                    .state(
                        web::types::PayloadConfig::default()
                            .limit(proxy_limit),
                    )
                    .route(web::route().to(proxy::proxy_http)),
            )
            .default_service(web::route().to(|| async {
                HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"}))
            }))
    })
    .bind(&bind)
    .map_err(|e| format!("bind {bind}: {e}"))?
    // Hard cap on graceful drain: a long /exec can't hold the
    // shutdown forever. After this many seconds we abort
    // in-flight handlers and exit.
    .shutdown_timeout(ntex::time::Seconds(30))
    .run()
    .await
    .map_err(|e| format!("serve: {e}"))
}
