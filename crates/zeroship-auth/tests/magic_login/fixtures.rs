//! Request and state observations private to the magic-login scenarios.

// The server and database stay on the case's compio thread.
#![allow(clippy::future_not_send)]

use crate::common::{self, CapturingMailer, auth_server::AuthServer};
use zeroship_auth::{
    csrf,
    store::{sessions, users},
};
use zeroship_core::UserId;

pub(super) struct RequestedLogin {
    pub csrf: String,
    pub nonce: String,
    pub token: String,
    pub return_to: String,
    pub landing_url: String,
}

impl RequestedLogin {
    pub async fn start(server: &AuthServer, mailer: &CapturingMailer, email: &str) -> Self {
        let return_to =
            common::native_authorize_return_to("magic-test-client", "http://127.0.0.1:9999/cb");
        let response = start_request(server, email, &return_to).await;
        assert_eq!(response.status().as_u16(), 200);
        let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf")
            .expect("requesting browser receives completion CSRF");
        let nonce = common::read_set_cookie(&response, "__Host-zsidp_magic_csrf")
            .expect("requesting browser receives the link nonce");
        let html = response.text().await.unwrap();
        assert!(
            html.contains("/magic/complete"),
            "start renders the code-entry form"
        );
        assert!(html.contains(&nonce));

        let sent = mailer.sent();
        let [email] = sent.as_slice() else {
            panic!("expected the login email: {sent:?}")
        };
        let link = email
            .text
            .split_whitespace()
            .find(|part| part.contains("/magic/verify?"))
            .expect("rendered email carries the redemption URL")
            .trim_matches(|ch| matches!(ch, '<' | '>' | '"' | '\''));
        let link = url::Url::parse(link).unwrap();
        let parameter = |key| {
            link.query_pairs()
                .find(|(name, _)| name == key)
                .unwrap_or_else(|| panic!("missing email link parameter {key}"))
                .1
                .into_owned()
        };
        assert_eq!(parameter("return_to"), return_to);
        Self {
            csrf,
            nonce,
            token: parameter("token"),
            return_to,
            landing_url: format!(
                "{}{}?{}",
                server.auth_base,
                link.path(),
                link.query().unwrap()
            ),
        }
    }

    pub async fn redeem(
        &self,
        server: &AuthServer,
        csrf: &str,
        cookie: Option<&str>,
    ) -> cyper::Response {
        post_form(
            server,
            "/magic/verify/redeem",
            &[
                ("csrf", csrf),
                ("token", &self.token),
                ("return_to", &self.return_to),
            ],
            cookie,
        )
        .await
    }

    pub async fn complete(
        &self,
        server: &AuthServer,
        code: &str,
        return_to: &str,
    ) -> cyper::Response {
        post_form(
            server,
            "/magic/complete",
            &[
                ("csrf", &self.csrf),
                ("csrf_nonce", &self.nonce),
                ("code", code),
                ("return_to", return_to),
            ],
            Some(&format!("__Host-zsidp_csrf={}", self.csrf)),
        )
        .await
    }
}

pub(super) async fn start_request(
    server: &AuthServer,
    email: &str,
    return_to: &str,
) -> cyper::Response {
    let csrf = csrf::generate_token();
    post_form(
        server,
        "/magic/start",
        &[("csrf", &csrf), ("email", email), ("return_to", return_to)],
        Some(&format!("__Host-zsidp_csrf={csrf}")),
    )
    .await
}

async fn post_form(
    server: &AuthServer,
    path: &str,
    fields: &[(&str, &str)],
    cookie: Option<&str>,
) -> cyper::Response {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish();
    let mut request = server
        .http
        .request(http::Method::POST, format!("{}{path}", server.auth_base))
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap();
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie).unwrap();
    }
    request.body(body).send().await.unwrap()
}

pub(super) async fn assert_link_state(server: &AuthServer, nonce: &str, consumed: bool) {
    let row = server
        .pg
        .query_one(
            "SELECT consumed_pending_at IS NULL, consumed_at IS NOT NULL \
         FROM zeroship.magic_links WHERE csrf_nonce = $1",
            &[&nonce],
        )
        .await
        .unwrap();
    if !consumed {
        assert!(
            row.get::<_, bool>(0),
            "failed request must release its link reservation"
        );
    }
    assert_eq!(row.get::<_, bool>(1), consumed, "link consumption");
}

pub(super) async fn assert_completion_state(
    server: &AuthServer,
    nonce: &str,
    consumed: bool,
    attempts: i16,
) {
    let row = server
        .pg
        .query_one(
            "SELECT consumed_pending_at IS NULL, consumed_at IS NOT NULL, attempts \
         FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&nonce],
        )
        .await
        .unwrap();
    if !consumed {
        assert!(
            row.get::<_, bool>(0),
            "failed request must release its completion reservation"
        );
    }
    assert_eq!(row.get::<_, bool>(1), consumed, "completion consumption");
    assert_eq!(
        row.get::<_, i16>(2),
        attempts,
        "only a new reservation or wrong code spends the attempt budget"
    );
}

pub(super) async fn session_count(server: &AuthServer) -> i64 {
    server
        .pg
        .query_one("SELECT COUNT(*) FROM zeroship.idp_sessions", &[])
        .await
        .unwrap()
        .get(0)
}

pub(super) async fn assert_login_session(
    server: &AuthServer,
    response: &cyper::Response,
    email: &str,
    return_to: &str,
) {
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(common::location(response), return_to);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let cookie = common::read_set_cookie(response, "__Host-zsidp_session")
        .expect("successful login sets the session cookie");
    let session = sessions::validate(&server.pg, cookie.parse().unwrap())
        .await
        .unwrap()
        .expect("returned cookie names a live session");
    assert_eq!(session.auth_method, "magic");
    assert_eq!(session.amr, ["magic"]);
    assert_eq!(session.acr.as_deref(), Some("urn:zeroship:magic"));
    let user = server
        .pg
        .query_one(
            "SELECT email::text, email_verified_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&session.user_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(user.get::<_, String>(0), email);
    assert!(user.get::<_, bool>(1), "magic login verifies this user");
    assert_eq!(session_count(server).await, 1);
}

pub(super) async fn assert_login_rejected(response: cyper::Response) {
    assert_eq!(
        response.status().as_u16(),
        200,
        "rejected login must not redirect"
    );
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    assert!(response.text().await.unwrap().contains("session expired"));
}

pub(super) async fn soft_lock(server: &AuthServer, user_id: &UserId) {
    for _ in 0..users::lockout::THRESHOLD {
        users::record_login_failure(&server.pg, user_id)
            .await
            .unwrap();
    }
    let locked: bool = server
        .pg
        .query_one(
            "SELECT locked_until > NOW() FROM zeroship.users WHERE id = $1",
            &[&user_id.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert!(locked, "recovery starts from a locked account");
}

pub(super) async fn assert_lock_cleared(server: &AuthServer, user_id: &UserId) {
    let row = server
        .pg
        .query_one(
            "SELECT failed_login_count, locked_until IS NULL FROM zeroship.users WHERE id = $1",
            &[&user_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 0);
    assert!(
        row.get::<_, bool>(1),
        "magic login clears the password lock"
    );
}
