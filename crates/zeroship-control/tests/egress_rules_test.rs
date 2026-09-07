//! Live-PG tests for creator self-service egress rules
//! (`/api/apps/{id}/egress-rules`).
//!
//! These drive the REAL ntex handlers with a real `AuthzGuard` bearer, so the
//! authorization they assert is the Cedar `env:read`/`env:write` on
//! `Resource::App` decision, not a stub. What they pin:
//!
//! - an app OWNER can write, list and delete rules on their own app, in both
//!   destination forms and both verdicts, canonicalised on the way in;
//! - a creator who is not a member of the app is refused on all three, and no
//!   row appears;
//! - what the grammar refuses is refused AND the nearest legal thing to it is
//!   accepted, in the same request path, so a refusal is evidence about the
//!   rule under test rather than about a validator that refuses everything;
//! - the plan's accept ceiling refuses the rule past it and does NOT charge a
//!   reject rule against it;
//! - the first range accept rule answers with the words that say the app has
//!   moved into the class that resolves before refusing.
//!
//! What they do NOT cover: that the rows reach the data plane. That is
//! `plan_catalog.rs::get_versions_projects_app_egress_rules_with_plan_caps`,
//! which drives the same store functions and then reads `get_versions`.

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::plan_catalog::{Plan, PlanCatalog};
use zeroship_control::pricing::{PlanPrice, FX_SCALE};
use zeroship_control::{
    egress_rules, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::net_policy::Verdict;
use zeroship_core::types::{AppNetPolicyLimits, AppRuntimeLimits};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("zship-egress-rules-{label}-{}", Uuid::new_v4().simple()));
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

