use chrono::{DateTime, Utc};
use compio_postgres::{Client, NoTls};
use uuid::Uuid;

use crate::wheel::TimerEntry;

#[derive(Debug, Clone)]
pub struct WorkflowSchedulerStore {
    db_url: String,
}

impl WorkflowSchedulerStore {
    #[must_use]
    pub fn new(db_url: impl Into<String>) -> Self {
        Self {
            db_url: db_url.into(),
        }
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
        conn.batch_execute(PROVISION_SQL).await?;
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
        tx.execute(
            "DELETE FROM workflow_scheduler.inflight WHERE run_id = $1",
            &[&run_id],
        )
        .await?;
        let row = tx
            .query_one(
                "INSERT INTO workflow_scheduler.timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, 0, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = workflow_scheduler.timers.generation + 1, \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
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
                "SELECT run_id, app_id, wake_at, generation, registered_at \
                   FROM workflow_scheduler.timers \
                  WHERE wake_at <= $1 \
                  ORDER BY wake_at, run_id \
                  LIMIT $2",
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
                "DELETE FROM workflow_scheduler.timers \
                  WHERE run_id = $1 AND generation = $2 AND wake_at <= now() \
                  RETURNING run_id, app_id, wake_at, generation",
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
            "INSERT INTO workflow_scheduler.inflight \
                (run_id, app_id, deadline, dispatch_generation, dispatched_at) \
             VALUES ($1, $2, $3, $4, now()) \
             ON CONFLICT (run_id) DO UPDATE \
                SET app_id = EXCLUDED.app_id, \
                    deadline = EXCLUDED.deadline, \
                    dispatch_generation = EXCLUDED.dispatch_generation, \
                    dispatched_at = now()",
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
                "DELETE FROM workflow_scheduler.inflight \
                  WHERE run_id = $1 \
                  RETURNING dispatch_generation",
                &[&run_id],
            )
            .await?;
        let generation = rows
            .first()
            .map_or(0, |row| row.get::<_, i64>("dispatch_generation") + 1);
        let row = tx
            .query_one(
                "INSERT INTO workflow_scheduler.timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = GREATEST(workflow_scheduler.timers.generation + 1, EXCLUDED.generation), \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
                &[&run_id, &app_id, &next_wake_at, &generation],
            )
            .await?;
        tx.commit().await?;
        Ok(TimerRow::from_row(&row))
    }

    #[allow(clippy::future_not_send)]
    pub async fn ack_terminal(&self, run_id: &str) -> Result<(), WorkflowSchedulerStoreError> {
        let mut conn = self.open_conn().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "DELETE FROM workflow_scheduler.inflight WHERE run_id = $1",
            &[&run_id],
        )
        .await?;
        tx.execute(
            "DELETE FROM workflow_scheduler.timers WHERE run_id = $1",
            &[&run_id],
        )
        .await?;
        tx.commit().await?;
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
                "DELETE FROM workflow_scheduler.inflight \
                  WHERE run_id = $1 \
                  RETURNING run_id, app_id, dispatch_generation",
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
                "INSERT INTO workflow_scheduler.timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = GREATEST(workflow_scheduler.timers.generation + 1, EXCLUDED.generation), \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
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
                "SELECT run_id, app_id, wake_at, generation, registered_at \
                   FROM workflow_scheduler.timers \
                  WHERE run_id = $1",
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
                "SELECT run_id, app_id, deadline, dispatch_generation, dispatched_at \
                   FROM workflow_scheduler.inflight \
                  WHERE run_id = $1",
                &[&run_id],
            )
            .await?;
        Ok(rows.first().map(InflightTimer::from_row))
    }

    #[cfg(test)]
    #[allow(clippy::future_not_send)]
    pub async fn clear_for_tests(&self) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        conn.batch_execute(
            "TRUNCATE TABLE workflow_scheduler.inflight, workflow_scheduler.timers",
        )
        .await?;
        Ok(())
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

const PROVISION_SQL: &str = "\
CREATE SCHEMA IF NOT EXISTS workflow_scheduler;
CREATE TABLE IF NOT EXISTS workflow_scheduler.timers (
  run_id text PRIMARY KEY,
  app_id uuid NOT NULL,
  wake_at timestamptz NOT NULL,
  generation bigint NOT NULL DEFAULT 0,
  registered_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS timers_due_idx ON workflow_scheduler.timers (wake_at);
CREATE TABLE IF NOT EXISTS workflow_scheduler.inflight (
  run_id text PRIMARY KEY,
  app_id uuid NOT NULL,
  deadline timestamptz NOT NULL,
  dispatch_generation bigint NOT NULL,
  dispatched_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS inflight_deadline_idx ON workflow_scheduler.inflight (deadline);
";
