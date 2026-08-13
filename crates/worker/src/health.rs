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
    if is_ready(control_ok, blob_ok) {
        web::HttpResponse::Ok().json(&serde_json::json!({"ready": true}))
    } else {
        tracing::warn!(
            control_ok,
            blob_ok,
            "worker readiness: refusing traffic"
        );
        web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"ready": false}))
    }
}

/// BOTH dependencies, not either. Split out from the HTTP shell so the
/// and/or mistake is testable without standing up a `WorkerConfig`.
#[must_use]
fn is_ready(control_ok: bool, blob_ok: bool) -> bool {
    control_ok && blob_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::readiness::staleness_budget;

    #[test]
    fn either_dependency_alone_is_not_enough() {
        assert!(is_ready(true, true));
        assert!(!is_ready(true, false));
        assert!(!is_ready(false, true));
        assert!(!is_ready(false, false));
    }

    #[test]
    fn a_worker_that_never_polled_control_is_not_ready() {
        let readiness = WorkerReadiness::new();
        let budget = staleness_budget(std::time::Duration::from_secs(3));
        assert!(!readiness.control.is_fresh(budget));
        readiness.control.mark_success();
        assert!(readiness.control.is_fresh(budget));
    }

    // What these do NOT catch: that `readyz` actually calls `is_ready` with
    // the freshness stamp and the blob probe rather than with two constants,
    // and that the route is mounted at /readyz at all. Both are covered end to
    // end against the real binary by tests/health_endpoints.sh.
}
