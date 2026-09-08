//! Turning 2FA off must TELL THE ACCOUNT HOLDER, and must revoke nothing.
//!
//! `/me/2fa/disable` was the only `/me/*` mutation next to `/reset`,
//! `/logout` and `/me/sessions/{id}/revoke` that produced no signal at all
//! beyond an audit row an operator reads. The question these tests pin is which
//! signal it owes.
//!
//! It does NOT owe a session teardown, and that is asserted here rather than
//! merely argued in a comment. Both routes already demand a re-auth proof - a
//! current TOTP code or the account password (`verify_reauth`) - and CSRF is not
//! a second barrier, because `csrf::matches` is a bare double-submit equality
//! that anyone holding the session cookie satisfies by setting both halves. So
//! whoever reaches this state holds material that, with 2FA now off, signs them
//! straight back in through `/login`. Revoking every session would evict the
//! account holder and cost that actor one redirect.
//!
//! What it owes is DETECTION: a notice to the registered address whose call to
//! action is a password reset, which is the one flow that does revoke
//! everything (family markers, `app_session_anchors`, `credential_version`, the
//! session rows).
//!
//! `enroll` is covered too, and not for symmetry. It reaches the SAME state -
//! replacing a confirmed credential resets `confirmed_at` to NULL, so
//! `is_enabled` goes false and `/login` stops challenging - so a notice that
//! fired only on `disable` would be routed around by an attacker who enrolls
//! instead. A FIRST enrollment turns nothing off and must stay silent, or the
//! notice becomes noise users learn to discard.
//!
//! WHAT THESE TESTS DO NOT CATCH. They assert the handler hands a message to
//! the `Mailer` it was given, not that any real transport delivered it: a
//! suppressed address, a bouncing domain, or a mail provider outage all look
//! identical here (`mailer.send` is best-effort by design and its failure is
//! logged, not surfaced). They also say nothing about the gateway app-session
//! tier - the fixture user holds no `app_user_identities` row - only that the
//! `IdP` session the caller used survives.
//!
//! The database is REQUIRED: a missing test database fails the run rather than
//! quietly passing an empty test body. Run with `--test-threads=1` (the auth
//! suite shares rows).

use crate::common;

use std::sync::Arc;

use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_mailer::{Email, Mailer};

use common::{test_auth_config, CapturingMailer};
use zeroship_auth::config::AuthConfig;
use zeroship_auth::identity::{password, totp};
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions::CreateSession;
use zeroship_auth::store::{sessions, totp as totp_store, users};

const FIXTURE_PASSWORD: &str = "correct-horse-battery-staple-2fa";
const CSRF_TOKEN: &str = "csrf-removal-notice";

// `common::CapturingMailer` keeps the whole `Email`, not a counter: the
// assertion these tests need is what the account holder was TOLD, and a count
// cannot distinguish a notice naming the removal from an empty render that
// still incremented.

