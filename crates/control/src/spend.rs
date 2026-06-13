//! Spend engine (billing PR5, ISS-31) — derive + persist each app's
//! [`SpendState`] from period spend vs its effective spend limit, with
//! hysteresis so an app near its cap does not flap.
//!
//! Two layers:
//!
//! * [`derive_state`] — PURE state machine. `pct = spend*100/limit` (integer
//!   math; `limit == 0` ⇒ `pct = u64::MAX` ⇒ Block — a free/cardless plan has
//!   no headroom). Transitions UP at the threshold; transitions DOWN only once
//!   `pct` drops below `(threshold − deadband)` (the anti-flap deadband). A
//!   genuine limit change (creator raises the cap / plan change) bypasses the
//!   deadband for that one tick so the app recovers immediately.
//! * [`SpendEngine`] — PG-backed. `evaluate_all` prices each app's current
//!   period usage via the PR4 catalog, resolves the effective limit, derives
//!   the new state vs the stored previous state, and on a transition UPSERTs
//!   `app_spend_state` + appends a `spend_state_history` row. `set_limit`
//!   upserts the per-app override (the M4 creator endpoint).
//!
//! Decision D1: enforcement rides the PULLed `RouteEntry.spend_state`
//! (registry JOINs `app_spend_state`), NOT a pushed `ControlEvent`. The cron
//! constructs `ControlEvent::SpendState` only for the audit log / future SSE.

use uuid::Uuid;
use zeroship_core::types::SpendState;

use crate::metering::{current_period_start_unix, Metering};
use crate::plan_catalog::PlanCatalog;
use crate::pricing::charge_cents;
use crate::registry::{Registry, RegistryError};

/// Thresholds (in integer percent of the limit) governing the state machine,
/// plus the recovery deadband.
///
/// `derive_state` enters a more-restrictive state at the threshold and only
/// relaxes once spend drops below `threshold − deadband_pct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendThresholds {
    pub warn_pct: u64,
    pub degrade_pct: u64,
    pub block_pct: u64,
    pub deadband_pct: u64,
}

impl Default for SpendThresholds {
    fn default() -> Self {
        Self {
            warn_pct: 80,
            degrade_pct: 95,
            block_pct: 100,
            deadband_pct: 5,
        }
    }
}

/// TEXT ↔ [`SpendState`] mapping for the `state` column. Matches the
/// `#[serde(rename_all = "snake_case")]` wire form so the DB value and the
/// `RouteEntry` JSON agree.
#[must_use]
pub fn spend_state_str(state: SpendState) -> &'static str {
    match state {
        SpendState::Allow => "allow",
        SpendState::Warn => "warn",
        SpendState::Degrade => "degrade",
        SpendState::Block => "block",
    }
}

/// Parse the `state` TEXT column. An unrecognised value fails CLOSED to
/// `Block` (defensive — the engine only ever writes the four known states).
#[must_use]
pub fn parse_spend_state(s: &str) -> SpendState {
    match s {
        "allow" => SpendState::Allow,
        "warn" => SpendState::Warn,
        "degrade" => SpendState::Degrade,
        _ => SpendState::Block,
    }
}

/// The threshold (percent) at which `state` was ENTERED — used to compute the
/// recovery deadband boundary `threshold − deadband` for the relax direction.
fn entry_threshold(state: SpendState, t: &SpendThresholds) -> u64 {
    match state {
        SpendState::Allow => 0,
        SpendState::Warn => t.warn_pct,
        SpendState::Degrade => t.degrade_pct,
        SpendState::Block => t.block_pct,
    }
}

/// Numeric severity ordering (Allow < Warn < Degrade < Block) so we can tell
/// "more restrictive" from "less restrictive".
fn severity(state: SpendState) -> u8 {
    match state {
        SpendState::Allow => 0,
        SpendState::Warn => 1,
        SpendState::Degrade => 2,
        SpendState::Block => 3,
    }
}

