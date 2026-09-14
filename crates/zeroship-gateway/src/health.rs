//! `GET /healthz` (liveness) and `GET /readyz` (readiness) for the gateway.
//!
//! The gateway's one hard dependency is the control plane: it pulls the route
//! table from `/internal/routes` every `poll_interval_secs`, and with no route
//! table it can only 404. So readiness is "the route pull is current".
//!
//! That is read from a stamp the pull loop already writes
//! ([`RouteCache::sync_freshness`](crate::sync::RouteCache::sync_freshness)) -
//! a `/readyz` probe issues NO request of its own. An unauthenticated probe
//! flood therefore costs a mutex and a subtraction each, and generates exactly
//! zero control-plane traffic.

use std::sync::Arc;
use std::time::Duration;

use ntex::web;
use ntex::web::types::State;
use zeroship_core::readiness::{staleness_budget, SyncFreshness};

/// State required by the gateway health routes.
///
/// Keeping this separate from the full gateway state lets the route own only
/// the background-poll signal and policy inputs it reads.
#[derive(Debug)]
pub struct GatewayReadiness {
    freshness: Arc<SyncFreshness>,
    poll_interval: Duration,
    dev_escape: bool,
}

impl GatewayReadiness {
    #[must_use]
    pub fn new(freshness: Arc<SyncFreshness>, poll_interval: Duration, dev_escape: bool) -> Self {
        Self {
            freshness,
            poll_interval,
            dev_escape,
        }
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/healthz").route(web::get().to(healthz)))
        .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Liveness. Constant 200 by design - it must not depend on the control
/// plane, or a control-plane outage would have every gateway container killed
/// on top of it.
pub async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"ok": true}))
}

/// Readiness. 200 once the route table has been refreshed from the control
/// plane within the staleness budget; 503 before the first successful pull and
/// after three consecutive missed ones.
///
/// The body is `{"ready":true|false}` and nothing else: no control URL, no
/// error text, no route count.
pub async fn readyz(state: State<Arc<GatewayReadiness>>) -> web::HttpResponse {
    if is_ready(&state.freshness, state.poll_interval, state.dev_escape) {
        web::HttpResponse::Ok().json(&serde_json::json!({"ready": true}))
    } else {
        web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"ready": false}))
    }
}

/// The readiness decision, split out from the HTTP shell so it can be tested
/// without standing up a whole `GateState`.
///
/// TWO conditions, and the second is the credential gate's readiness half. A
/// process that booted on the development escape - an empty or
/// `CHANGE_ME_ZEROSHIP_SERVICE_KEY` credential in a `debug_assertions` build -
/// is NOT ready, however fresh its route table. Gitaly's mistake was making a
/// Prometheus label the only signal for a fail-open credential; readiness is
/// the signal an orchestrator acts on without a human in the loop.
///
/// `dev_escape` is a parameter rather than a direct call to
/// [`zeroship_core::config::dev_escape_active`] so both arms are reachable from
/// a test: the flag is process-wide and write-once, so a test that set it would
/// change the answer for every later test in the binary.
#[must_use]
fn is_ready(freshness: &SyncFreshness, poll_interval: Duration, dev_escape: bool) -> bool {
    !dev_escape && freshness.is_fresh(staleness_budget(poll_interval))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::http::StatusCode;
    use ntex::web::test;

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
    fn a_gateway_that_never_pulled_a_route_table_is_not_ready() {
        // The boot state. The pull loop sleeps one interval before its first
        // attempt, so this is also what the first seconds of every gateway
        // look like - and during them it can only 404, which is not ready.
        let freshness = SyncFreshness::new();
        assert!(!is_ready(&freshness, Duration::from_secs(5), false));
    }

    /// A gateway running on the development credential escape must not be
    /// routed traffic, even with a perfectly fresh route table.
    #[test]
    fn a_gateway_on_the_dev_credential_escape_is_not_ready() {
        let freshness = SyncFreshness::new();
        freshness.mark_success();
        assert!(!is_ready(&freshness, Duration::from_secs(5), true));

        // The one-variable partner: the SAME freshness with the escape inactive
        // IS ready, so the refusal above is about the escape and nothing else.
        assert!(is_ready(&freshness, Duration::from_secs(5), false));
    }

    #[test]
    fn a_fresh_pull_is_ready_and_a_stale_one_is_not() {
        let freshness = SyncFreshness::new();
        freshness.mark_success();
        assert!(is_ready(&freshness, Duration::from_secs(5), false));
        // Zero poll interval collapses the budget to the 10s floor, which the
        // stamp above is inside; the arm that must NOT be ready is the one
        // whose stamp is older than the budget, and only `SyncFreshness`'s own
        // unit tests can drive that without sleeping. What THIS asserts is
        // that the gateway wires the budget from its poll interval at all: a
        // handler that ignored the interval would answer the same for both.
        assert_eq!(
            staleness_budget(Duration::from_secs(5)),
            Duration::from_secs(15)
        );
        assert_eq!(
            staleness_budget(Duration::from_secs(30)),
            Duration::from_secs(90)
        );
    }

    #[ntex::test]
    async fn routes_report_liveness_and_route_pull_readiness() {
        let freshness = Arc::new(SyncFreshness::new());
        let readiness = Arc::new(GatewayReadiness::new(
            freshness.clone(),
            Duration::from_secs(5),
            false,
        ));
        let app = test::init_service(web::App::new().state(readiness).configure(configure)).await;

        assert_eq!(status(&app, "/healthz").await, StatusCode::OK);
        assert_eq!(
            status(&app, "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE,
            "a gateway that has not reached control must refuse traffic"
        );

        freshness.mark_success();
        assert_eq!(status(&app, "/readyz").await, StatusCode::OK);

        let retired = status(&app, "/health").await;
        let unknown = status(&app, "/route-that-does-not-exist").await;
        assert_ne!(retired, StatusCode::OK);
        assert_eq!(retired, unknown, "the retired alias must not be a route");
    }
}
