//! `zeroship.users` CRUD.

use std::cell::Cell;

use compio_postgres::{Client, GenericClient};

use crate::error::{AuthError, Result};

thread_local! {
    /// Test-observable count of failed-login round-trips issued on THIS thread
    /// (real or dummy).
    ///
    /// Bumped once per serialized PG round-trip performed by
    /// [`record_login_failure`] and [`record_login_failure_dummy`]. It exists so a
    /// regression test can assert — without flaky wall-clock timing — that the
    /// real-password failure arm and the absent/OAuth-only failure arm perform an
    /// EQUIVALENT number of latency-visible DB round-trips (finding F7: post-verify
    /// DB-work asymmetry is an email-enumeration timing oracle).
    ///
    /// Thread-local (not a process global) so concurrent tests in the same
    /// binary can't corrupt one another's delta — `verify_password_credentials`
    /// runs its DB round-trips on the calling task's thread (only the Argon2
    /// verify hops to `spawn_blocking`). Production code never reads it.
    static LOGIN_FAILURE_ROUNDTRIPS: Cell<u64> = const { Cell::new(0) };
}

/// Current value of this thread's failed-login round-trip counter (test
/// instrumentation).
#[must_use]
pub fn login_failure_roundtrips() -> u64 {
    LOGIN_FAILURE_ROUNDTRIPS.with(Cell::get)
}

#[inline]
fn bump_login_failure_roundtrips() {
    LOGIN_FAILURE_ROUNDTRIPS.with(|c| c.set(c.get() + 1));
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: uuid::Uuid,
    pub email: String,
    pub email_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub name: String,
    pub avatar_url: Option<String>,
    pub password_hash: Option<String>,
    pub credential_version: i64,
    pub locked_until: Option<chrono::DateTime<chrono::Utc>>,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Look up a user by email. Returns `None` if not found.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn find_by_email(conn: &Client, email: &str) -> Result<Option<UserRow>> {
    let rows = conn
        .query(
            "SELECT id, email::text, email_verified_at, name, avatar_url, password_hash, \
                    credential_version, locked_until, disabled_at \
             FROM zeroship.users WHERE email = $1",
            &[&email],
        )
        .await
        .map_err(|e| AuthError::Db(format!("users find_by_email: {e}")))?;
    Ok(rows.first().map(row_to_user))
}

/// Look up a user by their primary key (`zeroship.users.id`).
///
/// The argument is the UUID rendered as a hyphenated string — that's the
/// shape `accept_login` stamps into the hydra session as `subject`, and
/// what the consent challenge then surfaces back via `info.subject`.
///
/// Returns `Ok(None)` if the string doesn't parse as a UUID OR if no row
/// matches. Callers handling consent flows treat both as "subject unknown
/// to us" → fall back to a minimal `id_token` (sub-only, claims omitted).
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure (parse failure is NOT an error —
/// it's a `None`, since the subject string is attacker-influenced).
pub async fn find_by_id(
    conn: &(impl GenericClient + ?Sized),
    id: &str,
) -> Result<Option<UserRow>> {
    let Ok(uuid) = uuid::Uuid::parse_str(id) else {
        return Ok(None);
    };
    let rows = conn
        .query(
            "SELECT id, email::text, email_verified_at, name, avatar_url, password_hash, \
                    credential_version, locked_until, disabled_at \
             FROM zeroship.users WHERE id = $1",
            &[&uuid],
        )
        .await
        .map_err(|e| AuthError::Db(format!("users find_by_id: {e}")))?;
    Ok(rows.first().map(row_to_user))
}

/// Insert a new user. Returns the created row.
///
/// # Errors
///
/// Returns `AuthError::Db` on conflict (e.g., duplicate email) or other PG failure.
pub async fn create(
    conn: &Client,
    email: &str,
    name: &str,
    password_hash: Option<&str>,
) -> Result<UserRow> {
    let rows = conn
        .query(
            "INSERT INTO zeroship.users (email, name, password_hash) \
             VALUES ($1, $2, $3) \
             RETURNING id, email::text, email_verified_at, name, avatar_url, password_hash, \
                       credential_version, locked_until, disabled_at",
            &[&email, &name, &password_hash],
        )
        .await
        .map_err(|e| {
            if let Some(db_err) = e.as_db_error() {
                if db_err.code().code() == "23505" {
                    return AuthError::Db("email already registered".into());
                }
            }
            AuthError::Db(format!("users create: {e}"))
        })?;
    let row = rows
        .first()
        .ok_or_else(|| AuthError::Db("users create: no row returned".into()))?;
    Ok(row_to_user(row))
}

/// Replace `zeroship.users.password_hash` with a fresh PHC string (Argon2id).
/// Used by the password-reset flow (P5-U6) to set a new credential after
/// a valid reset-token redeem.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn update_password_hash(
    conn: &(impl GenericClient + Sync),
    id: uuid::Uuid,
    phc: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE zeroship.users \
         SET password_hash = $1, \
             credential_version = credential_version + 1, \
             updated_at = NOW() \
         WHERE id = $2",
        &[&phc, &id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("users update_password_hash: {e}")))?;
    Ok(())
}

