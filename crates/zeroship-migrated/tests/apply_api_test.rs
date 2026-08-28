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
use zeroship_migrated::auth::{
    AuthError, Authenticator, ControlPlaneAuthenticator, VerifiedCaller,
};
use zeroship_migrated::policy::{ManagedPolicyConfig, MIGRATE_POLICY_FILENAME};
use zeroship_migrated::MigrationServiceState;

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
    ensure_migrated_service_tables(&client).await;
    client
}

async fn ensure_migrated_service_tables(conn: &Client) {
    conn.batch_execute("SELECT pg_advisory_lock(7330067);")
        .await
        .expect("lock migrated service table setup");
    conn.batch_execute(
        r#"
        CREATE SCHEMA IF NOT EXISTS zeroship;
        CREATE EXTENSION IF NOT EXISTS pgcrypto;

        CREATE TABLE IF NOT EXISTS zeroship.migrated_app_policies (
          app_id uuid NOT NULL,
          version bigint NOT NULL CHECK (version > 0),
          raw_toml text NOT NULL,
          parsed_profile jsonb NOT NULL,
          effective_profile jsonb NOT NULL,
          ceiling_id text NOT NULL,
          ceiling_version bigint NOT NULL CHECK (ceiling_version > 0),
          submitted_by uuid NOT NULL,
          submitted_at timestamptz NOT NULL DEFAULT now(),
          PRIMARY KEY (app_id, version)
        );

        CREATE TABLE IF NOT EXISTS zeroship.migrated_migrations (
          app_id uuid NOT NULL,
          migration_id uuid NOT NULL,
          status text NOT NULL CHECK (
            status IN ('planned', 'pending_approval', 'approved', 'applied', 'rejected')
          ),
          request_body jsonb NOT NULL,
          effective_profile jsonb NOT NULL,
          ceiling_id text NOT NULL,
          ceiling_version bigint NOT NULL CHECK (ceiling_version > 0),
          gated_versions jsonb NOT NULL DEFAULT '[]'::jsonb,
          submitted_by uuid NOT NULL,
          submitted_at timestamptz NOT NULL DEFAULT now(),
          approved_by uuid,
          approved_at timestamptz,
          applied_at timestamptz,
          approved_checksum text,
          descriptor_sha256 text,
          last_error text,
          PRIMARY KEY (app_id, migration_id)
        );

        CREATE TABLE IF NOT EXISTS zeroship.migrated_migration_audit (
          audit_id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
          app_id uuid NOT NULL,
          migration_id uuid NOT NULL,
          migration_versions jsonb NOT NULL DEFAULT '[]'::jsonb,
          action text NOT NULL CHECK (action IN ('submit', 'reject_pending', 'approve', 'apply')),
          outcome text NOT NULL,
          principal_id uuid NOT NULL,
          effective_profile jsonb NOT NULL,
          sealed_profile jsonb,
          ceiling_id text NOT NULL,
          ceiling_version bigint NOT NULL CHECK (ceiling_version > 0),
          detail jsonb NOT NULL DEFAULT '{}'::jsonb,
          created_at timestamptz NOT NULL DEFAULT now()
        );

        CREATE INDEX IF NOT EXISTS migrated_app_policies_app_submitted_idx
          ON zeroship.migrated_app_policies (app_id, submitted_at DESC);
        CREATE INDEX IF NOT EXISTS migrated_migrations_app_status_idx
          ON zeroship.migrated_migrations (app_id, status, submitted_at DESC);
        CREATE INDEX IF NOT EXISTS migrated_migration_audit_app_idx
          ON zeroship.migrated_migration_audit (app_id, migration_id, created_at);
        "#,
    )
    .await
    .expect("ensure migrated service tables");
    conn.batch_execute("SELECT pg_advisory_unlock(7330067);")
        .await
        .expect("unlock migrated service table setup");
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
        policy_config.parse_draft(&zeroship_migrated::policy::CreatorPolicyDraft {
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
    policy_store_dsn: String,
    policy_config: ManagedPolicyConfig,
) -> (Arc<MigrationServiceState>, PathBuf) {
    assert_policy_fixtures_are_current(&policy_config);
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
// 2026-08-10 (see `crates/migrated/policies/confined.policy.toml`) and not from
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
    // The stored `effective_profile` is now the managed-posture audit snapshot
    // (`{require_rls, destructive_ops, extensions}`); `destructive_ops` renders the
    // `DestructiveOps` Debug name.
    assert_eq!(body["effective_profile"]["destructive_ops"], "Forbid");

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
            .contains("sql.raw"),
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

/// Every applied `(descriptor_sha256, applied_at)` for an app, newest last.
async fn applied_descriptors(conn: &Client, app_id: &Uuid) -> Vec<Option<String>> {
    conn.query(
        "SELECT descriptor_sha256 FROM zeroship.migrated_migrations \
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
            .configure(zeroship_migrated::configure),
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

// A `safety.require_approval = "on_destructive"` draft gates ONLY destructive migrations:
// an additive create applies without approval, a DROP is held `pending_approval`.
fn on_destructive_policy() -> &'static str {
    "policy_version = 1\n\n[[require]]\nkey = \"safety.require_approval\"\nvalue = \"on_destructive\"\nscope = \"all\"\n\n[[grant]]\nkey = \"safety.destructive_ops\"\nvalue = \"allow\"\nscope = \"all\"\n"
}

#[ntex::test]
async fn on_destructive_gates_destructive_migration_only_pg() {
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

    // Additive create under `on_destructive` → NO approval needed, applies directly.
    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&with_policy(create_notes_request(), on_destructive_policy()))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "additive migration under on_destructive must not require approval"
    );
    assert!(table_exists(&conn, &app_id.to_string(), "notes").await);

    // A DROP under `on_destructive` → held pending_approval (409).
    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&with_policy(drop_notes_request(), on_destructive_policy()))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "destructive migration under on_destructive must require approval"
    );
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
    assert_eq!(body["error"], "migration_requires_operator_approval");
    let migration_id = body["migration_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .expect("pending migration id");
    let (status, _) = migration_status(&conn, &app_id, &migration_id)
        .await
        .expect("pending row exists");
    assert_eq!(status, "pending_approval");
    assert!(
        table_exists(&conn, &app_id.to_string(), "notes").await,
        "pending destructive migration must not drop the table"
    );

    let _ = std::fs::remove_dir_all(tmp);
    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &operator_id).await;
}

