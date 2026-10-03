//! Magic-login HTTP handoffs through the production router in an owned database.

mod fixtures;
mod native;
mod recovery;

use crate::support::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::identity::magic_link;

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn cookie_less_get_preserves_the_link_until_explicit_redemption() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        fixtures::register_magic_client(&server).await;
        let email = "magic@example.test";
        let issued = magic_link::issue(&server.pg, email, "login").await.unwrap();
        let return_to = support::native_authorize_return_to(
            "magic-test-client", "http://127.0.0.1:9999/cb",
        );
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", &issued.raw)
            .append_pair("return_to", &return_to)
            .finish();
        let response = server.http.get(format!("{}/magic/verify?{query}", server.auth_base))
            .unwrap().send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let csp =
            support::response_header(&response, "content-security-policy").expect("interstitial CSP");
        assert!(
            csp.contains("'nonce-"),
            "the interstitial keeps its per-response script nonce: {csp}"
        );
        assert_eq!(
            support::csp_directive(&csp, "form-action").as_deref(),
            Some("form-action 'self' http://127.0.0.1:9999"),
            "the interstitial's own nonce CSP must name the callback origin: {csp}"
        );
        let csrf = support::read_set_cookie(&response, "__Host-zsidp_magic_csrf")
            .expect("interstitial supplies its CSRF cookie");
        assert_ne!(csrf, issued.csrf_nonce, "this browser did not request the link");
        let html = response.text().await.unwrap();
        assert!(html.contains("action=\"/magic/verify/redeem\""),
            "a valid GET must render the redemption form");
        assert!(html.contains("name=\"return_to\""));
        assert!(html.contains(&csrf), "the form carries the cookie's CSRF value");

        let link = server.pg.query_one(
            "SELECT consumed_pending_at IS NULL AS unreserved, consumed_at IS NULL AS unconsumed \
             FROM zeroship.magic_links WHERE email = $1::citext AND purpose = 'login'", &[&email],
        ).await.unwrap();
        assert!(link.get::<_, bool>("unreserved"), "GET must not reserve the token");
        assert!(link.get::<_, bool>("unconsumed"), "GET must not consume the token");
        let counts = server.pg.query_one(
            "SELECT \
             (SELECT COUNT(*) FROM zeroship.magic_completions WHERE email = $1::citext) AS completions, \
             (SELECT COUNT(*) FROM zeroship.users WHERE email = $1::citext) AS users", &[&email],
        ).await.unwrap();
        assert_eq!(counts.get::<_, i64>("completions"), 0);
        assert_eq!(counts.get::<_, i64>("users"), 0);

        // Explicit submission from this browser must still redeem the same
        // token and produce a cross-device completion for the requesting one.
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .append_pair("token", &issued.raw)
            .append_pair("return_to", &return_to)
            .finish();
        let response = server.http.request(http::Method::POST,
            format!("{}/magic/verify/redeem", server.auth_base))
            .unwrap().header("content-type", "application/x-www-form-urlencoded").unwrap()
            .header("cookie", format!("__Host-zsidp_magic_csrf={csrf}")).unwrap()
            .body(body).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let html = response.text().await.unwrap();
        let completion = server.pg.query_one(
            "SELECT code, email::text FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&issued.csrf_nonce],
        ).await.expect("POST creates the completion");
        let code: String = completion.get("code");
        assert_eq!(completion.get::<_, String>("email"), email);
        assert!(html.contains(&code), "the redeeming browser receives its completion code");
        let verified: bool = server.pg.query_one(
            "SELECT email_verified_at IS NOT NULL FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        ).await.expect("POST creates the user").get(0);
        assert!(verified);
        assert!(magic_link::redeem_pending(&server.pg, &issued.raw).await.unwrap().is_none());
    }).await;
}

/// A form document's `form-action` names the relying-party origin only when the
/// continuation target's `redirect_uri` is registered to its `client_id`. A
/// forged target that merely has the right shape - an unregistered `redirect_uri`
/// on a real client, or an unknown `client_id` - must stay at exactly `'self'`,
/// and a registered callback must still be named.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn form_action_names_only_a_registered_callback() {
    Database::run(async |database| {
        let server = AuthServer::with_mailer(
            database,
            std::sync::Arc::new(support::CapturingMailer::default()),
        )
        .await;
        fixtures::register_magic_client(&server).await;
        let email = "magic-csp@example.test";

        let unregistered = support::native_authorize_return_to(
            "magic-test-client",
            "https://evil.attacker.example/cb",
        );
        let unknown = support::native_authorize_return_to(
            "magic-unknown-client",
            "http://127.0.0.1:9999/cb",
        );
        let registered = support::native_authorize_return_to(
            "magic-test-client",
            "http://127.0.0.1:9999/cb",
        );
        let callback_origin = "form-action 'self' http://127.0.0.1:9999";

        for target in [&unregistered, &unknown] {
            let response = fixtures::start_request(&server, email, target).await;
            assert_eq!(response.status().as_u16(), 200, "/magic/start");
            fixtures::assert_form_action(&response, "form-action 'self'");
        }
        let response = fixtures::start_request(&server, email, &registered).await;
        fixtures::assert_form_action(&response, callback_origin);

        for target in [&unregistered, &unknown] {
            let response = fixtures::await_request(&server, target).await;
            assert_eq!(response.status().as_u16(), 200, "/magic/await");
            fixtures::assert_form_action(&response, "form-action 'self'");
        }
        let response = fixtures::await_request(&server, &registered).await;
        fixtures::assert_form_action(&response, callback_origin);

        for target in [&unregistered, &unknown] {
            let issued = magic_link::issue(&server.pg, email, "login").await.unwrap();
            let response = fixtures::verify_request(&server, &issued.raw, target).await;
            assert_eq!(response.status().as_u16(), 200, "/magic/verify");
            fixtures::assert_form_action(&response, "form-action 'self'");
        }
        let issued = magic_link::issue(&server.pg, email, "login").await.unwrap();
        let response = fixtures::verify_request(&server, &issued.raw, &registered).await;
        fixtures::assert_form_action(&response, callback_origin);
    })
    .await;
}
