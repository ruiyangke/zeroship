mod fixture;

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{Client, NoTls};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_authz::{Action, Scope};
use zeroship_core::app_derivation;
use zeroship_core::device_grant::{PLATFORM_CLI_CLIENT_ID, PLATFORM_CLI_ISSUABLE_SCOPES};
use zeroship_id::{AppId, OrganizationId, ProjectId, UserId};
use zeroship_migrate::{
    effective_policy_from_charter_toml, ExecutorConfig, MigrationBackend, ProjectLockAcquisition,
};
use zeroship_migrate_postgres::PostgresBackend;
use zeroship_migrate_server::apply::{apply_ir_documents, ApplyMigrationsRequest};
use zeroship_migrate_server::auth::{
    AuthError, Authenticator, ControlPlaneAuthenticator, VerifiedCaller,
};
use zeroship_migrate_server::policy::{ManagedPolicyConfig, MIGRATE_POLICY_FILENAME};
use zeroship_migrate_server::rate_limit::{MutationRateLimiter, PostgresMutationRateLimiter};
use zeroship_migrate_server::schema_apply_store::SchemaApplyStore;
use zeroship_migrate_server::session::CompioPgSession;
use zeroship_migrate_server::MigrationServiceState;

const TEST_POLICY_SEAL_KEY: &[u8] = b"migrated integration policy seal key";

fn dsn() -> String {
    fixture::migrated_url()
}

