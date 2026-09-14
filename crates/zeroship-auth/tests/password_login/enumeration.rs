//! Compare the public refusal while observing the account and session effects.

#![allow(clippy::future_not_send)]

use super::fixtures::*;
use crate::common::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::{identity::password, store::users};

#[derive(Debug, PartialEq, Eq)]
struct Refusal {
    headers: Vec<(String, String)>,
    html: String,
}

async fn refusal(response: cyper::Response) -> Refusal {
    assert_eq!(response.status().as_u16(), 401);
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    let csrf = common::read_set_cookie(&response, "__Host-zsidp_csrf")
        .expect("the refusal supplies a fresh form token");
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
    assert!(html.contains("invalid email or password"));
    assert!(html.contains(&format!("name=\"csrf\" value=\"{csrf}\"")));
    // Only the request's fresh form token and transport metadata vary between
    // submissions. Preserve the rest of the response, including cookie policy.
    Refusal {
        headers,
        html: html.replace(&csrf, "FORM_CSRF"),
    }
}

#[ntex::test]
async fn password_refusals_are_equivalent_even_when_the_dummy_hash_matches() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let real = user(&server, "real@example.test").await;
        let locked = user(&server, "locked@example.test").await;
        let oauth = users::create(&server.orm, "oauth@example.test", "OAuth only", None)
            .await
            .unwrap();
        lock_through_login(&server, &locked).await;
        let expected =
            refusal(login(&server, &real.email, WRONG_PASSWORD, "192.0.2.1").await).await;
        let missing = "absent@example.test";
        assert!(
            users::find_by_email(&server.orm, missing)
                .await
                .unwrap()
                .is_none()
        );
        let padding = "absent-user-padding";
        assert!(
            password::verify(padding, password::dummy_hash()).unwrap(),
            "this scenario must submit a password that actually matches the dummy credential"
        );
        for (email, submitted) in [
            (missing, WRONG_PASSWORD),
            (missing, padding),
            (oauth.email.as_str(), padding),
            (locked.email.as_str(), padding),
        ] {
            let actual = refusal(login(&server, email, submitted, "192.0.2.2").await).await;
            assert_eq!(actual, expected, "public refusal for {email}");
            assert_eq!(session_count(&server).await, 0);
        }
        assert_state(&server, &real.id, 1, LockState::Clear).await;
        assert_state(&server, &oauth.id, 0, LockState::Clear).await;
        assert_state(
            &server,
            &locked.id,
            users::lockout::THRESHOLD,
            LockState::Active,
        )
        .await;
        assert!(
            users::find_by_email(&server.orm, missing)
                .await
                .unwrap()
                .is_none()
        );
        let response = login(&server, &real.email, PASSWORD, "192.0.2.3").await;
        assert_session(&server, &response, &real.id).await;
        assert_state(&server, &real.id, 0, LockState::Clear).await;
        assert_eq!(session_count(&server).await, 1);
    })
    .await;
}
