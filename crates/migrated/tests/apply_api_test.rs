use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use compio_postgres::{Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Scope, Statement};
use zeroship_authn::PatIssuer;
use zeroship_migrated::auth::{
    AuthError, Authenticator, ControlPlaneAuthenticator, VerifiedCaller,
};
use zeroship_migrated::policy::{ManagedPolicyConfig, MIGRATE_POLICY_FILENAME};
use zeroship_migrated::MigrationServiceState;

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_control_test";
const TEST_POLICY_SEAL_KEY: &[u8] = b"migrated integration policy seal key";

fn dsn() -> String {
    std::env::var("MIGRATED_TEST_DB")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-migrated-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

#[derive(Debug, Clone)]
struct TestCaller {
    principal_id: Uuid,
    actions: HashSet<Action>,
    owned_apps: HashSet<Uuid>,
}

#[derive(Debug, Default)]
struct StaticAuthenticator {
    callers: Mutex<HashMap<String, TestCaller>>,
}

impl StaticAuthenticator {
    fn new() -> Self {
        Self::default()
    }

    fn insert(
        &self,
        token: impl Into<String>,
        principal_id: Uuid,
        scopes: impl IntoIterator<Item = Scope>,
        owned_apps: impl IntoIterator<Item = Uuid>,
    ) {
        self.insert_actions(
            token,
            principal_id,
            scopes.into_iter().map(|scope| scope.action()),
            owned_apps,
        );
    }

    fn insert_actions(
        &self,
        token: impl Into<String>,
        principal_id: Uuid,
        actions: impl IntoIterator<Item = Action>,
        owned_apps: impl IntoIterator<Item = Uuid>,
    ) {
        let caller = TestCaller {
            principal_id,
            actions: actions.into_iter().collect(),
            owned_apps: owned_apps.into_iter().collect(),
        };
        self.callers
            .lock()
            .expect("static auth lock")
            .insert(token.into(), caller);
    }
}

#[async_trait(?Send)]
impl Authenticator for StaticAuthenticator {
    async fn verify_action(
        &self,
        token: &str,
        app_id: Uuid,
        required_action: Action,
    ) -> Result<VerifiedCaller, AuthError> {
        let callers = self.callers.lock().expect("static auth lock");
        let caller = callers.get(token).ok_or(AuthError::Unauthorized)?;
        if !caller.actions.contains(&required_action) || !caller.owned_apps.contains(&app_id) {
            return Err(AuthError::Forbidden);
        }
        Ok(VerifiedCaller {
            principal_id: caller.principal_id,
            token_id: None,
        })
    }
}

async fn admin_conn() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), NoTls)
        .await
        .expect("connect to zeroship_control_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    ensure_migrated_service_tables(&client).await;
    client
}

async fn ensure_migrated_service_tables(conn: &Client) {
    conn.batch_execute("SELECT pg_advisory_lock(7330067);")
        .await
        .expect("lock migrated service table setup");
    conn.batch_execute(include_str!(
        "../../../db/migrations/V0067__migrated_service.sql"
    ))
    .await
    .expect("ensure migrated service tables");
    conn.batch_execute("SELECT pg_advisory_unlock(7330067);")
        .await
        .expect("unlock migrated service table setup");
}

async fn cleanup_app(conn: &Client, app_id: &Uuid) {
    let schema = app_id.to_string();
    let role = zeroship_migrate::migrator_role_name(&schema).unwrap();
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE;",
            q(&format!("{schema}_migrations")),
            q(&schema),
        ))
        .await;
    let _ = conn
        .batch_execute(&format!(
            "DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='{r}') THEN \
                EXECUTE 'REASSIGN OWNED BY {rq} TO current_user'; \
                EXECUTE 'DROP OWNED BY {rq}'; \
                EXECUTE 'DROP ROLE {rq}'; \
             END IF; END $$;",
            r = role.replace('\'', "''"),
            rq = q(&role),
        ))
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.authz_decisions WHERE resource_id = $1",
            &[&app_id.to_string()],
        )
        .await;
    let _ = conn
        .execute("DELETE FROM zeroship.app_members WHERE app_id = $1", &[app_id])
        .await;
    let _ = conn
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[app_id])
        .await;
}