fn tmpdir(label: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("zs-migrated-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

#[derive(Debug, Clone)]
struct TestCaller {
    principal_id: UserId,
    actions: HashSet<Action>,
    owned_apps: HashSet<AppId>,
}

#[derive(Debug, Default)]
struct StaticAuthenticator {
    callers: Mutex<HashMap<String, TestCaller>>,
    /// Every `request_id` the handlers passed in, in call order, so a test can
    /// assert the id the caller sent is the id authz was given.
    seen_request_ids: Mutex<Vec<String>>,
}

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

#[derive(Debug)]
struct ThrottlingMutationRateLimiter;

#[async_trait(?Send)]
impl MutationRateLimiter for ThrottlingMutationRateLimiter {
    async fn consume(
        &self,
        _source_ip: Option<IpAddr>,
    ) -> Result<zeroship_authn::rate_limit::RateLimitDecision, String> {
        Ok(zeroship_authn::rate_limit::RateLimitDecision::Throttled(
            zeroship_authn::rate_limit::RateLimited {
                retry_after_secs: 20.0,
            },
        ))
    }
}

impl StaticAuthenticator {
    fn new() -> Self {
        Self::default()
    }

    fn insert(
        &self,
        token: impl Into<String>,
        principal_id: &UserId,
        scopes: impl IntoIterator<Item = Scope>,
        owned_apps: impl IntoIterator<Item = AppId>,
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
        principal_id: &UserId,
        actions: impl IntoIterator<Item = Action>,
        owned_apps: impl IntoIterator<Item = AppId>,
    ) {
        let caller = TestCaller {
            principal_id: principal_id.clone(),
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
        app_id: &AppId,
        required_action: Action,
        _request_ip: Option<IpAddr>,
        request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        self.seen_request_ids
            .lock()
            .expect("static auth lock")
            .push(request_id.to_owned());
        let callers = self.callers.lock().expect("static auth lock");
        let caller = callers.get(token).ok_or(AuthError::Unauthorized)?;
        if !caller.actions.contains(&required_action) || !caller.owned_apps.contains(app_id) {
            return Err(AuthError::Forbidden);
        }
        Ok(VerifiedCaller {
            principal_id: caller.principal_id.clone(),
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
    assert_platform_schema_present(&client).await;
    client
}

/// Refuse a database the platform migrations have not been applied to.
///
/// THIS USED TO CREATE THE TABLE UNDER TEST ITSELF, with a hand-written
/// `CREATE TABLE IF NOT EXISTS` for each of the three `migrated_*` tables. That
/// is verification-record class 7 in the fixture rather than the assertion: the
/// suite then measured a shape the corpus does not produce, so a column the
/// corpus declares `NOT NULL` could be nullable here, a CHECK could be absent,
/// and every case would still be green. `zeroship.app_schema_applies` now comes
/// from `db/migrations-ts/20260702000200_control_tables.ts` like every other
/// platform table, and a database without it fails loudly here.
async fn assert_platform_schema_present(conn: &Client) {
    const REQUIRED: [&str; 4] = ["plans", "users", "apps", "app_schema_applies"];
    let rows = conn
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'zeroship'",
            &[],
        )
        .await
        .expect("read information_schema for platform tables");
    let present: HashSet<String> = rows
        .iter()
        .map(|row| row.get::<_, String>("table_name"))
        .collect();
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|table| !present.contains(*table))
        .collect();
    assert!(
        missing.is_empty(),
        "the target database has no platform schema - missing zeroship.{}.\n\
         This target needs a database the PLATFORM migrations have been applied to; \
         it no longer creates any table of its own.\n\
         Run `tests/provision_test_backends.sh`, then:\n  \
         cargo test -p zeroship-migrate-server",
        missing.join(", zeroship."),
    );
}

async fn cleanup_app(conn: &Client, app_id: &AppId) {
    let schema = app_derivation::schema_name(app_id);
    // The vendor crate, not the composition root: `migrator_role_name` lives at
    // `zeroship_migrate_postgres::role`, which is how `src/apply.rs:19` in this
    // same crate already spells it. The old path did not resolve, so this test
    // target had stopped compiling - invisible while the clippy gate aborted
    // before reaching it.
    let role = zeroship_migrate_postgres::role::migrator_role_name(&schema).unwrap();
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    // THE WORKFLOW JOURNAL SCHEMA WAS THE THIRD ONE, and it was leaking. A
    // successful apply runs `runtime_dependents_sql`, which used to create
    // `app_<uuid>` beside `<uuid>`; this teardown dropped only the latter two.
    // It is now the app schema itself, so the journal drop is the third entry
    // below and names the derivation rather than composing a prefix.
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE;",
            q(&format!("{schema}_migrations")),
            q(&zeroship_migrate_server::provisioning::workflow_journal_schema_name(app_id)),
            q(&schema),
        ))
        .await;
    // BOTH ROLES, and the runtime one was leaking too. Every case here that
    // reaches a successful apply creates `app_<uuid>_role` and grants
    // `zeroship_worker` membership in it; only the migrator role was ever
    // dropped, so a shared test database accumulated one dead role and one dead
    // worker membership per apply, forever. That is not just untidiness: the
    // worker's boot-time posture check walks every `app_%_role` membership it
    // holds, so the leak makes a production-shaped check slower on every run and
    // muddies any measurement of it (the `runtime_dependents_sql` docstring's
    // "540 of 540" was taken over a population this suite had been growing).
    let runtime_role =
        zeroship_core::database_role::per_app_role_name(&schema).expect("test app role name");
    for r in [role, runtime_role] {
        let _ = conn
            .batch_execute(&format!(
                "DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='{r_lit}') THEN \
                    EXECUTE 'REASSIGN OWNED BY {rq} TO current_user'; \
                    EXECUTE 'DROP OWNED BY {rq}'; \
                    EXECUTE 'DROP ROLE {rq}'; \
                 END IF; END $$;",
                r_lit = r.replace('\'', "''"),
                rq = q(&r),
            ))
            .await;
    }
    let _ = conn
        .execute(
            "DELETE FROM zeroship.authz_decisions WHERE resource_id = $1",
            &[&app_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.organization_members om \
                 USING zeroship.apps a JOIN zeroship.projects p ON p.id = a.project_id \
                 WHERE om.organization_id = p.organization_id AND a.id = $1::text",
            &[&app_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.apps WHERE id = $1::text",
            &[&app_id.as_str()],
        )
        .await;
}

async fn cleanup_user(conn: &Client, user_id: &UserId) {
    let _ = conn
        .execute(
            "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
            &[&user_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.permission_tokens WHERE owner_id = $1",
            &[&user_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1",
            &[&user_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.organization_members WHERE user_id = $1",
            &[&user_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.principal_grants WHERE principal_id = $1",
            &[&user_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.identity_links WHERE principal_id = $1",
            &[&user_id.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            "DELETE FROM zeroship.users WHERE id = $1",
            &[&user_id.as_str()],
        )
        .await;
}

async fn seed_app(conn: &Client, app_id: &AppId, owner_id: &UserId) {
    cleanup_app(conn, app_id).await;
    cleanup_user(conn, owner_id).await;
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
    let email = format!("migrated-{}@zeroship.test", owner_id.as_str());
    conn.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, 'Migrated Test User', NOW())",
        &[&owner_id.as_str(), &email],
    )
    .await
    .expect("seed user");
    let name = format!("migrated-{}", app_id.as_str());
    // An app needs a project and a project needs an organization:
    // `apps.project_id` is NOT NULL against a RESTRICT foreign key, and the
    // organization is where `owner_id` is seated below. The seat is written by
    // joining `apps.project_id -> projects.organization_id`, the app's one path
    // to a human, so the two rows have to exist before it.
    let organization_id = OrganizationId::mint();
    let project_id = ProjectId::mint();
    conn.execute(
        "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
         VALUES ($1, $2, 'Migrate Fixture', 'fixture@zeroship.test')",
        &[
            &organization_id.as_str(),
            &format!("migrated-org-{}", Uuid::new_v4().simple()),
        ],
    )
    .await
    .expect("seed organization");
    conn.execute(
        "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
         VALUES ($1, $2, 'default', 'Default')",
        &[&project_id.as_str(), &organization_id.as_str()],
    )
    .await
    .expect("seed project");
    conn.execute(
        "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
         SELECT $1::text, $2, $3, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4",
        &[&app_id.as_str(), &name, &plan_id, &project_id.as_str()],
    )
    .await
    .expect("seed app");
    conn.execute(
        "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
             SELECT p.organization_id, $2, 'owner' FROM zeroship.apps a \
               JOIN zeroship.projects p ON p.id = a.project_id WHERE a.id = $1::text \
             ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        &[&app_id.as_str(), &owner_id.as_str()],
    )
    .await
    .expect("seed app owner");
}

fn state_for(authenticator: Arc<dyn Authenticator>) -> (Arc<MigrationServiceState>, PathBuf) {
    state_for_with_dsns(authenticator, dsn(), dsn())
}

async fn state_for_trusted_proxy(
    authenticator: Arc<dyn Authenticator>,
) -> (Arc<MigrationServiceState>, PathBuf) {
    let control_pg = Arc::new(admin_conn().await);
    state_for_with_policy_config_and_edge(
        authenticator,
        dsn(),
        dsn(),
        ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
            .expect("test policy config"),
        Arc::new(PostgresMutationRateLimiter::new(
            control_pg,
            zeroship_authn::rate_limit::Quota::per_minute(2, 3),
        )),
        true,
    )
}

fn state_for_with_dsns(
    authenticator: Arc<dyn Authenticator>,
    provision_dsn: String,
    control_dsn: String,
) -> (Arc<MigrationServiceState>, PathBuf) {
    state_for_with_policy_config(
        authenticator,
        provision_dsn,
        control_dsn,
        ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
            .expect("test policy config"),
    )
}

/// Fail as "the fixture is stale" before any test can fail as a wrong status code.
///
/// Every policy fixture below is a hand-written TOML literal, and the engine's
/// accepted-document set moves under it. When `runtime.lock_timeout_ms` stopped
/// being loadable (2026-08-10), `tighter_policy()` kept granting it and FIVE tests
/// across five unrelated behaviours went red at once: error redaction saw 422 where
/// it asserted 503, the stored-policy fallback saw 422 where it asserted 200, and so
/// on for repreflight, request-id propagation and the versioned policy API. A 422 for
/// an unparseable draft is indistinguishable, at the assertion level, from the status
/// each test meant to exercise, so none of them were testing what their name claims
/// and nothing in the output said "fixture".
///
/// This runs on the one funnel every service-level test goes through, and states the
/// intent of each fixture as an assertion:
///
/// * the three accepted drafts must PARSE and must ADMIT against the operator
///   ceiling (parse alone would not have caught a ceiling-side tightening);
/// * `escalating_policy` must parse but must be REFUSED by `admit` (it is what
///   `policy_api_rejects_escalating_draft_at_submit_pg` exercises);
/// * `malformed_policy` must NOT parse (same, for the 422 path).
///
/// NOT DERIVED from the registry, deliberately. The knob set is enumerable
/// (`PolicyRegistry::iter` plus each `KnobDef`'s `enforcement`/`default`), so a
/// fixture could be generated from the loadable keys. But these fixtures are not
/// "some accepted document": the tests assert on their MEANING (`effective_profile
/// .destructive_ops == "Forbid"`, an escalation that must be refused, two drafts that
/// must differ), and a generated document would restate whatever the registry
/// happens to hold rather than the posture the test is about. Generation would also
/// silently change the fixture bodies under assertions that compare `raw_toml`. So
/// the literals stay readable and this guard carries the currency check.
fn assert_policy_fixtures_are_current(policy_config: &ManagedPolicyConfig) {
    let app_id = AppId::mint();
    let parse = |body: &str| {
        policy_config.parse_draft(&zeroship_migrate_server::policy::CreatorPolicyDraft {
            filename: MIGRATE_POLICY_FILENAME,
            body,
        })
    };

    for (name, body) in [
        ("tighter_policy", tighter_policy()),
        ("second_tighter_policy", second_tighter_policy()),
        ("require_approval_policy", require_approval_policy()),
    ] {
        let draft = parse(body).unwrap_or_else(|err| {
            panic!(
                "policy fixture {name} no longer parses: {err:?}\n\
                 the engine's accepted-document set moved; fix the fixture, not the \
                 test that asserted a status code\nbody:\n{body}"
            )
        });
        policy_config
            .compose_effective_for_app(&app_id, None, Some(&draft))
            .unwrap_or_else(|err| {
                panic!(
                    "policy fixture {name} parses but no longer admits against the \
                     operator ceiling: {err:?}\nbody:\n{body}"
                )
            });
    }

    let escalating = parse(escalating_policy())
        .expect("escalating_policy must PARSE; it is refused at admit, not at load");
    assert!(
        policy_config
            .compose_effective_for_app(&app_id, None, Some(&escalating))
            .is_err(),
        "escalating_policy must be refused by admit; a fixture the ceiling now grants \
         would turn policy_api_rejects_escalating_draft_at_submit_pg into a no-op"
    );

    assert!(
        parse(malformed_policy()).is_err(),
        "malformed_policy must NOT parse; a fixture the loader now accepts would turn \
         policy_api_rejects_malformed_toml_at_submit_pg into a no-op"
    );
}

fn state_for_with_policy_config(
    authenticator: Arc<dyn Authenticator>,
    provision_dsn: String,
    control_dsn: String,
    policy_config: ManagedPolicyConfig,
) -> (Arc<MigrationServiceState>, PathBuf) {
    state_for_with_policy_config_and_edge(
        authenticator,
        provision_dsn,
        control_dsn,
        policy_config,
        Arc::new(AllowAllMutationRateLimiter),
        false,
    )
}

fn state_for_with_policy_config_and_edge(
    authenticator: Arc<dyn Authenticator>,
    provision_dsn: String,
    control_dsn: String,
    policy_config: ManagedPolicyConfig,
    mutation_rate_limiter: Arc<dyn MutationRateLimiter>,
    trust_proxy: bool,
) -> (Arc<MigrationServiceState>, PathBuf) {
    assert_policy_fixtures_are_current(&policy_config);
    let tmp = tmpdir("tmp");
    (
        Arc::new(MigrationServiceState::new(
            provision_dsn,
            control_dsn,
            tmp.clone(),
            authenticator,
            mutation_rate_limiter,
            trust_proxy,
            policy_config,
        )),
        tmp,
    )
}

/// Drive the explicit database lifecycle operation through the same HTTP surface
/// every apply caller uses. Keeping this separate from `seed_app` is deliberate:
/// an app row and a database schema are independent lifecycle facts, and the
/// refusal test needs to seed the former while leaving the latter absent.
async fn post_database_create<S, E>(
    service: &ntex::service::Pipeline<S>,
    database_id: &AppId,
    token: Option<&str>,
) -> (StatusCode, Value)
where
    S: ntex::Service<ntex::http::Request, Response = web::WebResponse, Error = E>,
    E: std::fmt::Debug,
{
    let uri = format!("/v1/databases/{}", database_id.as_str());
    let request = match token {
        Some(token) => test::TestRequest::post()
            .uri(&uri)
            .header("authorization", format!("Bearer {token}"))
            .to_request(),
        None => test::TestRequest::post().uri(&uri).to_request(),
    };
    let response = test::call_service(service, request).await;
    let status = response.status();
    let body = serde_json::from_slice(&test::read_body(response).await)
        .expect("database create response is JSON");
    (status, body)
}

async fn create_database<S, E>(
    service: &ntex::service::Pipeline<S>,
    database_id: &AppId,
    token: &str,
) where
    S: ntex::Service<ntex::http::Request, Response = web::WebResponse, Error = E>,
    E: std::fmt::Debug,
{
    let (status, body) = post_database_create(service, database_id, Some(token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "explicit database create failed: {body}"
    );
    assert_eq!(
        body,
        json!({"database_id": database_id.as_str()}),
        "database create returned the wrong identity"
    );
}

/// A syntactically valid descriptor hash for the cases that are about
/// something else.
///
/// Only the CONTROL PLANE compares this value against anything; `migrated`
/// checks its spelling and records it. So a fixture that is not about the
/// deploy precondition needs a well-formed hash and nothing more.
const TEST_DESCRIPTOR_SHA256: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";

/// A second, DIFFERENT well-formed hash - the "the build's descriptor bytes
/// moved" case.
const TEST_DESCRIPTOR_SHA256_NEXT: &str =
    "2222222222222222222222222222222222222222222222222222222222222222";

fn create_notes_request() -> Value {
    json!({
        "kind": "ir",
        "descriptor_sha256": TEST_DESCRIPTOR_SHA256,
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

/// The complete migration history for the rollback-coverage regression.
///
/// The first document is byte-for-byte the history returned by
/// [`create_notes_request`]. The second advances that history, so submitting
/// `create_notes_request()` afterwards is a real 1..K request against a journal
/// at 1..N rather than a different spelling of the same set.
fn complete_notes_history_request() -> Value {
    let mut request = create_notes_request();
    request["descriptor_sha256"] = json!(TEST_DESCRIPTOR_SHA256_NEXT);
    request["documents"]
        .as_array_mut()
        .expect("create-notes fixture documents")
        .push(json!({
            "filename": "0002_extend_notes.ir.json",
            "body": {
                "ir_version": 1,
                "name": "extend_notes",
                "ops": [{
                    "op": "addColumn",
                    "table": "notes",
                    "column": "summary",
                    "type": "text",
                    "nullable": true
                }]
            }
        }));
    request
}

fn lock_span_request() -> Value {
    let mut request = create_notes_request();
    request["descriptor_sha256"] = json!(TEST_DESCRIPTOR_SHA256_NEXT);
    request["documents"]
        .as_array_mut()
        .expect("create-notes fixture documents")
        .extend([
            json!({
                "filename": "0002_create_lock_span_marker.ir.json",
                "body": {
                    "ir_version": 1,
                    "name": "create_lock_span_marker",
                    "ops": [{
                        "op": "createTable",
                        "name": "lock_span_marker",
                        "columns": [
                            {"name": "value", "type": "text", "nullable": false}
                        ]
                    }]
                }
            }),
            json!({
                "filename": "0003_extend_notes.ir.json",
                "body": {
                    "ir_version": 1,
                    "name": "extend_notes_after_marker",
                    "flags": {"lock_timeout_ms": 30_000},
                    "ops": [{
                        "op": "addColumn",
                        "table": "notes",
                        "column": "lock_span_file_two",
                        "type": "text",
                        "nullable": true
                    }]
                }
            }),
        ]);
    request
}

fn denied_vendor_request() -> Value {
    json!({
        "kind": "ir",
        "descriptor_sha256": TEST_DESCRIPTOR_SHA256,
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
    let mut request = create_notes_request();
    request["descriptor_sha256"] = json!(TEST_DESCRIPTOR_SHA256_NEXT);
    request["documents"]
        .as_array_mut()
        .expect("create-notes fixture documents")
        .push(json!({
            "filename": "0002_drop_notes.ir.json",
            "body": {
                "ir_version": 1,
                "name": "drop_notes",
                "ops": [{
                    "op": "dropTable",
                    "table": "notes"
                }]
            }
        }));
    request
}

/// A creator migration that CREATES a table named exactly like the platform's
/// journal.
///
/// The journal now lives in the app's OWN schema, so this name is inside the
/// creator's declared scope as far as the confined ceiling is concerned. If the
/// op were permitted on a fresh app the engine's `CREATE TABLE IF NOT EXISTS`
/// bootstrap would ADOPT the creator's table as the journal.
///
/// `createTable` takes `name`, not `table`. The first draft of this fixture used
/// `table` and the request was refused as a MALFORMED IR ENVELOPE - a 422 that
/// looks exactly like a name refusal in the response and proves nothing about the
/// name. Keep the op shape valid or the case measures the typo.
fn create_platform_journal_table_request() -> Value {
    json!({
        "kind": "ir",
        "descriptor_sha256": TEST_DESCRIPTOR_SHA256,
        "documents": [{
            "filename": "0003_shadow_journal.ir.json",
            "body": {
                "ir_version": 1,
                "name": "shadow_journal",
                "ops": [{
                    "op": "createTable",
                    "name": "__zeroship_schema_migrations",
                    "columns": [
                        {"name": "creator_field", "type": "text", "nullable": false}
                    ]
                }]
            }
        }]
    })
}

/// The same name, as a `dropTable`.
fn drop_platform_journal_request() -> Value {
    let mut request = create_notes_request();
    request["descriptor_sha256"] = json!(TEST_DESCRIPTOR_SHA256_NEXT);
    request["documents"]
        .as_array_mut()
        .expect("create-notes fixture documents")
        .push(json!({
            "filename": "0003_drop_journal.ir.json",
            "body": {
                "ir_version": 1,
                "name": "drop_journal",
                "ops": [{
                    "op": "dropTable",
                    "table": "__zeroship_schema_migrations"
                }]
            }
        }));
    request
}

fn with_policy(mut request: Value, body: &str) -> Value {
    request["policy"] = json!({
        "filename": MIGRATE_POLICY_FILENAME,
        "body": body
    });
    request
}

// The creator policy draft is now a `zeroship-migrate-policy` `PolicyDoc` (grant rules
// against the operator ceiling), not the old `PolicyProfile` TOML. A draft may only
// TIGHTEN: `safety.destructive_ops` orders forbid <= warn <= allow, so `forbid` is
// admissible under the confined ceiling's `allow`.
//
// This draft ALSO carried `runtime.lock_timeout_ms = 1000`. It cannot: every
// `runtime.*` knob is registered `DeclaredOnly` with default 1, and the engine's
// II.6 load gate refuses ANY document raising one above its default
// (`DeclaredOnlyNonDefault`), in an operator ceiling or a creator draft alike. The
// grant was dropped from the shipped ceilings and the `policy.rs` unit tests on
// 2026-08-10 (see `crates/zeroship-migrate-server/policies/confined.policy.toml`) and not from
// here, so every test that PUT this fixture got a 422 for a parse failure instead
// of exercising the behaviour its name claims. `assert_policy_fixtures_are_current`
// below is what makes the next such removal fail as a stale fixture.
fn tighter_policy() -> &'static str {
    "policy_version = 1\n\n[[grant]]\nkey = \"safety.destructive_ops\"\nvalue = \"forbid\"\nscope = \"all\"\n"
}

// Approval is now the SEALED `safety.require_approval` obligation the engine declares and
// the host enforces. A draft authors it as a normal `[[require]]` — `always` gates
// EVERY migration for operator approval (destructive or not), the successor to the old
// managed-only `require_approval = true` overlay. The draft also RE-STATES the
// `safety.destructive_ops = allow` grant it wants kept (admit resolves grants
// from the draft layer, so a draft that only tightens one knob must re-state the
// ceiling grants it relies on — here, keeping destructive ops classifiable-not-denied
// so the approval gate can hold the DROP for review instead of the guard forbidding it).
fn require_approval_policy() -> &'static str {
    "policy_version = 1\n\n[[require]]\nkey = \"safety.require_approval\"\nvalue = \"always\"\nscope = \"all\"\n\n[[grant]]\nkey = \"safety.destructive_ops\"\nvalue = \"allow\"\nscope = \"all\"\n"
}

// A SECOND, textually distinct tightening, so the versioned-policy test can submit
// two drafts and tell version 1 from version 2 by `raw_toml`. It used to differ by a
// lower `runtime.lock_timeout_ms`, which the engine no longer loads at any non-default
// value; it now also revokes `schema.rename`, a real tightening of a grant the
// confined ceiling does carry (`value = true`).
fn second_tighter_policy() -> &'static str {
    "policy_version = 1\n\n[[grant]]\nkey = \"safety.destructive_ops\"\nvalue = \"forbid\"\nscope = \"all\"\n\n[[grant]]\nkey = \"schema.rename\"\nvalue = false\nscope = \"all\"\n"
}

// A draft that grants a capability the confined ceiling does not (`sql.raw`):
// `admit` REJECTS it (escalation), never clamps.
fn escalating_policy() -> &'static str {
    "policy_version = 1\n\n[[grant]]\nkey = \"sql.raw\"\nvalue = true\nscope = \"all\"\n"
}

// A malformed draft: `kez` is not a known key (`deny_unknown_fields` → parse error).
fn malformed_policy() -> &'static str {
    "policy_version = 1\n\n[[grant]]\nkez = \"sql.raw\"\nvalue = true\nscope = \"all\"\n"
}

const PLATFORM_ISSUER: &str = "https://auth.zeroship.test/oauth2";
const PLATFORM_KID: &str = "migrated-apply-api-test-kid";
const PLATFORM_KEY_SEED: u8 = 47;

/// The console BFF's client id, and the one these fixtures mint under.
///
/// Deliberately NOT `zeroship-cli`: that is the single client id whose token
/// scopes `BearerVerifier` intersects against the principal's live
/// `zeroship.principal_grants` rows, so a CLI bearer for a principal these
/// tests never seeded grants for would silently collapse to the CLI's default
/// issuable set instead of carrying the scope the test asked for.
const CONSOLE_CLIENT_ID: &str = "zeroship-console";

/// A local JWKS endpoint on its own ntex `System` thread.
///
/// The platform auth provider verifies an access token against the issuer's
/// published key over HTTP, so a fixture that hands out a platform bearer has
/// to leave a reachable JWKS behind it. Shaped after
/// `crates/zeroship-control/tests/common/mod.rs`; deliberately a private copy rather
/// than a shared module, because that one is a `tests/common` of another crate.
struct PlatformJwks {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlatformJwks {
    fn start() -> Self {
        let body = Arc::new(RwLock::new(platform_jwks_body()));
        let factory_body = body.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("migrated-apply-api-platform-jwks")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let body = factory_body.clone();
                        async move {
                            web::App::new().state(body).service(
                                web::resource("/.well-known/jwks.json")
                                    .route(web::get().to(platform_jwks_handler)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send platform jwks addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("platform jwks mock starts");
        Self {
            base: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base)
    }
}

impl Drop for PlatformJwks {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn platform_jwks_handler(body: web::types::State<Arc<RwLock<String>>>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(body.read().expect("jwks body lock").clone())
}

/// One JWKS server per test binary, started on first use. Held in a `OnceLock`
/// static, which is never dropped, so the server outlives every test here and
/// stays reachable for as long as any bearer it backs may be presented.
fn platform_jwks_url() -> String {
    static JWKS: OnceLock<PlatformJwks> = OnceLock::new();
    JWKS.get_or_init(PlatformJwks::start).jwks_url()
}

fn platform_encoding_key() -> EncodingKey {
    let sk = SigningKey::from_bytes(&[PLATFORM_KEY_SEED; 32]);
    let pkcs8 = sk.to_pkcs8_der().expect("encode pkcs8");
    EncodingKey::from_ed_der(pkcs8.as_bytes())
}

fn platform_jwks_body() -> String {
    let sk = SigningKey::from_bytes(&[PLATFORM_KEY_SEED; 32]);
    json!({
        "keys": [{
            "kid": PLATFORM_KID,
            "kty": "OKP",
            "alg": "EdDSA",
            "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes()),
        }]
    })
    .to_string()
}

/// A platform OAuth access token for `subject` carrying `scope`, signed by the
/// key `platform_jwks_url()` publishes.
fn platform_token(subject: &UserId, scope: &str) -> String {
    platform_token_for_client(subject, CONSOLE_CLIENT_ID, scope)
}

fn platform_token_for_client(subject: &UserId, client_id: &str, scope: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let claims = json!({
        "iss": PLATFORM_ISSUER,
        "sub": subject.as_str(),
        "aud": "control.zeroship.ai",
        "exp": now + 3600,
        "iat": now,
        "nbf": now.saturating_sub(1),
        "jti": Uuid::new_v4().to_string(),
        "client_id": client_id,
        "scope": scope,
    });
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some("at+jwt".to_string());
    header.kid = Some(PLATFORM_KID.to_string());
    encode(&header, &claims, &platform_encoding_key()).expect("platform token")
}

fn real_authenticator(conn: Client) -> ControlPlaneAuthenticator {
    let auth_provider = Arc::new(zeroship_core::auth_provider::AuthProvider::platform(
        zeroship_core::auth_provider::PlatformProvider::new(
            zeroship_core::auth_provider::PlatformConfig::new(
                PLATFORM_ISSUER,
                Some(platform_jwks_url()),
            )
            .expect("test platform auth config"),
        ),
    ));
    let control_pg = Arc::new(conn);
    let bearer_verifier = zeroship_authn::BearerVerifier::new(
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

/// Completed rows in the app's OWN journal, `"<app_uuid>".__zeroship_schema_migrations`.
///
/// BOTH HALVES OF THAT NAME MOVED on 2026-08-28 and the old spelling
/// (`"<app_uuid>_migrations".schema_migrations`) resolves to nothing, so a probe
/// left on it would return 0 forever and every `>= 1` assertion below would fail
/// while the code was correct. `journal_is_absent_at_the_unfenced_name` is the
/// case that keeps this helper honest by pinning the OTHER direction.
async fn journaled_count(conn: &Client, app_id: &AppId) -> i64 {
    let q = format!(
        "\"{}\".__zeroship_schema_migrations",
        app_derivation::schema_name(app_id).replace('"', "\"\"")
    );
    let lit = q.replace('\'', "''");
    let present = conn
        .query(
            &format!("SELECT to_regclass('{lit}') IS NOT NULL AS p"),
            &[],
        )
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

/// Does `to_regclass` resolve this exact relation?
async fn relation_exists(conn: &Client, qualified: &str) -> bool {
    let lit = qualified.replace('\'', "''");
    let rows = conn
        .query(
            &format!("SELECT to_regclass('{lit}') IS NOT NULL AS p"),
            &[],
        )
        .await
        .expect("regclass probe");
    rows[0].get("p")
}

async fn probe_bool(conn: &Client, sql: &str) -> bool {
    let rows = conn
        .query(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("probe failed: {sql}: {e}"));
    rows[0].get(0)
}

/// Explicit creation is idempotent and stops at the database's migration
/// boundary. Runtime-owned objects still belong to the later apply operation.
#[ntex::test]
async fn create_database_is_idempotent_and_provisions_only_schema_and_migrator_pg() {
    let conn = admin_conn().await;
    let database_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &database_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [database_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let service = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let first = post_database_create(&service, &database_id, Some("good-token")).await;
    let second = post_database_create(&service, &database_id, Some("good-token")).await;

    let schema = app_derivation::schema_name(&database_id);
    let migrator_role =
        zeroship_migrate_postgres::role::migrator_role_name(&schema).expect("migrator role name");
    let runtime_role =
        zeroship_core::database_role::per_app_role_name(&schema).expect("runtime role name");
    let data_schema_exists = probe_bool(
        &conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = '{}')",
            schema.replace('\'', "''")
        ),
    )
    .await;
    let migrator_role_exists = probe_bool(
        &conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{}')",
            migrator_role.replace('\'', "''")
        ),
    )
    .await;
    // THE WORKFLOW JOURNAL SCHEMA IS NO LONGER A WITNESS HERE, and deleting the
    // probe is the honest move rather than an omission. It was `app_<uuid>`
    // beside a data schema of `<uuid>`, so its absence proved that create had
    // not done apply-owned work. The journal schema is now
    // `provisioning::workflow_journal_schema_name` of the tenant, which is the
    // DATA schema this same test asserts create DOES make - so the probe would
    // now be asserting that one name is both present and absent. The runtime
    // role and the audit table below still witness the same boundary.
    let runtime_role_exists = probe_bool(
        &conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{}')",
            runtime_role.replace('\'', "''")
        ),
    )
    .await;
    let audit = format!(
        "{}.{}",
        quote_ident(&schema),
        quote_ident(zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE)
    );
    let audit_table_exists = relation_exists(&conn, &audit).await;
    let ledger_rows: i64 = conn
        .query_one(
            "SELECT count(*)::int8 FROM zeroship.app_schema_applies WHERE app_id = $1::text",
            &[&database_id.as_str()],
        )
        .await
        .expect("count create-only ledger rows")
        .get(0);

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &database_id).await;
    cleanup_user(&conn, &owner_id).await;

    let expected = (StatusCode::OK, json!({"database_id": database_id}));
    assert_eq!(first, expected, "first database create response");
    assert_eq!(second, expected, "idempotent database create response");
    assert!(data_schema_exists, "create did not create schema {schema}");
    assert!(
        migrator_role_exists,
        "create did not provision migrator role {migrator_role}"
    );
    assert!(
        !runtime_role_exists,
        "create crossed into apply-owned runtime role {runtime_role}"
    );
    assert!(
        !audit_table_exists,
        "create crossed into apply-owned audit table {audit}"
    );
    assert_eq!(
        ledger_rows, 0,
        "database creation must not open an apply ledger row"
    );
}

/// Database creation uses the same bearer scope and app ownership check as
/// apply, and every denial precedes provisioning.
#[ntex::test]
async fn create_database_denials_leave_no_schema_role_or_ledger_pg() {
    let conn = admin_conn().await;
    let database_id = AppId::mint();
    let other_app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &database_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "no-scope-token",
        &owner_id,
        [Scope::AppsRead],
        [database_id.clone()],
    );
    auth.insert(
        "wrong-app-token",
        &owner_id,
        [Scope::AppsDeploy],
        [other_app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let service = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let unauthenticated = post_database_create(&service, &database_id, None).await;
    let missing_scope = post_database_create(&service, &database_id, Some("no-scope-token")).await;
    let wrong_app = post_database_create(&service, &database_id, Some("wrong-app-token")).await;

    let schema = app_derivation::schema_name(&database_id);
    let migrator_role =
        zeroship_migrate_postgres::role::migrator_role_name(&schema).expect("migrator role name");
    let schema_exists = probe_bool(
        &conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = '{}')",
            schema.replace('\'', "''")
        ),
    )
    .await;
    let role_exists = probe_bool(
        &conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{}')",
            migrator_role.replace('\'', "''")
        ),
    )
    .await;
    let ledger_rows: i64 = conn
        .query_one(
            "SELECT count(*)::int8 FROM zeroship.app_schema_applies WHERE app_id = $1::text",
            &[&database_id.as_str()],
        )
        .await
        .expect("count denied-create ledger rows")
        .get(0);

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &database_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert_eq!(
        unauthenticated,
        (
            StatusCode::UNAUTHORIZED,
            json!({"error": "unauthenticated"})
        )
    );
    assert_eq!(
        missing_scope,
        (StatusCode::FORBIDDEN, json!({"error": "forbidden"}))
    );
    assert_eq!(
        wrong_app,
        (StatusCode::FORBIDDEN, json!({"error": "forbidden"}))
    );
    assert!(!schema_exists, "denied create made schema {schema}");
    assert!(
        !role_exists,
        "denied create made migrator role {migrator_role}"
    );
    assert_eq!(ledger_rows, 0, "denied create opened an apply ledger row");
}

/// An apply is not a database-creation operation.
///
/// This is deliberately a live PostgreSQL case because the refusal's important
/// property is absence of catalog side effects: `provision_migrator` creates a
/// cluster-global role and grants membership before it first touches the missing
/// schema. A mocked handler can prove the 409 body but cannot prove those objects
/// were never created, or that the deploy ledger stayed untouched.
#[ntex::test]
async fn apply_refuses_an_uncreated_database_before_provisioning_or_ledger_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let schema = app_derivation::schema_name(&app_id);
    let migrator_role =
        zeroship_migrate_postgres::role::migrator_role_name(&schema).expect("migrator role name");
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let response = test::call_service(
        &svc,
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
            .header("authorization", "Bearer good-token")
            .set_json(&create_notes_request())
            .to_request(),
    )
    .await;
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&test::read_body(response).await).expect("JSON refusal body");
    let schema_exists = probe_bool(
        &conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = '{}')",
            schema.replace('\'', "''")
        ),
    )
    .await;
    let role_exists = probe_bool(
        &conn,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{}')",
            migrator_role.replace('\'', "''")
        ),
    )
    .await;
    let membership_exists: bool = conn
        .query(
            "SELECT EXISTS (SELECT 1 FROM pg_auth_members membership \
                 JOIN pg_roles granted ON granted.oid = membership.roleid \
                 JOIN pg_roles member ON member.oid = membership.member \
                 WHERE granted.rolname = $1 AND member.rolname = current_user)",
            &[&migrator_role],
        )
        .await
        .expect("probe refused apply migrator membership")[0]
        .get(0);
    let ledger_rows: i64 = conn
        .query(
            "SELECT count(*)::int8 FROM zeroship.app_schema_applies WHERE app_id = $1::text",
            &[&app_id.as_str()],
        )
        .await
        .expect("count refused apply ledger rows")[0]
        .get(0);

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an absent database must be a creator-recoverable 409, never a 404 or 5xx: {body}"
    );
    assert_eq!(body["error"], "database_not_created", "{body}");
    assert_eq!(
        body["remedy"],
        format!("POST /v1/databases/{}", app_id.as_str()),
        "the body must name the explicit operation that makes a retry valid: {body}"
    );
    assert!(!schema_exists, "a refused apply created schema {schema}");
    assert!(
        !role_exists,
        "a refused apply leaked cluster-global role {migrator_role}"
    );
    assert!(
        !membership_exists,
        "a refused apply granted cluster-global role {migrator_role} to the provisioner"
    );
    assert_eq!(
        ledger_rows, 0,
        "a refused apply polluted the deploy gate's ledger head"
    );
}

async fn table_privileges(conn: &Client, grantee: &str, schema: &str, table: &str) -> Vec<String> {
    conn.query(
        "SELECT privilege_type \
           FROM information_schema.table_privileges \
          WHERE grantee = $1 AND table_schema = $2 AND table_name = $3 \
          ORDER BY privilege_type",
        &[&grantee, &schema, &table],
    )
    .await
    .expect("read exact table privileges")
    .iter()
    .map(|row| row.get(0))
    .collect()
}

/// Run `sql` under the PRODUCTION runtime identity for `app_schema` and switch
/// back, whatever happened.
///
/// The chain is the worker's, not a shortcut to the app role: `SET SESSION
/// AUTHORIZATION zeroship_worker` (the one login role every worker process
/// connects as) then `SET ROLE app_<id>_role`. `runtime_dependents_sql` grants
/// that membership `WITH INHERIT FALSE`, so the `SET ROLE` is load-bearing -
/// without it the worker holds no reach into the app schema at all - and the
/// two-step is exactly what `zeroship-data-v8` does per request.
///
/// Needs a superuser DSN for `SET SESSION AUTHORIZATION`. The migrate service's
/// own provisioning DSN is that principal (it creates roles and schemas), so a
/// target that cannot run this could not run the apply above either; the
/// `expect` names the requirement rather than skipping.
async fn as_app_runtime_identity(
    conn: &Client,
    app_schema: &str,
    sql: &str,
) -> Result<(), compio_postgres::Error> {
    let worker = quote_ident(zeroship_migrate_server::apply::WORKER_ROLE);
    let runtime = quote_ident(
        &zeroship_core::database_role::per_app_role_name(app_schema).expect("test app role name"),
    );
    conn.batch_execute(&format!(
        "SET SESSION AUTHORIZATION {worker}; SET ROLE {runtime};"
    ))
    .await
    .expect(
        "assuming the worker identity needs a superuser connection and a \
         zeroship_worker granted membership in the app runtime role - the first \
         comes from the test DSN, the second from provision_runtime_app_role",
    );
    let out = conn.batch_execute(sql).await;
    let restored = conn
        .batch_execute("RESET ROLE; RESET SESSION AUTHORIZATION;")
        .await;
    assert!(
        restored.is_ok(),
        "the admin identity must come back on this connection or every later \
         assertion in this case is measuring the wrong principal: {restored:?}"
    );
    out
}

/// Explicit database creation followed by a real apply must leave the creator
/// table usable through the production worker-to-app role chain.
///
/// There is deliberately no fixture grant here. The create endpoint and apply
/// endpoint are the complete public lifecycle that must establish runtime
/// authority. A test that grants columns itself proves only that PostgreSQL
/// honors the test's grant, not that the migration service produced one.
#[ntex::test]
async fn a_created_then_migrated_database_is_usable_by_the_runtime_role_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let schema = app_derivation::schema_name(&app_id);
    let migrator = zeroship_migrate_postgres::role::migrator_role_name(&schema).unwrap();
    let posture = conn
        .query_one(
            "SELECT pg_get_userbyid(n.nspowner) AS owner, \
                    has_schema_privilege($1, $2, 'USAGE') AS can_use, \
                    has_schema_privilege($1, $2, 'CREATE') AS can_create, \
                    pg_has_role(current_user, $1, 'SET') AS can_set \
               FROM pg_namespace n WHERE n.nspname = $2",
            &[&migrator, &schema],
        )
        .await
        .expect("inspect explicit database ownership");
    assert_eq!(posture.get::<_, String>("owner"), migrator);
    assert!(posture.get::<_, bool>("can_use"));
    assert!(posture.get::<_, bool>("can_create"));
    assert!(posture.get::<_, bool>("can_set"));
    conn.batch_execute(&format!(
        "BEGIN; SET LOCAL ROLE {}; CREATE TABLE {}.__fixture_role_probe(id bigint); ROLLBACK",
        quote_ident(&migrator),
        quote_ident(&schema),
    ))
    .await
    .expect("the provisioned migrator must create objects in its schema");

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "migration apply failed: {}",
        String::from_utf8_lossy(&body)
    );

    let table = format!("{}.{}", quote_ident(&schema), quote_ident("notes"));
    let runtime_role =
        zeroship_core::database_role::per_app_role_name(&schema).expect("test app role name");
    let app_table_privileges = table_privileges(&conn, &runtime_role, &schema, "notes").await;
    let audit_name = zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE;
    let audit_table_privileges = table_privileges(&conn, &runtime_role, &schema, audit_name).await;
    let audit = format!("{}.{}", quote_ident(&schema), quote_ident(audit_name));
    let sequence_privileges = conn
        .query(
            "SELECT \
                has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'USAGE'), \
                has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'SELECT'), \
                has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'UPDATE')",
            &[&runtime_role, &audit],
        )
        .await
        .expect("read exact audit sequence privileges");
    let sequence_privileges = (
        sequence_privileges[0].get::<_, bool>(0),
        sequence_privileges[0].get::<_, bool>(1),
        sequence_privileges[0].get::<_, bool>(2),
    );
    let schema_privileges = conn
        .query(
            "SELECT has_schema_privilege($1, $2, 'USAGE'), \
                    has_schema_privilege($1, $2, 'CREATE')",
            &[&runtime_role, &schema],
        )
        .await
        .expect("read exact runtime schema privileges");
    let schema_privileges = (
        schema_privileges[0].get::<_, bool>(0),
        schema_privileges[0].get::<_, bool>(1),
    );
    let publication =
        zeroship_core::replication_names::publication_name(&schema).expect("test publication name");
    let published_tables = conn
        .query(
            "SELECT c.relname
               FROM pg_publication_rel AS pr
               JOIN pg_publication AS p ON p.oid = pr.prpubid
               JOIN pg_class AS c ON c.oid = pr.prrelid
               JOIN pg_namespace AS n ON n.oid = c.relnamespace
              WHERE p.pubname = $1 AND n.nspname = $2
              ORDER BY c.relname",
            &[&publication, &schema],
        )
        .await
        .expect("read publication membership")
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>();
    println!(
        "information_schema.table_privileges for {runtime_role}.notes: \
         {app_table_privileges:?}"
    );
    println!(
        "audit privileges for {runtime_role}: table={audit_table_privileges:?}, \
         sequence={sequence_privileges:?}, schema={schema_privileges:?}"
    );
    let insert = as_app_runtime_identity(
        &conn,
        &schema,
        &format!(
            "INSERT INTO {table} (id, title, body) \
             VALUES ('note-1', 'runtime write', 'created then migrated')"
        ),
    )
    .await;
    let select = as_app_runtime_identity(
        &conn,
        &schema,
        &format!("SELECT title, body FROM {table} WHERE id = 'note-1'"),
    )
    .await;
    println!("runtime INSERT result: {insert:?}");
    println!("runtime SELECT result: {select:?}");

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert!(
        insert.is_ok(),
        "created-then-migrated runtime role could not INSERT its own table: {insert:?}"
    );
    assert!(
        select.is_ok(),
        "created-then-migrated runtime role could not SELECT its own table: {select:?}"
    );
    assert_eq!(
        audit_table_privileges,
        vec!["DELETE", "INSERT", "SELECT", "UPDATE"],
        "every table in the app schema must receive ordinary DML"
    );
    assert_eq!(
        sequence_privileges,
        (true, true, false),
        "the app role must be able to use and read its schema sequences"
    );
    assert_eq!(
        schema_privileges,
        (true, false),
        "the runtime role must enter its schema but cannot author objects"
    );
    for required in [
        "notes",
        zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE,
        "__zeroship_schema_migrations",
    ] {
        assert!(
            published_tables.iter().any(|table| table == required),
            "successful apply omitted {required} from publication {publication}: {published_tables:?}"
        );
    }
}

/// A real apply leaves the audit writer operational through the runtime role.
///
/// The catalog probes diagnose missing table or sequence grants. The write uses
/// the worker-to-app identity chain and is the binding correctness check.
#[ntex::test]
async fn a_real_apply_leaves_the_runtime_role_able_to_write_the_unmask_audit_row_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let schema = app_derivation::schema_name(&app_id);
    let table = zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE;
    let audit = format!("{}.{}", quote_ident(&schema), quote_ident(table));
    let runtime_role =
        zeroship_core::database_role::per_app_role_name(&schema).expect("test app role name");

    assert!(
        relation_exists(&conn, &audit).await,
        "a successful apply must leave {audit} in place"
    );

    let lit = |s: &str| s.replace('\'', "''");
    assert!(
        probe_bool(
            &conn,
            &format!(
                "SELECT has_table_privilege('{}', '{}', 'INSERT')",
                lit(&runtime_role),
                lit(&audit)
            ),
        )
        .await,
        "the runtime role must hold INSERT on {audit}"
    );
    assert!(
        probe_bool(
            &conn,
            &format!(
                "SELECT has_sequence_privilege('{}', pg_get_serial_sequence('{}', 'id'), 'USAGE')",
                lit(&runtime_role),
                lit(&audit)
            ),
        )
        .await,
        "the runtime role must hold USAGE on the BIGSERIAL sequence behind {audit}"
    );

    let write = as_app_runtime_identity(
        &conn,
        &schema,
        &format!(
            "INSERT INTO {audit} (collection, row_pk, \"column\", classification, outcome) \
             VALUES ('notes', 'row-1', 'body', 'pii', 'granted')"
        ),
    )
    .await;
    assert!(
        write.is_ok(),
        "the worker must be able to write an unmask audit row after apply: {write:?}"
    );

    let rows: i64 = conn
        .query(&format!("SELECT count(*)::int8 FROM {audit}"), &[])
        .await
        .expect("count audit rows")[0]
        .get(0);
    assert_eq!(rows, 1, "the audit row must be readable after the write");

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn apply_api_accepts_apps_migrate_owner_and_applies_ir_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
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
        table_exists(&conn, &app_derivation::schema_name(&app_id), "notes").await,
        "notes table must exist in the per-app schema"
    );
    assert!(
        !table_exists(&conn, "public", "notes").await,
        "Confined apply must not create the table in public"
    );
    assert!(
        journaled_count(&conn, &app_id).await >= 1,
        "IR apply must be journaled in the app's own schema under the platform prefix"
    );

    // WHERE THE JOURNAL IS, asserted both ways. The positive above passes on a
    // build that puts the journal anywhere reachable by that one name; these two
    // pin the placement itself.
    //
    // The unfenced spelling matters more than the old meta schema: the engine
    // bootstraps with `CREATE TABLE IF NOT EXISTS` and its table names are
    // literals, so if the prefix were dropped a creator declaring a table called
    // `schema_migrations` would have it silently adopted as the journal. This
    // asserts nothing occupies that name.
    assert!(
        !relation_exists(
            &conn,
            &format!(
                "\"{}_migrations\".__zeroship_schema_migrations",
                app_id.as_str()
            )
        )
        .await,
        "the separate <app>_migrations meta schema must be gone"
    );
    assert!(
        !relation_exists(
            &conn,
            &format!(
                "\"{}\".schema_migrations",
                app_derivation::schema_name(&app_id)
            )
        )
        .await,
        "the journal must NOT occupy the unfenced name a creator could declare"
    );

    // The row the platform keeps for itself, in the platform's own schema.
    let descriptors = applied_descriptors(&conn, &app_id).await;
    assert_eq!(
        descriptors.len(),
        1,
        "one apply request must record exactly one applied ledger row: {descriptors:?}"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// Two project ids that collide in PostgreSQL's 32-bit `hashtext` space still
/// own independent project locks.
///
/// The pair is fixed output from a 200,000-row live-PostgreSQL search over
/// `md5(generate_series)::uuid`. The fixture assertion keeps the regression
/// honest if PostgreSQL ever changes `hashtext`; the backend calls below, not a
/// reimplementation in the test, decide whether the two ids contend.
#[compio::test]
async fn hashtext_colliding_project_ids_take_independent_project_locks_pg() {
    const FIRST: &str = "84d98912-251e-c042-018c-bc5935cf3cb4";
    const SECOND: &str = "fb9bcb0b-0d64-0942-127a-d89c71893675";

    let holder = CompioPgSession::connect(&dsn())
        .await
        .expect("connect the project-lock holder");
    let contender = CompioPgSession::connect(&dsn())
        .await
        .expect("connect the independent project-lock contender");

    let first = FIRST.to_string();
    let second = SECOND.to_string();
    let collision = holder
        .client()
        .query_one(
            "SELECT hashtext($1) AS first_hash, hashtext($2) AS second_hash",
            &[&first, &second],
        )
        .await
        .expect("verify the fixed hashtext collision");
    let first_hash = collision.get::<_, i32>("first_hash");
    let second_hash = collision.get::<_, i32>("second_hash");
    assert_ne!(FIRST, SECOND, "the fixture must contain two distinct ids");
    assert_eq!(
        first_hash, second_hash,
        "the fixed fixture no longer collides under PostgreSQL hashtext"
    );

    let policy = effective_policy_from_charter_toml(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#,
    )
    .expect("compose the project-lock test policy");
    let first_cfg = ExecutorConfig::new(FIRST, "unused_first_schema", policy.clone());
    let second_cfg = ExecutorConfig::new(SECOND, "unused_second_schema", policy);
    let first_backend = PostgresBackend::new_generic(&holder);
    let second_backend = PostgresBackend::new_generic(&contender);

    first_backend
        .acquire_project_lock(&first_cfg)
        .await
        .expect("take FIRST's project lock");
    let second_outcome = second_backend
        .try_acquire_project_lock(&second_cfg)
        .await
        .expect("probe SECOND's project lock");
    let second_acquired = matches!(&second_outcome, ProjectLockAcquisition::Acquired);
    if second_acquired {
        second_backend
            .release_project_lock(&second_cfg)
            .await
            .expect("release SECOND's project lock");
    }
    first_backend
        .release_project_lock(&first_cfg)
        .await
        .expect("release FIRST's project lock");

    assert!(
        second_acquired,
        "distinct ids with hashtext={first_hash} contended on one project lock: {second_outcome:?}"
    );
}

/// A bundle owns one project advisory lock through its terminal ledger write.
///
/// The first blocker holds `ACCESS SHARE` on the table the final file alters.
/// That permits preparation, attestation, the submitted ledger insert, and the
/// earlier files to finish, then parks the final file at its
/// `AccessExclusiveLock` request. While it is parked, a second connection locks
/// that request's submitted ledger row. Once the final file is released, the
/// apply reaches `mark_applied` and waits on the row.
/// Probing the project advisory key at both waits proves one lock spans every
/// file and the terminal timestamp that decides which descriptor is newest.
#[compio::test]
async fn project_lock_spans_every_file_and_the_terminal_ledger_write_pg() {
    let conn = admin_conn().await;
    let ledger_conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (create_state, create_tmp) = state_for(auth);
    let create_service = test::init_service(
        web::App::new()
            .state(create_state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&create_service, &app_id, "good-token").await;

    let tmp = tmpdir("project-lock-span");
    let policy_config = ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
        .expect("test policy config");
    let schema_apply_store = SchemaApplyStore::new(dsn());
    let initial: ApplyMigrationsRequest =
        serde_json::from_value(create_notes_request()).expect("deserialize initial apply request");
    apply_ir_documents(
        &dsn(),
        &tmp,
        &app_id,
        &initial,
        &policy_config,
        &schema_apply_store,
        &owner_id,
    )
    .await
    .expect("create the table the final file will alter");

    let notes = format!(
        "{}.{}",
        quote_ident(&app_derivation::schema_name(&app_id)),
        quote_ident("notes")
    );
    conn.batch_execute(&format!("BEGIN; LOCK TABLE {notes} IN ACCESS SHARE MODE;"))
        .await
        .expect("hold the final-file table lock");

    let apply_dsn = dsn();
    let apply_tmp = tmp.clone();
    let apply_app_id = app_id.clone();
    let request: ApplyMigrationsRequest =
        serde_json::from_value(lock_span_request()).expect("deserialize lock-span apply request");
    let task_owner_id = owner_id.clone();
    let apply_task = compio::runtime::spawn(async move {
        let policy_config = ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
            .expect("test policy config");
        let schema_apply_store = SchemaApplyStore::new(apply_dsn.clone());
        apply_ir_documents(
            &apply_dsn,
            &apply_tmp,
            &apply_app_id,
            &request,
            &policy_config,
            &schema_apply_store,
            &task_owner_id,
        )
        .await
    });

    let mut reached_final_file = false;
    for _ in 0..500 {
        reached_final_file = conn
            .query_one(
                "SELECT EXISTS ( \
                     SELECT 1 FROM pg_locks \
                      WHERE locktype = 'relation' \
                        AND relation = to_regclass($1::text) \
                        AND mode = 'AccessExclusiveLock' AND NOT granted \
                 )",
                &[&notes],
            )
            .await
            .expect("observe the final file waiting on its table lock")
            .get(0);
        if reached_final_file {
            break;
        }
        compio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_file_contender_acquired = if reached_final_file {
        conn.query_one(
            "SELECT pg_try_advisory_lock( \
                    (h >> 32)::int4, ((h << 32) >> 32)::int4 \
               ) FROM (SELECT hashtextextended($1, 0) AS h) AS project_lock_key",
            &[&app_derivation::schema_name(&app_id)],
        )
        .await
        .expect("probe the apply's project advisory lock")
        .get::<_, bool>(0)
    } else {
        false
    };
    if final_file_contender_acquired {
        conn.execute(
            "SELECT pg_advisory_unlock( \
                    (h >> 32)::int4, ((h << 32) >> 32)::int4 \
               ) FROM (SELECT hashtextextended($1, 0) AS h) AS project_lock_key",
            &[&app_derivation::schema_name(&app_id)],
        )
        .await
        .expect("release unexpectedly acquired project lock");
    }

    let (ledger_backend_pid, held_submitted_row) = if reached_final_file {
        ledger_conn
            .batch_execute("BEGIN")
            .await
            .expect("begin the terminal-ledger blocker");
        let rows = ledger_conn
            .query(
                "SELECT migration_id FROM zeroship.app_schema_applies \
                  WHERE app_id = $1::text AND status = 'submitted' \
                  ORDER BY submitted_at DESC, migration_id DESC \
                  FOR UPDATE",
                &[&app_id.as_str()],
            )
            .await
            .expect("lock the in-flight apply's submitted ledger row");
        let pid = ledger_conn
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("read the terminal-ledger blocker's backend pid")
            .get::<_, i32>(0);
        (Some(pid), rows.len() == 1)
    } else {
        (None, false)
    };

    conn.batch_execute("ROLLBACK")
        .await
        .expect("release the final-file table lock");

    let mut reached_terminal_write = false;
    if held_submitted_row {
        let blocker_pid = ledger_backend_pid.expect("a held row has a blocker backend");
        for _ in 0..500 {
            reached_terminal_write = conn
                .query_one(
                    "SELECT EXISTS ( \
                         SELECT 1 FROM pg_stat_activity AS activity \
                          WHERE $1::int = ANY(pg_blocking_pids(activity.pid)) \
                            AND activity.wait_event_type = 'Lock' \
                            AND strpos( \
                                  activity.query, \
                                  'UPDATE zeroship.app_schema_applies' \
                                ) > 0 \
                     )",
                    &[&blocker_pid],
                )
                .await
                .expect("observe mark_applied waiting on the submitted row")
                .get(0);
            if reached_terminal_write {
                break;
            }
            compio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    let terminal_contender_acquired = if reached_terminal_write {
        conn.query_one(
            "SELECT pg_try_advisory_lock( \
                    (h >> 32)::int4, ((h << 32) >> 32)::int4 \
               ) FROM (SELECT hashtextextended($1, 0) AS h) AS project_lock_key",
            &[&app_derivation::schema_name(&app_id)],
        )
        .await
        .expect("probe the project lock while mark_applied is blocked")
        .get::<_, bool>(0)
    } else {
        false
    };
    if terminal_contender_acquired {
        conn.execute(
            "SELECT pg_advisory_unlock( \
                    (h >> 32)::int4, ((h << 32) >> 32)::int4 \
               ) FROM (SELECT hashtextextended($1, 0) AS h) AS project_lock_key",
            &[&app_derivation::schema_name(&app_id)],
        )
        .await
        .expect("release unexpectedly acquired terminal project lock");
    }
    if ledger_backend_pid.is_some() {
        ledger_conn
            .batch_execute("ROLLBACK")
            .await
            .expect("release the terminal-ledger row");
    }

    let apply_result = apply_task.await.expect("join the lock-span apply");

    let final_file_column_applied = conn
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM information_schema.columns \
                  WHERE table_schema = $1 \
                    AND table_name = 'notes' \
                    AND column_name = 'lock_span_file_two' \
             )",
            &[&app_derivation::schema_name(&app_id)],
        )
        .await
        .expect("observe the final file's completed schema effect")
        .get::<_, bool>(0);

    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(create_tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert!(
        reached_final_file,
        "the apply never reached the blocked DDL in its final file: {apply_result:?}"
    );
    assert!(
        !final_file_contender_acquired,
        "the project advisory lock was free while the final file was executing"
    );
    assert!(
        held_submitted_row,
        "the blocked apply did not expose exactly one submitted ledger row"
    );
    assert!(
        reached_terminal_write,
        "mark_applied never waited on the submitted ledger row: {apply_result:?}"
    );
    assert!(
        !terminal_contender_acquired,
        "the project advisory lock was free while mark_applied was waiting"
    );
    let outcome = apply_result.expect("lock-span apply succeeds after the blocker releases");
    assert!(
        final_file_column_applied,
        "the final file did not apply its column after the blocker released: {outcome:?}"
    );
}

/// A DESTRUCTIVE migration is no longer parked for an operator who does not exist.
///
/// THIS IS A CAPABILITY REMOVAL, PINNED SO IT CANNOT COME BACK BY ACCIDENT. Until
/// 2026-08-28 a `dropTable` preflighted as gated, the request answered
/// **409 `migration_requires_operator_approval`** naming a `migration_id`, and the
/// only way past it was `POST .../migrations/{id}/approve` - a route no dashboard,
/// CLI or service ever called. The creator's own destructive migration was
/// therefore a dead end.
///
/// WHAT THIS DOES NOT ASSERT, AND MUST NOT BE READ AS. The drop still does not
/// apply. Measured 2026-08-28 against live PostgreSQL 16: the request answers
/// **422** with *"apply (0002_drop_notes.ir.json): plan requires approval
/// (destructive) but none was given"* - the ENGINE's refusal, because this host
/// passes `Approval::None`. Removing the host's approval state machine removed a
/// refusal nobody could clear; it did not grant creators destructive migrations,
/// and giving them one is a separate decision about what `Approval` the host
/// asserts on a creator's behalf.
///
/// So this case asserts the ABSENCE of the unclearable refusal, not any
/// particular success. Asserting `status == OK` would make it a statement about
/// the confined ceiling's `safety.destructive_ops` value, which is not what
/// changed.
#[ntex::test]
async fn a_destructive_migration_is_not_parked_for_operator_approval_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
            .header("authorization", "Bearer good-token")
            .set_json(&body)
            .to_request()
    };

    // The table has to exist before dropping it is destructive rather than absurd.
    let resp = test::call_service(&svc, post(create_notes_request())).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = test::call_service(&svc, post(drop_notes_request())).await;
    let status = resp.status();
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_ne!(
        status,
        StatusCode::CONFLICT,
        "a destructive migration must not be parked awaiting an approval endpoint \
         that no longer exists: {body}"
    );
    assert_ne!(
        body["error"].as_str(),
        Some("migration_requires_operator_approval"),
        "the approval refusal must be unreachable: {body}"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// WHAT ACTUALLY STOPS A CREATOR MIGRATION THAT NAMES THE PLATFORM JOURNAL.
///
/// The exact refusal matters. A generic 4xx assertion cannot distinguish the
/// authoring fence from the independent apply-time and destructive guards:
///
/// * `createTable "__zeroship_schema_migrations"` -> the declarative load gate's
///   reserved-prefix refusal, before SQL is emitted. Provision-before-apply and
///   journal bootstrap ordering still prevent silent adoption if an unchecked
///   artifact reaches execution, but this HTTP path should not need that fallback.
/// * `dropTable "__zeroship_schema_migrations"` -> *"plan requires approval
///   (destructive) but none was given"*. That is the ENGINE's destructive gate,
///   reached because this host passes `Approval::None`. **A change that made the
///   host assert approval on the creator's behalf would hand them their own
///   journal**, and this assertion is the tripwire for it.
///
/// The prefix still moves the journal off a plausible accidental name, while the
/// authoring validator now closes the deliberate `createTable` case early.
#[ntex::test]
async fn what_refuses_a_creator_migration_that_names_the_platform_journal_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
            .header("authorization", "Bearer good-token")
            .set_json(&body)
            .to_request()
    };

    // A real migration first, so the journal exists to be attacked and the app
    // schema exists to be measured.
    let resp = test::call_service(&svc, post(create_notes_request())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let journaled_before = journaled_count(&conn, &app_id).await;
    assert!(
        journaled_before >= 1,
        "the journal must exist to be attacked"
    );

    // THE FIXTURE HAS ALREADY FAILED TWICE HERE, both times passing the case while
    // measuring itself: once with `"table":` on a `createTable` (refused as a
    // malformed IR envelope) and once with a column called `version` (refused as
    // colliding with an injected system column). Both are 4xx with a `detail`
    // that reads like a name refusal. The `expect` on each arm is what makes the
    // difference visible - a fixture fault produces a different string and the
    // case goes red instead of green.
    for (label, request, expect) in [
        (
            "createTable",
            create_platform_journal_table_request(),
            "reserved prefix '__zeroship'",
        ),
        (
            "dropTable",
            drop_platform_journal_request(),
            "requires approval (destructive)",
        ),
    ] {
        let resp = test::call_service(&svc, post(request)).await;
        let status = resp.status();
        let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label} on the platform journal answered {status}: {body}"
        );
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains(expect),
            "{label} was refused, but not by {expect:?} - the thing standing \
             between a creator and the platform journal has CHANGED, and the doc \
             comment above is now wrong: {body}"
        );
        assert!(
            relation_exists(
                &conn,
                &format!(
                    "\"{}\".__zeroship_schema_migrations",
                    app_derivation::schema_name(&app_id)
                )
            )
            .await,
            "the journal must still exist after a refused {label}"
        );
    }
    assert_eq!(
        journaled_count(&conn, &app_id).await,
        journaled_before,
        "a refused migration must not have journalled anything"
    );

    // ARM 2: AN APP THAT HAS NEVER MIGRATED. The refusal must come from the name
    // gate rather than from colliding with a journal an earlier request created.
    let fresh_id = AppId::mint();
    let fresh_owner = UserId::mint();
    seed_app(&conn, &fresh_id, &fresh_owner).await;
    let fresh_auth = Arc::new(StaticAuthenticator::new());
    fresh_auth.insert(
        "good-token",
        &fresh_owner,
        [Scope::AppsDeploy],
        [fresh_id.clone()],
    );
    let (fresh_state, fresh_tmp) = state_for(fresh_auth);
    let fresh_svc = test::init_service(
        web::App::new()
            .state(fresh_state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&fresh_svc, &fresh_id, "good-token").await;
    assert!(
        !relation_exists(
            &conn,
            &format!(
                "\"{}\".__zeroship_schema_migrations",
                app_derivation::schema_name(&fresh_id)
            )
        )
        .await,
        "this arm only means anything against an app with NO journal yet"
    );
    let resp = test::call_service(
        &fresh_svc,
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{}/migrations/apply", fresh_id.as_str()))
            .header("authorization", "Bearer good-token")
            .set_json(&create_platform_journal_table_request())
            .to_request(),
    )
    .await;
    let status = resp.status();
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an app's FIRST migration shadowed the journal and was accepted ({status}): {body}"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("reserved prefix '__zeroship'"),
        "the first-migration case did not reach the authoring name fence: {body}"
    );
    let _ = std::fs::remove_dir_all(fresh_tmp);
    cleanup_app(&conn, &fresh_id).await;
    cleanup_user(&conn, &fresh_owner).await;

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// Every applied `(descriptor_sha256, applied_at)` for an app, newest last.
async fn applied_descriptors(conn: &Client, app_id: &AppId) -> Vec<Option<String>> {
    conn.query(
        "SELECT descriptor_sha256 FROM zeroship.app_schema_applies \
          WHERE app_id = $1::text AND status = 'applied' \
          ORDER BY applied_at ASC, submitted_at ASC, migration_id ASC",
        &[&app_id.as_str()],
    )
    .await
    .expect("query applied descriptors")
    .iter()
    .map(|row| row.get::<_, Option<String>>("descriptor_sha256"))
    .collect()
}

/// Every applied row's `applied_versions`, newest last - the engine's own
/// `outcome.applied` for that request.
async fn applied_versions(conn: &Client, app_id: &AppId) -> Vec<Value> {
    conn.query(
        "SELECT applied_versions FROM zeroship.app_schema_applies \
          WHERE app_id = $1::text AND status = 'applied' \
          ORDER BY applied_at ASC, submitted_at ASC, migration_id ASC",
        &[&app_id.as_str()],
    )
    .await
    .expect("query applied versions")
    .iter()
    .map(|row| row.get::<_, Value>("applied_versions"))
    .collect()
}

/// A failure after the engine commits creator DDL must still close the control
/// plane's apply ledger row.
///
/// An existing `FOR ALL TABLES` publication makes the post-apply reconciliation
/// issue an `ALTER PUBLICATION ... SET TABLE` that PostgreSQL refuses. The notes
/// table therefore proves the engine commit happened before the failure; the
/// ledger assertion proves that failure did not strand the request as submitted.
#[ntex::test]
async fn a_post_ddl_failure_closes_the_schema_apply_ledger_row_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let publication = zeroship_core::replication_names::publication_name(app_id.as_str())
        .expect("app id is a valid publication seed");
    conn.batch_execute(&format!(
        "CREATE PUBLICATION {} FOR ALL TABLES",
        quote_ident(&publication)
    ))
    .await
    .expect("create the incompatible publication fixture");

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    let response_status = resp.status();
    let response_body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let ddl_committed = table_exists(&conn, &app_derivation::schema_name(&app_id), "notes").await;
    let ledger = conn
        .query(
            "SELECT status, last_error FROM zeroship.app_schema_applies \
              WHERE app_id = $1::text ORDER BY submitted_at ASC, migration_id ASC",
            &[&app_id.as_str()],
        )
        .await
        .expect("query the failed apply ledger row")
        .iter()
        .map(|row| {
            (
                row.get::<_, String>("status"),
                row.get::<_, Option<String>>("last_error"),
            )
        })
        .collect::<Vec<_>>();

    // Teardown precedes assertions so the deliberate red run does not leave a
    // global publication or per-app roles behind in the dedicated test server.
    conn.batch_execute(&format!(
        "DROP PUBLICATION IF EXISTS {}",
        quote_ident(&publication)
    ))
    .await
    .expect("drop the incompatible publication fixture");
    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert_eq!(
        response_status,
        StatusCode::SERVICE_UNAVAILABLE,
        "the publication failure must reach the API: {response_body}"
    );
    assert!(
        ddl_committed,
        "the notes table must be committed before publication reconciliation fails"
    );
    assert_eq!(
        ledger.len(),
        1,
        "one request must create exactly one ledger row: {ledger:?}"
    );
    assert_eq!(
        ledger[0].0, "failed",
        "a surviving process must close the post-DDL failure as terminal: {ledger:?}"
    );
    assert!(
        ledger[0]
            .1
            .as_deref()
            .is_some_and(|error| error.contains("publication")),
        "the terminal row must retain the post-DDL failure: {ledger:?}"
    );
}

/// A creator must not move the deploy ledger head behind the database journal.
///
/// The first request supplies and applies 1..N. The second request is the old
/// 1..K artifact: every migration it carries is already journaled, so the engine
/// applies nothing. Before the server attested full-set journal coverage, that
/// empty apply returned 200 and stamped the truncated descriptor as the newest
/// applied row, which made the deploy guard admit old code over the newer schema.
#[ntex::test]
async fn a_truncated_history_cannot_move_the_schema_ledger_backwards_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;
    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
            .header("authorization", "Bearer good-token")
            .set_json(&body)
            .to_request()
    };

    let first_resp = test::call_service(&svc, post(complete_notes_history_request())).await;
    assert_eq!(first_resp.status(), StatusCode::OK);
    let first: Value =
        serde_json::from_slice(&test::read_body(first_resp).await).expect("first JSON body");
    assert!(
        first["applied"]
            .as_array()
            .is_some_and(|versions| !versions.is_empty()),
        "the complete history must advance the real journal: {first}"
    );
    let missing_version = first["applied"]
        .as_array()
        .and_then(|versions| versions.last())
        .and_then(Value::as_str)
        .expect("the final applied version belongs to the 0002 document")
        .to_string();

    let rollback_resp = test::call_service(&svc, post(create_notes_request())).await;
    let rollback_status = rollback_resp.status();
    let rollback_body: Value =
        serde_json::from_slice(&test::read_body(rollback_resp).await).expect("rollback JSON body");
    let ledger: Vec<(String, Option<String>, Value)> = conn
        .query(
            "SELECT status, descriptor_sha256, applied_versions \
               FROM zeroship.app_schema_applies \
              WHERE app_id = $1::text \
              ORDER BY submitted_at ASC, migration_id ASC",
            &[&app_id.as_str()],
        )
        .await
        .expect("read every ledger row for the rollback attempt")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert_eq!(
        rollback_status,
        StatusCode::CONFLICT,
        "a truncated history must be refused; response={rollback_body}, ledger={ledger:?}"
    );
    assert_eq!(rollback_body["error"], "migration_history_incomplete");
    assert!(
        rollback_body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains(&missing_version)),
        "the refusal must name the omitted journaled version {missing_version}: {rollback_body}"
    );
    assert_eq!(
        ledger,
        vec![(
            "applied".to_string(),
            Some(TEST_DESCRIPTOR_SHA256_NEXT.to_string()),
            first["applied"].clone(),
        )],
        "the refusal must happen before any row for the truncated descriptor is written"
    );
}

