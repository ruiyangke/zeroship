//! Stripe state-reconciliation cron (#28 "Stripe reconciliation").
//!
//! The production BACKSTOP for missed / dropped / out-of-order Stripe webhooks. The
//! webhook handlers (`stripe_handlers`, `disputes`, `refund`) are the PRIMARY path;
//! Stripe guarantees only at-least-once, UNORDERED delivery, so a dropped
//! `invoice.paid` / `charge.refund.updated` / `charge.dispute.created` leaves OUR billing
//! state silently diverged from Stripe's. This cron periodically re-reads Stripe over a
//! BOUNDED recent window and records the drift it finds.
//!
//! READ-ONLY w.r.t. money by DEFAULT. It DETECTS + RECORDS + ALERTS — it does NOT
//! auto-correct cash on a transient Stripe read (an operator reviews each finding). The
//! single conservatively-safe exception is the dispute backstop: a fully-missed dispute
//! whose settling pi_/ch_ ALREADY links to one of our invoices is APPLIED directly
//! (`billing_disputes` row + its `dispute_debit`, idempotent on the du_…, under the
//! per-creator advisory lock) — gated behind a config flag defaulting OFF (FLAG by default).
//! We never mutate a finalized invoice, never issue a refund.
//!
//! Three reconcile passes over the recent window (mirroring the webhook events they back
//! up):
//!   1. INVOICES — for each recently-finalized invoice with a `billing_provider_refs`
//!      `in_…`, `GET /v1/invoices/{in_}` and compare Stripe's `status`/`amount_paid`
//!      against OUR `status`/`total_cents` + the cash we recorded (Σ charge rows). Stripe
//!      paid but we have no charge row → a missed `invoice.paid`
//!      (`missed_invoice_payment`).
//!   2. REFUNDS — for each non-terminal-failed `refunds` row, `GET /v1/refunds/{re_}` and
//!      compare status: Stripe `failed`/`canceled` but we still hold `pending`/`issued` →
//!      a missed `charge.refund.updated` (`refund_status_drift`).
//!   3. DISPUTES — for each recent `billing_disputes` row, `GET /v1/disputes/{du_}` and
//!      compare status; AND `GET /v1/disputes?created>=window` to find a Stripe dispute we
//!      have NEITHER a `billing_disputes` NOR a `pending_disputes` row for — a fully-missed
//!      `charge.dispute.created` (`missing_dispute`). The backstop MAY APPLY it directly when
//!      its linkage already exists, but ONLY when the auto-heal flag is enabled.
//!
//! MIRRORS the existing crons (`billing_reconcile` / `spend_reconcile` / `dunning` /
//! `metering_export`): a `pg_try_advisory_lock` single-flights the sweep fleet-wide; a
//! per-sweep entity cap + a bounded window keep the Stripe call rate-aware; each entity is
//! reconciled FAIL-SOFT (one bad entity logs + is skipped, never aborts the sweep); the
//! `tick_with(...)` seam drives a deterministic single tick against the mock `StripeApi`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::disputes::{record_dispute_created, resolve_invoice_for_dispute};
use crate::registry::RegistryError;
use crate::stripe_client::{StripeApi, StripeClient, StripeDispute};
use crate::AppState;

/// Default tick cadence in seconds (~hourly). The backstop is a low-urgency safety net —
/// a missed webhook should be caught within an hour. Each finding is idempotent (deduped
/// on its `dedup_key`), so a frequent tick is cheap: it re-records nothing once a drift is
/// already on file.
pub const DEFAULT_TICK_SECS: u64 = 3600;

/// Default reconciliation WINDOW in days. We only re-read Stripe entities created /
/// finalized within the last N days — a bounded window keeps the Stripe call count tight
/// (a backlog can't hammer the API) and matches the "recent drift" the backstop targets
/// (an old, settled invoice/refund/dispute is no longer churning webhooks).
pub const DEFAULT_WINDOW_DAYS: i64 = 7;

/// Default per-sweep, per-pass entity cap. At most this many invoices / refunds / disputes
/// are reconciled per tick, so even a large backlog issues a BOUNDED number of Stripe GETs
/// per sweep (rate-aware). The remainder is picked up next tick (oldest-unreconciled first
/// is not required — findings are idempotent, so order only affects latency).
pub const DEFAULT_ENTITY_CAP: i64 = 200;