/// Account-lockout policy (security finding L5). Per-user, conservative on
/// purpose so an attacker can't trivially lock out a victim — the per-email
/// leaky bucket (cap 10/hr) is the broad throttle; this is the escalation arm
/// that turns sustained guessing against ONE account into a hard stop with
/// exponential backoff.
///
/// Lockout engages on the Nth consecutive wrong-password attempt and is
/// cleared on the next success. Backoff grows with the overage past the
/// threshold (1 min → 2 → 4 → … capped) so a brief blip recovers fast while a
/// sustained attack escalates.
pub mod lockout {
    /// Consecutive failures before the account is locked.
    pub const THRESHOLD: i32 = 5;
    /// Initial lock duration once the threshold is first crossed.
    pub const INITIAL_BACKOFF_SECS: i64 = 60;
    /// Upper bound on the (exponentially growing) lock duration.
    pub const MAX_BACKOFF_SECS: i64 = 3600;

    /// Lock duration for a row whose `failed_login_count` has reached `count`
    /// (already including the failure being recorded). Returns `None` while
    /// below `THRESHOLD` (no lock yet).
    #[must_use]
    pub fn backoff_secs(count: i32) -> Option<i64> {
        if count < THRESHOLD {
            return None;
        }
        // Overage past the threshold doubles the window: 0→1×, 1→2×, 2→4×…
        // `min(20)` keeps the shift in range before the cap clamps it.
        let overage = (count - THRESHOLD).min(20);
        let secs = INITIAL_BACKOFF_SECS.saturating_mul(1_i64 << overage);
        Some(secs.min(MAX_BACKOFF_SECS))
    }
}

/// Record a failed password attempt against a real user: bump
/// `failed_login_count` and, once it reaches [`lockout::THRESHOLD`], stamp
/// `locked_until` with exponential backoff. Idempotent per-call (one increment
/// per invocation). Returns the resulting consecutive-failure count.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn record_login_failure(conn: &Client, id: uuid::Uuid) -> Result<i32> {
    // Increment atomically and read back the new count so the lock decision is
    // made against the row's authoritative value (no read-modify-write race).
    bump_login_failure_roundtrips();
    let rows = conn
        .query(
            "UPDATE zeroship.users \
             SET failed_login_count = failed_login_count + 1, \
                 updated_at = NOW() \
             WHERE id = $1 \
             RETURNING failed_login_count",
            &[&id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("users record_login_failure: {e}")))?;
    let Some(row) = rows.first() else {
        // No such user — nothing to lock (the enumeration-defense path never
        // reaches here, since it only fires for a real user).
        return Ok(0);
    };
    let count: i32 = row.get("failed_login_count");

    if let Some(secs) = lockout::backoff_secs(count) {
        conn.execute(
            "UPDATE zeroship.users \
             SET locked_until = NOW() + ($2 || ' seconds')::interval, \
                 updated_at = NOW() \
             WHERE id = $1",
            &[&id, &secs.to_string()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("users record_login_failure lock: {e}")))?;
    }
    Ok(count)
}