async fn cleanup_user(conn: &Client, user_id: &Uuid) {
    let _ = conn
        .execute(
            "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
            &[user_id],
        )
        .await;
    let _ = conn
        .execute("DELETE FROM zeroship.permission_tokens WHERE owner_id = $1", &[user_id])
        .await;
    let _ = conn
        .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[user_id])
        .await;
    let _ = conn
        .execute("DELETE FROM zeroship.app_members WHERE user_id = $1", &[user_id])
        .await;
    let _ = conn
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[user_id])
        .await;
}

async fn seed_app(conn: &Client, app_id: Uuid, owner_id: Uuid) {
    cleanup_app(conn, &app_id).await;
    cleanup_user(conn, &owner_id).await;
    let plan_id = "pln_migrated_phase1";
    conn.execute(
        "INSERT INTO zeroship.plans \
            (id, name, base_fee_cents, included_units, spend_limit_default_cents, runtime_limits_json) \
         VALUES ($1, 'Migrated Phase 1 Test', 0, 0, 0, '{}'::jsonb) \
         ON CONFLICT (id) DO NOTHING",
        &[&plan_id],
    )
    .await
    .expect("seed plan");
    let email = format!("migrated-{owner_id}@zeroship.test");
    conn.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, 'Migrated Test User', NOW())",
        &[&owner_id, &email],
    )
    .await
    .expect("seed user");
    let name = format!("migrated-{}", app_id.simple());
    conn.execute(
        "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash) \
         VALUES ($1, $2, $3, 'test-api-key', 'test-api-key-hash')",
        &[&app_id, &name, &plan_id],
    )
    .await
    .expect("seed app");
    conn.execute(
        "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
        &[&app_id, &owner_id],
    )
    .await
    .expect("seed app owner");
}

async fn seed_user(conn: &Client, user_id: Uuid, label: &str) {
    cleanup_user(conn, &user_id).await;
    let email = format!("migrated-{label}-{user_id}@zeroship.test");
    conn.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, $3, NOW())",
        &[&user_id, &email, &format!("Migrated {label}")],
    )
    .await
    .expect("seed user");
}

fn state_for(authenticator: Arc<dyn Authenticator>) -> (Arc<MigrationServiceState>, PathBuf) {
    state_for_with_dsns(authenticator, dsn(), dsn())
}

fn state_for_with_dsns(
    authenticator: Arc<dyn Authenticator>,
    provision_dsn: String,
    policy_store_dsn: String,
) -> (Arc<MigrationServiceState>, PathBuf) {
    state_for_with_policy_config(
        authenticator,
        provision_dsn,
        policy_store_dsn,
        ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
            .expect("test policy config"),
    )
}

fn state_for_with_ceiling_version(
    authenticator: Arc<dyn Authenticator>,
    ceiling_version: u64,
) -> (Arc<MigrationServiceState>, PathBuf) {
    state_for_with_policy_config(
        authenticator,
        dsn(),
        dsn(),
        ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), ceiling_version)
            .expect("test policy config"),
    )
}

fn state_for_with_policy_config(
    authenticator: Arc<dyn Authenticator>,
    provision_dsn: String,
    policy_store_dsn: String,
    policy_config: ManagedPolicyConfig,
) -> (Arc<MigrationServiceState>, PathBuf) {
    let tmp = tmpdir("tmp");
    (
        Arc::new(MigrationServiceState::new(
            provision_dsn,
            policy_store_dsn,
            tmp.clone(),
            authenticator,
            policy_config,
        )),
        tmp,
    )
}

async fn seed_platform_admin(conn: &Client, user_id: Uuid) {
    seed_user(conn, user_id, "operator").await;
    conn.execute(
        "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
         VALUES ($1, 'admin', $1)",
        &[&user_id],
    )
    .await
    .expect("seed platform admin role");
}

fn create_notes_request() -> Value {
    json!({
        "kind": "ir",
        "documents": [{
            "filename": "0001_create_notes.ir.json",
            "body": {
                "ir_version": 1,
                "name": "create_notes",
                "ops": [{
                    "op": "createTable",
                    "name": "notes",
                    "columns": [
                        {"name": "title", "type": "text", "nullable": false},
                        {"name": "body", "type": "text"}
                    ]
                }]
            }
        }]
    })
}

fn denied_vendor_request() -> Value {
    json!({
        "kind": "ir",
        "documents": [{
            "filename": "0001_create_role.ir.json",
            "body": {
                "ir_version": 1,
                "name": "create_role",
                "ops": [{
                    "op": "createRole",
                    "name": "zs_migrated_forbidden"
                }]
            }
        }]
    })
}

