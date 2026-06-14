//! Billing-notification cron (billing-ops gap #26, PR-6; design flow F).
//!
//! Every ~5min it sweeps the already-written billing transition rows for events that
//! have NOT yet produced a `sent` notification, claims each BEFORE sending (two-phase
//! `pending→sent`), sends via the [`BillingNotifier`](crate::notify::BillingNotifier)
//! seam, then flips the claim to `sent`. It is READ-ONLY w.r.t. money — it only reads
//! history/invoice/refund rows and writes its OWN `billing_notifications` send-ledger.
//!
//! ## Multi-node safety (design CRITICAL-3 / principle 7)
//!
//! Two guards, both required:
//!   1. **A dedicated advisory lock** ([`NOTIFY_SWEEP_ADVISORY_LOCK_KEY`], the
//!      "zsnotf"-family key, distinct from dunning/spend) held on a dedicated
//!      connection for the whole tick, exactly as `dunning.rs` does — only ONE instance
//!      sweeps per tick.
//!   2. **Claim-BEFORE-send**: the cron `INSERT … status='pending' ON CONFLICT DO
//!      NOTHING RETURNING`; the row whose INSERT WINS is the only one cleared to send.
//!      This covers the lock-handoff window (defence in depth) AND the multi-node race
//!      if the lock is ever dropped.
//!
//! ## At-least-once delivery / exactly-once claim (design MAJOR-A/B)
//!
//! A crash AFTER `Mailer::send` succeeds but BEFORE the `sent` flip commits leaves a
//! `pending` row whose `claimed_at` is past [`NOTIFY_REDRIVE_HORIZON`] (= 15min) → the
//! next tick RE-DRIVES the SAME row (re-claims via UPDATE, re-sends). The provider-side
//! `Idempotency-Key = (creator_id, kind, transition_id)` the notifier passes makes the
//! re-send an effective no-op at any provider that honours it.

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::notify::{BillingNotificationKind, Notification, NotificationDetail};
use crate::registry::RegistryError;
use crate::AppState;

/// Default tick cadence in seconds (~5min). The design names a ~5min tick so a crashed
/// send is retried within ≤ 3 ticks of [`NOTIFY_REDRIVE_HORIZON`].
pub const DEFAULT_TICK_SECS: u64 = 300;

/// The re-drive horizon (design MAJOR-B): a `pending` claim older than this is assumed
/// crashed-before-flip and is re-driven. 15min ≥ 3 cron ticks, so a still-in-flight
/// send is never prematurely re-driven, but a crashed one is retried promptly.
pub const NOTIFY_REDRIVE_HORIZON: Duration = Duration::from_secs(15 * 60);

/// How far back the sweep scans the history/invoice/refund tables. A watermark bound so
/// the cron does not re-scan ALL history forever (design note): a transition older than
/// this whose notification never sent is abandoned (it is long past actionable). The
/// LEFT JOIN on the ledger already excludes already-sent rows; this just caps the scan.
const NOTIFY_SCAN_WINDOW: &str = "30 days";

/// Stable `pg_advisory_lock` key for the notify sweep — the "zsnotf"-family constant,
/// distinct from dunning (`0x7a73_6475_6e6e_0001`) and spend (`0x7a73_7370_6e64_0001`)
/// so the sweeps never block each other.
const NOTIFY_SWEEP_ADVISORY_LOCK_KEY: i64 = 0x7a73_6e6f_7466_0001;

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps. A
/// transient PG/mailer error is logged and swallowed so the cron survives (mirrors
/// `dunning`/`spend_reconcile`).
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control billing-notify cron starting");
    loop {
        match tick(&state).await {
            Ok(n) if n > 0 => {
                tracing::info!(sent = n, "control billing-notify sweep sent notifications");
            }
            Ok(_) => { /* steady state; stay quiet */ }
            Err(e) => {
                tracing::error!(error = %e, "control billing-notify tick failed");
            }
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// One candidate notification scanned from the source tables (before claim/send).
struct Candidate {
    creator_id: Uuid,
    kind: BillingNotificationKind,
    transition_id: String,
    detail: NotificationDetail,
}

/// Run one notify sweep. Exposed so an integration test can drive a single tick
/// deterministically. Returns the number of notifications actually SENT (claimed +
/// delivered) this tick.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    // (1) Single-flight advisory lock for the whole sweep (mirrors dunning.rs). A loser
    // instance skips this tick.
    let lock_conn = state.registry.conn().await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&NOTIFY_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!("billing-notify: advisory lock held by another instance — skipping tick");
        return Ok(0);
    }

    let result = sweep(state).await;

    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&NOTIFY_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await
    {
        tracing::warn!(error = %e, "billing-notify: advisory unlock failed (lock frees on conn drop)");
    }

    result
}

