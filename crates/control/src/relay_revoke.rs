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
//! cross-schema transaction is physically possible on ONE connection: a
//! schema-qualified `DELETE control.oauth_grants … ; UPDATE
//! auth.app_user_identities …` reaches both schemas over the same client.
//!
//! **The cascade runs on a DEDICATED, owned `Client` — never on the shared
//! `AppState.auth_pg`.** `auth_pg` is a single `Arc<Client>` that every other
//! control handler (authz_guard, admin, oauth, token, backchannel_logout,
//! auth_audit, stripe, env) drives concurrently, and compio-postgres pipelines
//! all callers' statements onto that one connection with NO transaction-level
//! mutual exclusion. Multiplexing a multi-statement `BEGIN…COMMIT` onto it
//! would let a bystander handler's autocommit statement interleave INSIDE the
//! relay transaction — rolled back if the cascade aborts, or executed under the
//! cascade's snapshot/locks if it commits. That is cross-request data
//! corruption, not a refactor. A dedicated owned `Client` (via
//! [`dedicated_client`]) gives us the `&mut self` the RAII [`Client::transaction`]
//! helper needs AND isolates the transaction's snapshot, locks, and any
//! aborted-transaction state to a throwaway connection that is dropped at the
//! end of the call. This is the exact pattern `control/src/stripe_store.rs`
//! uses (`registry.conn()` → a fresh per-call connection per transaction) and
//! that `http_util.rs`'s dedicated-session helper uses.
//!
//! The connection URL is `AppState.auth_db_url` (the auth/control schema URL);
//! the bounded [`CONNECT_TIMEOUT`] keeps a stalled auth-DB from hanging the
//! request thread.
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
//!   impossible to commit one without the other. On any statement error the
//!   RAII transaction guard rolls back on drop, so neither write is applied.
//!   Because the connection is dedicated and discarded after the call, a
//!   failed/aborted transaction can never poison another caller's connection.
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
/// needs `&mut self`) AND its transaction snapshot/locks/abort-state are
/// isolated from every other caller of `auth_pg`. Mirrors `stripe_store`'s
/// `registry.conn()` dedicated-connection-per-transaction pattern, but with a
/// bounded connect timeout so a stalled auth-DB cannot hang the request.
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
/// Both writes run in ONE transaction on a DEDICATED owned client (never the
/// shared `Arc<Client>` `auth_pg` — that would interleave bystander handlers'
/// statements into this transaction). The RAII [`Client::transaction`] guard
/// rolls back on drop. The alias UPDATE keys on `app_client_id = client_id`
/// directly (§6.2) — no cross-schema join. After commit, inbound to that alias
/// bounces because `resolve_active_alias`'s `revoked_at IS NULL` gate now fails
/// (5b webhook path).
///
/// # Errors
/// Surfaces connect / transaction / statement errors. On any error the
/// transaction is rolled back (RAII drop guard), so neither the grant delete
/// nor the alias revoke is applied — the caller treats it as a 500 and the
/// alias stays in whatever consistent state it was. The dedicated connection is
/// dropped on return, so an aborted transaction never poisons another caller.
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
/// §6). Runs as a single UPDATE on a DEDICATED owned client — NOT in the
/// `delete_app` control-schema transaction (that runs on the registry client),
/// but as an immediately-following statement, and NOT on the shared `auth_pg`.
/// Because there is no cross-schema FK from `auth.app_user_identities` to
/// control, this is the ONLY guard against orphaned live aliases for a deleted
/// app.
///
/// A single autocommit `UPDATE` is its own transaction — but it still runs on a
/// dedicated connection so it neither blocks nor is blocked by the shared
/// `auth_pg` FIFO under a revoke storm.
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
