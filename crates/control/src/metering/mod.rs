//! Metering snapshots.
//!
//! Usage now reaches control as `UsageEvent`s on the durable stream. The
//! control-side writer for `zeroship.usage_aggregates` is the periodic spend
//! recompute cron: it scans the retained stream for the current billing period,
//! computes `SUM(value)` per `(app_id, metric)`, and overwrites this table as an
//! idempotent period snapshot. Trusted control-plane work emits through the same
//! durable usage-event stream via [`Metering::record_direct`]. Dev deployments
//! without a stream retain an immediate `usage_aggregates += delta` fallback,
//! because no recompute cron exists there. There is no worker report endpoint,
//! no `(worker_id, sequence)` dedup ledger, and no per-report `+=` fold.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use uuid::Uuid;
use zeroship_core::usage_event::{UsageEvent, UsageSubject};

use crate::registry::{Registry, RegistryError};
use crate::BillingStreamConfig;

pub mod provider;

/// Per-`owner_app` cap on auto-registered custom metrics in `billing_metrics`.
///
/// The stream producer should normally emit cataloged platform metrics. The
/// recompute snapshot writer still treats an unknown metric as custom and caps
/// the cardinality so a malformed event cannot FK-abort the whole period
/// snapshot or create unbounded catalog rows.
pub const MAX_CUSTOM_METRICS_PER_APP: i64 = 100;

/// Compute the calendar-month period start (00:00:00 UTC on the 1st) for a
/// given unix-seconds instant.
#[must_use]
pub fn period_start_unix(now_unix: i64) -> i64 {
    let dt = Utc.timestamp_opt(now_unix, 0).single().unwrap_or_else(Utc::now);
    Utc.with_ymd_and_hms(dt.year(), dt.month(), 1, 0, 0, 0)
        .single()
        .map_or(now_unix, |d| d.timestamp())
}

/// The current calendar-month period start as unix seconds.
#[must_use]
pub fn current_period_start_unix() -> i64 {
    period_start_unix(Utc::now().timestamp())
}

