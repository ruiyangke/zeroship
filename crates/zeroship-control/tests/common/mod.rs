#![allow(dead_code)]

pub mod authz_fixture;
pub mod deployments;
pub mod stripe_mock;

use std::sync::mpsc;
use std::sync::OnceLock;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::web::{self, HttpResponse};
use serde_json::json;
use uuid::Uuid;
use zeroship_control::Registry;
use zeroship_core::auth_provider::{AuthProvider, PlatformConfig, PlatformProvider};
use zeroship_core::{AppId, UserId};

pub const PLATFORM_ISSUER: &str = "https://auth.zeroship.test/oauth2";
/// The single execution zone these migrations seed
/// (`db/migrations-ts/20260914000450_execution_zones_default_zone.ts`).
pub const DEFAULT_EXECUTION_ZONE_ID: &str = "ezn_default000000000000000000";
const PLATFORM_KID: &str = "platform-control-test-kid";
const PLATFORM_KEY_SEED: u8 = 47;

/// Wait for every Postgres connection this test opened to actually close, then
/// report if any is still live.
///
/// Every `#[compio::test]` builds a private compio runtime and tears it down the
/// instant the test body returns. A connection's socket is owned by a detached
/// driver task, and dropping the `Client` only *asks* that task to shut down -
/// the `Terminate` write and the socket drop still have to be driven. If the
/// runtime goes away first the socket is orphaned: an io_uring submission
/// co-owns the descriptor and it is never reclaimed, so the descriptor and the
/// server-side backend survive for the whole process. Connections then
/// accumulate across a binary's tests until `max_connections` is the ceiling
/// the suite trips on.
///
/// Contract for callers: drop every handle that owns a connection FIRST - the
/// fixture holding `AppState` (its `control_pg` client) and the ntex test
/// service holding a cloned `Arc<AppState>` are both still alive at the end of
/// a test body, because Rust drops locals in reverse declaration order only
/// once the scope ends. A handle that is still alive keeps its connection
/// counted and makes this wait out its whole budget.
///
/// Timeouts print rather than fail: the leak this guards against is precisely
/// the kind nothing reported, so a silent teardown would repeat the defect.
/// Run with `--nocapture` to see the message.
#[allow(dead_code)]
pub async fn drain_pg() {
    if !compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await {
        eprintln!(
            "DRAIN-TIMEOUT: {} connection(s) still live",
            compio_postgres::live_connections()
        );
    }
}

/// The migrated PostgreSQL instance owned by this test binary.
pub fn require_control_db() -> String {
    test_database::url()
}

/// Refuse the calling test because a backend it requires is not there.
///
/// A missing non-database backend is a failed test rather than a skipped one.
///
/// A refusal names the REMEDY, not just the gap: `remedy` is the command to
/// run and what it does, so a first encounter needs no source dive.
#[track_caller]
pub fn refuse_missing_backend(backend: &str, problem: &str, remedy: &str) -> ! {
    panic!(
        "REFUSED: this test requires {backend}, and it is not there.\n\
         \n\
         \x20   backend   {backend}\n\
         \x20   problem   {problem}\n\
         \n\
         \x20   NO VERDICT WAS REACHABLE. The subject never ran, so this\n\
         \x20   failure says nothing about the code.\n\
         \n\
         \x20   {remedy}\n\
         \n\
         \x20   There is no environment variable that makes this a skip. A\n\
         \x20   backend this suite cannot reach is a failed run, not a green\n\
         \x20   one."
    )
}

