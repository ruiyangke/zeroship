//! Live-PG test for the email-claim swap SOURCE used by the cookie /
//! raw OP Bearer arms (relay sub-spec §7).
//!
//! `identities::lookup_relay_email` supplies the trusted relay alias used by
//! gateway session minting. The
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
//! REFUSES without a test database, naming `tests/provision_test_backends.sh`
//! following the convention for live-PG targets in this crate. No
//! environment variable turns it back into a skip.

mod common;

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_core::user_id::UserId;
use zeroship_gateway::identities;

#[allow(clippy::future_not_send)]
async fn require_pg() -> Client {
    let dsn = common::require_platform_db();
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

async fn seed_user(client: &Client, label: &str) -> UserId {
    let user_id = UserId::mint();
    let email = format!("{label}-{}@zeroship.test", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&user_id.as_str(), &email, &label],
        )
        .await
        .expect("seed user");
    user_id
}

async fn seed_oauth_client(client: &Client, client_id: &str) {
    let redirect_uris: Vec<String> =
        vec!["https://app.zeroship.test/__zeroship/auth/callback".into()];
    let scopes: Vec<String> = vec!["openid".into(), "offline_access".into()];
    client
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, redirect_uris, scopes, \
                 refresh_allowed, token_endpoint_auth_method, brokered) \
             VALUES ($1, $2, $3, $4, TRUE, 'client_secret_basic', TRUE) \
             ON CONFLICT (client_id) DO UPDATE SET \
                redirect_uris = EXCLUDED.redirect_uris, \
                scopes = EXCLUDED.scopes, \
                refresh_allowed = TRUE, \
                token_endpoint_auth_method = 'client_secret_basic', \
                brokered = TRUE",
            &[&client_id, &"relay fixture client", &redirect_uris, &scopes],
        )
        .await
        .expect("seed oauth client");
}

/// The pairwise subject the gateway would project for `(user, client)`, derived
/// through the production function with a fixture salt.
///
/// A hand-written `pws_<random>` is shape-valid and therefore survives
/// `is_pairwise_subject`, so a shape check cannot catch it - but it is not the
/// value any real credential for this pair carries. Since the stored subject is
/// IMMUTABLE (`identities::upsert` refuses to rewrite it), a fabricated seed
/// turns the first token mint added to this binary into an opaque 500.
fn fixture_pairwise_sub(client_id: &str, user_id: &UserId) -> String {
    zeroship_core::auth::derive_pairwise(
        &zeroship_core::crypto::derive_key("identities-relay-test-salt"),
        user_id,
        &format!("https://{client_id}.zeroship.test"),
    )
}

async fn cleanup(client: &Client, client_id: &str, user_id: &UserId) {
    let _ = client
        .execute(
            "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id.as_str()])
        .await;
    let _ = client
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
}

#[compio::test]
async fn lookup_relay_email_returns_active_alias_and_fails_closed_on_revoke() {
    let mut client = require_pg().await;
    let client_id = format!("oac_relayswap_{}", Uuid::new_v4().simple());
    seed_oauth_client(&client, &client_id).await;
    let user_id = seed_user(&client, "relayswap").await;
    let pairwise_sub = fixture_pairwise_sub(&client_id, &user_id);
    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());

    // Active alias row (relay_email set, revoked_at NULL).
    client
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub, relay_email) \
             VALUES ($1, $2, $3, $4)",
            &[&client_id, &user_id.as_str(), &pairwise_sub, &relay_email],
        )
        .await
        .expect("insert active alias");

    // Active ⇒ the swap source returns the alias (apps see the alias, §7).
    assert_eq!(
        identities::lookup_relay_email(&mut client, &client_id, &user_id)
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
            &[&client_id, &user_id.as_str()],
        )
        .await
        .expect("revoke alias");
    assert_eq!(
        identities::lookup_relay_email(&mut client, &client_id, &user_id)
            .await
            .expect("lookup revoked"),
        None,
        "a revoked alias must read as None (fail closed — no real-email leak)"
    );

    cleanup(&client, &client_id, &user_id).await;
}

