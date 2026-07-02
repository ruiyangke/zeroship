//! End-to-end Google federation flow against the in-process `crates/auth`
//! server + an in-process mock Google provider.
//!
//! Skips if `AUTH_DB_URL` is unset. The mock provider (see
//! `tests/common/mock_provider.rs`) is in-process so no real Google credentials
//! are required in CI.
//!
//! What this proves:
//!
//!   1. The auth server's `/oauth/google/start` correctly redirects to
//!      the configured upstream `authorize` URL (here: the mock's).
//!   2. The mock's redirect back into our `/oauth/google/callback`
//!      carries a code + state that the auth server validates.
//!   3. The token exchange against the mock returns a JWT-shaped ID
//!      token signed with the mock's key.
//!   4. The auth server verifies that JWT against the mock's JWKS
//!      (live network fetch via `JwksCache`).
//!   5. The linker creates a fresh `zeroship.users` row + an `zeroship.federated_identities`
//!      row for `(google, mock_user.subject)`.
//!   6. The handler sets a native IdP session cookie and redirects back to the
//!      native OP authorize request.

use std::sync::Arc;
use std::time::Duration;

use ntex::web;
use uuid::Uuid;

use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::server;
use zeroship_core::oidc_verify::JwksCache;

mod common;
use common::mock_provider::{MockProvider, MockUser, ProviderMode};
use common::{location, native_authorize_return_to, read_set_cookie, test_auth_config, CookieJar};

