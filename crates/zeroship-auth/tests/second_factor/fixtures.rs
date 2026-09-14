//! Accounts, authenticators and browser requests scoped to these scenarios.

#![allow(clippy::future_not_send)]

use crate::common::{self, CookieJar, auth_server::AuthServer};
use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};
use zeroship_auth::{
    identity::{password, totp},
    sessions::totp_challenge,
    store::{sessions, totp as totp_store, users},
};
use zeroship_core::UserId;

pub(super) const PASSWORD: &str = "second factor fixture password phrase";

pub(super) async fn account(server: &AuthServer, email: &str) -> users::UserRow {
    let hash = password::hash(PASSWORD).unwrap();
    Box::pin(users::create(&server.orm, email, "Second factor", Some(&hash)))
        .await
        .unwrap()
}

pub(super) struct Authenticator {
    pub secret: Vec<u8>,
    pub backups: Vec<String>,
}

impl Authenticator {
    pub async fn pending(server: &AuthServer, user: &UserId) -> Self {
        let secret = totp::generate_secret();
        let key = totp::key_from_config(server.config.settings.totp_enc_key.expose_str()).unwrap();
        let encrypted = totp::encrypt_secret(&key, user, &secret).unwrap();
        assert!(
            totp_store::enroll(&server.pg, user, &encrypted, false)
                .await
                .unwrap()
        );
        assert!(!totp_store::is_enabled(&server.pg, user).await.unwrap());
        Self {
            secret,
            backups: Vec::new(),
        }
    }

    pub async fn confirm(&mut self, server: &AuthServer, user: &UserId) {
        let (plain, hashes) = totp::generate_backup_codes().unwrap();
        assert!(
            totp_store::confirm(&server.pg, user, &hashes)
                .await
                .unwrap()
        );
        assert!(totp_store::is_enabled(&server.pg, user).await.unwrap());
        self.backups = plain;
    }

    pub async fn confirmed(server: &AuthServer, user: &UserId) -> Self {
        let mut device = Self::pending(server, user).await;
        device.confirm(server, user).await;
        device
    }

    pub fn code(&self) -> String {
        totp::code_at(&self.secret, unix_now())
    }

    /// Select a numeric code outside every window the challenge could accept.
    pub fn rejected_code(&self) -> (String, u64) {
        let now = unix_now();
        let deadline = now + u64::try_from(totp_challenge::CHALLENGE_MAX_AGE_SECS).unwrap();
        let first = (now / totp::STEP_SECS).saturating_sub(u64::from(totp::SKEW));
        let last = deadline / totp::STEP_SECS + u64::from(totp::SKEW);
        let accepted: HashSet<_> = (first..=last)
            .map(|step| totp::code_at(&self.secret, step * totp::STEP_SECS))
            .collect();
        let code = (0..10_u64.pow(u32::try_from(totp::DIGITS).unwrap()))
            .map(|value| format!("{value:0width$}", width = totp::DIGITS))
            .find(|value| !accepted.contains(value))
            .expect("a code outside the accepted windows");
        (code, deadline)
    }
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub(super) async fn post(
    server: &AuthServer,
    path: &str,
    cookies: &str,
    fields: &[(&str, &str)],
) -> cyper::Response {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish();
    server
        .http
        .request(http::Method::POST, format!("{}{path}", server.auth_base))
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap()
        .header("cookie", cookies)
        .unwrap()
        .body(body)
        .send()
        .await
        .unwrap()
}

pub(super) async fn password_login(
    server: &AuthServer,
    user: &users::UserRow,
    password: &str,
    return_to: &str,
) -> cyper::Response {
    let form = server
        .http
        .get(format!("{}/login", server.auth_base))
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(form.status().as_u16(), 200);
    let csrf = common::read_set_cookie(&form, "__Host-zsidp_csrf").unwrap();
    post(
        server,
        "/login",
        &format!("__Host-zsidp_csrf={csrf}"),
        &[
            ("csrf", &csrf),
            ("email", &user.email),
            ("password", password),
            ("return_to", return_to),
        ],
    )
    .await
}

pub(super) struct Challenge {
    pub cookies: CookieJar,
    pub csrf: String,
    pub return_to: String,
}

impl Challenge {
    pub async fn from_response(response: cyper::Response, return_to: &str) -> Self {
        assert_eq!(response.status().as_u16(), 200);
        assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        let stash = common::read_set_cookie(&response, totp_challenge::COOKIE_NAME)
            .expect("first factor produces a challenge cookie");
        assert!(!stash.is_empty());
        let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
        let mut cookies = CookieJar::default();
        cookies.absorb(&response);
        let html = response.text().await.unwrap();
        assert!(html.contains("action=\"/login/2fa\""));
        assert!(html.contains(&csrf));
        Self {
            cookies,
            csrf,
            return_to: return_to.into(),
        }
    }

    pub async fn password(server: &AuthServer, user: &users::UserRow, password: &str) -> Self {
        let return_to = AuthServer::fresh_challenge();
        Self::from_response(
            password_login(server, user, password, &return_to).await,
            &return_to,
        )
        .await
    }

    pub async fn submit(&mut self, server: &AuthServer, code: &str) -> cyper::Response {
        let response = post(
            server,
            "/login/2fa",
            &self.cookies.header(),
            &[
                ("csrf", &self.csrf),
                ("code", code),
                ("return_to", &self.return_to),
            ],
        )
        .await;
        self.cookies.absorb(&response);
        if let Some(csrf) = common::read_set_cookie(&response, "__Host-zsidp_csrf") {
            self.csrf = csrf;
        }
        response
    }

    pub async fn reject_wrong_code(&mut self, server: &AuthServer, device: &Authenticator) {
        let (code, deadline) = device.rejected_code();
        let response = self.submit(server, &code).await;
        assert!(
            unix_now() <= deadline,
            "negative control outlived its selected code window"
        );
        assert_refused(response, "invalid code").await;
    }
}

pub(super) async fn assert_refused(response: cyper::Response, message: &str) {
    assert_eq!(response.status().as_u16(), 401);
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    assert!(response.text().await.unwrap().contains(message));
}

pub(super) async fn assert_counts(server: &AuthServer, sessions: i64, identities: i64) {
    let row = server.pg.query_one("SELECT (SELECT COUNT(*) FROM zeroship.idp_sessions), (SELECT COUNT(*) FROM zeroship.federated_identities)", &[]).await.unwrap();
    assert_eq!(
        (row.get::<_, i64>(0), row.get::<_, i64>(1)),
        (sessions, identities)
    );
}

pub(super) async fn assert_session(
    server: &AuthServer,
    response: &cyper::Response,
    user: &UserId,
    return_to: &str,
    method: &str,
    amr: &[&str],
) {
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(common::location(response), return_to);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let raw = common::read_set_cookie(response, "__Host-zsidp_session")
        .expect("successful challenge supplies a session cookie");
    let session = sessions::validate(&server.pg, raw.parse().unwrap())
        .await
        .unwrap()
        .expect("cookie names a usable session");
    assert_eq!(&session.user_id, user);
    assert_eq!(session.auth_method, method);
    assert_eq!(session.amr, amr);
    assert_eq!(session.acr, Some(format!("urn:zeroship:{method}")));
    if amr.contains(&"otp") {
        assert_eq!(
            common::read_set_cookie(response, totp_challenge::COOKIE_NAME).as_deref(),
            Some("")
        );
    }
}

pub(super) async fn unused_backups(server: &AuthServer, user: &UserId) -> usize {
    totp_store::unused_backup_codes(&server.pg, user)
        .await
        .unwrap()
        .len()
}
