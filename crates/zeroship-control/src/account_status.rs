//! Organization payment/account status — the billing G2 dunning state machine.
//!
//! The spend engine (`spend.rs`, changeset 0039) caps USAGE within a paid
//! relationship. This module owns the orthogonal ACCOUNT-level gate: a
//! organization whose infra invoice cannot be charged would otherwise accrue
//! unbounded cost. We drive a failed-payment lifecycle off Stripe webhook
//! truth and persist a per-organization [`AccountState`] in
//! `zeroship.organization_billing_status`; the gateway pulls it onto each of the
//! organization's apps' [`RouteEntry`](zeroship_core::types::RouteEntry) (via the
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
//! `event.created` predates the organization's last recovery) is IGNORED so an
//! out-of-order/redelivered failure can never re-arm `past_due` on an
//! already-paying organization (critic #1 — `last_recovered_at` high-water). No
//! organization input sets it.


use zeroship_core::types::AccountState;

use crate::registry::Registry;
use crate::stripe_store::StripeError;

/// Default dunning window: a `past_due` organization whose oldest unpaid invoice has
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
    pub organization_id: String,
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

    /// Current state for an organization. `None` ⇒ no row ⇒ treated as `Active` by
    /// the gateway (the common free/cardless case).
    pub async fn get_state(&self, organization_id: &str) -> Result<Option<AccountState>, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        let rows = conn
            .query(
                "SELECT state FROM zeroship.organization_billing_status WHERE organization_id = $1",
                &[&organization_id],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(rows
            .first()
            .map(|r| parse_account_state(&r.get::<_, String>("state"))))
    }

    /// `invoice.payment_failed` webhook → move the organization to `past_due`.
    ///
    /// **Order-safe (critic #1).** `event_created` is the Stripe `event.created`
    /// of THIS failure. Stripe can redeliver / reorder webhooks, so a stale
    /// `payment_failed` can land AFTER an `invoice.paid` recovery. We persist the
    /// `event.created` of the last recovery in `last_recovered_at`; a failure
    /// whose `event_created <= last_recovered_at` is STALE (the organization already
    /// recovered after it was emitted) and is IGNORED — it must never re-arm
    /// `past_due` on an already-paying organization. This is the primary false-suspend
    /// guard.
    ///
    /// Otherwise: a re-delivered/repeat failure does NOT reset the dunning clock
    /// (`past_due_since`) — it only refreshes `last_payment_failure_at`. A NEW
    /// failed invoice starts a fresh window only if the organization was `active`. A
    /// `suspended` organization stays `suspended` (the failure is consistent with it).
    ///
    /// Returns the transition iff the state actually changed (active→past_due).
    pub async fn record_payment_failed(
        &self,
        organization_id: &str,
        failed_invoice_id: Option<&str>,
        event_created: i64,
    ) -> Result<Option<AccountTransition>, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        // PARENT-FIRST (schema redesign): `organization_billing_status.organization_id` now
        // FKs `organization_billing(organization_id)`, not `users` directly. A payment
        // failure can be the FIRST billing signal for an organization (no prior
        // `organization_billing` identity row — e.g. they never ran `billing/setup`),
        // so ensure the FK parent exists before the status UPSERT or the INSERT
        // FK-violates. Idempotent (ON CONFLICT DO NOTHING).
        conn.execute(
            "INSERT INTO zeroship.organization_billing (organization_id) \
             VALUES ($1) ON CONFLICT (organization_id) DO NOTHING",
            &[&organization_id],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
        // UPSERT with a guarded transition. A `prior` CTE snapshots the
        // pre-write state (the INSERT…ON CONFLICT can't see its own old row in
        // RETURNING), so we can tell a genuine active→past_due edge from a no-op.
        //
        // ORDER-SAFETY: `evt` is THIS event's `event.created` (passed as a unix
        // timestamp, converted to TIMESTAMPTZ). The ON CONFLICT branch is GATED:
        // when `evt <= last_recovered_at`, the row already saw a recovery emitted
        // AFTER this failure ⇒ this is a stale/redelivered failure and we leave
        // EVERY column unchanged (state, past_due_since, last_*). It can never
        // re-arm past_due on a recovered organization. For the no-row INSERT path
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
                    SELECT state FROM zeroship.organization_billing_status WHERE organization_id = $1 \
                 ), upserted AS ( \
                    INSERT INTO zeroship.organization_billing_status \
                        (organization_id, state, past_due_since, last_payment_failure_at, \
                         failed_invoice_id, last_event_at, updated_at) \
                     VALUES ($1, 'past_due', NOW(), NOW(), $2, to_timestamp($3::bigint), NOW()) \
                     ON CONFLICT (organization_id) DO UPDATE SET \
                        state = CASE \
                            WHEN zeroship.organization_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.organization_billing_status.last_recovered_at \
                                 THEN zeroship.organization_billing_status.state \
                            WHEN zeroship.organization_billing_status.state = 'active' THEN 'past_due' \
                            ELSE zeroship.organization_billing_status.state END, \
                        past_due_since = CASE \
                            WHEN zeroship.organization_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.organization_billing_status.last_recovered_at \
                                 THEN zeroship.organization_billing_status.past_due_since \
                            WHEN zeroship.organization_billing_status.state = 'active' THEN NOW() \
                            ELSE zeroship.organization_billing_status.past_due_since END, \
                        last_payment_failure_at = CASE \
                            WHEN zeroship.organization_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.organization_billing_status.last_recovered_at \
                                 THEN zeroship.organization_billing_status.last_payment_failure_at \
                            ELSE NOW() END, \
                        failed_invoice_id = CASE \
                            WHEN zeroship.organization_billing_status.last_recovered_at IS NOT NULL \
                                 AND to_timestamp($3::bigint) <= zeroship.organization_billing_status.last_recovered_at \
                                 THEN zeroship.organization_billing_status.failed_invoice_id \
                            ELSE $2 END, \
                        last_event_at = GREATEST( \
                            zeroship.organization_billing_status.last_event_at, to_timestamp($3::bigint)), \
                        updated_at = NOW() \
                     RETURNING state \
                 ) \
                 SELECT (SELECT state FROM prior) AS prior_state, \
                        (SELECT state FROM upserted) AS new_state",
                &[&organization_id, &failed_invoice_id, &event_created],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        self.transition_from_rows(organization_id, &rows, "payment_failed").await
    }

    /// `invoice.paid` (infra invoice) / payment recovery → move the organization back
    /// to `active`, clearing the dunning window. This is the REVERSIBILITY rail:
    /// it un-suspends a `suspended` organization (suspension is never permanent) and
    /// clears `past_due`. Idempotent: an already-`active` organization is a no-op.
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
        organization_id: &str,
        event_created: i64,
    ) -> Result<Option<AccountTransition>, StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        // Only existing rows can recover (no row ⇒ already effectively active —
        // a payment success for an organization with no prior failure needs no state).
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
                    SELECT state FROM zeroship.organization_billing_status WHERE organization_id = $1 \
                 ), updated AS ( \
                    UPDATE zeroship.organization_billing_status SET \
                        state = 'active', \
                        past_due_since = NULL, \
                        suspended_at = NULL, \
                        failed_invoice_id = NULL, \
                        last_recovered_at = GREATEST(last_recovered_at, to_timestamp($2::bigint)), \
                        last_event_at = GREATEST(last_event_at, to_timestamp($2::bigint)), \
                        updated_at = NOW() \
                     WHERE organization_id = $1 \
                     RETURNING state \
                 ) \
                 SELECT (SELECT state FROM prior) AS prior_state, \
                        (SELECT state FROM updated) AS new_state",
                &[&organization_id, &event_created],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        self.transition_from_rows(organization_id, &rows, "payment_recovered").await
    }

    /// Append a history row + return the transition iff `prior != new`.
    async fn transition_from_rows(
        &self,
        organization_id: &str,
        rows: &[compio_postgres::Row],
        reason: &'static str,
    ) -> Result<Option<AccountTransition>, StripeError> {
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let prior: Option<String> = row.get("prior_state");
        // `new_state` is NULL when the write affected no row (e.g. recovery for a
        // organization with no status row) ⇒ nothing changed.
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
        self.append_history(organization_id, from, to, reason).await?;
        Ok(Some(AccountTransition {
            organization_id: organization_id.to_owned(),
            from,
            to,
            reason,
        }))
    }

    async fn append_history(
        &self,
        organization_id: &str,
        from: AccountState,
        to: AccountState,
        reason: &str,
    ) -> Result<(), StripeError> {
        let conn = self
            .registry
            .conn()
            .await
            .map_err(|e| StripeError::Db(format!("{e}")))?;
        // `id` is the PR-6 surrogate PK (`obh_<base62>`) = the
        // `billing_notifications.transition_id` for the dunning-driven notification kinds
        // (payment_failed/past_due/suspended/recovered). Minted in Rust here (no SQL
        // DEFAULT — the disjoint prefix the notify dedup relies on cannot come from
        // `gen_random_uuid()`). `from_state`/`to_state` are the `account_state` domain —
        // bind `::text` (the domain param OID rejects a bare &str, same as
        // billing_period/DATE).
        let history_id = zeroship_core::typed_id::new_organization_billing_history_id();
        conn.execute(
            "INSERT INTO zeroship.organization_billing_status_history \
                (id, organization_id, from_state, to_state, reason) \
             VALUES ($1, $2, $3::text, $4::text, $5)",
            &[
                &history_id,
                &organization_id,
                &account_state_str(from),
                &account_state_str(to),
                &reason,
            ],
        )
        .await
        .map_err(|e| StripeError::Db(e.to_string()))?;
        Ok(())
    }

    /// Dunning sweep: suspend every `past_due` organization whose dunning window has
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
        // string-built interval). RETURNING the organization_id of each newly
        // suspended row drives the audit.
        let rows = conn
            .query(
                "UPDATE zeroship.organization_billing_status SET \
                    state = 'suspended', \
                    suspended_at = NOW(), \
                    updated_at = NOW() \
                 WHERE state = 'past_due' \
                   AND past_due_since IS NOT NULL \
                   AND NOW() - past_due_since > make_interval(days => $1::int) \
                 RETURNING organization_id",
                &[&(max_dunning_days as i32)],
            )
            .await
            .map_err(|e| StripeError::Db(e.to_string()))?;
        let mut transitions = Vec::with_capacity(rows.len());
        for row in &rows {
            let organization_id: String = row.get("organization_id");
            self.append_history(
                &organization_id,
                AccountState::PastDue,
                AccountState::Suspended,
                "dunning_exhausted",
            )
            .await?;
            transitions.push(AccountTransition {
                organization_id,
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