fn drop_notes_request() -> Value {
    json!({
        "kind": "ir",
        "documents": [{
            "filename": "0002_drop_notes.ir.json",
            "body": {
                "ir_version": 1,
                "name": "drop_notes",
                "ops": [{
                    "op": "dropTable",
                    "table": "notes"
                }]
            }
        }]
    })
}

fn with_policy(mut request: Value, body: &str) -> Value {
    request["policy"] = json!({
        "filename": MIGRATE_POLICY_FILENAME,
        "body": body
    });
    request
}

fn tighter_policy() -> &'static str {
    "[operational]\nlock_timeout_ms = 1000\n\n[data_security]\ndestructive_ops = \"forbid\"\n"
}

fn require_approval_policy() -> &'static str {
    "[data_security]\ndestructive_ops = \"require_approval\"\n"
}

fn second_tighter_policy() -> &'static str {
    "[operational]\nlock_timeout_ms = 500\n\n[data_security]\ndestructive_ops = \"forbid\"\n"
}

fn escalating_policy() -> &'static str {
    "[capabilities]\nraw_sql = true\n"
}

fn malformed_policy() -> &'static str {
    "[capabilities]\nraw_sq = true\n"
}

fn policy_for(name: &str, actions: Vec<Action>) -> Policy {
    Policy {
        name: name.to_string(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions,
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

async fn issue_pat_for_policy(
    conn: &Client,
    issuer: &PatIssuer,
    owner_id: Uuid,
    policy: Policy,
    name: &str,
) -> (String, Uuid) {
    let policy_json = policy.to_json_value();
    let hash = policy_hash(&policy_json);
    let token_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::days(1);
    let token = issuer
        .issue(token_id, owner_id, hash.clone(), expires_at)
        .expect("issue PAT");
    conn.execute(
        "INSERT INTO zeroship.permission_tokens \
            (id, owner_id, kind, name, policies, policy_hash, expires_at) \
         VALUES ($1, $2, 'pat', $3, $4, $5, $6)",
        &[&token_id, &owner_id, &name, &policy_json, &hash, &expires_at],
    )
    .await
    .expect("insert permission token");
    (token, token_id)
}

fn real_authenticator(conn: Client, issuer: Arc<PatIssuer>) -> ControlPlaneAuthenticator {
    let auth_provider = Arc::new(zeroship_core::auth_provider::AuthProvider::Platform(
        zeroship_core::auth_provider::PlatformProvider::new(
            zeroship_core::auth_provider::PlatformConfig::new(
                "http://127.0.0.1:1/oauth2",
                Some("http://127.0.0.1:1/.well-known/jwks.json".to_string()),
            )
            .expect("test platform auth config"),
        ),
    ));
    let control_pg = Arc::new(conn);
    let bearer_verifier = zeroship_authn::BearerVerifier::new(
        issuer,
        Arc::clone(&control_pg),
        auth_provider,
        zeroship_core::auth::default_trusted_oauth_clients(),
        "control.zeroship.ai".to_string(),
    );
    ControlPlaneAuthenticator::new(
        control_pg,
        zeroship_authz::load_platform_policies().expect("policies parse"),
        bearer_verifier,
    )
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query information_schema.tables");
    !rows.is_empty()
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

async fn journaled_count(conn: &Client, app_id: &Uuid) -> i64 {
    let meta = format!("{}_migrations", app_id);
    let q = format!("\"{}\".schema_migrations", meta.replace('"', "\"\""));
    let lit = q.replace('\'', "''");
    let present = conn
        .query(&format!("SELECT to_regclass('{lit}') IS NOT NULL AS p"), &[])
        .await
        .expect("regclass probe");
    if !present[0].get::<_, bool>("p") {
        return 0;
    }
    let rows = conn
        .query(
            &format!("SELECT count(*)::int8 AS n FROM {q} WHERE phase = 'completed'"),
            &[],
        )
        .await
        .expect("count journal");
    rows[0].get("n")
}

async fn stored_policy_count(conn: &Client, app_id: &Uuid) -> i64 {
    let rows = conn
        .query(
            "SELECT count(*)::int8 AS n \
               FROM zeroship.migrated_app_policies \
              WHERE app_id = $1",
            &[app_id],
        )
        .await
        .expect("count stored policies");
    rows[0].get("n")
}

async fn audit_rows(conn: &Client, app_id: &Uuid) -> Vec<(String, String, Uuid, Value)> {
    let rows = conn
        .query(
            "SELECT action, outcome, principal_id, detail \
               FROM zeroship.migrated_migration_audit \
              WHERE app_id = $1 \
              ORDER BY created_at ASC, audit_id ASC",
            &[app_id],
        )
        .await
        .expect("query migration audit");
    rows.into_iter()
        .map(|row| {
            (
                row.get("action"),
                row.get("outcome"),
                row.get("principal_id"),
                row.get("detail"),
            )
        })
        .collect()
}

async fn migration_status(
    conn: &Client,
    app_id: &Uuid,
    migration_id: &Uuid,
) -> Option<(String, Option<String>)> {
    let rows = conn
        .query(
            "SELECT status, last_error \
               FROM zeroship.migrated_migrations \
              WHERE app_id = $1 AND migration_id = $2",
            &[app_id, migration_id],
        )
        .await
        .expect("query migration status");
    rows.first()
        .map(|row| (row.get("status"), row.get("last_error")))
}

async fn first_audit_id(conn: &Client, app_id: &Uuid) -> Uuid {
    let rows = conn
        .query(
            "SELECT audit_id \
               FROM zeroship.migrated_migration_audit \
              WHERE app_id = $1 \
              ORDER BY created_at ASC, audit_id ASC \
              LIMIT 1",
            &[app_id],
        )
        .await
        .expect("query first audit id");
    rows[0].get("audit_id")
}

#[ntex::test]
async fn policy_api_submits_gets_and_lists_versioned_policy_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer good-token")
        .set_payload(tighter_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["version"], 1);
    assert_eq!(body["ceiling_version"], 1);
    assert_eq!(body["raw_toml"], tighter_policy());
    assert_eq!(
        body["effective_profile"]["data_security"]["destructive_ops"],
        "forbid"
    );

    let req = test::TestRequest::get()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer good-token")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["version"], 1);
    assert_eq!(body["raw_toml"], tighter_policy());

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer good-token")
        .set_payload(second_tighter_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["version"], 2);
    assert_eq!(body["raw_toml"], second_tighter_policy());

    let req = test::TestRequest::get()
        .uri(&format!("/v1/apps/{app_id}/policy?version=1"))
        .header("authorization", "Bearer good-token")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["version"], 1);
    assert_eq!(body["raw_toml"], tighter_policy());

    let req = test::TestRequest::get()
        .uri(&format!("/v1/apps/{app_id}/policy/versions"))
        .header("authorization", "Bearer good-token")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let versions = body["versions"].as_array().expect("versions array");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["version"], 1);
    assert_eq!(versions[1]["version"], 2);

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn policy_api_rejects_escalating_draft_at_submit_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer good-token")
        .set_payload(escalating_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_policy_invalid");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("capabilities.raw_sql"),
        "escalation should identify the knob, got: {body}"
    );
    assert_eq!(stored_policy_count(&conn, &app_id).await, 0);

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn policy_api_rejects_malformed_toml_at_submit_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer good-token")
        .set_payload(malformed_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_policy_invalid");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown field"),
        "malformed policy should identify parse failure, got: {body}"
    );
    assert_eq!(stored_policy_count(&conn, &app_id).await, 0);

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn policy_api_rejects_cross_app_get_and_put() {
    let app_id = Uuid::now_v7();
    let other_app = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("app-a-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/v1/apps/{other_app}/policy"))
        .header("authorization", "Bearer app-a-token")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{other_app}/policy"))
        .header("authorization", "Bearer app-a-token")
        .set_payload(tighter_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn apply_api_accepts_apps_migrate_owner_and_applies_ir_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert!(
        !body["applied"].as_array().unwrap().is_empty(),
        "IR apply response should include applied journal versions: {body}"
    );
    assert!(
        table_exists(&conn, &app_id.to_string(), "notes").await,
        "notes table must exist in the per-app schema"
    );
    assert!(
        !table_exists(&conn, "public", "notes").await,
        "Confined apply must not create the table in public"
    );
    assert!(
        journaled_count(&conn, &app_id).await >= 1,
        "IR apply must be journaled"
    );
    let audit = audit_rows(&conn, &app_id).await;
    assert!(
        audit
            .iter()
            .any(|(action, outcome, principal, _)| action == "submit"
                && outcome == "accepted"
                && principal == &owner_id),
        "non-destructive apply must audit submit: {audit:?}"
    );
    assert!(
        audit
            .iter()
            .any(|(action, outcome, principal, _)| action == "apply"
                && outcome == "applied"
                && principal == &owner_id),
        "non-destructive apply must audit apply: {audit:?}"
    );
    let audit_id = first_audit_id(&conn, &app_id).await;
    let update = conn
        .execute(
            "UPDATE zeroship.migrated_migration_audit \
                SET outcome = 'tampered' \
              WHERE audit_id = $1",
            &[&audit_id],
        )
        .await;
    assert!(update.is_err(), "audit rows must reject UPDATE");
    let delete = conn
        .execute(
            "DELETE FROM zeroship.migrated_migration_audit WHERE audit_id = $1",
            &[&audit_id],
        )
        .await;
    assert!(delete.is_err(), "audit rows must reject DELETE");

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn destructive_apply_requires_operator_approval_then_applies_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let operator_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_user(&conn, operator_id, "operator").await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("creator-token", owner_id, [Scope::AppsDeploy], [app_id]);
    auth.insert_actions(
        "operator-token",
        operator_id,
        [Action::AppsApproveMigration],
        [app_id],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(table_exists(&conn, &app_id.to_string(), "notes").await);

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&with_policy(drop_notes_request(), require_approval_policy()))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_requires_operator_approval");
    let migration_id = body["migration_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("pending migration id");
    assert!(
        table_exists(&conn, &app_id.to_string(), "notes").await,
        "pending destructive apply must not drop the table"
    );

    let creator_approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{app_id}/migrations/{migration_id}/approve"
        ))
        .header("authorization", "Bearer creator-token")
        .to_request();
    let resp = test::call_service(&svc, creator_approve).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        table_exists(&conn, &app_id.to_string(), "notes").await,
        "creator approval attempt must not apply"
    );

    let cross_app = Uuid::now_v7();
    let cross_approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{cross_app}/migrations/{migration_id}/approve"
        ))
        .header("authorization", "Bearer operator-token")
        .to_request();
    let resp = test::call_service(&svc, cross_approve).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let operator_approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{app_id}/migrations/{migration_id}/approve"
        ))
        .header("authorization", "Bearer operator-token")
        .to_request();
    let resp = test::call_service(&svc, operator_approve).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert!(
        !body["applied"].as_array().unwrap().is_empty(),
        "approved destructive apply should report applied versions: {body}"
    );
    assert!(
        !table_exists(&conn, &app_id.to_string(), "notes").await,
        "operator-approved destructive apply must drop the table"
    );

    let audit = audit_rows(&conn, &app_id).await;
    assert!(
        audit.iter().any(|(action, outcome, principal, _)| action == "reject_pending"
            && outcome == "requires_operator_approval"
            && principal == &owner_id),
        "pending rejection must be audited: {audit:?}"
    );
    assert!(
        audit
            .iter()
            .any(|(action, outcome, principal, _)| action == "approve"
                && outcome == "approved"
                && principal == &operator_id),
        "operator approval must be audited with approver: {audit:?}"
    );
    assert!(
        audit
            .iter()
            .any(|(action, outcome, principal, detail)| action == "apply"
                && outcome == "applied"
                && principal == &operator_id
                && detail["approval"] == "operator"),
        "approved apply must be audited as operator-approved: {audit:?}"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_user(&conn, &operator_id).await;
}