/// THE LEDGER ROW IS PER APPLY REQUEST, NOT PER APPLIED MIGRATION - and that is
/// a REQUIREMENT, not an observation about where the insert happens to sit.
///
/// The control plane's deploy precondition compares the artifact's descriptor
/// against the hash on the app's NEWEST APPLIED row. So when a build emits
/// different descriptor bytes for an unchanged migration set - an engine
/// upgrade, a codegen fix - the only thing that can move the app forward is a
/// migrate run that applies NOTHING and still records the new hash.
///
/// "Skip the ledger write when nothing applied" is the natural optimisation
/// here and it would brick every such app's next deploy permanently, with no
/// creator-reachable remedy. This case is what makes that optimisation fail
/// loudly instead: the second apply below journals zero versions.
#[ntex::test]
async fn a_re_apply_that_applies_nothing_still_records_the_new_descriptor_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
            .header("authorization", "Bearer good-token")
            .set_json(&body)
            .to_request()
    };

    let resp = test::call_service(&svc, post(create_notes_request())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let first: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert!(
        !first["applied"].as_array().unwrap().is_empty(),
        "the first apply must actually apply something, or the second one is not a re-apply: {first}"
    );

    // THE SAME DOCUMENTS, A DIFFERENT DESCRIPTOR. This is the engine-upgrade
    // shape: the migration set has not changed, so the engine has nothing to
    // do, but the descriptor those documents fold to has different bytes.
    let mut second_request = create_notes_request();
    second_request["descriptor_sha256"] = json!(TEST_DESCRIPTOR_SHA256_NEXT);
    let resp = test::call_service(&svc, post(second_request)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let second: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert!(
        second["applied"].as_array().unwrap().is_empty(),
        "the second apply must apply NOTHING - otherwise this case is not about \
         the empty-apply path at all: {second}"
    );

    assert_eq!(
        applied_descriptors(&conn, &app_id).await,
        vec![
            Some(TEST_DESCRIPTOR_SHA256.to_string()),
            Some(TEST_DESCRIPTOR_SHA256_NEXT.to_string()),
        ],
        "an apply request that journals nothing must STILL record its descriptor \
         as the newest applied row - without it the app can never deploy again",
    );

    // AND THE ROW SAYS SO. `descriptor_sha256` alone cannot tell a request that
    // advanced the schema from one that advanced nothing, which is exactly the
    // distinction the rollback hole turns on: a creator re-submitting an OLD IR
    // moves the ledger head backwards and the row looks identical to a real
    // apply. `applied_versions` is the engine's own `outcome.applied` for the
    // request, so the empty second row is visible as empty.
    let versions = applied_versions(&conn, &app_id).await;
    assert_eq!(versions.len(), 2, "two requests, two rows: {versions:?}");
    assert!(
        versions[0].as_array().is_some_and(|a| !a.is_empty()),
        "the first request applied migrations and must record them: {versions:?}"
    );
    assert_eq!(
        versions[1],
        json!([]),
        "the second request advanced nothing and the row must show it: {versions:?}"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn apply_api_5xx_detail_is_generic_and_does_not_leak_internals() {
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let bad_provision_dsn =
        "host=127.0.0.1 port=1 user=postgres password=zeroship dbname=zeroship_control_test"
            .to_string();
    let (state, tmp) = state_for_with_dsns(auth, bad_provision_dsn, dsn());
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
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
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "no-scope-token",
        &owner_id,
        [Scope::AppsRead],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer no-scope-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn apply_api_rejects_bearer_for_different_app() {
    let app_id = AppId::mint();
    let other_app = AppId::mint();
    let owner_id = UserId::mint();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "wrong-app-token",
        &owner_id,
        [Scope::AppsDeploy],
        [other_app.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
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
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
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
        .query(
            "SELECT 1 FROM pg_roles WHERE rolname = 'zs_migrated_forbidden'",
            &[],
        )
        .await
        .expect("query pg_roles");
    assert!(role_rows.is_empty(), "guard-denied createRole must not run");

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// A `.ir.json` body the creator wrote wrong is the CREATOR's fault, not the
/// service's.
///
/// `validate_request_shape` only checks that each document is a JSON object, so
/// `{"foo": 1}` passes it and fails later at the envelope parse. That parse failure
/// used to become `IrApplyError::Read`, which classifies as 503
/// `migration_infrastructure` - the same variant a genuine disk read failure produces.
/// The 5xx arm of `apply_error_response` then logs at ERROR and REPLACES the detail
/// with "migration service unavailable", so the creator learned neither what was wrong
/// nor which file, an operator got paged for a typo, and any client retrying on 5xx
/// retried a request that can never succeed.
///
/// The suppression of detail on 5xx is correct and stays; what was wrong is calling
/// this a 5xx.
#[ntex::test]
async fn apply_api_reports_malformed_ir_as_creator_fault_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    create_database(&svc, &app_id, "good-token").await;

    // A JSON object, so it clears validate_request_shape, but not an IR envelope.
    let malformed = json!({
        "kind": "ir",
        "descriptor_sha256": TEST_DESCRIPTOR_SHA256,
        "documents": [{
            "filename": "0001_broken.ir.json",
            "body": {"foo": 1}
        }]
    });

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&malformed)
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "creator-authored IR that does not parse is a 422, not a 503"
    );
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("0001_broken.ir.json"),
        "the creator must be told WHICH file failed, got: {body}"
    );

    // POSITIVE CONTROL for the classification, not for the request path: a well-formed
    // request against this same fixture succeeds. Without it, "422" is also what a
    // service that rejected everything would return.
    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a well-formed migration must still apply"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn apply_api_rejects_policy_draft_escalation_without_clamping() {
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&with_policy(create_notes_request(), escalating_policy()))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_policy_invalid");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("sql.raw") && detail.contains("GrantExceedsCharter"),
        "draft escalation should be rejected explicitly, got: {body}"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn apply_api_rejects_malformed_policy_draft_fail_closed() {
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&with_policy(create_notes_request(), malformed_policy()))
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

/// The direct edge route must retain the shared, PostgreSQL-backed mutation
/// bucket that the deleted control forward used to apply. Two tokens are the
/// configured burst; the third request from the same trusted source is refused,
/// while a different source still owns an independent bucket.
#[ntex::test]
async fn apply_api_rate_limits_each_source_ip_across_the_shared_store_pg() {
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let bucket_keys = [
        "migrate:mutation:ip:203.0.113.41",
        "migrate:mutation:ip:203.0.113.42",
    ];
    let cleanup = admin_conn().await;
    for key in bucket_keys {
        cleanup
            .execute(
                "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
                &[&key],
            )
            .await
            .expect("clear mutation rate-limit fixture");
    }
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (first_state, first_tmp) = state_for_trusted_proxy(auth.clone()).await;
    let (second_state, second_tmp) = state_for_trusted_proxy(auth).await;
    let first_service = test::init_service(
        web::App::new()
            .state(first_state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    let second_service = test::init_service(
        web::App::new()
            .state(second_state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let invalid_body = with_policy(create_notes_request(), malformed_policy());
    let post = |source_ip: &str| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
            .header("authorization", "Bearer good-token")
            .header("x-forwarded-for", format!("198.51.100.1, {source_ip}"))
            .set_json(&invalid_body)
            .to_request()
    };

    let mut statuses = Vec::new();
    for _ in 0..2 {
        let response = test::call_service(&first_service, post("203.0.113.41")).await;
        statuses.push(response.status());
        let _ = test::read_body(response).await;
    }
    let third = test::call_service(&second_service, post("203.0.113.41")).await;
    let third_retry_after = third
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    statuses.push(third.status());
    let _ = test::read_body(third).await;
    let other_source = test::call_service(&second_service, post("203.0.113.42")).await;
    let other_status = other_source.status();
    let _ = test::read_body(other_source).await;

    assert_eq!(
        statuses,
        [
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::TOO_MANY_REQUESTS,
        ],
        "the third same-IP mutation must be throttled after the burst"
    );
    assert!(
        third_retry_after.is_some(),
        "a throttled caller needs a Retry-After bound"
    );
    assert_eq!(
        other_status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a different source IP must not inherit the exhausted bucket"
    );

    for key in bucket_keys {
        cleanup
            .execute(
                "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
                &[&key],
            )
            .await
            .expect("remove mutation rate-limit fixture");
    }
    let _ = std::fs::remove_dir_all(first_tmp);
    let _ = std::fs::remove_dir_all(second_tmp);
}

/// Rollback is not implemented yet, but it is already a mutating route. Bind
/// the security gate before Phase 2 replaces the stub so that implementation
/// cannot accidentally publish an unthrottled DDL path.
#[ntex::test]
async fn rollback_route_passes_through_the_mutation_rate_limiter() {
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for_with_policy_config_and_edge(
        auth,
        dsn(),
        dsn(),
        ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
            .expect("test policy config"),
        Arc::new(ThrottlingMutationRateLimiter),
        false,
    );
    let service = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let request = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/rollback", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .to_request();
    let response = test::call_service(&service, request).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("20")
    );

    let _ = std::fs::remove_dir_all(tmp);
}

/// The CLI posts the generated IR envelope verbatim. Pin the configured ingress
/// contract so Ntex's extractor default cannot reject a real migration bundle
/// before authentication and throttling run.
#[ntex::test]
async fn apply_route_accepts_a_body_larger_than_ntexs_default_limit() {
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for_with_policy_config_and_edge(
        auth,
        dsn(),
        dsn(),
        ManagedPolicyConfig::default_confined(TEST_POLICY_SEAL_KEY.to_vec(), 1)
            .expect("test policy config"),
        Arc::new(ThrottlingMutationRateLimiter),
        false,
    );
    let service = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    let mut body = create_notes_request();
    body["policy"] = json!({
        "filename": MIGRATE_POLICY_FILENAME,
        "body": "#".repeat(40_000),
    });

    let request = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .set_json(&body)
        .to_request();
    let response = test::call_service(&service, request).await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a body below APPLY_REQUEST_BODY_BYTES must reach the handler"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

#[ntex::test]
async fn real_delegating_authenticator_accepts_apps_deploy_owner_bearer() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let token = platform_token(&owner_id, "apps:deploy");

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);
    let caller = authenticator
        .verify_bearer(&token, &app_id, Scope::AppsDeploy, "test-request-id")
        .await
        .expect("app owner holding apps:deploy verifies");
    assert_eq!(caller.principal_id, owner_id);

    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// The migration service is now a platform CLI bearer's first possible
/// authenticated destination. It must materialize the default rows itself so
/// an operator can narrow the same token immediately and permanently.
#[ntex::test]
async fn first_cli_apply_auth_materializes_defaults_and_honors_later_narrowing_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let scope = PLATFORM_CLI_ISSUABLE_SCOPES.join(" ");
    let token = platform_token_for_client(&owner_id, PLATFORM_CLI_CLIENT_ID, &scope);
    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);

    authenticator
        .verify_bearer(&token, &app_id, Scope::AppsDeploy, "first-cli-apply")
        .await
        .expect("the unseeded CLI fallback authorizes its first apply");

    let marker_count: i64 = conn
        .query_one(
            "SELECT COUNT(*) FROM zeroship.identity_links \
             WHERE principal_id = $1 AND provider = 'platform'",
            &[&owner_id.as_str()],
        )
        .await
        .expect("query platform identity marker")
        .get(0);
    let stored_grants = conn
        .query(
            "SELECT grant_name FROM zeroship.principal_grants \
             WHERE principal_id = $1 ORDER BY grant_name",
            &[&owner_id.as_str()],
        )
        .await
        .expect("query materialized CLI grants")
        .iter()
        .map(|row| row.get::<_, String>("grant_name"))
        .collect::<Vec<_>>();
    assert_eq!(
        marker_count, 1,
        "the platform identity marker was not materialized"
    );
    assert_eq!(
        stored_grants,
        PLATFORM_CLI_ISSUABLE_SCOPES.map(str::to_owned),
        "the exact default CLI grant set was not materialized"
    );

    conn.execute(
        "DELETE FROM zeroship.principal_grants \
         WHERE principal_id = $1 AND grant_name = 'apps:deploy'",
        &[&owner_id.as_str()],
    )
    .await
    .expect("operator narrows apps:deploy");
    let error = authenticator
        .verify_bearer(&token, &app_id, Scope::AppsDeploy, "narrowed-cli-apply")
        .await
        .expect_err("the already-issued bearer must observe live narrowing");
    assert!(matches!(error, AuthError::Forbidden));
    let deploy_grants: i64 = conn
        .query_one(
            "SELECT COUNT(*) FROM zeroship.principal_grants \
             WHERE principal_id = $1 AND grant_name = 'apps:deploy'",
            &[&owner_id.as_str()],
        )
        .await
        .expect("query narrowed grant")
        .get(0);
    assert_eq!(
        deploy_grants, 0,
        "a later request re-seeded an operator revocation"
    );

    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn real_delegating_authenticator_rejects_bearer_without_apps_deploy_scope() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    // The principal OWNS the app; only the scope is short. Cedar would allow an
    // owner `apps:deploy`, so the denial can only come from the token's own
    // scope-derived policy.
    let token = platform_token(&owner_id, "apps:read");

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);
    let err = authenticator
        .verify_bearer(&token, &app_id, Scope::AppsDeploy, "test-request-id")
        .await
        .expect_err("bearer without apps:deploy must be denied");
    assert!(matches!(err, AuthError::Forbidden));

    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// A scope lowers to `Resource::Any`, so the token itself names no app: the
/// per-app narrowing is entirely Cedar app membership. This is the test that
/// proves that membership actually bites - the same bearer that verifies for
/// the app its subject owns must be refused for an app it does not.
#[ntex::test]
async fn real_delegating_authenticator_rejects_bearer_for_different_app_owner() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let other_app_id = AppId::mint();
    let other_owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;
    seed_app(&conn, &other_app_id, &other_owner_id).await;

    let token = platform_token(&owner_id, "apps:deploy");

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);
    let caller = authenticator
        .verify_bearer(&token, &app_id, Scope::AppsDeploy, "test-request-id")
        .await
        .expect("the same bearer verifies for the app its subject owns");
    assert_eq!(caller.principal_id, owner_id);

    let err = authenticator
        .verify_bearer(&token, &other_app_id, Scope::AppsDeploy, "test-request-id")
        .await
        .expect_err("a bearer for one owner must not authorize a different owner's app");
    assert!(matches!(err, AuthError::Forbidden));

    cleanup_app(&conn, &app_id).await;
    cleanup_app(&conn, &other_app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_user(&conn, &other_owner_id).await;
}

#[ntex::test]
async fn real_delegating_authenticator_rejects_malformed_bearer() {
    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);
    let err = authenticator
        .verify_bearer(
            "not-a-jwt",
            &AppId::mint(),
            Scope::AppsDeploy,
            "test-request-id",
        )
        .await
        .expect_err("malformed bearer must be denied");
    assert!(
        matches!(err, AuthError::Unauthorized),
        "a malformed bearer is an authentication failure, not an infrastructure one: {err:?}"
    );
}

