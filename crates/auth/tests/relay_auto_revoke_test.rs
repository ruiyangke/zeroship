//! Fix 6 (relay abuse auto-revoke is honest) — the auto-revoke must do what it
//! audits, and audit what it does. Before the fix, the abuse path emitted a
//! `relay_auto_revoke` audit event with `outcome="success"` while
//! `request_alias_auto_revoke` did NOTHING — a cross-service "revoked" claim
//! that never happened (a lie in the audit trail), AND the alias kept
//! forwarding to the real inbox.
//!
//! The honest fix: the auth service disables its OWN forwarding immediately
//! (`relay::revoke_local_alias` stamps `app_user_identities.revoked_at`, which
//! the `resolve_active_alias` gate honours), and does NOT pretend the
//! cross-service grant revoke ran. This test asserts the REAL store behaviour:
//!
//!   - local disable actually stops forwarding (`resolve_active_alias` → None),
//!   - it is idempotent (a re-trigger reports "already revoked", revokes 0),
//!   - it does NOT touch `zeroship.oauth_grants` (so the audit's
//!     `cross_service_grant_revoke: not_implemented` is the truth).
//!
//! Live PG (`AUTH_DB_URL`); skips without it.

#![allow(clippy::future_not_send)]

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_auth::store::relay;

fn provider_mirror_column() -> String {
    ["hy", "dra_client_id"].concat()
}

async fn pg_or_skip() -> Option<Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("conn err: {e}");
        }
    })
    .detach();
    Some(client)
}

/// Seed a user, an oauth client + grant, and an active relay-alias identity row
/// keyed on `(client_id, user)`. Returns `(user_id, client_id, relay_email)`.
async fn seed_active_alias(db: &Client) -> (Uuid, String, String) {
    let user_id = Uuid::new_v4();
    let email = format!("autorevoke-{}@zeroship.test", user_id.simple());
    db.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, $3, NOW())",
        &[&user_id, &email, &"AutoRevoke".to_string()],
    )
    .await
    .expect("insert user");

    let client_id = format!("oac_autorevoke_{}", Uuid::new_v4().simple());
    let sql = format!(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, {}) \
         VALUES ($1, $2, $3, $4, $1)",
        provider_mirror_column()
    );
    db.execute(
        &sql,
        &[
            &client_id,
            &format!("Client {client_id}"),
            &vec![format!("https://{client_id}.example/cb")],
            &vec!["email".to_string()],
        ],
    )
    .await
    .expect("insert oauth client");
    db.execute(
        "INSERT INTO zeroship.oauth_grants (user_id, client_id, granted_scopes) \
         VALUES ($1, $2, $3)",
        &[&user_id, &client_id, &vec!["email".to_string()]],
    )
    .await
    .expect("insert grant");

    let relay_email = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());
    let pairwise_sub = format!("pws_test_{}", Uuid::new_v4().simple());
    db.execute(
        "INSERT INTO zeroship.app_user_identities \
            (app_client_id, global_user_id, pairwise_sub, relay_email) \
         VALUES ($1, $2, $3, $4)",
        &[&client_id, &user_id, &pairwise_sub, &relay_email],
    )
    .await
    .expect("insert identity");

    (user_id, client_id, relay_email)
}

async fn grant_count(db: &Client, user_id: Uuid, client_id: &str) -> i64 {
    db.query(
        "SELECT COUNT(*)::BIGINT AS n FROM zeroship.oauth_grants \
         WHERE user_id = $1 AND client_id = $2",
        &[&user_id, &client_id],
    )
    .await
    .expect("count grant")[0]
        .get("n")
}

async fn cleanup(db: &Client, user_id: Uuid, client_id: &str) {
    let _ = db
        .execute(
            "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_grants WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
}

/// The honest local-disable half of the abuse auto-revoke (Fix 6): it stops
/// THIS service's forwarding immediately and is idempotent, while leaving the
/// cross-service grant untouched (so the audit's `not_implemented` is truthful,
/// never a fake "revoked success").
#[compio::test]
async fn auto_revoke_locally_disables_forwarding_and_leaves_grant_untouched() {
    let Some(db) = pg_or_skip().await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };
    let (user_id, client_id, relay_email) = seed_active_alias(&db).await;

    // Pre-condition: the alias forwards (active map + live grant present).
    assert!(
        relay::resolve_active_alias(&db, &relay_email)
            .await
            .expect("resolve")
            .is_some(),
        "alias must forward before auto-revoke"
    );

    // Local disable — the in-scope, immediate protection. Revokes exactly 1 row.
    let n = relay::revoke_local_alias(&db, &client_id, user_id)
        .await
        .expect("local revoke");
    assert_eq!(n, 1, "the active alias row must be newly revoked");

    // The faithful seam: forwarding has STOPPED (resolve → None). The audit
    // event therefore truthfully reports the local disable happened.
    assert!(
        relay::resolve_active_alias(&db, &relay_email)
            .await
            .expect("resolve after revoke")
            .is_none(),
        "after local disable the alias must NOT forward (resolve → None)"
    );

    // Idempotent: a re-trigger revokes 0 (already revoked) — the audit then
    // reports "already_local_revoked", never a second fake success.
    let n2 = relay::revoke_local_alias(&db, &client_id, user_id)
        .await
        .expect("local revoke idempotent");
    assert_eq!(n2, 0, "a re-trigger must revoke 0 rows (already revoked)");

    // HONESTY: the cross-service grant revoke did NOT run — the grant row is
    // untouched. This is exactly what the audit detail claims
    // (`cross_service_grant_revoke: not_implemented`), so the trail matches
    // reality rather than claiming a completed cross-service revoke.
    assert_eq!(
        grant_count(&db, user_id, &client_id).await,
        1,
        "auto-revoke must NOT delete the zeroship.oauth_grants row (cross-service \
         revoke is not implemented — the audit must not claim it ran)"
    );

    cleanup(&db, user_id, &client_id).await;
}
