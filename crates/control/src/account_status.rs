//! Creator payment/account status — the billing G2 dunning state machine.
//!
//! The spend engine (`spend.rs`, changeset 0039) caps USAGE within a paid
//! relationship. This module owns the orthogonal ACCOUNT-level gate: a
//! creator whose infra invoice cannot be charged would otherwise accrue
//! unbounded cost. We drive a failed-payment lifecycle off Stripe webhook
//! truth and persist a per-creator [`AccountState`] in
//! `zeroship.creator_billing_status`; the gateway pulls it onto each of the
//! creator's apps' [`RouteEntry`](zeroship_core::types::RouteEntry) (via the
//! owner join in `registry.get_routes`) and gates dispatch on it.
//!
//! ```text
//!   active ──invoice.payment_failed──► past_due ──dunning exhausted──► suspended
//!      ▲                                   │                                │
//!      └──────── invoice.paid ─────────────┴────────── invoice.paid ────────┘
//! ```
//!
//! * **active** — payment current; served (subject to spend).
//! * **past_due** — ≥1 invoice failed and Stripe's own retries are running; the
//!   customer-favourable GRACE window — STILL SERVED (the warning state).
//! * **suspended** — the dunning window (`max_dunning_days`, default 7) elapsed;
//!   the gateway 402s. REVERSIBLE: a later `invoice.paid` flips straight back to
//!   active. Suspension is never a one-way trap.
//!
//! **Webhook-truth-only + reversible + order-safe** is the false-suspend guard:
//! only a signature-verified Stripe event (the webhook caller verifies the
//! signature BEFORE invoking these methods) ever moves the state; a recovered
//! payment always un-suspends; and a STALE `payment_failed` (one whose Stripe
//! `event.created` predates the creator's last recovery) is IGNORED so an
//! out-of-order/redelivered failure can never re-arm `past_due` on an
//! already-paying creator (critic #1 — `last_recovered_at` high-water). No
//! creator input sets it.

use uuid::Uuid;

use zeroship_core::types::AccountState;

use crate::registry::Registry;
use crate::stripe_store::StripeError;

/// Default dunning window: a `past_due` creator whose oldest unpaid invoice has
/// been failing longer than this is suspended. We leave Stripe's own retry
/// schedule running (we do NOT mark the invoice uncollectible), so this is an
/// upper bound on how long the platform eats infra cost on a dead card.
pub const DEFAULT_MAX_DUNNING_DAYS: i64 = 7;

/// TEXT ↔ [`AccountState`] mapping for the `state` column. Matches the
/// `#[serde(rename_all = "snake_case")]` wire form so the DB value and the
/// `RouteEntry` JSON agree.
#[must_use]
pub fn account_state_str(state: AccountState) -> &'static str {
    match state {
        AccountState::Active => "active",
        AccountState::PastDue => "past_due",
        AccountState::Suspended => "suspended",
    }
}

/// Parse the `state` TEXT column. An unrecognised value fails CLOSED to
/// `Suspended` (defensive — the writer only ever persists the three known
/// states, guarded by a CHECK constraint). Failing closed here means a
/// corrupt/unknown value blocks rather than silently serves free infra.
#[must_use]
pub fn parse_account_state(s: &str) -> AccountState {
    match s {
        "active" => AccountState::Active,
        "past_due" => AccountState::PastDue,
        _ => AccountState::Suspended,
    }
}

/// One account-state transition the cron/webhook performed — returned so callers
/// can audit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountTransition {
    pub creator_id: Uuid,
    pub from: AccountState,
    pub to: AccountState,
    pub reason: &'static str,
}

#[allow(missing_debug_implementations)]
pub struct AccountStatusStore {
    registry: Registry,
}

impl AccountStatusStore {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Current state for a creator. `None` ⇒ no row ⇒ treated as `Active` by
    /// the gateway (the common free/cardless case).
    pub async fn get_state(&self, creator_id: Uuid) -> Result<Option<AccountState>, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT state FROM zeroship.creator_billing_status WHERE creator_id = $1",
                &[&creator_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(rows
            .first()
            .map(|r| parse_account_state(&r.get::<_, String>("state"))))
    }