/// The authz audit row records the request id the caller sent, so a denial can
/// be joined back to the request that caused it.
///
/// The handler used to mint a fresh uuid at the authz call, which is correlated
/// with nothing: it appears in no gateway log, no control-plane log, and no
/// client's records. Honouring an inbound `x-request-id` is what the control
/// plane already does, so the two services agree on the identifier.
#[ntex::test]
async fn authz_receives_the_callers_request_id_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert(
        "good-token",
        &owner_id,
        [Scope::AppsDeploy],
        [app_id.clone()],
    );
    let (state, tmp) = state_for(auth.clone());
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    // THE APPLY ENDPOINT reaches authorization before its expected database
    // precondition refusal. This case used to drive
    // `PUT /v1/apps/{app}/policy`, which was deleted with the policy store; a
    // request-id test pointed at a route that 404s before authz runs would observe
    // an empty `seen_request_ids` and read as a failure of the header plumbing
    // rather than of the fixture.
    let caller_request_id = "req-from-the-caller-0001";
    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", "Bearer good-token")
        .header("x-request-id", caller_request_id)
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let seen = auth
        .seen_request_ids
        .lock()
        .expect("static auth lock")
        .clone();
    assert!(
        seen.contains(&caller_request_id.to_string()),
        "authz was given {seen:?}, none of which is the caller's {caller_request_id:?}; \
         an id minted at the authz call correlates with nothing"
    );

    drop(tmp);
    cleanup_app(&conn, &app_id).await;
}

