//! Federation flows through owned provider and auth HTTP listeners.

#![allow(clippy::future_not_send)]

use std::{io::Write, net::TcpListener};
use url::Url;
use zeroship_auth::store::{identities, sessions, users};
use zeroship_core::UserId;

use super::provider::{CLIENT_ID, CLIENT_SECRET, Provider, ProviderServer, User};
use crate::support::{self, auth_server::AuthServer, database::Database};

pub(super) struct Fixture {
    pub server: AuthServer,
    pub provider: ProviderServer,
    /// The case's own forwarded client IP, so its rate-limit buckets and audit
    /// rows never collide with another case sharing this database.
    ip: String,
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
        Self {
            server,
            provider,
            ip: fixture_ip(),
        }
    }

    pub fn client_ip(&self) -> &str {
        &self.ip
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
            .header("x-forwarded-for", self.client_ip())
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(start.status().as_u16(), 302);
        let stash = support::read_set_cookie(&start, kind.stash_cookie()).unwrap();
        assert!(!stash.is_empty());
        let authorize = Url::parse(&support::location(&start)).unwrap();
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
        let callback = Url::parse(&support::location(&upstream)).unwrap();
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
        assert!(support::read_set_cookie(response, "__Host-zsidp_session").is_none());
        self.assert_stash_cleared(response);
        let rows = self
            .server
            .pg
            .query(
                "SELECT detail->>'reason' FROM zeroship.audit_events \
             WHERE event_type = 'oauth_callback_failure' AND auth_method = $1 \
               AND ip = $2 ORDER BY id",
                &[
                    &self.provider.kind().name(),
                    &self.client_ip().parse::<std::net::IpAddr>().unwrap(),
                ],
            )
            .await
            .expect("callback refusal records an audit event");
        let observed: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        assert_eq!(observed, reasons);
    }

    pub fn assert_stash_cleared(&self, response: &cyper::Response) {
        assert_eq!(
            support::read_set_cookie(response, self.provider.kind().stash_cookie()).as_deref(),
            Some("")
        );
    }

    pub async fn assert_counts(&self, users: i64, identities: i64, sessions: i64) {
        let profile = self.provider.user();
        let row = self
            .server
            .pg
            .query_one(
                "SELECT \
                    (SELECT COUNT(*) FROM zeroship.users WHERE email = $1::citext), \
                    (SELECT COUNT(*) FROM zeroship.federated_identities \
                        WHERE provider = $2 AND subject = $3), \
                    (SELECT COUNT(*) FROM zeroship.idp_sessions s \
                        JOIN zeroship.users u ON u.id = s.user_id \
                        WHERE u.email = $1::citext)",
                &[&profile.email, &self.provider.kind().name(), &profile.subject],
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
            "users, identities and sessions this case minted"
        );
    }

    pub async fn assert_session(
        &self,
        response: &cyper::Response,
        attempt: &Attempt,
        amr: &[&str],
    ) -> UserId {
        assert_eq!(response.status().as_u16(), 303);
        assert_eq!(support::location(response), attempt.return_to);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let cookie = support::read_set_cookie(response, "__Host-zsidp_session")
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
        request = request
            .header("x-forwarded-for", fixture.client_ip())
            .unwrap();
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

/// A per-case email whose local part carries a fresh tag, so cases that share a
/// database never act on one another's user, link or completion rows.
pub(super) fn email(label: &str, domain: &str) -> String {
    format!("{label}-{}@{domain}", uuid::Uuid::new_v4().simple())
}

/// A per-case forwarded client IP, keeping rate-limit buckets and audit rows
/// scoped to the case that minted it.
pub(super) fn fixture_ip() -> String {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    format!("10.{}.{}.{}", bytes[0], bytes[1], bytes[2])
}

pub(super) async fn confirm_password(
    fixture: &Fixture,
    response: &cyper::Response,
    password: &str,
) -> cyper::Response {
    assert_eq!(response.status().as_u16(), 302);
    assert!(support::read_set_cookie(response, "__Host-zsidp_session").is_none());
    fixture.assert_stash_cleared(response);
    let link = Url::parse(&fixture.server.auth_base)
        .unwrap()
        .join(&support::location(response))
        .unwrap();
    assert_eq!(link.path(), "/link");
    let token = param(&link, "token");
    let form = fixture
        .server
        .http
        .get(link.as_str())
        .unwrap()
        .header("x-forwarded-for", fixture.client_ip())
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(form.status().as_u16(), 200);
    let csrf = support::read_set_cookie(&form, "__Host-zsidp_csrf").unwrap();
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
        .header("x-forwarded-for", fixture.client_ip())
        .unwrap()
        .body(body)
        .send()
        .await
        .unwrap()
}