    /// `invoice.payment_failed` webhook → move the creator to `past_due`.
    ///
    /// **Order-safe (critic #1).** `event_created` is the Stripe `event.created`
    /// of THIS failure. Stripe can redeliver / reorder webhooks, so a stale
    /// `payment_failed` can land AFTER an `invoice.paid` recovery. We persist the
    /// `event.created` of the last recovery in `last_recovered_at`; a failure
    /// whose `event_created <= last_recovered_at` is STALE (the creator already
    /// recovered after it was emitted) and is IGNORED — it must never re-arm
    /// `past_due` on an already-paying creator. This is the primary false-suspend
    /// guard.
    ///
    /// Otherwise: a re-delivered/repeat failure does NOT reset the dunning clock
    /// (`past_due_since`) — it only refreshes `last_payment_failure_at`. A NEW
    /// failed invoice starts a fresh window only if the creator was `active`. A
    /// `suspended` creator stays `suspended` (the failure is consistent with it).
    ///
    /// Returns the transition iff the state actually changed (active→past_due).
    pub async fn record_payment_failed(
        &self,
        creator_id: Uuid,
        failed_invoice_id: Option<&str>,
        event_created: i64,
    ) -> Result<Option<AccountTransition>, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        // UPSERT with a guarded transition. A `prior` CTE snapshots the
        // pre-write state (the INSERT…ON CONFLICT can't see its own old row in
        // RETURNING), so we can tell a genuine active→past_due edge from a no-op.
        //
        // ORDER-SAFETY: `evt` is THIS event's `event.created` (passed as a unix
        // timestamp, converted to TIMESTAMPTZ). The ON CONFLICT branch is GATED:
        // when `evt <= last_recovered_at`, the row already saw a recovery emitted
        // AFTER this failure ⇒ this is a stale/redelivered failure and we leave
        // EVERY column unchanged (state, past_due_since, last_*). It can never
        // re-arm past_due on a recovered creator. For the no-row INSERT path
        // there is no prior recovery, so a first-seen failure always arms.
        //
        // Otherwise:
        //   * active → past_due, set past_due_since = NOW() (start the window).
        //   * already past_due → keep past_due_since (idempotent — do NOT restart
        //     the clock on redelivery or subsequent failures in the same window).
        //   * suspended → stays suspended (clock already elapsed).
        let rows = conn
            .query(
                "WITH prior AS ( \
                    SELECT state FROM zeroship.creator_billing_status WHERE creator_id = $1 \
                 ), upserted AS ( \
                    INSERT INTO zeroship.creator_billing_status \
                        (creator_id, state, past_due_since, last_payment_failure_at, \
                         failed_invoice_id, last_event_at, updated_at) \
                     VALUES ($1, 'past_due', NOW(), NOW(), $2, to_timestamp($3::bigint), NOW()) \
                     ON CONFLICT (creator_id) DO UPDATE SET \
                        state = CASE \
                            WHEN zeroship.creator_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.creator_billing_status.last_recovered_at \
                                 THEN zeroship.creator_billing_status.state \
                            WHEN zeroship.creator_billing_status.state = 'active' THEN 'past_due' \
                            ELSE zeroship.creator_billing_status.state END, \
                        past_due_since = CASE \
                            WHEN zeroship.creator_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.creator_billing_status.last_recovered_at \
                                 THEN zeroship.creator_billing_status.past_due_since \
                            WHEN zeroship.creator_billing_status.state = 'active' THEN NOW() \
                            ELSE zeroship.creator_billing_status.past_due_since END, \
                        last_payment_failure_at = CASE \
                            WHEN zeroship.creator_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.creator_billing_status.last_recovered_at \
                                 THEN zeroship.creator_billing_status.last_payment_failure_at \
                            ELSE NOW() END, \
                        failed_invoice_id = CASE \
                            WHEN zeroship.creator_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.creator_billing_status.last_recovered_at \
                                 THEN zeroship.creator_billing_status.failed_invoice_id \
                            ELSE $2 END, \
                        last_event_at = GREATEST( \
                            zeroship.creator_billing_status.last_event_at, to_timestamp($3::bigint)), \
                        updated_at = NOW() \
                     RETURNING state \
                 ) \
                 SELECT (SELECT state FROM prior) AS prior_state, \
                        (SELECT state FROM upserted) AS new_state",
                &[&creator_id, &failed_invoice_id, &event_created],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(self.transition_from_rows(creator_id, &rows, "payment_failed").await?)
    }

    /// `invoice.paid` (infra invoice) / payment recovery → move the creator back
    /// to `active`, clearing the dunning window. This is the REVERSIBILITY rail:
    /// it un-suspends a `suspended` creator (suspension is never permanent) and
    /// clears `past_due`. Idempotent: an already-`active` creator is a no-op.
    ///
    /// **Order-safe (critic #1).** `event_created` (the Stripe `event.created` of
    /// this recovery) is stamped into `last_recovered_at` (advanced monotonically
    /// via `GREATEST`). A later-but-stale `payment_failed` whose `event.created`
    /// predates this value is then ignored by [`Self::record_payment_failed`], so
    /// a recovery can never be undone by an out-of-order failure.
    ///
    /// Returns the transition iff the state actually changed.
    pub async fn record_payment_recovered(
        &self,
        creator_id: Uuid,
        event_created: i64,
    ) -> Result<Option<AccountTransition>, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        // Only existing rows can recover (no row ⇒ already effectively active —
        // a payment success for a creator with no prior failure needs no state).
        // A `prior` CTE snapshots the pre-UPDATE state so we can tell a real
        // {past_due,suspended}→active edge from a no-op (already active). The
        // UPDATE clears the window + suspension (REVERSIBILITY: un-suspends) AND
        // advances `last_recovered_at`/`last_event_at` to this event's
        // `event.created` (monotonic via GREATEST) so a stale later failure is
        // gated out. We stamp the recovery timestamp EVEN on an already-active
        // no-op so the ordering high-water still advances.
        let rows = conn
            .query(
                "WITH prior AS ( \
                    SELECT state FROM zeroship.creator_billing_status WHERE creator_id = $1 \
                 ), updated AS ( \
                    UPDATE zeroship.creator_billing_status SET \
                        state = 'active', \
                        past_due_since = NULL, \
                        suspended_at = NULL, \
                        failed_invoice_id = NULL, \
                        last_recovered_at = GREATEST(last_recovered_at, to_timestamp($2::bigint)), \
                        last_event_at = GREATEST(last_event_at, to_timestamp($2::bigint)), \
                        updated_at = NOW() \
                     WHERE creator_id = $1 \
                     RETURNING state \
                 ) \
                 SELECT (SELECT state FROM prior) AS prior_state, \
                        (SELECT state FROM updated) AS new_state",
                &[&creator_id, &event_created],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(self.transition_from_rows(creator_id, &rows, "payment_recovered").await?)
    }