#[ntex::test]
async fn approval_repreflight_engine_error_audits_rejected_preflight_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let operator_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_user(&conn, operator_id, "operator").await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("creator-token", owner_id, [Scope::AppsDeploy], [app_id]);
    auth.insert_actions(
        "operator-token",
        operator_id,
        [Action::AppsApproveMigration],
        [app_id],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(table_exists(&conn, &app_id.to_string(), "notes").await);

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&with_policy(drop_notes_request(), require_approval_policy()))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let migration_id = body["migration_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("pending migration id");
    let reviewed_gated_versions = body["gated_versions"]
        .as_array()
        .expect("gated versions array")
        .clone();

    conn.batch_execute(&format!(
        "DROP TABLE {}.{}",
        quote_ident(&app_id.to_string()),
        quote_ident("notes")
    ))
    .await
    .expect("simulate live drift before approval");
    assert!(
        !table_exists(&conn, &app_id.to_string(), "notes").await,
        "out-of-band drift should remove the table before approval re-preflight"
    );

    let approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{app_id}/migrations/{migration_id}/approve"
        ))
        .header("authorization", "Bearer operator-token")
        .to_request();
    let resp = test::call_service(&svc, approve).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_failed");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("migration preflight"),
        "engine preflight refusal should preserve the existing error shape: {body}"
    );

    let audit = audit_rows(&conn, &app_id).await;
    assert!(
        audit.iter().any(|(action, outcome, principal, detail)| {
            action == "approve"
                && outcome == "rejected_preflight"
                && principal == &operator_id
                && detail["reviewed_gated_versions"]
                    .as_array()
                    .is_some_and(|versions| {
                        versions.as_slice() == reviewed_gated_versions.as_slice()
                    })
                && detail["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("migration preflight"))
                && detail["re_submit_required"] == true
        }),
        "engine preflight refusal must be audited: {audit:?}"
    );
    let (status, last_error) = migration_status(&conn, &app_id, &migration_id)
        .await
        .expect("workflow row exists");
    assert_eq!(status, "failed");
    assert!(
        last_error
            .as_deref()
            .unwrap_or_default()
            .contains("approval preflight failed"),
        "workflow row should record the approval preflight failure: {last_error:?}"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_user(&conn, &operator_id).await;
}

#[ntex::test]
async fn approval_refuses_stale_ceiling_after_operator_tightening_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let operator_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_user(&conn, operator_id, "operator").await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("creator-token", owner_id, [Scope::AppsDeploy], [app_id]);
    auth.insert_actions(
        "operator-token",
        operator_id,
        [Action::AppsApproveMigration],
        [app_id],
    );
    let (state_v1, tmp_v1) = state_for_with_ceiling_version(auth.clone(), 1);
    let svc_v1 = test::init_service(
        web::App::new()
            .state(state_v1)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc_v1, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(table_exists(&conn, &app_id.to_string(), "notes").await);

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&with_policy(drop_notes_request(), require_approval_policy()))
        .to_request();
    let resp = test::call_service(&svc_v1, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let migration_id = body["migration_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("pending migration id");

    let (state_v2, tmp_v2) = state_for_with_ceiling_version(auth, 2);
    let svc_v2 = test::init_service(
        web::App::new()
            .state(state_v2)
            .configure(zeroship_migrated::configure),
    )
    .await;
    let approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{app_id}/migrations/{migration_id}/approve"
        ))
        .header("authorization", "Bearer operator-token")
        .to_request();
    let resp = test::call_service(&svc_v2, approve).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_approval_stale_ceiling");
    assert_eq!(body["migration_id"], migration_id.to_string());
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("re-submit required"),
        "stale approval refusal should tell the caller to re-submit: {body}"
    );
    assert!(
        table_exists(&conn, &app_id.to_string(), "notes").await,
        "stale approval must not apply the reviewed destructive migration"
    );

    let audit = audit_rows(&conn, &app_id).await;
    assert!(
        audit.iter().any(|(action, outcome, principal, detail)| {
            action == "approve"
                && outcome == "rejected_stale"
                && principal == &operator_id
                && detail["reason"] == "ceiling_changed_since_submit"
                && detail["submitted_ceiling_version"] == 1
                && detail["current_ceiling_version"] == 2
                && detail["re_submit_required"] == true
        }),
        "stale approval refusal must be audited: {audit:?}"
    );
    let (status, last_error) = migration_status(&conn, &app_id, &migration_id)
        .await
        .expect("workflow row exists");
    assert_eq!(status, "failed");
    assert!(
        last_error
            .as_deref()
            .unwrap_or_default()
            .contains("re-submit required"),
        "stale workflow row should force a re-submit: {last_error:?}"
    );

    let _ = std::fs::remove_dir_all(tmp_v1);
    let _ = std::fs::remove_dir_all(tmp_v2);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_user(&conn, &operator_id).await;
}

