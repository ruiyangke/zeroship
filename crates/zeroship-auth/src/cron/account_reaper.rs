//! Account-erasure reaper (ISS-12 / GDPR Art. 17).
//!
//! The companion to the `/me/delete` request flow (`store::users::request_deletion`,
//! `ui::account_deletion`). A user who asked to be deleted is marked
//! non-authenticating and given a grace window ([`GRACE_DAYS`]); this periodic
//! task erases the account once that window elapses and the request was not
//! cancelled.
//!
//! It mirrors `token_sweep`'s `loop { tick; sleep }` shape and runs on the same
//! detached compio task vector (`cron::spawn_all`) - zero tokio.
//!
//! ## Erasure is a hard DELETE, and there is no second branch
//!
//! This module used to fork: hard-delete a user with no financial history,
//! ANONYMIZE one that had some. Both halves of that fork are gone, for reasons
//! that were measured rather than argued.
//!
//! **The predicate could not run.** `user_has_financial_history` queried
//! `zeroship.organization_members`, `zeroship.organization_accounts` and
//! `zeroship.invoices` on the AUTH service's connection. MEASURED against
//! `information_schema.role_table_grants` on a database with the full corpus
//! applied, `zeroship_auth` holds NO privilege on any of the three. Under the
//! real role that query does not answer "retain" - it raises `42501` inside
//! `erase_one_tx` and fails the erasure of every due user. It passed in tests
//! only because they connect as `postgres`.
//!
//! **The retention it expressed belongs somewhere else.** Financial history is
//! an ORGANIZATION's, not a person's, and no billing edge points at
//! `zeroship.users` any more. What a hard delete can still strand is the
//! ORGANIZATION - `organization_members.user_id` is `ON DELETE CASCADE`, so the
//! last owner's seat goes with them and nobody can be seated again. That is now
//! refused BEFORE the window opens, by the erasure preflight the request flow
//! runs against the control plane (`crate::control_client::erasure_preflight`),
//! and re-checked here before the delete. A blocker that reappeared during the
//! grace window leaves the user pending and loud, never half-erased.
//!
//! **The FK list is gone too.** `ATTRIBUTION_FKS` named the users references
//! PostgreSQL "does not clear by itself" and `SET NULL`ed them by hand. Read out
//! of `pg_constraint`, the blocking set was FIVE constraints, of which the list
//! named one - and three of the five were `NOT NULL`, so `SET NULL` was not a
//! spelling they accepted. `db/migrations-ts/20260907000000_user_erasure_edges.ts`
//! declares every users edge as CASCADE (identity) or SET NULL (attribution),
//! so PostgreSQL clears them, under the constraint owner's privileges rather
//! than the reaper's. Nothing here walks a list.
//!
//! ## The re-check is the fence, not the request-time preflight
//!
//! The grace window is long, and the control plane's answer can change inside
//! it: an invoice for the previous month is finalized by the billing sweep
//! whether or not its subject asked to be deleted, so a person who requested
//! erasure while settled can be in debt by the time the window elapses. A
//! precondition checked only at request time is a check that WAS true once.
//! The reaper is the only actor running at the moment of erasure, so it asks
//! again ([`still_erasable`]) and refuses on either rule.
//!
//! ## The ownership rule is re-decided INSIDE the transaction, under the lock
//!
//! [`still_erasable`] is an HTTP round trip, so it necessarily happens outside
//! the erasure transaction and before it - and a check that is not in the
//! transaction is a check something can invalidate between the answer and the
//! commit. Two triggers did: a co-owner who leaves in that window (their
//! departure passes, because two owners were still seated when it ran), and a
//! second replica ticking the same due batch. Either way both statements
//! commit and the organization is left with no owner - the state
//! `zeroship_control::organizations`'s module header says no route can repair,
//! for the reason it gives there: the rule is about a SET of rows, so no CHECK
//! can hold it, and every other membership mutation is serialized on the
//! organization row instead.
//!
//! Erasure was the one membership mutation that took no such lock:
//! `organization_members.user_id` is `ON DELETE CASCADE`, so the seat goes away
//! as a consequence of deleting a `zeroship.users` row and nothing ever names
//! the organization. [`refuse_if_it_strands_an_organization`] is that lock,
//! taken by the same statement shape and therefore against the same waiters.
//! The HTTP preflight is not redundant: it still carries the money rule and the
//! per-organization remedy a person reads, neither of which is answerable from
//! this connection.
//!
//! ## A failure is loud, per user, and durable
//!
//! `23503` (foreign key) and `23514` (check) are the two SQLSTATEs a correct
//! DELETE can still raise when someone adds a reference the migration above did
//! not anticipate. They are recorded with the constraint name, on the user they
//! happened to, in `zeroship.audit_events` - not swallowed by a
//! `tracing::error!` inside the loop and forgotten.
//!
//! A REFUSED user is recorded the same way and for the same reason: it is left
//! pending forever otherwise, and a pending user nobody can see is a deletion
//! request that silently never happens. Every refusal carries a `stage` an
//! operator can select on - `billing` when the organization owes, `preflight`
//! for the ownership rule as the control plane answered it and for a control
//! plane that could not answer, `ownership` for the in-transaction fence below
//! (a `preflight` that answered clear and was then overtaken, which is a
//! different event and worth telling apart), `constraint`/`database` for a
//! DELETE that failed - so
//!
//!   SELECT actor_user_id, detail FROM zeroship.audit_events
//!    WHERE event_type = 'account_erasure_failed' AND detail->>'stage' = 'billing'
//!
//! is the whole query for "whose erasure is money holding up". The same rows
//! are emitted as `tracing::error!` with the reason attached, so a log-based
//! alert on `account_reaper` needs nothing new either.
//!
//! They deliberately do NOT reach `/readyz`. A poison row is a data problem;
//! failing readiness for one would pull the whole auth service out of rotation
//! and turn one un-erasable account into an outage for every login.