/// Enumeration-defense companion to [`record_login_failure`]: issue ONE
/// throwaway `UPDATE zeroship.users … WHERE id = $1` against a random,
/// guaranteed-absent UUID so the absent / OAuth-only / no-credential failure
/// arm performs the SAME serialized PG round-trip the real-password arm does
/// (finding F7).
///
/// Without this, the real-password wrong-password arm runs
/// `record_login_failure` (a `users` UPDATE) before its audit row while the
/// absent arm runs only the audit INSERT — a measurable post-Argon2 latency
/// delta that leaks whether an email belongs to a real, password-bearing
/// account. This is the DB-round-trip analog of the dummy-hash that already
/// equalizes the Argon2 wall time. The UPDATE matches zero rows (random UUID),
/// so it never mutates any account.
///
/// Best-effort by contract: like the real arm, a fault here must NOT change the
/// credential decision. The caller logs and proceeds.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn record_login_failure_dummy(conn: &Client) -> Result<()> {
    bump_login_failure_roundtrips();
    // A fresh v4 UUID never collides with a real `users.id` (UUIDv7 + this is
    // not persisted), so the UPDATE always matches 0 rows. We mirror the real
    // statement's shape (same table, same SET targets, RETURNING) so PG plans
    // and executes equivalent work.
    let absent = uuid::Uuid::new_v4();
    conn.query(
        "UPDATE zeroship.users \
         SET failed_login_count = failed_login_count + 1, \
             updated_at = NOW() \
         WHERE id = $1 \
         RETURNING failed_login_count",
        &[&absent],
    )
    .await
    .map_err(|e| AuthError::Db(format!("users record_login_failure_dummy: {e}")))?;
    Ok(())
}

/// Clear the lockout state after a successful login: zero `failed_login_count`
/// and clear `locked_until`. No-op write when already clean.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn reset_login_failures(conn: &Client, id: uuid::Uuid) -> Result<()> {
    conn.execute(
        "UPDATE zeroship.users \
         SET failed_login_count = 0, \
             locked_until = NULL, \
             updated_at = NOW() \
         WHERE id = $1 \
           AND (failed_login_count <> 0 OR locked_until IS NOT NULL)",
        &[&id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("users reset_login_failures: {e}")))?;
    Ok(())
}

/// Contact details an [`request_deletion`] returns so the caller can send the
/// confirm/undo email and audit the request. (`UserRow` would also carry these,
/// but a dedicated struct documents exactly what the delete flow needs.)
#[derive(Debug, Clone)]
pub struct DeletionRequest {
    pub user_id: uuid::Uuid,
    pub email: String,
    pub name: String,
    pub scheduled_for: chrono::DateTime<chrono::Utc>,
    /// Sessions torn down as part of the request (idp + gateway), for audit.
    pub idp_sessions_revoked: u64,
    pub gateway_sessions_revoked: u64,
}

/// Begin an account-deletion request (ISS-12 / GDPR Art. 17), atomically:
///
///   1. soft-disable the account (`disabled_at = NOW()`) so the existing
///      login/eligibility gates reject it immediately,
///   2. stamp `deletion_requested_at = NOW()` and
///      `deletion_scheduled_for = NOW() + grace_days`,
///   3. bump `credential_version` so any already-issued IdP/gateway session
///      (which binds the version) stops validating, and
///   4. mark every live `idp_sessions` / `gateway_sessions` row revoked.
///
/// Idempotent on an already-requested row: it leaves an existing
/// `deletion_requested_at` untouched (the schedule does not slide) but still
/// re-asserts the disable + revocation. Returns `Ok(None)` if no such user.
///
/// Hydra login-session teardown (a network call to the admin API) is NOT done
/// here — it is the caller's responsibility, mirroring the password-reset flow
/// (`ui/reset.rs`), so this function stays a pure DB transaction.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure (the transaction is rolled back).
pub async fn request_deletion(
    conn: &Client,
    id: uuid::Uuid,
    grace_days: i64,
) -> Result<Option<DeletionRequest>> {
    conn.execute("BEGIN", &[])
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion begin: {e}")))?;
    let result = request_deletion_tx(conn, id, grace_days).await;
    match result {
        Ok(value) => {
            conn.execute("COMMIT", &[])
                .await
                .map_err(|e| AuthError::Db(format!("request_deletion commit: {e}")))?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rb) = conn.execute("ROLLBACK", &[]).await {
                tracing::error!(error = %rb, "request_deletion rollback failed");
            }
            Err(e)
        }
    }
}