#[ntex::test]
async fn approval_repreflight_refuses_when_current_policy_changes_reviewed_scope_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let operator_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_user(&conn, operator_id, "operator").await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("creator-token", owner_id, [Scope::AppsDeploy], [app_id]);
    auth.insert_actions(
        "operator-token",
        operator_id,
        [Action::AppsApproveMigration],
        [app_id],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer creator-token")
        .set_payload(require_approval_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let migration_id = body["migration_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("pending migration id");
    let reviewed_gated_versions = body["gated_versions"]
        .as_array()
        .expect("gated versions array")
        .clone();
    assert!(
        !reviewed_gated_versions.is_empty(),
        "policy-gated create should carry reviewed gated versions: {body}"
    );
    assert!(
        !table_exists(&conn, &app_id.to_string(), "notes").await,
        "policy-gated create must stay pending until approval"
    );

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer creator-token")
        .set_payload(tighter_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{app_id}/migrations/{migration_id}/approve"
        ))
        .header("authorization", "Bearer operator-token")
        .to_request();
    let resp = test::call_service(&svc, approve).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_approval_preflight_changed");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("re-submit required"),
        "changed approval preflight should force re-submit: {body}"
    );
    assert!(
        !table_exists(&conn, &app_id.to_string(), "notes").await,
        "changed approval preflight must not apply the pending migration"
    );
    let audit = audit_rows(&conn, &app_id).await;
    assert!(
        audit.iter().any(|(action, outcome, principal, detail)| {
            action == "approve"
                && outcome == "rejected_preflight_changed"
                && principal == &operator_id
                && detail["reviewed_gated_versions"]
                    .as_array()
                    .is_some_and(|versions| {
                        versions.as_slice() == reviewed_gated_versions.as_slice()
                    })
                && detail["current_gated_versions"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                && detail["re_submit_required"] == true
        }),
        "changed approval preflight must be audited: {audit:?}"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_user(&conn, &operator_id).await;
}

#[ntex::test]
async fn approve_endpoint_real_authenticator_denies_creator_and_allows_operator_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let operator_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_platform_admin(&conn, operator_id).await;

    let issuer = Arc::new(PatIssuer::dev_insecure());
    let (creator_token, _creator_token_id) = issue_pat_for_policy(
        &conn,
        &issuer,
        owner_id,
        policy_for("migrated creator migrate only", vec![Action::AppsDeploy]),
        "migrated creator PAT",
    )
    .await;
    let (operator_token, _operator_token_id) = issue_pat_for_policy(
        &conn,
        &issuer,
        operator_id,
        policy_for(
            "migrated operator approve",
            vec![Action::AppsApproveMigration],
        ),
        "migrated operator PAT",
    )
    .await;

    let auth_conn = admin_conn().await;
    let auth: Arc<dyn Authenticator> = Arc::new(real_authenticator(auth_conn, issuer));
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", format!("Bearer {creator_token}"))
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(table_exists(&conn, &app_id.to_string(), "notes").await);

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", format!("Bearer {creator_token}"))
        .set_json(&with_policy(drop_notes_request(), require_approval_policy()))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let migration_id = body["migration_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("pending migration id");

    let creator_approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{app_id}/migrations/{migration_id}/approve"
        ))
        .header("authorization", format!("Bearer {creator_token}"))
        .to_request();
    let resp = test::call_service(&svc, creator_approve).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "real Cedar path must deny creator PATs that hold only apps:migrate"
    );
    assert!(table_exists(&conn, &app_id.to_string(), "notes").await);

    let operator_approve = test::TestRequest::post()
        .uri(&format!(
            "/v1/apps/{app_id}/migrations/{migration_id}/approve"
        ))
        .header("authorization", format!("Bearer {operator_token}"))
        .to_request();
    let resp = test::call_service(&svc, operator_approve).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !table_exists(&conn, &app_id.to_string(), "notes").await,
        "real admin approve token should apply the reviewed destructive migration"
    );
    let audit = audit_rows(&conn, &app_id).await;
    assert!(
        audit.iter().any(|(action, outcome, principal, _)| {
            action == "approve" && outcome == "approved" && principal == &operator_id
        }),
        "real operator approval must be audited: {audit:?}"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_user(&conn, &operator_id).await;
}