/// Stable `pg_advisory_lock` key for the stripe-reconcile sweep. Distinct from every other
/// cron's key (spend `…0001`-spend, billing `…6c6c_0001`). Two control instances racing
/// this sweep would issue duplicate Stripe GETs and race the finding dedup; the
/// session-scoped `pg_try_advisory_lock` single-flights it fleet-wide (a loser skips the
/// tick). Arbitrary FIXED 64-bit constant (derived from "zsrecon1").
const STRIPE_RECONCILE_ADVISORY_LOCK_KEY: i64 = 0x7a73_7265_636f_0001;

/// Knobs for one reconcile sweep (mirrors the other crons' module constants, but bundled
/// so a test can drive a deterministic tick with a tiny window / cap / heal stance).
#[derive(Debug, Clone, Copy)]
pub struct ReconcileConfig {
    /// How far back (days) to reconcile. Entities older than this are skipped.
    pub window_days: i64,
    /// Max entities reconciled per pass per sweep (rate-aware cap).
    pub entity_cap: i64,
    /// AUTO-HEAL stance for the dispute backstop. OFF by default (FLAG only): a
    /// fully-missed dispute is recorded as a `missing_dispute` finding for operator review.
    /// When ON, a missed dispute whose settling pi_/ch_ ALREADY resolves to one of our
    /// invoices is APPLIED DIRECTLY (a `billing_disputes` row + its `dispute_debit`, idempotent
    /// on the `du_…`, under the per-creator advisory lock) — parking would be a silent no-op
    /// because the only promotion site fires at `invoice.paid`, which already ran. A dispute
    /// that does NOT yet resolve to a linkage is only flagged (the webhook path parks the
    /// genuine pre-`invoice.paid` race; a Connect charge has no linkage to anchor to).
    pub auto_heal_disputes: bool,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            window_days: DEFAULT_WINDOW_DAYS,
            entity_cap: DEFAULT_ENTITY_CAP,
            auto_heal_disputes: false,
        }
    }
}

/// What one sweep recorded — counts per finding source, for the log line + the test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepSummary {
    /// Findings freshly INSERTED this sweep (deduped re-observations are NOT counted).
    pub findings_recorded: usize,
    /// Disputes APPLIED via the gated backstop this sweep (0 unless `auto_heal_disputes`).
    /// A freshly-applied `billing_disputes` row + its `dispute_debit`; a redelivery / already
    /// recorded dispute is idempotent and NOT counted.
    pub disputes_healed: usize,
    /// `true` iff this instance held the advisory lock and actually ran (vs. a single-flight
    /// skip).
    pub ran: bool,
}

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps
/// `tick_secs`. A transient error is logged and swallowed so the cron task survives
/// (mirrors `billing_reconcile` / `spend_reconcile`).
//
// `AppState`/`Registry` hold `!Send` handles; the lint is structural.
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control stripe_reconcile cron starting");
    loop {
        match tick(&state).await {
            Ok(s) if s.findings_recorded > 0 || s.disputes_healed > 0 => {
                tracing::warn!(
                    findings = s.findings_recorded,
                    healed = s.disputes_healed,
                    "control stripe_reconcile sweep recorded billing drift"
                );
            }
            Ok(_) => { /* no drift — steady state; stay quiet */ }
            Err(e) => {
                tracing::error!(error = %e, "control stripe_reconcile tick failed");
            }
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Run one reconcile sweep against the live Stripe client built from `state`, with the
/// default [`ReconcileConfig`].
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<SweepSummary, RegistryError> {
    let stripe = StripeClient::new(crate::SecretString::new(
        state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(state.stripe_base_url.clone());
    tick_with(state, &stripe, Utc::now().timestamp(), ReconcileConfig::default()).await
}

/// Sweep core, parameterized on the [`StripeApi`], the wall-clock `now` (unix seconds), and
/// the [`ReconcileConfig`] — so an integration test can drive a single deterministic tick
/// against a mock-Stripe server with a tiny window/cap/heal stance.
///
/// Single-flights the whole sweep under the advisory lock (multi-instance safety); a loser
/// returns `SweepSummary { ran: false, .. }`.
#[allow(clippy::future_not_send)]
pub async fn tick_with<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    now_unix: i64,
    cfg: ReconcileConfig,
) -> Result<SweepSummary, RegistryError> {
    // Multi-instance safety: single-flight the sweep fleet-wide. The lock conn is held for
    // the whole sweep and released below (and on drop, since the lock is session-scoped).
    let lock_conn = state.registry.conn().await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&STRIPE_RECONCILE_ADVISORY_LOCK_KEY],
        )
        .await?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!(
            "stripe_reconcile: advisory lock held by another instance — skipping tick"
        );
        return Ok(SweepSummary { ran: false, ..SweepSummary::default() });
    }

    let result = sweep(state, stripe, now_unix, cfg).await;

    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&STRIPE_RECONCILE_ADVISORY_LOCK_KEY],
        )
        .await
    {
        tracing::warn!(error = %e, "stripe_reconcile: advisory unlock failed (frees on conn drop)");
    }

    result
}