/// Convert a unix-seconds period start to the `billing_period` DATE value.
///
/// BIND SITE NOTE: the `period` column is a DATE domain, so writes bind through
/// `$N::date`; Postgres applies the date-to-domain assignment cast.
pub(crate) fn period_date(period_start_unix_secs: i64) -> NaiveDate {
    let dt = Utc
        .timestamp_opt(period_start_unix_secs, 0)
        .single()
        .unwrap_or_else(Utc::now);
    NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
        .unwrap_or_else(|| Utc::now().date_naive().with_day(1).unwrap_or(dt.date_naive()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageAggregate {
    pub app_id: Uuid,
    pub metric: String,
    pub total: i64,
}

/// Period snapshot writer + aggregate read helpers.
#[derive(Clone, Debug)]
pub struct Metering {
    registry: Registry,
}

impl Metering {
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Record trusted control-plane usage in the current billing period.
    pub async fn record_direct(
        &self,
        app_id: &Uuid,
        deltas: &[(String, i64)],
        billing_stream: Option<&BillingStreamConfig>,
    ) -> Result<(), RegistryError> {
        self.record_direct_at(app_id, deltas, Utc::now().timestamp(), billing_stream)
            .await
    }

    /// Apply trusted control-plane usage deltas at an explicit event time.
    ///
    /// With a billing stream, accepted positive deltas become ordinary durable
    /// [`UsageEvent`]s, so both provider forwarding and repeated local snapshot
    /// recomputes observe the same source of truth. Without a stream, the
    /// aggregate table is incremented directly as a dev-only fallback.
    pub async fn record_direct_at(
        &self,
        app_id: &Uuid,
        deltas: &[(String, i64)],
        event_time_unix_secs: i64,
        billing_stream: Option<&BillingStreamConfig>,
    ) -> Result<(), RegistryError> {
        let mut conn = self.registry.conn().await?;
        let tx = conn.transaction().await?;
        let period = period_date(event_time_unix_secs);
        let mut accepted = Vec::with_capacity(deltas.len());
        for (metric, delta) in deltas {
            if *delta <= 0 {
                continue;
            }
            if !Self::register_metric(&tx, app_id, metric).await? {
                tracing::warn!(
                    app_id = %app_id,
                    metric = %metric,
                    billing_event = "custom_metric_cap_refused",
                    "metering: custom metric refused at per-app cap — dropping direct delta"
                );
                continue;
            }
            let value = u64::try_from(*delta).map_err(|_| {
                RegistryError::InvalidInput(format!(
                    "positive direct usage delta for metric {metric} must fit u64"
                ))
            })?;
            accepted.push((metric.clone(), value));
            if billing_stream.is_none() {
                tx.execute(
                    "INSERT INTO zeroship.usage_aggregates AS u \
                       (app_id, period, metric, total, updated_at) \
                     VALUES ($1, $2::date, $3, $4, NOW()) \
                     ON CONFLICT (app_id, period, metric) \
                     DO UPDATE SET total = u.total + EXCLUDED.total, updated_at = NOW()",
                    &[app_id, &period, metric, delta],
                )
                .await?;
            }
        }
        tx.commit().await?;

        let Some(billing_stream) = billing_stream else {
            return Ok(());
        };
        if accepted.is_empty() {
            return Ok(());
        }

        let events: Vec<_> = accepted
            .into_iter()
            .map(|(meter, value)| UsageEvent {
                event_id: Uuid::now_v7().to_string(),
                source: "zeroship-control".to_string(),
                subject: UsageSubject {
                    app: Some(*app_id),
                    creator: Uuid::nil(),
                },
                meter,
                value,
                event_time: event_time_unix_secs,
                dims: BTreeMap::new(),
            })
            .collect();
        let outbox = billing_stream.control_usage_outbox().map_err(|error| {
            RegistryError::Database(format!("initialize control usage outbox: {error}"))
        })?;
        outbox.enqueue_events(&events).map_err(|error| {
            RegistryError::Database(format!("enqueue control usage events: {error}"))
        })?;
        billing_stream.start_control_usage_outbox().map_err(|error| {
            RegistryError::Database(format!("start control usage outbox: {error}"))
        })?;
        Ok(())
    }

    /// Replace every `usage_aggregates` row for `period_start_unix_secs` with
    /// the provided full-period snapshot.
    ///
    /// This is intentionally overwrite semantics, not `+=`: re-running the same
    /// recompute writes the same totals and cannot double-count. The caller must
    /// pass a complete snapshot for the period it is replacing.
    pub async fn replace_period_snapshot(
        &self,
        period_start_unix_secs: i64,
        aggregates: &[UsageAggregate],
    ) -> Result<usize, RegistryError> {
        let mut conn = self.registry.conn().await?;
        let tx = conn.transaction().await?;
        let period = period_date(period_start_unix_secs);

        tx.execute(
            "DELETE FROM zeroship.usage_aggregates WHERE period = $1::date",
            &[&period],
        )
        .await?;

        let mut written = 0usize;
        let mut resolved_cache = HashSet::<(Uuid, String)>::new();
        for aggregate in aggregates {
            if aggregate.total < 0 {
                tracing::warn!(
                    app_id = %aggregate.app_id,
                    metric = %aggregate.metric,
                    total = aggregate.total,
                    "metering: negative snapshot total refused"
                );
                continue;
            }
            if aggregate.total == 0 {
                continue;
            }
            let cache_key = (aggregate.app_id, aggregate.metric.clone());
            if !resolved_cache.contains(&cache_key) {
                if !Self::register_metric(&tx, &aggregate.app_id, &aggregate.metric).await? {
                    tracing::warn!(
                        app_id = %aggregate.app_id,
                        metric = %aggregate.metric,
                        billing_event = "custom_metric_cap_refused",
                        "metering: custom metric refused at per-app cap — dropping snapshot total"
                    );
                    continue;
                }
                resolved_cache.insert(cache_key);
            }

            tx.execute(
                "INSERT INTO zeroship.usage_aggregates \
                   (app_id, period, metric, total, updated_at) \
                 VALUES ($1, $2::date, $3, $4, NOW()) \
                 ON CONFLICT (app_id, period, metric) DO UPDATE SET \
                   total = EXCLUDED.total, updated_at = NOW()",
                &[
                    &aggregate.app_id,
                    &period,
                    &aggregate.metric,
                    &aggregate.total,
                ],
            )
            .await?;
            written += 1;
        }

        tx.commit().await?;
        Ok(written)
    }

    async fn register_metric<C: compio_postgres::GenericClient + Sync>(
        conn: &C,
        owner_app: &Uuid,
        metric: &str,
    ) -> Result<bool, RegistryError> {
        let existing = conn
            .query(
                "SELECT kind::text AS kind FROM zeroship.billing_metrics WHERE metric = $1",
                &[&metric],
            )
            .await?;
        if let Some(row) = existing.first() {
            let kind: String = row.get("kind");
            if kind == "custom" {
                conn.execute(
                    "UPDATE zeroship.billing_metrics SET last_seen_at = NOW() WHERE metric = $1",
                    &[&metric],
                )
                .await?;
            }
            return Ok(true);
        }

        let inserted = conn
            .query(
                "INSERT INTO zeroship.billing_metrics (metric, kind, unit, owner_app, last_seen_at) \
                 SELECT $1, 'custom', 'unit', $2, NOW() \
                 WHERE (SELECT COUNT(*) FROM zeroship.billing_metrics \
                          WHERE owner_app = $2 AND kind = 'custom') < $3 \
                 ON CONFLICT (metric) DO UPDATE SET last_seen_at = NOW() \
                 RETURNING metric",
                &[&metric, owner_app, &MAX_CUSTOM_METRICS_PER_APP],
            )
            .await?;
        Ok(!inserted.is_empty())
    }

    /// All aggregated metric totals for an app in a given period, as a
    /// `metric → total` map.
    pub async fn period_totals(
        &self,
        app_id: &Uuid,
        period_start_unix_secs: i64,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        let conn = self.registry.conn().await?;
        Self::period_totals_on(&conn, app_id, period_start_unix_secs).await
    }

    /// As [`Metering::period_totals`] but on a borrowed connection.
    pub async fn period_totals_on<C: compio_postgres::GenericClient + Sync>(
        conn: &C,
        app_id: &Uuid,
        period_start_unix_secs: i64,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        let rows = conn
            .query(
                "SELECT metric, total FROM zeroship.usage_aggregates \
                 WHERE app_id = $1 AND period = $2::date",
                &[app_id, &period_date(period_start_unix_secs)],
            )
            .await?;
        let mut out = HashMap::new();
        for row in &rows {
            out.insert(row.get::<_, String>("metric"), row.get::<_, i64>("total"));
        }
        Ok(out)
    }

    /// All aggregated metric totals for an app in the current calendar-month
    /// period.
    pub async fn current_period_totals(
        &self,
        app_id: &Uuid,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        self.period_totals(app_id, current_period_start_unix()).await
    }

    /// Read the aggregated total for a `(app_id, period_start, metric)` bucket.
    pub async fn total(
        &self,
        app_id: &Uuid,
        period_start_unix_secs: i64,
        metric: &str,
    ) -> Result<i64, RegistryError> {
        let conn = self.registry.conn().await?;
        let rows = conn
            .query(
                "SELECT total FROM zeroship.usage_aggregates \
                 WHERE app_id = $1 AND period = $2::date \
                   AND metric = $3",
                &[app_id, &period_date(period_start_unix_secs), &metric],
            )
            .await?;
        Ok(rows.first().map_or(0, |r| r.get("total")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_start_is_first_of_month_utc() {
        let mid_june = Utc.with_ymd_and_hms(2026, 6, 13, 12, 34, 56).unwrap().timestamp();
        let ps = period_start_unix(mid_june);
        let expected = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap().timestamp();
        assert_eq!(ps, expected);
    }

    #[test]
    fn period_start_differs_across_month_boundary() {
        let jun = period_start_unix(Utc.with_ymd_and_hms(2026, 6, 30, 23, 59, 59).unwrap().timestamp());
        let jul = period_start_unix(Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap().timestamp());
        assert_ne!(jun, jul, "June and July must land in different periods");
        assert_eq!(jul, Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap().timestamp());
    }
}