/// The source address is authz input, not just a rate-limit key. Drive the real
/// verifier and Cedar audit writer so a value captured only at the HTTP edge
/// cannot satisfy this regression.
#[ntex::test]
async fn trusted_source_ip_reaches_the_authz_context_and_audit_row_pg() {
    let conn = admin_conn().await;
    let bucket_key = "migrate:mutation:ip:203.0.113.77";
    conn.execute(
        "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
        &[&bucket_key],
    )
    .await
    .expect("clear source-IP mutation bucket fixture");
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    seed_app(&conn, &app_id, &owner_id).await;

    let token = platform_token(&owner_id, "apps:deploy");
    let authenticator = Arc::new(real_authenticator(admin_conn().await));
    let (state, tmp) = state_for_trusted_proxy(authenticator).await;
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let request_id = format!("migrated-source-ip-{}", Uuid::new_v4().simple());
    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{}/migrations/apply", app_id.as_str()))
        .header("authorization", format!("Bearer {token}"))
        .header("x-request-id", request_id.as_str())
        .header("x-forwarded-for", "198.51.100.9, 203.0.113.77")
        .set_json(&with_policy(create_notes_request(), malformed_policy()))
        .to_request();
    let response = test::call_service(&svc, req).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let _ = test::read_body(response).await;

    let observed_ip: Option<String> = conn
        .query_one(
            "SELECT host(request_ip) FROM zeroship.authz_decisions WHERE request_id = $1",
            &[&request_id],
        )
        .await
        .expect("query authz audit source IP")
        .get(0);
    assert_eq!(
        observed_ip.as_deref(),
        Some("203.0.113.77"),
        "authz did not receive the trusted proxy's rightmost source address"
    );

    conn.execute(
        "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
        &[&bucket_key],
    )
    .await
    .expect("remove source-IP mutation bucket fixture");
    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}
