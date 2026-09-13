//! PostgreSQL advisory locks for auth multi-instance coordination.

use std::future::Future;

use compio_postgres::{Client, GenericClient};
use zeroship_core::UserId;

use crate::error::{AuthError, Result};

/// Stable process-wide lock for platform OP signing-key registry reconciliation.
pub const OP_SIGNING_KEY_BOOTSTRAP_LOCK: i64 = 0x0042_B007_A071_0003;

/// Fleet-wide single-flight lease for the account-erasure sweep
/// (`crate::cron::account_reaper`).
///
/// Every auth process detaches that cron with no coordination, and its due-scan
/// is an unsharded scan of the whole table, so without a lease every replica
/// ticks the same batch at the same time. That is not merely wasted work: two
/// reapers erasing two co-owners of one organization each observed the other's
/// seat and both committed, which is the state
/// `zeroship_control::organizations`'s module header says no route can repair.
pub const ACCOUNT_REAPER_SWEEP_LOCK: i64 = 0x0042_B007_A071_0004;

/// Refresh-token family hierarchy namespace for the outer per-user lock.
pub const NS_USER: i32 = 0x7a55_0001;
/// Refresh-token family hierarchy namespace for the inner per-family lock.
pub const NS_FAM: i32 = 0x7a55_0002;

/// Every one-argument advisory key this crate takes, paired with what takes it.
///
/// The registry is the coverage: [`tests::all_one_argument_keys_are_distinct`]
/// iterates it, so a key outside it is a key nothing checks. Two sweeps sharing
/// a key block each other fleet-wide and the loser simply stops running, with
/// nothing to read but an absence - the failure
/// `zeroship_control::cron::lock_keys` was built around.
///
/// `NS_USER` / `NS_FAM` are deliberately absent: they are the FIRST argument of
/// the two-argument form, which PostgreSQL keeps in a different key space from
/// the one-argument form, so comparing them against these would be comparing
/// values that can never collide.
///
/// The one-argument space IS shared with the control plane, which runs its own
/// sweeps against this same database. Distinctness across that boundary rests
/// on the two prefixes: auth's keys begin `0x0042_B007`, control's begin
/// `0x7a73`. Neither crate can see the other's list, so keep the prefix.
pub const ONE_ARGUMENT_KEYS: [(i64, &str); 2] = [
    (OP_SIGNING_KEY_BOOTSTRAP_LOCK, "OP signing-key bootstrap"),
    (ACCOUNT_REAPER_SWEEP_LOCK, "account reaper sweep"),
];

/// Run `f` while holding a session-scoped PostgreSQL advisory lock.
///
/// The same [`Client`] must be used for lock, work, and unlock: PostgreSQL
/// advisory locks are scoped to the physical session.
pub async fn with_advisory_lock<F, Fut, R>(conn: &Client, key: i64, f: F) -> Result<R>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R>>,
{
    acquire_advisory_lock(conn, key).await?;

    let result = f().await;
    let unlock = release_advisory_lock(conn, key).await;

    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(unlock_err)) => {
            tracing::error!(
                error = %unlock_err,
                lock_key = key,
                "pg_advisory_unlock failed after locked operation error"
            );
            Err(err)
        }
    }
}

/// Try to take a session-scoped advisory lock, returning `false` when a peer
/// process already holds it.
///
/// The waiting form is wrong for a periodic sweep: a tick that queued behind
/// every peer would still run, one after another, which is the pile-up the
/// lease exists to prevent. A loser skips and the next cadence retries.
///
/// The caller MUST release on the same [`Client`] (see
/// [`release_advisory_lock`]) - the lock is scoped to the physical session, and
/// a pooled connection handed back while still holding it takes the lock out of
/// circulation for as long as the pool keeps it.
///
/// # Errors
///
/// [`AuthError::Db`] if the lock statement itself failed.
pub async fn try_acquire_advisory_lock(conn: &Client, key: i64) -> Result<bool> {
    let rows = conn
        .query("SELECT pg_try_advisory_lock($1) AS locked", &[&key])
        .await
        .map_err(|e| AuthError::Db(format!("pg_try_advisory_lock({key}): {e}")))?;
    Ok(rows.first().is_some_and(|row| row.get::<_, bool>("locked")))
}