async fn build_test_state(db_url: &str, label: &str) -> Fixture {
    let (control_pg_client, control_pg_conn) =
        connect(db_url, NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
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
        mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
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

/// Seed a plan whose only interesting field is its ACCEPT-rule ceiling.
async fn seed_plan_with_max_accept_rules(catalog: &PlanCatalog, max_grants: u32) -> Plan {
    let plan = Plan {
        id: zeroship_core::typed_id::new_plan_id(),
        name: format!("egress-rules-cap-{max_grants}"),
        price: PlanPrice {
            base_fee_cents: 0,
            included_units: 100_000,
            fx_pico_cents_per_unit: Some(FX_SCALE as u64),
            spend_limit_default_cents: 0,
        },
        runtime: AppRuntimeLimits {
            cpu_limit_ms: Some(30_000),
            wall_timeout_ms: Some(30_000),
            heap_limit_mb: Some(256),
        },
        net: AppNetPolicyLimits {
            max_sockets: 7,
            egress_ceiling_bytes: 11 * 1024 * 1024,
            max_grants,
        },
        archived: false,
        assignable_by_creator: false,
    };
    catalog.upsert(&plan, Some(plan.archived)).await.expect("upsert plan")
}

async fn count_rules(pg: &Client, app_id: Uuid) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_egress_rules WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("count rules");
    rows[0].get("n")
}

async fn cleanup_app(pg: &Client, app_id: Uuid) {
    let _ = pg
        .execute(
            "DELETE FROM zeroship.app_egress_rules WHERE app_id = $1",
            &[&app_id],
        )
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.app_audit WHERE app_id = $1", &[&app_id])
        .await;
}

fn post(app_id: Uuid, bearer: &str, body: serde_json::Value) -> test::TestRequest {
    test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/egress-rules"))
        .header("authorization", bearer.to_string())
        .set_json(&body)
}

#[compio::test]
async fn owner_can_write_list_and_delete_rules_of_both_forms() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "owner").await;
    let owner = common::authz_fixture::seeded_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("egress-owner-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
            None,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(egress_rules::configure),
    )
    .await;

    // Deny-by-default: a fresh app holds no rules at all.
    let req = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/egress-rules", app_record.id))
        .header("authorization", owner.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    assert_eq!(body["rules"].as_array().unwrap().len(), 0);
    assert_eq!(body["limits"]["used_accept_rules"], 0);
    assert_eq!(body["limits"]["max_accept_rules"], 10, "free tier ceiling");

    // A name rule. The destination is normalised on the way in and the kind is
    // inferred rather than supplied.
    let req = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({
            "verdict": "accept",
            "destination": "SMTP.Example.COM.",
            "port": 587,
            "note": "mail relay"
        }),
    )
    .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("rule json");
    assert_eq!(body["rule"]["destination"], "smtp.example.com");
    assert_eq!(body["rule"]["kind"], "name");
    assert_eq!(body["rule"]["verdict"], "accept");
    assert_eq!(body["rule"]["port"], 587);
    assert!(
        body["notice"].is_null(),
        "a NAME rule never moves the app into the resolving class"
    );
    assert_eq!(
        body["rule"]["created_by"],
        owner.user_id.to_string(),
        "the creator is recorded as the author, not an operator"
    );

    // A range rule, written with host bits set, stored canonical.
    let req = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({
            "verdict": "accept",
            "destination": "198.51.100.7/24",
            "port": 443
        }),
    )
    .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("range json");
    assert_eq!(
        body["rule"]["destination"], "198.51.100.0/24",
        "the range is canonicalised, so one range is one primary key"
    );
    assert_eq!(body["rule"]["kind"], "cidr");

    let req = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/egress-rules", app_record.id))
        .header("authorization", owner.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list json");
    assert_eq!(body["rules"].as_array().unwrap().len(), 2);
    assert_eq!(body["limits"]["used_accept_rules"], 2);
    assert_eq!(body["limits"]["used_reject_rules"], 0);

    // Delete addresses the rule by destination and port. The destination is
    // canonicalised the same way, so the spelling that created the row is not
    // the only spelling that removes it.
    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{}/egress-rules", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({"destination": "SMTP.example.com", "port": 587}))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    let req = test::TestRequest::delete()
        .uri(&format!("/api/apps/{}/egress-rules", app_record.id))
        .header("authorization", owner.bearer())
        .set_json(&serde_json::json!({"destination": "198.51.100.9/24", "port": 443}))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(count_rules(&fx.state.control_pg, app_record.id).await, 0);

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn a_creator_cannot_touch_another_creators_app() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "stranger").await;
    let owner = common::authz_fixture::seeded_principal(&fx.state).await;
    let stranger = common::authz_fixture::seeded_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("egress-stranger-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
            None,
        )
        .await
        .expect("create app");

    // The owner writes one rule, so the stranger's DELETE has a real row to
    // aim at: a 403 on an EMPTY table would not distinguish refusal from
    // "nothing to delete".
    egress_rules::upsert_rule(
        fx.state.control_pg.as_ref(),
        app_record.id,
        &egress_rules::EgressRuleBody {
            verdict: Verdict::Accept,
            destination: "smtp.example.com".to_string(),
            port: 587,
            note: None,
        },
        &owner.user_id.to_string(),
    )
    .await
    .expect("owner seed rule");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(egress_rules::configure),
    )
    .await;

    let read = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/egress-rules", app_record.id))
        .header("authorization", stranger.bearer())
        .to_request();
    assert_eq!(
        test::call_service(&app, read).await.status(),
        StatusCode::FORBIDDEN,
        "a non-member must not read another creator's egress rules"
    );

    let write = post(
        app_record.id,
        &stranger.bearer(),
        serde_json::json!({"verdict": "accept", "destination": "evil.example.com", "port": 443}),
    )
    .to_request();
    assert_eq!(
        test::call_service(&app, write).await.status(),
        StatusCode::FORBIDDEN
    );

    let delete = test::TestRequest::delete()
        .uri(&format!("/api/apps/{}/egress-rules", app_record.id))
        .header("authorization", stranger.bearer())
        .set_json(&serde_json::json!({"destination": "smtp.example.com", "port": 587}))
        .to_request();
    assert_eq!(
        test::call_service(&app, delete).await.status(),
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        count_rules(&fx.state.control_pg, app_record.id).await,
        1,
        "the stranger neither added nor removed a row"
    );

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    stranger.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Every refusal below is PAIRED with the nearest legal thing to it, sent
/// through the same handler in the same test.
///
/// The pairing is the whole point. A validator that refuses everything passes
/// any suite made only of refusals, and one that accepts everything passes any
/// suite made only of acceptances. Each pair differs in ONE thing - the `*`,
/// one bit of prefix length, the verdict - so a green result says that one
/// thing is what decided it.
#[compio::test]
async fn the_grammar_refuses_and_accepts_in_pairs() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "grammar").await;
    let owner = common::authz_fixture::seeded_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("egress-grammar-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
            None,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(egress_rules::configure),
    )
    .await;

    // (verdict, destination, expected status, what the pair isolates)
    let cases: [(&str, &str, StatusCode, &str); 8] = [
        // Wildcards are GONE: not narrowed, not catalog-checked, not
        // representable. The control is the same registrable domain, exact.
        ("accept", "*.example.com", StatusCode::BAD_REQUEST, "wildcard"),
        ("accept", "api.example.com", StatusCode::OK, "exact name"),
        // The accept prefix floor, and its control one bit narrower.
        ("accept", "10.0.0.0/8", StatusCode::BAD_REQUEST, "below the v4 floor"),
        ("accept", "10.0.0.0/16", StatusCode::OK, "at the v4 floor"),
        // The floor is ACCEPT-only. The identical range as a REJECT is legal,
        // and so is the broadest range there is - refusing it would be
        // refusing the strictest rule in the grammar.
        ("reject", "10.0.0.0/8", StatusCode::OK, "reject has no floor"),
        ("reject", "0.0.0.0/0", StatusCode::OK, "reject everything"),
        // An IP literal is refused in favour of the range spelling, because
        // the two are decided in different phases and the reader must be able
        // to tell which. The control is that same address written as a /32.
        ("accept", "203.0.113.4", StatusCode::BAD_REQUEST, "bare IP literal"),
        ("accept", "203.0.113.4/32", StatusCode::OK, "the /32 spelling"),
    ];

    let mut written = 0;
    for (verdict, destination, expected, what) in cases {
        let req = post(
            app_record.id,
            &owner.bearer(),
            serde_json::json!({"verdict": verdict, "destination": destination, "port": 443}),
        )
        .to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        if status == StatusCode::BAD_REQUEST {
            let body: serde_json::Value =
                serde_json::from_slice(&test::read_body(resp).await).expect("error json");
            let detail = body["detail"].as_str().unwrap_or_default().to_string();
            if destination.starts_with("*.") {
                assert!(
                    detail.contains("wildcard"),
                    "the wildcard refusal must SAY wildcards are gone, got {detail:?}"
                );
            }
        } else {
            written += 1;
        }
        assert_eq!(status, expected, "{verdict} {destination} ({what})");
    }
    assert_eq!(
        count_rules(&fx.state.control_pg, app_record.id).await,
        written,
        "exactly the accepted half of each pair wrote a row"
    );

    // A body with no verdict is refused rather than assumed to mean accept.
    let req = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"destination": "novrdct.example.com", "port": 443}),
    )
    .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::BAD_REQUEST,
        "a rule without a verdict is a 400, never a defaulted accept"
    );

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn the_plan_cap_counts_accept_rules_and_not_reject_rules() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "cap").await;
    let catalog = PlanCatalog::new(fx.state.registry.clone());
    let plan = seed_plan_with_max_accept_rules(&catalog, 1).await;
    let owner = common::authz_fixture::seeded_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("egress-cap-{}", Uuid::new_v4().simple()),
            &plan.id,
            &owner.user_id,
            None,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(egress_rules::configure),
    )
    .await;

    let first = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"verdict": "accept", "destination": "one.example.com", "port": 5432}),
    )
    .to_request();
    assert_eq!(test::call_service(&app, first).await.status(), StatusCode::OK);

    let second = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"verdict": "accept", "destination": "two.example.com", "port": 5432}),
    )
    .to_request();
    let resp = test::call_service(&app, second).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "the accept rule past the plan ceiling is refused"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("cap json");
    assert_eq!(body["max_rules"], 1);
    assert_eq!(body["used_rules"], 1);
    assert_eq!(body["verdict"], "accept");

    // The control, differing from the refused call in ONE thing: the verdict.
    // A reject can only narrow what the app reaches, so charging it against
    // the accept ceiling would mean the ceiling caps safety.
    let reject = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"verdict": "reject", "destination": "two.example.com", "port": 5432}),
    )
    .to_request();
    assert_eq!(
        test::call_service(&app, reject).await.status(),
        StatusCode::OK,
        "a reject rule is not charged against the accept ceiling"
    );

    // Re-noting the rule the app already holds is not a new row, so the cap
    // does not block it. Without this the ceiling would freeze an app's rules.
    let renote = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({
            "verdict": "accept",
            "destination": "one.example.com",
            "port": 5432,
            "note": "primary replica"
        }),
    )
    .to_request();
    let resp = test::call_service(&app, renote).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("renote json");
    assert_eq!(body["rule"]["note"], "primary replica");

    assert_eq!(count_rules(&fx.state.control_pg, app_record.id).await, 2);

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(catalog);
    drop(fx);
    common::drain_pg().await;
}