use std::time::Duration;

use compio_postgres::{Client, GenericClient};
use serde_json::json;
use zeroship_core::UserId;

use crate::advisory_lock::{self, ACCOUNT_REAPER_SWEEP_LOCK};
use crate::audit::{self, AuditEvent};
use crate::control_client::{self, PreflightError};
use crate::error::{AuthError, Result};

/// Grace window between a deletion REQUEST and the IRREVERSIBLE erasure.
///
/// The request flow schedules `deletion_scheduled_for = NOW() + GRACE_DAYS`;
/// the reaper only erases rows whose schedule is in the past. It is also the
/// lifetime of the undo token mailed with the confirmation
/// (`crate::identity::deletion_cancel`): the window a person is told they have
/// and the window the token works in are the same window by construction.
pub const GRACE_DAYS: i64 = 30;

/// How often the reaper wakes. Hourly is plenty for a 30-day window and matches
/// `token_sweep`'s cadence.
const INTERVAL_SECS: u64 = 60 * 60;

/// Where the reaper asks whether a due user is still erasable, and what it
/// presents when it asks.
///
/// The keyring is this service's OWN assertion key, shared with the `/me/delete`
/// handler so both halves of the deletion lifecycle assert the same identity. It
/// is not a shared root: control grants `svc/auth` the preflight endpoint and
/// nothing else.
#[derive(Debug, Clone)]
pub struct ControlAccess {
    pub control_url: String,
    pub keyring: std::sync::Arc<zeroship_core::service_peers::ServiceKeyring>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaperReport {
    /// Users hard-deleted.
    pub erased: u64,
    /// Users left pending because their erasure failed or was re-blocked. Each
    /// one has its own audit row; this is the count, not the record.
    pub failed: u64,
}

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps.
///
/// A failing tick is logged and swallowed so a transient PG hiccup doesn't kill
/// the task (mirrors `token_sweep::run`). The sleep is one hour.
///
/// # One reaper per fleet, per tick
///
/// `cron::spawn_all` detaches this task in EVERY auth process, and [`find_due`]
/// is an unsharded scan of the whole table, so every replica would otherwise
/// erase the same batch at the same moment. The lease
/// ([`ACCOUNT_REAPER_SWEEP_LOCK`]) is the single-flight, and a loser skips the
/// tick rather than queueing behind the winner to redo its work an hour late.
///
/// It is hygiene, NOT the ownership fence. A lease bounds how many reapers run
/// at once; it says nothing about the creator who leaves an organization while
/// the one reaper is mid-erasure. That is
/// [`refuse_if_it_strands_an_organization`], and it holds whether or not this
/// lease exists.
//
// The per-thread connection pool is `!Send`; the lint is structural.
#[allow(clippy::future_not_send)]
pub async fn run(refresh_pool: crate::oidc::refresh::RefreshSessionPool, control: ControlAccess) {
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
                .acquire()
                .await
                .map_err(|e| AuthError::Db(format!("account_reaper checkout: {e}")))?;
            if !advisory_lock::try_acquire_advisory_lock(&conn, ACCOUNT_REAPER_SWEEP_LOCK).await? {
                tracing::debug!("account_reaper: another instance holds the sweep lease");
                return Ok(ReaperReport::default());
            }
            let result = tick(&mut conn, &control).await;
            // Released on every arm, including the failing one. The lock is
            // scoped to this physical session and the session goes back to the
            // pool, so an unreleased key would take the whole sweep out of
            // service for as long as the pool keeps this connection - a failure
            // that reads as "there was nothing to erase".
            if let Err(e) =
                advisory_lock::release_advisory_lock(&conn, ACCOUNT_REAPER_SWEEP_LOCK).await
            {
                tracing::error!(error = %e, "account_reaper: sweep lease release failed");
            }
            result
        }
        .await;
        match result {
            Ok(report) => {
                if report.erased + report.failed > 0 {
                    tracing::info!(
                        erased = report.erased,
                        failed = report.failed,
                        "account_reaper completed"
                    );
                }
            }
            Err(e) => tracing::error!(error = %e, "account_reaper tick failed"),
        }
        compio::time::sleep(Duration::from_secs(INTERVAL_SECS)).await;
    }
}

