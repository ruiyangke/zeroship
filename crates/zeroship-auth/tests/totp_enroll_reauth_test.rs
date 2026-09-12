//! `POST /me/2fa/enroll` re-auth gate - driven through the REAL ntex route.
//!
//! `/me/2fa/disable` and `/me/2fa/enroll` reach the same end state: an account
//! that no longer has a working second factor. `disable` demands a re-auth proof
//! (a current TOTP code or the account password); `enroll` overwrites the stored
//! secret and resets `confirmed_at` to NULL, which makes `is_enabled` false and
//! stops `/login` from challenging. A session cookie alone must not be enough to
//! reach that state, or a stolen cookie disarms 2FA and then re-arms it against
//! the attacker's own authenticator.
//!
//! These tests stand the handler up against live Postgres with a real session
//! and a real CSRF cookie, so they pin the ROUTE's behaviour, not the store
//! function's. The database is REQUIRED: a missing test database fails the run
//! rather than quietly passing an empty test body.
//!
//! Run with `--test-threads=1` (the auth suite shares rows).

use crate::common;

use std::sync::Arc;

use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_mailer::Mailer;

use common::test_auth_config;
use zeroship_auth::config::AuthConfig;
use zeroship_auth::identity::{password, totp};
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions::CreateSession;
use zeroship_auth::store::{sessions, totp as totp_store, users};

/// The account password every fixture user is seeded with; the password arm of
/// the re-auth proof submits it verbatim.
const FIXTURE_PASSWORD: &str = "correct-horse-battery-staple-2fa";

const CSRF_TOKEN: &str = "csrf-enroll-reauth";

/// A signed-in user plus everything the `/me/2fa/enroll` handler resolves from
/// the request: a live `idp_sessions` row and the matching cookie pair.
struct EnrollFixture {
    cfg: Arc<AuthConfig>,
    pg: Arc<compio_postgres::Client>,
    user_id: zeroship_core::UserId,
    session_id: Uuid,
}

impl EnrollFixture {
    #[allow(clippy::future_not_send)]
    async fn boot(label: &str) -> Self {
        let db_url = crate::common::test_database_url();
        let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[totp_enroll_reauth_test] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let tag = Uuid::new_v4().simple().to_string();
        let phc = password::hash(FIXTURE_PASSWORD).expect("hash fixture password");
        let user = users::create(
            &pg,
            &format!("totp-enroll-{label}-{tag}@zeroship.test"),
            "Enroll Reauth",
            Some(&phc),
        )
        .await
        .expect("create fixture user");

        let session = sessions::create(
            &pg,
            &CreateSession {
                user_id: user.id.clone(),
                auth_method: "password",
                amr: vec!["pwd".to_owned()],
                acr: None,
                expected_credential_version: None,
                idle_minutes: session_cookie::IDLE_MINUTES,
                absolute_hours: session_cookie::ABSOLUTE_HOURS,
            },
        )
        .await
        .expect("create fixture session");

        Self {
            cfg: Arc::new(test_auth_config(&db_url)),
            pg,
            user_id: user.id,
            session_id: session.id,
        }
    }

    /// The at-rest key the handler itself derives, so a secret this test seeds
    /// decrypts under the same key the route uses.
    fn key(&self) -> [u8; 32] {
        totp::key_from_config(self.cfg.settings.totp_enc_key.expose_str()).expect("totp enc key")
    }

    /// Seed a PENDING credential and hand back its raw secret.
    #[allow(clippy::future_not_send)]
    async fn seed_pending(&self) -> Vec<u8> {
        let secret = totp::generate_secret();
        let ct = totp::encrypt_secret(&self.key(), &self.user_id, &secret).expect("encrypt");
        // Seeding always starts from a user with no credential, so the store's
        // confirmed-clobber guard never applies here.
        assert!(
            totp_store::enroll(&self.pg, &self.user_id, &ct, false)
                .await
                .expect("seed enroll"),
            "seeding a pending credential must write"
        );
        secret
    }

    /// Seed a CONFIRMED (active, login-gating) credential and hand back its raw
    /// secret so a test can compute a valid current code from it.
    #[allow(clippy::future_not_send)]
    async fn seed_confirmed(&self) -> Vec<u8> {
        let secret = self.seed_pending().await;
        let (_, hashes) = totp::generate_backup_codes().expect("backup codes");
        totp_store::confirm(&self.pg, &self.user_id, &hashes)
            .await
            .expect("seed confirm");
        secret
    }

