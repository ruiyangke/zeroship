//! Password-reset landing behavior with a real, still redeemable token.

use crate::common::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::identity::password_reset;

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn get_is_uncacheable_and_leaves_the_reset_token_live() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let user = super::fixtures::user(&server.pg, "reset-landing@example.test").await;
        let token = password_reset::issue(&server.pg, &user.email)
            .await
            .unwrap();
        assert!(
            password_reset::is_live(&server.pg, &token.raw)
                .await
                .unwrap()
        );
        let response = server
            .http
            .get(format!("{}/reset?token={}", server.auth_base, token.raw))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
        let html = response.text().await.unwrap();
        assert!(html.contains("action=\"/reset\""));
        assert!(html.contains(&format!("name=\"token\" value=\"{}\"", token.raw)));
        assert!(html.contains(&csrf));
        assert!(
            password_reset::is_live(&server.pg, &token.raw)
                .await
                .unwrap()
        );
    })
    .await;
}
