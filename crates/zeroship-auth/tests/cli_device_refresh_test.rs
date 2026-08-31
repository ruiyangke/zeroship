//! The first-party CLI's device grant, and the refresh family it hands back.
//!
//! `crates/auth/tests/oidc_refresh_token_test.rs` already covers the generic
//! rotation and reuse machinery for an ordinary app client. What it cannot
//! cover is the property that makes the CLI different: the token control
//! accepts is a PLATFORM PRINCIPAL token (`sub` = the `zeroship.users` UUID,
//! `aud` = control's resource audience), not an app-sector pairwise token. A
//! rotation that quietly minted the ordinary shape would return HTTP 200 and
//! then be refused by `zeroship_authn::BearerVerifier::verify_bearer`, which
//! is the failure this file exists to make loud.

use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ntex::web;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::oidc::device_token::DEVICE_CODE_GRANT_TYPE;
use zeroship_auth::oidc::refresh::RefreshSessionPool;
use zeroship_auth::oidc::{Issuer, ACCESS_TOKEN_TTL_SECS};
use zeroship_auth::server;
use zeroship_core::config::Operational;
use zeroship_core::device_grant::{OP_PROVIDER, PLATFORM_CLI_CLIENT_ID};

use common::test_auth_config;

const ISSUER: &str = "https://auth.zeroship.test/oauth2";
const CONTROL_AUDIENCE: &str = "control.zeroship.ai";
/// What `zeroship login` asks for: resource scopes plus the marker that says
/// "give me a refresh token".
const CLI_SCOPE: &str = "offline_access apps:deploy apps:read";
/// Spelled out rather than read from the constant the code reads, so this
/// assertion measures the registration instead of restating it.
const EXPECTED_REGISTERED_SCOPES: [&str; 6] = [
    "apps:archive",
    "apps:deploy",
    "apps:read",
    "apps:write",
    "secrets:read",
    "offline_access",
];
/// The scopes the CLI's token may carry AUTHORITY for. `offline_access` is
/// deliberately absent: it manages the grant, it does not widen it.
const EXPECTED_ISSUABLE_SCOPES: [&str; 5] = [
    "apps:archive",
    "apps:deploy",
    "apps:read",
    "apps:write",
    "secrets:read",
];

// ─── Lifetime ceilings, as LITERALS ──────────────────────────────────────────
//
// Every bound below is a literal, never the constant it bounds. Comparing an
// observed lifetime to the constant that produced it proves the plumbing and
// passes for any value: `ACCESS_TOKEN_TTL_SECS` was set to `12 * 60 * 60` and
// the whole auth suite stayed green at 645 passed, byte-identical.
//
// `issuer.rs`'s own `ttl_tests` bounds the CONSTANT. These bound what the
// server actually put on the wire and in the database, so a mint that ignores
// the constant fails here even when the constant is right. The two fail for
// different reasons and neither subsumes the other.

/// See `crates/auth/src/oidc/issuer.rs`, `MAX_ACCESS_TOKEN_LIFETIME_SECS`, for
/// the argument. Restated as a literal rather than imported so the two are
/// independent witnesses.
const MAX_ACCESS_TOKEN_LIFETIME_SECS: i64 = 30 * 60;
const MIN_ACCESS_TOKEN_LIFETIME_SECS: i64 = 120;

/// The longest a refresh FAMILY may live before re-authorization, in days.
///
/// This is the credential the change moved the long life onto, so it is the
/// one that most needs a bound - and it had none in any form before now.
/// Ninety days is the coarsest re-authorization cadence that is still a
/// cadence; past a quarter an un-rotated family is a permanent credential in
/// all but name, and the whole revocation story would rest on reuse detection
/// firing.
const MAX_REFRESH_FAMILY_LIFETIME_DAYS: f64 = 90.0;

/// And it must be worth having: a family that expires inside a day gives the
/// human nothing the access token did not already give them.
const MIN_REFRESH_FAMILY_LIFETIME_DAYS: f64 = 1.0;