// The migration-store state machine directly: plan (pending) → approve (stamps
// approved_checksum = X) → drift-detected revert (approved_checksum ≠ X') → pending.
#[ntex::test]
async fn store_state_machine_plan_approve_and_content_drift_revert_pg() {
    use zeroship_migrated::migration_store::{MigrationStore, StoreMigrationInput};
    use zeroship_migrated::policy::ManagedPosture;
    use zeroship_migrate::DestructiveOps;

    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    let operator_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;
    seed_user(&conn, operator_id, "operator").await;

    let store = MigrationStore::new(dsn());
    let migration_id = Uuid::now_v7();
    let posture = ManagedPosture {
        require_rls: false,
        destructive_ops: DestructiveOps::Allow,
        extensions: vec![],
    };
    let input = || StoreMigrationInput {
        app_id,
        migration_id,
        principal_id: owner_id,
        request_body: json!({"kind": "ir", "documents": []}),
        effective_profile: &posture,
        ceiling_id: "confined-default",
        ceiling_version: 1,
        gated_versions: &[],
        descriptor_sha256: TEST_DESCRIPTOR_SHA256,
    };

    // PLAN (requires approval) → pending_approval, no approved_checksum yet.
    store.insert_pending(input()).await.expect("insert pending");
    let (status, _) = migration_status(&conn, &app_id, &migration_id)
        .await
        .expect("row exists");
    assert_eq!(status, "pending_approval");
    assert!(approved_checksum(&conn, &app_id, &migration_id).await.is_none());

    // APPROVE stamps status=approved + approved_checksum = X.
    let x = "checksum-X";
    store
        .mark_approved(app_id, migration_id, operator_id, x)
        .await
        .expect("approve");
    let (status, _) = migration_status(&conn, &app_id, &migration_id)
        .await
        .expect("row exists");
    assert_eq!(status, "approved");
    assert_eq!(
        approved_checksum(&conn, &app_id, &migration_id).await.as_deref(),
        Some(x)
    );

    // A double-approve of an already-approved row is a no-op (guarded on
    // status='pending_approval').
    let err = store
        .mark_approved(app_id, migration_id, operator_id, "checksum-Y")
        .await;
    assert!(err.is_err(), "re-approving a non-pending row must fail");

    // CONTENT DRIFT: the re-resolved checksum X' ≠ approved_checksum → revert to
    // pending_approval, clearing the stale approval.
    store
        .revert_to_pending(app_id, migration_id, "content drifted")
        .await
        .expect("revert");
    let (status, last_error) = migration_status(&conn, &app_id, &migration_id)
        .await
        .expect("row exists");
    assert_eq!(status, "pending_approval");
    assert_eq!(last_error.as_deref(), Some("content drifted"));
    assert!(
        approved_checksum(&conn, &app_id, &migration_id).await.is_none(),
        "revert must clear the stale approved_checksum"
    );

    cleanup_app(&conn, &app_id).await;
    cleanup_user(&conn, &operator_id).await;
}