/// PURE spend-state derivation with hysteresis.
///
/// * `spend_cents` — priced period spend.
/// * `limit_cents` — the EFFECTIVE limit (override else plan default). `0` ⇒
///   any spend is over (free/cardless plan) ⇒ Block.
/// * `prev` — the app's previously-stored state (the hysteresis anchor).
/// * `limit_changed` — `true` when the effective limit differs from the one
///   used at `prev`'s computation. A genuine limit change bypasses the
///   deadband for this tick (immediate recovery on a raised cap).
///
/// Upward transitions (to a MORE restrictive state) take effect at the
/// threshold. Downward transitions (to a LESS restrictive state) are held at
/// `prev` until `pct` drops below `entry_threshold(prev) − deadband_pct`.
#[must_use]
pub fn derive_state(
    spend_cents: u64,
    limit_cents: u64,
    t: &SpendThresholds,
    prev: SpendState,
    limit_changed: bool,
) -> SpendState {
    let pct = if limit_cents == 0 {
        u64::MAX
    } else {
        // u128 intermediate so a large spend*100 cannot overflow u64.
        let p = (u128::from(spend_cents) * 100) / u128::from(limit_cents);
        u64::try_from(p).unwrap_or(u64::MAX)
    };

    // Raw state purely from the thresholds (the upward boundary).
    let raw = if pct >= t.block_pct {
        SpendState::Block
    } else if pct >= t.degrade_pct {
        SpendState::Degrade
    } else if pct >= t.warn_pct {
        SpendState::Warn
    } else {
        SpendState::Allow
    };

    // Entering a MORE-restrictive state, or holding the same state: take raw
    // immediately (no deadband on the way up).
    if severity(raw) >= severity(prev) {
        return raw;
    }

    // raw is LESS restrictive than prev — a relaxation. A genuine limit change
    // recovers immediately (the percentage dropped by construction, not by
    // accrual oscillation), bypassing the deadband.
    if limit_changed {
        return raw;
    }

    // Anti-flap: only relax once pct has fallen below (entry_threshold(prev) −
    // deadband). Otherwise HOLD prev.
    let relax_boundary = entry_threshold(prev, t).saturating_sub(t.deadband_pct);
    if pct < relax_boundary {
        raw
    } else {
        prev
    }
}

/// PG-backed spend engine. Shares the control plane's per-query connection
/// model via [`Registry`]; prices usage with the PR4 catalog.
#[derive(Clone, Debug)]
pub struct SpendEngine {
    registry: Registry,
    catalog: PlanCatalog,
    metering: Metering,
    thresholds: SpendThresholds,
}

/// One state transition produced by [`SpendEngine::evaluate_all`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendTransition {
    pub app_id: Uuid,
    pub old: SpendState,
    pub new: SpendState,
}

