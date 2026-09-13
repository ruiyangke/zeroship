//! Account-linking requests and observations private to these scenarios.

#![allow(clippy::future_not_send)]

use crate::common::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::{
    identity::{
        linker::{self, LinkOutcome, LinkResume, ResolvedProfile},
        password,
    },
    store::{identities, sessions, users},
};
use zeroship_authn::rate_limit::Quota;

pub(super) const PASSWORD: &str = "correct account linking password phrase";
pub(super) const WRONG_PASSWORD: &str = "incorrect account linking password phrase";

pub(super) struct Confirmation {
    pub user: users::UserRow,
    pub token: String,
    pub subject: String,
    pub return_to: String,
}

impl Confirmation {
    pub async fn new(server: &AuthServer, email: &str, subject: &str) -> Self {
        let hash = password::hash(PASSWORD).unwrap();
        let user = users::create(&server.pg, email, "Account linking", Some(&hash))
            .await
            .unwrap();
        let return_to =
            common::native_authorize_return_to("link-test-client", "http://127.0.0.1:9999/cb");
        let token = issue(server, &user, subject, &return_to).await;
        let confirmation = Self {
            user,
            token,
            subject: subject.into(),
            return_to,
        };
        assert_unlinked(server, &confirmation).await;
        confirmation
    }

    pub async fn token_for_target(&self, server: &AuthServer, target: &str) -> String {
        issue(server, &self.user, &self.subject, target).await
    }

    pub async fn submit(&self, server: &AuthServer, password: &str, ip: &str) -> cyper::Response {
        submit_token(server, &self.token, password, ip).await
    }

    fn bucket_key(&self, ip: &str) -> String {
        format!("link_attempt:{}:{ip}", self.user.id.as_str())
    }

    pub async fn assert_tokens(&self, server: &AuthServer, ip: &str, expected: f64) {
        let matches: bool = server.pg.query_one(
            "SELECT tokens::DOUBLE PRECISION = $2 FROM zeroship.rate_limits WHERE bucket_key = $1",
            &[&self.bucket_key(ip), &expected],
        ).await.expect("link requests persist their own rate-limit bucket").get(0);
        assert!(matches, "unexpected remaining token balance for {ip}");
    }

    /// Prevent elapsed request time from replenishing the bucket under test.
    pub async fn freeze_refill(&self, database: &Database, ip: &str) {
        let updated = database.connect().await.execute(
            "UPDATE zeroship.rate_limits SET updated_at = NOW() + INTERVAL '1 day' WHERE bucket_key = $1",
            &[&self.bucket_key(ip)],
        ).await.unwrap();
        assert_eq!(updated, 1, "freeze only the bucket created by this case");
    }

    /// Make the persisted bucket old enough for the production quota to refill it.
    pub async fn elapse_refill(&self, database: &Database, ip: &str) {
        let quota = Quota::LINK_ATTEMPT;
        let refill_seconds = (quota.capacity + 1.0) / quota.refill_per_sec;
        assert!(refill_seconds.is_finite() && refill_seconds > 0.0);
        let updated = database.connect().await.execute(
            "UPDATE zeroship.rate_limits \
             SET updated_at = NOW() - $2::DOUBLE PRECISION * INTERVAL '1 second' WHERE bucket_key = $1",
            &[&self.bucket_key(ip), &refill_seconds],
        ).await.unwrap();
        assert_eq!(updated, 1, "age only the bucket created by this case");
    }
}

async fn issue(
    server: &AuthServer,
    user: &users::UserRow,
    subject: &str,
    return_to: &str,
) -> String {
    let outcome = linker::resolve_or_link(
        &server.pg,
        &ResolvedProfile {
            provider: "github",
            subject,
            email: &user.email,
            name: Some("Federated profile"),
            avatar_url: None,
            provider_trusted_for_email: true,
            raw_profile: None,
        },
        LinkResume::ReturnTo(return_to),
        server
            .config
            .settings
            .stash_signing_key
            .expose_str()
            .as_bytes(),
    )
    .await
    .unwrap();
    let LinkOutcome::NeedsConfirmation {
        pending_token,
        existing_email,
        provider,
    } = outcome
    else {
        panic!("a password-bearing account requires confirmation: {outcome:?}");
    };
    assert_eq!(existing_email, user.email);
    assert_eq!(provider, "github");
    pending_token
}

pub(super) async fn submit_token(
    server: &AuthServer,
    token: &str,
    password: &str,
    ip: &str,
) -> cyper::Response {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", token)
        .finish();
    let response = server
        .http
        .get(format!("{}/link?{query}", server.auth_base))
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf")
        .expect("link form supplies CSRF cookie");
    let html = response.text().await.unwrap();
    assert!(html.contains("action=\"/link\""));
    assert!(html.contains(&csrf));
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", token)
        .append_pair("password", password)
        .finish();
    server
        .http
        .request(http::Method::POST, format!("{}/link", server.auth_base))
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap()
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .unwrap()
        .header("x-forwarded-for", ip)
        .unwrap()
        .body(body)
        .send()
        .await
        .unwrap()
}

pub(super) async fn assert_unlinked(server: &AuthServer, confirmation: &Confirmation) {
    assert!(
        identities::find_by_provider_subject(&server.pg, "github", &confirmation.subject)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        identities::list_for_user(&server.pg, &confirmation.user.id)
            .await
            .unwrap()
            .is_empty()
    );
    let sessions: i64 = server
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&confirmation.user.id.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        sessions, 0,
        "no session may be persisted before successful confirmation"
    );
}

pub(super) async fn assert_refused(
    server: &AuthServer,
    confirmation: &Confirmation,
    response: cyper::Response,
    status: u16,
    message: &str,
) {
    assert_eq!(response.status().as_u16(), status);
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    assert!(response.text().await.unwrap().contains(message));
    assert_unlinked(server, confirmation).await;
}

pub(super) async fn assert_linked(
    server: &AuthServer,
    confirmation: &Confirmation,
    response: &cyper::Response,
) {
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(common::location(response), confirmation.return_to);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let cookie = common::read_set_cookie(response, "__Host-zsidp_session")
        .expect("linking returns a session cookie");
    let session = sessions::validate(&server.pg, cookie.parse().unwrap())
        .await
        .unwrap()
        .expect("returned cookie names a live session");
    assert_eq!(session.user_id, confirmation.user.id);
    assert_eq!(session.auth_method, "github");
    assert_eq!(session.amr, ["oauth", "pwd"]);
    assert_eq!(session.acr.as_deref(), Some("urn:zeroship:github"));
    let linked = identities::list_for_user(&server.pg, &confirmation.user.id)
        .await
        .unwrap();
    let [identity] = linked.as_slice() else {
        panic!("expected the confirmed identity: {linked:?}")
    };
    assert_eq!(identity.provider, "github");
    assert_eq!(identity.subject, confirmation.subject);
    assert_eq!(
        identity.email_at_link.as_deref(),
        Some(confirmation.user.email.as_str())
    );
}