async fn acquire_advisory_lock(conn: &Client, key: i64) -> Result<()> {
    conn.execute("SELECT pg_advisory_lock($1)", &[&key])
        .await
        .map_err(|e| AuthError::Db(format!("pg_advisory_lock({key}): {e}")))?;
    Ok(())
}

/// Release a session-scoped advisory lock taken on this same `conn`.
///
/// # Errors
///
/// [`AuthError::Db`] if the unlock statement failed.
pub async fn release_advisory_lock(conn: &Client, key: i64) -> Result<()> {
    conn.execute("SELECT pg_advisory_unlock($1)", &[&key])
        .await
        .map_err(|e| AuthError::Db(format!("pg_advisory_unlock({key}): {e}")))?;
    Ok(())
}

/// Acquire a transaction-scoped two-argument advisory lock on this connection.
///
/// The caller must already be inside the transaction whose writes the lock
/// protects. PostgreSQL releases this form automatically at COMMIT/ROLLBACK.
pub async fn with_xact_advisory_lock2<C>(conn: &C, ns: i32, key: i32) -> Result<()>
where
    C: GenericClient + ?Sized,
{
    conn.execute(
        "SELECT pg_advisory_xact_lock($1::INT4, $2::INT4)",
        &[&ns, &key],
    )
    .await
    .map_err(|e| AuthError::Db(format!("pg_advisory_xact_lock({ns},{key}): {e}")))?;
    Ok(())
}

/// Acquire the refresh hierarchy's per-user xact advisory lock.
///
/// The SQL deliberately hashes in Postgres as `hashtext(user_id::text)`, matching
/// the P5b lock contract and all companion writers.
pub async fn lock_refresh_user_xact<C>(conn: &C, user_id: &UserId) -> Result<()>
where
    C: GenericClient + ?Sized,
{
    conn.execute(
        "SELECT pg_advisory_xact_lock($1::INT4, hashtext($2::text))",
        &[&NS_USER, &user_id.as_str()],
    )
    .await
    .map_err(|e| {
        AuthError::Db(format!(
            "refresh user advisory lock {}: {e}",
            user_id.as_str()
        ))
    })?;
    Ok(())
}

/// Acquire the refresh hierarchy's per-family xact advisory lock.
pub async fn lock_refresh_family_xact<C>(conn: &C, refresh_family_id: &str) -> Result<()>
where
    C: GenericClient + ?Sized,
{
    conn.execute(
        "SELECT pg_advisory_xact_lock($1::INT4, hashtext($2::text))",
        &[&NS_FAM, &refresh_family_id],
    )
    .await
    .map_err(|e| {
        AuthError::Db(format!(
            "refresh family advisory lock {refresh_family_id}: {e}"
        ))
    })?;
    Ok(())
}

/// Stable i64 advisory-lock key for one OAuth grant mutation.
#[must_use]
pub fn oauth_grant_lock_key(user_id: &UserId, client_id: &str) -> i64 {
    stable_lock_key(
        b"zeroship-auth:oauth-grant:",
        &[user_id.as_str().as_bytes(), client_id.as_bytes()],
    )
}

/// Stable i64 advisory-lock key serializing guarded identity-unlinks for one
/// user. The orphan guard in `store::identities::unlink_preserving_credential`
/// must see *committed* state of the other identities, but a single
/// auto-commit statement under READ COMMITTED takes its snapshot before it
/// blocks on `zeroship.users FOR UPDATE` — so two concurrent unlinks each see the
/// other identity still present and both delete, orphaning the account.
/// Holding a session advisory lock on the user across the unlink statement
/// fully serializes the two callers: the loser's unlink statement runs as a
/// fresh implicit transaction *after* the winner commits and releases the
/// lock, so its orphan check reads the winner's committed delete.
#[must_use]
pub fn identity_unlink_lock_key(user_id: &UserId) -> i64 {
    stable_lock_key(
        b"zeroship-auth:identity-unlink:",
        &[user_id.as_str().as_bytes()],
    )
}

fn stable_lock_key(prefix: &[u8], parts: &[&[u8]]) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in prefix {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    for part in parts {
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    i64::from_ne_bytes(hash.to_ne_bytes())
}

#[cfg(test)]
mod tests;
