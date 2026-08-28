#![allow(dead_code)]

pub mod authz_fixture;
pub mod stripe_mock;

use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::sync::OnceLock;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::web::{self, HttpResponse};
use serde_json::json;
use uuid::Uuid;
use zeroship_core::auth_provider::{AuthProvider, PlatformConfig, PlatformProvider};
use zeroship_control::Registry;

pub const PLATFORM_ISSUER: &str = "https://auth.zeroship.test/oauth2";
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

/// The database every live-DB test in this crate dials, or a REFUSAL.
///
/// THE ONE PLACE THIS CRATE RESOLVES A DSN. Until 2026-08-21 there were
/// eighteen: this function, fifteen private `db_url()` copies beside it, and
/// two more in the lib's own `#[cfg(test)]` modules (`src/http_util.rs`,
/// `src/cron/spend_recompute.rs`), each carrying the same
///
///   .unwrap_or_else(|| "postgresql://postgres:zeroship@localhost:5440/\
///                       zeroship_billing_test".to_string())
///
/// A silent fallback is bad in the ordinary way -- a run with no configuration
/// reports passes for work it did not do -- and this one was worse than that.
/// It named the BILLING database from files like `authz_guard_oauth_test.rs`
/// and `deploy_test.rs`, which have nothing to do with billing, so the failure
/// mode was not "no database" but "the wrong database, silently".
///
/// WHAT THE PREFLIGHT ADDS ON TOP OF DELETING THE FALLBACK. Having a DSN is not
/// the same as having a database. On 2026-08-21 the overlay named the shared
/// `zeroship` database on :5440 and that database's `zeroship` schema had been
/// dropped out from under it, so every test here connected, ran its fixture and
/// failed inside an assertion with `42P01 relation "zeroship.plans" does not
/// exist`. Forty-one modules share this target, so that presented as a wall of
/// named tests FAILING with a database error -- which is what a real regression
/// looks like. It cost two people an evening.
///
/// [`zeroship_testkit::live_db::require`] ends the process with one block
/// instead. See its header for why exiting beats panicking here.
///
/// MEMOISED, so the probe costs one connection per test binary rather than one
/// per test.
pub fn require_control_db() -> String {
    static CHECKED: OnceLock<String> = OnceLock::new();
    CHECKED
        .get_or_init(|| {
            // `zeroship` is the platform schema; `zeroship_migrations` is the
            // journal beside it. Asking for BOTH is what separates "never
            // migrated" from "migrated and then partly dismantled" - the second
            // is what actually happened, and a check for the journal alone
            // would have called that database ready.
            zeroship_testkit::live_db::require_configured(
                zeroship_core::config::test_database_url_opt(),
                &["zeroship", "zeroship_migrations"],
            )
        })
        .clone()
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
pub fn platform_token(subject: Uuid, scope: &str) -> String {
    platform_token_for_client(subject, scope, "zeroship-cli")
}

/// The console BFF's client id. Distinct from `zeroship-cli` on purpose: it is
/// NOT the client whose scopes control narrows against the CLI grant set, so a
/// console bearer carries exactly the scopes it was minted with. Fixtures for
/// operator and billing surfaces use this, because those scopes are outside the
/// CLI's issuable set.
pub const CONSOLE_CLIENT_ID: &str = "zeroship-console";

pub fn platform_token_for_client(subject: Uuid, scope: &str, client_id: &str) -> String {
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
        "client_id": client_id,
        "scope": scope,
    });
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some("at+jwt".to_string());
    header.kid = Some(PLATFORM_KID.to_string());
    encode(&header, &claims, &platform_encoding_key()).expect("platform token")
}

pub fn platform_bearer(subject: Uuid, scope: &str) -> String {
    format!("Bearer {}", platform_token(subject, scope))
}