    /// POST `/me/2fa/enroll` through the real route with a valid session cookie
    /// and a valid CSRF double-submit, plus whatever re-auth proof is supplied.
    #[allow(clippy::future_not_send)]
    async fn post_enroll(&self, code: Option<&str>, pw: Option<&str>) -> (u16, serde_json::Value) {
        // An enroll that replaces a CONFIRMED credential mails the account
        // holder, so the route resolves `State<Arc<dyn Mailer>>` and a bare
        // `web::App` without one 500s on the path this file is about. What the
        // notice SAYS is pinned in `totp_removal_notice_test`; here the mailer
        // only has to exist.
        let mailer: Arc<dyn Mailer> = Arc::new(common::CapturingMailer::default());
        let svc = test::init_service(
            web::App::new()
                .state(self.cfg.clone())
                .state(self.pg.clone())
                .state(mailer)
                .service(
                    web::resource("/me/2fa/enroll")
                        .route(web::post().to(zeroship_auth::ui::totp::enroll)),
                ),
        )
        .await;

        let mut body = url::form_urlencoded::Serializer::new(String::new());
        body.append_pair("csrf", CSRF_TOKEN);
        if let Some(code) = code {
            body.append_pair("code", code);
        }
        if let Some(pw) = pw {
            body.append_pair("password", pw);
        }
        let body = body.finish();

        let req = test::TestRequest::post()
            .uri("/me/2fa/enroll")
            .header("content-type", "application/x-www-form-urlencoded")
            .header(
                "cookie",
                format!(
                    "__Host-zsidp_csrf={CSRF_TOKEN}; {}={}",
                    session_cookie::COOKIE_NAME,
                    self.session_id
                ),
            )
            .set_payload(body)
            .to_request();

        let resp = test::call_service(&svc, req).await;
        let status = resp.status().as_u16();
        let bytes = test::read_body(resp).await;
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// The raw stored secret, decrypted - lets a test prove the secret was (or
    /// was not) replaced by the request under test.
    #[allow(clippy::future_not_send)]
    async fn stored_secret(&self) -> Option<Vec<u8>> {
        let cred = totp_store::find(&self.pg, &self.user_id)
            .await
            .expect("find")?;
        Some(
            totp::decrypt_secret(&self.key(), &self.user_id, &cred.encrypted_secret)
                .expect("decrypt stored secret"),
        )
    }

    #[allow(clippy::future_not_send)]
    async fn is_enabled(&self) -> bool {
        totp_store::is_enabled(&self.pg, &self.user_id)
            .await
            .expect("is_enabled")
    }

    #[allow(clippy::future_not_send)]
    async fn confirmed_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        totp_store::find(&self.pg, &self.user_id)
            .await
            .expect("find")
            .and_then(|c| c.confirmed_at)
    }

    /// Count audit rows of `event_type` for this user, so a refusal can be shown
    /// to leave a trail rather than failing silently.
    #[allow(clippy::future_not_send)]
    async fn audit_count(&self, event_type: &str) -> i64 {
        self.pg
            .query_one(
                "SELECT COUNT(*) AS n FROM zeroship.audit_events \
                 WHERE actor_user_id = $1 AND event_type = $2",
                &[&self.user_id.as_str(), &event_type],
            )
            .await
            .expect("count audit rows")
            .get("n")
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
                &[&format!("totp:verify:{}", self.user_id.as_str())],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.audit_events WHERE actor_user_id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.totp_backup_codes WHERE user_id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.totp_credentials WHERE user_id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
    }
}

/// A current code for `secret`, the same proof a user reads off their
/// authenticator app.
fn current_code(secret: &[u8]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    totp::code_at(secret, now)
}

// ---------------------------------------------------------------------------
// The defect: a session cookie alone must not disarm a confirmed second factor.
// ---------------------------------------------------------------------------

