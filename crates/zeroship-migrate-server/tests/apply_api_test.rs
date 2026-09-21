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
use zeroship_core::device_grant::{PLATFORM_CLI_CLIENT_ID, PLATFORM_CLI_ISSUABLE_SCOPES};
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{database_derivation, BindingId, DatabaseId};
use zeroship_id::{AppId, OrganizationId, ProjectId, UserId};
use zeroship_migrate::{
    effective_policy_from_charter_toml, ExecutorConfig, MigrationBackend, ProjectLockAcquisition,
};
use zeroship_migrate_postgres::PostgresBackend;
use zeroship_migrate_server::datastore::cluster;
use zeroship_migrate_server::auth::{
    AuthError, Authenticator, ControlPlaneAuthenticator, VerifiedCaller,
};
use zeroship_migrate_server::policy::{ManagedPolicyConfig, MIGRATE_POLICY_FILENAME};
use zeroship_migrate_server::rate_limit::{MutationRateLimiter, PostgresMutationRateLimiter};
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
/// The platform schema must come from the corpus, not from a hand-written
/// `CREATE TABLE IF NOT EXISTS`: that is verification-record class 7 in the
/// fixture rather than the assertion, measuring a shape the corpus does not
/// produce, so a column the corpus declares `NOT NULL` could be nullable here, a
/// CHECK could be absent, and every case would still be green.
/// `zeroship.apps` comes from
/// `db/migrations-ts/20260702000200_control_tables.ts` like every other platform
/// table, and a database without it fails loudly here.
async fn assert_platform_schema_present(conn: &Client) {
    const REQUIRED: [&str; 3] = ["plans", "users", "apps"];
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

/// Remove an app and every database it reached.
///
/// A case owns the database it was seeded with, so cleanup follows the binding
/// edge: each bound database's schema and its three derived roles go, then the
/// rows. Roles are CLUSTER-wide, so a leaked one outlives the database it was
/// named for and is visible to every other case on this server.
async fn cleanup_app(conn: &Client, app_id: &AppId) {
    let databases = conn
        .query(
            "SELECT database_id FROM zeroship.database_bindings WHERE app_id = $1::text",
            &[&app_id.as_str()],
        )
        .await
        .map(|rows| {
            rows.iter()
                .map(|row| row.get::<_, String>("database_id"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // The binding rows go first: `database_bindings` holds RESTRICT foreign
    // keys onto both the app and the database, so neither row can be deleted
    // while an edge between them stands.
    let _ = conn
        .execute(
            "DELETE FROM zeroship.database_bindings WHERE app_id = $1::text",
            &[&app_id.as_str()],
        )
        .await;
    for database in databases {
        let Ok(database) = DatabaseId::parse(&database) else {
            continue;
        };
        cleanup_database(conn, &database).await;
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

/// Drop one database's schema, the three roles derived from its id, and its
/// control row.
async fn cleanup_database(conn: &Client, database: &DatabaseId) {
    let schema = database_derivation::schema_name(database);
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let _ = conn
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE;", q(&schema)))
        .await;
    let roles = [
        database_derivation::migrator_role_name(database).expect("the migrator role name fits"),
        database_derivation::capability_role_name(
            database,
            zeroship_core::database_role::DatabaseCapability::ReadWrite,
        )
        .expect("the readwrite role name fits"),
        database_derivation::capability_role_name(
            database,
            zeroship_core::database_role::DatabaseCapability::ReadOnly,
        )
        .expect("the readonly role name fits"),
    ];
    for role in roles {
        let _ = conn
            .batch_execute(&format!(
                "DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='{r_lit}') THEN \
                    EXECUTE 'REASSIGN OWNED BY {rq} TO current_user'; \
                    EXECUTE 'DROP OWNED BY {rq}'; \
                    EXECUTE 'DROP ROLE {rq}'; \
                 END IF; END $$;",
                r_lit = role.replace('\'', "''"),
                rq = q(&role),
            ))
            .await;
    }
    let _ = conn
        .execute(
            "DELETE FROM zeroship.databases WHERE id = $1::text",
            &[&database.as_str()],
        )
        .await;
    let _ = conn
        .execute(
            &format!(
                "DELETE FROM {}.{} WHERE database_id = $1::text",
                cluster::ADMIN_SCHEMA,
                cluster::EPOCH_TABLE
            ),
            &[&database.as_str()],
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

/// The apply route: the APP that authorizes the call and the DATABASE whose
/// schema the DDL lands in.
///
/// Composed from two parsed typed ids, never from raw text, so a case cannot
/// address a database by a spelling the extractor would refuse.
fn apply_uri(app_id: &AppId, database_id: &DatabaseId) -> String {
    format!(
        "/v1/apps/{}/databases/{}/migrations/apply",
        app_id.as_str(),
        database_id.as_str()
    )
}

/// The datastore every database in this target is placed on.
///
/// One row, because one PostgreSQL server holds every schema these cases
/// create: `zeroship.datastores` is keyed on a cluster's own identity, so two
/// rows for one server would be a fiction the placement keys would then hold
/// databases against.
fn fixture_datastore() -> String {
    let body = "apitestfixture";
    format!("dst_{body}{}", "0".repeat(25 - body.len()))
}

/// The zone the platform corpus seeds and `zeroship.projects` defaults to.
const DEFAULT_ZONE: &str = "ezn_default000000000000000000";

/// Make the CLUSTER match a declared database, through the reconciler's own
/// statements.
///
/// The apply writes into `db_<dbs>` and never creates it. The schema, its owner
/// `zs_db_<dbs>_mig` and the two capability roles are minted by the cluster
/// reconciler in ONE transaction with the epoch row that names them, so a
/// fixture that hand-rolled a `CREATE SCHEMA` would be asserting against a
/// shape production does not produce - and would give the schema an owner no
/// binding inherits.
async fn converge_database_schema(database: &DatabaseId) {
    let (mut client, conn) = compio_postgres::connect(&dsn(), NoTls)
        .await
        .expect("connect to converge a database");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    cluster::apply_bootstrap_corpus(&client)
        .await
        .expect("bootstrap this cluster's platform roles and admin schema");
    cluster::converge_database(&mut client, database, 1)
        .await
        .expect("converge the declared database");
}

/// Seed an app, a project-owned database on the fixture datastore, and a LIVE
/// binding between them.
///
/// The binding is what the apply route admits against, so it is part of the
/// world every case needs rather than a per-case detail. Control declares a
/// binding `pending` and a reconciler observes it; these rows are written
/// already live because what is under test here is the apply, not convergence.
async fn seed_app(conn: &Client, app_id: &AppId, owner_id: &UserId) -> DatabaseId {
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

    let datastore = fixture_datastore();
    conn.execute(
        "INSERT INTO zeroship.datastores (id, system_identifier, execution_zone_id, status) \
         VALUES ($1, 7788990011223344, $2, 'active') ON CONFLICT (id) DO NOTHING",
        &[&datastore, &DEFAULT_ZONE],
    )
    .await
    .expect("declare the fixture datastore");
    let database = DatabaseId::mint();
    conn.execute(
        "INSERT INTO zeroship.databases \
             (id, project_id, execution_zone_id, datastore_id, name, status) \
         VALUES ($1, $2, $3, $4, $5, 'active')",
        &[
            &database.as_str(),
            &project_id.as_str(),
            &DEFAULT_ZONE,
            &datastore,
            &format!("migrated-{}", Uuid::new_v4().simple()),
        ],
    )
    .await
    .expect("declare the app's database");
    conn.execute(
        "INSERT INTO zeroship.database_bindings \
             (id, app_id, database_id, project_id, capability, status, observed_generation) \
         VALUES ($1, $2, $3, $4, 'readwrite', 'active', 1)",
        &[
            &BindingId::mint().as_str().to_owned(),
            &app_id.as_str(),
            &database.as_str(),
            &project_id.as_str(),
        ],
    )
    .await
    .expect("declare the app's live binding");
    database
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
/// accepted-document set moves under it. A fixture that no longer parses makes
/// every test that uses it fail with a 422 that is indistinguishable, at the
/// assertion level, from the status each test meant to exercise, so none of them
/// test what their name claims and nothing in the output says "fixture".
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
            .compose_effective_for_schema(&app_id, app_id.as_str(), None, Some(&draft))
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
            .compose_effective_for_schema(&app_id, app_id.as_str(), None, Some(&escalating))
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
/// The journal lives in the app's OWN schema, so this name is inside the
/// creator's declared scope as far as the confined ceiling is concerned. If the
/// op were permitted on a fresh app the engine's `CREATE TABLE IF NOT EXISTS`
/// bootstrap would ADOPT the creator's table as the journal.
///
/// `createTable` takes `name`, not `table`; a wrong field name is refused as a
/// MALFORMED IR ENVELOPE - a 422 that looks exactly like a name refusal in the
/// response and proves nothing about the name. Keep the op shape valid or the
/// case measures the typo.
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

// The creator policy draft is a `zeroship-migrate-policy` `PolicyDoc` (grant rules
// against the operator ceiling), not a `PolicyProfile` TOML. A draft may only
// TIGHTEN: `safety.destructive_ops` orders forbid <= warn <= allow, so `forbid` is
// admissible under the confined ceiling's `allow`.
//
// No `runtime.*` knob can be raised: every one is registered `DeclaredOnly` with
// default 1, and the engine's load gate refuses ANY document raising one above its
// default (`DeclaredOnlyNonDefault`), in an operator ceiling or a creator draft
// alike. A fixture carrying one gets a 422 for a parse failure instead of
// exercising the behaviour its name claims; `assert_policy_fixtures_are_current`
// below is what makes such a removal fail as a stale fixture.
fn tighter_policy() -> &'static str {
    "policy_version = 1\n\n[[grant]]\nkey = \"safety.destructive_ops\"\nvalue = \"forbid\"\nscope = \"all\"\n"
}

// Approval is the SEALED `safety.require_approval` obligation the engine declares and
// the host enforces. A draft authors it as a normal `[[require]]` — `always` gates
// EVERY migration for operator approval (destructive or not). The draft also RE-STATES the
// `safety.destructive_ops = allow` grant it wants kept (admit resolves grants
// from the draft layer, so a draft that only tightens one knob must re-state the
// ceiling grants it relies on — here, keeping destructive ops classifiable-not-denied
// so the approval gate can hold the DROP for review instead of the guard forbidding it).
fn require_approval_policy() -> &'static str {
    "policy_version = 1\n\n[[require]]\nkey = \"safety.require_approval\"\nvalue = \"always\"\nscope = \"all\"\n\n[[grant]]\nkey = \"safety.destructive_ops\"\nvalue = \"allow\"\nscope = \"all\"\n"
}

// A SECOND, textually distinct tightening, so the versioned-policy test can submit
// two drafts and tell version 1 from version 2 by `raw_toml`. It revokes
// `schema.rename`, a real tightening of a grant the confined ceiling carries
// (`value = true`).
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
/// The prefix and the nesting are load-bearing: a probe on any other spelling
/// returns 0 forever and every `>= 1` assertion below fails while the code is
/// correct. `journal_is_absent_at_the_unfenced_name` is the case that keeps this
/// helper honest by pinning the OTHER direction.
async fn journaled_count(conn: &Client, database: &DatabaseId) -> i64 {
    let q = format!(
        "\"{}\".__zeroship_schema_migrations",
        database_derivation::schema_name(database).replace('"', "\"\"")
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

/// An apply is not a database-creation operation.
///
/// A database whose cluster reconciler has not converged it has no schema, and
/// the apply refuses rather than creating one: a schema this service invented
/// would be owned by a role no binding inherits. The refusal therefore names no
/// remedy - there is no call a creator can make to hurry a reconciler.
///
/// This is deliberately a live PostgreSQL case because the refusal.s important
/// property is absence of catalog side effects: `provision_migrator` creates a
/// cluster-global role and grants membership before it first touches the
/// missing schema. A mocked handler can prove the 409 body but cannot prove
/// those objects were never created, or that the ledger stayed untouched.
#[ntex::test]
async fn apply_refuses_an_uncreated_database_before_provisioning_or_ledger_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let database = seed_app(&conn, &app_id, &owner_id).await;

    let schema = database_derivation::schema_name(&database);
    let migrator_role =
        database_derivation::migrator_role_name(&database).expect("migrator role name");
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
            .uri(&apply_uri(&app_id, &database))
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

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an absent database must be a creator-recoverable 409, never a 404 or 5xx: {body}"
    );
    assert_eq!(body["error"], "database_not_created", "{body}");
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains(database.as_str())),
        "the refusal must name the database it is about: {body}"
    );
    assert!(
        body["remedy"].is_null(),
        "a database waiting on its reconciler has no call to name: {body}"
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
}

/// Run `sql` as one of a database's CAPABILITY roles and switch back, whatever
/// happened.
///
/// A production session reaches a capability through its binding role, which is
/// what `crates/zeroship-data-orm/tests/postgres_binding_fence.rs` measures.
/// What is under test here is the GRANT the apply left on the audit table, so
/// this assumes the capability directly: an intermediate role that inherits it
/// would prove the same privilege through one more hop.
async fn as_capability_role(
    conn: &Client,
    role: &str,
    sql: &str,
) -> Result<(), compio_postgres::Error> {
    conn.batch_execute(&format!("SET ROLE {}", quote_ident(role)))
        .await
        .expect("a superuser connection may assume any role");
    let out = conn.batch_execute(sql).await;
    conn.batch_execute("RESET ROLE")
        .await
        .expect("the admin identity must come back or every later assertion measures the wrong principal");
    out
}

/// A converged database keeps ITS OWN migrator as the schema owner across an
/// apply, and the apply publishes that schema's tables.
///
/// Ownership is the load-bearing half. The cluster reconciler hands
/// `db_<dbs>` to `zs_db_<dbs>_mig` and re-asserts that on every pass, so an
/// apply that ran as any other role would either be refused by the owner or -
/// holding `CREATEROLE` - take ownership away and have the next pass take it
/// back, leaving objects owned by one role inside a schema owned by another.
#[ntex::test]
async fn an_apply_keeps_the_reconcilers_migrator_as_the_schema_owner_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
    converge_database_schema(&database).await;

    let schema = database_derivation::schema_name(&database);
    let migrator =
        database_derivation::migrator_role_name(&database).expect("the migrator role name fits");
    let before: String = conn
        .query_one(
            "SELECT pg_get_userbyid(nspowner) AS owner FROM pg_namespace WHERE nspname = $1",
            &[&schema],
        )
        .await
        .expect("the converged schema must exist")
        .get("owner");
    assert_eq!(
        before, migrator,
        "the control: the reconciler owns this schema before the apply"
    );

    let resp = test::call_service(
        &svc,
        test::TestRequest::post()
            .uri(&apply_uri(&app_id, &database))
            .header("authorization", "Bearer good-token")
            .set_json(&create_notes_request())
            .to_request(),
    )
    .await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "migration apply failed: {}",
        String::from_utf8_lossy(&body)
    );

    let after: String = conn
        .query_one(
            "SELECT pg_get_userbyid(nspowner) AS owner FROM pg_namespace WHERE nspname = $1",
            &[&schema],
        )
        .await
        .expect("the schema must still exist")
        .get("owner");
    assert_eq!(
        after, migrator,
        "the apply must not move the schema's owner off the role the reconciler minted"
    );
    let table_owner: String = conn
        .query_one(
            "SELECT pg_get_userbyid(c.relowner) AS owner FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = 'notes'",
            &[&schema],
        )
        .await
        .expect("the apply must have created the table")
        .get("owner");
    assert_eq!(
        table_owner, migrator,
        "a table owned by anything but the schema's owner is a split the reconciler cannot repair"
    );

    let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
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
    for required in [
        "notes",
        zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE,
        "__zeroship_schema_migrations",
    ] {
        assert!(
            published_tables.iter().any(|table| table == required),
            "a successful apply omitted {required} from {publication}: {published_tables:?}"
        );
    }

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// A real apply leaves the unmask audit table writable by the CAPABILITY roles
/// a bound session narrows through, and by nobody else's.
///
/// The audit table is platform DDL inside a creator schema, so the apply owes
/// it a grant: without one the table exists and every unmask fails on the audit
/// write rather than on the unmask. The co-tenant control is what makes the
/// grant mean something - a `GRANT ... TO PUBLIC`, or one issued against the
/// wrong database's roles, would satisfy the first half alone.
#[ntex::test]
async fn an_apply_leaves_the_audit_table_writable_by_its_own_capability_roles_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let database = seed_app(&conn, &app_id, &owner_id).await;
    // A SECOND database, converged and never applied to. Its readwrite role is
    // the control: it exists, it is a legal grantee, and it must hold nothing
    // on the first database's audit table.
    let neighbour_app = AppId::mint();
    let neighbour_owner = UserId::mint();
    let neighbour = seed_app(&conn, &neighbour_app, &neighbour_owner).await;

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
    converge_database_schema(&database).await;
    converge_database_schema(&neighbour).await;

    let resp = test::call_service(
        &svc,
        test::TestRequest::post()
            .uri(&apply_uri(&app_id, &database))
            .header("authorization", "Bearer good-token")
            .set_json(&create_notes_request())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let schema = database_derivation::schema_name(&database);
    let table = zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE;
    let audit = format!("{}.{}", quote_ident(&schema), quote_ident(table));
    assert!(
        relation_exists(&conn, &audit).await,
        "a successful apply must leave {audit} in place"
    );

    let capability = |database: &DatabaseId, capability| {
        database_derivation::capability_role_name(database, capability)
            .expect("the capability role name fits")
    };
    let readwrite = capability(&database, DatabaseCapability::ReadWrite);
    let readonly = capability(&database, DatabaseCapability::ReadOnly);
    let co_tenant = capability(&neighbour, DatabaseCapability::ReadWrite);

    let lit = |s: &str| s.replace('\'', "''");
    // BOTH capabilities, including read-only: an unmask is a READ that produced
    // plaintext, so a read-only binding performs them and its row has to land.
    for role in [&readwrite, &readonly] {
        assert!(
            probe_bool(
                &conn,
                &format!(
                    "SELECT has_table_privilege('{}', '{}', 'INSERT')",
                    lit(role),
                    lit(&audit)
                ),
            )
            .await,
            "{role} must hold INSERT on {audit}"
        );
        assert!(
            probe_bool(
                &conn,
                &format!(
                    "SELECT has_sequence_privilege('{}', pg_get_serial_sequence('{}', 'id'), 'USAGE')",
                    lit(role),
                    lit(&audit)
                ),
            )
            .await,
            "{role} must hold USAGE on the BIGSERIAL sequence behind {audit}"
        );
        // The grant is INSERT and the sequence, and nothing else: a session that
        // could SELECT here would read every other actor's audit trail back
        // through the app.
        assert!(
            !probe_bool(
                &conn,
                &format!(
                    "SELECT has_table_privilege('{}', '{}', 'SELECT')",
                    lit(role),
                    lit(&audit)
                ),
            )
            .await,
            "{role} must not be able to read {audit} back"
        );
    }
    assert!(
        !probe_bool(
            &conn,
            &format!(
                "SELECT has_table_privilege('{}', '{}', 'INSERT')",
                lit(&co_tenant),
                lit(&audit)
            ),
        )
        .await,
        "the control: {co_tenant} belongs to another database and must hold nothing on {audit}"
    );

    let write = as_capability_role(
        &conn,
        &readwrite,
        &format!(
            "INSERT INTO {audit} (collection, row_pk, \"column\", classification, outcome) \
             VALUES ('notes', 'row-1', 'body', 'pii', 'granted')"
        ),
    )
    .await;
    assert!(
        write.is_ok(),
        "a bound session must be able to write an unmask audit row after apply: {write:?}"
    );
    let rows: i64 = conn
        .query(&format!("SELECT count(*)::int8 FROM {audit}"), &[])
        .await
        .expect("count audit rows")[0]
        .get(0);
    assert_eq!(rows, 1, "the audit row must be readable by the owner");

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
    cleanup_app(&conn, &neighbour_app).await;
    cleanup_user(&conn, &neighbour_owner).await;
}

#[ntex::test]
async fn apply_api_accepts_apps_migrate_owner_and_applies_ir_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
    converge_database_schema(&database).await;

    let req = test::TestRequest::post()
        .uri(&apply_uri(&app_id, &database))
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
        table_exists(&conn, &database_derivation::schema_name(&database), "notes").await,
        "notes table must exist in the per-app schema"
    );
    assert!(
        !table_exists(&conn, "public", "notes").await,
        "Confined apply must not create the table in public"
    );
    assert!(
        journaled_count(&conn, &database).await >= 1,
        "IR apply must be journaled in the app's own schema under the platform prefix"
    );

    // WHERE THE JOURNAL IS, asserted both ways. The positive above passes on a
    // build that puts the journal anywhere reachable by that one name; these two
    // pin the placement itself.
    //
    // The unfenced spelling matters: the engine bootstraps with `CREATE TABLE IF NOT EXISTS` and its table names are
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
                database_derivation::schema_name(&database)
            )
        )
        .await,
        "the journal must NOT occupy the unfenced name a creator could declare"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

/// Two project ids that collide in PostgreSQL's 32-bit `hashtext` space still
/// own independent project locks.
///
/// The pair is fixed output from a live-PostgreSQL search over
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

/// A DESTRUCTIVE migration is not parked for an operator who does not exist.
///
/// THIS IS A CAPABILITY REMOVAL, PINNED SO IT CANNOT COME BACK BY ACCIDENT. A
/// `dropTable` preflighted as gated must not answer
/// **409 `migration_requires_operator_approval`** naming a `migration_id` that
/// only an operator `approve` route could clear.
///
/// WHAT THIS DOES NOT ASSERT, AND MUST NOT BE READ AS. The drop still does not
/// apply. The request answers **422** with *"apply (0002_drop_notes.ir.json):
/// plan requires approval (destructive) but none was given"* - the ENGINE's
/// refusal, because this host passes `Approval::None`. Removing the host's
/// approval state machine removed a refusal nobody could clear; it did not grant
/// creators destructive migrations, and giving them one is a separate decision
/// about what `Approval` the host asserts on a creator's behalf.
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
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
    converge_database_schema(&database).await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&apply_uri(&app_id, &database))
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
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
    converge_database_schema(&database).await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&apply_uri(&app_id, &database))
            .header("authorization", "Bearer good-token")
            .set_json(&body)
            .to_request()
    };

    // A real migration first, so the journal exists to be attacked and the app
    // schema exists to be measured.
    let resp = test::call_service(&svc, post(create_notes_request())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let journaled_before = journaled_count(&conn, &database).await;
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
                    database_derivation::schema_name(&database)
                )
            )
            .await,
            "the journal must still exist after a refused {label}"
        );
    }
    assert_eq!(
        journaled_count(&conn, &database).await,
        journaled_before,
        "a refused migration must not have journalled anything"
    );

    // ARM 2: AN APP THAT HAS NEVER MIGRATED. The refusal must come from the name
    // gate rather than from colliding with a journal an earlier request created.
    let fresh_id = AppId::mint();
    let fresh_owner = UserId::mint();
    let fresh_database = seed_app(&conn, &fresh_id, &fresh_owner).await;
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
    converge_database_schema(&fresh_database).await;
    assert!(
        !relation_exists(
            &conn,
            &format!(
                "\"{}\".__zeroship_schema_migrations",
                database_derivation::schema_name(&fresh_database)
            )
        )
        .await,
        "this arm only means anything against an app with NO journal yet"
    );
    let resp = test::call_service(
        &fresh_svc,
        test::TestRequest::post()
            .uri(&apply_uri(&fresh_id, &fresh_database))
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

#[ntex::test]
async fn apply_api_5xx_detail_is_generic_and_does_not_leak_internals() {
    let app_id = AppId::mint();
    let seed = admin_conn().await;
    let owner_id = UserId::mint();
    let database = seed_app(&seed, &app_id, &owner_id).await;
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
        .uri(&apply_uri(&app_id, &database))
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
    // An UNBOUND database, deliberately: authorization is decided before the
    // binding is consulted, so this refusal must not depend on one existing.
    let database = DatabaseId::mint();
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
        .uri(&apply_uri(&app_id, &database))
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
    // An UNBOUND database, deliberately: authorization is decided before the
    // binding is consulted, so this refusal must not depend on one existing.
    let database = DatabaseId::mint();
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
        .uri(&apply_uri(&app_id, &database))
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
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
    converge_database_schema(&database).await;

    let req = test::TestRequest::post()
        .uri(&apply_uri(&app_id, &database))
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
/// must classify as a creator fault (422), never as `IrApplyError::Read` / 503
/// `migration_infrastructure` - the same variant a genuine disk read failure produces.
/// The 5xx arm of `apply_error_response` logs at ERROR and REPLACES the detail
/// with "migration service unavailable", so miscategorising it would leave the creator
/// knowing neither what was wrong nor which file, page an operator for a typo, and
/// make a retrying client retry a request that can never succeed.
///
/// The suppression of detail on 5xx is correct and stays.
#[ntex::test]
async fn apply_api_reports_malformed_ir_as_creator_fault_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
    converge_database_schema(&database).await;

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
        .uri(&apply_uri(&app_id, &database))
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
        .uri(&apply_uri(&app_id, &database))
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
    let seed = admin_conn().await;
    let owner_id = UserId::mint();
    let database = seed_app(&seed, &app_id, &owner_id).await;
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
        .uri(&apply_uri(&app_id, &database))
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
    let seed = admin_conn().await;
    let owner_id = UserId::mint();
    let database = seed_app(&seed, &app_id, &owner_id).await;
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
        .uri(&apply_uri(&app_id, &database))
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
/// bucket. Two tokens are the configured burst; the third request from the same
/// trusted source is refused, while a different source still owns an independent
/// bucket.
#[ntex::test]
async fn apply_api_rate_limits_each_source_ip_across_the_shared_store_pg() {
    let app_id = AppId::mint();
    let seed = admin_conn().await;
    let owner_id = UserId::mint();
    let database = seed_app(&seed, &app_id, &owner_id).await;
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
            .uri(&apply_uri(&app_id, &database))
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
    // An UNBOUND database, deliberately: authorization is decided before the
    // binding is consulted, so this refusal must not depend on one existing.
    let database = DatabaseId::mint();
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
        .uri(&apply_uri(&app_id, &database))
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
    let _database = seed_app(&conn, &app_id, &owner_id).await;

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
    let _database = seed_app(&conn, &app_id, &owner_id).await;

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
    let _database = seed_app(&conn, &app_id, &owner_id).await;

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
    let _database = seed_app(&conn, &app_id, &owner_id).await;
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
/// A freshly minted uuid would be correlated with nothing - no gateway log, no
/// control-plane log, no client's records. Honouring an inbound `x-request-id` is
/// what the control plane already does, so the two services agree on the
/// identifier.
#[ntex::test]
async fn authz_receives_the_callers_request_id_pg() {
    let conn = admin_conn().await;
    let app_id = AppId::mint();
    let owner_id = UserId::mint();
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
    // precondition refusal. A request-id test pointed at a route that 404s before
    // authz runs would observe an empty `seen_request_ids` and read as a failure
    // of the header plumbing rather than of the fixture.
    let caller_request_id = "req-from-the-caller-0001";
    let req = test::TestRequest::post()
        .uri(&apply_uri(&app_id, &database))
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
    let database = seed_app(&conn, &app_id, &owner_id).await;

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
        .uri(&apply_uri(&app_id, &database))
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
