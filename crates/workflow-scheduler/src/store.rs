use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use compio_postgres::{Client, NoTls};
use uuid::Uuid;

use crate::wheel::TimerEntry;

#[derive(Debug, Clone)]
pub struct WorkflowSchedulerStore {
    db_url: String,
    schema: String,
    metrics: Option<Arc<WorkflowSchedulerStoreMetrics>>,
}

#[derive(Debug, Default)]
pub struct WorkflowSchedulerStoreMetrics {
    workflow_runs_reads: AtomicUsize,
}

impl WorkflowSchedulerStoreMetrics {
    #[must_use]
    pub fn workflow_runs_reads(&self) -> usize {
        self.workflow_runs_reads.load(Ordering::SeqCst)
    }

    fn record_workflow_runs_read(&self) {
        self.workflow_runs_reads.fetch_add(1, Ordering::SeqCst);
    }
}

impl WorkflowSchedulerStore {
    #[must_use]
    pub fn new(db_url: impl Into<String>) -> Self {
        Self::new_with_schema(db_url, "workflow_scheduler")
    }

    #[must_use]
    pub fn new_with_schema(db_url: impl Into<String>, schema: impl Into<String>) -> Self {
        let schema = schema.into();
        assert_valid_schema_name(&schema);
        Self {
            db_url: db_url.into(),
            schema,
            metrics: None,
        }
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<WorkflowSchedulerStoreMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    #[allow(clippy::future_not_send)]
    async fn open_conn(&self) -> Result<Client, WorkflowSchedulerStoreError> {
        let (client, connection) = compio_postgres::connect(&self.db_url, NoTls).await?;
        compio::runtime::spawn(async move {
            if let Err(err) = connection.run().await {
                tracing::error!(error = %err, "workflow scheduler pg connection error");
            }
        })
        .detach();
        Ok(client)
    }

    #[allow(clippy::future_not_send)]
    pub async fn provision(&self) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        conn.batch_execute(&self.provision_sql()).await?;
        Ok(())
    }

    #[allow(clippy::future_not_send)]
    pub async fn register_timer(
        &self,
        run_id: &str,
        app_id: Uuid,
        wake_at: DateTime<Utc>,
    ) -> Result<TimerRow, WorkflowSchedulerStoreError> {
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        tx.execute(&format!("DELETE FROM {}.inflight WHERE run_id = $1", self.quoted_schema()), &[&run_id])
            .await?;
        let row = tx
            .query_one(
                &format!("INSERT INTO {}.timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, 0, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = {schema}.timers.generation + 1, \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
                    self.quoted_schema(),
                    schema = self.quoted_schema()
                ),
                &[&run_id, &app_id, &wake_at],
            )
            .await?;
        tx.commit().await?;
        Ok(TimerRow::from_row(&row))
    }

    #[allow(clippy::future_not_send)]
    pub async fn load_window(
        &self,
        horizon: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<TimerRow>, WorkflowSchedulerStoreError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let conn = self.open_conn().await?;
        let rows = conn
            .query(
                &format!("SELECT run_id, app_id, wake_at, generation, registered_at \
                   FROM {}.timers \
                  WHERE wake_at <= $1 \
                  ORDER BY wake_at, run_id \
                  LIMIT $2", self.quoted_schema()),
                &[&horizon, &limit],
            )
            .await?;
        Ok(rows.iter().map(TimerRow::from_row).collect())
    }