/// The advisory-lock-protected body: run the three reconcile passes, accumulate the summary.
#[allow(clippy::future_not_send)]
async fn sweep<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    now_unix: i64,
    cfg: ReconcileConfig,
) -> Result<SweepSummary, RegistryError> {
    let window_start = now_unix - cfg.window_days.max(0) * 86_400;
    let mut summary = SweepSummary { ran: true, ..SweepSummary::default() };

    summary.findings_recorded += reconcile_invoices(state, stripe, window_start, cfg).await?;
    summary.findings_recorded += reconcile_refunds(state, stripe, window_start, cfg).await?;
    let (df, dp) = reconcile_disputes(state, stripe, window_start, now_unix, cfg).await?;
    summary.findings_recorded += df;
    summary.disputes_healed += dp;

    Ok(summary)
}

// ===========================================================================
// Pass 1 — invoices.
// ===========================================================================

/// Reconcile recently-finalized invoices against Stripe. For each invoice with a
/// `billing_provider_refs(provider='stripe', ref_kind='invoice')` `in_…` whose invoice was
/// finalized within the window, `GET /v1/invoices/{in_}` and compare. FAIL-SOFT per invoice.
#[allow(clippy::future_not_send)]
async fn reconcile_invoices<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    window_start: i64,
    cfg: ReconcileConfig,
) -> Result<usize, RegistryError> {
    let conn = state.registry.conn().await?;
    let ws = unix_to_dt(window_start);
    // Recently-finalized invoices joined to their Stripe in_…. Bounded by the window
    // (finalized_at) + the entity cap. A draft/void invoice has no settled cash to
    // reconcile, so restrict to finalized non-void rows.
    let rows = conn
        .query(
            "SELECT i.id, i.status::text AS status, i.total_cents, r.external_id \
               FROM zeroship.invoices i \
               JOIN zeroship.billing_provider_refs r \
                 ON r.invoice_id = i.id AND r.provider = 'stripe' AND r.ref_kind = 'invoice' \
              WHERE i.finalized_at IS NOT NULL AND i.finalized_at >= $1 \
                AND i.status <> 'void' \
              ORDER BY i.finalized_at DESC \
              LIMIT $2",
            &[&ws, &cfg.entity_cap],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    let mut recorded = 0usize;
    for row in &rows {
        let invoice_id: String = row.get("id");
        let our_status: String = row.get("status");
        let our_total: i64 = row.get("total_cents");
        let stripe_in: String = row.get("external_id");

        // FAIL-SOFT: a transient Stripe read on one invoice must not abort the sweep.
        let inv = match stripe.get_invoice(&stripe_in).await {
            Ok(inv) => inv,
            Err(e) => {
                tracing::warn!(invoice_id = %invoice_id, stripe = %stripe_in, error = %e,
                    "stripe_reconcile: invoice GET failed — skipping this invoice");
                continue;
            }
        };

        // Cash WE recorded for this invoice: the sum of `charge` rows (what `invoice.paid`
        // appends). A missed `invoice.paid` leaves this 0 while Stripe shows amount_paid>0.
        let cash: i64 = conn
            .query(
                "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS cash \
                   FROM zeroship.invoice_payments \
                  WHERE invoice_id = $1 AND kind = 'charge'",
                &[&invoice_id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?
            .first()
            .map_or(0, |r| r.get::<_, i64>("cash"));

        // DRIFT 1 (missed_invoice_payment): Stripe collected cash but we have NO charge row.
        if inv.amount_paid > 0 && cash == 0 {
            let our = json!({ "status": our_status, "total_cents": our_total, "cash_collected_cents": cash });
            let stripe_v = json!({ "status": inv.status, "amount_paid": inv.amount_paid, "amount_due": inv.amount_due });
            recorded += record_finding(
                &conn,
                Finding {
                    kind: "missed_invoice_payment",
                    severity: "high",
                    entity_id: &stripe_in,
                    our_value: &our,
                    stripe_value: &stripe_v,
                },
            )
            .await?;
            // A missed payment is a distinct, higher-severity fact — record it and move on
            // (do not also emit a status-drift for the same invoice in the same sweep).
            continue;
        }

        // DRIFT 2 (invoice_status_drift): Stripe says paid but our invoice is not finalized
        // as paid-equivalent, OR Stripe voided/uncollectible and we did not. We map loosely:
        // a Stripe `void`/`uncollectible` while our row is `finalized` (still expecting cash)
        // is the drift we care about. Status names differ across the two systems, so we flag
        // on the cash-relevant mismatch only (a paid Stripe invoice we DO have cash for is
        // consistent — no finding).
        let stripe_terminal_no_cash =
            matches!(inv.status.as_str(), "void" | "uncollectible");
        if stripe_terminal_no_cash && cash > 0 {
            let our = json!({ "status": our_status, "total_cents": our_total, "cash_collected_cents": cash });
            let stripe_v = json!({ "status": inv.status, "amount_paid": inv.amount_paid });
            recorded += record_finding(
                &conn,
                Finding {
                    kind: "invoice_status_drift",
                    severity: "high",
                    entity_id: &stripe_in,
                    our_value: &our,
                    stripe_value: &stripe_v,
                },
            )
            .await?;
        }
    }
    Ok(recorded)
}

// ===========================================================================
// Pass 2 — refunds.
// ===========================================================================

/// Reconcile non-terminal-failed refunds against Stripe. OUR `refunds.status` is one of
/// `{pending, issued, failed, canceled}` (0049 + 0054); a refund already `failed`/`canceled`
/// is terminal and needs no re-check. For each `pending`/`issued` refund with a
/// `refund_provider_refs(ref_kind='refund')` `re_…` created within the window, `GET
/// /v1/refunds/{re_}` and flag a Stripe `failed`/`canceled` we missed. FAIL-SOFT per refund.
#[allow(clippy::future_not_send)]
async fn reconcile_refunds<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    window_start: i64,
    cfg: ReconcileConfig,
) -> Result<usize, RegistryError> {
    let conn = state.registry.conn().await?;
    let ws = unix_to_dt(window_start);
    let rows = conn
        .query(
            "SELECT rf.id, rf.status::text AS status, rf.amount_cents, pr.external_id \
               FROM zeroship.refunds rf \
               JOIN zeroship.refund_provider_refs pr \
                 ON pr.refund_id = rf.id AND pr.provider = 'stripe' AND pr.ref_kind = 'refund' \
              WHERE rf.status IN ('pending','issued') \
                AND rf.created_at >= $1 \
              ORDER BY rf.created_at DESC \
              LIMIT $2",
            &[&ws, &cfg.entity_cap],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    let mut recorded = 0usize;
    for row in &rows {
        let refund_id: String = row.get("id");
        let our_status: String = row.get("status");
        let our_amount: i64 = row.get("amount_cents");
        let stripe_re: String = row.get("external_id");

        let re = match stripe.get_refund(&stripe_re).await {
            Ok(re) => re,
            Err(e) => {
                tracing::warn!(refund_id = %refund_id, stripe = %stripe_re, error = %e,
                    "stripe_reconcile: refund GET failed — skipping this refund");
                continue;
            }
        };

        // DRIFT (refund_status_drift): Stripe says the refund failed/canceled (the cash
        // bounced) but we still hold it pending/issued — a missed `charge.refund.updated`.
        if matches!(re.status.as_str(), "failed" | "canceled") {
            let our = json!({ "status": our_status, "amount_cents": our_amount });
            let stripe_v = json!({ "status": re.status, "amount": re.amount });
            recorded += record_finding(
                &conn,
                Finding {
                    kind: "refund_status_drift",
                    severity: "high",
                    entity_id: &stripe_re,
                    our_value: &our,
                    stripe_value: &stripe_v,
                },
            )
            .await?;
        }
    }
    Ok(recorded)
}

// ===========================================================================
// Pass 3 — disputes.
// ===========================================================================

/// Reconcile disputes against Stripe in two halves:
///   (a) for each recent `billing_disputes` row, `GET /v1/disputes/{du_}` and flag a
///       status drift (Stripe progressed the dispute past what we recorded);
///   (b) `GET /v1/disputes?created>=window` and flag any Stripe dispute we have NEITHER a
///       `billing_disputes` NOR a `pending_disputes` row for (a fully-missed
///       `charge.dispute.created`). When `auto_heal_disputes` is ON and the dispute's
///       settling pi_/ch_ ALREADY resolves to one of our invoices, APPLY it directly (a
///       `billing_disputes` row + its `dispute_debit`, idempotent on the du_…) — otherwise
///       FLAG only.
///
/// Returns `(findings_recorded, disputes_healed)`. FAIL-SOFT per dispute.
#[allow(clippy::future_not_send)]
async fn reconcile_disputes<S: StripeApi>(
    state: &AppState,
    stripe: &S,
    window_start: i64,
    _now_unix: i64,
    cfg: ReconcileConfig,
) -> Result<(usize, usize), RegistryError> {
    let conn = state.registry.conn().await?;
    let ws = unix_to_dt(window_start);
    let mut recorded = 0usize;
    let mut healed = 0usize;

    // (a) Status-drift on the disputes we DO know about.
    let rows = conn
        .query(
            "SELECT id, provider_dispute_id, status::text AS status, amount_cents \
               FROM zeroship.billing_disputes \
              WHERE created_at >= $1 \
              ORDER BY created_at DESC \
              LIMIT $2",
            &[&ws, &cfg.entity_cap],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    for row in &rows {
        let our_status: String = row.get("status");
        let our_amount: i64 = row.get("amount_cents");
        let du: String = row.get("provider_dispute_id");

        let sd = match stripe.get_dispute(&du).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(provider_dispute_id = %du, error = %e,
                    "stripe_reconcile: dispute GET failed — skipping this dispute");
                continue;
            }
        };
        let stripe_bucket = crate::disputes::DisputeStatus::from_stripe(&sd.status).as_str();
        // Drift iff our recorded bucket disagrees with Stripe's CURRENT bucket — i.e. Stripe
        // resolved (won/lost) a dispute we still hold `open` (a missed `.updated`/`.closed`),
        // or the amount disagrees.
        if stripe_bucket != our_status || sd.amount != our_amount {
            let our = json!({ "status": our_status, "amount_cents": our_amount });
            let stripe_v = json!({ "status": stripe_bucket, "raw_status": sd.status, "amount": sd.amount });
            recorded += record_finding(
                &conn,
                Finding {
                    kind: "dispute_status_drift",
                    severity: "high",
                    entity_id: &du,
                    our_value: &our,
                    stripe_value: &stripe_v,
                },
            )
            .await?;
        }
    }

    // (b) Fully-missed disputes: Stripe disputes in the window we have NO row for.
    let listed = match stripe.list_disputes(window_start, u32::try_from(cfg.entity_cap).unwrap_or(100)).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "stripe_reconcile: list_disputes failed — skipping missing-dispute pass");
            return Ok((recorded, healed));
        }
    };
    for sd in &listed {
        // Known already? A row in billing_disputes OR pending_disputes for this du_… means
        // the webhook (or a prior backstop) handled it — no finding.
        let known = conn
            .query(
                "SELECT 1 AS x FROM zeroship.billing_disputes WHERE provider_dispute_id = $1 \
                 UNION ALL \
                 SELECT 1 AS x FROM zeroship.pending_disputes WHERE provider_dispute_id = $1 \
                 LIMIT 1",
                &[&sd.id],
            )
            .await
            .map_err(|e| RegistryError::Database(e.to_string()))?;
        if !known.is_empty() {
            continue;
        }

        // A genuinely-missed dispute. RECORD the finding (always).
        let stripe_v = json!({
            "status": sd.status,
            "amount": sd.amount,
            "currency": sd.currency,
            "charge": sd.charge,
            "payment_intent": sd.payment_intent,
            "reason": sd.reason,
        });
        let our = json!({ "present": false });
        recorded += record_finding(
            &conn,
            Finding {
                kind: "missing_dispute",
                severity: "high",
                entity_id: &sd.id,
                our_value: &our,
                stripe_value: &stripe_v,
            },
        )
        .await?;

        // GATED, conservatively-safe AUTO-HEAL (default OFF): if the settling pi_/ch_ ALREADY
        // links to one of our invoices, APPLY the dispute directly (billing_disputes row +
        // dispute_debit, idempotent on the du_…, under the per-creator lock). Parking it would
        // be a silent no-op — the only promotion site fires at invoice.paid, which already ran
        // before this dropped dispute existed. A not-yet-linked dispute is left flagged.
        if cfg.auto_heal_disputes {
            match try_backstop_dispute(state, sd).await {
                Ok(true) => healed += 1,
                Ok(false) => { /* no linkage yet / already recorded — finding stands */ }
                Err(e) => {
                    tracing::warn!(provider_dispute_id = %sd.id, error = %e,
                        "stripe_reconcile: dispute backstop heal failed — finding recorded, not healed");
                }
            }
        }
    }

    Ok((recorded, healed))
}