/// The lock-held sweep body: scan → claim-before-send → send → flip, for each kind.
#[allow(clippy::future_not_send)]
async fn sweep(state: &AppState) -> Result<usize, RegistryError> {
    let candidates = scan_unsent(state).await?;
    let mut sent = 0usize;
    for c in candidates {
        // (2) CLAIM BEFORE SEND. The row whose INSERT WINS (returns) is the only one
        // cleared to send; a stale `pending` row past the horizon is re-claimed via the
        // ON CONFLICT UPDATE branch. A concurrent loser (lock-handoff window) gets 0 rows.
        let claimed = claim(state, &c).await?;
        if !claimed {
            continue;
        }
        // (3) Resolve the recipient, render, send. On a suppressed recipient the send is
        // a deliverability skip — we still flip to `sent` (the suppression is permanent;
        // re-driving would never succeed). On any other transport error we LEAVE the row
        // `pending` so the next tick (past the horizon) re-drives the SAME row.
        let Some((email, name)) = resolve_recipient(state, c.creator_id).await? else {
            // No email on file — nothing to deliver; flip to `sent` so it is not retried.
            mark_sent(state, &c).await?;
            continue;
        };
        let idempotency_key = format!("{}:{}:{}", c.creator_id, c.kind.as_str(), c.transition_id);
        let notification = Notification {
            to_email: email,
            to_name: name,
            kind: c.kind,
            detail: c.detail.clone(),
            idempotency_key,
        };
        match state.notifier.notify(&state.control_pg, &notification).await {
            Ok(()) | Err(zeroship_mailer::MailerError::Suppressed(_)) => {
                mark_sent(state, &c).await?;
                sent += 1;
            }
            Err(e) => {
                // Leave `pending`; the next tick past NOTIFY_REDRIVE_HORIZON re-drives.
                tracing::warn!(
                    error = %e,
                    creator_id = %c.creator_id,
                    kind = c.kind.as_str(),
                    "billing-notify: send failed — leaving pending for re-drive",
                );
            }
        }
    }
    Ok(sent)
}

