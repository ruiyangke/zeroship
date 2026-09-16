//! `GET /healthz` (liveness) and `GET /readyz` (readiness) for the worker.
//!
//! The worker cannot serve a request without BOTH of its dependencies:
//!
//! - the control plane, which tells it which deploy of an app to run
//!   (`/internal/versions`) and hands it the app's env;
//! - the blob store, which holds the module bytes of that deploy.
//!
//! The two are probed differently on purpose:
//!
//! - Control-plane reachability is read from the freshness stamp the
//!   process-wide version poller already writes. A `/readyz` probe issues no
//!   control-plane request at all.
//! - The blob store has no such loop, so it IS actively probed - through a
//!   [`ReadinessGate`], which bounds the probe with a timeout, caches the
//!   outcome for a short TTL and collapses concurrent probes. Worst case is
//!   one blob-store stat (local) or one HEAD (S3) per TTL per process.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::State;
use zeroship_core::readiness::{staleness_budget, ReadinessGate, SyncFreshness};

use crate::WorkerConfig;

/// Process-wide readiness state, shared by the version poller (writer) and
/// every ntex worker thread's `/readyz` handler (readers).
#[derive(Debug, Default)]
pub struct WorkerReadiness {
    /// Stamped by `sync::version_poll_loop` on each successful control poll.
    pub control: SyncFreshness,
    /// Guards the actively-probed blob-store check.
    pub blob_store: ReadinessGate,
}

impl WorkerReadiness {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/healthz").route(web::get().to(healthz)))
        .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Liveness. Constant 200 by design - a control-plane or blob-store outage
/// must not get every worker container restarted on top of it.
pub async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"ok": true}))
}

/// Readiness. 200 only when the control poll is current AND the blob store
/// answers. The body is `{"ready":true|false}` - which of the two failed is in
/// the logs, not in the response to an unauthenticated caller.
pub async fn readyz(
    config: State<Arc<WorkerConfig>>,
    readiness: State<Arc<WorkerReadiness>>,
) -> web::HttpResponse {
    let budget = staleness_budget(std::time::Duration::from_secs(config.poll_interval_secs));
    let control_ok = readiness.control.is_fresh(budget);
    let blob_ok = readiness
        .blob_store
        .ready(|| async {
            match config.blob_store.probe().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "worker readiness: blob store unreachable");
                    false
                }
            }
        })
        .await;
    let dev_escape = zeroship_core::config::dev_escape_active();
    if is_ready(control_ok, blob_ok, dev_escape) {
        web::HttpResponse::Ok().json(&serde_json::json!({"ready": true}))
    } else {
        tracing::warn!(
            control_ok,
            blob_ok,
            dev_escape,
            "worker readiness: refusing traffic"
        );
        web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"ready": false}))
    }
}

/// BOTH dependencies, not either, AND no credential escape. Split out from the
/// HTTP shell so the and/or mistake is testable without standing up a
/// `WorkerConfig`.
///
/// `dev_escape` is the credential gate's readiness half: a worker that booted
/// on an empty or `CHANGE_ME_ZEROSHIP_SERVICE_KEY` credential in a
/// `debug_assertions` build must not be routed traffic, however healthy its two
/// dependencies are. It is a PARAMETER, not a call to
/// [`zeroship_core::config::dev_escape_active`], because the underlying flag is
/// process-wide and write-once: a test that set it would change the answer for
/// every later test in this binary.
#[must_use]
fn is_ready(control_ok: bool, blob_ok: bool, dev_escape: bool) -> bool {
    control_ok && blob_ok && !dev_escape
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::http::StatusCode;
    use ntex::web::test;
    use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
    use zeroship_core::readiness::staleness_budget;

    fn config(root: &std::path::Path) -> Arc<WorkerConfig> {
        let blob_store: Arc<dyn BlobStore> = Arc::new(
            LocalDiskBlobStore::new(root.to_owned()).expect("create test blob store"),
        );
        Arc::new(WorkerConfig {
            service_auth: crate::identity_fixture::service_auth(),
            control_url: "http://127.0.0.1:1".to_owned(),
            control_key: String::new(),
            kv_store: None,
            storage_backend: None,
            max_isolates: 1,
            poll_interval_secs: 60,
            shutdown_timeout_secs: 0,
            blob_store,
        })
    }

    async fn status(
        app: &ntex::Pipeline<
            impl ntex::Service<
                ntex::http::Request,
                Response = ntex::web::WebResponse,
                Error = ntex::web::Error,
            >,
        >,
        path: &str,
    ) -> StatusCode {
        test::call_service(app, test::TestRequest::get().uri(path).to_request())
            .await
            .status()
    }

    #[test]
    fn either_dependency_alone_is_not_enough() {
        assert!(is_ready(true, true, false));
        assert!(!is_ready(true, false, false));
        assert!(!is_ready(false, true, false));
        assert!(!is_ready(false, false, false));
    }

    /// A worker running on the development credential escape must not be routed
    /// traffic, even with both dependencies healthy.
    #[test]
    fn the_dev_credential_escape_alone_makes_a_healthy_worker_not_ready() {
        assert!(!is_ready(true, true, true));
        // The one-variable partner: identical inputs with the escape inactive
        // ARE ready, so the refusal is about the escape and nothing else.
        assert!(is_ready(true, true, false));
    }

    #[test]
    fn a_worker_that_never_polled_control_is_not_ready() {
        let readiness = WorkerReadiness::new();
        let budget = staleness_budget(std::time::Duration::from_secs(3));
        assert!(!readiness.control.is_fresh(budget));
        readiness.control.mark_success();
        assert!(readiness.control.is_fresh(budget));
    }

    #[ntex::test]
    async fn routes_report_liveness_and_dependency_readiness() {
        let storage = tempfile::tempdir().expect("private health-test storage");
        let config = config(storage.path());
        let readiness = Arc::new(WorkerReadiness::new());
        let app = test::init_service(
            web::App::new()
                .state(config)
                .state(readiness.clone())
                .configure(configure),
        )
        .await;

        assert_eq!(status(&app, "/healthz").await, StatusCode::OK);
        assert_eq!(
            status(&app, "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE,
            "a worker that has not reached control must refuse traffic"
        );

        readiness.control.mark_success();
        assert_eq!(status(&app, "/readyz").await, StatusCode::OK);

        let retired = status(&app, "/health").await;
        let unknown = status(&app, "/route-that-does-not-exist").await;
        assert_ne!(retired, StatusCode::OK);
        assert_eq!(retired, unknown, "the retired alias must not be a route");
    }
}
