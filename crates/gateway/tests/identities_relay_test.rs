//! Live-PG test for the email-claim swap SOURCE used by the cookie /
//! raw-Hydra / DPoP-introspection arms (relay sub-spec §7).
//!
//! `identities::lookup_relay_email` is what `project_pairwise` reads to swap
//! the real email for the relay alias on every pairwise-projecting arm. The
//! invariant it enforces is the whole point of the swap:
//!
//!   - active alias (relay_email NOT NULL, revoked_at NULL) ⇒ returns the alias
//!   - revoked alias (revoked_at set) ⇒ returns None (fail closed → empty email)
//!   - no alias / no row ⇒ returns None (fail closed → empty email)
//!
//! Because every arm does `relay_email.unwrap_or_default()` (empty when None),
//! `None` here is exactly the fail-closed-no-real-email-leak behaviour. This is
//! the real function the arms call, not a stub.
//!
//! Skipped without `AUTH_DB_URL` (1b-anchor convention).

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_gateway::identities;

#[allow(clippy::future_not_send)]
async fn pg_or_skip() -> Option<Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    Some(client)
}

async fn seed_user(client: &Client, label: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    let email = format!("{label}-{}@zeroship.test", user_id.simple());
    client
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&user_id, &email, &label],
        )
        .await
        .expect("seed user");
    user_id
}

async fn cleanup(client: &Client, client_id: &str, user_id: Uuid) {
    let _ = client
        .execute(
            "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
}

#[compio::test]
async fn lookup_relay_email_returns_active_alias_and_fails_closed_on_revoke() {
    let Some(mut client) = pg_or_skip().await else {
        eprintln!("[identities_relay_test] skip (no AUTH_DB_URL)");
        return;
    };
    let client_id = format!("oac_relayswap_{}", Uuid::new_v4().simple());
    let user_id = seed_user(&client, "relayswap").await;
    let pairwise_sub = format!("pws_{}", Uuid::new_v4().simple());
    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());

    // Active alias row (relay_email set, revoked_at NULL).
    client
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub, relay_email) \
             VALUES ($1, $2, $3, $4)",
            &[&client_id, &user_id, &pairwise_sub, &relay_email],
        )
        .await
        .expect("insert active alias");

    // Active ⇒ the swap source returns the alias (apps see the alias, §7).
    assert_eq!(
        identities::lookup_relay_email(&mut client, &client_id, user_id)
            .await
            .expect("lookup active"),
        Some(relay_email.clone()),
        "active alias must be returned for the email-claim swap"
    );

    // Revoke (the 5c cascade write) ⇒ the swap source returns None ⇒ the arm
    // emits an EMPTY email (fail closed), NEVER the real address.
    client
        .execute(
            "UPDATE zeroship.app_user_identities SET revoked_at = now() \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&client_id, &user_id],
        )
        .await
        .expect("revoke alias");
    assert_eq!(
        identities::lookup_relay_email(&mut client, &client_id, user_id)
            .await
            .expect("lookup revoked"),
        None,
        "a revoked alias must read as None (fail closed — no real-email leak)"
    );

    cleanup(&client, &client_id, user_id).await;
}

#[compio::test]
async fn lookup_relay_email_is_none_when_no_alias_minted() {
    let Some(mut client) = pg_or_skip().await else {
        eprintln!("[identities_relay_test] skip (no AUTH_DB_URL)");
        return;
    };
    let client_id = format!("oac_noalias_{}", Uuid::new_v4().simple());
    let user_id = seed_user(&client, "noalias").await;
    let pairwise_sub = format!("pws_{}", Uuid::new_v4().simple());

    // Identity row exists (gateway upserted the pairwise mapping) but no alias
    // minted yet — relay_email NULL (consent ran before, or no email scope).
    client
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub) \
             VALUES ($1, $2, $3)",
            &[&client_id, &user_id, &pairwise_sub],
        )
        .await
        .expect("insert row without alias");

    // No alias ⇒ None ⇒ the arm fails closed (empty email).
    assert_eq!(
        identities::lookup_relay_email(&mut client, &client_id, user_id)
            .await
            .expect("lookup no-alias"),
        None,
        "absent alias must read as None (fail closed)"
    );

    cleanup(&client, &client_id, user_id).await;
}