/// Heal a fully-missed dispute (gated by `auto_heal_disputes`). Idempotent + clearly safe.
/// Two cases, split on whether the settling `pi_…`/`ch_…`→invoice linkage EXISTS YET:
///
///   * LINKAGE EXISTS NOW (the common backstop case — `invoice.paid` already ran, but the
///     `charge.dispute.created` webhook was dropped): APPLY the dispute DIRECTLY via
///     [`record_dispute_created`] (UPSERT `billing_disputes` + append the `dispute_debit`,
///     idempotent on the `du_…`, taking the per-creator advisory lock). Parking here would be a
///     SILENT BUG: the ONLY promotion site is `resolve_pending_disputes_for_linkage`, fired
///     solely when the linkage is FRESHLY written at `invoice.paid` — which already happened,
///     before the dispute existed. A parked row against an already-linked invoice would never
///     promote, so the cap would stay permanently under-tightened (the platform could refund
///     cash it never kept). We therefore apply, not park.
///   * NO LINKAGE YET (the dispute raced ahead of `invoice.paid`, OR a Connect end-user charge
///     we never invoiced): leave it for the operator finding. We do NOT park here — the
///     primary webhook path already parks the pre-`invoice.paid` race, and a Connect charge has
///     no linkage that will ever come; parking it would poison the holding table. We return
///     `false` (no heal applied) and the recorded finding stands.
///
/// Returns `true` iff the dispute was freshly applied (a redelivery / already-recorded dispute
/// returns `false` — `record_dispute_created` no-ops idempotently on the `du_…`).
#[allow(clippy::future_not_send)]
async fn try_backstop_dispute(
    state: &AppState,
    sd: &StripeDispute,
) -> Result<bool, RegistryError> {
    let pi = sd.payment_intent.as_deref().unwrap_or("");
    let ch = sd.charge.as_deref().unwrap_or("");
    if pi.is_empty() && ch.is_empty() {
        return Ok(false);
    }
    if sd.amount <= 0 {
        return Ok(false);
    }
    let candidates: Vec<&str> = [pi, ch].into_iter().filter(|s| !s.is_empty()).collect();
    let conn = state.registry.conn().await?;
    let resolved = resolve_invoice_for_dispute(&conn, &candidates).await?;
    let Some(invoice_id) = resolved else {
        // No pi_/ch_→invoice linkage. Either a pre-`invoice.paid` race (the webhook path parks
        // it) or a Connect end-user charge we never invoiced. Don't park here — leave the
        // finding. The order-independent promotion handles the legitimate race when paid lands.
        return Ok(false);
    };
    let evidence_due_at = sd.evidence_due_by.map(unix_to_dt);
    // The linkage EXISTS — apply the dispute directly (idempotent on the du_…, takes the
    // per-creator advisory lock). `record_dispute_created` needs an owned `&mut` connection.
    let mut conn = conn;
    let rec = record_dispute_created(
        &mut conn,
        &invoice_id,
        sd.amount,
        &sd.currency,
        sd.reason.as_deref(),
        evidence_due_at,
        &sd.id,
    )
    .await?;
    Ok(rec.newly_created)
}

