#![allow(clippy::future_not_send)]

use super::fixtures::*;
use crate::common::{self, CapturingMailer, auth_server::AuthServer, database::Database};
use std::sync::Arc;
use zeroship_auth::store::users;

enum Device {
    Same,
    Other,
}

async fn magic_login_requires_second_factor(device: Device) {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let user = users::create(&server.orm, "creator@example.test", "Magic account", None).await.unwrap();
        let authenticator = Authenticator::confirmed(&server, &user.id).await;
        let return_to = AuthServer::fresh_challenge();
        let form = server.http.get(format!("{}/login", server.auth_base)).unwrap().send().await.unwrap();
        let csrf = common::read_set_cookie(&form, "__Host-zsidp_csrf").unwrap();
        let start = post(&server, "/magic/start", &format!("__Host-zsidp_csrf={csrf}"),
            &[("csrf", &csrf), ("email", &user.email), ("return_to", &return_to)]).await;
        assert_eq!(start.status().as_u16(), 200);
        let nonce = common::read_set_cookie(&start, "__Host-zsidp_magic_csrf").unwrap();
        let requester_csrf = common::read_set_cookie(&start, "__Host-zsidp_csrf").unwrap();
        let sent = mailer.sent();
        let [message] = sent.as_slice() else { panic!("expected the magic-link email") };
        assert_eq!(message.to.email, user.email);
        let link = message.text.split_whitespace().find(|word| word.contains("/magic/verify?")).unwrap()
            .trim_matches(|ch| matches!(ch, '<' | '>' | '"' | '\''));
        let link = url::Url::parse(link).unwrap();
        assert_eq!(link.origin(), url::Url::parse(&server.auth_base).unwrap().origin());
        let token = link.query_pairs().find(|(key, _)| key == "token").unwrap().1.into_owned();
        let mut landing = server.http.get(link.as_str()).unwrap();
        if matches!(device, Device::Same) { landing = landing.header("cookie", format!("__Host-zsidp_magic_csrf={nonce}")).unwrap(); }
        let landing = landing.send().await.unwrap();
        assert_eq!(landing.status().as_u16(), 200);
        let redeemer = match device {
            Device::Same => nonce.clone(),
            Device::Other => common::read_set_cookie(&landing, "__Host-zsidp_magic_csrf").unwrap(),
        };
        let response = post(&server, "/magic/verify/redeem", &format!("__Host-zsidp_magic_csrf={redeemer}"),
            &[("csrf", &redeemer), ("token", &token), ("return_to", &return_to)]).await;
        let response = match device {
            Device::Same => response,
            Device::Other => {
                assert_eq!(response.status().as_u16(), 200);
                assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
                assert_counts(&server, 0, 0).await;
                let code: String = server.pg.query_one("SELECT code FROM zeroship.magic_completions WHERE csrf_nonce = $1", &[&nonce]).await.unwrap().get(0);
                assert!(response.text().await.unwrap().contains(&code));
                post(&server, "/magic/complete", &format!("__Host-zsidp_csrf={requester_csrf}"),
                    &[("csrf", &requester_csrf), ("csrf_nonce", &nonce), ("code", &code), ("return_to", &return_to)]).await
            }
        };
        let mut challenge = Challenge::from_response(response, &return_to).await;
        assert_counts(&server, 0, 0).await;
        let consumed: bool = server.pg.query_one("SELECT consumed_at IS NOT NULL FROM zeroship.magic_links WHERE csrf_nonce = $1", &[&nonce]).await.unwrap().get(0);
        assert!(consumed, "the link is consumed before the challenge is issued");
        if matches!(device, Device::Other) {
            let consumed: bool = server.pg.query_one("SELECT consumed_at IS NOT NULL FROM zeroship.magic_completions WHERE csrf_nonce = $1", &[&nonce]).await.unwrap().get(0);
            assert!(consumed, "the completion is consumed before the challenge is issued");
        }
        challenge.reject_wrong_code(&server, &authenticator).await;
        assert_counts(&server, 0, 0).await;
        let response = challenge.submit(&server, &authenticator.code()).await;
        assert_session(&server, &response, &user.id, &return_to, "magic", &["magic", "otp"]).await;
        assert_counts(&server, 1, 0).await;
    }).await;
}

#[ntex::test]
async fn same_device_magic_redemption_requires_a_second_factor() {
    magic_login_requires_second_factor(Device::Same).await;
}

#[ntex::test]
async fn cross_device_magic_completion_requires_a_second_factor() {
    magic_login_requires_second_factor(Device::Other).await;
}
