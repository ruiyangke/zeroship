//! `/healthz` + `/readyz` for the creator migration service, through the REAL
//! `zeroship_migrate_server::configure` route table.
//!
//! The valuable arm here is the DOWN one, and it needs no database at all: a
//! policy-store DSN pointing at a closed port must make `/readyz` answer 503
//! while `/healthz` still answers 200. Those two together are what separate a
//! readiness endpoint from a second liveness endpoint - a `/readyz` hardcoded
//! to 200 passes every up-case test ever written.
//!
//! The same route table decides which mutating routes this service serves at
//! all, so the case that pins that shape lives here too.


use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_authz::Action;
use zeroship_id::DatabaseId;
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
        _database_id: &DatabaseId,
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
    let postgres = crate::fixture::Postgres::start();
    let (status, body) = status_of(state_on(postgres.url()), "/readyz").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("true"), "body: {body}");
}

#[ntex::test]
async fn retired_health_alias_is_not_registered() {
    let state = state_on(DEAD_DSN);
    let retired = status_of(state.clone(), "/health").await.0;
    let unknown = status_of(state, "/route-that-does-not-exist").await.0;
    assert_ne!(retired, StatusCode::OK);
    assert_eq!(retired, unknown, "the retired alias must not be a route");
}

async fn post_status(
    state: Arc<MigrationServiceState>,
    path: &str,
    body: &serde_json::Value,
) -> (StatusCode, String) {
    let app = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::api::configure),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(path)
            .set_json(body)
            .to_request(),
    )
    .await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, String::from_utf8_lossy(&body).to_string())
}

/// The only schema this service writes is a creator database's, through the
/// apply route addressed by that database. A platform-owned schema arrives
/// through the platform migrations, so no route here installs one.
///
/// The request is well formed for the install route this service does not
/// carry, so a table that still served it would answer from its handler rather
/// than from a body extractor. The apply route is the positive control: the
/// same table, the same method and no credential reach its handler and are
/// refused as unauthenticated, so a table that routed nothing could not pass.
#[ntex::test]
async fn no_platform_schema_install_route_is_served_beside_the_creator_apply() {
    let state = state_on(DEAD_DSN);
    let install = serde_json::json!({
        "bundle": "probe",
        "schema": "customer",
        "dialect": "postgres",
        "version": 1,
        "fingerprint": "a".repeat(64),
        "stamp": {"table": "__zeroship_probe_version", "row_id": "probe"},
        "policy": "policy_version = 1\n",
        "versions": [
            {"version": 1, "sql": "CREATE TABLE \"customer\".\"t\" (id text PRIMARY KEY)"}
        ],
    });

    let (install_status, install_body) =
        post_status(state.clone(), "/v1/schema-bundles/apply", &install).await;
    let (unknown_status, _) =
        post_status(state.clone(), "/v1/route-that-does-not-exist", &install).await;
    assert_eq!(
        install_status,
        StatusCode::NOT_FOUND,
        "a platform schema install must not be a route: {install_body}"
    );
    assert_eq!(install_status, unknown_status);

    let apply = serde_json::json!({
        "kind": "ir",
        "descriptor_sha256": "1".repeat(64),
        "documents": [{"filename": "0001_notes.ir.json", "body": {}}],
    });
    let (apply_status, apply_body) = post_status(
        state,
        &format!(
            "/v1/databases/{}/migrations/apply",
            DatabaseId::mint().as_str()
        ),
        &apply,
    )
    .await;
    assert_eq!(
        apply_status,
        StatusCode::UNAUTHORIZED,
        "the creator apply must reach its handler: {apply_body}"
    );
    assert!(
        apply_body.contains("unauthenticated"),
        "the refusal must be the apply handler's own: {apply_body}"
    );
}

// What these do NOT catch:
//   - The cache TTL. Each test builds a fresh state, so no test here observes
//     a second probe hitting the cached answer; that is unit-tested in
//     crates/zeroship-core/src/readiness.rs.
//   - A HANGING Postgres (packets dropped rather than refused), which is what
//     the gate's probe timeout exists for. DEAD_DSN gets a connection refused.