/// Scan the source tables for transitions WITHOUT a `sent` notification (and `pending`
/// rows past the re-drive horizon). Each LEFT JOIN excludes a `sent` ledger row and a
/// fresh `pending` claim (only stale `pending` rows re-qualify). Bounded by
/// [`NOTIFY_SCAN_WINDOW`] so the cron never re-scans all history.
#[allow(clippy::future_not_send)]
async fn scan_unsent(state: &AppState) -> Result<Vec<Candidate>, RegistryError> {
    let conn = state.registry.conn().await?;
    let horizon_secs = i64::try_from(NOTIFY_REDRIVE_HORIZON.as_secs()).unwrap_or(900);
    let mut out = Vec::new();

    // (a) Dunning-driven kinds from creator_billing_status_history. `to_state`/`reason`
    // map to the notification kind. We notify the actionable edges: →past_due,
    // →suspended, →active (recovered). The notification kind is the dedup `kind`; the
    // transition row's surrogate `id` is the `transition_id`.
    let rows = conn
        .query(
            "SELECT h.id, h.creator_id, h.to_state, h.reason \
               FROM zeroship.creator_billing_status_history h \
               LEFT JOIN zeroship.billing_notifications n \
                 ON n.creator_id = h.creator_id \
                AND n.transition_id = h.id \
                AND n.kind = CASE h.to_state \
                      WHEN 'past_due'  THEN 'past_due' \
                      WHEN 'suspended' THEN 'suspended' \
                      WHEN 'active'    THEN 'recovered' END::zeroship.billing_notification_kind \
              WHERE h.at > NOW() - $1::text::interval \
                AND h.to_state IN ('past_due','suspended','active') \
                AND ( n.status IS NULL \
                   OR (n.status = 'pending' AND n.claimed_at < NOW() - make_interval(secs => $2::double precision)) )",
            &[&NOTIFY_SCAN_WINDOW, &(horizon_secs as f64)],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    for r in &rows {
        let to_state: String = r.get("to_state");
        let kind = match to_state.as_str() {
            "past_due" => BillingNotificationKind::PastDue,
            "suspended" => BillingNotificationKind::Suspended,
            "active" => BillingNotificationKind::Recovered,
            _ => continue,
        };
        out.push(Candidate {
            creator_id: r.get("creator_id"),
            kind,
            transition_id: r.get("id"),
            detail: NotificationDetail::default(),
        });
    }

    // (b) Newly-finalized invoices. transition_id = invoice id. The frozen total is
    // formatted into the body (never re-priced).
    let rows = conn
        .query(
            "SELECT i.id, i.creator_id, i.period, i.total_cents, i.currency \
               FROM zeroship.invoices i \
               LEFT JOIN zeroship.billing_notifications n \
                 ON n.creator_id = i.creator_id \
                AND n.transition_id = i.id \
                AND n.kind = 'invoice_finalized'::zeroship.billing_notification_kind \
              WHERE i.status = 'finalized' \
                AND i.finalized_at > NOW() - $1::text::interval \
                AND ( n.status IS NULL \
                   OR (n.status = 'pending' AND n.claimed_at < NOW() - make_interval(secs => $2::double precision)) )",
            &[&NOTIFY_SCAN_WINDOW, &(horizon_secs as f64)],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    for r in &rows {
        let total: i64 = r.get("total_cents");
        let period: chrono::NaiveDate = r.get("period");
        out.push(Candidate {
            creator_id: r.get("creator_id"),
            kind: BillingNotificationKind::InvoiceFinalized,
            transition_id: r.get("id"),
            detail: NotificationDetail {
                period_label: Some(period.format("%B %Y").to_string()),
                amount_label: Some(format_money(total, &r.get::<_, String>("currency"))),
                refund_destination_label: None,
            },
        });
    }

    // (c) Newly-issued refunds. transition_id = refund id; creator via the invoice FK.
    let rows = conn
        .query(
            "SELECT r.id, i.creator_id, r.amount_cents, r.currency, r.destination \
               FROM zeroship.refunds r \
               JOIN zeroship.invoices i ON i.id = r.invoice_id \
               LEFT JOIN zeroship.billing_notifications n \
                 ON n.creator_id = i.creator_id \
                AND n.transition_id = r.id \
                AND n.kind = 'refunded'::zeroship.billing_notification_kind \
              WHERE r.status = 'issued' \
                AND r.created_at > NOW() - $1::text::interval \
                AND ( n.status IS NULL \
                   OR (n.status = 'pending' AND n.claimed_at < NOW() - make_interval(secs => $2::double precision)) )",
            &[&NOTIFY_SCAN_WINDOW, &(horizon_secs as f64)],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    for r in &rows {
        let amount: i64 = r.get("amount_cents");
        let dest: String = r.get("destination");
        let dest_label = if dest == "cash" {
            "cash to your card"
        } else {
            "credit to your balance"
        };
        out.push(Candidate {
            creator_id: r.get("creator_id"),
            kind: BillingNotificationKind::Refunded,
            transition_id: r.get("id"),
            detail: NotificationDetail {
                period_label: None,
                amount_label: Some(format_money(amount, &r.get::<_, String>("currency"))),
                refund_destination_label: Some(dest_label.to_owned()),
            },
        });
    }

    // (d) Newly-opened disputes (PR-8). transition_id = the `dsp_…` dispute id; creator
    // via the invoice FK. We notify ONLY the open-dispute event (the cardholder disputed a
    // charge) — won/lost resolutions are not separate creator emails (they're operator
    // bookkeeping). The disputed amount is formatted into the body (frozen, never
    // re-priced).
    let rows = conn
        .query(
            "SELECT d.id, i.creator_id, d.amount_cents, d.currency \
               FROM zeroship.billing_disputes d \
               JOIN zeroship.invoices i ON i.id = d.invoice_id \
               LEFT JOIN zeroship.billing_notifications n \
                 ON n.creator_id = i.creator_id \
                AND n.transition_id = d.id \
                AND n.kind = 'disputed'::zeroship.billing_notification_kind \
              WHERE d.created_at > NOW() - $1::text::interval \
                AND ( n.status IS NULL \
                   OR (n.status = 'pending' AND n.claimed_at < NOW() - make_interval(secs => $2::double precision)) )",
            &[&NOTIFY_SCAN_WINDOW, &(horizon_secs as f64)],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    for r in &rows {
        let amount: i64 = r.get("amount_cents");
        out.push(Candidate {
            creator_id: r.get("creator_id"),
            kind: BillingNotificationKind::Disputed,
            transition_id: r.get("id"),
            detail: NotificationDetail {
                period_label: None,
                amount_label: Some(format_money(amount, &r.get::<_, String>("currency"))),
                refund_destination_label: None,
            },
        });
    }

    Ok(out)
}

/// Claim a candidate BEFORE sending. Returns `true` iff THIS sweep won the claim (the
/// INSERT returned, or the stale-`pending` re-claim UPDATE returned). A concurrent loser
/// (or a fresh `pending` row that has not yet aged past the horizon) returns `false`.
#[allow(clippy::future_not_send)]
async fn claim(state: &AppState, c: &Candidate) -> Result<bool, RegistryError> {
    let conn = state.registry.conn().await?;
    let horizon_secs = i64::try_from(NOTIFY_REDRIVE_HORIZON.as_secs()).unwrap_or(900) as f64;
    // The INSERT wins for a never-claimed transition. ON CONFLICT we re-claim ONLY a
    // stale `pending` row (crashed before flip, past the horizon); a `sent` row or a
    // fresh `pending` row updates 0 rows (the WHERE excludes them) so RETURNING is empty.
    let rows = conn
        .query(
            "INSERT INTO zeroship.billing_notifications \
                (creator_id, kind, transition_id, status, claimed_at) \
             VALUES ($1, $2::text::zeroship.billing_notification_kind, $3, 'pending', NOW()) \
             ON CONFLICT (creator_id, kind, transition_id) DO UPDATE \
                SET claimed_at = NOW() \
              WHERE zeroship.billing_notifications.status = 'pending' \
                AND zeroship.billing_notifications.claimed_at < NOW() - make_interval(secs => $4::double precision) \
             RETURNING transition_id",
            &[&c.creator_id, &c.kind.as_str(), &c.transition_id, &horizon_secs],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(!rows.is_empty())
}

/// Flip a claimed row to `sent` after a successful (or suppressed/no-recipient) send.
#[allow(clippy::future_not_send)]
async fn mark_sent(state: &AppState, c: &Candidate) -> Result<(), RegistryError> {
    let conn = state.registry.conn().await?;
    conn.execute(
        "UPDATE zeroship.billing_notifications \
            SET status = 'sent', sent_at = NOW() \
          WHERE creator_id = $1 \
            AND kind = $2::text::zeroship.billing_notification_kind \
            AND transition_id = $3",
        &[&c.creator_id, &c.kind.as_str(), &c.transition_id],
    )
    .await
    .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(())
}

/// Resolve the creator's email + display name from `users` (creator_id == users.id).
/// `None` ⇒ no user row (e.g. an erased creator) — the cron flips the claim to `sent`
/// without sending.
#[allow(clippy::future_not_send)]
async fn resolve_recipient(
    state: &AppState,
    creator_id: Uuid,
) -> Result<Option<(String, Option<String>)>, RegistryError> {
    let conn = state.registry.conn().await?;
    let rows = conn
        .query(
            "SELECT email::text AS email, name FROM zeroship.users WHERE id = $1",
            &[&creator_id],
        )
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;
    Ok(rows.first().map(|r| {
        let name: String = r.get("name");
        (r.get::<_, String>("email"), if name.is_empty() { None } else { Some(name) })
    }))
}

/// Format cents into a display money string (USD launch). e.g. `1234` → `"$12.34"`.
fn format_money(cents: i64, currency: &str) -> String {
    let sym = if currency.eq_ignore_ascii_case("usd") { "$" } else { "" };
    let whole = cents / 100;
    let frac = (cents % 100).abs();
    format!("{sym}{whole}.{frac:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tick_is_five_minutes() {
        assert_eq!(DEFAULT_TICK_SECS, 300);
    }

    #[test]
    fn redrive_horizon_is_fifteen_minutes() {
        assert_eq!(NOTIFY_REDRIVE_HORIZON, Duration::from_secs(15 * 60));
    }

    #[test]
    fn notify_lock_key_is_distinct() {
        assert_ne!(NOTIFY_SWEEP_ADVISORY_LOCK_KEY, 0x7a73_6475_6e6e_0001_i64); // dunning
        assert_ne!(NOTIFY_SWEEP_ADVISORY_LOCK_KEY, 0x7a73_7370_6e64_0001_i64); // spend
    }

    #[test]
    fn money_formats_usd() {
        assert_eq!(format_money(1234, "usd"), "$12.34");
        assert_eq!(format_money(0, "usd"), "$0.00");
        assert_eq!(format_money(5, "usd"), "$0.05");
        assert_eq!(format_money(100, "usd"), "$1.00");
    }
}
