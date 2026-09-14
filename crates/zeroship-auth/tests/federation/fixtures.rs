//! Federation flows through owned provider and auth HTTP listeners.

#![allow(clippy::future_not_send)]

use std::{io::Write, net::TcpListener};
use url::Url;
use zeroship_auth::store::{identities, sessions, users};
use zeroship_core::UserId;

use super::provider::{CLIENT_ID, CLIENT_SECRET, Provider, ProviderServer, User};
use crate::common::{self, auth_server::AuthServer, database::Database};

pub(super) struct Fixture {
    pub server: AuthServer,
    pub provider: ProviderServer,
}

impl Fixture {
    pub async fn new(database: &Database, kind: Provider, user: User) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let auth_base = format!("http://{}", listener.local_addr().unwrap());
        let provider = ProviderServer::start(kind, user).await;
        let callback = format!("{auth_base}/oauth/{}/callback", kind.name());
        let mut secret = tempfile::NamedTempFile::new().unwrap();
        secret.write_all(CLIENT_SECRET.as_bytes()).unwrap();
        let secret_path = secret.path().to_str().unwrap();
        let authorize = provider.url("/authorize");
        let token = provider.url("/token");
        let jwks = provider.url("/jwks");
        let profile = provider.url("/user");
        let emails = provider.url("/emails");
        let extra = match kind {
            Provider::Google => vec![
                "--google-client-id",
                CLIENT_ID,
                "--google-client-secret-file",
                secret_path,
                "--google-redirect-uri",
                &callback,
                "--google-auth-url",
                &authorize,
                "--google-token-url",
                &token,
                "--google-jwks-url",
                &jwks,
                "--google-issuer",
                provider.base(),
            ],
            Provider::GitHub => vec![
                "--github-client-id",
                CLIENT_ID,
                "--github-client-secret-file",
                secret_path,
                "--github-redirect-uri",
                &callback,
                "--github-authorize-url",
                &authorize,
                "--github-token-url",
                &token,
                "--github-user-url",
                &profile,
                "--github-emails-url",
                &emails,
            ],
        };
        let server = AuthServer::with_listener(database, listener, &extra).await;
        assert_eq!(server.auth_base, auth_base);
        Self { server, provider }
    }

    pub async fn begin(&self) -> Attempt {
        let kind = self.provider.kind();
        let return_to = format!("{}&idp_hint={}", AuthServer::fresh_challenge(), kind.name());
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("return_to", &format!("{return_to}&prompt=login"))
            .finish();
        let start = self
            .server
            .http
            .get(format!(
                "{}/oauth/{}/start?{query}",
                self.server.auth_base,
                kind.name()
            ))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(start.status().as_u16(), 302);
        let stash = common::read_set_cookie(&start, kind.stash_cookie()).unwrap();
        assert!(!stash.is_empty());
        let authorize = Url::parse(&common::location(&start)).unwrap();
        assert_eq!(
            authorize.origin(),
            Url::parse(self.provider.base()).unwrap().origin()
        );
        assert_eq!(authorize.path(), "/authorize");
        assert_eq!(param(&authorize, "client_id"), CLIENT_ID);
        assert_eq!(param(&authorize, "response_type"), "code");
        assert_eq!(param(&authorize, "code_challenge_method"), "S256");
        assert!(!param(&authorize, "code_challenge").is_empty());
        assert_eq!(param(&authorize, "prompt"), "login");
        match kind {
            Provider::Google => {
                assert_eq!(param(&authorize, "max_age"), "0");
                assert!(!param(&authorize, "nonce").is_empty());
            }
            Provider::GitHub => assert!(!authorize.query_pairs().any(|(key, _)| key == "max_age")),
        }
        let redirect = Url::parse(&param(&authorize, "redirect_uri")).unwrap();
        assert_eq!(
            redirect.as_str(),
            format!("{}/oauth/{}/callback", self.server.auth_base, kind.name())
        );
        let upstream = self
            .server
            .http
            .get(authorize.as_str())
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(upstream.status().as_u16(), 302);
        let callback = Url::parse(&common::location(&upstream)).unwrap();
        assert_eq!(callback.origin(), redirect.origin());
        assert_eq!(callback.path(), redirect.path());
        assert_eq!(param(&callback, "state"), param(&authorize, "state"));
        assert!(!param(&callback, "code").is_empty());
        Attempt {
            callback,
            stash,
            return_to,
        }
    }

    pub async fn assert_refused(&self, response: &cyper::Response, reasons: &[&str]) {
        assert_eq!(response.status().as_u16(), 200);
        assert!(common::read_set_cookie(response, "__Host-zsidp_session").is_none());
        self.assert_stash_cleared(response);
        let rows = self
            .server
            .pg
            .query(
                "SELECT detail->>'reason' FROM zeroship.audit_events \
             WHERE event_type = 'oauth_callback_failure' AND auth_method = $1 ORDER BY id",
                &[&self.provider.kind().name()],
            )
            .await
            .expect("callback refusal records an audit event");
        let observed: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        assert_eq!(observed, reasons);
    }

    pub fn assert_stash_cleared(&self, response: &cyper::Response) {
        assert_eq!(
            common::read_set_cookie(response, self.provider.kind().stash_cookie()).as_deref(),
            Some("")
        );
    }

    pub async fn assert_counts(&self, users: i64, identities: i64, sessions: i64) {
        let row = self
            .server
            .pg
            .query_one(
                "SELECT (SELECT COUNT(*) FROM zeroship.users), \
                    (SELECT COUNT(*) FROM zeroship.federated_identities), \
                    (SELECT COUNT(*) FROM zeroship.idp_sessions)",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            (
                row.get::<_, i64>(0),
                row.get::<_, i64>(1),
                row.get::<_, i64>(2)
            ),
            (users, identities, sessions),
            "all persisted users, identities and sessions in this case"
        );
    }

    pub async fn assert_session(
        &self,
        response: &cyper::Response,
        attempt: &Attempt,
        amr: &[&str],
    ) -> UserId {
        assert_eq!(response.status().as_u16(), 303);
        assert_eq!(common::location(response), attempt.return_to);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let cookie = common::read_set_cookie(response, "__Host-zsidp_session")
            .expect("callback supplies a session cookie");
        let session = sessions::validate(&self.server.pg, cookie.parse().unwrap())
            .await
            .unwrap()
            .expect("returned cookie names a usable session");
        let kind = self.provider.kind();
        assert_eq!(session.auth_method, kind.name());
        assert_eq!(session.amr, amr);
        assert_eq!(session.acr, Some(format!("urn:zeroship:{}", kind.name())));
        let profile = self.provider.user();
        let user = users::find_by_email(&self.server.orm, &profile.email)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.user_id, user.id);
        let linked = identities::list_for_user(&self.server.pg, &user.id)
            .await
            .unwrap();
        let [identity] = linked.as_slice() else {
            panic!("expected the provider identity: {linked:?}")
        };
        assert_eq!(identity.provider, kind.name());
        assert_eq!(identity.subject, profile.subject);
        assert_eq!(
            identity.email_at_link.as_deref(),
            Some(profile.email.as_str())
        );
        user.id
    }

    pub async fn assert_created_profile(&self) {
        let profile = self.provider.user();
        let user = users::find_by_email(&self.server.orm, &profile.email)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.name, profile.name);
        assert_eq!(user.avatar_url.as_deref(), Some(profile.picture.as_str()));
        assert!(user.email_verified_at.is_some());
        assert!(user.password_hash.is_none());
        self.assert_counts(1, 1, 1).await;
    }
}

