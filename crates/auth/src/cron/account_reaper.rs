//! Account-erasure reaper (ISS-12 / GDPR Art. 17).
//!
//! The companion to the `/me/delete` request flow (`store::users::request_deletion`,
//! `ui::account_deletion`). A user who asked to be deleted is soft-disabled and
//! given a grace window ([`GRACE_DAYS`]); this periodic task is what actually
//! erases the account once that window elapses and the request was not
//! cancelled.
//!
//! It mirrors `token_sweep`'s `loop { tick; sleep }` shape and runs on the same
//! detached compio task vector (`cron::spawn_all`) — zero tokio.
//!
//! ## The billing-retention decision (OPERATOR: REVIEW THIS DEFAULT)
//!
//! GDPR's right-to-erasure is NOT absolute. Art. 17(3)(b) exempts processing
//! "necessary for compliance with a legal obligation" — and a marketplace that
//! has paid a creator via Stripe Connect has tax / anti-money-laundering /
//! chargeback-window obligations to retain that financial history. So the
//! reaper branches (see [`erase_one`] / [`user_has_financial_history`]):
//!
//!   * **No financial history** → **hard `DELETE`** of the `zeroship.users`
//!     row. The 10 `ON DELETE CASCADE` FKs into `users(id)` tear down all
//!     dependents (identities, sessions, grants, app memberships, …).
//!
//!   * **Has financial history** (owns a `creator_accounts` row, i.e. a
//!     Stripe-Connect account, possibly with `payouts`) → **ANONYMIZE in place**:
//!     overwrite the PII (`email` → an irreversible per-row tombstone,
//!     `name`/`avatar_url`/`password_hash` cleared), stamp `anonymized_at`, and
//!     RETAIN the `creator_accounts` + `payouts` rows. The financial ledger
//!     keeps its `creator_id` FK target alive (those rows are `ON DELETE
//!     RESTRICT` and MUST NOT be deleted), but it can no longer be tied back to
//!     a natural person.
//!
//! **This default is a policy choice, isolated in `user_has_financial_history`
//! + `erase_one` so an operator / counsel can change it (e.g. add a longer
//! retention horizon, narrow what counts as "financial history", or hash the
//! email under a pepper instead of a random tombstone) without touching the
//! lifecycle plumbing.** Flagged for explicit operator confirmation.
//!
//! ## Privilege note
//!
//! The reaper runs on the auth service's DB connection. It must be able to
//! `UPDATE`/`DELETE` `zeroship.users` AND `SET NULL` the attribution FK columns
//! in control-owned tables (`app_members.added_by`, `platform_admin_roles.granted_by`,
//! `platform_policies.updated_by`, `oauth_clients.created_by`) — all in the one
//! shared `zeroship` schema. Verify the `zeroship_auth` role's grants cover
//! these cross-table writes in any RLS / least-privilege hardening pass
//! (`0025_roles_rls.sql`).

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::{Client, GenericClient};
use uuid::Uuid;

use crate::error::{AuthError, Result};

/// Grace window between a deletion REQUEST and the IRREVERSIBLE erasure.
///
/// The request flow schedules `deletion_scheduled_for = NOW() + GRACE_DAYS`;
/// the reaper only erases rows whose schedule is in the past.
pub const GRACE_DAYS: i64 = 30;

/// How often the reaper wakes. Hourly is plenty for a 30-day window and matches
/// `token_sweep`'s cadence.
const INTERVAL_SECS: u64 = 60 * 60;

