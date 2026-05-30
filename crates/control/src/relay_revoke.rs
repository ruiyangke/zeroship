//! Revocation cascade for relay aliases (relay sub-spec §6, must-fix B4).
//!
//! When a grant is revoked — by the user (`revoke_grant`), or implicitly by
//! deleting the whole app (`delete_app`) — the relay alias for that
//! `(app_client_id, global_user)` MUST be disabled so it stops forwarding the
//! third-party mail to the user's real inbox. Without this, a revoked grant
//! leaves a **live alias forwarding to the real inbox** — the exact privacy
//! failure the relay design exists to prevent.
//!
//! ## Owner + mechanism (§6, B4 closed)
//!
//! Control's revoke paths own the cascade. AGENTS.md guarantees ONE Postgres
//! instance with `control` and `auth` as separate schemas, so a single
//! cross-schema transaction is physically possible — the question is *which
//! client object* opens it. `AppState.auth_pg` is an `Arc<Client>`
//! (a shared connection), and `Client::transaction()` needs `&mut self`, so
//! `auth_pg` **cannot** open a transaction. AppState already carries the
//! answer: `auth_db_url`, the URL the field doc says exists *"for short-lived
//! dedicated sessions"*. We open a **fresh, owned `Client`** on that URL (the
//! exact pattern `http_util.rs` already uses), giving an owned `mut` client
//! that CAN open a transaction.
//!
//! ## Two cascade shapes
//!
//! - [`revoke_grant_cascade`] — the explicit user-revoke path. ONE atomic
//!   transaction: `DELETE` the `control.oauth_grants` row, then `UPDATE`
//!   `auth.app_user_identities.revoked_at = now()` for the SAME
//!   `(client_id, user)` — keyed on `app_client_id = client_id` (§6.2, no
//!   cross-schema join). Atomicity guarantees there is no window where the
//!   grant is gone but the alias still forwards.
//! - [`revoke_all_aliases_for_client`] — the app-delete companion. The
//!   `control` DB rows cascade-delete with the `control.apps` row, but there
//!   is **NO cross-schema FK** from `auth.app_user_identities` to control
//!   (intentional — auth may live in a separate cluster), so this companion
//!   UPDATE is the ONLY thing preventing orphaned live aliases for a deleted
//!   app. It revokes EVERY user's alias for that app's `client_id`.
//!
//! ## Failure semantics (§6, named)
//!
//! - The explicit-revoke `DELETE` + `UPDATE` are one transaction → it is
//!   impossible to commit one without the other.
//! - The app-delete companion runs as an immediately-following statement after
//!   the control-schema delete (which runs on the registry client, a different
//!   connection), so it is NOT in the same transaction as the `control.apps`
//!   delete — by construction. A dropped companion statement would strand live
//!   aliases, so `delete_app` does NOT swallow a companion failure: it returns
//!   a 500 (`deleted: true, aliases_revoked: false`) so the operator can retry
//!   the revoke out-of-band against the deterministic `client_id`. The §10
//!   app-delete test catches a dropped companion statement.
//! - Re-grant keeps the SAME alias (auth's `mint_alias_at_consent` clears
//!   `revoked_at` on the deterministic row), reusing the same `pws_`.

use std::str::FromStr;
use std::time::Duration;

use compio_postgres::{Client, Config, Error, NoTls};

/// Bounded socket-connect timeout for the dedicated revoke client. A hung
/// auth-DB connect must NOT block the control request handler indefinitely
/// (revoke runs inline on `revoke_grant`'s request thread). compio-postgres
/// applies this per socket-level attempt and surfaces a proper connect `Error`
/// on expiry, so the caller's existing error arm handles it (revoke → 500,
/// app-delete companion → 500 with `aliases_revoked: false`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Open a fresh, owned `Client` against the auth/control DB URL and spawn its
/// connection task. The returned client is `mut`-able, so it CAN open a
/// transaction (the shared `Arc<Client>` `auth_pg` cannot — `transaction()`
/// needs `&mut self`). Mirrors `http_util.rs`'s dedicated-session pattern, but
/// with a bounded connect timeout so a stalled auth-DB cannot hang the request.
///
/// # Errors
/// Surfaces the URL-parse error or the underlying connect error (including a
/// `TimedOut` connect error if the socket-connect exceeds [`CONNECT_TIMEOUT`]).
async fn dedicated_client(auth_db_url: &str) -> Result<Client, Error> {
    let mut config = Config::from_str(auth_db_url)?;
    // Only set a default if the URL didn't already pin one — never override an
    // operator-supplied `connect_timeout`.
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(CONNECT_TIMEOUT);
    }
    let (client, connection) = config.connect(NoTls).await?;
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    Ok(client)
}

