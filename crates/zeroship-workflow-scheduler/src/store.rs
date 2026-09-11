use chrono::{DateTime, Utc};
use compio_postgres::{Client, GenericClient, NoTls};
use uuid::Uuid;

use crate::wheel::TimerEntry;

#[derive(Debug, Clone)]
pub struct WorkflowSchedulerStore {
    db_url: String,
    schema: String,
}

impl WorkflowSchedulerStore {
    #[must_use]
    pub fn new(db_url: impl Into<String>) -> Self {
        Self::new_with_schema(db_url, "zeroship")
    }

    #[must_use]
    pub fn new_with_schema(db_url: impl Into<String>, schema: impl Into<String>) -> Self {
        let schema = schema.into();
        assert_valid_schema_name(&schema);
        Self {
            db_url: db_url.into(),
            schema,
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

    /// Create the scheduler schema and tables. TEST AND LOCAL SETUP ONLY.
    ///
    /// Production establishes these objects from
    /// `db/migrations-ts/20260811000100_workflow_scheduler_store.ts`, like every
    /// other platform table. Services must not call this: the first statement is
    /// `CREATE SCHEMA IF NOT EXISTS`, and Postgres checks the database-level
    /// CREATE privilege BEFORE the existence short-circuit, so under any
    /// least-privilege role it fails with SQLSTATE 42501 whether or not the
    /// schema is already there. Use [`Self::ensure_ready`] on a service path.
    ///
    /// The DDL here and the migration are two spellings of the same objects, so
    /// they can drift. `tests/golden_path.sh` is what catches that: it asserts
    /// the columns against a database built by migrations alone.
    #[doc(hidden)]
    #[allow(clippy::future_not_send)]
    pub async fn provision(&self) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        conn.batch_execute(&self.provision_sql()).await?;
        Ok(())
    }

    /// Verify the migration-owned scheduler store is present.
    ///
    /// Replaces the former per-tick `provision()` call on service paths. It
    /// reads rather than writes, so it needs no privilege the store's own
    /// queries do not already need.
    #[allow(clippy::future_not_send)]
    pub async fn ensure_ready(&self) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        let schema = self.quoted_schema();
        let rows = conn
            .query(
                "SELECT to_regclass($1) IS NOT NULL AS timers, \
                        to_regclass($2) IS NOT NULL AS inflight",
                &[
                    &format!("{schema}.workflow_scheduler_timers"),
                    &format!("{schema}.workflow_scheduler_inflight"),
                ],
            )
            .await?;
        let ready = rows
            .first()
            .is_some_and(|row| row.get::<_, bool>("timers") && row.get::<_, bool>("inflight"));
        if ready {
            Ok(())
        } else {
            Err(WorkflowSchedulerStoreError::NotProvisioned {
                schema: self.schema.clone(),
            })
        }
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
        let row = self.register_timer_on(&tx, run_id, app_id, wake_at).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// [`Self::register_timer`] on a borrowed connection, so it composes inside
    /// a caller's existing transaction. The timer store lives in its own schema
    /// but the SAME database as the workflow journal (see
    /// `Registry::workflow_store_db_url`), so a caller that writes a run row and
    /// registers its timer can do both in one transaction and never publish a
    /// run whose timer registration failed.
    #[allow(clippy::future_not_send)]
    pub async fn register_timer_on<C>(
        &self,
        conn: &C,
        run_id: &str,
        app_id: Uuid,
        wake_at: DateTime<Utc>,
    ) -> Result<TimerRow, WorkflowSchedulerStoreError>
    where
        C: GenericClient + Sync,
    {
        conn.execute(&format!("DELETE FROM {}.workflow_scheduler_inflight WHERE run_id = $1", self.quoted_schema()), &[&run_id])
            .await?;
        let row = conn
            .query_one(
                &format!("INSERT INTO {}.workflow_scheduler_timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, 0, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = {schema}.workflow_scheduler_timers.generation + 1, \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
                    self.quoted_schema(),
                    schema = self.quoted_schema()
                ),
                &[&run_id, &app_id, &wake_at],
            )
            .await?;
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
                   FROM {}.workflow_scheduler_timers \
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
                &format!("DELETE FROM {}.workflow_scheduler_timers \
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
            &format!("INSERT INTO {}.workflow_scheduler_inflight \
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
                      FROM {schema}.workflow_scheduler_timers \
                     WHERE wake_at <= $1 \
                     ORDER BY wake_at, run_id \
                     LIMIT $2 \
                     FOR UPDATE SKIP LOCKED \
                 ), moved AS ( \
                    DELETE FROM {schema}.workflow_scheduler_timers timers \
                     USING due \
                     WHERE timers.run_id = due.run_id \
                     RETURNING timers.run_id, timers.app_id, timers.wake_at, timers.generation \
                 ), upserted AS ( \
                    INSERT INTO {schema}.workflow_scheduler_inflight \
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
        let row = self
            .ack_register_next_on(&tx, run_id, app_id, next_wake_at)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    /// [`Self::ack_register_next`] on a borrowed connection. See
    /// [`Self::register_timer_on`] for why composing inside a caller's
    /// transaction is both possible and worth doing.
    #[allow(clippy::future_not_send)]
    pub async fn ack_register_next_on<C>(
        &self,
        conn: &C,
        run_id: &str,
        app_id: Uuid,
        next_wake_at: DateTime<Utc>,
    ) -> Result<TimerRow, WorkflowSchedulerStoreError>
    where
        C: GenericClient + Sync,
    {
        let rows = conn
            .query(
                &format!("DELETE FROM {}.workflow_scheduler_inflight \
                  WHERE run_id = $1 \
                  RETURNING dispatch_generation", self.quoted_schema()),
                &[&run_id],
            )
            .await?;
        let generation = rows
            .first()
            .map_or(0, |row| row.get::<_, i64>("dispatch_generation") + 1);
        let row = conn
            .query_one(
                &format!("INSERT INTO {}.workflow_scheduler_timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = GREATEST({schema}.workflow_scheduler_timers.generation + 1, EXCLUDED.generation), \
                        registered_at = now() \
                 RETURNING run_id, app_id, wake_at, generation, registered_at",
                    self.quoted_schema(),
                    schema = self.quoted_schema()
                ),
                &[&run_id, &app_id, &next_wake_at, &generation],
            )
            .await?;
        Ok(TimerRow::from_row(&row))
    }

    #[allow(clippy::future_not_send)]
    pub async fn ack_terminal(&self, run_id: &str) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        self.ack_terminal_on(&conn, run_id).await
    }

    /// [`Self::ack_terminal`] on a borrowed connection. See
    /// [`Self::register_timer_on`].
    #[allow(clippy::future_not_send)]
    pub async fn ack_terminal_on<C>(
        &self,
        conn: &C,
        run_id: &str,
    ) -> Result<(), WorkflowSchedulerStoreError>
    where
        C: GenericClient + Sync,
    {
        conn.execute(
            &format!("DELETE FROM {}.workflow_scheduler_inflight WHERE run_id = $1", self.quoted_schema()),
            &[&run_id],
        )
        .await?;
        conn.execute(
            &format!("DELETE FROM {}.workflow_scheduler_timers WHERE run_id = $1", self.quoted_schema()),
            &[&run_id],
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::future_not_send)]
    pub async fn ack_park(&self, run_id: &str) -> Result<(), WorkflowSchedulerStoreError> {
        let conn = self.open_conn().await?;
        self.ack_park_on(&conn, run_id).await
    }

    /// [`Self::ack_park`] on a borrowed connection. See
    /// [`Self::register_timer_on`].
    #[allow(clippy::future_not_send)]
    pub async fn ack_park_on<C>(
        &self,
        conn: &C,
        run_id: &str,
    ) -> Result<(), WorkflowSchedulerStoreError>
    where
        C: GenericClient + Sync,
    {
        conn.execute(
            &format!("DELETE FROM {}.workflow_scheduler_inflight WHERE run_id = $1", self.quoted_schema()),
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
                &format!("DELETE FROM {}.workflow_scheduler_inflight \
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
                &format!("INSERT INTO {}.workflow_scheduler_timers \
                    (run_id, app_id, wake_at, generation, registered_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (run_id) DO UPDATE \
                    SET app_id = EXCLUDED.app_id, \
                        wake_at = EXCLUDED.wake_at, \
                        generation = GREATEST({schema}.workflow_scheduler_timers.generation + 1, EXCLUDED.generation), \
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
                   FROM {}.workflow_scheduler_timers \
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
                   FROM {}.workflow_scheduler_inflight \
                  WHERE run_id = $1", self.quoted_schema()),
                &[&run_id],
            )
            .await?;
        Ok(rows.first().map(InflightTimer::from_row))
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
                      FROM {schema}.workflow_scheduler_inflight \
                     WHERE deadline <= $1 \
                     ORDER BY deadline, run_id \
                     LIMIT $2 \
                     FOR UPDATE SKIP LOCKED \
                 ) \
                 UPDATE {schema}.workflow_scheduler_inflight inflight \
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

    fn quoted_schema(&self) -> String {
        quote_ident(&self.schema)
    }

    fn provision_sql(&self) -> String {
        let schema = self.quoted_schema();
        format!(
            "\
CREATE SCHEMA IF NOT EXISTS {schema};
CREATE TABLE IF NOT EXISTS {schema}.workflow_scheduler_timers (
  run_id text PRIMARY KEY,
  app_id uuid NOT NULL,
  wake_at timestamptz NOT NULL,
  generation bigint NOT NULL DEFAULT 0,
  registered_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS workflow_scheduler_timers_due_idx ON {schema}.workflow_scheduler_timers (wake_at);
CREATE TABLE IF NOT EXISTS {schema}.workflow_scheduler_inflight (
  run_id text PRIMARY KEY,
  app_id uuid NOT NULL,
  deadline timestamptz NOT NULL,
  dispatch_generation bigint NOT NULL,
  dispatched_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS workflow_scheduler_inflight_deadline_idx ON {schema}.workflow_scheduler_inflight (deadline);
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
    #[error(
        "workflow scheduler store is missing: schema `{schema}` has no timers/inflight tables. \
         Apply db/migrations-ts (20260811000100_workflow_scheduler_store.ts) against this database"
    )]
    NotProvisioned { schema: String },
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
