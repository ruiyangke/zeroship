//! Account-erasure reaper (ISS-12 / GDPR Art. 17).
//!
//! The companion to the `/me/delete` request flow (`store::users::request_deletion`,
//! `ui::account_deletion`). A user who asked to be deleted is marked
//! non-authenticating and given a grace window ([`GRACE_DAYS`]); this periodic
//! task erases the account once that window elapses and the request was not
//! cancelled.
//!
//! It mirrors `token_sweep`'s `loop { tick; sleep }` shape and runs on the same
//! detached compio task vector (`cron::spawn_all`) — zero tokio.
//!
//! ## The billing-retention decision (OPERATOR: REVIEW THIS DEFAULT)
//!
//! GDPR's right-to-erasure is NOT absolute. Art. 17(3)(b) exempts processing
//! "necessary for compliance with a legal obligation" — and a marketplace that
//! has paid an organization via Stripe Connect has tax / anti-money-laundering /
//! chargeback-window obligations to retain that financial history. So the
//! reaper branches (see [`erase_one`] / [`user_has_financial_history`]):
//!
//!   * **No financial history** → **hard `DELETE`** of the `zeroship.users`
//!     row. The 10 `ON DELETE CASCADE` FKs into `users(id)` tear down all
//!     dependents (identities, sessions, grants, memberships, …).
//!
//!   * **Has financial history** → **ANONYMIZE in place**: overwrite the PII
//!     (`email` → an irreversible per-row tombstone, `name`/`avatar_url`/
//!     `password_hash` cleared) and stamp `anonymized_at`.
//!
//! ### HALF THE ORIGINAL REASON FOR THAT BRANCH IS GONE, AND THE OTHER HALF IS NOT
//!
//! The branch used to be justified by referential integrity: the billing ledger
//! pointed at `users(id)` under `ON DELETE RESTRICT`, so a hard delete would
//! either fail or take financial records with it. **No billing edge points at
//! `users` any more** — the subject is `organizations(id)` — so a hard delete no
//! longer threatens a single invoice or payout row, and this predicate is not
//! keeping the ledger alive.
//!
//! What it keeps alive is the ORGANIZATION. `organization_members.user_id` is
//! `ON DELETE CASCADE`, so hard-deleting the last owner of an organization that
//! still holds invoices leaves a billed entity with nobody seated on it: nobody
//! to dispute a charge, nobody to attach a card, and no route by which the
//! platform can reach the party it is invoicing. That is why the predicate
//! resolves through membership rather than being deleted along with the FKs that
//! motivated it.
//!
//! **This default is a policy choice, isolated in `user_has_financial_history`
//! and `erase_one` so an operator / counsel can change it (e.g. add a longer
//! retention horizon, narrow what counts as "financial history", or hash the
//! email under a pepper instead of a random tombstone) without touching the
//! lifecycle plumbing.** Flagged for explicit operator confirmation.
//!
//! The cleaner end state is for organization DELETION to answer this, rather
//! than user deletion inferring it — that decision belongs with the endpoint
//! that deletes organizations and is deliberately not taken here.
//!
//! ## Privilege note
//!
//! The reaper runs on the auth service's DB connection. It must be able to
//! `UPDATE`/`DELETE` `zeroship.users` AND `SET NULL` the attribution FK columns
//! in control-owned tables that do not clear themselves
//! (`oauth_clients.created_by`) — all in the one shared `zeroship` schema.
//! Verify the `zeroship_auth` role's grants cover these cross-table writes in
//! any RLS / least-privilege hardening pass (`0025_roles_rls.sql`).
//!
//! The organization and project membership tables need no such grant: their
//! users edges are declared `ON DELETE SET NULL` / `ON DELETE CASCADE`, so the
//! DELETE of the user row does the work. See [`ATTRIBUTION_FKS`].

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

/// The attribution FK columns pointing at `zeroship.users(id)` that PostgreSQL
/// does NOT clear by itself.
///
/// A hard `DELETE` of an erased user is BLOCKED while any of these still
/// reference it, so the reaper `SET NULL`s them first. Kept as data so the set
/// is auditable in one place (and so the test asserting SET-NULL behaviour and
/// this list cannot silently drift apart).
///
/// # Why the organization tables are absent, and why that is not an oversight
///
/// `zeroship.app_members.added_by` used to be here. That table is deleted, and
/// its replacements - `organization_members`, `project_members`,
/// `organizations`, `projects`, `organization_invites` - declare EVERY
/// attribution edge onto `users` as `ON DELETE SET NULL` and every identity
/// edge as `ON DELETE CASCADE`. PostgreSQL therefore clears them itself, so
/// listing them here would be a second mechanism doing the database's work, and
/// an entry naming a table with a SET NULL edge would be indistinguishable from
/// one naming a table that genuinely blocks.
///
/// The rule for a future editor is the edge, not the column name: a users FK
/// with `NO ACTION` or `RESTRICT` belongs here; one with `SET NULL` or
/// `CASCADE` must not.
const ATTRIBUTION_FKS: &[(&str, &str)] = &[("zeroship.oauth_clients", "created_by")];

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
// The per-thread connection pool is `!Send`; the lint is structural.
#[allow(clippy::future_not_send)]
pub async fn run(refresh_pool: crate::oidc::refresh::RefreshSessionPool) {
    tracing::info!(
        interval_secs = INTERVAL_SECS,
        grace_days = GRACE_DAYS,
        "account_reaper cron starting"
    );
    loop {
        let result = async {
            let pool = refresh_pool
                .checkout_pool("account reaper")
                .await
                .map_err(|e| AuthError::Db(format!("account_reaper pool: {e}")))?;
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AuthError::Db(format!("account_reaper checkout: {e}")))?;
            tick(&mut conn).await
        }
        .await;
        match result {
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
pub async fn tick(db: &mut Client) -> Result<ReaperReport> {
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
                EraseOutcome::Skipped => {}
            },
            Err(e) => {
                tracing::error!(error = %e, user_id = %user_id, "account_reaper erase failed; skipping");
            }
        }
    }
    Ok(report)
}