/// Run one erasure pass. Finds every user past their grace window and erases
/// each in its OWN transaction (a fault on one user must not roll back the
/// others' completed erasures).
///
/// # Errors
///
/// Returns `AuthError::Db` if the due-scan itself fails. A per-user failure is
/// RECORDED (see [`record_failure`]) and skipped, so one poison row doesn't
/// stall the queue; the next tick retries it.
#[doc(hidden)]
pub async fn tick(db: &mut Client, control: &ControlAccess) -> Result<ReaperReport> {
    let due = find_due(db).await?;
    let mut report = ReaperReport::default();
    for user_id in due {
        // The preflight is asked OUTSIDE the transaction and BEFORE it, so a
        // control plane that cannot answer costs a skipped user rather than an
        // open transaction held across a network round trip.
        match still_erasable(control, &user_id).await {
            Ok(()) => {}
            Err(refusal) => {
                report.failed += 1;
                record_failure(db, &user_id, refusal.stage, &refusal.reason, None).await;
                continue;
            }
        }
        match erase_one(db, &user_id).await {
            Ok(EraseOutcome::Erased) => report.erased += 1,
            Ok(EraseOutcome::Skipped) => {}
            Ok(EraseOutcome::Refused(refusal)) => {
                report.failed += 1;
                record_failure(db, &user_id, refusal.stage, &refusal.reason, None).await;
            }
            Err(e) => {
                report.failed += 1;
                let (stage, constraint) = classify(&e);
                record_failure(db, &user_id, stage, &e.to_string(), constraint).await;
            }
        }
    }
    Ok(report)
}

/// One refusal to erase, with the stage an operator selects on and the reason a
/// person reads. Both go into the audit row and the error log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Refusal {
    stage: &'static str,
    reason: String,
}