/// The longest a SPENT refresh token may still be replayed, in seconds.
///
/// This is the lost-response retry window, so it needs to span one HTTP
/// timeout and a retry, not a session. Past a few minutes a rotated-away token
/// keeps working, which is the exact thing reuse detection exists to stop.
const MAX_REPLAY_WINDOW_SECS: f64 = 300.0;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    db: Arc<Client>,
    issuer: Arc<Issuer>,
    user_id: Uuid,
    key_dir: PathBuf,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        let Some(db_url) = db_url() else {
            zeroship_test_support::skip(
                "[cli_device_refresh] skip (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))",
            );
            return None;
        };
        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(err) = pg_connection.run().await {
                eprintln!("[cli_device_refresh] pg connection error: {err}");
            }
        })
        .detach();
        let db = Arc::new(pg_client);

        let issuer = Arc::new(test_issuer());
        common::publish_op_key_once(&issuer, &db)
            .await
            .expect("publish active OP key");

        // The reserved first-party registration is what the whole flow hangs
        // off, and it is reconciled at auth boot in production
        // (`crates/auth/src/main.rs`).
        zeroship_auth::oidc::device_token::reconcile_platform_cli_client(db.as_ref())
            .await
            .expect("reconcile platform CLI client");

        let user_id = Uuid::new_v4();
        let email = format!("cli-device-{}@zeroship.test", Uuid::new_v4().simple());
        db.execute(
            "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
             VALUES ($1, $2::citext, NOW(), 'CLI Device User')",
            &[&user_id, &email],
        )
        .await
        .expect("seed CLI device user");

        let key_dir = make_key_dir();
        let hash_key_file = key_dir.join("refresh-hmac.keys");
        let idem_key_file = key_dir.join("refresh-idem.key");
        write_secret_file(
            &hash_key_file,
            b"1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        );
        write_secret_file(&idem_key_file, b"refresh-idem-key-material-32-bytes");

        let mut cfg = test_auth_config(&db_url);
        cfg.settings.refresh_hash_key_file = Operational::new(hash_key_file);
        cfg.settings.refresh_idem_key_file = Operational::new(idem_key_file);
        let cfg = Arc::new(cfg);

        let cfg_state = cfg.clone();
        let db_state = db.clone();
        let issuer_state = issuer.clone();
        let refresh_pool = RefreshSessionPool::new(db_url.clone(), 4);
        let refresh_pool_state = refresh_pool.clone();
        let srv = web::test::server(move || {
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            let issuer_state = issuer_state.clone();
            let refresh_pool_state = refresh_pool_state.clone();
            async move {
                web::App::new()
                    .state(cfg_state)
                    .state(db_state)
                    .state(issuer_state)
                    .state(refresh_pool_state)
                    .middleware(SecurityHeaders::default())
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let auth_base = format!("http://{}", srv.addr());
        Some(Self {
            srv,
            auth_base,
            db,
            issuer,
            user_id,
            key_dir,
        })
    }

    /// Drive the RFC 8628 legs a human drives, and return the token response.
    ///
    /// Approval is written straight to the row because the browser leg has its
    /// own coverage in `device_grant_test.rs`; what this file is measuring
    /// starts at redemption.
    #[allow(clippy::future_not_send)]
    async fn login(&self) -> (TokenResponse, String) {
        let authorization = self.device_authorization(CLI_SCOPE).await;
        let device_code = authorization["device_code"]
            .as_str()
            .expect("device authorization returns device_code")
            .to_string();
        let user_code = authorization["user_code"]
            .as_str()
            .expect("device authorization returns user_code")
            .to_string();
        self.approve(&user_code).await;
        let (status, body) = self.device_token(&device_code).await;
        assert_eq!(status, 200, "device code redemption failed: {body}");
        let token: TokenResponse =
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("decode token: {e}: {body}"));
        (token, device_code)
    }

    #[allow(clippy::future_not_send)]
    async fn device_authorization(&self, scope: &str) -> Value {
        let (status, body) = self.device_authorization_raw(scope).await;
        assert_eq!(status, 200, "device authorization failed: {body}");
        serde_json::from_str(&body).expect("decode device authorization response")
    }

    #[allow(clippy::future_not_send)]
    async fn device_authorization_raw(&self, scope: &str) -> (u16, String) {
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", PLATFORM_CLI_CLIENT_ID)
            .append_pair("scope", scope)
            .finish();
        post_form(
            &format!("{}/oauth2/device/authorization", self.auth_base),
            form,
        )
        .await
    }

    #[allow(clippy::future_not_send)]
    async fn approve(&self, user_code: &str) {
        let approved = self
            .db
            .execute(
                "UPDATE zeroship.device_grants \
                 SET principal_id = $1, sid = $2, \
                     auth_credential_version = \
                        (SELECT credential_version FROM zeroship.users WHERE id = $1), \
                     status = 'approved' \
                 WHERE user_code = $3 AND provider = $4",
                &[
                    &self.user_id,
                    &Uuid::new_v4().to_string(),
                    &user_code,
                    &OP_PROVIDER,
                ],
            )
            .await
            .expect("approve CLI device grant");
        assert_eq!(approved, 1, "approve exactly one device grant");
    }

    #[allow(clippy::future_not_send)]
    async fn device_token(&self, device_code: &str) -> (u16, String) {
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", DEVICE_CODE_GRANT_TYPE)
            .append_pair("device_code", device_code)
            .append_pair("client_id", PLATFORM_CLI_CLIENT_ID)
            .finish();
        post_form(&format!("{}/oauth2/token", self.auth_base), form).await
    }

    #[allow(clippy::future_not_send)]
    async fn refresh(&self, refresh_token: &str) -> (u16, String) {
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", PLATFORM_CLI_CLIENT_ID)
            .finish();
        post_form(&format!("{}/oauth2/token", self.auth_base), form).await
    }

    /// How long the live refresh family the CLI is holding has left, in days,
    /// measured from the row the server wrote rather than from a constant.
    ///
    /// `(absolute, idle)`: the absolute cap the family cannot outlive however
    /// often it rotates, and this token's own idle expiry.
    #[allow(clippy::future_not_send)]
    async fn live_family_lifetime_days(&self) -> (f64, f64) {
        let row = self
            .db
            .query_one(
                "SELECT (EXTRACT(EPOCH FROM (family_absolute_expires_at - NOW())) / 86400.0)::float8 \
                          AS absolute_days, \
                        (EXTRACT(EPOCH FROM (expires_at - NOW())) / 86400.0)::float8 AS idle_days \
                 FROM zeroship.oauth_refresh_tokens \
                 WHERE user_id = $1 AND rotated_at IS NULL AND revoked_at IS NULL \
                 ORDER BY issued_at DESC LIMIT 1",
                &[&self.user_id],
            )
            .await
            .expect("load live refresh family row");
        (
            row.get::<_, f64>("absolute_days"),
            row.get::<_, f64>("idle_days"),
        )
    }

    /// How long the SPENT predecessor may still be replayed, in seconds, from
    /// the row rotation wrote.
    #[allow(clippy::future_not_send)]
    async fn replay_window_secs(&self) -> f64 {
        let row = self
            .db
            .query_one(
                "SELECT EXTRACT(EPOCH FROM (idem_expires_at - NOW()))::float8 AS window_secs \
                 FROM zeroship.oauth_refresh_tokens \
                 WHERE user_id = $1 AND rotated_at IS NOT NULL \
                   AND idem_expires_at IS NOT NULL \
                 ORDER BY rotated_at DESC LIMIT 1",
                &[&self.user_id],
            )
            .await
            .expect("load rotated predecessor row");
        row.get::<_, f64>("window_secs")
    }

    /// Rows in `zeroship.token_revocations` for the (client, subject) pair
    /// control's bearer read path consults.
    #[allow(clippy::future_not_send)]
    async fn revocation_marker(&self, sub: &str) -> i64 {
        let rows = self
            .db
            .query(
                "SELECT COUNT(*)::BIGINT AS n FROM zeroship.token_revocations \
                 WHERE client_id = $1 AND sub = $2",
                &[&PLATFORM_CLI_CLIENT_ID, &sub],
            )
            .await
            .expect("count token revocations");
        rows[0].get::<_, i64>("n")
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        let _ = self
            .db
            .execute(
                "DELETE FROM zeroship.oauth_refresh_tokens WHERE user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .db
            .execute(
                "DELETE FROM zeroship.token_revocations WHERE sub = $1",
                &[&self.user_id.to_string()],
            )
            .await;
        let _ = self
            .db
            .execute(
                "DELETE FROM zeroship.device_grants WHERE principal_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .db
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
        let _ = std::fs::remove_dir_all(&self.key_dir);
        drop(self.srv);
    }
}

/// Every claim the CLI's access token must carry for control to accept it.
fn assert_platform_principal_token(fx: &Fixture, access_token: &str, scope: &str) {
    let claims = fx
        .issuer
        .verify_access_token(access_token)
        .expect("verify CLI access token");
    assert_eq!(
        claims.sub,
        fx.user_id.to_string(),
        "control keys authorization off the platform principal UUID, not a pairwise subject"
    );
    assert_eq!(claims.aud, CONTROL_AUDIENCE);
    assert_eq!(claims.client_id, PLATFORM_CLI_CLIENT_ID);
    assert_eq!(claims.scope, scope);
    // WIRING: the token's own exp matches the constant the response advertises.
    // True for any value of that constant - it is not the property below.
    assert_eq!(
        claims.exp - claims.iat,
        ACCESS_TOKEN_TTL_SECS,
        "a refreshable CLI credential takes the OP's short access-token lifetime"
    );
    // VALUE: the lifetime the server actually stamped, against literals. This
    // is what fails when the constant is moved back to twelve hours.
    let lifetime = claims.exp - claims.iat;
    assert!(
        lifetime <= MAX_ACCESS_TOKEN_LIFETIME_SECS,
        "the OP stamped a {lifetime}s access token; ceiling is \
         {MAX_ACCESS_TOKEN_LIFETIME_SECS}s. Nothing recalls a bearer this long-lived \
         except the token_revocations marker - the long life belongs in the refresh \
         family, which rotation and reuse detection can kill."
    );
    assert!(
        lifetime >= MIN_ACCESS_TOKEN_LIFETIME_SECS,
        "the OP stamped a {lifetime}s access token; floor is \
         {MIN_ACCESS_TOKEN_LIFETIME_SECS}s (the CLI's 60s expiry skew would make it \
         stale on arrival)."
    );
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_cli_registration_permits_refresh_and_registers_offline_access() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let row = fx
        .db
        .query_one(
            "SELECT scopes, refresh_allowed FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&PLATFORM_CLI_CLIENT_ID],
        )
        .await
        .expect("load reconciled CLI registration");
    let expected: Vec<String> = EXPECTED_REGISTERED_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect();
    assert_eq!(
        row.get::<_, Vec<String>>("scopes"),
        expected,
        "the CLI cannot ask for offline_access unless the registration lists it"
    );
    assert!(
        row.get::<_, bool>("refresh_allowed"),
        "refresh.rs refuses the grant outright when the client is not refresh_allowed"
    );
    let issuable: Vec<String> = zeroship_core::device_grant::PLATFORM_CLI_ISSUABLE_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect();
    assert_eq!(
        issuable, EXPECTED_ISSUABLE_SCOPES,
        "offline_access manages the grant; it must not widen the resource-authority ceiling"
    );
    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_cli_device_grant_returns_a_short_access_token_and_a_refresh_token() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let (token, device_code) = fx.login().await;

    assert_eq!(token.token_type, "Bearer");
    assert_eq!(token.scope, "apps:deploy apps:read offline_access");
    assert_eq!(
        token.expires_in, ACCESS_TOKEN_TTL_SECS as u64,
        "a 12-hour bearer cannot be recalled; a short one plus a refresh family can"
    );
    assert!(
        token.id_token.is_none(),
        "the device grant mints no nonce-less id_token"
    );
    let refresh_token = token
        .refresh_token
        .as_deref()
        .expect("the CLI asked for offline_access and must get a refresh token");
    assert!(refresh_token.starts_with("zrt_"), "{refresh_token}");
    assert_platform_principal_token(
        &fx,
        &token.access_token,
        "apps:deploy apps:read offline_access",
    );

    // The device code is single-use: the row is deleted on redemption, so a
    // second exchange cannot mint a second credential from one approval.
    let (status, body) = fx.device_token(&device_code).await;
    assert_eq!(status, 400, "second redemption must fail: {body}");
    let err: Value = serde_json::from_str(&body).expect("decode error body");
    assert_eq!(err["error"], "expired_token");

    fx.cleanup().await;
}

/// The refresh family is where this change PUT the long life, so its lifetime
/// is the one that most needs a bound - and it had none anywhere in the tree,
/// in any form, before this test. Neither `FAMILY_ABSOLUTE_DAYS` nor
/// `FAMILY_IDLE_DAYS` nor `IDEM_WINDOW_SECS` was read by a single assertion.
///
/// Measured off the rows the server wrote, against literals.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_refresh_family_and_its_replay_window_are_bounded() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let (token, _device_code) = fx.login().await;
    let first_refresh = token.refresh_token.clone().expect("root refresh token");

    let (absolute_days, idle_days) = fx.live_family_lifetime_days().await;
    assert!(
        absolute_days <= MAX_REFRESH_FAMILY_LIFETIME_DAYS,
        "the refresh family runs {absolute_days:.2} days without re-authorization; \
         ceiling is {MAX_REFRESH_FAMILY_LIFETIME_DAYS} days. Past a quarter this is a \
         permanent credential and revocation rests entirely on reuse detection."
    );
    assert!(
        absolute_days >= MIN_REFRESH_FAMILY_LIFETIME_DAYS,
        "the refresh family runs only {absolute_days:.2} days; floor is \
         {MIN_REFRESH_FAMILY_LIFETIME_DAYS} day. Shorter than that and it buys nothing \
         over the access token it backs."
    );
    assert!(
        idle_days <= absolute_days,
        "idle expiry {idle_days:.2}d outlives the absolute cap {absolute_days:.2}d, \
         so the cap does not cap"
    );

    // Rotate once so a spent predecessor with a replay window exists.
    let (status, body) = fx.refresh(&first_refresh).await;
    assert_eq!(status, 200, "rotation failed: {body}");
    let replay_window = fx.replay_window_secs().await;
    assert!(
        replay_window <= MAX_REPLAY_WINDOW_SECS,
        "a spent refresh token stays replayable for {replay_window:.1}s; ceiling is \
         {MAX_REPLAY_WINDOW_SECS}s. This is a lost-response retry, not a session: \
         every second of it is a second a rotated-away token still works."
    );
    assert!(
        replay_window > 0.0,
        "no replay window at all ({replay_window:.1}s) turns a legitimate retry after a \
         dropped response into a family revocation"
    );

    fx.cleanup().await;
}

/// The OP issues a COARSE ceiling: the client registration, and nothing else.
///
/// This assertion is unchanged from when it was filed as a KNOWN GAP, and the
/// reframing is the point - the behaviour it measures is now deliberate rather
/// than missing, so read it as the issuer half of a two-part contract:
///
/// * here, the OP caps the scope to the registration without consulting
///   `zeroship.principal_grants`. It cannot do otherwise:
///   `db/migrations-ts/20260702000900_grants.ts:55` gives `zeroship_auth`
///   SELECT only on that table, and nothing in `crates/auth` may write it.
/// * control then intersects that scope with the principal's LIVE grants on
///   every bearer request (`crates/authn/src/lib.rs`,
///   `platform_cli_entitlement`), which is where an operator's narrowing takes
///   effect. Pinned by
///   `an_operator_deleting_a_grant_row_narrows_the_next_cli_request` in
///   `crates/control/tests/authz_guard_oauth_test.rs`.
///
/// So a wide scope HERE is not authority. Asserting it stays wide is what
/// stops someone "fixing" the gap in this function, which would mint
/// `scope: ""` on every first login: the fixture user holds no grant rows, and
/// auth has no way to provision them.
///
/// What this does NOT show: that narrowing works. Only the control-side test
/// named above shows that, and this one passes identically whether or not it
/// does.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn the_cli_device_grant_caps_scope_to_the_client_registration_only() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let rows = fx
        .db
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.principal_grants \
             WHERE principal_id = $1",
            &[&fx.user_id],
        )
        .await
        .expect("count principal grants");
    assert_eq!(
        rows[0].get::<_, i64>("n"),
        0,
        "the measurement needs a principal with no stored grants"
    );

    let (token, _device_code) = fx.login().await;
    assert_eq!(
        token.scope, "apps:deploy apps:read offline_access",
        "the OP narrowed the registration ceiling; entitlement is control's job, not the issuer's"
    );

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn a_cli_refresh_rotation_keeps_the_platform_principal_token_shape() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let (token, _device_code) = fx.login().await;
    let first_refresh = token.refresh_token.clone().expect("root refresh token");

    let (status, body) = fx.refresh(&first_refresh).await;
    assert_eq!(status, 200, "CLI refresh rotation failed: {body}");
    let rotated: TokenResponse =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("decode rotation: {e}: {body}"));
    let second_refresh = rotated
        .refresh_token
        .clone()
        .expect("rotation must return the successor refresh token");
    assert_ne!(
        second_refresh, first_refresh,
        "rotation means the presented token is replaced, not reissued"
    );
    assert_eq!(rotated.expires_in, ACCESS_TOKEN_TTL_SECS as u64);
    // The whole point: the rotated token is the same shape control accepted at
    // login. A pairwise subject or an app audience here would be a 200 that
    // control refuses.
    assert_platform_principal_token(
        &fx,
        &rotated.access_token,
        "apps:deploy apps:read offline_access",
    );

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reusing_a_rotated_cli_refresh_token_kills_the_family_and_recalls_the_access_token() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let (token, _device_code) = fx.login().await;
    let first_refresh = token.refresh_token.clone().expect("root refresh token");
    let sub = fx.user_id.to_string();
    assert_eq!(
        fx.revocation_marker(&sub).await,
        0,
        "no marker before anything goes wrong"
    );

    let (status, body) = fx.refresh(&first_refresh).await;
    assert_eq!(status, 200, "first rotation failed: {body}");
    let rotated: TokenResponse = serde_json::from_str(&body).expect("decode rotation");
    let second_refresh = rotated.refresh_token.clone().expect("successor");

    // A rotated-away token gets exactly ONE lost-response retry, and it returns
    // the SAME successor rather than minting a second one. That is the retry
    // arm, not the reuse arm, and it must not kill the family.
    let (status, body) = fx.refresh(&first_refresh).await;
    assert_eq!(status, 200, "the single lost-response retry must be served: {body}");
    let replay: TokenResponse = serde_json::from_str(&body).expect("decode replay");
    assert_eq!(
        replay.refresh_token.as_deref(),
        Some(second_refresh.as_str()),
        "the retry replays the cached successor; it does not rotate again"
    );
    assert_eq!(
        fx.revocation_marker(&sub).await,
        0,
        "a served retry is not reuse"
    );

    // The third presentation has no honest explanation left.
    let (status, body) = fx.refresh(&first_refresh).await;
    assert_eq!(status, 400, "reuse must be refused: {body}");
    let err: Value = serde_json::from_str(&body).expect("decode reuse error");
    assert_eq!(err["error"], "invalid_grant");

    // Reuse revokes the FAMILY, so the token the honest client is holding dies
    // with it - that is what distinguishes reuse detection from "this one
    // string is invalid".
    let (status, body) = fx.refresh(&second_refresh).await;
    assert_eq!(
        status, 400,
        "the successor must die with its family: {body}"
    );

    // And the marker control's bearer read path consults is written, so the
    // outstanding ACCESS token is recalled too rather than living out its TTL.
    assert_eq!(
        fx.revocation_marker(&sub).await,
        1,
        "reuse must write the (zeroship-cli, principal) revocation marker"
    );

    fx.cleanup().await;
}

#[allow(clippy::future_not_send)]
async fn post_form(url: &str, body: String) -> (u16, String) {
    let resp = cyper::Client::new()
        .request(http::Method::POST, url.to_string())
        .expect("build form POST")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(body)
        .send()
        .await
        .expect("send form POST");
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("read response body");
    (status, body)
}

fn db_url() -> Option<String> {
    zeroship_core::config::test_database_url_opt()
}

fn test_issuer() -> Issuer {
    let signing = common::op_signing_key();
    Issuer::from_signing_key(&signing, [11_u8; 32], ISSUER.to_string()).expect("issuer")
}

fn make_key_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!("zs-cli-refresh-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("create key dir");
    path
}

fn write_secret_file(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write secret file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("secret file permissions");
    }
}
