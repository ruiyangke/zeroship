//! Federated identity storage and credential preservation in owned `PostgreSQL`.

use crate::common::database::Database;
use uuid::Uuid;

use zeroship_auth::store::identities;
use zeroship_auth::store::identities::GuardedUnlink;

#[compio::test]
async fn identities_link_find_list_unlink_roundtrip() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        // Seed: an OAuth-only user (password_hash NULL). Email is CITEXT so the
        // bind must be cast — compio-postgres binds &str as TEXT.
        let email = format!("identities-{}@example.test", Uuid::new_v4().simple());
        let user_id = zeroship_core::UserId::mint();
        client
            .execute(
                "INSERT INTO zeroship.users (id, email, name, password_hash) \
                 VALUES ($1, $2::citext, $3, NULL)",
                &[&user_id.as_str(), &email, &"OAuth Only"],
            )
            .await
            .expect("seed user");

        let provider = "google";
        let subject = format!("sub-{}", Uuid::new_v4().simple());

        // (1) Linking succeeds and round-trips email + provider + subject.
        let profile = serde_json::json!({
            "sub": subject,
            "email": email,
            "name": "OAuth Only",
            "picture": "https://example.test/avatar.png",
        });
        let linked = identities::link(
            &client,
            &user_id,
            provider,
            &subject,
            Some(&email),
            Some(&profile),
        )
        .await
        .expect("link identity");
        assert_eq!(linked.user_id, user_id);
        assert_eq!(linked.provider, provider);
        assert_eq!(linked.subject, subject);
        assert_eq!(linked.email_at_link.as_deref(), Some(email.as_str()));

        // (2) find_by_provider_subject returns Some for the real link.
        let found = identities::find_by_provider_subject(&client, provider, &subject)
            .await
            .expect("find by provider+subject");
        let found = found.expect("identity should exist");
        assert_eq!(found.id, linked.id);
        assert_eq!(found.user_id, user_id);

        // (3) Wrong subject returns None.
        let missing = identities::find_by_provider_subject(&client, provider, "no-such-subject")
            .await
            .expect("find missing");
        assert!(missing.is_none(), "unknown subject must return None");

        // (4) list_for_user returns the one identity.
        let listed = identities::list_for_user(&client, &user_id)
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, linked.id);

        // (5) unlink returns true; subsequent find returns None.
        let removed = identities::unlink(&client, &user_id, provider)
            .await
            .expect("unlink");
        assert!(removed, "unlink should report a row was deleted");
        let after = identities::find_by_provider_subject(&client, provider, &subject)
            .await
            .expect("find after unlink");
        assert!(after.is_none(), "identity must be gone after unlink");

        // (6) Idempotent unlink — second call returns false.
        let removed_again = identities::unlink(&client, &user_id, provider)
            .await
            .expect("unlink again");
        assert!(!removed_again, "second unlink must report no rows deleted");
    })
    .await;
}

#[compio::test]
async fn guarded_unlink_allows_only_one_concurrent_oauth_only_unlink() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = format!("identities-{}@example.test", Uuid::new_v4().simple());
        let user_id = zeroship_core::UserId::mint();
        client
            .execute(
                "INSERT INTO zeroship.users (id, email, name, password_hash) \
                 VALUES ($1, $2::citext, $3, NULL)",
                &[&user_id.as_str(), &email, &"OAuth Only"],
            )
            .await
            .expect("seed user");

        let google_subject = format!("google-{}", Uuid::new_v4().simple());
        let github_subject = format!("github-{}", Uuid::new_v4().simple());
        identities::link(
            &client,
            &user_id,
            "google",
            &google_subject,
            Some(&email),
            None,
        )
        .await
        .expect("link google");
        identities::link(
            &client,
            &user_id,
            "github",
            &github_subject,
            Some(&email),
            None,
        )
        .await
        .expect("link github");

        let client_a = database.connect_as_auth().await;
        let client_b = database.connect_as_auth().await;
        let google_pid = client_a
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let github_pid = client_b
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let mut locker = database.connect().await;
        let transaction = locker.transaction().await.expect("begin user lock");
        transaction
            .query_one(
                "SELECT id FROM zeroship.users WHERE id = $1 FOR UPDATE",
                &[&user_id.as_str()],
            )
            .await
            .expect("hold the user while unlink operations start");
        let google_user_id = user_id.clone();
        let unlink_google = compio::runtime::spawn(async move {
            identities::unlink_preserving_credential(&client_a, &google_user_id, "google").await
        });
        let google_waiting = database.wait_until_blocked(&[google_pid]).await;
        let github_user_id = user_id.clone();
        let unlink_github = compio::runtime::spawn(async move {
            identities::unlink_preserving_credential(&client_b, &github_user_id, "github").await
        });
        let both_waiting = database.wait_until_blocked(&[google_pid, github_pid]).await;
        transaction.commit().await.expect("release waiting unlinks");

        let (google_result, github_result) = futures::join!(unlink_google, unlink_github);
        let google_result = google_result
            .expect("join google unlink")
            .expect("google unlink");
        let github_result = github_result
            .expect("join github unlink")
            .expect("github unlink");

        assert_eq!(google_result, GuardedUnlink::Unlinked);
        assert_eq!(github_result, GuardedUnlink::WouldOrphan);

        let remaining = identities::list_for_user(&client, &user_id)
            .await
            .expect("list remaining identities");
        assert_eq!(
            remaining.len(),
            1,
            "concurrent guarded unlink must leave one sign-in identity"
        );
        assert_eq!(remaining[0].provider, "github");
        assert_eq!(remaining[0].subject, github_subject);
        assert!(
            google_waiting && both_waiting,
            "unlink operations must overlap before the user lock is released"
        );
    })
    .await;
}
