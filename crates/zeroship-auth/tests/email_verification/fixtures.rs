//! Email-verification HTTP requests and persisted outcomes.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their owning compio runtime"
)]

use crate::common::{self, auth_server::AuthServer};
use compio_postgres::Client;
use zeroship_auth::{identity::verification, store::users};
use zeroship_core::UserId;

pub(super) async fn issue(pg: &Client, email: &str) -> (UserId, String) {
    let user = users::create(pg, email, "Verification", None)
        .await
        .unwrap();
    let issued = verification::issue(pg, &user.id, email).await.unwrap();
    (user.id, issued.raw)
}

pub(super) async fn landing(server: &AuthServer, token: &str) -> String {
    let response = server
        .http
        .get(format!("{}/verify?token={token}", server.auth_base))
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf")
        .expect("verification landing supplies CSRF");
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    let html = response.text().await.unwrap();
    assert!(html.contains("action=\"/verify/redeem\""));
    assert!(html.contains(&format!("name=\"token\" value=\"{token}\"")));
    assert!(html.contains(&csrf));
    csrf
}

pub(super) async fn redeem(
    server: &AuthServer,
    token: &str,
    csrf: &str,
    cookie: Option<&str>,
    request_id: &str,
) -> cyper::Response {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", token)
        .append_pair("csrf", csrf)
        .finish();
    let mut request = server
        .http
        .request(
            http::Method::POST,
            format!("{}/verify/redeem", server.auth_base),
        )
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap()
        .header("x-request-id", request_id)
        .unwrap();
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie).unwrap();
    }
    request.body(body).send().await.unwrap()
}

pub(super) async fn assert_state(pg: &Client, user_id: &UserId, redeemed: bool) {
    let row = pg
        .query_one(
            "SELECT u.email_verified_at IS NOT NULL, v.consumed_at IS NOT NULL \
         FROM zeroship.users u JOIN zeroship.email_verifications v ON v.user_id = u.id \
         WHERE u.id = $1",
            &[&user_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, bool>(0),
        redeemed,
        "the user's verification state"
    );
    assert_eq!(
        row.get::<_, bool>(1),
        redeemed,
        "the token's consumption state"
    );
    let sessions: i64 = pg
        .query_one("SELECT COUNT(*) FROM zeroship.idp_sessions", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(sessions, 0, "email verification does not sign in a user");
}

pub(super) async fn assert_failure_audit(pg: &Client, request_id: &str) {
    let rows = pg
        .query(
            "SELECT outcome, detail->>'reason' AS reason FROM zeroship.audit_events \
         WHERE event_type = 'verification_redeemed' AND request_id = $1",
            &[&request_id],
        )
        .await
        .unwrap();
    let [row] = rows.as_slice() else {
        panic!("expected the correlated verification audit: {rows:?}")
    };
    assert_eq!(row.get::<_, String>("outcome"), "failure");
    assert_eq!(row.get::<_, String>("reason"), "invalid_or_expired");
}

pub(super) async fn assert_success(response: cyper::Response) {
    assert_eq!(response.status().as_u16(), 200);
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    assert!(response.text().await.unwrap().contains("Email verified"));
}
