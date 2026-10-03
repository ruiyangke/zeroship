//! Compare the public refusal while observing the account and session effects.

#![allow(clippy::future_not_send)]

use super::fixtures::*;
use crate::support::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::store::users;

#[derive(Debug, PartialEq, Eq)]
struct Refusal {
    headers: Vec<(String, String)>,
    html: String,
}

async fn refusal(response: cyper::Response) -> Refusal {
    assert_eq!(response.status().as_u16(), 401);
    assert!(support::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    let csrf = support::read_set_cookie(&response, "__Host-zsidp_csrf")
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
async fn password_refusals_are_equivalent_even_when_the_padding_matches() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let real = user(&server, &email("real")).await;
        let locked = user(&server, &email("locked")).await;
        let oauth = users::create(&server.orm, &email("oauth"), "OAuth only", None)
            .await
            .unwrap();
        lock_through_login(&server, &locked).await;
        let expected =
            refusal(login(&server, &real.email, WRONG_PASSWORD, &fixture_ip()).await).await;
        let missing = email("absent");
        assert!(
            users::find_by_email(&server.orm, &missing)
                .await
                .unwrap()
                .is_none()
        );
        // The password the padding credential is a hash of. That it matches,
        // and that `password::verify_or_pad` still refuses it, is pinned beside
        // the padding itself: `crates/zeroship-auth/src/identity/password.rs`,
        // `padding_never_verifies_even_the_password_it_was_hashed_from`.
        let padding = "absent-user-padding";
        let case_ids = [&real.id, &locked.id, &oauth.id];
        for (email, submitted) in [
            (missing.as_str(), WRONG_PASSWORD),
            (missing.as_str(), padding),
            (oauth.email.as_str(), padding),
            (locked.email.as_str(), padding),
        ] {
            let actual = refusal(login(&server, email, submitted, &fixture_ip()).await).await;
            assert_eq!(actual, expected, "public refusal for {email}");
            assert_eq!(session_count(&server, &case_ids).await, 0);
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
            users::find_by_email(&server.orm, &missing)
                .await
                .unwrap()
                .is_none()
        );
        let response = login(&server, &real.email, PASSWORD, &fixture_ip()).await;
        assert_session(&server, &response, &real.id).await;
        assert_state(&server, &real.id, 0, LockState::Clear).await;
        assert_eq!(session_count(&server, &case_ids).await, 1);
    })
    .await;
}
