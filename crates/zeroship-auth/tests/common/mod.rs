//! Private fixtures and HTTP utilities for the auth integration target.

#![allow(dead_code)]

pub mod auth_server;
pub mod database;
pub mod mock_control;

use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_core::config::{Secret, SourceKind};

/// Require the configured platform database for fixtures that do not yet own one.
///
/// Missing or unmigrated databases fail the run. The connection preflight is
/// memoized for these shared-database callers; owned fixtures use [`database`]
/// instead and manage their own server lifecycle.
#[must_use]
pub fn test_database_url() -> String {
    static DSN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DSN.get_or_init(|| {
        platform_fixture::live_db::require_configured(
            zeroship_core::config::test_database_url_opt(),
            platform_fixture::live_db::PLATFORM_SCHEMAS,
        )
    })
    .clone()
}

// ─── AuthConfig test fixture ─────────────────────────────────────────────
//
// Build AuthConfig through the CLI resolver so unset fields retain their
// declared defaults. Fixtures provide an ephemeral bind address and explicit
// secret inputs. Federation scenarios pass provider settings as CLI flags and
// resolve provider credentials from a file owned during server construction.
#[must_use]
pub fn test_auth_config(db_url: &str) -> AuthConfig {
    test_auth_config_with(db_url, &[])
}

/// [`test_auth_config`] plus EXTRA command-line arguments.
///
/// Extra settings arrive as flags rather than as field writes, because a
/// resolved `Operational<T>` has no setter: a test that reached past the
/// resolver would be configuring a shape production never produces.
///
/// Secrets are the deliberate exception. A `Secret<T>` has NO value flag by
/// construction - the whole point of the conversion is that credential material
/// never reaches an argument vector - so a fixture can supply one only through a
/// file, an environment variable, or the in-memory shape both of those resolve
/// to. Files would need per-test temp-dir custody and the environment is
/// process-global (a sibling test running concurrently would see it), so the
/// fixture writes the resolved shape directly: `SourceKind::Env` with material,
/// which is byte-for-byte what `ZEROSHIP_AUTH_STASH_SIGNING_KEY=<literal>`
/// produces.
#[allow(dead_code)]
pub fn test_auth_config_with(db_url: &str, extra: &[&str]) -> AuthConfig {
    test_auth_config_at(db_url, "http://localhost:0", extra)
}

/// Resolve server settings against the address owned by the HTTP fixture.
pub fn test_auth_config_at(db_url: &str, public_url: &str, extra: &[&str]) -> AuthConfig {
    // The console uses another origin on the auth host so the production
    // same-site filter admits it when the fixture binds an ephemeral address.
    let mut console_origin = url::Url::parse(public_url).expect("fixture public URL");
    console_origin
        .set_port(Some(5173))
        .expect("fixture console port");
    let console_origin = console_origin.origin().ascii_serialization();
    let mut args: Vec<&str> = Vec::from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        // Admit the console origin so the framed login routes (/login, /signup,
        // /consent) emit the relaxed `frame-ancestors` — the rewritten threat
        // model test pins this NEW contract.
        "--frame-ancestor-origins",
        &console_origin,
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        public_url,
    ]);
    args.extend_from_slice(extra);
    // `parse_from` runs the generated resolver, exactly as real boot does.
    let mut cfg = AuthConfig::parse_from(args);
    cfg.settings.database_url = test_secret(db_url);
    cfg.settings.stash_signing_key = test_secret("test-stash-key-not-for-prod-32bytes!");
    cfg.settings.totp_enc_key =
        test_secret("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
    // Token exchange reads the key files through the resolved AuthConfig.
    // These paths still come from the shared fixture; owning their lifetime
    // alongside each server is part of the remaining fixture conversion.
    let (hash_file, idem_file) = session_keys::session_key_files();
    cfg.settings.refresh_hash_key_file = zeroship_core::config::Operational::new(hash_file);
    cfg.settings.refresh_idem_key_file = zeroship_core::config::Operational::new(idem_file);
    cfg
}

/// A secret in the shape an in-memory literal resolves to. See
/// [`test_auth_config_with`] for why a fixture supplies secrets this way.
#[allow(dead_code)]
#[must_use]
pub fn test_secret(material: &str) -> Secret<String> {
    Secret::supplied(SourceKind::Env, Some(material.to_owned()))
}

#[allow(clippy::future_not_send)]
pub async fn dedicated_test_db(db_url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(db_url, compio_postgres::NoTls)
        .await
        .expect("connect dedicated test database session");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("dedicated test database connection error: {e}");
        }
    })
    .detach();
    client
}