/// The immutable-binding guard in `identities::upsert`, exercised through the
/// real function against a real database.
///
/// `identity_upsert_sql`'s `WHERE ...pairwise_sub = EXCLUDED.pairwise_sub` is what
/// makes a stored subject immutable: a conflicting insert that carries a
/// DIFFERENT subject updates zero rows, and `upsert` turns that zero into a
/// hard error rather than silently re-keying the row every app uses as its
/// permanent per-user foreign key.
///
/// The unit test above this file (`identities.rs`) only asserts the SQL STRING
/// contains that clause, which would still pass if the clause were moved into a
/// comment and the runtime check dropped. This test asserts the BEHAVIOUR:
/// second bind fails, and the first subject is still what a read returns.
#[compio::test]
async fn upsert_refuses_to_rebind_a_stored_pairwise_subject() {
    let mut client = require_pg().await;
    let client_id = format!("oac_rebind_{}", Uuid::new_v4().simple());
    seed_oauth_client(&client, &client_id).await;
    let user_id = seed_user(&client, "rebind").await;

    // Both subjects are DERIVED the way the gateway derives them, so neither is
    // a hand-invented string that happens to differ: they are the two values
    // the same user genuinely projects to under two sector identifiers, which
    // is exactly the configuration drift the guard exists to refuse.
    let salt = zeroship_core::auth::derive_pairwise_salt(b"identities-rebind-fixture-salt");
    let subject_a = zeroship_core::auth::derive_pairwise(
        &salt,
        &user_id,
        "https://rebind-a.zeroship.test",
    );
    let subject_b = zeroship_core::auth::derive_pairwise(
        &salt,
        &user_id,
        "https://rebind-b.zeroship.test",
    );
    assert_ne!(
        subject_a, subject_b,
        "the fixture must offer the guard two genuinely different subjects"
    );

    identities::upsert(&mut client, &client_id, &user_id, &subject_a)
        .await
        .expect("first bind must be accepted");

    // Re-deriving the SAME subject is the normal re-login path and must still
    // pass, so the test cannot be satisfied by a guard that rejects everything.
    identities::upsert(&mut client, &client_id, &user_id, &subject_a)
        .await
        .expect("re-binding the same subject is idempotent, not a conflict");

    let err = identities::upsert(&mut client, &client_id, &user_id, &subject_b)
        .await
        .expect_err("re-binding a DIFFERENT subject must fail closed");
    let message = format!("{err}");
    assert!(
        message.contains("pairwise binding changed"),
        "the refusal must name the binding, got: {message}"
    );

    // The row must still carry the FIRST subject: failing closed means the
    // stored identity survived, not that the write half-landed.
    assert_eq!(
        identities::lookup_pairwise_sub(&mut client, &client_id, &user_id)
            .await
            .expect("lookup after refused rebind"),
        Some(subject_a.clone()),
        "the refused rebind must leave the original subject stored"
    );

    cleanup(&client, &client_id, &user_id).await;
}

#[compio::test]
async fn lookup_relay_email_is_none_when_no_alias_minted() {
    let mut client = require_pg().await;
    let client_id = format!("oac_noalias_{}", Uuid::new_v4().simple());
    seed_oauth_client(&client, &client_id).await;
    let user_id = seed_user(&client, "noalias").await;
    let pairwise_sub = fixture_pairwise_sub(&client_id, &user_id);

    // Identity row exists (gateway upserted the pairwise mapping) but no alias
    // minted yet — relay_email NULL (consent ran before, or no email scope).
    client
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub) \
             VALUES ($1, $2, $3)",
            &[&client_id, &user_id.as_str(), &pairwise_sub],
        )
        .await
        .expect("insert row without alias");

    // No alias ⇒ None ⇒ the arm fails closed (empty email).
    assert_eq!(
        identities::lookup_relay_email(&mut client, &client_id, &user_id)
            .await
            .expect("lookup no-alias"),
        None,
        "absent alias must read as None (fail closed)"
    );

    cleanup(&client, &client_id, &user_id).await;
}