pub fn console_bearer(subject: Uuid, scope: &str) -> String {
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
pub async fn provision_app_workflow_schema(pg: &compio_postgres::Client, app_id: &Uuid) {
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

#[allow(dead_code)]
pub async fn seed_usage_total(
    pg: &compio_postgres::Client,
    app: Uuid,
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
        &[&app, &period_date(period_start_unix), &metric, &total],
    )
    .await
    .expect("seed usage total");
}

#[allow(dead_code)]
pub async fn seed_usage_delta(
    pg: &compio_postgres::Client,
    app: Uuid,
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
        &[&app, &period_date(period_start_unix), &metric, &delta],
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
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
        .expect("valid first-of-month period")
}

/// Hand this caller a far-future billing window that NO other caller can touch.
///
/// THIS LOOKS LIKE AN ORDINARY HELPER AND IS NOT. Read this before changing it,
/// and before adding a caller.
///
/// WHAT IT GUARANTEES
/// ------------------
/// Every call returns an instant in a month `STRIDE_MONTHS` after the previous
/// call's, so the windows callers actually touch are DISJOINT BY CONSTRUCTION.
/// Not "unlikely to collide" - disjoint. Two tests can never land in one period,
/// so a period-wide aggregate can never see a neighbour's rows.
///
/// WHAT IT REPLACED, AND WHY BOTH WERE WRONG
/// -----------------------------------------
///   `isolated_closed_period_now()`  memoised ONE random month in a `OnceLock`
///                                   and handed it to every caller, so all 78
///                                   seeding tests in the five billing modules
///                                   piled into a single period.
///   `unique_closed_period_now()`    (billing_safety_net_test) drew a FRESH
///                                   random month per call out of 2400. Its name
///                                   claimed uniqueness it did not have: 2400
///                                   months is a collision space, not an
///                                   allocator.
///
/// The bug they combined to produce: `billing_safety_net_test` asserts
/// `subjects_checked == 1`, but `reconcile_pass` counts EVERY subject in the
/// period, summed over every meter in it. A test owns its app; it did NOT own
/// its period. When the random draw hit a seeded month the count came back 121
/// instead of 1.
///
/// MEASURED 2026-08-20 (counting periods where a draw would break that
/// assertion) - these two numbers are measured, the rates below are modelled:
///
///   before the binary merge   6 loaded periods, worst 96,  128 subjects
///   after  the binary merge   2 loaded periods, worst 126, 128 subjects
///   with this allocator       0 loaded periods, by construction
///
/// The 128 contaminating subjects were IDENTICAL across the first two: merging
/// the test binaries did not create the hazard, it concentrated it. On a model
/// of four draws against 2400 months that is ~1.0 percent per run before and
/// ~0.33 percent after - real, and rarer, but never zero. This is zero.
///
/// WHY THIS ONLY BECAME CORRECT WHEN THE TEST BINARIES MERGED
/// ---------------------------------------------------------
/// The counter is process-global. Until 2026-08-20 these files were 48 separate
/// executables, and a process-global counter would have restarted at zero in
/// every one of them - handing the SAME months to different binaries while
/// promising they were unique. It is only sound because `tests/live_db.rs` and
/// `tests/main.rs` put every caller in ONE process. If these files are ever
/// split back into separate targets, THIS FUNCTION SILENTLY BREAKS: it keeps
/// returning values and they stop being unique. Split the targets and you must
/// key the window by something the whole run agrees on instead.
///
/// DO NOT REPLACE THIS WITH A LOCK
/// -------------------------------
/// A mutex around the sweeps cannot fix what this fixes. The rows OUTLIVE the
/// lock, and the collision is a later test DRAWING an earlier test's month -
/// which serialisation does not prevent, because the two tests were never
/// concurrent in the first place. The gate has always run `--test-threads 1`.
/// The defect is address allocation, not concurrency.
///
/// THE CONTRACT FOR CALLERS
/// ------------------------
/// ONE CALL PER TEST. Bind it to a local and reuse that local; a second call
/// gives you a DIFFERENT window, which is the point. Two call sites in
/// `billing_credit_test` used to rely on the memoised value being the same
/// instant twice and were hoisted to a local when this landed.
///
/// You may derive earlier months from the returned instant: the window reserved
/// for you is the returned month and the `STRIDE_MONTHS - 1` months before it.
/// Today callers reach at most two months back (`prev_period(now)` and, in one
/// test, `prev_period(now - 40 days)`), and the self-invoicing safety net reads
/// the returned month itself, so a stride of 4 leaves one month of slack. Reach
/// further back than that and you are in your neighbour's window.
#[allow(dead_code)]
pub fn next_isolated_period() -> i64 {
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Months per caller. See "THE CONTRACT FOR CALLERS" above.
    const STRIDE_MONTHS: u32 = 4;
    /// First month handed out, as an offset from January of the base year.
    /// Starts past zero so the earliest window's back-reach stays inside it.
    const FIRST_OFFSET_MONTHS: u32 = 4;
    /// Windows available before the arithmetic leaves the reserved band. The
    /// tree uses about 64; this asserts rather than wrapping into a month
    /// another caller already owns.
    const MAX_WINDOWS: u32 = 600;

    static NEXT_WINDOW: AtomicU32 = AtomicU32::new(0);
    let window = NEXT_WINDOW.fetch_add(1, Ordering::Relaxed);
    assert!(
        window < MAX_WINDOWS,
        "next_isolated_period() exhausted its {MAX_WINDOWS} private windows. \
         Raise MAX_WINDOWS (the band has room; see ISOLATED_PERIOD_BASE_YEAR); \
         do NOT wrap, because wrapping silently reissues a month another test \
         already seeded, which is the exact bug this replaced."
    );

    let offset = FIRST_OFFSET_MONTHS + window * STRIDE_MONTHS;
    let year = ISOLATED_PERIOD_BASE_YEAR + (offset / 12) as i32;
    let month = offset % 12 + 1;
    chrono::Utc
        .with_ymd_and_hms(year, month, 15, 12, 0, 0)
        .single()
        .expect("valid isolated billing period")
        .timestamp()
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
/// 2036-08 landed inside a window this allocator had handed to a proration
/// test, putting two modules' apps in one period. Nothing asserted on that
/// period, so it was latent - but it is the same defect, and "we got away with
/// it" is not isolation.
///
/// So the band sits ABOVE every literal in the crate rather than among them,
/// and `period_band_is_reserved_for_the_allocator` in `billing_safety_net_test`
/// fails if a new literal moves into it. That check is what makes "disjoint by
/// construction" a property rather than a hope; without it this constant is
/// just a comment.
pub const ISOLATED_PERIOD_BASE_YEAR: i32 = 2100;

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
