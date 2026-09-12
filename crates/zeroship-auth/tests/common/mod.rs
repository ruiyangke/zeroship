//! Private fixtures and HTTP utilities for the auth integration target.

#![allow(dead_code)]

pub mod auth_server;
pub mod database;
pub mod mock_control;
pub mod mock_provider;

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

/// A stable process-specific signing key for fixtures using the shared database.
///
/// Distinct processes need distinct issuers so a peer cannot retire a key before
/// its owner publishes it. Memoization keeps the key stable across this process's
/// fixtures; key-retention tests construct their own issuers in owned databases.
pub fn op_signing_key() -> ed25519_dalek::SigningKey {
    static SEED: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let seed = SEED.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // The pid separates two live processes; the clock separates a reused
        // pid from the process that held it before. The crate name is kept so
        // a kid in a log still names the binary that minted it.
        zeroship_core::crypto::derive_key(&format!(
            "{}-{}-{nanos}",
            env!("CARGO_CRATE_NAME"),
            std::process::id(),
        ))
    });
    ed25519_dalek::SigningKey::from_bytes(seed)
}

// ─── AuthConfig test fixture ─────────────────────────────────────────────
//
// Every test that boots an in-process auth server needs an `AuthConfig`.
// Building one as a struct literal means re-listing 30+ fields verbatim,
// and every new field added in a future phase forces a fixture-sync
// commit across every test file. Driving the same `clap::Parser::parse_from`
// path the CLI uses lets unset fields take their declared defaults
// automatically — new fields land with their defaults, no fixture churn.
//
// `test_auth_config` bakes in the overrides every fixture needs:
// random bind port and explicit test-only secret inputs. Federation-specific
// tests build on the returned config by
// mutating the OAuth fields directly (cheaper than parsing again with
// 8 more CLI args).
/// The console origin the test fixture admits via `frame-ancestors` on the
/// framed login routes (immersive iframe login, design §4.3). The rewritten
/// clickjacking test reads this from the booted config rather than hard-coding
/// it, exercising the route-aware security headers against the live `/login`.
#[allow(dead_code)]
pub const TEST_CONSOLE_ORIGIN: &str = "http://localhost:5173";

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
    let mut args: Vec<&str> = Vec::from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        // Admit the console origin so the framed login routes (/login, /signup,
        // /consent) emit the relaxed `frame-ancestors` — the rewritten threat
        // model test pins this NEW contract.
        "--frame-ancestor-origins",
        TEST_CONSOLE_ORIGIN,
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://localhost:0",
    ]);
    args.extend_from_slice(extra);
    // `parse_from` runs the generated resolver, exactly as real boot does.
    let mut cfg = AuthConfig::parse_from(args);
    cfg.settings.database_url = test_secret(db_url);
    cfg.settings.stash_signing_key = test_secret("test-stash-key-not-for-prod-32bytes!");
    cfg.settings.totp_enc_key =
        test_secret("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
    // The session-secret keyring, which `main.rs` REFUSES TO BOOT WITHOUT.
    // Every fixture needs it now, because every token exchange establishes a
    // session and a session's secret is hashed under this keyring - where
    // before, only an exchange that issued a refresh token reached it, so a
    // fixture could omit it and still serve tokens.
    //
    // That gap is what a device-grant test found: it configured no keyring
    // (it never asked for `offline_access`), and the exchange answered 500.
    // The fixture was simply less configured than any real deployment.
    //
    // The pair comes from `tests/fixtures/session_keys.rs` rather than from a local
    // helper, because the control plane's `PlatformOp` fixture builds its own
    // `AuthConfig` the same way and MISSED this when it was added here. One
    // function is what stops the two drifting again, and the fixtures that
    // used to write a second pair of their own on top of this one no longer
    // do - a duplicate of this operation is precisely how the control plane's
    // copy came to be missing. `tests/session_keyring_fixture_gate.sh` rules
    // on that: a fixture that mounts the auth server and drives a token
    // exchange must have a keyring, whatever route it takes to one.
    //
    // `cli_device_refresh_test.rs` is the one fixture that still writes its
    // own pair after calling this. It is correct - it configures both fields
    // - so the gate passes it; it is simply not consolidated.
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
///
/// A test whose subject IS the seat - `consent_ui_test`, which asserts on the
/// consenting user's ROLE - writes the `organization_members` row itself,
/// joining through `apps.project_id -> projects.organization_id`. That is the
/// replacement for the deleted `zeroship.app_members` row, and the reason this
/// helper deliberately seats nobody.
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

/// Publish this process's key for fixtures using the configured shared database.
///
/// Only successful publication sets the process flag. Owned database fixtures
/// call the issuer directly because this flag does not identify a database.
pub async fn publish_op_key_once(
    issuer: &zeroship_auth::oidc::Issuer,
    db: &compio_postgres::Client,
) -> zeroship_auth::error::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PUBLISHED: AtomicBool = AtomicBool::new(false);
    if PUBLISHED.load(Ordering::SeqCst) {
        return Ok(());
    }
    issuer.publish_active_key(db).await?;
    PUBLISHED.store(true, Ordering::SeqCst);
    Ok(())
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
/// The returned uuid is the person the proof names; a mint must use it as its
/// subject, because `Issuer` refuses a mint whose subject is not the validated
/// session's person.
pub async fn validated_session(
    pg: &compio_postgres::Client,
    label: &str,
) -> (zeroship_auth::session_store::ValidatedSession, uuid::Uuid) {
    use std::io::Write as _;

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let person_id: uuid::Uuid = pg
        .query_one(
            "INSERT INTO zeroship.users (email, name) VALUES ($1::citext, $2) RETURNING id",
            &[&format!("{label}-{tag}@zeroship.test"), &"Witness Fixture"],
        )
        .await
        .expect("seed person")
        .get("id");

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
    let subject = person_id.to_string();
    let grant_id = zeroship_auth::session_store::upsert_grant(
        pg,
        person_id,
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
            person_id,
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
