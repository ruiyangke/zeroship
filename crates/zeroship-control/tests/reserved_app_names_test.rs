//! Live-PG tests that `POST /api/apps` REFUSES a name the platform edge
//! already routes.
//!
//! # Why this exists next to the unit tests
//!
//! `crates/zeroship-control/src/reserved_names.rs` unit-tests the PREDICATE:
//! `is_reserved_app_name("console")` is true. That is a different claim from
//! the one the platform depends on, which is that the real create route
//! refuses the name. A call site that stopped being reached - the `if` moved
//! below the INSERT, an earlier `return`, a second create path added beside
//! `Registry::create_app` - leaves every predicate test green and the platform
//! origin takeable. These drive the REAL ntex handler with a real `AuthzGuard`
//! bearer against a real database, so what they assert is the path.
//!
//! # What the four names actually cost, and why they differ
//!
//! An app's name IS its hostname label, and `deploy/ops/Caddyfile` claims four
//! labels under the same domain the creator-app wildcard serves. Two shapes:
//!
//! - `console` and `api` are proxied TO THE GATEWAY, which resolves them as
//!   ordinary creator apps. A creator holding one of those names serves their
//!   own content from a platform ORIGIN. For `console` that origin is named in
//!   `[auth].frame_ancestor_origins` (`deploy/ops/zeroship.toml`), so it is the
//!   one origin the auth service's `frame-ancestors` CSP allows to frame the
//!   real `/login` page - the clickjacking the allowlist exists to prevent.
//! - `auth` and `control` are proxied to their own services and NEVER reach the
//!   gateway. An app registered under those names is unroutable rather than
//!   dangerous: the creator holds a name whose requests they will never see.
//!   (Behind an edge that is not this Caddyfile, the wildcard resolves them and
//!   the first shape applies to these too - which is the other half of why the
//!   registry, not the edge, is where this is refused.)
//!
//! All four are refused; the two reasons are why the set is not obviously a
//! tidiness list.
//!
//! # What these tests do NOT cover
//!
//! Only CREATION. `Registry::create_app` is the sole point a name is claimed -
//! there is no rename or name-update route (no `UPDATE zeroship.apps SET name`
//! exists anywhere in `crates/control/src`, and `/api/apps/{id}` carries GET
//! and DELETE only, `crates/zeroship-control/src/main.rs`). The only other production
//! caller of `create_app` is `crates/zeroship-control/src/bin/dev_provision.rs`, which
//! goes through the same function; `registry_refuses_a_reserved_name_directly`
//! below is that vector's coverage. Direct `INSERT INTO zeroship.apps` outside
//! the registry exists only in test fixtures. Nothing here proves the GATEWAY
//! would refuse to serve such an app if a row reached the table by another
//! route - that arm is unreachable by construction today, not tested.

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::registry::RegistryError;
use zeroship_control::reserved_names::RESERVED_APP_NAMES;
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "zship-reserved-names-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_test_state(label: &str) -> Fixture {
    let db_url = common::require_control_db();
    let (control_pg_client, control_pg_conn) =
        connect(&db_url, NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(&db_url).await.expect("registry");
    zeroship_control::plan_catalog::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        workflow_blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
        gateway_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg: Arc::new(control_pg_client),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        auth_provider: zeroship_control::platform_auth_provider(
            "https://auth.zeroship.test/oauth2",
            Some(common::platform_jwks_url()),
        ),
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

/// The production route table for `POST /api/apps`, mounted exactly as
/// `crates/zeroship-control/src/main.rs` mounts it.
macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(
            web::App::new().state($fx.state.clone()).service(
                web::resource("/api/apps")
                    .route(web::post().to(api::create_app))
                    .route(web::get().to(api::list_apps)),
            ),
        )
        .await
    }};
}

/// `POST /api/apps` with `name`, yielding `(status, decoded body)`.
///
/// A macro, not a function: `test::init_service` hands back an opaque
/// `Pipeline<impl Service<..>>` that cannot be named in a signature.
macro_rules! create {
    ($control:expr, $bearer:expr, $name:expr) => {{
        let req = test::TestRequest::post()
            .uri("/api/apps")
            .header("authorization", $bearer)
            .set_json(&serde_json::json!({
                "name": $name,
                "plan_id": zeroship_control::plan_catalog::free_plan_id(),
            }))
            .to_request();
        let resp = test::call_service(&$control, req).await;
        let status = resp.status();
        let bytes = test::read_body(resp).await;
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::json!({ "error": String::from_utf8_lossy(&bytes).to_string() })
        });
        (status, body)
    }};
}

