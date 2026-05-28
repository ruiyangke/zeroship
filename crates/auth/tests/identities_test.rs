//! Live-PG roundtrip for `auth::store::identities`.
//!
//! Skipped unless `AUTH_DB_URL` is set. Seeds an OAuth-only user (no password
//! hash), exercises link/find/list/unlink, then cleans up via FK cascade.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::store::identities::GuardedUnlink;
use zeroship_auth::store::{identities, migrations};

#[compio::test]
async fn identities_link_find_list_unlink_roundtrip() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping identities_test (no AUTH_DB_URL)");
        return;
    };

    let client = pg_connect(&dsn).await;

    migrations::migrate(&client).await.expect("migrate");

    // Seed: an OAuth-only user (password_hash NULL). Email is CITEXT so the
    // bind must be cast — compio-postgres binds &str as TEXT.
    let email = format!("identities-{}@example.test", Uuid::new_v4().simple());
    let row = client
        .query_one(
            "INSERT INTO auth.users (email, name, password_hash) \
             VALUES ($1::citext, $2, NULL) RETURNING id",
            &[&email, &"OAuth Only"],
        )
        .await
        .expect("seed user");
    let user_id: Uuid = row.get("id");

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
        user_id,
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
    let listed = identities::list_for_user(&client, user_id)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, linked.id);

    // (5) unlink returns true; subsequent find returns None.
    let removed = identities::unlink(&client, user_id, provider)
        .await
        .expect("unlink");
    assert!(removed, "unlink should report a row was deleted");
    let after = identities::find_by_provider_subject(&client, provider, &subject)
        .await
        .expect("find after unlink");
    assert!(after.is_none(), "identity must be gone after unlink");

    // (6) Idempotent unlink — second call returns false.
    let removed_again = identities::unlink(&client, user_id, provider)
        .await
        .expect("unlink again");
    assert!(!removed_again, "second unlink must report no rows deleted");

    // Cleanup. ON DELETE CASCADE on auth.identities.user_id would have caught
    // any stray row; we still drop the user explicitly to leave the schema
    // clean for the next test run.
    client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
        .await
        .ok();
}

#[compio::test]
async fn guarded_unlink_allows_only_one_concurrent_oauth_only_unlink() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping identities_test (no AUTH_DB_URL)");
        return;
    };

    let client = pg_connect(&dsn).await;
    migrations::migrate(&client).await.expect("migrate");

    let email = format!("identities-{}@example.test", Uuid::new_v4().simple());
    let row = client
        .query_one(
            "INSERT INTO auth.users (email, name, password_hash) \
             VALUES ($1::citext, $2, NULL) RETURNING id",
            &[&email, &"OAuth Only"],
        )
        .await
        .expect("seed user");
    let user_id: Uuid = row.get("id");

    let google_subject = format!("google-{}", Uuid::new_v4().simple());
    let github_subject = format!("github-{}", Uuid::new_v4().simple());
    identities::link(&client, user_id, "google", &google_subject, Some(&email), None)
        .await
        .expect("link google");
    identities::link(&client, user_id, "github", &github_subject, Some(&email), None)
        .await
        .expect("link github");

    let client_a = pg_connect(&dsn).await;
    let client_b = pg_connect(&dsn).await;
    let unlink_google = compio::runtime::spawn(async move {
        identities::unlink_preserving_credential(&client_a, user_id, "google").await
    });
    let unlink_github = compio::runtime::spawn(async move {
        identities::unlink_preserving_credential(&client_b, user_id, "github").await
    });

    let google_result = unlink_google
        .await
        .expect("join google unlink")
        .expect("google unlink");
    let github_result = unlink_github
        .await
        .expect("join github unlink")
        .expect("github unlink");

    let unlinked = [google_result, github_result]
        .into_iter()
        .filter(|r| *r == GuardedUnlink::Unlinked)
        .count();
    let refused = [google_result, github_result]
        .into_iter()
        .filter(|r| *r == GuardedUnlink::WouldOrphan)
        .count();
    assert_eq!(unlinked, 1, "exactly one unlink should delete a row");
    assert_eq!(refused, 1, "exactly one unlink should hit the orphan guard");

    let remaining = identities::list_for_user(&client, user_id)
        .await
        .expect("list remaining identities");
    assert_eq!(
        remaining.len(),
        1,
        "concurrent guarded unlink must leave one sign-in identity"
    );

    client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
        .await
        .ok();
}

async fn pg_connect(dsn: &str) -> Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("identities_test connection error: {e}");
        }
    })
    .detach();
    client
}
