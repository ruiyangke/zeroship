//! Requests and persisted-state assertions private to password-login scenarios.

#![allow(clippy::future_not_send)]

use crate::common::{self, auth_server::AuthServer};
use zeroship_auth::{
    identity::password,
    store::{sessions, users},
};
use zeroship_core::UserId;

pub(super) const PASSWORD: &str = "password login fixture phrase";
pub(super) const WRONG_PASSWORD: &str = "incorrect password fixture phrase";

pub(super) enum LockState {
    Clear,
    Active,
    Expired,
}

pub(super) async fn user(server: &AuthServer, email: &str) -> users::UserRow {
    let hash = password::hash(PASSWORD).unwrap();
    users::create(&server.orm, email, "Password login", Some(&hash))
        .await
        .unwrap()
}

pub(super) async fn login(
    server: &AuthServer,
    email: &str,
    password: &str,
    ip: &str,
) -> cyper::Response {
    let csrf = form_csrf(server, "/login").await;
    post(
        server,
        "/login",
        &csrf,
        &[
            ("email", email),
            ("password", password),
            ("return_to", "/me"),
        ],
        ip,
    )
    .await
}

pub(super) async fn reset(server: &AuthServer, token: &str, password: &str) -> cyper::Response {
    let csrf = form_csrf(server, &format!("/reset?token={token}")).await;
    post(
        server,
        "/reset",
        &csrf,
        &[("token", token), ("password", password)],
        "192.0.2.200",
    )
    .await
}

async fn form_csrf(server: &AuthServer, path: &str) -> String {
    let response = server
        .http
        .get(format!("{}{path}", server.auth_base))
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    common::read_set_cookie(&response, "__Host-zsidp_csrf").expect("form supplies CSRF cookie")
}

async fn post(
    server: &AuthServer,
    path: &str,
    csrf: &str,
    fields: &[(&str, &str)],
    ip: &str,
) -> cyper::Response {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", csrf)
        .extend_pairs(fields.iter().copied())
        .finish();
    server
        .http
        .request(http::Method::POST, format!("{}{path}", server.auth_base))
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

pub(super) async fn assert_rejected(response: cyper::Response) {
    assert_eq!(response.status().as_u16(), 401);
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("invalid email or password")
    );
}

pub(super) async fn assert_session(server: &AuthServer, response: &cyper::Response, id: &UserId) {
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(common::location(response), "/me");
    let cookie =
        common::read_set_cookie(response, "__Host-zsidp_session").expect("login sets a session");
    let session = sessions::validate(&server.pg, cookie.parse().unwrap())
        .await
        .unwrap()
        .expect("cookie names a live session");
    assert_eq!(&session.user_id, id);
    assert_eq!(session.auth_method, "pwd");
    assert_eq!(session.amr, ["pwd"]);
    assert_eq!(session.acr.as_deref(), Some("urn:zeroship:pwd"));
}

pub(super) async fn assert_state(server: &AuthServer, id: &UserId, failures: i32, lock: LockState) {
    let (lock_present, locked) = match lock {
        LockState::Clear => (false, false),
        LockState::Active => (true, true),
        LockState::Expired => (true, false),
    };
    let row = server.pg.query_one(
        "SELECT failed_login_count, locked_until IS NOT NULL, COALESCE(locked_until > NOW(), false) \
         FROM zeroship.users WHERE id = $1", &[&id.as_str()],
    ).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), failures, "persisted failure count");
    assert_eq!(
        row.get::<_, bool>(1),
        lock_present,
        "persisted lock timestamp"
    );
    assert_eq!(row.get::<_, bool>(2), locked, "active lock");
}

pub(super) async fn lock_through_login(server: &AuthServer, user: &users::UserRow) {
    const { assert!(users::lockout::THRESHOLD > 0) };
    for failure in 1..=users::lockout::THRESHOLD {
        // Each request has its own IP so the account lock is exercised before
        // the independent per-IP limiter can reject it.
        let ip = format!("198.51.100.{failure}");
        assert_rejected(login(server, &user.email, WRONG_PASSWORD, &ip).await).await;
        let lock = if failure >= users::lockout::THRESHOLD {
            LockState::Active
        } else {
            LockState::Clear
        };
        assert_state(server, &user.id, failure, lock).await;
    }
    assert_rejected(login(server, &user.email, PASSWORD, "198.51.100.200").await).await;
    assert_state(
        server,
        &user.id,
        users::lockout::THRESHOLD,
        LockState::Active,
    )
    .await;
}

pub(super) async fn session_count(server: &AuthServer) -> i64 {
    server
        .pg
        .query_one("SELECT COUNT(*) FROM zeroship.idp_sessions", &[])
        .await
        .unwrap()
        .get(0)
}