async fn app_row_count(fx: &Fixture, name: &str) -> i64 {
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.apps \
             WHERE lower(name) = lower($1::text)",
            &[&name],
        )
        .await
        .expect("count apps by name");
    rows[0].get("n")
}

async fn cleanup_app(fx: &Fixture, id: Uuid) {
    // `app_members.app_id` is a `uuid` column - bind the `Uuid` directly, the
    // way `Registry::create_app` does.
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.app_members WHERE app_id = $1", &[&id])
        .await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&id])
        .await;
}

/// THE PATH TEST, with its control in the same fixture and the same call.
///
/// Every reserved name is refused AND an ordinary name is accepted, because a
/// refusal on its own cannot tell "refuses reserved names" apart from "refuses
/// everything" - a gate that denies universally reads as correct from the
/// refusal side alone.
#[compio::test]
async fn every_reserved_name_is_refused_and_an_ordinary_name_is_accepted() {
    let fx = build_test_state("refuse").await;
    let creator = common::authz_fixture::seeded_principal(&fx.state).await;
    let control = init_control!(fx);

    assert_eq!(
        RESERVED_APP_NAMES,
        ["api", "auth", "console", "control"],
        "the set under test is the one the Caddyfile defines; \
         reserved_names::reserved_set_matches_the_edge is what keeps it that way"
    );

    for name in RESERVED_APP_NAMES {
        let (status, body) = create!(control, creator.bearer(), name);
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "POST /api/apps name={name} must be refused; got {body}"
        );
        let message = body["error"].as_str().unwrap_or_default();
        assert!(
            message.contains(name) && message.contains("reserved"),
            "the refusal for '{name}' must name the label and say it is reserved, \
             so a creator does not read it as the charset rule: {message}"
        );
        assert_eq!(
            app_row_count(&fx, name).await,
            0,
            "'{name}' must leave no row behind"
        );
    }

    // THE CONTROL, one variable away: same call, same fixture, same bearer, a
    // name the edge does not claim.
    let ordinary = format!("reserved-ctl-{}", Uuid::new_v4().simple());
    let (status, body) = create!(control, creator.bearer(), &ordinary);
    assert_eq!(
        status,
        StatusCode::CREATED,
        "an unreserved name must still be creatable through the same route: {body}"
    );
    let created_id: Uuid = body["id"]
        .as_str()
        .expect("created app carries an id")
        .parse()
        .expect("app id parses");

    cleanup_app(&fx, created_id).await;
    creator.cleanup(&fx.state).await;
    drop(control);
    drop(fx);
    common::drain_pg().await;
}

/// Case-insensitivity THROUGH THE ROUTE. Hostnames are case-insensitive
/// (RFC 4343) and `create_app`'s charset rule admits uppercase, so `CONSOLE`
/// and `console` name the same origin to every browser and proxy on the path.
///
/// The control here is an uppercase name that is NOT reserved: it separates
/// "the route folds case before matching the reserved set" from "the route
/// refuses uppercase", which a refusal-only test cannot distinguish.
#[compio::test]
async fn a_reserved_name_is_refused_in_every_letter_case() {
    let fx = build_test_state("case").await;
    let creator = common::authz_fixture::seeded_principal(&fx.state).await;
    let control = init_control!(fx);

    for name in ["CONSOLE", "Console", "cOnSoLe", "AUTH", "Api", "cOntrol"] {
        let (status, body) = create!(control, creator.bearer(), name);
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "'{name}' resolves to the same host as its lowercase form and must \
             be refused too; got {body}"
        );
        assert_eq!(app_row_count(&fx, name).await, 0);
    }

    let ordinary = format!("ReservedCase{}", Uuid::new_v4().simple());
    let (status, body) = create!(control, creator.bearer(), &ordinary);
    assert_eq!(
        status,
        StatusCode::CREATED,
        "uppercase itself is not what is refused - an uppercase unreserved name \
         must be created: {body}"
    );
    let created_id: Uuid = body["id"]
        .as_str()
        .expect("created app carries an id")
        .parse()
        .expect("app id parses");

    cleanup_app(&fx, created_id).await;
    creator.cleanup(&fx.state).await;
    drop(control);
    drop(fx);
    common::drain_pg().await;
}