    #[allow(clippy::future_not_send)]
    pub async fn move_timer_to_inflight(
        &self,
        entry: &TimerEntry,
        deadline: DateTime<Utc>,
    ) -> Result<Option<FiredTimer>, WorkflowSchedulerStoreError> {
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        let rows = tx
            .query(
                &format!("DELETE FROM {}.timers \
                  WHERE run_id = $1 AND generation = $2 AND wake_at <= now() \
                  RETURNING run_id, app_id, wake_at, generation", self.quoted_schema()),
                &[&entry.run_id, &entry.generation],
            )
            .await?;
        let Some(row) = rows.first() else {
            tx.commit().await?;
            return Ok(None);
        };
        let run_id: String = row.get("run_id");
        let app_id: Uuid = row.get("app_id");
        let wake_at: DateTime<Utc> = row.get("wake_at");
        let generation: i64 = row.get("generation");
        tx.execute(
            &format!("INSERT INTO {}.inflight \
                (run_id, app_id, deadline, dispatch_generation, dispatched_at) \
             VALUES ($1, $2, $3, $4, now()) \
             ON CONFLICT (run_id) DO UPDATE \
                SET app_id = EXCLUDED.app_id, \
                    deadline = EXCLUDED.deadline, \
                    dispatch_generation = EXCLUDED.dispatch_generation, \
                    dispatched_at = now()", self.quoted_schema()),
            &[&run_id, &app_id, &deadline, &generation],
        )
        .await?;
        tx.commit().await?;
        Ok(Some(FiredTimer {
            run_id,
            app_id,
            wake_at,
            dispatch_generation: generation,
            deadline,
        }))
    }