#[ntex::test]
async fn apply_api_uses_stored_current_policy_when_no_inline_draft_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer good-token")
        .set_payload(tighter_policy())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["version"], 1);
    assert_eq!(body["ceiling_version"], 1);

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        table_exists(&conn, &app_id.to_string(), "notes").await,
        "stored-policy apply should still create the table in the app schema"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn apply_api_5xx_detail_is_generic_and_does_not_leak_internals() {
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let bad_provision_dsn =
        "host=127.0.0.1 port=1 user=postgres password=zeroship dbname=zeroship_control_test"
            .to_string();
    let (state, tmp) = state_for_with_dsns(auth, bad_provision_dsn, dsn());
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&with_policy(create_notes_request(), tighter_policy()))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_infrastructure");
    assert_eq!(body["detail"], "migration service unavailable");
    let serialized = body.to_string();
    assert!(
        !serialized.contains("127.0.0.1")
            && !serialized.contains("postgres")
            && !serialized.contains(tmp.to_string_lossy().as_ref()),
        "5xx response must not leak connection or filesystem details: {body}"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn apply_api_rejects_bearer_without_apps_migrate_scope() {
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("no-scope-token", owner_id, [Scope::AppsRead], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer no-scope-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn apply_api_rejects_bearer_for_different_app() {
    let app_id = Uuid::now_v7();
    let other_app = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("wrong-app-token", owner_id, [Scope::AppsDeploy], [other_app]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer wrong-app-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn apply_api_rejects_confined_denied_vendor_op_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&denied_vendor_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("createRole")
            || detail.contains("capability")
            || detail.contains("Confined"),
        "denied vendor op should surface guard/capability attribution, got: {body}"
    );

    let role_rows = conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = 'zs_migrated_forbidden'", &[])
        .await
        .expect("query pg_roles");
    assert!(role_rows.is_empty(), "guard-denied createRole must not run");

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn apply_api_rejects_policy_draft_escalation_without_clamping() {
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&with_policy(
            create_notes_request(),
            "[capabilities]\nraw_sql = true\n",
        ))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_policy_invalid");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("capabilities.raw_sql")
            && detail.contains("exceeds tier ceiling"),
        "draft escalation should be rejected explicitly, got: {body}"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn apply_api_rejects_malformed_policy_draft_fail_closed() {
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&with_policy(
            create_notes_request(),
            "[capabilities]\nraw_sq = true\n",
        ))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_policy_invalid");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(MIGRATE_POLICY_FILENAME) && detail.contains("unknown field"),
        "malformed policy should be rejected, got: {body}"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn real_delegating_authenticator_accepts_apps_migrate_owner_pat() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let issuer = Arc::new(PatIssuer::dev_insecure());
    let (token, token_id) = issue_pat_for_policy(
        &conn,
        &issuer,
        owner_id,
        policy_for("migrated apps migrate", vec![Action::AppsDeploy]),
        "migrated integration PAT",
    )
    .await;

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn, issuer);
    let caller = authenticator
        .verify_bearer(&token, app_id, Scope::AppsDeploy)
        .await
        .expect("PAT owner with apps:migrate verifies");
    assert_eq!(caller.principal_id, owner_id);
    assert_eq!(caller.token_id, Some(token_id));

    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn real_delegating_authenticator_rejects_pat_without_apps_migrate_scope() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let issuer = Arc::new(PatIssuer::dev_insecure());
    let (token, _token_id) = issue_pat_for_policy(
        &conn,
        &issuer,
        owner_id,
        policy_for("migrated apps read only", vec![Action::AppsRead]),
        "migrated no-scope PAT",
    )
    .await;

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn, issuer);
    let err = authenticator
        .verify_bearer(&token, app_id, Scope::AppsDeploy)
        .await
        .expect_err("PAT without apps:migrate must be denied");
    assert!(matches!(err, AuthError::Forbidden));

    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn real_delegating_authenticator_rejects_pat_for_different_app_owner() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let other_app_id = Uuid::now_v7();
    let other_owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_app(&conn, other_app_id, other_owner_id).await;

    let issuer = Arc::new(PatIssuer::dev_insecure());
    let (token, _token_id) = issue_pat_for_policy(
        &conn,
        &issuer,
        owner_id,
        policy_for("migrated apps migrate", vec![Action::AppsDeploy]),
        "migrated wrong-app PAT",
    )
    .await;

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn, issuer);
    let err = authenticator
        .verify_bearer(&token, other_app_id, Scope::AppsDeploy)
        .await
        .expect_err("PAT for one owner must not authorize a different app");
    assert!(matches!(err, AuthError::Forbidden));

    cleanup_app(&conn, &app_id).await;
    cleanup_app(&conn, &other_app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_user(&conn, &other_owner_id).await;
}

#[ntex::test]
async fn real_delegating_authenticator_rejects_malformed_bearer() {
    let auth_conn = admin_conn().await;
    let issuer = Arc::new(PatIssuer::dev_insecure());
    let authenticator = real_authenticator(auth_conn, issuer);
    let err = authenticator
        .verify_bearer("not-a-jwt", Uuid::now_v7(), Scope::AppsDeploy)
        .await
        .expect_err("malformed bearer must be denied");
    assert!(matches!(err, AuthError::Unauthorized));
}
