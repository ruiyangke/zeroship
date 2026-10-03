//! Reset requests own their database and their observation of hashing.

use super::*;
use crate::store::users;
use crate::test_database::Database;
use ntex::web::{self, test};
use std::cell::Cell;
use std::rc::Rc;

const NEW_PASSWORD: &str = "brand new reset password phrase";
const OLD_PASSWORD: &str = "old reset password phrase";

/// A per-case email so cases sharing the database never share reset tokens.
fn reset_email(label: &str) -> String {
    format!("{label}-{}@example.test", uuid::Uuid::new_v4().simple())
}

/// A per-case forwarded client IP, keeping reset rate-limit buckets scoped to
/// the case that minted it.
fn reset_ip() -> String {
    let bytes = uuid::Uuid::new_v4();
    let bytes = bytes.as_bytes();
    format!("10.{}.{}.{}", bytes[0], bytes[1], bytes[2])
}

#[allow(
    clippy::future_not_send,
    reason = "the HTTP service and hash observation belong to this test's runtime"
)]
async fn observed_post(
    req: HttpRequest,
    form: Form<ResetForm>,
    db: State<Arc<compio_postgres::Client>>,
    calls: State<Rc<Cell<usize>>>,
) -> HttpResponse {
    submit(&req, &form, db.as_ref(), async |plaintext| {
        calls.set(calls.get() + 1);
        hash_password(plaintext).await
    })
    .await
}

// ntex's service type stays inferred at the call site. Every invocation owns
// its service and counter, even when other tests hash passwords concurrently.
macro_rules! reset_service {
    ($pg:expr, $calls:expr, $post:expr) => {{
        let app = test::init_service(
            web::App::new().state($pg).state($calls).service(
                web::resource("/reset")
                    .route(web::get().to(get))
                    .route(web::post().to($post)),
            ),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/reset?token=harvest-csrf-only")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let csrf = response
            .headers()
            .get_all(SET_COOKIE)
            .filter_map(|value| value.to_str().ok())
            .find_map(|cookie| cookie.strip_prefix("__Host-zsidp_csrf="))
            .expect("GET sets the CSRF cookie")
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        (app, csrf)
    }};
}

fn request(csrf: &str, token: &str, ip: &str) -> test::TestRequest {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", csrf)
        .append_pair("token", token)
        .append_pair("password", NEW_PASSWORD)
        .finish();
    test::TestRequest::post()
        .uri("/reset")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .header("x-forwarded-for", ip)
        .set_payload(body)
}

async fn stored_password(pg: &compio_postgres::Client, id: &zeroship_core::UserId) -> String {
    pg.query_one(
        "SELECT password_hash FROM zeroship.users WHERE id = $1",
        &[&id.as_str()],
    )
    .await
    .expect("read password hash")
    .get(0)
}

#[ntex::test]
async fn dead_tokens_never_reach_hashing() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let orm = database.orm().await;
        let calls = Rc::new(Cell::new(0_usize));
        let (app, csrf) = reset_service!(pg.clone(), calls.clone(), observed_post);
        let email = reset_email("reset-dead");
        users::create(&orm, &email, "Reset", None).await.unwrap();
        let client_ip = reset_ip();
        let mut tokens = vec!["never-issued-token".to_owned()];
        let expired = password_reset::issue(&pg, &email).await.unwrap();
        pg.execute(
            "UPDATE zeroship.magic_links SET expires_at = NOW() - INTERVAL '1 hour' \
             WHERE email = $1::citext AND purpose = 'reset'",
            &[&email],
        )
        .await
        .unwrap();
        tokens.push(expired.raw);

        // Submit expiry before issuing another token, which would also consume
        // the expired row and hide whether expiry itself is enforced.
        for token in tokens {
            let response =
                test::call_service(&app, request(&csrf, &token, &client_ip).to_request()).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = test::read_body(response).await;
            assert!(String::from_utf8_lossy(&body).contains("reset link invalid or expired"));
            assert_eq!(calls.get(), 0, "dead tokens must not reach hashing");
        }

        let consumed = password_reset::issue(&pg, &email).await.unwrap();
        assert!(password_reset::redeem(&pg, &consumed.raw)
            .await
            .unwrap()
            .is_some());
        let response =
            test::call_service(&app, request(&csrf, &consumed.raw, &client_ip).to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = test::read_body(response).await;
        assert!(String::from_utf8_lossy(&body).contains("reset link invalid or expired"));
        assert_eq!(calls.get(), 0, "consumed tokens must not reach hashing");
    })
    .await;
}