/// The FIRST range accept rule moves an app out of the class that refuses an
/// unlisted destination without looking it up, and into the class that resolves
/// first and refuses afterwards. Nothing else in the product tells a creator
/// that, so the response to that one call has to.
///
/// The three calls differ in one thing each: a name rule first (no notice, so
/// the notice is not simply on every write), then the first range rule (the
/// notice), then a second range rule (no notice, so it is a one-time statement
/// about the app rather than a label on range rules).
#[compio::test]
async fn the_first_range_accept_rule_says_the_app_now_resolves_before_refusing() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "notice").await;
    let owner = common::authz_fixture::seeded_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("egress-notice-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
            None,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(egress_rules::configure),
    )
    .await;

    let name_rule = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"verdict": "accept", "destination": "api.example.com", "port": 443}),
    )
    .to_request();
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(test::call_service(&app, name_rule).await).await)
            .expect("name json");
    assert!(
        body["notice"].is_null(),
        "a names-only app has not moved anywhere"
    );

    // A range REJECT cannot turn a refusal into an admission, so it does not
    // open the gate and must not claim to.
    let range_reject = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"verdict": "reject", "destination": "203.0.113.0/24", "port": 443}),
    )
    .to_request();
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(test::call_service(&app, range_reject).await).await)
            .expect("reject json");
    assert!(
        body["notice"].is_null(),
        "a range REJECT does not move the app into the resolving class"
    );

    let first_range = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"verdict": "accept", "destination": "198.51.100.0/24", "port": 443}),
    )
    .to_request();
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(test::call_service(&app, first_range).await).await)
            .expect("first range json");
    let notice = body["notice"]
        .as_str()
        .expect("the first range ACCEPT rule must carry the notice")
        .to_string();
    assert!(
        notice.contains("resolved first and refused afterwards"),
        "the notice must state the new order of operations: {notice}"
    );
    assert!(
        notice.contains("nameserver"),
        "the notice must say where the lookup goes: {notice}"
    );
    assert!(
        notice.contains("443"),
        "the notice must name the port it applies to: {notice}"
    );

    let second_range = post(
        app_record.id,
        &owner.bearer(),
        serde_json::json!({"verdict": "accept", "destination": "192.0.2.0/24", "port": 443}),
    )
    .to_request();
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(test::call_service(&app, second_range).await).await)
            .expect("second range json");
    assert!(
        body["notice"].is_null(),
        "the notice is a one-time statement about the app, not a label on range rules"
    );

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Rules are an unordered SET under deny-overrides, so an accept range inside
/// a reject range at the same port is dead. The list says so, because a rule
/// that does nothing and a rule that works look identical otherwise.
#[compio::test]
async fn a_dead_accept_reports_the_effective_verdict() {
    let db_url = db_url();
    let fx = build_test_state(&db_url, "effective").await;
    let owner = common::authz_fixture::seeded_principal(&fx.state).await;
    let app_record = fx
        .state
        .registry
        .create_app(
            &format!("egress-eff-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &owner.user_id,
            None,
        )
        .await
        .expect("create app");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(egress_rules::configure),
    )
    .await;

    for body in [
        serde_json::json!({"verdict": "reject", "destination": "198.51.100.0/24", "port": 443}),
        serde_json::json!({"verdict": "accept", "destination": "198.51.100.128/25", "port": 443}),
        // The control: the same shape of accept range at a DIFFERENT port, so
        // the reject cannot reach it. Without this row the assertion below
        // would pass against an implementation that reports reject for every
        // accept range.
        serde_json::json!({"verdict": "accept", "destination": "198.51.100.128/25", "port": 8443}),
    ] {
        let req = post(app_record.id, &owner.bearer(), body).to_request();
        assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);
    }

    let req = test::TestRequest::get()
        .uri(&format!("/api/apps/{}/egress-rules", app_record.id))
        .header("authorization", owner.bearer())
        .to_request();
    let body: serde_json::Value =
        serde_json::from_slice(&test::read_body(test::call_service(&app, req).await).await)
            .expect("list json");
    let rules = body["rules"].as_array().expect("rules array");

    let dead = rules
        .iter()
        .find(|r| r["destination"] == "198.51.100.128/25" && r["port"] == 443)
        .expect("the covered accept rule is listed");
    assert_eq!(dead["verdict"], "accept", "the row still says what was written");
    assert_eq!(
        dead["effective_verdict"], "reject",
        "and the list says what it actually does"
    );

    let live = rules
        .iter()
        .find(|r| r["destination"] == "198.51.100.128/25" && r["port"] == 8443)
        .expect("the other-port accept rule is listed");
    assert_eq!(
        live["effective_verdict"], "accept",
        "a reject at another port does not reach it"
    );

    cleanup_app(&fx.state.control_pg, app_record.id).await;
    owner.cleanup(&fx.state).await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}