// ===========================================================================
// Findings recorder.
// ===========================================================================

/// One drift finding to append (borrowed view; `record_finding` computes the dedup key).
struct Finding<'a> {
    kind: &'a str,
    severity: &'a str,
    entity_id: &'a str,
    our_value: &'a serde_json::Value,
    stripe_value: &'a serde_json::Value,
}

/// Append a finding to `billing_reconciliation_findings`, IDEMPOTENTLY. The dedup key is a
/// stable fingerprint of the drift IDENTITY (`kind:entity:sha256(our|stripe)`), so the SAME
/// drift re-observed on a later sweep collides on the UNIQUE(dedup_key) and the `ON CONFLICT
/// DO NOTHING` no-ops (no duplicate finding). A drift that CHANGES (our value advances)
/// yields a fresh key → a fresh finding (the progress trail). Returns 1 iff a row was
/// freshly inserted, else 0.
#[allow(clippy::future_not_send)]
async fn record_finding<C>(conn: &C, f: Finding<'_>) -> Result<usize, RegistryError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let dedup_key = finding_dedup_key(f.kind, f.entity_id, f.our_value, f.stripe_value);
    let id = zeroship_core::typed_id::new_reconcile_finding_id();
    let inserted = conn
        .query(
            "INSERT INTO zeroship.billing_reconciliation_findings \
               (id, kind, severity, entity_id, our_value, stripe_value, dedup_key) \
             VALUES ($1, $2::text::zeroship.reconciliation_finding_kind, \
                     $3::text::zeroship.reconciliation_finding_severity, $4, $5, $6, $7) \
             ON CONFLICT (dedup_key) DO NOTHING \
             RETURNING id",
            &[
                &id,
                &f.kind,
                &f.severity,
                &f.entity_id,
                &f.our_value,
                &f.stripe_value,
                &dedup_key,
            ],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    if inserted.is_empty() {
        // A re-observation of a known drift — already on file.
        return Ok(0);
    }
    // Loud, structured log so a high-severity drift is noticed even without a dashboard.
    tracing::warn!(
        target: "control.stripe_reconcile",
        kind = %f.kind,
        severity = %f.severity,
        entity_id = %f.entity_id,
        our = %f.our_value,
        stripe = %f.stripe_value,
        "stripe reconciliation drift detected (operator-review)"
    );
    Ok(1)
}

/// Stable, value-free fingerprint of a drift's identity. SHA-256 over the compared values'
/// canonical JSON so an equivalent re-observation hashes identically (idempotent), but a
/// CHANGED drift hashes differently (a new finding). `kind:entity:hash` keeps the human
/// prefix readable in the row.
fn finding_dedup_key(
    kind: &str,
    entity_id: &str,
    our_value: &serde_json::Value,
    stripe_value: &serde_json::Value,
) -> String {
    let mut hasher = Sha256::new();
    // serde_json serializes Value maps in a stable (sorted by insertion for json! macro)
    // order; to be robust we hash the compact string of each side under a separator.
    hasher.update(our_value.to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(stripe_value.to_string().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("{kind}:{entity_id}:{hex}")
}

/// Convert unix seconds to a UTC `DateTime` for a `TIMESTAMPTZ` bind.
fn unix_to_dt(unix: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc
        .timestamp_opt(unix, 0)
        .single()
        .unwrap_or_else(chrono::Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_and_cap() {
        let c = ReconcileConfig::default();
        assert_eq!(c.window_days, DEFAULT_WINDOW_DAYS);
        assert_eq!(c.entity_cap, DEFAULT_ENTITY_CAP);
        assert!(!c.auto_heal_disputes, "auto-heal MUST default OFF (flag-by-default)");
    }

    #[test]
    fn default_tick_is_hourly() {
        assert_eq!(DEFAULT_TICK_SECS, 3600);
    }

    #[test]
    fn dedup_key_is_stable_and_value_sensitive() {
        let our = json!({ "status": "issued" });
        let stripe_a = json!({ "status": "failed" });
        let stripe_b = json!({ "status": "canceled" });
        let k1 = finding_dedup_key("refund_status_drift", "re_1", &our, &stripe_a);
        let k2 = finding_dedup_key("refund_status_drift", "re_1", &our, &stripe_a);
        let k3 = finding_dedup_key("refund_status_drift", "re_1", &our, &stripe_b);
        assert_eq!(k1, k2, "same drift → same key (idempotent)");
        assert_ne!(k1, k3, "changed stripe value → new key (progress trail)");
        assert!(k1.starts_with("refund_status_drift:re_1:"));
    }

    #[test]
    fn advisory_lock_key_is_distinct() {
        // Must not collide with the other crons' keys (spend / billing).
        assert_ne!(STRIPE_RECONCILE_ADVISORY_LOCK_KEY, 0x7a73_6269_6c6c_0001);
    }
}