#[ntex::test]
async fn accepted_request_reaches_the_observed_hasher() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let orm = database.orm().await;
        let email = reset_email("reset-accepted");
        let user = users::create(&orm, &email, "Reset", None)
            .await
            .unwrap();
        let token = password_reset::issue(&pg, &user.email).await.unwrap();
        let calls = Rc::new(Cell::new(0_usize));
        let (app, csrf) = reset_service!(pg.clone(), calls.clone(), observed_post);
        let client_ip = reset_ip();
        let response =
            test::call_service(&app, request(&csrf, &token.raw, &client_ip).to_request()).await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers().get(LOCATION).unwrap(), "/login");
        assert_eq!(calls.get(), 1, "the observer must see accepted hashing");
        assert!(
            password::verify(NEW_PASSWORD, &stored_password(&pg, &user.id).await)
                .await
                .unwrap()
        );
        assert!(!password_reset::is_live(&pg, &token.raw).await.unwrap());
    })
    .await;
}

#[ntex::test]
async fn production_route_updates_the_credential_and_consumes_the_token() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let orm = database.orm().await;
        let old_hash = hash_password(OLD_PASSWORD.to_owned()).await.unwrap();
        let email = reset_email("reset-prod");
        let user = users::create(&orm, &email, "Reset", Some(&old_hash))
            .await
            .unwrap();
        let token = password_reset::issue(&pg, &user.email).await.unwrap();
        let (app, csrf) = reset_service!(pg.clone(), (), post);
        let client_ip = reset_ip();
        let response =
            test::call_service(&app, request(&csrf, &token.raw, &client_ip).to_request()).await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers().get(LOCATION).unwrap(), "/login");
        let stored = stored_password(&pg, &user.id).await;
        assert!(password::verify(NEW_PASSWORD, &stored).await.unwrap());
        assert!(!password::verify(OLD_PASSWORD, &stored).await.unwrap());
        assert!(!password_reset::is_live(&pg, &token.raw).await.unwrap());
    })
    .await;
}

#[ntex::test]
async fn exhausted_ip_is_rejected_before_hashing_a_live_token() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let orm = database.orm().await;
        let email = reset_email("reset-exhausted");
        let user = users::create(&orm, &email, "Reset", None)
            .await
            .unwrap();
        let token = password_reset::issue(&pg, &user.email).await.unwrap();
        let client_ip = reset_ip();
        let other_ip = reset_ip();
        // A future refill timestamp makes exhaustion independent of how long
        // the test runner takes to dispatch the request.
        database
            .connect()
            .await
            .execute(
                "INSERT INTO zeroship.rate_limits (bucket_key, tokens, updated_at) \
             VALUES ($1, 0, NOW() + INTERVAL '1 day')",
                &[&format!("reset_ip:{client_ip}")],
            )
            .await
            .unwrap();
        let calls = Rc::new(Cell::new(0_usize));
        let (app, csrf) = reset_service!(pg.clone(), calls.clone(), observed_post);
        let response =
            test::call_service(&app, request(&csrf, &token.raw, &client_ip).to_request()).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = test::read_body(response).await;
        assert!(String::from_utf8_lossy(&body).contains("too many attempts"));
        assert_eq!(calls.get(), 0, "throttling must precede hashing");
        assert!(password_reset::is_live(&pg, &token.raw).await.unwrap());

        let response =
            test::call_service(&app, request(&csrf, &token.raw, &other_ip).to_request()).await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(calls.get(), 1, "a different IP has an independent budget");
        assert!(
            password::verify(NEW_PASSWORD, &stored_password(&pg, &user.id).await)
                .await
                .unwrap()
        );
    })
    .await;
}

/// `ResetQuery` MUST reject the legacy `t=` name. The field is `token`, and
/// accepting `t=` would silently keep a back-compat alias in place;
/// `/link` and every other token-redeem handler use `?token=`.
#[test]
fn reset_query_accepts_token_param_and_rejects_legacy_t_param() {
    fn parse(q: &str) -> std::result::Result<ResetQuery, serde::de::value::Error> {
        use serde::Deserialize;
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(q.as_bytes())
            .into_owned()
            .collect();
        let de = serde::de::value::MapDeserializer::new(pairs.into_iter());
        ResetQuery::deserialize(de)
    }

    let q = parse("token=abc").expect("token= must parse");
    assert_eq!(q.token, "abc");

    let legacy = parse("t=abc");
    assert!(
        legacy.is_err(),
        "legacy ?t= must not deserialize into ResetQuery; got {legacy:?}"
    );
}

/// The `/reset` inline `<script nonce>` must carry an independent
/// per-response CSP nonce, NOT the CSRF token.
#[test]
fn reset_page_script_nonce_is_independent_of_csrf() {
    use askama::Template;

    let csrf = "csrf-double-submit-token-value";
    let script_nonce = "independent-csp-script-nonce";
    let page = ResetPage {
        token: "tok_abc",
        csrf,
        script_nonce,
        error: None,
    };

    let html = page.render().expect("reset page renders");

    assert!(
        html.contains(&format!("nonce=\"{script_nonce}\"")),
        "inline <script> must carry the independent script nonce"
    );
    assert!(
        !html.contains(&format!("nonce=\"{csrf}\"")),
        "inline <script> must NOT reuse the CSRF token as its nonce"
    );
}
