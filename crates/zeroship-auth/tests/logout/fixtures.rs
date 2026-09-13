//! Browser sessions and requests private to the logout scenarios.

#![allow(clippy::future_not_send)]

use std::sync::Arc;

use crate::common::{self, CookieJar, auth_server::AuthServer};
use zeroship_auth::{
    identity::password,
    oidc::Issuer,
    store::{sessions, users},
};
use zeroship_core::UserId;

const PASSWORD: &str = "logout fixture password phrase";

pub(super) fn issuer() -> Arc<Issuer> {
    Arc::new(
        Issuer::from_signing_key(
            &ed25519_dalek::SigningKey::from_bytes(&[19; 32]),
            [9; 32],
            "https://auth.zeroship.test/oauth2".to_owned(),
        )
        .unwrap(),
    )
}

pub(super) async fn account(server: &AuthServer, email: &str) -> users::UserRow {
    users::create(
        &server.pg,
        email,
        "Logout fixture",
        Some(&password::hash(PASSWORD).unwrap()),
    )
    .await
    .unwrap()
}

pub(super) struct BrowserSession {
    pub cookies: CookieJar,
    pub id: uuid::Uuid,
    pub user_id: UserId,
}

impl BrowserSession {
    pub async fn login(server: &AuthServer, user: &users::UserRow) -> Self {
        let response = server
            .http
            .get(format!("{}/login", server.auth_base))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
        let mut cookies = CookieJar::default();
        cookies.absorb(&response);
        let response = post(
            server,
            "/login",
            &cookies.header(),
            &[
                ("csrf", &csrf),
                ("email", &user.email),
                ("password", PASSWORD),
                ("return_to", "/me"),
            ],
        )
        .await;
        assert_eq!(response.status().as_u16(), 303);
        assert_eq!(common::location(&response), "/me");
        let id = common::read_set_cookie(&response, "__Host-zsidp_session")
            .unwrap()
            .parse()
            .unwrap();
        cookies.absorb(&response);
        let browser = Self {
            cookies,
            id,
            user_id: user.id.clone(),
        };
        browser.assert_active(server).await;
        browser
    }

    pub async fn confirmation(&mut self, server: &AuthServer) -> String {
        let response = server
            .http
            .get(format!("{}/logout", server.auth_base))
            .unwrap()
            .header("cookie", self.cookies.header())
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
        self.cookies.absorb(&response);
        let html = response.text().await.unwrap();
        assert!(html.contains("action=\"/logout\""));
        assert!(html.contains(&format!("name=\"csrf\" value=\"{csrf}\"")));
        csrf
    }

    pub async fn logout(&self, server: &AuthServer, csrf: &str) -> cyper::Response {
        post(server, "/logout", &self.cookies.header(), &[("csrf", csrf)]).await
    }

    pub async fn assert_active(&self, server: &AuthServer) {
        let session = sessions::validate(&server.pg, self.id)
            .await
            .unwrap()
            .expect("the browser session must remain usable");
        assert_eq!(session.user_id, self.user_id);
        let response = server
            .http
            .get(format!("{}/me", server.auth_base))
            .unwrap()
            .header("cookie", self.cookies.header())
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
    }

    pub async fn assert_revoked(&self, server: &AuthServer) {
        assert!(
            sessions::validate(&server.pg, self.id)
                .await
                .unwrap()
                .is_none()
        );
        // Reuse the original cookie to prove server-side revocation, even if a
        // client ignores the browser-cookie deletion in the logout response.
        let response = server
            .http
            .get(format!("{}/me", server.auth_base))
            .unwrap()
            .header("cookie", self.cookies.header())
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 302);
        assert_eq!(common::location(&response), "/login");
    }
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

pub(super) fn assert_logged_out(response: &cyper::Response) {
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(common::location(response), "/login");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        common::read_set_cookie(response, "__Host-zsidp_session").as_deref(),
        Some("")
    );
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|value| value.starts_with("__Host-zsidp_session="))
        .unwrap();
    assert!(cookie.split(';').any(|part| part.trim() == "Max-Age=0"));
}