#[ntex::test]
async fn google_federation_creates_new_user() {
    // 0. Env-skip check.
    let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
        eprintln!("[e2e_google] skip (need AUTH_DB_URL)");
        return;
    };

    // 1. Boot the mock Google provider on a random port. The mock will
    //    return this canned identity through both `/token` (as ID-token
    //    claims) and `/.well-known/jwks.json` (with a matching key).
    let test_email = format!("e2e-google-{}@gmail.com", Uuid::new_v4().simple());
    let mock_user = MockUser {
        subject: format!("google-sub-{}", Uuid::new_v4().simple()),
        email: test_email.clone(),
        email_verified: true,
        name: Some("E2E Google User".into()),
        picture: Some("https://lh3.googleusercontent.com/test.png".into()),
        login: None,
        additional_emails: Vec::new(),
    };
    let mock = MockProvider::start(ProviderMode::Google, mock_user.clone()).await;
    eprintln!("[e2e_google] mock provider at {}", mock.base);

    // 2. Connect PG.
    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[e2e_google] pg connection driver: {e}");
        }
    })
    .detach();
    let pg = Arc::new(pg_client);

    // 3. Boot the auth server pointing at the mock for Google URLs. The
    //    `google_redirect_uri` is fixed up after `web::test::server`
    //    binds (we can't know the bound port until then), but ntex's
    //    `App` factory closes over the original config; instead we use
    //    the ntex test server URL after-the-fact to drive the request,
    //    and pre-stamp the redirect_uri with the future loopback host
    //    pattern.
    //
    //    Trick: the auth server's `/oauth/google/start` builds the
    //    upstream authorize URL using `cfg.google_redirect_uri`
    //    verbatim — the URL just needs to round-trip back to the same
    //    callback we drive. Since we DON'T follow the redirect to a
    //    real browser, we don't need that URL to actually resolve.
    //    We use the auth_base URL after-the-fact.
    let mut cfg_inner = test_auth_config(&db_url);
    cfg_inner.google_client_id = Some("mock-google-client".into());
    cfg_inner.google_client_secret = Some("mock-google-secret".into());
    // Placeholder redirect — the actual value only matters for the
    // upstream `/authorize` redirect step, which we DON'T follow to a
    // real Google. The mock will dutifully echo whatever we sent in
    // the `redirect_uri` query param.
    cfg_inner.google_redirect_uri = "http://placeholder/oauth/google/callback".to_string();
    cfg_inner.google_auth_url = mock.google_auth_url();
    cfg_inner.google_token_url = mock.google_token_url();
    cfg_inner.google_jwks_url = mock.google_jwks_url();
    cfg_inner.google_issuer = mock.google_issuer();
    let cfg = Arc::new(cfg_inner);
    let google_jwks = Arc::new(JwksCache::new(&cfg.google_jwks_url));

    let cfg_state = cfg.clone();
    let db_state = pg.clone();
    let jwks_state = google_jwks.clone();
    let refresh_pool_state =
        zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = web::test::server(move || {
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let jwks_state = jwks_state.clone();
        let refresh_pool_state = refresh_pool_state.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(db_state)
                .state(jwks_state)
                .state(refresh_pool_state)
                .middleware(SecurityHeaders::default())
                .configure(server::configure(true, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    eprintln!("[e2e_google] auth server at {auth_base}");

    // 4. Start from a native OP authorize request.
    let http = cyper::Client::new();
    let mut jar = CookieJar::default();
    let return_to = native_authorize_return_to(
        &format!("native-google-{}", Uuid::new_v4().simple()),
        "https://app.zeroship.test/callback",
    );

    // 5. GET /oauth/google/start?return_to=… → 302 to the mock's
    //    /authorize. The handler sets the stash cookie on the way out.
    let start_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    let start_url = format!("{auth_base}/oauth/google/start?{start_query}");
    let resp = http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/google/start")
        .send()
        .await
        .expect("send /oauth/google/start");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "GET /oauth/google/start expected 302 (got {})",
        resp.status()
    );
    let mock_authorize_loc = location(&resp);
    assert!(
        mock_authorize_loc.starts_with(&mock.google_auth_url()),
        "redirect target must be the mock's /authorize: got {mock_authorize_loc}"
    );
    let stash_cookie = read_set_cookie(&resp, "zsidp_google_stash")
        .expect("zsidp_google_stash on /oauth/google/start");

    // 6. Follow the redirect to the mock's /authorize. The mock echoes
    //    code+state back to our callback. We don't follow it
    //    automatically — read the Location header and feed it back.
    let resp = http
        .request(http::Method::GET, &mock_authorize_loc)
        .expect("build mock /authorize")
        .send()
        .await
        .expect("send mock /authorize");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "mock /authorize expected 302 (got {})",
        resp.status()
    );
    let callback_url = location(&resp);
    // The mock 302s back to whatever `redirect_uri` we sent in the
    // authorize query — which is `cfg.google_redirect_uri`. That's a
    // placeholder host; rewrite to point at our auth-test server.
    let callback_with_local = callback_url.replace(
        "http://placeholder/oauth/google/callback",
        &format!("{auth_base}/oauth/google/callback"),
    );
    assert!(
        callback_with_local.starts_with(&format!("{auth_base}/oauth/google/callback")),
        "rewrote callback url must point at auth test server: {callback_with_local}"
    );

    // 7. POST the callback URL with the stash cookie attached. The
    //    handler reads the stash, exchanges the code (against the mock
    //    /token), verifies the ID token (against the mock JWKS), and
    //    resumes the native OP authorize request.
    jar.set("zsidp_google_stash", &stash_cookie);
    let resp = http
        .request(http::Method::GET, &callback_with_local)
        .expect("build /oauth/google/callback")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send /oauth/google/callback");
    assert_eq!(
        resp.status().as_u16(),
        303,
        "GET /oauth/google/callback expected 303 (got {} body={:?})",
        resp.status(),
        resp.text().await.ok()
    );
    let final_loc = location(&resp);
    assert_eq!(final_loc, return_to);

    // 8. Database assertions: a user row was created with the mock's
    //    email, and an identity row for (google, mock_user.subject)
    //    pointing at that user.
    let user_rows = pg
        .query(
            "SELECT id, name, email_verified_at FROM zeroship.users WHERE email = $1::citext",
            &[&test_email.as_str()],
        )
        .await
        .expect("user select");
    assert_eq!(
        user_rows.len(),
        1,
        "exactly one zeroship.users row for the mock email"
    );
    let user_id: uuid::Uuid = user_rows[0].get("id");
    let user_name: String = user_rows[0].get("name");
    let email_verified_at: Option<chrono::DateTime<chrono::Utc>> =
        user_rows[0].try_get("email_verified_at").ok();
    assert_eq!(user_name, "E2E Google User", "user.name copied from mock");
    assert!(
        email_verified_at.is_some(),
        "email_verified_at should be set because @gmail.com is trusted when Google verifies it"
    );

    let identity_rows = pg
        .query(
            "SELECT user_id FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
            &[&"google", &mock_user.subject.as_str()],
        )
        .await
        .expect("identity select");
    assert_eq!(
        identity_rows.len(),
        1,
        "exactly one zeroship.federated_identities row for (google, sub)"
    );
    let identity_user_id: uuid::Uuid = identity_rows[0].get("user_id");
    assert_eq!(identity_user_id, user_id, "identity points at the new user");

    // 9. Cleanup.
    pg.execute(
        "DELETE FROM zeroship.federated_identities WHERE user_id = $1",
        &[&user_id],
    )
    .await
    .ok();
    pg.execute(
        "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
        &[&user_id],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await
        .ok();
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
    drop(mock);
}

#[ntex::test]
async fn google_federation_rejects_untrusted_domain_without_hd() {
    let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
        eprintln!("[e2e_google untrusted] skip (need AUTH_DB_URL)");
        return;
    };

    let test_email = format!(
        "e2e-google-untrusted-{}@example.test",
        Uuid::new_v4().simple()
    );
    let mock_user = MockUser {
        subject: format!("google-sub-{}", Uuid::new_v4().simple()),
        email: test_email.clone(),
        email_verified: true,
        name: Some("E2E Untrusted Google User".into()),
        picture: Some("https://lh3.googleusercontent.com/test.png".into()),
        login: None,
        additional_emails: Vec::new(),
    };
    let mock = MockProvider::start(ProviderMode::Google, mock_user.clone()).await;
    eprintln!("[e2e_google untrusted] mock provider at {}", mock.base);

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[e2e_google untrusted] pg connection driver: {e}");
        }
    })
    .detach();
    let pg = Arc::new(pg_client);

    let mut cfg_inner = test_auth_config(&db_url);
    cfg_inner.google_client_id = Some("mock-google-client".into());
    cfg_inner.google_client_secret = Some("mock-google-secret".into());
    cfg_inner.google_redirect_uri = "http://placeholder/oauth/google/callback".to_string();
    cfg_inner.google_auth_url = mock.google_auth_url();
    cfg_inner.google_token_url = mock.google_token_url();
    cfg_inner.google_jwks_url = mock.google_jwks_url();
    cfg_inner.google_issuer = mock.google_issuer();
    let cfg = Arc::new(cfg_inner);
    let google_jwks = Arc::new(JwksCache::new(&cfg.google_jwks_url));

    let cfg_state = cfg.clone();
    let db_state = pg.clone();
    let jwks_state = google_jwks.clone();
    let refresh_pool_state =
        zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = web::test::server(move || {
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let jwks_state = jwks_state.clone();
        let refresh_pool_state = refresh_pool_state.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(db_state)
                .state(jwks_state)
                .state(refresh_pool_state)
                .middleware(SecurityHeaders::default())
                .configure(server::configure(true, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    let http = cyper::Client::new();
    let mut jar = CookieJar::default();
    let return_to = native_authorize_return_to(
        &format!("native-google-untrusted-{}", Uuid::new_v4().simple()),
        "https://app.zeroship.test/callback",
    );
    let start_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    let start_url = format!("{auth_base}/oauth/google/start?{start_query}");
    let resp = http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/google/start")
        .send()
        .await
        .expect("send /oauth/google/start");
    assert_eq!(resp.status().as_u16(), 302);
    let mock_authorize_loc = location(&resp);
    let stash_cookie = read_set_cookie(&resp, "zsidp_google_stash")
        .expect("zsidp_google_stash on /oauth/google/start");

    let resp = http
        .request(http::Method::GET, &mock_authorize_loc)
        .expect("build mock /authorize")
        .send()
        .await
        .expect("send mock /authorize");
    assert_eq!(resp.status().as_u16(), 302);
    let callback_url = location(&resp);
    let callback_with_local = callback_url.replace(
        "http://placeholder/oauth/google/callback",
        &format!("{auth_base}/oauth/google/callback"),
    );

    jar.set("zsidp_google_stash", &stash_cookie);
    let resp = http
        .request(http::Method::GET, &callback_with_local)
        .expect("build /oauth/google/callback")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send /oauth/google/callback");

    assert_eq!(
        resp.status().as_u16(),
        200,
        "untrusted Google domain without hd must fail closed, not complete native login"
    );
    let body = resp.text().await.expect("body");
    assert!(
        body.to_lowercase().contains("internal error") || body.to_lowercase().contains("link"),
        "error page should not continue OAuth login: body={body}"
    );

    let user_rows = pg
        .query(
            "SELECT id, email_verified_at FROM zeroship.users WHERE email = $1::citext",
            &[&test_email.as_str()],
        )
        .await
        .expect("user select");
    assert!(
        user_rows.is_empty(),
        "untrusted Google domain must not auto-create a user row"
    );

    let identity_rows = pg
        .query(
            "SELECT user_id FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
            &[&"google", &mock_user.subject.as_str()],
        )
        .await
        .expect("identity select");
    assert!(
        identity_rows.is_empty(),
        "untrusted Google domain must not create an identity row"
    );

    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
    drop(mock);
}
