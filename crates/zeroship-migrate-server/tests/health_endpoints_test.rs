//! `/healthz` + `/readyz` for the creator migration service, through the REAL
//! `zeroship_migrate_server::configure` route table.
//!
//! The valuable arm here is the DOWN one, and it needs no database at all: a
//! policy-store DSN pointing at a closed port must make `/readyz` answer 503
//! while `/healthz` still answers 200. Those two together are what separate a
//! readiness endpoint from a second liveness endpoint - a `/readyz` hardcoded
//! to 200 passes every up-case test ever written.

mod fixture;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_authz::Action;
use zeroship_id::AppId;
use zeroship_migrate_server::auth::{AuthError, Authenticator, VerifiedCaller};
use zeroship_migrate_server::policy::ManagedPolicyConfig;
use zeroship_migrate_server::rate_limit::MutationRateLimiter;
use zeroship_migrate_server::MigrationServiceState;

const TEST_POLICY_SEAL_KEY: &[u8] = b"migrated health probe policy seal key";

/// A DSN whose host:port nothing listens on. The connect fails fast rather
/// than hanging, which is the case the probe's timeout is NOT covering here.
const DEAD_DSN: &str =
    "host=127.0.0.1 port=9199 user=postgres password=zeroship dbname=zeroship connect_timeout=2";

/// Readiness never consults the authenticator, so the stub only has to exist.
#[derive(Debug)]
struct RefusingAuthenticator;

#[derive(Debug)]
struct AllowAllMutationRateLimiter;

#[async_trait(?Send)]
impl MutationRateLimiter for AllowAllMutationRateLimiter {
    async fn consume(
        &self,
        _source_ip: Option<IpAddr>,
    ) -> Result<zeroship_authn::rate_limit::RateLimitDecision, String> {
        Ok(zeroship_authn::rate_limit::RateLimitDecision::Allowed)
    }
}

#[async_trait(?Send)]
impl Authenticator for RefusingAuthenticator {
    async fn verify_action(
        &self,
        _token: &str,
        _app_id: &AppId,
        _required_action: Action,
        _request_ip: Option<IpAddr>,
        _request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        Err(AuthError::Unauthorized)
    }
}

fn state_on(dsn: &str) -> Arc<MigrationServiceState> {
    let tmp: PathBuf =
        std::env::temp_dir().join(format!("zs-migrated-health-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&tmp).expect("mkdir tmp");
    Arc::new(MigrationServiceState::new(
        dsn.to_string(),
        dsn.to_string(),
        tmp,
        Arc::new(RefusingAuthenticator),
        Arc::new(AllowAllMutationRateLimiter),
        false,
        ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
            .expect("test policy config"),
    ))
}

async fn status_of(state: Arc<MigrationServiceState>, path: &str) -> (StatusCode, String) {
    let app = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    let resp = test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, String::from_utf8_lossy(&body).to_string())
}

#[ntex::test]
async fn healthz_is_200_even_with_no_database_at_all() {
    // Liveness must not depend on Postgres: if it did, a database blip would
    // get every migrated container restarted on top of the outage.
    let (status, body) = status_of(state_on(DEAD_DSN), "/healthz").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("\"ok\""), "body: {body}");
}

#[ntex::test]
async fn readyz_is_503_when_postgres_is_unreachable() {
    // The SAME state whose /healthz answered 200 above answers 503 here. One
    // service, one process, two endpoints with different jobs.
    let (status, body) = status_of(state_on(DEAD_DSN), "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert!(body.contains("false"), "body: {body}");

    // No detail leak: the caller is unauthenticated, so the body must not
    // carry the DSN, the host, the port, or the driver's error text.
    for leak in [
        "127.0.0.1",
        "9199",
        "postgres://",
        "password",
        "dbname",
        "refused",
    ] {
        assert!(!body.contains(leak), "readyz body leaked {leak:?}: {body}");
    }
}

#[ntex::test]
async fn readyz_is_200_when_postgres_answers() {
    let postgres = fixture::Postgres::start();
    let (status, body) = status_of(state_on(postgres.url()), "/readyz").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("true"), "body: {body}");
}

// What these do NOT catch:
//   - That `/health` is gone. These drive `configure`, which no longer
//     registers it; the 404 is asserted against the real binary in
//     tests/health_endpoints.sh.
//   - The cache TTL. Each test builds a fresh state, so no test here observes
//     a second probe hitting the cached answer; that is unit-tested in
//     crates/zeroship-core/src/readiness.rs.
//   - A HANGING Postgres (packets dropped rather than refused), which is what
//     the gate's probe timeout exists for. DEAD_DSN gets a connection refused.