/// The NON-cascade attribution FK columns pointing at `zeroship.users(id)`.
/// A hard `DELETE` of an erased user is BLOCKED while any of these still
/// reference it, so the reaper `SET NULL`s them first. Kept as data so the set
/// is auditable in one place (and the test that asserts SET-NULL behaviour and
/// this list can't silently drift apart).
///
/// `(table, column)`. Sourced from the schema FK inventory (0003/0004):
///   - `app_members.added_by`
///   - `platform_admin_roles.granted_by`
///   - `platform_policies.updated_by`
///   - `oauth_clients.created_by`
const ATTRIBUTION_FKS: &[(&str, &str)] = &[
    ("zeroship.app_members", "added_by"),
    ("zeroship.platform_admin_roles", "granted_by"),
    ("zeroship.platform_policies", "updated_by"),
    ("zeroship.oauth_clients", "created_by"),
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaperReport {
    /// Users hard-deleted (no financial history).
    pub hard_deleted: u64,
    /// Users anonymized in place (financial history retained).
    pub anonymized: u64,
    /// Attribution FK references nulled across all erased users.
    pub attribution_fks_nulled: u64,
}

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps.
///
/// A failing tick is logged and swallowed so a transient PG hiccup doesn't kill
/// the task (mirrors `token_sweep::run`). The sleep is one hour.
//
// `compio_postgres::Client` is `!Send`; the lint is structural (mirrors the
// other cron tasks).
#[allow(clippy::future_not_send)]
pub async fn run(db: Arc<Client>) {
    tracing::info!(
        interval_secs = INTERVAL_SECS,
        grace_days = GRACE_DAYS,
        "account_reaper cron starting"
    );
    loop {
        match tick(&db).await {
            Ok(report) => {
                if report.hard_deleted + report.anonymized > 0 {
                    tracing::info!(
                        hard_deleted = report.hard_deleted,
                        anonymized = report.anonymized,
                        attribution_fks_nulled = report.attribution_fks_nulled,
                        "account_reaper completed"
                    );
                }
            }
            Err(e) => tracing::error!(error = %e, "account_reaper tick failed"),
        }
        compio::time::sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

/// Run one erasure pass. Finds every user past their grace window and not yet
/// anonymized, and erases each in its OWN transaction (a fault on one user must
/// not roll back the others' completed erasures).
///
/// # Errors
///
/// Returns `AuthError::Db` if the due-scan itself fails. A per-user erasure
/// failure is logged and skipped (so one poison row doesn't stall the queue);
/// the next tick retries it.
#[doc(hidden)]
pub async fn tick(db: &Client) -> Result<ReaperReport> {
    let due = find_due(db).await?;
    let mut report = ReaperReport::default();
    for user_id in due {
        match erase_one(db, user_id).await {
            Ok(outcome) => match outcome {
                EraseOutcome::HardDeleted { attribution_fks_nulled } => {
                    report.hard_deleted += 1;
                    report.attribution_fks_nulled += attribution_fks_nulled;
                }
                EraseOutcome::Anonymized { attribution_fks_nulled } => {
                    report.anonymized += 1;
                    report.attribution_fks_nulled += attribution_fks_nulled;
                }
            },
            Err(e) => {
                tracing::error!(error = %e, user_id = %user_id, "account_reaper erase failed; skipping");
            }
        }
    }
    Ok(report)
}

/// Users whose erasure is due: a schedule in the past AND not already
/// anonymized. A CANCELLED request has `deletion_scheduled_for = NULL`, so it
/// is structurally excluded (the `IS NOT NULL` predicate), never erased.
async fn find_due(db: &Client) -> Result<Vec<Uuid>> {
    let rows = db
        .query(
            "SELECT id FROM zeroship.users \
             WHERE deletion_scheduled_for IS NOT NULL \
               AND deletion_scheduled_for <= NOW() \
               AND anonymized_at IS NULL",
            &[],
        )
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper find_due: {e}")))?;
    Ok(rows.iter().map(|r| r.get::<_, Uuid>("id")).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EraseOutcome {
    HardDeleted { attribution_fks_nulled: u64 },
    Anonymized { attribution_fks_nulled: u64 },
}

/// Erase one due user inside a single transaction. SET-NULLs the attribution
/// FKs first (so a hard DELETE is never blocked), then branches on the
/// billing-retention policy ([`user_has_financial_history`]).
async fn erase_one(conn: &Client, user_id: Uuid) -> Result<EraseOutcome> {
    conn.execute("BEGIN", &[])
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper begin: {e}")))?;
    let result = erase_one_tx(conn, user_id).await;
    match result {
        Ok(outcome) => {
            conn.execute("COMMIT", &[])
                .await
                .map_err(|e| AuthError::Db(format!("account_reaper commit: {e}")))?;
            Ok(outcome)
        }
        Err(e) => {
            if let Err(rb) = conn.execute("ROLLBACK", &[]).await {
                tracing::error!(error = %rb, "account_reaper rollback failed");
            }
            Err(e)
        }
    }
}

async fn erase_one_tx(conn: &(impl GenericClient + Sync), user_id: Uuid) -> Result<EraseOutcome> {
    // 1. Clear the NON-cascade attribution references so a hard DELETE is not
    //    blocked (and so an anonymized creator is no longer credited as the
    //    actor on others' rows).
    let attribution_fks_nulled = null_attribution_fks(conn, user_id).await?;

    // 2. Branch on the retention policy.
    if user_has_financial_history(conn, user_id).await? {
        anonymize_user(conn, user_id).await?;
        Ok(EraseOutcome::Anonymized { attribution_fks_nulled })
    } else {
        hard_delete_user(conn, user_id).await?;
        Ok(EraseOutcome::HardDeleted { attribution_fks_nulled })
    }
}

/// Whether the user has financial history that GDPR Art. 17(3)(b) lets us
/// retain. This means EITHER:
///   * they own a `creator_accounts` row (a Stripe-Connect *payout* account —
///     payouts FK it, so it anchors the creator-revenue ledger), OR
///   * they have an `invoices` row (an infra-cost invoice — the durable
///     marketplace billing artifact, keyed by `creator_id`).
/// Either is a retain-on-erase anchor: an invoiced creator's `users` row is
/// anonymized-in-place (not hard-deleted) so the invoice's `creator_id` FK
/// target stays alive. A never-billed creator (neither) is hard-deleted and
/// CASCADE reaps the empty billing shell.
///
/// **Policy knob (operator-editable).** Widen/narrow this predicate to change
/// what blocks a hard delete.
async fn user_has_financial_history(
    conn: &(impl GenericClient + Sync),
    user_id: Uuid,
) -> Result<bool> {
    let rows = conn
        .query(
            "SELECT 1 WHERE EXISTS ( \
                SELECT 1 FROM zeroship.creator_accounts WHERE creator_id = $1 \
             ) OR EXISTS ( \
                SELECT 1 FROM zeroship.invoices WHERE creator_id = $1 \
             )",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper financial-history check: {e}")))?;
    Ok(!rows.is_empty())
}

/// `SET NULL` every attribution FK in [`ATTRIBUTION_FKS`] that points at this
/// user. Returns the total rows nulled across all tables. Table/column names
/// come from a compile-time constant list (never user input), so the formatted
/// SQL is injection-safe.
async fn null_attribution_fks(conn: &(impl GenericClient + Sync), user_id: Uuid) -> Result<u64> {
    let mut total = 0;
    for (table, column) in ATTRIBUTION_FKS {
        let sql = format!("UPDATE {table} SET {column} = NULL WHERE {column} = $1");
        total += conn
            .execute(&sql, &[&user_id])
            .await
            .map_err(|e| AuthError::Db(format!("account_reaper null {table}.{column}: {e}")))?;
    }
    Ok(total)
}

/// Hard-delete the `users` row. The 10 `ON DELETE CASCADE` FKs into `users(id)`
/// remove every dependent (identities, sessions, grants, memberships, …).
async fn hard_delete_user(conn: &(impl GenericClient + Sync), user_id: Uuid) -> Result<()> {
    conn.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper hard delete: {e}")))?;
    Ok(())
}

/// Anonymize the `users` row in place: overwrite PII with an irreversible
/// per-row tombstone, clear credentials, and stamp `anonymized_at`. Retains the
/// row (and therefore the financial ledger's FK target). Idempotent via the
/// `anonymized_at IS NULL` guard — a re-run is a no-op.
async fn anonymize_user(conn: &(impl GenericClient + Sync), user_id: Uuid) -> Result<()> {
    // The tombstone email is derived from the user id, NOT the original
    // address: it is unique (preserves the UNIQUE constraint), routable to
    // nowhere (`.invalid` per RFC 6761), and carries no PII.
    let tombstone = format!("deleted+{}@deleted.invalid", user_id.simple());
    conn.execute(
        "UPDATE zeroship.users \
         SET email = $2::citext, \
             name = '[deleted]', \
             avatar_url = NULL, \
             password_hash = NULL, \
             email_verified_at = NULL, \
             credential_version = credential_version + 1, \
             anonymized_at = NOW(), \
             updated_at = NOW() \
         WHERE id = $1 \
           AND anonymized_at IS NULL",
        &[&user_id, &tombstone],
    )
    .await
    .map_err(|e| AuthError::Db(format!("account_reaper anonymize: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_fk_list_matches_schema_inventory() {
        // The five NON-cascade FKs to zeroship.users are: the four attribution
        // columns (here) plus creator_accounts.creator_id (handled by the
        // anonymize/retain branch, NOT nulled). If a new attribution FK is
        // added to the schema, it MUST be added here or a hard DELETE will be
        // blocked — this assertion is the reminder.
        assert_eq!(ATTRIBUTION_FKS.len(), 4);
        assert!(ATTRIBUTION_FKS.contains(&("zeroship.app_members", "added_by")));
        assert!(ATTRIBUTION_FKS.contains(&("zeroship.platform_admin_roles", "granted_by")));
        assert!(ATTRIBUTION_FKS.contains(&("zeroship.platform_policies", "updated_by")));
        assert!(ATTRIBUTION_FKS.contains(&("zeroship.oauth_clients", "created_by")));
    }

    #[test]
    fn grace_window_is_thirty_days() {
        assert_eq!(GRACE_DAYS, 30);
    }
}