/// A cookie-only enroll against a CONFIRMED credential must be refused and must
/// leave the credential armed. Anything else means a stolen session cookie can
/// turn 2FA off (`is_enabled` -> false stops `/login` challenging) and then
/// re-arm it against the attacker's own authenticator.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn enroll_without_reauth_does_not_disarm_a_confirmed_credential() {
    let fx = EnrollFixture::boot("nodisarm").await;
    let original = fx.seed_confirmed().await;
    let confirmed_before = fx
        .confirmed_at()
        .await
        .expect("seeded credential is confirmed");
    assert!(fx.is_enabled().await, "fixture must start with 2FA armed");

    let (status, body) = fx.post_enroll(None, None).await;

    assert_eq!(
        status, 401,
        "cookie-only enroll over a confirmed credential must be refused, got {status} {body}"
    );
    assert!(
        fx.is_enabled().await,
        "a refused enroll must leave the confirmed credential armed"
    );
    assert_eq!(
        fx.confirmed_at().await,
        Some(confirmed_before),
        "confirmed_at must be untouched by a refused enroll"
    );
    assert_eq!(
        fx.stored_secret().await.as_deref(),
        Some(original.as_slice()),
        "a refused enroll must not overwrite the stored secret"
    );
    assert_eq!(
        fx.audit_count("totp_enroll_refused").await,
        1,
        "a refused enroll must leave an audit trail, as a refused disable does"
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// The legitimate paths must keep working. A fix that blocks all re-enrollment
// is not a fix.
// ---------------------------------------------------------------------------

/// Re-enrollment WITH the account password succeeds and does reset the
/// credential to pending - the user rotating to a new phone.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn enroll_with_password_reauth_rotates_a_confirmed_credential() {
    let fx = EnrollFixture::boot("pwrotate").await;
    let original = fx.seed_confirmed().await;

    let (status, body) = fx.post_enroll(None, Some(FIXTURE_PASSWORD)).await;

    assert_eq!(
        status, 200,
        "password re-auth must be accepted, got {status} {body}"
    );
    assert!(
        body["otpauth_uri"]
            .as_str()
            .is_some_and(|u| u.starts_with("otpauth://totp/")),
        "a successful enroll returns provisioning material: {body}"
    );
    assert!(
        !fx.is_enabled().await,
        "an authorised re-enroll resets the credential to pending"
    );
    assert!(fx.confirmed_at().await.is_none(), "confirmed_at cleared");
    assert_ne!(
        fx.stored_secret().await.as_deref(),
        Some(original.as_slice()),
        "an authorised re-enroll installs a fresh secret"
    );

    fx.cleanup().await;
}

/// Re-enrollment WITH a current TOTP code succeeds too - the other proof
/// `verify_reauth` accepts, and the only one an account without a password has.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn enroll_with_totp_code_reauth_rotates_a_confirmed_credential() {
    let fx = EnrollFixture::boot("coderotate").await;
    let original = fx.seed_confirmed().await;
    let code = current_code(&original);

    let (status, body) = fx.post_enroll(Some(&code), None).await;

    assert_eq!(
        status, 200,
        "TOTP-code re-auth must be accepted, got {status} {body}"
    );
    assert!(
        !fx.is_enabled().await,
        "an authorised re-enroll resets the credential to pending"
    );
    assert_ne!(
        fx.stored_secret().await.as_deref(),
        Some(original.as_slice()),
        "an authorised re-enroll installs a fresh secret"
    );

    fx.cleanup().await;
}

/// A wrong password is not a proof: the credential stays armed.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn enroll_with_wrong_password_is_refused() {
    let fx = EnrollFixture::boot("wrongpw").await;
    let original = fx.seed_confirmed().await;

    let (status, body) = fx.post_enroll(None, Some("not-the-password")).await;

    assert_eq!(
        status, 401,
        "a wrong password must be refused, got {status} {body}"
    );
    assert!(fx.is_enabled().await, "credential stays armed");
    assert_eq!(
        fx.stored_secret().await.as_deref(),
        Some(original.as_slice()),
        "secret untouched"
    );

    fx.cleanup().await;
}

/// FIRST enrollment has nothing to protect. A user with no credential at all
/// must be able to enroll with a session cookie alone - requiring re-auth here
/// would make 2FA impossible to turn on.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn first_enrollment_needs_no_reauth() {
    let fx = EnrollFixture::boot("first").await;
    assert!(
        totp_store::find(&fx.pg, &fx.user_id)
            .await
            .expect("find")
            .is_none(),
        "fixture starts with no credential"
    );

    let (status, body) = fx.post_enroll(None, None).await;

    assert_eq!(
        status, 200,
        "first enrollment must be frictionless, got {status} {body}"
    );
    assert_eq!(body["confirmed"], serde_json::Value::Bool(false));
    let cred = totp_store::find(&fx.pg, &fx.user_id)
        .await
        .expect("find")
        .expect("first enroll writes a credential");
    assert!(cred.confirmed_at.is_none(), "a fresh enrollment is pending");

    fx.cleanup().await;
}

/// A PENDING credential is not yet protecting anything either (`is_enabled` is
/// false, so login is not gated). Replacing it - the user who scanned the QR
/// into the wrong app and wants a new one - must stay frictionless.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn re_enrolling_over_a_pending_credential_needs_no_reauth() {
    let fx = EnrollFixture::boot("pending").await;
    let first = fx.seed_pending().await;
    assert!(
        !fx.is_enabled().await,
        "a pending credential does not gate login"
    );

    let (status, body) = fx.post_enroll(None, None).await;

    assert_eq!(
        status, 200,
        "replacing a pending enrollment must be frictionless, got {status} {body}"
    );
    assert_ne!(
        fx.stored_secret().await.as_deref(),
        Some(first.as_slice()),
        "the pending secret is replaced"
    );
    assert!(
        !fx.is_enabled().await,
        "still pending after the replacement"
    );

    fx.cleanup().await;
}