async fn approved_checksum(conn: &Client, app_id: &Uuid, migration_id: &Uuid) -> Option<String> {
    let rows = conn
        .query(
            "SELECT approved_checksum FROM zeroship.migrated_migrations \
              WHERE app_id = $1 AND migration_id = $2",
            &[app_id, migration_id],
        )
        .await
        .expect("query approved_checksum");
    rows.first().and_then(|row| row.get("approved_checksum"))
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
    assert_eq!(status, "rejected");
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
    assert_eq!(status, "rejected");
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
            .configure(zeroship_migrated::configure),
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
            .configure(zeroship_migrated::configure),
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
            .configure(zeroship_migrated::configure),
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
            .configure(zeroship_migrated::configure),
    )
    .await;

    let caller_request_id = "req-from-the-caller-0001";
    let req = test::TestRequest::put()
        .uri(&format!("/v1/apps/{app_id}/policy"))
        .header("authorization", "Bearer good-token")
        .header("x-request-id", caller_request_id)
        .set_payload(tighter_policy())
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

/// Re-submitting the SAME gated migration must reuse the pending row, not mint a
/// second one.
///
/// A creator's CI retries a failed deploy; each retry used to insert another
/// `pending_approval` row with a fresh id. The operator then sees N rows for one
/// decision, approving one leaves N-1 stale rows pending forever, and nothing
/// reaps them. The content is what gets approved, so identical content is one
/// pending migration however many times it is submitted.
#[ntex::test]
async fn resubmitting_a_gated_migration_reuses_the_pending_row_pg() {
    let conn = admin_conn().await;
    let app_id = Uuid::now_v7();
    let owner_id = Uuid::new_v4();
    seed_app(&conn, app_id, owner_id).await;

    let auth = Arc::new(StaticAuthenticator::new());
    auth.insert("creator-token", owner_id, [Scope::AppsDeploy], [app_id]);
    let (state, tmp) = state_for(auth);
    let svc = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrated::configure),
    )
    .await;

    // Create the table first so the DROP below is a real destructive change.
    let req = test::TestRequest::post()
        .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
        .header("authorization", "Bearer creator-token")
        .set_json(&create_notes_request())
        .to_request();
    assert_eq!(test::call_service(&svc, req).await.status(), StatusCode::OK);

    // Submit the gated migration twice with byte-identical bodies.
    let gated = with_policy(drop_notes_request(), require_approval_policy());
    let mut ids = Vec::new();
    for attempt in 0..2 {
        let req = test::TestRequest::post()
            .uri(&format!("/v1/apps/{app_id}/migrations/apply"))
            .header("authorization", "Bearer creator-token")
            .set_json(&gated)
            .to_request();
        let resp = test::call_service(&svc, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "attempt {attempt} must be held for approval"
        );
        let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("json body");
        assert_eq!(body["error"], "migration_requires_operator_approval");
        ids.push(
            body["migration_id"]
                .as_str()
                .and_then(|raw| Uuid::parse_str(raw).ok())
                .expect("pending migration id"),
        );
    }

    assert_eq!(
        ids[0], ids[1],
        "a re-submission must name the migration already awaiting approval"
    );

    let rows = conn
        .query(
            "SELECT count(*)::bigint AS n FROM zeroship.migrated_migrations \
              WHERE app_id = $1 AND status = 'pending_approval'",
            &[&app_id],
        )
        .await
        .expect("count pending");
    let pending: i64 = rows[0].get("n");
    assert_eq!(
        pending, 1,
        "two submissions of one migration must leave ONE pending row, found {pending}"
    );

    drop(tmp);
    cleanup_app(&conn, &app_id).await;
}