/// A reserved name and a MALFORMED name are different outcomes with different
/// fixes, and a creator must be able to tell them apart from the response
/// alone: 409 says a well-formed name is unavailable, 400 says the name is not
/// a legal name at all. If both collapsed to one status the refusal would send
/// the creator to fix the wrong thing.
#[compio::test]
async fn a_reserved_name_and_a_malformed_name_answer_differently() {
    let fx = build_test_state("status").await;
    let creator = common::authz_fixture::seeded_principal(&fx.state).await;
    let control = init_control!(fx);

    let (reserved_status, reserved_body) = create!(control, creator.bearer(), "console");
    let (malformed_status, malformed_body) =
        create!(control, creator.bearer(), "not a legal name!");

    creator.cleanup(&fx.state).await;
    drop(control);
    drop(fx);
    common::drain_pg().await;

    assert_eq!(
        reserved_status,
        StatusCode::CONFLICT,
        "a reserved name is well-formed and unavailable: {reserved_body}"
    );
    assert_eq!(
        malformed_status,
        StatusCode::BAD_REQUEST,
        "a charset violation is malformed input: {malformed_body}"
    );
    assert_ne!(
        reserved_body["error"], malformed_body["error"],
        "the two refusals must not read the same either"
    );
}

/// A name in the APP-ID namespace is refused by the real create route.
///
/// The other tests here are about hostname labels the edge claims. This one is
/// about a different collision with the same remedy: `create_app`'s charset
/// rule admits `_`, so `app_0123456789ABCDEFGHIJKL` is a legal NAME and also a
/// legal `zeroship_core::app_id::AppId` - one string that is both, which is
/// what makes any name-or-id discriminator unwritable. The CLI's `--app` used
/// to be exactly such a discriminator.
///
/// `registry::name_validation_tests` rules on the predicate without a database.
/// This is the PATH claim, for the same reason stated in this file's header:
/// a predicate stays green when its call site stops being reached.
#[compio::test]
async fn a_name_in_the_app_id_namespace_is_refused_by_the_route() {
    let fx = build_test_state("appid").await;
    let creator = common::authz_fixture::seeded_principal(&fx.state).await;
    let control = init_control!(fx);

    let id_shaped = "app_0123456789ABCDEFGHIJKL";
    assert!(
        zeroship_core::app_id::AppId::parse(id_shaped).is_ok(),
        "the premise: this name really is a well-formed app id"
    );

    let (status, body) = create!(control, creator.bearer(), id_shaped);
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a name that is also an app id must be refused; got {body}"
    );
    let message = body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains(id_shaped) && message.contains("reserved"),
        "the refusal must name the label and say it is reserved: {message}"
    );
    assert_eq!(app_row_count(&fx, id_shaped).await, 0);

    // THE CONTROL, one variable away: the same route, the same charset,
    // underscores and the word "app" included - what is refused is the id
    // PREFIX, not the underscore and not the word.
    let ordinary = format!("my_app_{}", Uuid::new_v4().simple());
    let (status, body) = create!(control, creator.bearer(), &ordinary);
    assert_eq!(
        status,
        StatusCode::CREATED,
        "an underscore name outside the id namespace must still be created: {body}"
    );
    let created_id: Uuid = body["id"]
        .as_str()
        .expect("created app carries an id")
        .parse()
        .expect("app id parses");

    cleanup_app(&fx, created_id).await;
    creator.cleanup(&fx.state).await;
    drop(control);
    drop(fx);
    common::drain_pg().await;
}

/// The registry is the choke point, and `crates/zeroship-control/src/bin/dev_provision.rs`
/// reaches it WITHOUT the HTTP handler. Driving `Registry::create_app` directly
/// covers that vector and pins the typed error the HTTP layer maps.
#[compio::test]
async fn registry_refuses_a_reserved_name_directly() {
    let fx = build_test_state("registry").await;
    let creator = common::authz_fixture::seeded_principal(&fx.state).await;
    let plan = zeroship_control::plan_catalog::free_plan_id();

    let err = fx
        .state
        .registry
        .create_app("api", &plan, &creator.user_id)
        .await
        .expect_err("the registry itself must refuse a reserved name");
    assert!(
        matches!(err, RegistryError::ReservedName(_)),
        "the error must be typed so every caller can map it; got {err:?}"
    );

    // Same control as the HTTP tests: the direct call still creates an
    // unreserved name, so the refusal above is about the name.
    let ordinary = format!("reserved-reg-{}", Uuid::new_v4().simple());
    let record = fx
        .state
        .registry
        .create_app(&ordinary, &plan, &creator.user_id)
        .await
        .expect("an unreserved name is created by the same call");

    cleanup_app(&fx, record.id).await;
    creator.cleanup(&fx.state).await;
    drop(fx);
    common::drain_pg().await;
}