/// Users whose erasure is due: an explicit request, a schedule in the past,
/// and not already anonymized. A schedule alone denies authentication but is
/// not authority to erase the account.
async fn find_due(db: &Client) -> Result<Vec<Uuid>> {
    let rows = db
        .query(
            "SELECT id FROM zeroship.users \
             WHERE deletion_requested_at IS NOT NULL \
               AND deletion_scheduled_for IS NOT NULL \
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
    Skipped,
}

/// Erase one due user inside a single transaction. SET-NULLs the attribution
/// FKs first (so a hard DELETE is never blocked), then branches on the
/// billing-retention policy ([`user_has_financial_history`]).
async fn erase_one(conn: &mut Client, user_id: Uuid) -> Result<EraseOutcome> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper begin: {e}")))?;
    let result = erase_one_tx(&tx, user_id).await;
    match result {
        Ok(EraseOutcome::Skipped) => {
            tx.rollback()
                .await
                .map_err(|e| AuthError::Db(format!("account_reaper rollback skip: {e}")))?;
            Ok(EraseOutcome::Skipped)
        }
        Ok(outcome) => {
            tx.commit()
                .await
                .map_err(|e| AuthError::Db(format!("account_reaper commit: {e}")))?;
            Ok(outcome)
        }
        Err(e) => {
            if let Err(rb) = tx.rollback().await {
                tracing::error!(error = %rb, "account_reaper rollback failed");
            }
            Err(e)
        }
    }
}

async fn erase_one_tx(conn: &(impl GenericClient + Sync), user_id: Uuid) -> Result<EraseOutcome> {
    // Serialize against cancellation and recheck the complete erasure
    // authority after the due scan. The row lock makes cancellation either
    // win first (this returns Skipped) or wait for the committed erasure.
    let due = conn
        .query(
            "SELECT 1 FROM zeroship.users \
             WHERE id = $1 \
               AND deletion_requested_at IS NOT NULL \
               AND deletion_scheduled_for IS NOT NULL \
               AND deletion_scheduled_for <= NOW() \
               AND anonymized_at IS NULL \
             FOR UPDATE",
            &[&user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper lock due user: {e}")))?;
    if due.is_empty() {
        return Ok(EraseOutcome::Skipped);
    }

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

/// Whether this user is seated on an organization with financial history that
/// GDPR Art. 17(3)(b) lets us retain.
///
/// The predicate is now two hops rather than one, and the hop is the whole
/// change: financial history belongs to an ORGANIZATION, so the question is not
/// "does this person own a Stripe account" but "would deleting this person
/// strand an organization that has been paid or invoiced". It is true when the
/// user holds ANY seat at an organization that EITHER:
///   * owns an `organization_accounts` row (a Stripe-Connect *payout* account —
///     `payouts` FKs it, so it anchors the revenue ledger), OR
///   * has an `invoices` row (an infra-cost invoice — the durable marketplace
///     billing artifact).
///
/// ANY seat, not just an owner's: an admin or a `billing` seat is equally a
/// person the ledger's counterparty is reachable through, and narrowing to
/// `role = 'owner'` would hard-delete them while the organization survives —
/// which is the narrower-looking choice and the more destructive one.
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
                SELECT 1 FROM zeroship.organization_members m \
                 WHERE m.user_id = $1 \
                   AND ( EXISTS ( \
                            SELECT 1 FROM zeroship.organization_accounts oa \
                             WHERE oa.organization_id = m.organization_id \
                         ) \
                      OR EXISTS ( \
                            SELECT 1 FROM zeroship.invoices i \
                             WHERE i.organization_id = m.organization_id \
                         ) ) \
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

    /// The list is exactly the users FKs that BLOCK a delete. Asserting the
    /// whole set rather than its size is the point: a size check passes when
    /// one entry is swapped for another, and the failure this guards is a new
    /// blocking FK reaching production unlisted, where it strands the reaper on
    /// a foreign-key violation rather than on anything a log line explains.
    #[test]
    fn attribution_fk_list_is_exactly_the_blocking_edges() {
        assert_eq!(
            ATTRIBUTION_FKS,
            &[("zeroship.oauth_clients", "created_by")],
            "a users FK with NO ACTION or RESTRICT belongs here; one with SET NULL or CASCADE \
             must not"
        );
        // `zeroship.app_members` is DELETED. An entry naming it would make
        // every hard delete fail on an undefined table, and the failure would
        // read as a permissions problem.
        assert!(
            !ATTRIBUTION_FKS.iter().any(|(table, _)| table.contains("app_members")),
            "app_members no longer exists"
        );
        // The organization tables declare SET NULL / CASCADE, so PostgreSQL
        // clears them. Listing one would be a second mechanism doing the
        // database's work.
        for table in [
            "organization_members",
            "project_members",
            "organizations",
            "projects",
            "organization_invites",
        ] {
            assert!(
                !ATTRIBUTION_FKS.iter().any(|(listed, _)| listed.contains(table)),
                "{table} clears its own users edges; listing it here duplicates the schema"
            );
        }
    }

    #[test]
    fn grace_window_is_thirty_days() {
        assert_eq!(GRACE_DAYS, 30);
    }
}