pub struct PlatformJwks {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlatformJwks {
    pub fn start() -> Self {
        let body = Arc::new(RwLock::new(platform_jwks_body()));
        let factory_body = body.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-common-platform-jwks")
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

    pub fn jwks_url(&self) -> String {
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

/// The JWKS endpoint a control test's platform auth provider must point at.
///
/// One server per test BINARY, started on first use and never shut down. The
/// OAuth bearer path is the ONLY principal path into control, and it verifies
/// the access token against the issuer's published key over HTTP - so a fixture
/// that hands out a platform bearer has to leave a reachable JWKS behind it for
/// as long as any request may use that bearer. Held in a `OnceLock` static,
/// which is never dropped, so the server outlives every test in the binary.
///
/// Before PATs were removed most fixtures pointed the provider at
/// `http://127.0.0.1:9/...` - deliberately unreachable, because those tests
/// authenticated with a locally-signed PAT and never reached the OAuth arm.
pub fn platform_jwks_url() -> String {
    static JWKS: OnceLock<PlatformJwks> = OnceLock::new();
    JWKS.get_or_init(PlatformJwks::start).jwks_url()
}

pub fn platform_auth_provider(jwks_url: String) -> Arc<AuthProvider> {
    Arc::new(AuthProvider::platform(PlatformProvider::new(
        PlatformConfig::new(PLATFORM_ISSUER, Some(jwks_url)).expect("platform config"),
    )))
}

/// A first-party CLI bearer. `zeroship-cli` is the one client id whose scopes
/// control intersects with the principal's live `zeroship.principal_grants`
/// rows, so a token minted here carries at most
/// `PLATFORM_CLI_ISSUABLE_SCOPES` unless the test seeds grants of its own.
pub fn platform_token(subject: &UserId, scope: &str) -> String {
    platform_token_for_client(subject, scope, "zeroship-cli")
}

/// The console BFF's client id. Distinct from `zeroship-cli` on purpose: it is
/// NOT the client whose scopes control narrows against the CLI grant set, so a
/// console bearer carries exactly the scopes it was minted with. Fixtures for
/// operator and billing surfaces use this, because those scopes are outside the
/// CLI's issuable set.
pub const CONSOLE_CLIENT_ID: &str = "zeroship-console";

pub fn platform_token_for_client(subject: &UserId, scope: &str, client_id: &str) -> String {
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

pub fn platform_bearer(subject: &UserId, scope: &str) -> String {
    format!("Bearer {}", platform_token(subject, scope))
}

pub fn console_bearer(subject: &UserId, scope: &str) -> String {
    format!(
        "Bearer {}",
        platform_token_for_client(subject, scope, CONSOLE_CLIENT_ID)
    )
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

/// Give a test-seeded app the workflow journal schema a deployed app has.
///
/// A test seeds an app by INSERTing a row into `zeroship.apps`. Production gets
/// an app that way only as the first half of a deploy: the second half is the
/// migration apply, and that is what creates the app's `app_<uuid>` journal
/// schema and hands it to the narrow `zeroship_workflow_owner` role. So a
/// seeded app has no journal schema, and `PgStore::provision` - which
/// deliberately holds no CREATE and creates no schema of its own (2a44ea8ef,
/// pinned by `worker_provisioning_uses_a_precreated_narrow_owner_role`) - fails
/// with `schema "app_<uuid>" does not exist` until this runs.
///
/// Calls the migration service's own provisioning function rather than issuing
/// a CREATE SCHEMA here. A hand-rolled one in test code would make the same
/// tests pass over a schema owned by whoever the test connected as, which is a
/// privilege shape production never has - the tests would then be green about a
/// journal nothing in production could have created.
///
/// Call it AFTER inserting the app row and BEFORE `PgStore::provision`.
#[allow(dead_code)]
pub async fn provision_app_workflow_schema(pg: &compio_postgres::Client, app_id: &AppId) {
    zeroship_migrate_server::provisioning::provision_workflow_journal_schema(pg, app_id)
        .await
        .expect("provision app workflow journal schema");
}

/// Idempotently seed the built-in plan tiers (free/pro/unlimited) into the test
/// DB's plan catalog so `create_app`/`set_plan` (which validate `plan_id`
/// against `zeroship.plans`) accept the built-in ids. A no-op on a
/// re-run (ON CONFLICT DO UPDATE on deterministic `pln_…` ids). Tests that
/// create apps with [`zeroship_control::plan_catalog::free_plan_id`] call
/// this in their setup first.
#[allow(dead_code)]
pub async fn ensure_builtin_plans(registry: &Registry) {
    zeroship_control::plan_catalog::seed_plans(registry)
        .await
        .expect("seed built-in plans for test");
}

/// The project a fixture's `zeroship.apps` row belongs to.
///
/// `apps.project_id` is NOT NULL against a RESTRICT foreign key, so a fixture
/// that INSERTs an app row directly has to name one. This mints (or finds) the
/// owner's PERSONAL organization and its default project through the same
/// production function the zero-config create path uses, so a fixture cannot
/// seed a shape production would never write - which is the failure mode a
/// hand-rolled `INSERT INTO zeroship.organizations` fixture would have.
///
/// Idempotent per owner: call it once per fixture app or once per owner, the
/// answer is the same project.
///
/// A test that needs the app to sit somewhere ELSE - a shared organization, or
/// a project the owner reaches only through a `project_members` row - must NOT
/// use this. It should build that shape explicitly, because the placement is
/// then the thing under test.
#[allow(dead_code)]
pub async fn personal_project_for(registry: &Registry, owner: &UserId) -> String {
    zeroship_control::organizations::ensure_personal_project(registry, owner)
        .await
        .unwrap_or_else(|err| panic!("provision personal project for {}: {err:?}", owner.as_str()))
        .as_str()
        .to_string()
}

/// A project for a fixture app whose OWNERSHIP is not what the test is about.
///
/// Seeds an organization with NO members and a project inside it. An app placed
/// here is owner-less by construction, which is the right shape for two kinds of
/// test and the wrong shape for a third:
///
/// - RIGHT for a test about something other than authority (workflow admission,
///   OAuth client provisioning): the app needs a home, not a creator.
/// - RIGHT for the orphaned-app reaper, whose whole subject IS an owner-less
///   app - it is the state the reaper exists to find.
/// - WRONG for anything asserting an authorization outcome. Use
///   [`personal_project_for`] and seat members through
///   `zeroship_control::organizations`, so the fixture goes through the same
///   rank fence production does.
///
/// It writes the rows directly rather than through the module, because
/// `create_organization` necessarily seats its caller as owner and an
/// owner-less organization is precisely what this is for.
#[allow(dead_code)]
pub async fn unowned_project(pg: &compio_postgres::Client) -> String {
    unowned_project_in(pg, &seed_organization(pg).await).await
}

/// A fresh member-less `zeroship.organizations` row: THE BILLING SUBJECT.
///
/// Every billing fixture in this crate starts here rather than at
/// `zeroship.users`. That is the shape change, not a rename: the party an
/// invoice, a payout, a Connect account and a `billing_notifications` claim key
/// on is an organization, and a test that mints a user and passes its uuid as
/// the subject would not fail loudly - the column is `text`, so the id would
/// simply match no organization row and the FK would refuse it at write time
/// with an error about the wrong thing.
///
/// The address is `billing_email`, which is where notices go now, so a notify
/// test can assert on it without inventing a member.
#[allow(dead_code)]
pub async fn seed_organization(pg: &compio_postgres::Client) -> String {
    let organization_id = zeroship_core::typed_id::generate("org");
    let slug = format!("fixture-{}", Uuid::new_v4().simple());
    pg.execute(
        "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
         VALUES ($1, $2, 'Fixture Organization', $3)",
        &[&organization_id, &slug, &format!("{slug}@zeroship.test")],
    )
    .await
    .expect("seed fixture organization");
    organization_id
}

/// A project inside an organization the caller already has, for a test that
/// needs several apps to share ONE billing subject.
#[allow(dead_code)]
pub async fn unowned_project_in(pg: &compio_postgres::Client, organization_id: &str) -> String {
    let project_id = zeroship_core::typed_id::generate("prj");
    let slug = format!("prj-{}", Uuid::new_v4().simple());
    pg.execute(
        "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
         VALUES ($1, $2, $3, 'Default')",
        &[&project_id, &organization_id, &slug],
    )
    .await
    .expect("seed fixture project");
    project_id
}

/// A fixture `zeroship.apps` row, together with the ownership chain it now
/// requires. Returns the app id.
///
/// A fixture app is no longer one row. `apps.project_id` is NOT NULL against a
/// RESTRICT foreign key and a project needs an organization, so the chain is
/// organization -> project -> app, and this writes all three. Every direct
/// `INSERT INTO zeroship.apps` in this crate's tests goes through here or
/// through [`unowned_project`], so the chain is spelled once.
///
/// THE ORGANIZATION IS FRESH PER CALL AND HAS NO MEMBERS. That is deliberate:
/// two fixture apps never share a billing subject or an authority root unless a
/// test seats the same member in both, which is the isolation the deleted
/// `zeroship.app_members` row used to give per app. Ownership is therefore NOT
/// expressed here - a test that means "this user owns this app" follows with
/// [`seat_app_organization_member`], the direct replacement for an
/// `app_members` owner row.
///
/// See [`unowned_project`] for which tests a member-less organization is right
/// for, and for what to do instead when the placement IS the thing under test.
#[allow(dead_code)]
pub async fn seed_app(pg: &compio_postgres::Client, name: &str, plan_id: &str) -> AppId {
    let organization_id = seed_organization(pg).await;
    seed_app_in_organization(pg, name, plan_id, &organization_id).await
}

/// A fixture app whose BILLING SUBJECT the caller names.
///
/// `apps.organization_id` is `NOT NULL` and consumed by the composite key
/// `(project_id, organization_id) -> projects(id, organization_id)`, so it is
/// written here from the same value the project was created under. Passing a
/// different organization is not a fixture that mis-attributes usage; it is a
/// row PostgreSQL refuses.
#[allow(dead_code)]
pub async fn seed_app_in_organization(
    pg: &compio_postgres::Client,
    name: &str,
    plan_id: &str,
    organization_id: &str,
) -> AppId {
    let project_id = unowned_project_in(pg, organization_id).await;
    let app_id = AppId::mint();
    // The zone is named, not defaulted: the column carries no default, the
    // way Control names one when it creates an app.
    pg.execute(
        "INSERT INTO zeroship.apps \
             (id, name, plan_id, project_id, organization_id, execution_zone_id) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &app_id.as_str(),
            &name,
            &plan_id,
            &project_id,
            &organization_id,
            &DEFAULT_EXECUTION_ZONE_ID,
        ],
    )
    .await
    .expect("seed fixture app");
    app_id
}

/// A fixture app in a named execution zone.
///
/// An app's zone is frozen by trigger once written, so it can only be chosen
/// at creation: a test that needs an app outside the deployment's default zone
/// has to say so here rather than update the row afterwards.
#[allow(dead_code)]
pub async fn seed_app_in_zone(
    pg: &compio_postgres::Client,
    name: &str,
    plan_id: &str,
    execution_zone_id: &str,
) -> AppId {
    let organization_id = seed_organization(pg).await;
    let project_id = unowned_project_in(pg, &organization_id).await;
    let app_id = AppId::mint();
    pg.execute(
        "INSERT INTO zeroship.apps \
             (id, name, plan_id, project_id, organization_id, execution_zone_id) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &app_id.as_str(),
            &name,
            &plan_id,
            &project_id,
            &organization_id,
            &execution_zone_id,
        ],
    )
    .await
    .expect("seed fixture app in a named zone");
    app_id
}

/// Seat `user` directly in `organization` at `role`.
///
/// The peer of [`seat_app_organization_member`] for a fixture that has an
/// organization but no app: the billing routes gate on money authority at
/// `Resource::Organization`, so a token with no seat there is refused however
/// many apps its holder owns elsewhere.
#[allow(dead_code)]
pub async fn seat_organization_member(
    pg: &compio_postgres::Client,
    organization_id: &str,
    user: &UserId,
    role: &str,
) {
    pg.execute(
        "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        &[&organization_id, &user.as_str(), &role],
    )
    .await
    .expect("seat organization member");
}

/// The organization one fixture app bills, read back off the app row.
#[allow(dead_code)]
pub async fn app_organization(pg: &compio_postgres::Client, app: &AppId) -> String {
    pg.query(
        "SELECT organization_id FROM zeroship.apps WHERE id = $1",
        &[&app.as_str()],
    )
    .await
    .expect("read app organization")
    .first()
    .expect("fixture app exists")
    .get("organization_id")
}

/// Seat `user` in the organization behind `app`'s project, at `role`.
///
/// The replacement for a `zeroship.app_members` row. An app reaches its
/// authority root through exactly one path - `apps.project_id ->
/// projects.organization_id` - so the seat is written by joining that path
/// rather than by the caller carrying an organization id it would have to keep
/// in agreement.
///
/// `role` must name a row in `zeroship.organization_roles`; the foreign key
/// there makes an invented role unspellable rather than silently powerless.
/// Upserts, so a test may promote or demote the same user by calling again.
///
/// IT READS THE AFFECTED-ROW COUNT, and that is the whole reason this is not a
/// one-line `execute`. `INSERT ... SELECT` over a result set that matched
/// NOTHING is a SUCCESSFUL statement affecting no rows: an app id that does not
/// exist, or an app whose project row was never written, seats nobody and
/// reports nothing. The test then fails much later as a 403 from whichever
/// route wanted an owner, with no line pointing back at the fixture. The shell
/// peer of this function (`seat_app_owner` in `tests/lib/organization_fixture.sh`)
/// reads a token back out of the database for exactly the same reason.
#[allow(dead_code)]
pub async fn seat_app_organization_member(
    pg: &compio_postgres::Client,
    app: &AppId,
    user: &UserId,
    role: &str,
) {
    let seated = pg
        .execute(
            "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
             SELECT p.organization_id, $2, $3 FROM zeroship.apps a \
               JOIN zeroship.projects p ON p.id = a.project_id WHERE a.id = $1 \
             ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role",
            &[&app.as_str(), &user.as_str(), &role],
        )
        .await
        .expect("seat organization member for fixture app");
    assert_eq!(
        seated,
        1,
        "seating {} as '{role}' on app {} affected {seated} row(s). The \
         INSERT ... SELECT matched no app reaching an organization through \
         apps.project_id -> projects.organization_id, which is a SUCCESSFUL \
         statement that seats nobody. Create the app before seating it.",
        user.as_str(),
        app.as_str()
    );
}

#[allow(dead_code)]
pub async fn seed_usage_total(
    pg: &compio_postgres::Client,
    app: &AppId,
    period_start_unix: i64,
    metric: &str,
    total: i64,
) {
    seed_metric_catalog(pg, metric).await;
    pg.execute(
        "INSERT INTO zeroship.usage_aggregates AS u (app_id, period, metric, total, updated_at) \
         VALUES ($1, $2::date, $3, $4, NOW()) \
         ON CONFLICT (app_id, period, metric) DO UPDATE SET \
           total = EXCLUDED.total, updated_at = NOW()",
        &[
            &app.as_str(),
            &period_date(period_start_unix),
            &metric,
            &total,
        ],
    )
    .await
    .expect("seed usage total");
}

#[allow(dead_code)]
pub async fn seed_usage_delta(
    pg: &compio_postgres::Client,
    app: &AppId,
    period_start_unix: i64,
    metric: &str,
    delta: i64,
) {
    seed_metric_catalog(pg, metric).await;
    pg.execute(
        "INSERT INTO zeroship.usage_aggregates AS u (app_id, period, metric, total, updated_at) \
         VALUES ($1, $2::date, $3, $4, NOW()) \
         ON CONFLICT (app_id, period, metric) DO UPDATE SET \
           total = u.total + EXCLUDED.total, updated_at = NOW()",
        &[
            &app.as_str(),
            &period_date(period_start_unix),
            &metric,
            &delta,
        ],
    )
    .await
    .expect("seed usage delta");
}

#[allow(dead_code)]
pub async fn seed_metric_catalog(pg: &compio_postgres::Client, metric: &str) {
    pg.execute(
        "INSERT INTO zeroship.billing_metrics (metric, kind, unit) \
         VALUES ($1, 'platform', 'op') \
         ON CONFLICT (metric) DO UPDATE SET unit = EXCLUDED.unit",
        &[&metric],
    )
    .await
    .expect("seed billing metric");
}

/// Idempotently seed the platform pricing-catalog singletons that billing tests
/// assume already exist: the global `pricing_config` FX row (`FX_SCALE` = 1c per
/// unit, well above the floor) plus the `requests` platform counter in
/// `billing_metrics` + `metric_weights`.
///
/// Migrations create these tables but seed NO rows — the FX is operator data and
/// the pricing engine fails CLOSED when it's absent (see
/// `missing_default_fx_aborts_sweep_and_bills_no_one`). So a fixture that reads
/// the global FX / platform weights must seed them itself instead of depending
/// on some other test binary having run first (nondeterministic under the
/// suite's per-binary parallel runner, and impossible on a freshly-migrated DB).
/// Uses `ON CONFLICT DO NOTHING` so it never clobbers a concurrent test's
/// in-flight FX/weight mutation.
#[allow(dead_code)]
pub async fn seed_pricing_catalog(pg: &compio_postgres::Client) {
    seed_metric_catalog(pg, "requests").await;
    pg.execute(
        "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
         VALUES ('requests', 1, 1) ON CONFLICT (metric) DO NOTHING",
        &[],
    )
    .await
    .expect("seed metric weight");
    // FX_SCALE = 1_000_000_000_000 (10^12) — the canonical 1c/unit default FX.
    pg.execute(
        "INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) \
         VALUES ('global', 1000000000000) ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .expect("seed global pricing_config");
}

#[allow(dead_code)]
pub fn period_date(period_start_unix: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc
        .timestamp_opt(period_start_unix, 0)
        .single()
        .unwrap_or_else(chrono::Utc::now);
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).expect("valid first-of-month period")
}

/// Months per caller. See "THE CONTRACT FOR CALLERS" on `next_isolated_period`.
const STRIDE_MONTHS: u32 = 4;
/// First month handed out, as an offset from the run's own base. Starts past
/// zero so the earliest window's back-reach stays inside the run.
const FIRST_OFFSET_MONTHS: u32 = 4;
/// Windows available to ONE RUN before its arithmetic leaves the region the next
/// run's base scan will look above. This asserts rather than wrapping into a
/// month another caller already owns.
const MAX_WINDOWS: u32 = 600;

/// Where `window` lands, in months from the band's first month, for a run whose
/// base is `run_base`.
///
/// SPLIT OUT SO ONE PROPERTY CAN BE BOUND WITHOUT A SECOND RUN: that the run's
/// base is IN the address, not merely resolved beside it. A fresh database
/// resolves a base of zero, so a formula that dropped the term returns exactly
/// the same months there and the defect is invisible until the second run - the
/// whole reason it survived. `the_run_base_is_part_of_the_window_address` in
/// `billing_safety_net_test` compares two bases through this function instead.
#[allow(dead_code)]
pub fn isolated_period_offset_months(run_base: u32, window: u32) -> u32 {
    run_base + FIRST_OFFSET_MONTHS + window * STRIDE_MONTHS
}

/// Hand this caller a far-future billing window that NO other caller can touch,
/// AND that no PREVIOUS RUN against this database has already touched.
///
/// THIS LOOKS LIKE AN ORDINARY HELPER AND IS NOT. Read this before changing it,
/// and before adding a caller.
///
/// WHAT IT GUARANTEES
/// ------------------
/// Two axes, and both are needed:
///
///   WITHIN a run   Every call returns an instant `STRIDE_MONTHS` after the
///                  previous call's, so the windows callers touch are DISJOINT
///                  BY CONSTRUCTION. Not "unlikely to collide" - disjoint.
///   ACROSS runs    The whole run sits ABOVE every period any earlier run left
///                  in this database, because the first call READS the database
///                  and starts the run one month past the highest period any
///                  `date` column in the `zeroship` schema holds inside the
///                  band. A month a previous run seeded is never reissued.
///
/// WHY THE SECOND AXIS EXISTS. The counter is process-local and the months were
/// FIXED calendar months, so every run handed out the same window sequence.
/// Within one process that is isolation; against one database it is a
/// guarantee that expires when the process does. `reconcile_pass` sweeps a
/// period FLEET-WIDE, so run N counted every prior run's subjects and the
/// `subjects_checked` assertions in `billing_safety_net_test` failed on the
/// second run against the same database - by an exact multiple of the run
/// count, which is what a re-seeded shared period looks like. The suite was
/// green only when a database reset had happened immediately before it.
///
/// WHY READING THE MAX IS SOUND, and not merely likely. Each run's own rows
/// land at or above its base, and its base is one month past the previous
/// maximum, so the maximum STRICTLY INCREASES across runs that seed anything.
/// A run that seeds nothing leaves it where it was and has nothing to collide
/// with. The scan is over the CATALOG rather than a hand-written table list, so
/// a new table with a period column is covered the day it exists; a list would
/// go stale silently, which is the same class of defect as the one above.
///
/// The band is finite. `ISOLATED_PERIOD_CEILING_YEAR` bounds it, the scan
/// ignores anything at or above the ceiling (so a far-future sentinel written
/// by some other subsystem cannot pin every run to one base), and running off
/// the top is an assert naming the repair - drop and recreate the test
/// database - rather than a silent wrap onto months already seeded.
///
/// DO NOT REPLACE THIS WITH A LOCK
/// -------------------------------
/// A mutex around the sweeps cannot fix what this fixes. The rows OUTLIVE the
/// lock, and the collision is a later test taking an earlier test's month -
/// which serialisation does not prevent, because the two tests were never
/// concurrent in the first place. The gate has always run `--test-threads 1`.
/// The defect is address allocation, not concurrency.
///
/// TWO RUNS AT ONCE AGAINST ONE DATABASE STILL COLLIDE, and always did: both
/// would read the same maximum and claim the same base. Nothing here makes
/// that safe, and nothing else in this directory does either - the modules
/// share one Postgres and one role. Run them one at a time.
///
/// THE CONTRACT FOR CALLERS
/// ------------------------
/// ONE CALL PER TEST. Bind it to a local and reuse that local; a second call
/// gives you a DIFFERENT window, which is the point.
///
/// You may derive earlier months from the returned instant: the window reserved
/// for you is the returned month and the `STRIDE_MONTHS - 1` months before it.
/// Callers reach at most two months back (`prev_period(now)` and, in one test,
/// `prev_period(now - 40 days)`), and the self-invoicing safety net reads the
/// returned month itself, so the stride leaves slack. Reach further back than
/// that and you are in your neighbour's window.
#[allow(dead_code)]
pub async fn next_isolated_period() -> i64 {
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU32, Ordering};

    let run_base = run_band_base_months().await;

    static NEXT_WINDOW: AtomicU32 = AtomicU32::new(0);
    let window = NEXT_WINDOW.fetch_add(1, Ordering::Relaxed);
    assert!(
        window < MAX_WINDOWS,
        "next_isolated_period() exhausted its {MAX_WINDOWS} private windows. \
         Raise MAX_WINDOWS (the band has room; see ISOLATED_PERIOD_BASE_YEAR); \
         do NOT wrap, because wrapping silently reissues a month another test \
         already seeded, which is the exact bug this replaced."
    );

    let offset = isolated_period_offset_months(run_base, window);
    let year = ISOLATED_PERIOD_BASE_YEAR + (offset / 12) as i32;
    let month = offset % 12 + 1;
    assert!(
        year < ISOLATED_PERIOD_CEILING_YEAR,
        "the isolated billing period band is exhausted on this database: the \
         next window would land in {year}, at or past \
         ISOLATED_PERIOD_CEILING_YEAR ({ISOLATED_PERIOD_CEILING_YEAR}), which \
         the base scan does not look at. Every run consumes a slice of the band \
         and never gives it back, so the repair is to drop and recreate the test \
         database. Do NOT raise the ceiling to get past this - the months above \
         it are invisible to the scan, so windows there would be reissued to \
         every later run."
    );
    chrono::Utc
        .with_ymd_and_hms(year, month, 15, 12, 0, 0)
        .single()
        .expect("valid isolated billing period")
        .timestamp()
}

/// Months from the band's first month to this RUN's first month, resolved once
/// per process against the database the run is about to write to.
///
/// Memoised: the scan costs one connection per test binary, not one per test.
/// The value is resolved BEFORE any window is handed out, so the rows it sees
/// in the band are, by construction, previous runs' and never this one's.
async fn run_band_base_months() -> u32 {
    static RUN_BASE: OnceLock<u32> = OnceLock::new();
    if let Some(base) = RUN_BASE.get() {
        return *base;
    }
    let resolved = resolve_run_band_base().await;
    // A race can only be lost to a value computed the same way from the same
    // database, so the loser adopting the winner's base is correct rather than
    // merely tolerable.
    let _ = RUN_BASE.set(resolved);
    *RUN_BASE.get().expect("run band base is resolved")
}

/// Read the highest period any `date` column in the `zeroship` schema holds
/// inside the band, and return the month AFTER it.
///
/// THE COLUMN SET COMES FROM THE CATALOG, not from a list in this file. Several
/// tables carry a `zeroship.billing_period` column today and the next one would
/// be covered the day it is created; a hand-written list would keep printing a
/// base while quietly stopping short of a table that had started holding
/// periods. `typbasetype` is what makes the domain visible - `billing_period`
/// is a domain over `date`, so a match on the column's own type name finds
/// nothing.
///
/// NOT MEMOISED, deliberately: `run_band_base_months` is the memoised one, and
/// this is also what `a_later_run_starts_above_every_period_this_run_seeded`
/// calls to ask what a LATER run would resolve after this one has written.
#[allow(dead_code)]
pub async fn resolve_run_band_base() -> u32 {
    let db_url = require_control_db();
    let (client, connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect to resolve the isolated billing period band base");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let columns = client
        .query(
            "SELECT c.relname AS table_name, a.attname AS column_name \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_type t ON t.oid = a.atttypid \
             WHERE n.nspname = 'zeroship' \
               AND c.relkind = 'r' \
               AND a.attnum > 0 \
               AND NOT a.attisdropped \
               AND COALESCE(NULLIF(t.typbasetype, 0), t.oid) = 'date'::regtype \
             ORDER BY c.relname, a.attname",
            &[],
        )
        .await
        .expect("enumerate the schema's date columns");
    assert!(
        !columns.is_empty(),
        "no date column was found in the zeroship schema, so the isolated \
         billing period band base cannot be resolved. The billing period is a \
         date domain and at least usage_aggregates and invoices carry one; an \
         empty result means this database is not migrated, or the enumeration \
         is broken. Either way, assuming a base of zero would hand this run the \
         same months as the last one."
    );

    let mut parts = Vec::new();
    for row in &columns {
        let table: String = row.get("table_name");
        let column: String = row.get("column_name");
        // The names come from the catalog, so they are already the real
        // identifiers; this refuses anything that would need quoting rather
        // than splicing it into SQL and hoping.
        for name in [&table, &column] {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "refusing to splice the catalog name {name:?} into SQL"
            );
        }
        parts.push(format!(
            "SELECT max({column})::date AS m FROM zeroship.{table} \
             WHERE {column} >= $1::date AND {column} < $2::date"
        ));
    }

    let band_start = chrono::NaiveDate::from_ymd_opt(ISOLATED_PERIOD_BASE_YEAR, 1, 1)
        .expect("the band's first month is a valid date");
    let band_end = chrono::NaiveDate::from_ymd_opt(ISOLATED_PERIOD_CEILING_YEAR, 1, 1)
        .expect("the band's ceiling is a valid date");
    let sql = format!("SELECT max(m) AS m FROM ({}) s", parts.join(" UNION ALL "));
    let rows = client
        .query(&sql, &[&band_start, &band_end])
        .await
        .expect("read the highest period already seeded in the band");
    let latest: Option<chrono::NaiveDate> = rows
        .first()
        .expect("a bare aggregate returns one row")
        .get("m");

    let Some(latest) = latest else {
        return 0;
    };
    months_since_band_start(latest) + 1
}

/// Where a period sits in the band, counted in months from its first month.
///
/// The ONE spelling of that arithmetic. `resolve_run_band_base` turns an
/// observed high-water mark into a base with it, and the regression test turns
/// a window it was handed into the same units to compare them; two copies could
/// disagree by a month and the comparison would still look like it held.
#[allow(dead_code)]
pub fn months_since_band_start(period: chrono::NaiveDate) -> u32 {
    use chrono::Datelike;
    let months = (period.year() - ISOLATED_PERIOD_BASE_YEAR) * 12 + (period.month() as i32 - 1);
    u32::try_from(months).expect("a period inside the band is at or after its first month")
}

/// First year of the band `next_isolated_period()` reserves. NOTHING ELSE MAY
/// HARDCODE A PERIOD AT OR ABOVE THIS YEAR.
///
/// The allocator only makes windows disjoint among ITS OWN callers. Two other
/// things write `usage_aggregates` rows into far-future months and neither goes
/// through it:
///
///   * hardcoded literals - `src/cron/spend_recompute.rs` pins 2035/2036/2042/
///     2043 and `tests/stream_forwarder_recompute_test.rs` does the same;
///   * the LIB test binary - those `#[cfg(test)]` modules run in a DIFFERENT
///     PROCESS from `tests/live_db.rs` against the SAME database, so the
///     allocator's process-global counter cannot see them at all.
///
/// Measured 2026-08-20 with the band at 2030: `spend_recompute`'s hardcoded
/// far-future period landed inside a window this allocator had handed to a
/// proration test, putting two modules' apps in one period. Nothing asserted on
/// that period, so it was latent - but it is the same defect, and "we got away
/// with it" is not isolation.
///
/// So the band sits ABOVE every literal in the crate rather than among them,
/// and `period_band_is_reserved_for_the_allocator` in `billing_safety_net_test`
/// fails if a new literal moves into it. That check is what makes "disjoint by
/// construction" a property rather than a hope; without it this constant is
/// just a comment.
///
/// IT IS ALSO THE FLOOR OF THE PER-RUN BASE SCAN. `resolve_run_band_base` only
/// looks at periods at or above this year, so a literal that moved into the
/// band would not merely share a month with one window - it would be read as a
/// previous run's high-water mark and push every later run's base past it.
pub const ISOLATED_PERIOD_BASE_YEAR: i32 = 2100;

/// One past the last year `next_isolated_period()` will hand out, and the top of
/// the window the per-run base scan reads.
///
/// Two jobs, and they are the same job seen from both ends. The scan ignores
/// periods at or above this year, so a far-future SENTINEL written by something
/// that is not this allocator cannot become the high-water mark that every run
/// then starts from - which would hand every run the same base and undo the
/// whole point. And the allocator refuses to hand out a window at or above it,
/// because a month the scan cannot see is a month the next run will hand out
/// again.
///
/// Raising it is therefore NOT the repair for an exhausted band. Recreate the
/// test database; the band is meant to be cheap to reclaim, not to last.
pub const ISOLATED_PERIOD_CEILING_YEAR: i32 = 100_000;

#[allow(dead_code)]
pub fn lite_billing_stack(
    registry: Registry,
    stripe_base_url: String,
    tax_provider: Arc<dyn zeroship_control::tax::TaxProvider>,
) -> Arc<zeroship_control::metering::provider::BillingStack> {
    use zeroship_control::metering::provider::{
        BillingStack, ControlLiteStore, LiteStore, PlatformSecretResolver, ProviderCtx,
    };
    use zeroship_control::{SecretString, StripeStore};

    let store: Arc<dyn LiteStore> = Arc::new(ControlLiteStore::new(
        registry.clone(),
        StripeStore::new(registry.clone()),
        SecretString::new("sk_test_mock".to_string()),
        stripe_base_url,
        tax_provider,
    ));
    let ctx = ProviderCtx::new(
        serde_json::json!({}),
        Arc::new(PlatformSecretResolver),
        Some(store),
    );
    let provider = zeroship_control::metering::provider::builtin_registry()
        .build("lite", &ctx)
        .expect("test lite billing provider builds");
    Arc::new(BillingStack {
        meter: Arc::clone(&provider),
        invoicer: provider,
    })
}

#[path = "../../src/test_database/mod.rs"]
mod test_database;