struct Fixture {
    cfg: Arc<AuthConfig>,
    pg: Arc<compio_postgres::Client>,
    mailer: Arc<CapturingMailer>,
    user_id: Uuid,
    email: String,
    session_id: Uuid,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot(label: &str) -> Self {
        let db_url = crate::common::test_database_url();
        let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[totp_removal_notice_test] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let tag = Uuid::new_v4().simple().to_string();
        let email = format!("totp-notice-{label}-{tag}@zeroship.test");
        let phc = password::hash(FIXTURE_PASSWORD).expect("hash fixture password");
        let user = users::create(&pg, &email, "Removal Notice", Some(&phc))
            .await
            .expect("create fixture user");

        let session = sessions::create(
            &pg,
            &CreateSession {
                user_id: user.id,
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
            mailer: Arc::new(CapturingMailer::default()),
            user_id: user.id,
            email,
            session_id: session.id,
        }
    }

    fn key(&self) -> [u8; 32] {
        totp::key_from_config(self.cfg.settings.totp_enc_key.expose_str()).expect("totp enc key")
    }

    /// Seed a CONFIRMED (login-gating) credential; hand back its raw secret.
    #[allow(clippy::future_not_send)]
    async fn seed_confirmed(&self) -> Vec<u8> {
        let secret = totp::generate_secret();
        let ct = totp::encrypt_secret(&self.key(), self.user_id, &secret).expect("encrypt");
        assert!(
            totp_store::enroll(&self.pg, self.user_id, &ct, false)
                .await
                .expect("seed enroll"),
            "seeding a pending credential must write"
        );
        let (_, hashes) = totp::generate_backup_codes().expect("backup codes");
        totp_store::confirm(&self.pg, self.user_id, &hashes)
            .await
            .expect("seed confirm");
        secret
    }

    /// POST one of the `/me/2fa/*` routes through the real ntex service, with a
    /// valid session cookie, a valid CSRF double-submit, and whatever re-auth
    /// proof is supplied. `route` is `"disable"` or `"enroll"`.
    #[allow(clippy::future_not_send)]
    async fn post(&self, route: &str, code: Option<&str>, pw: Option<&str>) -> (u16, String) {
        let mailer_state: Arc<dyn Mailer> = self.mailer.clone();
        let handler = match route {
            "disable" => web::post().to(zeroship_auth::ui::totp::disable),
            "enroll" => web::post().to(zeroship_auth::ui::totp::enroll),
            other => panic!("unknown route {other}"),
        };
        let uri = format!("/me/2fa/{route}");
        let svc = test::init_service(
            web::App::new()
                .state(self.cfg.clone())
                .state(self.pg.clone())
                .state(mailer_state)
                .service(web::resource(uri.as_str()).route(handler)),
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
            .uri(&uri)
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
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[allow(clippy::future_not_send)]
    async fn is_enabled(&self) -> bool {
        totp_store::is_enabled(&self.pg, self.user_id)
            .await
            .expect("is_enabled")
    }

    /// The enforcement truth for the `IdP` tier: does the cookie the caller used
    /// still resolve to a live session? A `gateway_sessions` row count would
    /// not answer this - that table is the audit record, not the request path.
    ///
    /// `VALIDATE_SESSION_SQL` requires BOTH `revoked_at IS NULL` and the row's
    /// `credential_version` still matching the user's, so this one call goes
    /// false under either teardown a future change might reach for.
    #[allow(clippy::future_not_send)]
    async fn session_still_valid(&self) -> bool {
        sessions::validate(&self.pg, self.session_id)
            .await
            .expect("validate session")
            .is_some()
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        for sql in [
            "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
        ] {
            let _ = self
                .pg
                .execute(sql, &[&format!("totp:verify:{}", self.user_id)])
                .await;
        }
        for sql in [
            "DELETE FROM zeroship.audit_events WHERE actor_user_id = $1",
            "DELETE FROM zeroship.totp_backup_codes WHERE user_id = $1",
            "DELETE FROM zeroship.totp_credentials WHERE user_id = $1",
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            "DELETE FROM zeroship.users WHERE id = $1",
        ] {
            let _ = self.pg.execute(sql, &[&self.user_id]).await;
        }
    }
}

/// Assert the captured message is the second-factor-removal notice, addressed
/// to the account, and carrying the password-reset remedy. Anything weaker
/// (a count, a non-empty body) would pass on an empty render.
fn assert_is_removal_notice(msg: &Email, to: &str) {
    assert_eq!(msg.to.email, to, "the notice goes to the account address");
    assert!(
        msg.subject.to_lowercase().contains("two-factor"),
        "subject must name the change, got {:?}",
        msg.subject
    );
    let html = msg.html.as_deref().unwrap_or_default();
    for body in [msg.text.as_str(), html] {
        assert!(
            body.contains("removed"),
            "the notice must say the authenticator was removed, got {body:?}"
        );
        assert!(
            body.contains("/forgot"),
            "the notice must offer the password reset - the only flow that \
             revokes anything - got {body:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// disable
// ---------------------------------------------------------------------------

/// The defect: a successful `/me/2fa/disable` told the account holder nothing.
/// It must now send the notice - and must still leave the session working,
/// because tearing it down would evict the owner and cost an attacker who
/// already passed `verify_reauth` exactly one `/login`.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn disable_notifies_the_account_holder_and_revokes_nothing() {
    let fx = Fixture::boot("disable").await;
    fx.seed_confirmed().await;
    assert!(fx.is_enabled().await, "fixture must start with 2FA armed");

    let (status, body) = fx.post("disable", None, Some(FIXTURE_PASSWORD)).await;

    assert_eq!(status, 200, "password re-auth must disable 2FA, got {status} {body}");
    assert!(!fx.is_enabled().await, "2FA must be off after a successful disable");

    let sent = fx.mailer.sent();
    assert_eq!(sent.len(), 1, "exactly one removal notice, got {}", sent.len());
    assert_is_removal_notice(&sent[0], &fx.email);

    assert!(
        fx.session_still_valid().await,
        "turning 2FA off must NOT revoke the caller's session: the proof it \
         demanded is the same material that signs the actor back in, so a \
         teardown would only evict the account holder"
    );

    fx.cleanup().await;
}

/// A REFUSED disable must send nothing. A notice on a failed attempt is a
/// mail-flood primitive for anyone holding a session cookie, and it trains the
/// account holder to ignore the one message that matters.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn refused_disable_sends_no_notice() {
    let fx = Fixture::boot("refused").await;
    fx.seed_confirmed().await;

    let (status, body) = fx.post("disable", None, None).await;

    assert_eq!(status, 401, "a cookie-only disable must be refused, got {status} {body}");
    assert!(fx.is_enabled().await, "a refused disable leaves 2FA armed");
    assert!(
        fx.mailer.sent().is_empty(),
        "a refused disable must not mail the account holder"
    );

    fx.cleanup().await;
}

// ---------------------------------------------------------------------------
// enroll - the same end state, reached by the other door
// ---------------------------------------------------------------------------

/// Replacing a CONFIRMED credential resets it to pending, which turns the login
/// challenge off just as `disable` does. It must send the same notice, or an
/// attacker reaches the unprotected state without one.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn enroll_over_a_confirmed_credential_notifies() {
    let fx = Fixture::boot("reenroll").await;
    fx.seed_confirmed().await;

    let (status, body) = fx.post("enroll", None, Some(FIXTURE_PASSWORD)).await;

    assert_eq!(status, 200, "password re-auth must be accepted, got {status} {body}");
    assert!(
        !fx.is_enabled().await,
        "re-enrollment resets the credential to pending, so 2FA is off"
    );

    let sent = fx.mailer.sent();
    assert_eq!(
        sent.len(),
        1,
        "re-enrolling reaches the same unprotected state as disable and owes \
         the same notice, got {}",
        sent.len()
    );
    assert_is_removal_notice(&sent[0], &fx.email);

    fx.cleanup().await;
}

/// A FIRST enrollment turns nothing off. Mailing here would make the notice
/// routine, which is how a security notice stops being read.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn first_enrollment_sends_no_notice() {
    let fx = Fixture::boot("firstenroll").await;
    assert!(!fx.is_enabled().await, "fixture starts with no credential");

    let (status, body) = fx.post("enroll", None, None).await;

    assert_eq!(status, 200, "a first enrollment needs no re-auth, got {status} {body}");
    assert!(
        fx.mailer.sent().is_empty(),
        "a first enrollment removes nothing and must send no notice"
    );

    fx.cleanup().await;
}
