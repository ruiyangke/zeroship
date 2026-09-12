//! Magic-login HTTP handoffs through the production router in an owned database.

mod fixtures;
mod native;

use crate::common::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::identity::magic_link;

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn cookie_less_get_preserves_the_link_until_explicit_redemption() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let email = "magic@example.test";
        let issued = magic_link::issue(&server.pg, email, "login").await.unwrap();
        let return_to = common::native_authorize_return_to(
            "magic-test-client", "http://127.0.0.1:9999/cb",
        );
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", &issued.raw)
            .append_pair("return_to", &return_to)
            .finish();
        let response = server.http.get(format!("{}/magic/verify?{query}", server.auth_base))
            .unwrap().send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let csrf = common::read_set_cookie(&response, "__Host-zsidp_magic_csrf")
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