/// Re-ask the control plane whether this human can still be erased. Both rules
/// are re-asked: whether they are the sole owner of a live organization, and
/// whether an organization whose last owner seat is theirs still owes.
///
/// Either can appear DURING the grace window and each appears a different way.
/// Ownership changes because somebody acted; a debt appears because the billing
/// sweep finalized last month's invoice, which needs nobody to act at all and
/// is therefore the arm a request-time check cannot stand in for.
///
/// An UNANSWERABLE preflight stops the delete too.
#[allow(clippy::future_not_send)]
async fn still_erasable(
    control: &ControlAccess,
    user_id: &UserId,
) -> std::result::Result<(), Refusal> {
    match control_client::erasure_preflight(&control.control_url, &control.keyring, user_id).await {
        Ok(report) if report.is_clear() => Ok(()),
        Ok(report) => Err(classify_blockers(&report)),
        Err(PreflightError::NoCredential(detail)) => Err(Refusal {
            stage: "preflight",
            reason: format!("erasure cannot be verified: {detail}"),
        }),
        Err(e) => Err(Refusal {
            stage: "preflight",
            reason: e.to_string(),
        }),
    }
}

/// Turn a non-clear report into the one refusal recorded for this user.
///
/// The reason names EVERY blocker of both kinds, because the person has to
/// clear all of them and a record naming one would send them round again. The
/// STAGE is `billing` whenever any organization owes, so that selecting on it
/// finds every money-held erasure; an ownership blocker beside a debt does not
/// hide the debt from that query.
fn classify_blockers(report: &control_client::ErasurePreflight) -> Refusal {
    let mut parts = Vec::new();
    if !report.blockers.is_empty() {
        let named = report
            .blockers
            .iter()
            .map(|b| b.organization_slug.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("sole owner of live organizations: {named}"));
    }
    if !report.billing_blockers.is_empty() {
        let named = report
            .billing_blockers
            .iter()
            .map(|b| {
                format!(
                    "{} (owed {} {}, {} unpaid invoice(s), {} uninvoiced period(s))",
                    b.organization_slug,
                    b.owed_cents,
                    b.currency,
                    b.unpaid_invoice_count,
                    b.unbilled_period_count
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("outstanding billing on: {named}"));
    }
    Refusal {
        stage: if report.billing_blockers.is_empty() {
            "preflight"
        } else {
            "billing"
        },
        // A report that is not clear has at least one blocker, so `parts` is
        // never empty on this path. The fallback keeps an empty reason out of
        // the audit row if a future arm makes `is_clear` false some other way.
        reason: if parts.is_empty() {
            "erasure refused by the control plane".to_string()
        } else {
            parts.join("; ")
        },
    }
}

/// Users whose erasure is due: an explicit request and a schedule in the past.
/// A schedule alone denies authentication but is not authority to erase the
/// account.
async fn find_due(db: &Client) -> Result<Vec<UserId>> {
    let rows = db
        .query(
            "SELECT id FROM zeroship.users \
             WHERE deletion_requested_at IS NOT NULL \
               AND deletion_scheduled_for IS NOT NULL \
               AND deletion_scheduled_for <= NOW()",
            &[],
        )
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper find_due: {e}")))?;
    rows.iter()
        .map(|row| {
            crate::entity_ids::user_id_with_context(row, "id", "account reaper user_id is invalid")
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EraseOutcome {
    Erased,
    Skipped,
    /// The transaction decided, under the organization row lock, that this
    /// erasure would strand an organization. The audit row is written by the
    /// caller, after the rollback, for the reason [`record_failure`] gives.
    Refused(Refusal),
}

/// Erase one due user inside a single transaction.
async fn erase_one(conn: &mut Client, user_id: &UserId) -> Result<EraseOutcome> {
    let tx = conn
        .transaction()
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper begin: {e}")))?;
    let result = erase_one_tx(&tx, user_id).await;
    match result {
        Ok(outcome @ (EraseOutcome::Skipped | EraseOutcome::Refused(_))) => {
            tx.rollback()
                .await
                .map_err(|e| AuthError::Db(format!("account_reaper rollback skip: {e}")))?;
            Ok(outcome)
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

async fn erase_one_tx(
    conn: &(impl GenericClient + Sync),
    user_id: &UserId,
) -> Result<EraseOutcome> {
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
             FOR UPDATE",
            &[&user_id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper lock due user: {e}")))?;
    if due.is_empty() {
        return Ok(EraseOutcome::Skipped);
    }
    if let Some(refusal) = refuse_if_it_strands_an_organization(conn, user_id).await? {
        return Ok(EraseOutcome::Refused(refusal));
    }
    hard_delete_user(conn, user_id).await?;
    Ok(EraseOutcome::Erased)
}

/// Decide the ownership rule where it can be enforced: inside the erasure
/// transaction, under the same `zeroship.organizations` row lock every other
/// membership mutation takes.
///
/// # Two statements, and the order is the mechanism
///
/// The first LOCKS every live organization this human holds ANY seat on. The
/// second re-reads the rule, and it is the one that asks about ownership. They
/// are separate statements deliberately: in
/// READ COMMITTED each statement takes its own snapshot, so the second one sees
/// whatever committed while the first was blocked on a concurrent departure.
/// Folding them into one statement puts the decisive `NOT EXISTS` in the
/// snapshot the lock was taken FROM, which is the snapshot that is out of date.
///
/// A departure that arrives the other way round loses the same way: it takes
/// this lock in `leave_organization`, waits for this transaction, and then
/// evaluates its own `owners > 1` predicate against a database in which the
/// erased seat is gone - so it refuses instead. Neither order produces an
/// ownerless organization.
///
/// # Why the locking statement asks about SEATS and the re-check asks about OWNERSHIP
///
/// The set this transaction must serialize against is not "organizations this
/// human owns" but "organizations this human could be the last owner of by the
/// time the DELETE runs", and those differ, because ownership is reachable by
/// PROMOTION. `transfer_ownership` in the control plane raises a sitting member
/// to owner with a plain `UPDATE organization_members SET role`, and a
/// referencing-side RI trigger fires only when the key columns change - so that
/// UPDATE takes NO lock on `zeroship.users`, and holding this human's row
/// `FOR UPDATE` does not serialize it. Locking only the owner seats therefore
/// leaves every organization where this human is currently a developer
/// unlocked, the promotion commits underneath the erasure, and the seat is
/// cascaded away: an ownerless organization, by the one route the earlier
/// argument here claimed was closed.
///
/// So the lock is taken over every seat, and `role = 'owner'` is asked only in
/// the re-check, which runs in a later snapshot and therefore sees a promotion
/// that committed while the lock was being waited for.
///
/// A brand-new seat is a different case and IS closed by the users-row lock: an
/// INSERT into `organization_members` takes `FOR KEY SHARE` on the referenced
/// `zeroship.users` row for its foreign key, and the caller already holds
/// `FOR UPDATE` on exactly that row. Promotion had to be handled here because
/// it is the one way in which the set grows WITHOUT touching that row.
///
/// # Deterministic lock order
///
/// `ORDER BY o.id` with `FOR UPDATE` locks in sorted order - PostgreSQL puts
/// `LockRows` above `Sort` - so two reapers erasing two co-owners walk the
/// shared organizations in the same direction and one waits rather than
/// deadlocking.
///
/// # Scope
///
/// This is the OWNERSHIP rule only, and only for LIVE organizations - the rule
/// whose violation no route repairs. The money rule stays with the control
/// plane, which owns the billing tables and the remedies a person is shown.
async fn refuse_if_it_strands_an_organization(
    conn: &(impl GenericClient + Sync),
    user_id: &UserId,
) -> Result<Option<Refusal>> {
    conn.query(
        "SELECT o.id FROM zeroship.organizations o \
          WHERE o.dissolved_at IS NULL \
            AND EXISTS (SELECT 1 FROM zeroship.organization_members m \
                         WHERE m.organization_id = o.id \
                           AND m.user_id = $1) \
          ORDER BY o.id \
          FOR UPDATE",
        &[&user_id.as_str()],
    )
    .await
    .map_err(|e| AuthError::Db(format!("account_reaper lock seated organizations: {e}")))?;

    let stranded = conn
        .query(
            "SELECT o.slug::text AS slug FROM zeroship.organizations o \
              WHERE o.dissolved_at IS NULL \
                AND EXISTS (SELECT 1 FROM zeroship.organization_members m \
                             WHERE m.organization_id = o.id \
                               AND m.user_id = $1 \
                               AND m.role = 'owner') \
                AND NOT EXISTS (SELECT 1 FROM zeroship.organization_members rival \
                                 WHERE rival.organization_id = o.id \
                                   AND rival.role = 'owner' \
                                   AND rival.user_id <> $1) \
              ORDER BY o.slug",
            &[&user_id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("account_reaper recheck ownership: {e}")))?;
    if stranded.is_empty() {
        return Ok(None);
    }
    let named = stranded
        .iter()
        .map(|row| row.get::<_, String>("slug"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Some(Refusal {
        stage: "ownership",
        reason: format!(
            "erasure would leave these organizations without an owner: {named}; \
             the preflight cleared before the last co-owner departed"
        ),
    }))
}

/// Hard-delete the `users` row. Every FK into `users(id)` is CASCADE or SET
/// NULL, so PostgreSQL removes or clears each dependent itself - and it does so
/// with the CONSTRAINT OWNER's privileges, which is why this works on tables
/// `zeroship_auth` cannot write directly.
///
/// The SQLSTATE is preserved rather than flattened into a message: `23503` and
/// `23514` are what a newly-added blocking reference looks like, and the
/// constraint name in them is the only thing that says which one.
async fn hard_delete_user(conn: &(impl GenericClient + Sync), user_id: &UserId) -> Result<()> {
    conn.execute(
        "DELETE FROM zeroship.users WHERE id = $1",
        &[&user_id.as_str()],
    )
    .await
    .map_err(|e| {
        if let Some(db_err) = e.as_db_error() {
            let constraint = db_err.constraint().unwrap_or("<unnamed>");
            let table = db_err.table().unwrap_or("<unknown>");
            return AuthError::DbCode {
                code: db_err.code().code().to_string(),
                message: format!("account_reaper hard delete blocked by {table}.{constraint}: {e}"),
            };
        }
        AuthError::Db(format!("account_reaper hard delete: {e}"))
    })?;
    Ok(())
}

/// Split an erasure error into the stage it names and the constraint it names,
/// so the audit row says which reference blocked rather than only that one did.
fn classify(err: &AuthError) -> (&'static str, Option<String>) {
    match err {
        AuthError::DbCode { code, message } if code == "23503" || code == "23514" => {
            let constraint = message
                .split("blocked by ")
                .nth(1)
                .and_then(|rest| rest.split(':').next())
                .map(str::to_owned);
            ("constraint", constraint)
        }
        AuthError::DbCode { .. } => ("database", None),
        _ => ("database", None),
    }
}

/// Record ONE user's failed erasure where it survives the process: an audit row
/// naming the user, the stage and the constraint, plus a structured error log.
///
/// The audit row is written on the connection AFTER the erasure transaction has
/// rolled back. Inside it, the row would roll back with the failure it exists to
/// describe.
#[allow(clippy::future_not_send)]
async fn record_failure(
    db: &Client,
    user_id: &UserId,
    stage: &str,
    reason: &str,
    constraint: Option<String>,
) {
    tracing::error!(
        user_id = user_id.as_str(),
        stage,
        constraint = constraint.as_deref().unwrap_or("-"),
        reason,
        "account_reaper: erasure failed; the account remains pending"
    );
    audit::emit(
        db,
        &AuditEvent {
            event_type: "account_erasure_failed",
            outcome: "failure",
            user_id: Some(user_id),
            client_id: None,
            request_id: None,
            ip: None,
            user_agent: None,
            auth_method: None,
            detail: json!({
                "stage": stage,
                "constraint": constraint,
                "reason": reason,
            }),
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_window_is_thirty_days() {
        assert_eq!(GRACE_DAYS, 30);
    }

    /// A blocked DELETE has to reach the audit row naming the CONSTRAINT, not
    /// just "a database error". This is the parse that carries it, and it is
    /// bound to the exact message `hard_delete_user` builds.
    #[test]
    fn a_foreign_key_refusal_names_its_constraint() {
        let err = AuthError::DbCode {
            code: "23503".into(),
            message: "account_reaper hard delete blocked by zeroship.widgets.widgets_owner_fkey: \
                      db error: ERROR: update or delete on table \"users\" violates foreign key"
                .into(),
        };
        let (stage, constraint) = classify(&err);
        assert_eq!(stage, "constraint");
        assert_eq!(
            constraint.as_deref(),
            Some("zeroship.widgets.widgets_owner_fkey")
        );
    }

    #[test]
    fn a_check_violation_is_classified_the_same_way() {
        let err = AuthError::DbCode {
            code: "23514".into(),
            message: "account_reaper hard delete blocked by zeroship.seats.seats_rank_check: x"
                .into(),
        };
        assert_eq!(classify(&err).0, "constraint");
    }

    /// The control differing in one variable: a NON-constraint SQLSTATE takes
    /// the other arm, so "constraint" in an audit row means a constraint really
    /// was named.
    fn report(blockers: &str, billing: &str) -> control_client::ErasurePreflight {
        serde_json::from_str(&format!(
            "{{\"blockers\":[{blockers}],\"billing_blockers\":[{billing}]}}"
        ))
        .expect("parse")
    }

    const OWNERSHIP: &str = r#"{"organization_id":"org_1","organization_slug":"solo",
        "organization_name":"Solo","personal":true,"other_member_count":0,
        "project_count":0,"remedy":"dissolve"}"#;
    const OWES: &str = r#"{"organization_id":"org_2","organization_slug":"indebted",
        "organization_name":"Indebted","personal":false,"dissolved":true,
        "owed_cents":1250,"currency":"usd","unpaid_invoice_count":1,
        "unbilled_period_count":0,"remedy":"settle_invoices"}"#;

    /// A money refusal has to be findable by an operator, so it carries its own
    /// stage rather than sharing the ownership rule's. Selecting
    /// `detail->>'stage' = 'billing'` is how "whose erasure is money holding
    /// up" is answered, and this is what makes that query complete.
    #[test]
    fn an_owing_organization_is_recorded_under_its_own_stage() {
        let refusal = classify_blockers(&report("", OWES));
        assert_eq!(refusal.stage, "billing");
        assert!(refusal.reason.contains("indebted"), "{refusal:?}");
        assert!(refusal.reason.contains("1250"), "{refusal:?}");
    }

    /// The ownership rule keeps the stage it had. The pair is what makes
    /// "billing" mean money rather than "any refusal".
    #[test]
    fn a_sole_owner_refusal_keeps_the_preflight_stage() {
        let refusal = classify_blockers(&report(OWNERSHIP, ""));
        assert_eq!(refusal.stage, "preflight");
        assert!(refusal.reason.contains("solo"), "{refusal:?}");
    }

    /// Both at once: the reason names BOTH, so the person is not sent round
    /// twice, and the stage stays `billing`, so the debt is not hidden from
    /// the operator query by an ownership blocker standing beside it.
    #[test]
    fn both_rules_at_once_name_both_and_stay_findable_as_billing() {
        let refusal = classify_blockers(&report(OWNERSHIP, OWES));
        assert_eq!(refusal.stage, "billing");
        assert!(refusal.reason.contains("solo"), "{refusal:?}");
        assert!(refusal.reason.contains("indebted"), "{refusal:?}");
    }

    #[test]
    fn another_sqlstate_is_not_reported_as_a_constraint_failure() {
        let err = AuthError::DbCode {
            code: "42501".into(),
            message: "account_reaper hard delete: permission denied for table users".into(),
        };
        assert_eq!(classify(&err), ("database", None));
        let plain = AuthError::Db("account_reaper begin: connection closed".into());
        assert_eq!(classify(&plain), ("database", None));
    }
}