    #[allow(clippy::future_not_send)]
    pub async fn claim_due_timers(
        &self,
        horizon: DateTime<Utc>,
        limit: i64,
        deadline: DateTime<Utc>,
    ) -> Result<Vec<FiredTimer>, WorkflowSchedulerStoreError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        let rows = tx
            .query(
                &format!("WITH due AS ( \
                    SELECT run_id \
                      FROM {schema}.timers \
                     WHERE wake_at <= $1 \
                     ORDER BY wake_at, run_id \
                     LIMIT $2 \
                     FOR UPDATE SKIP LOCKED \
                 ), moved AS ( \
                    DELETE FROM {schema}.timers timers \
                     USING due \
                     WHERE timers.run_id = due.run_id \
                     RETURNING timers.run_id, timers.app_id, timers.wake_at, timers.generation \
                 ), upserted AS ( \
                    INSERT INTO {schema}.inflight \
                        (run_id, app_id, deadline, dispatch_generation, dispatched_at) \
                    SELECT run_id, app_id, $3, generation, now() \
                      FROM moved \
                    ON CONFLICT (run_id) DO UPDATE \
                       SET app_id = EXCLUDED.app_id, \
                           deadline = EXCLUDED.deadline, \
                           dispatch_generation = EXCLUDED.dispatch_generation, \
                           dispatched_at = now() \
                    RETURNING run_id \
                 ) \
                 SELECT moved.run_id, moved.app_id, moved.wake_at, moved.generation, \
                        $3::timestamptz AS deadline \
                   FROM moved \
                   JOIN upserted ON upserted.run_id = moved.run_id \
                  ORDER BY moved.wake_at, moved.run_id",
                    schema = self.quoted_schema()
                ),
                &[&horizon, &limit, &deadline],
            )
            .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|row| FiredTimer {
                run_id: row.get("run_id"),
                app_id: row.get("app_id"),
                wake_at: row.get("wake_at"),
                dispatch_generation: row.get("generation"),
                deadline: row.get("deadline"),
            })
            .collect())
    }

    #[allow(clippy::future_not_send)]
    pub async fn ack_register_next(
        &self,
        run_id: &str,
        app_id: Uuid,
        next_wake_at: DateTime<Utc>,
    ) -> Result<TimerRow, WorkflowSchedulerStoreError> {
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        let rows = tx
            .query(
                &format!("DELETE FROM {}.inflight \
                  WHERE run_id = $1 \
                  RETURNING dispatch_generation", self.quoted_schema()),
                &[&run_id],
            )
            .await?;
        let generation = rows
            .first()
            .map_or(0, |row| row.get::<_, i64>("dispatch_generation") + 1);
        let row = tx
            .query_one(
                &format!("INSERT INTO {}.timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = GREATEST({schema}.timers.generation + 1, EXCLUDED.generation), \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
                    self.quoted_schema(),
                    schema = self.quoted_schema()
                ),
                &[&run_id, &app_id, &next_wake_at, &generation],
            )
            .await?;
        tx.commit().await?;
        Ok(TimerRow::from_row(&row))
    }

    #[allow(clippy::future_not_send)]
    pub async fn ack_terminal(&self, run_id: &str) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        conn.execute(
            &format!("DELETE FROM {}.inflight WHERE run_id = $1", self.quoted_schema()),
            &[&run_id],
        )
        .await?;
        conn.execute(
            &format!("DELETE FROM {}.timers WHERE run_id = $1", self.quoted_schema()),
            &[&run_id],
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::future_not_send)]
    pub async fn reconcile_inflight_to_timer(
        &self,
        run_id: &str,
        wake_at: DateTime<Utc>,
    ) -> Result<Option<TimerRow>, WorkflowSchedulerStoreError> {
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        let rows = tx
            .query(
                &format!("DELETE FROM {}.inflight \
                  WHERE run_id = $1 \
                  RETURNING run_id, app_id, dispatch_generation", self.quoted_schema()),
                &[&run_id],
            )
            .await?;
        let Some(row) = rows.first() else {
            tx.commit().await?;
            return Ok(None);
        };
        let run_id: String = row.get("run_id");
        let app_id: Uuid = row.get("app_id");
        let generation: i64 = row.get::<_, i64>("dispatch_generation") + 1;
        let row = tx
            .query_one(
                &format!("INSERT INTO {}.timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = GREATEST({schema}.timers.generation + 1, EXCLUDED.generation), \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
                    self.quoted_schema(),
                    schema = self.quoted_schema()
                ),
                &[&run_id, &app_id, &wake_at, &generation],
            )
            .await?;
        tx.commit().await?;
        Ok(Some(TimerRow::from_row(&row)))
    }

    #[allow(clippy::future_not_send)]
    pub async fn timer(&self, run_id: &str) -> Result<Option<TimerRow>, WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        let rows = conn
            .query(
                &format!("SELECT run_id, app_id, wake_at, generation, registered_at \
                   FROM {}.timers \
                  WHERE run_id = $1", self.quoted_schema()),
                &[&run_id],
            )
            .await?;
        Ok(rows.first().map(TimerRow::from_row))
    }

    #[allow(clippy::future_not_send)]
    pub async fn inflight(
        &self,
        run_id: &str,
    ) -> Result<Option<InflightTimer>, WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        let rows = conn
            .query(
                &format!("SELECT run_id, app_id, deadline, dispatch_generation, dispatched_at \
                   FROM {}.inflight \
                  WHERE run_id = $1", self.quoted_schema()),
                &[&run_id],
            )
            .await?;
        Ok(rows.first().map(InflightTimer::from_row))
    }

    /// One-shot cutover/disaster-recovery seed from the workflow journal.
    ///
    /// This is intentionally called once during scheduler startup, immediately
    /// after `provision()` and before the wheel can fire. It is not a standing
    /// due-run scan; steady-state scheduling is driven by register/ack writes
    /// and the inflight reaper reads only this crate's private store.
    #[allow(clippy::future_not_send)]
    pub async fn boot_reconcile_from_workflow_runs(
        &self,
    ) -> Result<usize, WorkflowSchedulerStoreError> {
        if let Some(metrics) = &self.metrics {
            metrics.record_workflow_runs_read();
        }
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        let rows = tx
            .query(
                "SELECT id, app_id, wake_at \
                   FROM zeroship.workflow_runs \
                  WHERE state IN ('queued','running','sleeping','waiting','compensating') \
                    AND wake_at IS NOT NULL",
                &[],
            )
            .await?;
        for row in &rows {
            let run_id: String = row.get("id");
            let app_id: Uuid = row.get("app_id");
            let wake_at: DateTime<Utc> = row.get("wake_at");
            tx.execute(
                &format!("INSERT INTO {}.timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, 0, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = GREATEST({schema}.timers.generation, EXCLUDED.generation), \
                        registered_at = now()",
                    self.quoted_schema(),
                    schema = self.quoted_schema()
                ),
                &[&run_id, &app_id, &wake_at],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(rows.len())
    }

    #[allow(clippy::future_not_send)]
    pub async fn claim_lapsed_inflight(
        &self,
        now: DateTime<Utc>,
        limit: i64,
        next_deadline: DateTime<Utc>,
    ) -> Result<Vec<LapsedInflightTimer>, WorkflowSchedulerStoreError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        let rows = tx
            .query(
                &format!("WITH due AS ( \
                    SELECT run_id \
                      FROM {schema}.inflight \
                     WHERE deadline <= $1 \
                     ORDER BY deadline, run_id \
                     LIMIT $2 \
                     FOR UPDATE SKIP LOCKED \
                 ) \
                 UPDATE {schema}.inflight inflight \
                    SET deadline = $3, dispatched_at = now() \
                   FROM due \
                  WHERE inflight.run_id = due.run_id \
                  RETURNING inflight.run_id, inflight.app_id, inflight.deadline, \
                            inflight.dispatch_generation, inflight.dispatched_at",
                    schema = self.quoted_schema()
                ),
                &[&now, &limit, &next_deadline],
            )
            .await?;
        tx.commit().await?;
        Ok(rows.iter().map(LapsedInflightTimer::from_row).collect())
    }

    #[cfg(test)]
    #[allow(clippy::future_not_send)]
    pub async fn clear_for_tests(&self) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        conn.batch_execute(
            &format!("TRUNCATE TABLE {}.inflight, {}.timers", self.quoted_schema(), self.quoted_schema()),
        )
        .await?;
        Ok(())
    }

    fn quoted_schema(&self) -> String {
        quote_ident(&self.schema)
    }

    fn provision_sql(&self) -> String {
        let schema = self.quoted_schema();
        format!(
            "\
CREATE SCHEMA IF NOT EXISTS {schema};
CREATE TABLE IF NOT EXISTS {schema}.timers (
  run_id text PRIMARY KEY,
  app_id uuid NOT NULL,
  wake_at timestamptz NOT NULL,
  generation bigint NOT NULL DEFAULT 0,
  registered_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS timers_due_idx ON {schema}.timers (wake_at);
CREATE TABLE IF NOT EXISTS {schema}.inflight (
  run_id text PRIMARY KEY,
  app_id uuid NOT NULL,
  deadline timestamptz NOT NULL,
  dispatch_generation bigint NOT NULL,
  dispatched_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS inflight_deadline_idx ON {schema}.inflight (deadline);
"
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerRow {
    pub run_id: String,
    pub app_id: Uuid,
    pub wake_at: DateTime<Utc>,
    pub generation: i64,
    pub registered_at: DateTime<Utc>,
}

impl TimerRow {
    fn from_row(row: &compio_postgres::Row) -> Self {
        Self {
            run_id: row.get("run_id"),
            app_id: row.get("app_id"),
            wake_at: row.get("wake_at"),
            generation: row.get("generation"),
            registered_at: row.get("registered_at"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightTimer {
    pub run_id: String,
    pub app_id: Uuid,
    pub deadline: DateTime<Utc>,
    pub dispatch_generation: i64,
    pub dispatched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LapsedInflightTimer {
    pub run_id: String,
    pub app_id: Uuid,
    pub deadline: DateTime<Utc>,
    pub dispatch_generation: i64,
    pub dispatched_at: DateTime<Utc>,
}

impl LapsedInflightTimer {
    fn from_row(row: &compio_postgres::Row) -> Self {
        Self {
            run_id: row.get("run_id"),
            app_id: row.get("app_id"),
            deadline: row.get("deadline"),
            dispatch_generation: row.get("dispatch_generation"),
            dispatched_at: row.get("dispatched_at"),
        }
    }
}

impl InflightTimer {
    fn from_row(row: &compio_postgres::Row) -> Self {
        Self {
            run_id: row.get("run_id"),
            app_id: row.get("app_id"),
            deadline: row.get("deadline"),
            dispatch_generation: row.get("dispatch_generation"),
            dispatched_at: row.get("dispatched_at"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredTimer {
    pub run_id: String,
    pub app_id: Uuid,
    pub wake_at: DateTime<Utc>,
    pub dispatch_generation: i64,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowSchedulerStoreError {
    #[error(transparent)]
    Postgres(#[from] compio_postgres::Error),
}

fn assert_valid_schema_name(schema: &str) {
    assert!(
        is_valid_schema_name(schema),
        "invalid workflow scheduler schema name: {schema}"
    );
}

fn is_valid_schema_name(schema: &str) -> bool {
    let mut chars = schema.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn quote_ident(raw: &str) -> String {
    format!("\"{}\"", raw.replace('"', "\"\""))
}
