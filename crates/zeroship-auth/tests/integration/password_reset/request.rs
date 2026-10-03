//! Reset requests preserve their public response while enforcing the email quota.

#![allow(clippy::future_not_send)]

use crate::support::{self, CapturingMailer, auth_server::AuthServer, database::Database};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use zeroship_auth::{identity::password_reset, store::users};
use zeroship_authn::rate_limit::Quota;

#[derive(Debug, PartialEq, Eq)]
struct Confirmation {
    headers: Vec<(String, String)>,
    html: String,
}

async fn request(server: &AuthServer, email: &str, ip: &str) -> Confirmation {
    let response = server
        .http
        .get(format!("{}/forgot", server.auth_base))
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let csrf = support::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
    let html = response.text().await.unwrap();
    assert!(html.contains("action=\"/forgot\""));
    assert!(html.contains(&csrf));
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("email", email)
        .finish();
    let response = server
        .http
        .request(http::Method::POST, format!("{}/forgot", server.auth_base))
        .unwrap()
        .header("content-type", "application/x-www-form-urlencoded")
        .unwrap()
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .unwrap()
        .header("x-forwarded-for", ip)
        .unwrap()
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(support::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    let csrf = support::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
    assert!(!csrf.is_empty());
    let mut headers: Vec<_> = response
        .headers()
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "date" | "x-request-id"))
        .map(|(name, value)| {
            (
                name.to_string(),
                value.to_str().unwrap().replace(&csrf, "FORM_CSRF"),
            )
        })
        .collect();
    headers.sort();
    let html = response.text().await.unwrap();
    assert!(html.contains("If an account exists"));
    Confirmation {
        headers,
        html: html.replace(&csrf, "FORM_CSRF"),
    }
}

fn latest_token(server: &AuthServer, mailer: &CapturingMailer, email: &str) -> String {
    let messages = mailer.sent();
    let message = messages
        .iter()
        .rev()
        .find(|message| message.to.email == email)
        .unwrap();
    assert_eq!(message.subject, "Reset your zeroship password");
    let link = message
        .text
        .split_whitespace()
        .find(|word| word.starts_with("http://"))
        .unwrap();
    let link = url::Url::parse(link).unwrap();
    assert_eq!(
        link.origin(),
        url::Url::parse(&server.auth_base).unwrap().origin()
    );
    assert_eq!(link.path(), "/reset");
    assert!(message.html.as_deref().unwrap().contains(link.as_str()));
    link.query_pairs()
        .find(|(key, _)| key == "token")
        .unwrap()
        .1
        .into_owned()
}

#[ntex::test]
async fn reset_email_budget_is_shared_across_ips_without_changing_the_public_confirmation() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let email = super::fixtures::email("creator");
        let other_email = super::fixtures::email("other");
        let absent_email = super::fixtures::email("absent");
        let mut user_ids = Vec::new();
        for address in [&email, &other_email] {
            let user = users::create(&server.orm, address, "Reset request", None)
                .await
                .unwrap();
            user_ids.push(user.id);
        }
        let quota = Quota::FORGOT_EMAIL;
        assert!(
            quota.capacity.is_finite() && quota.capacity >= 1.0 && quota.capacity.fract() == 0.0
        );
        assert!(
            Quota::FORGOT_IP.capacity > quota.capacity + 1.0,
            "this scenario must reach the email quota before the independent IP quota"
        );
        let ip = super::fixtures::fixture_ip();
        let other_ip = super::fixtures::fixture_ip();
        let absent_ip = super::fixtures::fixture_ip();
        // The buckets the production handler keys for this case's email and IP.
        let keys = vec![
            format!(
                "forgot_email:{}",
                hex::encode(Sha256::digest(email.as_bytes()))
            ),
            format!("forgot_ip:{ip}"),
        ];
        let expected = request(&server, &email, &ip).await;
        let mut accepted: u32 = 1;
        loop {
            let mut balances: Vec<f64> = server
                .pg
                .query(
                    "SELECT tokens::DOUBLE PRECISION FROM zeroship.rate_limits \
                     WHERE bucket_key = ANY($1)",
                    &[&keys],
                )
                .await
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            balances.sort_by(f64::total_cmp);
            let mut expected_balances = [
                quota.capacity - f64::from(accepted),
                Quota::FORGOT_IP.capacity - f64::from(accepted),
            ];
            expected_balances.sort_by(f64::total_cmp);
            assert_eq!(balances, expected_balances);
            // The case owns these buckets. Freeze refill between HTTP requests
            // while retaining the balances consumed by the production handler.
            let updated = server
                .pg
                .execute(
                    "UPDATE zeroship.rate_limits SET updated_at = NOW() + INTERVAL '1 day' \
                     WHERE bucket_key = ANY($1)",
                    &[&keys],
                )
                .await
                .unwrap();
            assert_eq!(updated, 2);
            if f64::from(accepted) >= quota.capacity {
                break;
            }
            assert_eq!(request(&server, &email, &ip).await, expected);
            accepted += 1;
        }
        assert_eq!(mailer.sent().len(), usize::try_from(accepted).unwrap());
        let last = latest_token(&server, &mailer, &email);
        assert!(password_reset::is_live(&server.pg, &last).await.unwrap());
        for address_ip in [&ip, &other_ip] {
            assert_eq!(
                request(&server, &format!(" {} ", email.to_ascii_uppercase()), address_ip).await,
                expected
            );
            assert_eq!(mailer.sent().len(), usize::try_from(accepted).unwrap());
            assert_eq!(latest_token(&server, &mailer, &email), last);
            assert!(password_reset::is_live(&server.pg, &last).await.unwrap());
        }
        assert_eq!(request(&server, &other_email, &ip).await, expected);
        assert_eq!(mailer.sent().len(), usize::try_from(accepted).unwrap() + 1);
        let other = latest_token(&server, &mailer, &other_email);
        assert!(password_reset::is_live(&server.pg, &other).await.unwrap());
        assert!(password_reset::is_live(&server.pg, &last).await.unwrap());
        assert_eq!(request(&server, &absent_email, &absent_ip).await, expected);
        assert_eq!(mailer.sent().len(), usize::try_from(accepted).unwrap() + 1);
        let ids: Vec<&str> = user_ids.iter().map(zeroship_core::UserId::as_str).collect();
        let sessions: i64 = server
            .pg
            .query_one(
                "SELECT COUNT(*) FROM zeroship.idp_sessions WHERE user_id = ANY($1)",
                &[&ids],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(sessions, 0);
    })
    .await;
}
