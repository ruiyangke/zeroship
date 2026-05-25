//! `zeroship-sandbox` — pluggable backends (docker, k8s) for AI builder
//! sandboxes.
//!
//! Each editor sandbox gets its own runtime — a Docker container or a
//! libkrun-microVM Pod, depending on `SANDBOX_BACKEND`. Files and
//! commands are driven through the [`zeroship_sandbox::backend::Backend`]
//! abstraction.
//!
//! See `docs/superpowers/specs/2026-04-27-zeroship-editor-design.md`
//! for the full architecture.

use ntex::web;
use zeroship_sandbox::{
    admin_handlers, handlers, preview, preview_share_handlers, preview_ws, registry,
    AppState,
};
use zeroship_sandbox::config::SandboxConfig;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[ntex::main]
async fn main() -> std::io::Result<()> {
    zeroship_core::observability::init_tracing("info,sandbox_agent=debug,zeroship_sandbox=debug");

    let config = match SandboxConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "sandbox config error");
            std::process::exit(1);
        }
    };

    // FM-D: backend-aware startup banner. The previous unconditional
    // `else` branch printed k8s config (namespace, runtime_class,
    // port-forward) even when backend=nomad-ch — operators reading
    // the boot log got a misleading mix of irrelevant k8s fields and
    // missing nomad-ch fields. Print the section that matches the
    // configured backend; common fields (port, idle_timeout,
    // max_lifetime) print regardless.
    tracing::info!(
        backend = %config.backend,
        idle_timeout_secs = config.idle_timeout_secs,
        max_lifetime_secs = config.max_lifetime_secs,
        "sandbox boot config"
    );
    match config.backend.as_str() {
        "docker" => {
            tracing::info!(
                image = %config.image,
                workspace_root = %config.workspace_root.display(),
                network = %config.network,
                memory_mib = config.memory_mb,
                cpus = config.cpus,
                "sandbox docker backend"
            );
        }
        "k8s" => {
            tracing::info!(
                namespace = %config.k8s.namespace,
                agent_image = %config.k8s.image,
                runtime_class = %config.k8s.runtime_class,
                port_forward = config.k8s.use_port_forward,
                memory_mib = config.memory_mb,
                cpus = config.cpus,
                "sandbox k8s backend"
            );
        }
        "nomad-ch" => {
            tracing::info!(
                nomad_addr = %config.nomad_ch.nomad_addr,
                datacenter = %config.nomad_ch.datacenter,
                runtime_dir = %config.nomad_ch.runtime_dir.display(),
                host_state_dir = %config.nomad_ch.host_state_dir.display(),
                user_home_dir_root = %config.nomad_ch.user_home_dir_root.display(),
                vm_index_floor = config.nomad_ch.vm_index_floor,
                vm_index_ceil = config.nomad_ch.vm_index_ceil,
                subnet_second_octet = config.nomad_ch.subnet_second_octet,
                alloc_running_timeout_secs = config.nomad_ch.alloc_running_timeout_secs,
                livez_timeout_secs = config.nomad_ch.agent_livez_timeout_secs,
                startup_orphan_cleanup = config.nomad_ch.startup_orphan_cleanup,
                "sandbox nomad-ch backend"
            );
        }
        other => {
            tracing::warn!(backend = %other, "sandbox unknown backend; no banner detail");
        }
    }

    if config.token().is_empty() {
        tracing::warn!("sandbox: SANDBOX_TOKEN not set — endpoints are unauthenticated");
    }

    // Build state (probes the backend at startup).
    let state = match AppState::from_config(config.clone()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "sandbox backend probe failed");
            std::process::exit(1);
        }
    };

    // Idle GC sweep — kills runtimes idle longer than `idle_timeout_secs`.
    registry::start_idle_gc(state.clone());

    // Preview WebSocket-Upgrade forwarder (Phase 2). Bound on a
    // separate port (default 9092; configurable via
    // `SANDBOX_PREVIEW_WS_PORT`) per the proposal's Phase-2 fallback
    // ("the controller listens on a separate port for Upgrade
    // forwarding"). The HTTP path on `config.port` continues to handle
    // /sandboxes/{id}/preview/{port}/{path*} non-Upgrade traffic.
    let ws_port = preview_ws::ws_port_from_env();
    let ws_state = state.clone();
    compio::runtime::spawn(async move {
        if let Err(e) = preview_ws::serve(ws_state, ws_port).await {
            tracing::error!(error = %e, "sandbox preview_ws serve exited");
        }
    })
    .detach();
    tracing::info!(ws_port, "sandbox preview-ws listening");

    let bind = format!("0.0.0.0:{}", config.port);
    tracing::info!(bind = %bind, "sandbox listening");

    // Round-2 fixer / CRITICAL #3: keep a strong handle to AppState
    // so we can call `trigger_shutdown()` AFTER ntex's `server.run()`
    // returns. ntex installs its own SIGINT/SIGTERM handler — when
    // those signals arrive, `run()` stops accepting and waits for
    // in-flight requests to drain, then returns. We then flip the
    // shutdown flag so the detached heartbeat / takeover / health
    // tasks observe it on their next iteration and exit cleanly.
    //
    // Limitation: this is post-drain (not pre-drain) — peers won't
    // see the `'draining'` host status until after ntex has finished
    // draining HTTP. A pre-drain notification would require a signal
    // handler that runs BEFORE ntex's, which compio doesn't yet
    // expose. Tracked as a follow-up; not blocking for Round-2.
    let shutdown_state = state.clone();

    let server_result = web::server(async move || {
        web::App::new()
            .state(state.clone())
            .service(
                web::resource("/health")
                    .route(web::get().to(|| async {
                        web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
                    })),
            )
            // Liveness — process is up. Always 200; load balancers
            // should restart on repeated 5xx, not on /readyz=503.
            .service(
                web::resource("/livez")
                    .route(web::get().to(|| async {
                        web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
                    })),
            )
            // Readiness — backend is reachable. Returns 503 when
            // the backend probe most recently failed (kubectl auth
            // expired, cluster unreachable, etc.). LBs / orchestrators
            // route around the controller until the backend recovers.
            .service(
                web::resource("/readyz").route(web::get().to(handlers::readyz)),
            )
            // R26-API2: Prometheus text-exposition exporter over the
            // atomic counters in `crate::metrics`. Mounted at the
            // conventional `/metrics` path (root, NOT under /admin/*)
            // because Prometheus scrapers default to that location;
            // auth is `AdminRole::ReadOnly` so the bearer-leak threat
            // model stays symmetric with the rest of the operator API.
            // See `admin_handlers::metrics_endpoint` for the §10.0
            // envelope contract on 401/403/503.
            .service(
                web::resource("/metrics")
                    .route(web::get().to(admin_handlers::metrics_endpoint)),
            )
            .service(
                web::resource("/sandboxes")
                    .route(web::post().to(handlers::create_sandbox))
                    .route(web::get().to(handlers::list_sandboxes)),
            )
            .service(
                web::resource("/sandboxes/{id}")
                    .route(web::get().to(handlers::get_sandbox))
                    .route(web::delete().to(handlers::stop_sandbox)),
            )
            .service(
                web::resource("/sandboxes/{id}/exec")
                    .route(web::post().to(handlers::exec)),
            )
            .service(
                web::resource("/sandboxes/{id}/file-tree")
                    .route(web::get().to(handlers::file_tree)),
            )
            .service(
                web::resource("/sandboxes/{id}/files/{path}*")
                    .route(web::get().to(handlers::read_file))
                    .route(web::put().to(handlers::write_file))
                    .route(web::delete().to(handlers::delete_file)),
            )
            // Phase-3 admin / operator API. Auth is the
            // SANDBOX_ADMIN_TOKEN_PATH bearer (NOT SANDBOX_TOKEN);
            // when the path is unset every endpoint 503s with
            // "admin api disabled". See `admin_handlers.rs` for the
            // full surface + the deferred JWT/scope shape.
            .service(
                web::resource("/admin/sandboxes")
                    .route(web::get().to(admin_handlers::list_all_sandboxes)),
            )
            .service(
                web::resource("/admin/sandboxes/{id}")
                    .route(web::get().to(admin_handlers::get_sandbox_detail)),
            )
            .service(
                web::resource("/admin/users/{user_id}/sandboxes")
                    .route(web::get().to(admin_handlers::list_user_sandboxes)),
            )
            .service(
                web::resource("/admin/users/{user_id}/shares")
                    .route(web::get().to(admin_handlers::list_user_shares)),
            )
            .service(
                web::resource("/admin/users/{user_id}/export")
                    .route(web::get().to(admin_handlers::export_user)),
            )
            .service(
                web::resource("/admin/users/{user_id}")
                    .route(web::delete().to(admin_handlers::delete_user)),
            )
            .service(
                web::resource("/admin/hosts")
                    .route(web::get().to(admin_handlers::list_hosts)),
            )
            // Snapshot/restore admin routes (PR 2c stubs — return 501
            // until SANDBOX_SNAPSHOT_ENABLED=true ships in PR 3+).
            .service(
                web::resource("/admin/sandboxes/{id}/snapshot")
                    .route(web::post().to(admin_handlers::snapshot_sandbox)),
            )
            .service(
                web::resource("/admin/sandboxes/{id}/wake")
                    .route(web::post().to(admin_handlers::wake_sandbox)),
            )
            // C-7-LT-PR2: GET /wake/{wake_id} polling endpoint paired
            // with the POST above. Registered as a sibling resource so
            // ntex's path-matcher routes both shapes correctly (the
            // 1-param `{id}/wake` POST vs the 2-param
            // `{id}/wake/{wake_id}` GET).
            .service(
                web::resource("/admin/sandboxes/{id}/wake/{wake_id}")
                    .route(web::get().to(admin_handlers::poll_wake)),
            )
            .service(
                web::resource("/admin/sandboxes/{id}/cold-boot")
                    .route(web::post().to(admin_handlers::cold_boot_sandbox)),
            )
            // Phase-3 share-token mint/list/revoke (§ III). The
            // `/share` resource is registered BEFORE the catch-all
            // `/preview/{port}/{path}*` so ntex matches the more
            // specific routes first.
            .service(
                web::resource("/sandboxes/{id}/preview/{port}/share")
                    .route(web::post().to(preview_share_handlers::mint_share))
                    .route(web::get().to(preview_share_handlers::list_share))
                    .route(
                        web::delete().to(preview_share_handlers::revoke_all_share),
                    ),
            )
            .service(
                web::resource(
                    "/sandboxes/{id}/preview/{port}/share/{token_id}",
                )
                .route(web::delete().to(preview_share_handlers::revoke_one_share)),
            )
            // Preview proxy (§ II.2). Creator-authed; signed v1.1
            // forward to the agent at /proxy/{port}/{path*}. Body cap
            // matches the agent (100 MiB) plus 1 MiB serialization slack.
            //
            // NOTE: `{path}*` (tail-match) — NOT `{path:.*}`. ntex's
            // regex constraint matches a single path segment only;
            // `{tail}*` is the documented multi-segment tail-match
            // syntax. See gateway/main.rs for the same pattern.
            .service(
                web::resource("/sandboxes/{id}/preview/{port}/{path}*")
                    .state(
                        web::types::PayloadConfig::default()
                            .limit(preview::DEFAULT_MAX_BODY_BYTES + 1024 * 1024),
                    )
                    .route(web::route().to(preview::preview_proxy)),
            )
    })
    .bind(&bind)?
    .run()
    .await;

    // ntex.run() returned: SIGINT/SIGTERM was received and the HTTP
    // listener has finished draining. Flip the shutdown flag so the
    // detached heartbeat / takeover / health-probe loops exit on
    // their next iteration; best-effort UPDATE the host row to
    // `'draining'` so peers see the intent.
    tracing::info!("sandbox: HTTP server stopped; signalling background tasks to drain");
    shutdown_state.trigger_shutdown().await;
    // Bound the wait so a hung pg pool can't keep us alive forever.
    let drain_grace_secs = std::env::var("SANDBOX_HA_DRAIN_GRACE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    tracing::info!(
        drain_grace_secs,
        "sandbox: waiting for background tasks to observe shutdown"
    );
    compio::time::sleep(std::time::Duration::from_secs(drain_grace_secs)).await;
    tracing::info!("sandbox: drain grace elapsed; exiting");

    server_result
}