impl SpendEngine {
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self {
            catalog: PlanCatalog::new(registry.clone()),
            metering: Metering::new(registry.clone()),
            registry,
            thresholds: SpendThresholds::default(),
        }
    }

    /// Resolve `(plan_id, prev_state, override_limit, eval_limit)` for one app.
    /// `prev_state`/`eval_limit` come from `app_spend_state` (default
    /// Allow / 0 when there is no row yet).
    async fn app_state_row(
        conn: &compio_postgres::Client,
        app_id: &Uuid,
    ) -> Result<(String, SpendState, Option<i64>, i64), RegistryError> {
        let rows = conn
            .query(
                "SELECT a.plan_id, s.state, s.spend_limit_cents, s.eval_limit_cents \
                 FROM zeroship.apps a \
                 LEFT JOIN zeroship.app_spend_state s ON s.app_id = a.id \
                 WHERE a.id = $1",
                &[app_id],
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| RegistryError::NotFound(format!("app {app_id}")))?;
        let plan_id: String = row.get("plan_id");
        let prev = row
            .get::<_, Option<String>>("state")
            .as_deref()
            .map_or(SpendState::Allow, parse_spend_state);
        let override_limit: Option<i64> = row.get("spend_limit_cents");
        let eval_limit: i64 = row.get::<_, Option<i64>>("eval_limit_cents").unwrap_or(0);
        Ok((plan_id, prev, override_limit, eval_limit))
    }

    /// Evaluate every app: price current-period usage, derive the new state vs
    /// the stored previous state, and on a transition persist it. Returns ONLY
    /// the apps that transitioned.
    pub async fn evaluate_all(&self) -> Result<Vec<SpendTransition>, RegistryError> {
        let conn = self.registry.conn().await?;
        let app_rows = conn
            .query("SELECT id FROM zeroship.apps", &[])
            .await?;
        let period_start = current_period_start_unix();
        let mut transitions = Vec::new();

        for row in &app_rows {
            let app_id: Uuid = row.get("id");
            let (plan_id, prev, override_limit, prev_eval_limit) =
                Self::app_state_row(&conn, &app_id).await?;

            // Plan → price model + default spend limit. A missing plan row
            // (should not happen — plan_id is an FK) is skipped, not crashed.
            let Some(plan) = self.catalog.get(&plan_id).await? else {
                tracing::warn!(app_id = %app_id, plan_id = %plan_id, "spend: app plan not in catalog — skipping");
                continue;
            };

            // Price the current period's usage.
            let usage = self.metering.period_totals(&app_id, period_start).await?;
            let breakdown = charge_cents(&plan.price, &usage);
            let spend_cents = breakdown.total_cents;

            // Effective limit: override else plan default.
            let limit_cents = override_limit.map_or(plan.price.spend_limit_default_cents, |o| {
                u64::try_from(o).unwrap_or(0)
            });
            let limit_i64 = i64::try_from(limit_cents).unwrap_or(i64::MAX);

            let limit_changed = limit_i64 != prev_eval_limit;
            let new = derive_state(spend_cents, limit_cents, &self.thresholds, prev, limit_changed);

            if new != prev {
                let spend_i64 = i64::try_from(spend_cents).unwrap_or(i64::MAX);
                Self::persist_transition(
                    &conn,
                    &app_id,
                    prev,
                    new,
                    spend_i64,
                    limit_i64,
                    period_start,
                )
                .await?;
                transitions.push(SpendTransition { app_id, old: prev, new });
            } else {
                // No transition, but keep the stored spend/eval-limit fresh so
                // the next tick's `limit_changed` comparison is accurate and
                // the dashboard sees current spend. Upsert without history.
                Self::touch_state(
                    &conn,
                    &app_id,
                    new,
                    i64::try_from(spend_cents).unwrap_or(i64::MAX),
                    limit_i64,
                    period_start,
                )
                .await?;
            }
        }
        Ok(transitions)
    }

    /// UPSERT the spend row to the new state AND append a history row. Called
    /// only on an actual transition.
    async fn persist_transition(
        conn: &compio_postgres::Client,
        app_id: &Uuid,
        from: SpendState,
        to: SpendState,
        spend_cents: i64,
        eval_limit_cents: i64,
        period_start_unix: i64,
    ) -> Result<(), RegistryError> {
        conn.execute(
            "INSERT INTO zeroship.app_spend_state \
               (app_id, state, spend_cents, eval_limit_cents, period_start, updated_at) \
             VALUES ($1, $2, $3, $4, to_timestamp($5::double precision), NOW()) \
             ON CONFLICT (app_id) DO UPDATE SET \
               state = EXCLUDED.state, spend_cents = EXCLUDED.spend_cents, \
               eval_limit_cents = EXCLUDED.eval_limit_cents, \
               period_start = EXCLUDED.period_start, updated_at = NOW()",
            &[
                app_id,
                &spend_state_str(to),
                &spend_cents,
                &eval_limit_cents,
                &(period_start_unix as f64),
            ],
        )
        .await?;
        conn.execute(
            "INSERT INTO zeroship.spend_state_history \
               (app_id, from_state, to_state, spend_cents, limit_cents) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                app_id,
                &spend_state_str(from),
                &spend_state_str(to),
                &spend_cents,
                &eval_limit_cents,
            ],
        )
        .await?;
        Ok(())
    }

    /// UPSERT spend/eval-limit freshness WITHOUT a state change (no history).
    /// Preserves the creator override column (`spend_limit_cents`) — it is set
    /// only via `set_limit`.
    async fn touch_state(
        conn: &compio_postgres::Client,
        app_id: &Uuid,
        state: SpendState,
        spend_cents: i64,
        eval_limit_cents: i64,
        period_start_unix: i64,
    ) -> Result<(), RegistryError> {
        conn.execute(
            "INSERT INTO zeroship.app_spend_state \
               (app_id, state, spend_cents, eval_limit_cents, period_start, updated_at) \
             VALUES ($1, $2, $3, $4, to_timestamp($5::double precision), NOW()) \
             ON CONFLICT (app_id) DO UPDATE SET \
               spend_cents = EXCLUDED.spend_cents, \
               eval_limit_cents = EXCLUDED.eval_limit_cents, \
               period_start = EXCLUDED.period_start, updated_at = NOW()",
            &[
                app_id,
                &spend_state_str(state),
                &spend_cents,
                &eval_limit_cents,
                &(period_start_unix as f64),
            ],
        )
        .await?;
        Ok(())
    }

    /// Set (or clear, with `None`) the per-app spend-limit override. Used by
    /// the M4 creator endpoint. Upserts `app_spend_state.spend_limit_cents`
    /// without touching the derived state — the next `evaluate_all` tick
    /// re-derives against the new effective limit (immediate recovery on a
    /// raise, via the `limit_changed` deadband bypass).
    pub async fn set_limit(
        &self,
        app_id: &Uuid,
        cents: Option<u64>,
    ) -> Result<(), RegistryError> {
        let conn = self.registry.conn().await?;
        let limit: Option<i64> = cents.map(|c| i64::try_from(c).unwrap_or(i64::MAX));
        conn.execute(
            "INSERT INTO zeroship.app_spend_state (app_id, spend_limit_cents, period_start, updated_at) \
             VALUES ($1, $2, to_timestamp($3::double precision), NOW()) \
             ON CONFLICT (app_id) DO UPDATE SET \
               spend_limit_cents = EXCLUDED.spend_limit_cents, updated_at = NOW()",
            &[app_id, &limit, &(current_period_start_unix() as f64)],
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> SpendThresholds {
        SpendThresholds::default()
    }

    #[test]
    fn derive_state_bands_table_driven() {
        let th = t();
        // limit = 100 cents → pct == spend.
        // Fresh evaluation (prev = Allow), no limit change.
        let cases = [
            (0u64, SpendState::Allow),
            (50, SpendState::Allow),
            (79, SpendState::Allow),
            (80, SpendState::Warn),
            (94, SpendState::Warn),
            (95, SpendState::Degrade),
            (99, SpendState::Degrade),
            (100, SpendState::Block),
            (150, SpendState::Block),
        ];
        for (spend, want) in cases {
            assert_eq!(
                derive_state(spend, 100, &th, SpendState::Allow, false),
                want,
                "spend={spend} from Allow",
            );
        }
    }

    #[test]
    fn zero_limit_is_block_on_any_spend() {
        let th = t();
        // limit == 0 ⇒ free/cardless ⇒ any non-zero spend is Block.
        assert_eq!(
            derive_state(0, 0, &th, SpendState::Allow, false),
            SpendState::Block,
            "even zero spend against a zero limit is over (pct=∞)",
        );
        assert_eq!(
            derive_state(1, 0, &th, SpendState::Allow, false),
            SpendState::Block,
        );
    }

    #[test]
    fn upward_transition_takes_threshold_immediately() {
        let th = t();
        // From Allow, crossing into Degrade range jumps straight to Degrade
        // (no per-step delay on the way up).
        assert_eq!(
            derive_state(96, 100, &th, SpendState::Allow, false),
            SpendState::Degrade,
        );
        // From Warn straight to Block.
        assert_eq!(
            derive_state(100, 100, &th, SpendState::Warn, false),
            SpendState::Block,
        );
    }

    #[test]
    fn degrade_recovers_only_after_deadband() {
        let th = t();
        // App is currently Degrade (entered at 95%). Spend ebbs to 92% — that
        // is BELOW degrade_pct (95) so the raw state is Warn, BUT it is not yet
        // below the deadband boundary (95 − 5 = 90). The anti-flap rule HOLDS
        // Degrade: a borderline app must not flap Degrade↔Warn each tick.
        assert_eq!(
            derive_state(92, 100, &th, SpendState::Degrade, false),
            SpendState::Degrade,
            "92% must HOLD Degrade (within the 90..95 deadband) — no flap",
        );
        // Only once it drops below 90 does it relax to Warn.
        assert_eq!(
            derive_state(89, 100, &th, SpendState::Degrade, false),
            SpendState::Warn,
            "89% (< 90 deadband boundary) relaxes Degrade → Warn",
        );
        // And the Warn→Allow boundary is warn_pct − deadband = 75.
        assert_eq!(
            derive_state(76, 100, &th, SpendState::Warn, false),
            SpendState::Warn,
            "76% holds Warn (within 75..80 deadband)",
        );
        assert_eq!(
            derive_state(74, 100, &th, SpendState::Warn, false),
            SpendState::Allow,
            "74% (< 75) relaxes Warn → Allow",
        );
    }

    #[test]
    fn block_recovers_immediately_when_limit_raised() {
        let th = t();
        // App is Blocked at a 100-cent limit (spend 100 = 100%). The creator
        // raises the limit to 1000 cents. Same spend is now 10% → Allow. With
        // `limit_changed = true` the deadband is bypassed and the app recovers
        // immediately on the next tick (it does NOT stay pinned in Block).
        assert_eq!(
            derive_state(100, 1000, &th, SpendState::Block, true),
            SpendState::Allow,
            "a raised limit recovers immediately (deadband bypassed)",
        );
        // Sanity: WITHOUT a limit change, the same percentages would still
        // recover here because 10% is far below every deadband boundary — so
        // re-run with a value that WOULD be pinned by the deadband to prove the
        // bypass matters. spend 92 / limit 100 from Block: raw is Warn, but the
        // Block deadband boundary is 95; pct=92 < 95 so it already relaxes one
        // step even without a change. To show the bypass, use spend 96/limit
        // 100 from Block (pct=96 ≥ 95 deadband → would HOLD Block) but with a
        // limit change it should still take raw (Degrade).
        assert_eq!(
            derive_state(96, 100, &th, SpendState::Block, false),
            SpendState::Block,
            "96% holds Block (within the 95..100 deadband)",
        );
        assert_eq!(
            derive_state(96, 100, &th, SpendState::Block, true),
            SpendState::Degrade,
            "with limit_changed, 96% takes raw (Degrade), bypassing the deadband",
        );
    }
}