// ─── PKCE ────────────────────────────────────────────────────────────────
//
// Canonical implementations live in `zeroship_core::pkce` so the gateway
// OIDC RP module and these integration tests share one source. These
// thin wrappers exist purely to preserve the historical names used
// throughout the auth test suite (`pkce_verifier`, `pkce_challenge_s256`).

/// See [`zeroship_core::pkce::generate_verifier`].
pub fn pkce_verifier() -> String {
    zeroship_core::pkce::generate_verifier()
}

/// See [`zeroship_core::pkce::s256_challenge`].
pub fn pkce_challenge_s256(verifier: &str) -> String {
    zeroship_core::pkce::s256_challenge(verifier)
}

// ─── HTTP helpers ────────────────────────────────────────────────────────

pub fn extract_query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Read the `value` field of the first matching `Set-Cookie: <name>=<value>; ...`
/// header.
pub fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if let Some((n, v)) = first.split_once('=') {
            if n.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

pub fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub fn assert_redirect(resp: &cyper::Response, what: &str) {
    let s = resp.status().as_u16();
    assert!(
        (300..400).contains(&s),
        "{what}: expected 3xx redirect, got {s}"
    );
}

// ─── Cookie jar ──────────────────────────────────────────────────────────

/// Minimal cookie jar: `name → value`. Ignores Domain/Path/Expires; the test
/// flows only hit the auth-test host and never overlap cookie names that matter.
#[derive(Default)]
pub struct CookieJar {
    inner: std::collections::HashMap<String, String>,
}

impl CookieJar {
    /// Absorb every `Set-Cookie` header from a cyper response.
    pub fn absorb(&mut self, resp: &cyper::Response) {
        for hv in resp.headers().get_all(http::header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            // `name=value; ...attrs`. We only care about `name=value`.
            let first = s.split(';').next().unwrap_or("");
            if let Some((name, value)) = first.split_once('=') {
                let name = name.trim();
                let value = value.trim();
                if name.is_empty() {
                    continue;
                }
                // Empty value = browser-style deletion; honour it.
                if value.is_empty() {
                    self.inner.remove(name);
                } else {
                    self.inner.insert(name.to_string(), value.to_string());
                }
            }
        }
    }

    pub fn set(&mut self, name: &str, value: &str) {
        self.inner.insert(name.to_string(), value.to_string());
    }

    /// Serialize to a `Cookie:` header value (`a=1; b=2`).
    pub fn header(&self) -> String {
        let mut parts: Vec<String> = self.inner.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.sort();
        parts.join("; ")
    }
}

// ─── Native authorization request helpers ────────────────────────────────

pub fn native_authorize_return_to(client_id: &str, redirect_uri: &str) -> String {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &format!("st-{}", Uuid::new_v4().simple()))
        .append_pair("nonce", &format!("nc-{}", Uuid::new_v4().simple()))
        // 43-char base64url SHA-256 placeholder with the right S256 shape.
        .append_pair(
            "code_challenge",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .append_pair("code_challenge_method", "S256")
        .finish();
    format!("/oauth2/authorize?{q}")
}

// ─── App ownership chain ─────────────────────────────────────────────────

/// The project a fixture's `zeroship.apps` row belongs to.
///
/// `apps.project_id` is NOT NULL against a RESTRICT foreign key and a project
/// needs an organization, so an app row can no longer be seeded on its own.
/// This mints an organization with NO members and a project inside it, which is
/// the right home for every app in this crate's tests: they are about OIDC,
/// sessions, consent and account lifecycle, and an app here needs a place to
/// exist rather than a creator.
pub async fn unowned_project(pg: &compio_postgres::Client) -> String {
    let organization_id = zeroship_core::typed_id::generate("org");
    let project_id = zeroship_core::typed_id::generate("prj");
    pg.execute(
        "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
         VALUES ($1, $2, 'Auth Fixture Organization', 'fixture@zeroship.test')",
        &[
            &organization_id,
            &format!("auth-fixture-{}", Uuid::new_v4().simple()),
        ],
    )
    .await
    .expect("seed fixture organization");
    pg.execute(
        "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
         VALUES ($1, $2, 'default', 'Default')",
        &[&project_id, &organization_id],
    )
    .await
    .expect("seed fixture project");
    project_id
}

// ─── DB cleanup ──────────────────────────────────────────────────────────

/// Delete sessions + user row for `email`. CITEXT columns require an explicit
/// `text→citext` cast for the bind (compio-postgres binds `&str` as TEXT; PG
/// won't auto-cast in a WHERE). Sessions are deleted first to avoid tripping
/// the FK from `zeroship.idp_sessions.user_id`. Errors are swallowed (best-effort).
pub async fn cleanup_user(pg: &compio_postgres::Client, email: &str) {
    let _ = pg
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id IN \
             (SELECT id FROM zeroship.users WHERE email = $1::citext)",
            &[&email],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        )
        .await;
}