pub(super) struct Attempt {
    pub callback: Url,
    pub stash: String,
    pub return_to: String,
}

impl Attempt {
    pub async fn complete(&self, fixture: &Fixture) -> cyper::Response {
        self.send(fixture, &self.callback, Some(&self.stash)).await
    }

    pub async fn send(
        &self,
        fixture: &Fixture,
        callback: &Url,
        stash: Option<&str>,
    ) -> cyper::Response {
        let mut request = fixture.server.http.get(callback.as_str()).unwrap();
        if let Some(stash) = stash {
            request = request
                .header(
                    "cookie",
                    format!("{}={stash}", fixture.provider.kind().stash_cookie()),
                )
                .unwrap();
        }
        request.send().await.unwrap()
    }
}

fn param(url: &Url, key: &str) -> String {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .unwrap_or_else(|| panic!("missing {key} in {url}"))
        .1
        .into_owned()
}

pub(super) async fn confirm_password(
    fixture: &Fixture,
    response: &cyper::Response,
    password: &str,
) -> cyper::Response {
    assert_eq!(response.status().as_u16(), 302);
    assert!(common::read_set_cookie(response, "__Host-zsidp_session").is_none());
    fixture.assert_stash_cleared(response);
    let link = Url::parse(&fixture.server.auth_base)
        .unwrap()
        .join(&common::location(response))
        .unwrap();
    assert_eq!(link.path(), "/link");
    let token = param(&link, "token");
    let form = fixture
        .server
        .http
        .get(link.as_str())
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(form.status().as_u16(), 200);
    let csrf = common::read_set_cookie(&form, "__Host-zsidp_csrf").unwrap();
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &token)
        .append_pair("password", password)
        .finish();
    fixture
        .server
        .http
        .request(
            http::Method::POST,
            format!("{}/link", fixture.server.auth_base),
        )
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap()
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .unwrap()
        .body(body)
        .send()
        .await
        .unwrap()
}