async fn request_deletion_tx(
    conn: &Client,
    id: uuid::Uuid,
    grace_days: i64,
) -> Result<Option<DeletionRequest>> {
    let rows = conn
        .query(
            "UPDATE zeroship.users \
             SET disabled_at = COALESCE(disabled_at, NOW()), \
                 deletion_requested_at = COALESCE(deletion_requested_at, NOW()), \
                 deletion_scheduled_for = COALESCE( \
                     deletion_scheduled_for, NOW() + make_interval(days => $2::int)), \
                 credential_version = credential_version + 1, \
                 updated_at = NOW() \
             WHERE id = $1 \
               AND anonymized_at IS NULL \
             RETURNING email::text AS email, name, deletion_scheduled_for",
            &[&id, &i32::try_from(grace_days).unwrap_or(30)],
        )
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion update users: {e}")))?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let email: String = row.get("email");
    let name: String = row.get("name");
    let scheduled_for: chrono::DateTime<chrono::Utc> = row.get("deletion_scheduled_for");

    let idp_sessions_revoked = conn
        .execute(
            "UPDATE zeroship.idp_sessions SET revoked_at = NOW() \
             WHERE user_id = $1 AND revoked_at IS NULL",
            &[&id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion revoke idp_sessions: {e}")))?;
    // DELETE (not UPDATE revoked_at): the `zeroship_auth` role has only
    // SELECT,DELETE on gateway_sessions (no UPDATE — changeset 0025), so an
    // `UPDATE` here fails permission-denied under the real role (it passed
    // tests only because they run as superuser). DELETE matches the existing
    // password-reset cascade (`ui/reset.rs`) and also clears the rows that
    // would otherwise be orphaned on a later hard-delete (gateway_sessions has
    // no FK to users, so CASCADE never reaches them).
    let gateway_sessions_revoked = conn
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE user_id = $1",
            &[&id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("request_deletion delete gateway_sessions: {e}")))?;

    Ok(Some(DeletionRequest {
        user_id: id,
        email,
        name,
        scheduled_for,
        idp_sessions_revoked,
        gateway_sessions_revoked,
    }))
}

/// Cancel an in-flight account-deletion request within the grace window
/// (ISS-12): clear `disabled_at`, `deletion_requested_at`, and
/// `deletion_scheduled_for`, re-enabling the account. Returns `true` if a
/// pending request was cancelled, `false` if there was nothing to cancel
/// (no request in flight, or the account is already anonymized — terminal).
///
/// Sessions are NOT restored — the user signs in fresh, exactly as after a
/// password reset.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn cancel_deletion(conn: &Client, id: uuid::Uuid) -> Result<bool> {
    let n = conn
        .execute(
            "UPDATE zeroship.users \
             SET disabled_at = NULL, \
                 deletion_requested_at = NULL, \
                 deletion_scheduled_for = NULL, \
                 updated_at = NOW() \
             WHERE id = $1 \
               AND deletion_requested_at IS NOT NULL \
               AND anonymized_at IS NULL",
            &[&id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("cancel_deletion: {e}")))?;
    Ok(n > 0)
}

/// Bump `last_login_at` to `NOW()`.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn touch_last_login(conn: &Client, id: uuid::Uuid) -> Result<()> {
    conn.execute(
        "UPDATE zeroship.users SET last_login_at = NOW(), updated_at = NOW() WHERE id = $1",
        &[&id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("users touch_last_login: {e}")))?;
    Ok(())
}

fn row_to_user(row: &compio_postgres::Row) -> UserRow {
    UserRow {
        id: row.get("id"),
        email: row.get::<_, String>("email"),
        email_verified_at: row.try_get("email_verified_at").ok(),
        name: row.get("name"),
        avatar_url: row.try_get("avatar_url").ok(),
        password_hash: row.try_get("password_hash").ok(),
        credential_version: row.get("credential_version"),
        locked_until: row.try_get("locked_until").ok(),
        disabled_at: row.try_get("disabled_at").ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::lockout;

    #[test]
    fn no_lock_below_threshold() {
        for count in 0..lockout::THRESHOLD {
            assert_eq!(
                lockout::backoff_secs(count),
                None,
                "count {count} is below threshold and must not lock"
            );
        }
    }

    #[test]
    fn lock_engages_at_threshold_with_initial_backoff() {
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD),
            Some(lockout::INITIAL_BACKOFF_SECS),
            "first lock uses the initial backoff"
        );
    }

    #[test]
    fn backoff_doubles_then_caps() {
        // Each failure past the threshold doubles the window.
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD + 1),
            Some(lockout::INITIAL_BACKOFF_SECS * 2)
        );
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD + 2),
            Some(lockout::INITIAL_BACKOFF_SECS * 4)
        );
        // Far past the threshold the window saturates at the cap, never beyond,
        // and never overflows (large overage is clamped, no panic).
        assert_eq!(
            lockout::backoff_secs(lockout::THRESHOLD + 100),
            Some(lockout::MAX_BACKOFF_SECS)
        );
        assert_eq!(lockout::backoff_secs(i32::MAX), Some(lockout::MAX_BACKOFF_SECS));
    }
}