/// Atomically delete the `control.oauth_grants` row for `(user, client_id)`
/// AND revoke the matching relay alias (§6). Returns the number of grant rows
/// deleted (0 ⇒ the caller answers 404 and the alias UPDATE is a no-op inside
/// the same transaction, so nothing is half-applied).
///
/// Both writes run in ONE transaction on a dedicated owned client (the
/// `Arc<Client>` `auth_pg` cannot open a transaction). The alias UPDATE keys on
/// `app_client_id = client_id` directly (§6.2) — no cross-schema join. After
/// commit, inbound to that alias bounces because `resolve_active_alias`'s
/// `revoked_at IS NULL` gate now fails (5b webhook path).
///
/// # Errors
/// Surfaces connect / transaction / statement errors. On any error the
/// transaction is rolled back (drop guard), so neither the grant delete nor the
/// alias revoke is applied — the caller treats it as a 500 and the alias stays
/// in whatever consistent state it was.
pub async fn revoke_grant_cascade(
    auth_db_url: &str,
    user_id: &uuid::Uuid,
    client_id: &str,
) -> Result<u64, Error> {
    let mut conn = dedicated_client(auth_db_url).await?;
    let tx = conn.transaction().await?;
    // DELETE the grant row FIRST (acquires its row lock — §6 concurrency
    // contract: both writers serialize on the control.oauth_grants row).
    let deleted = tx
        .execute(
            "DELETE FROM control.oauth_grants WHERE user_id = $1 AND client_id = $2",
            &[user_id, &client_id],
        )
        .await?;
    // SAME transaction — revoke the relay alias for THIS (app, user). Keyed
    // DIRECTLY on app_client_id = client_id (oac_…, §6.2): no join, exact-match
    // on the same value the explicit-revoke path already holds. Only an ACTIVE
    // alias is touched (revoked_at IS NULL) so a re-revoke is idempotent.
    tx.execute(
        "UPDATE auth.app_user_identities \
            SET revoked_at = now() \
          WHERE app_client_id = $2 \
            AND global_user_id = $1 \
            AND revoked_at IS NULL",
        &[user_id, &client_id],
    )
    .await?;
    tx.commit().await?;
    Ok(deleted)
}

/// Revoke ALL relay aliases for an app's `client_id` (the app-delete companion,
/// §6). Runs as a single UPDATE on a dedicated owned client — NOT in the
/// `delete_app` control-schema transaction (that runs on the registry client),
/// but as an immediately-following statement. Because there is no cross-schema
/// FK from `auth.app_user_identities` to control, this is the ONLY guard
/// against orphaned live aliases for a deleted app.
///
/// Keyed on `app_client_id = client_id` = `client_id_for_app(uuid)` — the SAME
/// deterministic value the gateway wrote and the explicit-revoke path uses.
/// Returns the number of aliases revoked.
///
/// # Errors
/// Surfaces connect / statement errors. The caller (`delete_app`) does NOT
/// swallow them: a missed companion leaves LIVE aliases forwarding real mail
/// for a deleted app, and there is no background reconciler that re-runs this
/// UPDATE — so `delete_app` returns a 500 (`deleted: true, aliases_revoked:
/// false`) on error so the operator can retry out-of-band. The UPDATE is
/// idempotent and keyed only on the uuid-derived `client_id`, so a retry
/// against that same `client_id` converges (the §10 test asserts it fires and
/// revokes all of the app's aliases).
pub async fn revoke_all_aliases_for_client(
    auth_db_url: &str,
    client_id: &str,
) -> Result<u64, Error> {
    let conn = dedicated_client(auth_db_url).await?;
    let revoked = conn
        .execute(
            "UPDATE auth.app_user_identities \
                SET revoked_at = now() \
              WHERE app_client_id = $1 \
                AND revoked_at IS NULL",
            &[&client_id],
        )
        .await?;
    Ok(revoked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Regression for the review's connect-timeout finding: a stalled / refused
    /// auth-DB connect must surface as an `Err` (so `delete_app`/`revoke_grant`
    /// can return a 500), NOT hang the request thread. Against an unreachable
    /// URL the companion returns within the bounded `CONNECT_TIMEOUT`, never
    /// blocking indefinitely. (No live DB required.)
    #[compio::test]
    async fn companion_connect_failure_is_bounded_not_hung() {
        // 192.0.2.0/24 is TEST-NET-1 (RFC 5737) — guaranteed unroutable, so the
        // connect must rely on the timeout (not an instant RST) to give up.
        let unreachable = "postgres://nobody@192.0.2.1:5432/nodb";
        let started = Instant::now();
        let result = revoke_all_aliases_for_client(unreachable, "oac_test").await;
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "an unreachable auth-DB must surface an error, not silently succeed"
        );
        // Bounded by CONNECT_TIMEOUT (5s) + slack; a regression that drops the
        // timeout would hang here far past this bound.
        assert!(
            elapsed < CONNECT_TIMEOUT + Duration::from_secs(5),
            "connect must give up within the bounded timeout, took {elapsed:?}"
        );
    }

    /// `dedicated_client` must not override an operator-supplied connect timeout
    /// — it only fills in a default when the URL pins none.
    #[test]
    fn respects_explicit_connect_timeout_in_url() {
        let cfg = Config::from_str("postgres://u@h:5432/db?connect_timeout=2")
            .expect("parse url with connect_timeout");
        assert_eq!(
            cfg.get_connect_timeout().copied(),
            Some(Duration::from_secs(2)),
            "an explicit connect_timeout in the URL must be parsed and preserved"
        );
    }
}
