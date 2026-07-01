//! Gateway OIDC RP end-to-end test against live hydra + crates/auth.
//!
//! Boots the auth server in-process on a random port (same pattern as
//! `crates/auth/tests/e2e_password.rs`), registers a per-test creator-app
//! OIDC client with hydra, then drives the full authorization-code + PKCE
//! dance using the gateway's `OidcRp` directly (no live gateway HTTP
//! server). Asserts:
//!
//!   - `OidcRp::build_authorize_redirect` produces a `/oauth2/auth` URL
//!     with `client_id`, `response_type=code`, `code_challenge_method=S256`,
//!     and the per-call `state` + `nonce`.
//!   - Hydra's `/oauth2/auth` 302s to `/login?login_challenge=...` on the
//!     auth server; submitting the login form drives `accept_login` and
//!     hydra's continuation 302s carry us through `/consent` (skip path)
//!     to a callback URL containing `code` + `state`.
//!   - `OidcRp::finish_callback` exchanges the code, verifies the ID
//!     token (signature, `iss`, `aud`, `nonce`), and returns the claims
//!     for the user we seeded; the `original_path` round-trips.
//!   - `gateway::sessions::{create, validate, revoke}` against those
//!     claims behaves exactly as in `sessions_test.rs`, but driven from
//!     a real ID-token round-trip rather than synthetic args.
//!
//! Skipped silently when `AUTH_DB_URL` or `AUTH_HYDRA_ADMIN` is unset
//! (same convention as `crates/auth/tests/e2e_password.rs`).
//!
//! ─── Issuer URL gotcha ─────────────────────────────────────────────────
//!
//! Hydra is configured (per `ops/hydra.yaml::urls.self.issuer`) to emit
//! `iss: https://auth.zeroship.ai/` regardless of which interface a token
//! request came in on. The gateway's `OidcRp` dials hydra at
//! `http://127.0.0.1:4444` (loopback admin/public) during tests, so the
//! OIDC dial URL and the expected `iss` differ. We use
//! `OidcRp::with_issuer` (added alongside this test) to override the
//! verifier's expected issuer to the canonical
//! `https://auth.zeroship.ai/`.
//!
//! ─── Cookie jar ────────────────────────────────────────────────────────
//!
//! Hydra emits Set-Cookie with Domain=auth.zeroship.ai; our cyper client
//! lives outside any browser so we hand-roll a minimal jar (`name → value`)
//! and replay every cookie back on every hop. The same approach
//! `crates/auth/tests/common/mod.rs::CookieJar` uses — duplicated here
//! because integration tests in `crates/gateway/tests/` can't reach into
//! the auth crate's test-only `common` module.

use std::sync::Arc;
use std::time::Duration;

use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;

use zeroship_gateway::oidc_rp::OidcRp;
use zeroship_gateway::sessions::{create, revoke_app_sessions_for_user, validate, NewSession};

// ─── Helpers (duplicated from crates/auth/tests/common/mod.rs) ──────────

fn extract_query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Rewrite any URL whose origin is the configured Hydra `issuer_base` to the
/// loopback `dial_base` the test can actually reach.
///
/// Hydra mints `redirect_to` URLs under its configured issuer host
/// (`urls.self.issuer`), which differs by deployment: the prod template uses
/// `https://auth.zeroship.ai`, the compose stack (`ops/hydra-dev.yaml`) uses
/// `http://auth.zeroship.localhost`. The test, however, dials Hydra on a
/// loopback port. Parameterising both ends lets the same test run against
/// either issuer; hard-coding `auth.zeroship.ai → 127.0.0.1:4444` left the
/// compose `redirect_to` unrewritten and the request hung.
fn rewrite_to_hydra_loopback(raw_url: &str, issuer_base: &str, dial_base: &str) -> String {
    let issuer_base = issuer_base.trim_end_matches('/');
    let dial_base = dial_base.trim_end_matches('/');
    if let Some(rest) = raw_url.strip_prefix(issuer_base) {
        return format!("{dial_base}{rest}");
    }
    raw_url.to_string()
}

fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
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

fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn assert_redirect(resp: &cyper::Response, what: &str) {
    let s = resp.status().as_u16();
    assert!(
        (300..400).contains(&s),
        "{what}: expected 3xx redirect, got {s}"
    );
}

#[derive(Default)]
struct CookieJar {
    inner: std::collections::HashMap<String, String>,
}

