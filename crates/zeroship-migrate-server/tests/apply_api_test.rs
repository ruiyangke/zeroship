use std::collections::{HashMap, HashSet};
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
use zeroship_migrate::{
    effective_policy_from_charter_toml, ExecutorConfig, MigrationBackend,
    ProjectLockAcquisition,
};
use zeroship_migrate_postgres::PostgresBackend;
use zeroship_migrate_server::apply::{apply_ir_documents, ApplyMigrationsRequest};
use zeroship_migrate_server::auth::{
    AuthError, Authenticator, ControlPlaneAuthenticator, VerifiedCaller,
};
use zeroship_migrate_server::policy::{ManagedPolicyConfig, MIGRATE_POLICY_FILENAME};
use zeroship_migrate_server::schema_apply_store::SchemaApplyStore;
use zeroship_migrate_server::session::CompioPgSession;
use zeroship_migrate_server::MigrationServiceState;

const TEST_POLICY_SEAL_KEY: &[u8] = b"migrated integration policy seal key";

/// The database this target dials, or a panic naming the provisioner.
///
/// It used to fall back to `dbname=zeroship_control_test` on :5440, a database
/// no other part of the workspace creates. On a box that happened to have one
/// the run went green against it; on a box that did not, the connect failed
/// with an address nothing in the tree had chosen.
fn dsn() -> String {
    zeroship_core::config::test_database_url()
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
    /// Every `request_id` the handlers passed in, in call order, so a test can
    /// assert the id the caller sent is the id authz was given.
    seen_request_ids: Mutex<Vec<String>>,
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
        request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        self.seen_request_ids
            .lock()
            .expect("static auth lock")
            .push(request_id.to_owned());
        let callers = self.callers.lock().expect("static auth lock");
        let caller = callers.get(token).ok_or(AuthError::Unauthorized)?;
        if !caller.actions.contains(&required_action) || !caller.owned_apps.contains(&app_id) {
            return Err(AuthError::Forbidden);
        }
        Ok(VerifiedCaller {
            principal_id: caller.principal_id,
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
         cargo test -p zeroship-migrate-server --features live-db-tests",
        missing.join(", zeroship."),
    );
}

async fn cleanup_app(conn: &Client, app_id: &Uuid) {
    let schema = app_id.to_string();
    // The vendor crate, not the composition root: `migrator_role_name` lives at
    // `zeroship_migrate_postgres::role`, which is how `src/apply.rs:19` in this
    // same crate already spells it. The old path did not resolve, so this test
    // target had stopped compiling - invisible while the clippy gate aborted
    // before reaching it.
    let role = zeroship_migrate_postgres::role::migrator_role_name(&schema).unwrap();
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    // THE WORKFLOW JOURNAL SCHEMA IS THE THIRD ONE, and it was leaking. A
    // successful apply runs `runtime_dependents_sql`, which creates `app_<uuid>`
    // beside `<uuid>`; this teardown dropped only the latter two.
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE;",
            q(&format!("{schema}_migrations")),
            q(&format!("app_{schema}")),
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
    let runtime_role = zeroship_core::database_role::per_app_role_name(&schema)
        .expect("test app role name");
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
    let app_id = Uuid::now_v7();
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
    assert_policy_fixtures_are_current(&policy_config);
    let tmp = tmpdir("tmp");
    (
        Arc::new(MigrationServiceState::new(
            provision_dsn,
            control_dsn,
            tmp.clone(),
            authenticator,
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

fn two_file_lock_span_request() -> Value {
    json!({
        "kind": "ir",
        "descriptor_sha256": TEST_DESCRIPTOR_SHA256_NEXT,
        "documents": [
            {
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
            },
            {
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
            }
        ]
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
    json!({
        "kind": "ir",
        "descriptor_sha256": TEST_DESCRIPTOR_SHA256,
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
    json!({
        "kind": "ir",
        "descriptor_sha256": TEST_DESCRIPTOR_SHA256,
        "documents": [{
            "filename": "0003_drop_journal.ir.json",
            "body": {
                "ir_version": 1,
                "name": "drop_journal",
                "ops": [{
                    "op": "dropTable",
                    "table": "__zeroship_schema_migrations"
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
/// `crates/control/tests/common/mod.rs`; deliberately a private copy rather
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
fn platform_token(subject: Uuid, scope: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let claims = json!({
        "iss": PLATFORM_ISSUER,
        "sub": subject.to_string(),
        "aud": "control.zeroship.ai",
        "exp": now + 3600,
        "iat": now,
        "nbf": now.saturating_sub(1),
        "jti": Uuid::new_v4().to_string(),
        "client_id": CONSOLE_CLIENT_ID,
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
async fn journaled_count(conn: &Client, app_id: &Uuid) -> i64 {
    let q = format!(
        "\"{}\".__zeroship_schema_migrations",
        app_id.to_string().replace('"', "\"\"")
    );
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

/// Does `to_regclass` resolve this exact relation?
async fn relation_exists(conn: &Client, qualified: &str) -> bool {
    let lit = qualified.replace('\'', "''");
    let rows = conn
        .query(&format!("SELECT to_regclass('{lit}') IS NOT NULL AS p"), &[])
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

/// Run `sql` under the PRODUCTION runtime identity for `app_schema` and switch
/// back, whatever happened.
///
/// The chain is the worker's, not a shortcut to the app role: `SET SESSION
/// AUTHORIZATION zeroship_worker` (the one login role every worker process
/// connects as) then `SET ROLE app_<id>_role`. `runtime_dependents_sql` grants
/// that membership `WITH INHERIT FALSE`, so the `SET ROLE` is load-bearing -
/// without it the worker holds no reach into the app schema at all - and the
/// two-step is exactly what `zeroship-plugin-db` does per request.
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
        &zeroship_core::database_role::per_app_role_name(app_schema)
            .expect("test app role name"),
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
    let restored = conn.batch_execute("RESET ROLE; RESET SESSION AUTHORIZATION;").await;
    assert!(
        restored.is_ok(),
        "the admin identity must come back on this connection or every later \
         assertion in this case is measuring the wrong principal: {restored:?}"
    );
    out
}

/// THE APPLY PATH'S OWN CALL ORDER, BOUND - not the constraint re-proved.
///
/// `provision_audit_unmask_table` has to run BEFORE THE LAST
/// `apply::provision_runtime_app_role`, because that function grants the runtime
/// role `INSERT` and sequence `USAGE` through `GRANT ... ON ALL TABLES/SEQUENCES
/// IN SCHEMA` - snapshots over what exists when they run. Get it wrong and the
/// audit table and its `BIGSERIAL` sequence are both unreachable to the only
/// process that writes them, so every `unmask()` answers `permission denied` and
/// the record of who read plaintext is lost.
///
/// WHY THIS CASE EXISTS AT ALL. `apply::live_audit_unmask_provisioning::
/// the_audit_table_must_be_provisioned_before_the_runtime_role` already proves
/// the CONSTRAINT is real, by calling the two functions itself in three orders.
/// It cannot see `apply.rs`. Swap the audit-table creation with the last
/// `provision_runtime_app_role` in `apply_ir_request` and that case stays green,
/// because it never asks what order production uses. This one runs a real apply
/// through the HTTP surface and then reaches the table the way the worker does,
/// so the order under test is the order that ships. Measured both ways on
/// `PostgreSQL` 17.11 (`server_version_num=170011`): green as written, and
/// `permission denied for table __zeroship_audit_unmask` with the two calls
/// swapped.
///
/// IF THE CREATION WERE DELETED OUTRIGHT rather than moved, this fails one
/// assertion earlier and says so: `to_regclass` resolves nothing, so the
/// existence assertion goes first and names deletion. That ordering is
/// deliberate - `has_table_privilege` ERRORS on a missing relation, and a probe
/// that panics inside the driver would report a reordering hazard as a malformed
/// query.
///
/// THE REAL INSERT IS THE DECISIVE ASSERTION, and the two catalog probes above
/// it are only for diagnosis. `has_table_privilege` is blind to schema `USAGE`:
/// measured on the same server, a role with `INSERT` on a table but no `USAGE`
/// on its schema probes `t` and still fails the write with `permission denied
/// for schema`. Only executing the statement covers schema USAGE, the table ACL
/// and the sequence ACL at once - which is all three objects the ordering hazard
/// can cost.
///
/// WHAT IT DOES NOT BIND. Any position that satisfies the constraint stays green,
/// including moving the creation down beside the second `provision_runtime_app_role`
/// - correctly, since the resulting privileges are identical. It also rules only
/// on the SUCCESS path: a creation moved into the `Ok` arm would leave an app
/// whose apply was refused with a schema and no audit table, and this case would
/// not see it. The COLUMNS are the DDL's own NOT NULL set, not a copy of the data
/// plane's INSERT list, so a column added on the plugin-db side cannot make this
/// a false red - that pairing is held by
/// `zeroship-plugin-db/tests/integration.rs`, which drives the real
/// `write_audit_unmask_row` against this same production DDL.
#[ntex::test]
async fn a_real_apply_leaves_the_runtime_role_able_to_write_the_unmask_audit_row_pg() {
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
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let schema = app_id.to_string();
    let table = zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE;
    let audit = format!("{}.{}", quote_ident(&schema), quote_ident(table));
    let runtime_role = zeroship_core::database_role::per_app_role_name(&schema)
        .expect("test app role name");

    // DELETION, not reordering: say which one before touching any privilege.
    assert!(
        relation_exists(&conn, &audit).await,
        "a successful apply must leave {audit} in place - the apply path is the \
         ONLY creator of the unmask audit table since the DDL left the worker, \
         so if this is absent the creation is gone rather than misplaced"
    );

    // Diagnosis: which of the two snapshot grants was missed.
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
        "the runtime role must hold INSERT on {audit}. GRANT ... ON ALL TABLES \
         IN SCHEMA is a snapshot, so this is false exactly when apply_ir_request \
         creates the table AFTER its last provision_runtime_app_role"
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
        "the runtime role must hold USAGE on the BIGSERIAL sequence behind \
         {audit} - the second object the same ordering mistake costs, and the \
         one that would still fail if the table grant alone were repaired"
    );

    // The write itself, as the worker, by the production identity chain.
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
        "the worker must be able to write an unmask audit row after a real \
         apply; this is the statement crud/unmask.rs issues on every plaintext \
         read, and a failure here is every unmask() in the app returning \
         permission denied: {write:?}"
    );

    // And it landed - an INSERT that silently affected nothing would pass the
    // line above.
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
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
        !relation_exists(&conn, &format!("\"{app_id}_migrations\".__zeroship_schema_migrations"))
            .await,
        "the separate <app>_migrations meta schema must be gone"
    );
    assert!(
        !relation_exists(&conn, &format!("\"{app_id}\".schema_migrations")).await,
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

/// A bundle owns one project advisory lock, not one lock per IR file.
///
/// The blocker holds `ACCESS SHARE` on the table file 2 alters. That permits
/// preflight and file 1 to finish, then parks file 2 at its `AccessExclusiveLock`
/// request. A second session probes the project advisory key at that exact point.
#[compio::test]
async fn project_lock_spans_every_file_in_a_two_file_apply_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let tmp = tmpdir("project-lock-span");
    let policy_config = ManagedPolicyConfig::default_confined(
        TEST_POLICY_SEAL_KEY.to_vec(),
        1,
    )
    .expect("test policy config");
    let schema_apply_store = SchemaApplyStore::new(dsn());
    let initial: ApplyMigrationsRequest = serde_json::from_value(create_notes_request())
        .expect("deserialize initial apply request");
    apply_ir_documents(
        &dsn(),
        &tmp,
        &app_id,
        &initial,
        &policy_config,
        &schema_apply_store,
        owner_id,
    )
    .await
    .expect("create the table file 2 will alter");

    let notes = format!("{}.{}", quote_ident(&app_id.to_string()), quote_ident("notes"));
    conn.batch_execute(&format!(
        "BEGIN; LOCK TABLE {notes} IN ACCESS SHARE MODE;"
    ))
    .await
    .expect("hold the file-2 table lock");

    let apply_dsn = dsn();
    let apply_tmp = tmp.clone();
    let request: ApplyMigrationsRequest = serde_json::from_value(two_file_lock_span_request())
        .expect("deserialize two-file apply request");
    let apply_task = compio::runtime::spawn(async move {
        let policy_config = ManagedPolicyConfig::default_confined(
            TEST_POLICY_SEAL_KEY.to_vec(),
            1,
        )
        .expect("test policy config");
        let schema_apply_store = SchemaApplyStore::new(apply_dsn.clone());
        apply_ir_documents(
            &apply_dsn,
            &apply_tmp,
            &app_id,
            &request,
            &policy_config,
            &schema_apply_store,
            owner_id,
        )
        .await
    });

    let mut reached_file_two = false;
    for _ in 0..500 {
        reached_file_two = conn
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
            .expect("observe file 2 waiting on its table lock")
            .get(0);
        if reached_file_two {
            break;
        }
        compio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let contender_acquired = if reached_file_two {
        conn.query_one(
            "SELECT pg_try_advisory_lock( \
                    (h >> 32)::int4, ((h << 32) >> 32)::int4 \
               ) FROM (SELECT hashtextextended($1, 0) AS h) AS project_lock_key",
            &[&app_id.to_string()],
        )
        .await
        .expect("probe the apply's project advisory lock")
        .get::<_, bool>(0)
    } else {
        false
    };
    if contender_acquired {
        conn.execute(
            "SELECT pg_advisory_unlock( \
                    (h >> 32)::int4, ((h << 32) >> 32)::int4 \
               ) FROM (SELECT hashtextextended($1, 0) AS h) AS project_lock_key",
            &[&app_id.to_string()],
        )
        .await
        .expect("release unexpectedly acquired project lock");
    }

    conn.batch_execute("ROLLBACK")
        .await
        .expect("release the file-2 table lock");
    let apply_result = apply_task.await.expect("join the two-file apply");

    let file_two_column_applied = conn
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM information_schema.columns \
                  WHERE table_schema = $1 \
                    AND table_name = 'notes' \
                    AND column_name = 'lock_span_file_two' \
             )",
            &[&app_id.to_string()],
        )
        .await
        .expect("observe file 2's completed schema effect")
        .get::<_, bool>(0);

    let _ = std::fs::remove_dir_all(&tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;

    assert!(
        reached_file_two,
        "the apply never reached the blocked DDL in file 2: {apply_result:?}"
    );
    assert!(
        !contender_acquired,
        "the project advisory lock was free while file 2 was executing"
    );
    let outcome = apply_result.expect("two-file apply succeeds after the blocker releases");
    assert!(
        file_two_column_applied,
        "file 2 did not apply its column after the blocker released: {outcome:?}"
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
            .header("authorization", "Bearer good-token")
            .set_json(&body)
            .to_request()
    };

    // A real migration first, so the journal exists to be attacked and the app
    // schema exists to be measured.
    let resp = test::call_service(&svc, post(create_notes_request())).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let journaled_before = journaled_count(&conn, &app_id).await;
    assert!(journaled_before >= 1, "the journal must exist to be attacked");

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
                &format!("\"{app_id}\".__zeroship_schema_migrations")
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
    let fresh_id = Uuid::now_v7();
    let fresh_owner = Uuid::new_v4();
    seed_app(&conn, fresh_id, fresh_owner).await;
    let fresh_auth = Arc::new(StaticAuthenticator::new());
    fresh_auth.insert("good-token", fresh_owner, [Scope::AppsDeploy], [fresh_id]);
    let (fresh_state, fresh_tmp) = state_for(fresh_auth);
    let fresh_svc = test::init_service(
        web::App::new()
            .state(fresh_state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    assert!(
        !relation_exists(&conn, &format!("\"{fresh_id}\".__zeroship_schema_migrations")).await,
        "this arm only means anything against an app with NO journal yet"
    );
    let resp = test::call_service(
        &fresh_svc,
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{fresh_id}/migrations/apply"))
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
async fn applied_descriptors(conn: &Client, app_id: &Uuid) -> Vec<Option<String>> {
    conn.query(
        "SELECT descriptor_sha256 FROM zeroship.app_schema_applies \
          WHERE app_id = $1 AND status = 'applied' \
          ORDER BY applied_at ASC, submitted_at ASC, migration_id ASC",
        &[app_id],
    )
    .await
    .expect("query applied descriptors")
    .iter()
    .map(|row| row.get::<_, Option<String>>("descriptor_sha256"))
    .collect()
}

/// Every applied row's `applied_versions`, newest last - the engine's own
/// `outcome.applied` for that request.
async fn applied_versions(conn: &Client, app_id: &Uuid) -> Vec<Value> {
    conn.query(
        "SELECT applied_versions FROM zeroship.app_schema_applies \
          WHERE app_id = $1 AND status = 'applied' \
          ORDER BY applied_at ASC, submitted_at ASC, migration_id ASC",
        &[app_id],
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let publication =
        zeroship_core::replication_names::publication_name(&app_id.to_string())
            .expect("app id is a valid publication seed");
    conn.batch_execute(&format!(
        "CREATE PUBLICATION {} FOR ALL TABLES",
        quote_ident(&publication)
    ))
    .await
    .expect("create the incompatible publication fixture");

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    let response_status = resp.status();
    let response_body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    let ddl_committed = table_exists(&conn, &app_id.to_string(), "notes").await;
    let ledger = conn
        .query(
            "SELECT status, last_error FROM zeroship.app_schema_applies \
              WHERE app_id = $1 ORDER BY submitted_at ASC, migration_id ASC",
            &[&app_id],
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let post = |body: Value| {
        test::TestRequest::post()
            .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
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
            .configure(zeroship_migrate_server::configure),
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
            .configure(zeroship_migrate_server::configure),
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
            .configure(zeroship_migrate_server::configure),
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
            .configure(zeroship_migrate_server::configure),
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

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
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
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
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&with_policy(
            create_notes_request(),
            escalating_policy(),
        ))
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .set_json(&with_policy(
            create_notes_request(),
            malformed_policy(),
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
async fn real_delegating_authenticator_accepts_apps_deploy_owner_bearer() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let token = platform_token(owner_id, "apps:deploy");

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);
    let caller = authenticator
        .verify_bearer(&token, app_id, Scope::AppsDeploy, "test-request-id")
        .await
        .expect("app owner holding apps:deploy verifies");
    assert_eq!(caller.principal_id, owner_id);

    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &owner_id).await;
}

#[ntex::test]
async fn real_delegating_authenticator_rejects_bearer_without_apps_deploy_scope() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    // The principal OWNS the app; only the scope is short. Cedar would allow an
    // owner `apps:deploy`, so the denial can only come from the token's own
    // scope-derived policy.
    let token = platform_token(owner_id, "apps:read");

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);
    let err = authenticator
        .verify_bearer(&token, app_id, Scope::AppsDeploy, "test-request-id")
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let other_app_id = Uuid::now_v7();
    let other_owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_app(&conn, other_app_id, other_owner_id).await;

    let token = platform_token(owner_id, "apps:deploy");

    let auth_conn = admin_conn().await;
    let authenticator = real_authenticator(auth_conn);
    let caller = authenticator
        .verify_bearer(&token, app_id, Scope::AppsDeploy, "test-request-id")
        .await
        .expect("the same bearer verifies for the app its subject owns");
    assert_eq!(caller.principal_id, owner_id);

    let err = authenticator
        .verify_bearer(&token, other_app_id, Scope::AppsDeploy, "test-request-id")
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
        .verify_bearer("not-a-jwt", Uuid::now_v7(), Scope::AppsDeploy, "test-request-id")
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
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("good-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth.clone());
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;

    // THE APPLY ENDPOINT, because it is the only authenticated one left. This
    // case used to drive `PUT /v1/apps/{app}/policy`, which was deleted with the
    // policy store; a request-id test pointed at a route that 404s before authz
    // runs would observe an empty `seen_request_ids` and read as a failure of the
    // header plumbing rather than of the fixture.
    let caller_request_id = "req-from-the-caller-0001";
    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer good-token")
        .header("x-request-id", caller_request_id)
        .set_json(&create_notes_request())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

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