/// Delete every rate-limit row whose `bucket_key` matches any of the given
/// `LIKE` patterns. Best-effort — errors are swallowed.
pub async fn cleanup_rate_limits_like(pg: &compio_postgres::Client, patterns: &[&str]) {
    for pat in patterns {
        let _ = pg
            .execute(
                "DELETE FROM zeroship.rate_limits WHERE bucket_key LIKE $1",
                &[pat],
            )
            .await;
    }
}

/// A `Mailer` that keeps every message instead of transporting it.
///
/// Any handler that mails now needs `State<Arc<dyn Mailer>>` registered, so a
/// fixture standing one of those routes up on a bare `web::App` has to supply
/// SOMETHING. This is that something, and it is deliberately not a silent sink:
/// a test asserting a notice was sent needs to read the rendered message, and a
/// test asserting one was NOT sent needs the same object to be empty. It does
/// not consult `zeroship.email_suppressions` - suppression is the transport's
/// job and no test here is about it.
#[derive(Debug, Default)]
pub struct CapturingMailer {
    sent: std::sync::Mutex<Vec<zeroship_mailer::Email>>,
}

impl CapturingMailer {
    pub fn sent(&self) -> Vec<zeroship_mailer::Email> {
        self.sent.lock().expect("capturing mailer lock").clone()
    }
}

#[async_trait::async_trait]
impl zeroship_mailer::Mailer for CapturingMailer {
    async fn send(
        &self,
        _db: &compio_postgres::Client,
        msg: zeroship_mailer::Email,
    ) -> Result<zeroship_mailer::MessageId, zeroship_mailer::MailerError> {
        let n = {
            let mut sent = self.sent.lock().expect("capturing mailer lock");
            sent.push(msg);
            sent.len()
        };
        Ok(zeroship_mailer::MessageId(format!("test-message-{n}")))
    }
}

/// A person, a platform grant and a session, and the proof the creating
/// statement produced.
///
/// Every `Issuer::issue_*` mint takes a `ValidatedSession`, and the type has no
/// constructor outside `zeroship_auth::session_store` - so a test that wants to
/// mint has to establish a real session, exactly as production does. That is
/// the point of the witness: a fixture cannot fabricate one, and a test that
/// could would be testing nothing.
///
/// The returned user id is the person the proof names; a mint must use it as its
/// subject, because `Issuer` refuses a mint whose subject is not the validated
/// session's person.
pub async fn validated_session(
    pg: &compio_postgres::Client,
    label: &str,
) -> (
    zeroship_auth::session_store::ValidatedSession,
    zeroship_core::UserId,
) {
    use std::io::Write as _;

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let person_id = zeroship_core::UserId::mint();
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
        &[
            &person_id.as_str(),
            &format!("{label}-{tag}@zeroship.test"),
            &"Witness Fixture",
        ],
    )
    .await
    .expect("seed person");

    let dir = tempfile::tempdir().expect("private witness keys");
    let hash_path = dir.path().join("hash");
    let idem_path = dir.path().join("idem");
    for (path, body) in [
        (
            &hash_path,
            "1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\n".as_bytes(),
        ),
        (&idem_path, "witness-fixture-idempotency-secret".as_bytes()),
    ] {
        let mut file = std::fs::File::create(path).expect("create key file");
        file.write_all(body).expect("write key file");
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod key file");
        }
    }
    let keys = zeroship_auth::session_store::SessionSecretKeys::from_files(&hash_path, &idem_path)
        .expect("load session keys");

    let scopes = vec!["openid".to_string()];
    let subject = person_id.as_str().to_owned();
    let grant_id = zeroship_auth::session_store::upsert_grant(
        pg,
        &person_id,
        &zeroship_auth::session_store::Audience::Platform,
        &subject,
        &scopes,
        None,
    )
    .await
    .expect("seed grant");
    let created = zeroship_auth::session_store::create(
        pg,
        &keys,
        &zeroship_auth::session_store::NewSession {
            person_id: &person_id,
            grant_id: &grant_id,
            subject: &subject,
            grant_scopes: &scopes,
            parent_session_id: None,
            kind: zeroship_auth::session_store::SessionKind::Cli,
            scopes: &scopes,
            amr: &[],
            acr: None,
            label: None,
            expected_credential_epoch: None,
            idle_days: 7,
            absolute_days: 30,
            with_secret: false,
        },
    )
    .await
    .expect("create session")
    .expect("session created");
    (created.proof, person_id)
}

#[path = "../../../../tests/fixtures/platform_db/mod.rs"]
mod platform_fixture;

#[path = "../../../../tests/fixtures/session_keys.rs"]
mod session_keys;