impl CookieJar {
    fn absorb(&mut self, resp: &cyper::Response) {
        for hv in resp.headers().get_all(http::header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            let first = s.split(';').next().unwrap_or("");
            if let Some((name, value)) = first.split_once('=') {
                let name = name.trim();
                let value = value.trim();
                if name.is_empty() {
                    continue;
                }
                if value.is_empty() {
                    self.inner.remove(name);
                } else {
                    self.inner.insert(name.to_string(), value.to_string());
                }
            }
        }
    }

    fn set(&mut self, name: &str, value: &str) {
        self.inner.insert(name.to_string(), value.to_string());
    }

    fn header(&self) -> String {
        let mut parts: Vec<String> =
            self.inner.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.sort();
        parts.join("; ")
    }
}

// ─── Test ────────────────────────────────────────────────────────────────

#[ntex::test]
async fn gateway_oidc_rp_full_dance() {
    // 0. Env-skip — same convention as crates/auth e2e tests.
    let (Ok(db_url), Ok(hydra_admin_url)) = (
        std::env::var("AUTH_DB_URL"),
        std::env::var("AUTH_HYDRA_ADMIN"),
    ) else {
        eprintln!("[oidc_rp_e2e] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
        return;
    };
    // The loopback base the test can actually dial Hydra on.
    let hydra_public = std::env::var("AUTH_HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());
    // The issuer Hydra mints into `redirect_to`/`iss` (`urls.self.issuer`).
    // Defaults to the prod template's value (with its trailing slash); the
    // compose stack overrides it to `http://auth.zeroship.localhost` (no
    // trailing slash) via AUTH_HYDRA_ISSUER. We use this string *verbatim* as
    // the expected `iss` — the trailing slash is deployment-specific (Hydra
    // echoes `urls.self.issuer` byte-for-byte), so munging it here would break
    // one deployment or the other. The host origin (scheme + host, no path)
    // drives the loopback rewrite below; both differ from `hydra_public` (a
    // loopback dial URL).
    let expected_iss = std::env::var("AUTH_HYDRA_ISSUER")
        .unwrap_or_else(|_| "https://auth.zeroship.ai/".to_string());
    // Origin used for the redirect_to → loopback rewrite (drop any path/slash).
    let issuer_base = expected_iss.trim_end_matches('/').to_string();

    // 1. Boot PG client + run auth migrations.
    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[oidc_rp_e2e] pg connection driver: {e}");
        }
    })
    .detach();
    let pg_client = Arc::new(pg_client);

    // 2. Boot crates/auth in-process on a random port.
    let admin = HydraAdmin::new(&hydra_admin_url);
    // Federation (Google/GitHub) is OFF for this test: the gateway↔auth
    // password-login flow doesn't exercise the `/oauth/{google,github}/*`
    // routes, so we pass `None` client IDs and `configure(false, false)`.
    let cfg = Arc::new(AuthConfig {
        addr: "127.0.0.1:0".to_string(),
        db_url: db_url.clone(),
        hydra_admin_url: Some(hydra_admin_url.clone()),
        allow_remote_hydra_admin: false,
        // Strict fail-closed framing for this RP↔IdP password-flow test (no
        // console overlay origins). Field added when AuthConfig gained
        // frame_ancestor_origins in the immersive-popup CSP work.
        frame_ancestor_origins: vec![],
        hydra_public_url: Some(hydra_public.clone()),
        auth_provider: None,
        supabase_url: None,
        supabase_anon_key: None,
        gotrue_email_hook_secret: None,
        control_url: None,
        control_key: String::new(),
        clients_config: "ops/auth-clients.example.toml".to_string(),
        bootstrap: false,
        dev_insecure: Some(true),
        insecure_dev: true,
        // TOTP routes aren't exercised by this RP↔IdP password-flow test; empty
        // is fine (the `insecure_dev` path substitutes `DEV_TOTP_ENC_KEY`). Field
        // added when AuthConfig gained TOTP 2FA (ISS-11).
        totp_enc_key: String::new(),
        stash_signing_key: "test-stash-key-not-for-prod-32bytes!".to_string(),
        google_client_id: None,
        google_client_secret: None,
        google_redirect_uri: "https://auth.zeroship.ai/oauth/google/callback".to_string(),
        google_auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
        google_token_url: "https://oauth2.googleapis.com/token".to_string(),
        google_jwks_url: "https://www.googleapis.com/oauth2/v3/certs".to_string(),
        google_issuer: "https://accounts.google.com".to_string(),
        github_client_id: None,
        github_client_secret: None,
        github_redirect_uri: "https://auth.zeroship.ai/oauth/github/callback".to_string(),
        github_authorize_url: "https://github.com/login/oauth/authorize".to_string(),
        github_token_url: "https://github.com/login/oauth/access_token".to_string(),
        github_user_url: "https://api.github.com/user".to_string(),
        github_emails_url: "https://api.github.com/user/emails".to_string(),
        mailer: "stdout".to_string(),
        smtp_host: None,
        smtp_port: 587,
        smtp_username: None,
        smtp_password: None,
        smtp_tls: zeroship_mailer::SmtpTls::Starttls,
        resend_api_key: None,
        mail_from_email: "auth@zeroship.ai".to_string(),
        mail_from_name: "zeroship".to_string(),
        public_url: "http://localhost:9092".to_string(),
        auth_signing_key_file: None,
        auth_pairwise_salt_file: None,
        // OP refresh-token families (P5b) + gateway-brokered secret (P5a) — this
        // Hydra-RP e2e doesn't exercise them; defaults keep the literal complete.
        auth_broker_secret_file: None,
        auth_broker_secret_previous_file: None,
        refresh_hash_key_file: None,
        refresh_idem_key_file: None,
        refresh_pool_size: 4,
        postmark_webhook_user: None,
        postmark_webhook_password: None,
        // Relay (Slice 5) — this e2e doesn't exercise the relay path; defaults.
        relay_domain: "relay.zeroship.localhost".to_string(),
        relay_inbound_user: None,
        relay_inbound_password: None,
        relay_forward_mailer: "stdout".to_string(),
        relay_smtp_host: None,
        relay_smtp_port: 587,
        relay_smtp_username: None,
        relay_smtp_password: None,
        relay_smtp_tls: zeroship_mailer::SmtpTls::Plaintext,
        jwk_rotation_days: 90,
        jwk_retain_days: 31,
        cron_tick_secs: 86400,
        audit_retention_check_secs: 3600,
        config_path: None,
        no_config: true,
        check_config: false,
        check_config_format: "text".to_string(),
        obs: Default::default(),
    });
    let admin_state = admin.clone();
    let cfg_state = cfg.clone();
    let db_state = pg_client.clone();
    let srv = web::test::server(move || {
        let admin_state = admin_state.clone();
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        async move {
            web::App::new()
                .state(admin_state)
                .state(cfg_state)
                .state(db_state)
                .middleware(SecurityHeaders::new(vec![]))
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    eprintln!("[oidc_rp_e2e] auth server at {auth_base}");

    // 3. Register a per-test OIDC client representing one creator app.
    let test_client_id = format!("gw-{}", Uuid::new_v4().simple());
    let test_client_secret = "gw-test-secret-do-not-use-in-prod".to_string();
    // The gateway never actually fetches this — the test intercepts the
    // final 302 by reading its Location header.
    let test_redirect = "http://127.0.0.1:9999/__zeroship/auth/callback";
    admin
        .create_client(&OAuth2Client {
            client_id: test_client_id.clone(),
            client_name: Some("gateway oidc_rp e2e".into()),
            client_secret: Some(test_client_secret.clone()),
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            response_types: vec!["code".into()],
            redirect_uris: vec![test_redirect.into()],
            post_logout_redirect_uris: vec![],
            scope: "openid offline_access email profile".into(),
            // OidcRp posts client creds in the form body as
            // `client_id`/`client_secret` (see oidc_rp.rs::finish_callback).
            // That's the `client_secret_post` auth method.
            token_endpoint_auth_method: "client_secret_post".into(),
            subject_type: "public".into(),
            access_token_strategy: None,
            id_token_signed_response_alg: Some("EdDSA".into()),
            audience: vec![],
            skip_consent: true,
            require_consent: false,
            require_logout_consent: false,
            frontchannel_logout_uri: None,
            backchannel_logout_uri: None,
        })
        .await
        .expect("create test client");

    // 3b. Seed the matching `zeroship.oauth_clients` row. The Slice-3
    //     skip-consent fast path upserts `zeroship.oauth_grants`, whose
    //     `client_id` FK targets `zeroship.oauth_clients(client_id)`. Without
    //     this row the upsert fails its FK and `/consent` renders a
    //     ContactSupport 200 instead of the skip 302 — so the test only ever
    //     passed against a DB where some other run had seeded the client.
    //     (Real per-app clients always exist here via `ensure_app_client`.)
    let redirect_uris: Vec<String> = vec![test_redirect.to_string()];
    let scopes: Vec<String> = vec![
        "openid".into(),
        "offline_access".into(),
        "email".into(),
        "profile".into(),
    ];
    pg_client
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                 (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
             VALUES ($1, $2, $3, $4, TRUE, $1) \
             ON CONFLICT (client_id) DO NOTHING",
            &[
                &test_client_id,
                &"gateway oidc_rp e2e",
                &redirect_uris,
                &scopes,
            ],
        )
        .await
        .expect("seed zeroship.oauth_clients");

    // 4. Seed a user directly into zeroship.users (avoids the signup HTTP
    //    flow — the e2e_password test already covers that, and we want
    //    a deterministic `sub` to assert against).
    let email = format!("gw-{}@zeroship.test", Uuid::new_v4().simple());
    let password = "gateway-test-password-with-enough-bytes-1234";
    let phc = zeroship_auth::identity::password::hash(password).expect("argon2 hash");
    let user_id = Uuid::new_v4();
    pg_client
        .execute(
            "INSERT INTO zeroship.users (id, email, name, password_hash, email_verified_at) \
             VALUES ($1, $2::citext, $3, $4, NOW())",
            &[&user_id, &email, &"Gateway Test", &phc],
        )
        .await
        .expect("seed user");

    // 5. Build the OidcRp under test.
    //
    // The OIDC dial URL is loopback hydra (the only place we can actually
    // dial); `with_issuer` overrides the expected `iss` to hydra's
    // configured value so verification matches what hydra emits.
    let rp = OidcRp::new(
        hydra_public.clone(),
        test_client_id.clone(),
        test_client_secret.clone(),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(expected_iss.clone());

    // 6. Build the authorize redirect — sanity-check the URL shape, then
    //    follow it.
    let (auth_url, stash) =
        rp.build_authorize_redirect("/some/path", test_redirect);
    assert!(
        auth_url.contains(&format!("client_id={test_client_id}")),
        "auth url should embed client_id: {auth_url}"
    );
    assert!(
        auth_url.contains("code_challenge_method=S256"),
        "auth url should request S256 PKCE: {auth_url}"
    );
    assert!(
        auth_url.contains("response_type=code"),
        "auth url should ask for code: {auth_url}"
    );

    let http = cyper::Client::new();
    let mut jar = CookieJar::default();

    // 7. GET /oauth2/auth → 302 to /login?login_challenge=...
    let resp = http
        .request(http::Method::GET, &auth_url)
        .expect("build /oauth2/auth")
        .send()
        .await
        .expect("send /oauth2/auth");
    assert_redirect(&resp, "hydra /oauth2/auth → /login");
    jar.absorb(&resp);
    let login_loc = location(&resp);
    let login_challenge = extract_query_param(&login_loc, "login_challenge")
        .unwrap_or_else(|| panic!("no login_challenge in {login_loc}"));

    // 8. GET /login on our auth server — renders form, sets CSRF cookie.
    let login_get_url = format!("{auth_base}/login?login_challenge={login_challenge}");
    let resp = http
        .request(http::Method::GET, &login_get_url)
        .expect("build GET /login")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET /login");
    assert!(
        resp.status().is_success(),
        "GET /login expected 2xx, got {}",
        resp.status()
    );
    let csrf_cookie = read_set_cookie(&resp, "zsidp_csrf")
        .expect("zsidp_csrf cookie on GET /login");
    jar.set("zsidp_csrf", &csrf_cookie);

    // 9. POST /login — verify password, accept_login at hydra, 302 back
    //    to /oauth2/auth on hydra.
    let login_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_cookie)
        .append_pair("email", &email)
        .append_pair("password", password)
        .finish();
    let resp = http
        .request(http::Method::POST, &login_get_url)
        .expect("build POST /login")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", jar.header())
        .expect("cookie header")
        .body(login_body)
        .send()
        .await
        .expect("send POST /login");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "POST /login expected 302, got {}",
        resp.status()
    );
    jar.absorb(&resp);
    let to_hydra = location(&resp);
    assert!(
        !to_hydra.is_empty(),
        "POST /login must set Location to hydra redirect_to"
    );

    // 10. Follow back to hydra → 302 to /consent?consent_challenge=...
    let to_hydra_local = rewrite_to_hydra_loopback(&to_hydra, &issuer_base, &hydra_public);
    let resp = http
        .request(http::Method::GET, &to_hydra_local)
        .expect("build GET hydra-from-login")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET hydra-from-login");
    assert_redirect(&resp, "hydra (post-login) → /consent");
    jar.absorb(&resp);
    let consent_loc = location(&resp);
    let consent_challenge = extract_query_param(&consent_loc, "consent_challenge")
        .unwrap_or_else(|| panic!("no consent_challenge in {consent_loc}"));

    // 11. GET /consent — skip-consent path → 302 back to hydra.
    let consent_url = format!("{auth_base}/consent?consent_challenge={consent_challenge}");
    let resp = http
        .request(http::Method::GET, &consent_url)
        .expect("build GET /consent")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET /consent");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "GET /consent skip-path expected 302, got {}",
        resp.status()
    );
    jar.absorb(&resp);
    let to_hydra = location(&resp);

    // 12. Follow that redirect → final 302 to test_redirect with ?code=...&state=...
    let to_hydra_local = rewrite_to_hydra_loopback(&to_hydra, &issuer_base, &hydra_public);
    let resp = http
        .request(http::Method::GET, &to_hydra_local)
        .expect("build GET hydra-from-consent")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET hydra-from-consent");
    assert_redirect(&resp, "hydra (post-consent) → RP redirect_uri");
    let cb_url = location(&resp);
    assert!(
        cb_url.starts_with(test_redirect),
        "callback URL must start with redirect_uri (got {cb_url})"
    );
    let code = extract_query_param(&cb_url, "code").expect("code param");
    let state_param = extract_query_param(&cb_url, "state").expect("state param");

    // 13. Hand the code+state+stash to OidcRp::finish_callback. This is
    //     the actual unit under test — code exchange + ID-token verify
    //     against hydra's JWKS.
    let (claims, original_path, granted_scopes) = rp
        .finish_callback(&code, &state_param, &stash)
        .await
        .expect("finish_callback");
    assert_eq!(
        original_path, "/some/path",
        "stash must preserve original_path"
    );
    // The token endpoint's granted-scope response flows through to the cookie
    // session (Slice 3, §1.4). The interactive flow requests at least `openid`,
    // so the granted set is non-empty.
    assert!(
        granted_scopes.contains(&"openid".to_string()),
        "granted scopes from finish_callback should include openid: {granted_scopes:?}"
    );
    assert_eq!(
        claims.sub,
        user_id.to_string(),
        "id-token sub must match the seeded zeroship.users.id"
    );

    // 14. Mint a per-origin gateway session against those claims and
    //     drive validate → revoke → validate (no-op).
    //
    // The session store fns are RLS-scoped (changeset 0025) and need a
    // `&mut Client`. `pg_client` is shared (`Arc`, also lent to GateState), so
    // open a dedicated mutable client off the same DSN for these store ops.
    let (mut sess_client, sess_connection) =
        compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("connect pg (session store)");
    compio::runtime::spawn(async move {
        if let Err(e) = sess_connection.run().await {
            eprintln!("[oidc_rp_e2e] session-store pg driver: {e}");
        }
    })
    .detach();
    let app_id = Uuid::new_v4();
    let session = create(
        &mut sess_client,
        &NewSession {
            user_id: &claims.sub,
            app_id,
            email: claims.email.as_deref(),
            name: claims.name.as_deref(),
            avatar_url: claims.picture.as_deref(),
            email_verified: claims.email_verified.unwrap_or(false),
            // Faithful: persist the REAL granted scopes from the token exchange,
            // so the round-trip below proves the cookie path reads them back.
            granted_scopes: &granted_scopes,
            auth_time: claims.auth_time,
            amr: claims.amr.as_deref().unwrap_or(&[]),
        },
    )
    .await
    .expect("session create");

    let validated = validate(&mut sess_client, session.id, app_id)
        .await
        .expect("validate");
    let validated = validated.expect("session must validate immediately after creation");
    assert_eq!(validated.user_id, claims.sub);
    assert_eq!(validated.app_id, app_id);
    // Slice 3: granted_scopes round-trips through create → validate on the same
    // row (the cookie path's scope source — no zeroship.oauth_grants join).
    assert_eq!(
        validated.granted_scopes, granted_scopes,
        "validate() must return the granted_scopes written at create"
    );

    // Per-app revoke (the only revoke path under RLS): revokes this app's
    // session(s) for the subject.
    revoke_app_sessions_for_user(&mut sess_client, app_id, &claims.sub)
        .await
        .expect("revoke");
    let after_revoke = validate(&mut sess_client, session.id, app_id)
        .await
        .expect("validate post-revoke");
    assert!(
        after_revoke.is_none(),
        "session must not validate after revoke"
    );

    // 15. Cleanup — best-effort; failures here don't fail the test.
    let _ = admin.delete_client(&test_client_id).await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE id = $1",
            &[&session.id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.users WHERE id = $1",
            &[&user_id],
        )
        .await;
    // Remove the seeded client (cascades any zeroship.oauth_grants rows minted
    // by the skip-consent path), keeping the test re-runnable.
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&test_client_id],
        )
        .await;

    // Allow audit::emit tasks to flush before tearing down the server.
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}