    /// Append a history row + return the transition iff `prior != new`.
    async fn transition_from_rows(
        &self,
        creator_id: Uuid,
        rows: &[compio_postgres::Row],
        reason: &'static str,
    ) -> Result<Option<AccountTransition>, StripeError> {
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let prior: Option<String> = row.get("prior_state");
        // `new_state` is NULL when the write affected no row (e.g. recovery for a
        // creator with no status row) ⇒ nothing changed.
        let Some(new) = row.get::<_, Option<String>>("new_state") else {
            return Ok(None);
        };
        let from = prior
            .as_deref()
            .map_or(AccountState::Active, parse_account_state);
        let to = parse_account_state(&new);
        if from == to {
            return Ok(None);
        }
        self.append_history(creator_id, from, to, reason).await?;
        Ok(Some(AccountTransition {
            creator_id,
            from,
            to,
            reason,
        }))
    }

    async fn append_history(
        &self,
        creator_id: Uuid,
        from: AccountState,
        to: AccountState,
        reason: &str,
    ) -> Result<(), StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        conn.execute(
            "INSERT INTO zeroship.creator_billing_status_history \
                (creator_id, from_state, to_state, reason) VALUES ($1, $2, $3, $4)",
            &[
                &creator_id,
                &account_state_str(from),
                &account_state_str(to),
                &reason,
            ],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(())
    }

    /// Dunning sweep: suspend every `past_due` creator whose dunning window has
    /// elapsed (`NOW() - past_due_since > max_dunning_days`). Returns the
    /// transitions performed so the cron can audit them.
    ///
    /// The time comparison runs IN Postgres so it is consistent fleet-wide and
    /// uses the partial index on `past_due` rows. We do NOT touch Stripe here —
    /// Stripe's own retries keep running; this is purely the platform's
    /// stop-eating-infra-cost deadline.
    pub async fn suspend_exhausted(
        &self,
        max_dunning_days: i64,
    ) -> Result<Vec<AccountTransition>, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        // `make_interval(days => $1)` keeps the threshold parameterised (no
        // string-built interval). RETURNING the creator_id of each newly
        // suspended row drives the audit.
        let rows = conn
            .query(
                "UPDATE zeroship.creator_billing_status SET \
                    state = 'suspended', \
                    suspended_at = NOW(), \
                    updated_at = NOW() \
                 WHERE state = 'past_due' \
                   AND past_due_since IS NOT NULL \
                   AND NOW() - past_due_since > make_interval(days => $1::int) \
                 RETURNING creator_id",
                &[&(max_dunning_days as i32)],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let mut transitions = Vec::with_capacity(rows.len());
        for row in &rows {
            let creator_id: Uuid = row.get("creator_id");
            self.append_history(
                creator_id,
                AccountState::PastDue,
                AccountState::Suspended,
                "dunning_exhausted",
            )
            .await?;
            transitions.push(AccountTransition {
                creator_id,
                from: AccountState::PastDue,
                to: AccountState::Suspended,
                reason: "dunning_exhausted",
            });
        }
        Ok(transitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_str_roundtrips() {
        for s in [AccountState::Active, AccountState::PastDue, AccountState::Suspended] {
            assert_eq!(parse_account_state(account_state_str(s)), s);
        }
    }

    #[test]
    fn unknown_state_fails_closed_to_suspended() {
        // Defensive: a corrupt/unknown TEXT value must block, never serve free
        // infra. (Mirrors spend's fail-closed-to-Block.)
        assert_eq!(parse_account_state("garbage"), AccountState::Suspended);
        assert_eq!(parse_account_state(""), AccountState::Suspended);
    }

    #[test]
    fn default_dunning_window_is_seven_days() {
        assert_eq!(DEFAULT_MAX_DUNNING_DAYS, 7);
    }
}
