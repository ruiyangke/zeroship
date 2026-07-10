use async_trait::async_trait;
use chrono::{DateTime, Utc};
use compio_postgres::{Client, GenericClient, NoTls};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::typed_id;

use crate::engine::{
    cap_exceeded, child_dedup_key, child_signal_type, state_cap_error, StepCheckpoint,
    WorkflowEngineConfig, WorkflowOutputRef,
};
use crate::errors::WorkflowError;
use crate::store::{
    ChildTerminalPayload, CompensatingRunUpdate, CompensationProgress, PausedRunUpdate, RunLockRow,
    StepWriteOutcome, TransitionRunUpdate, WorkflowStore, WorkflowTx,
};

const BLOB_REF_JOURNAL_BYTES: i64 = 160;
pub const JOURNAL_TABLE_SUFFIXES: [&str; 5] = ["runs", "steps", "signals", "subscriptions", "blobs"];

#[derive(Clone, Debug)]
pub struct PgStore {
    db_url: String,
    tables: WorkflowTables,
}

#[derive(Clone, Debug)]
pub struct WorkflowTables {
    pub app_id: Uuid,
    pub app_schema: String,
    pub runs: String,
    pub steps: String,
    pub signals: String,
    pub subscriptions: String,
    pub blobs: String,
}

impl PgStore {
    #[must_use]
    pub fn new(db_url: impl Into<String>, app_id: Uuid) -> Self {
        let tables = WorkflowTables::for_app_id(&app_id);
        Self {
            db_url: db_url.into(),
            tables,
        }
    }

    #[must_use]
    pub fn tables(&self) -> &WorkflowTables {
        &self.tables
    }

    pub async fn provision<C>(platform_client: &C, app_id: &Uuid) -> Result<WorkflowTables, WorkflowError>
    where
        C: GenericClient + Sync,
    {
        let tables = WorkflowTables::for_app_id(app_id);
        platform_client
            .batch_execute(&provision_sql(&tables))
            .await?;
        reassert_table_revokes(platform_client, &tables).await?;
        Ok(tables)
    }
}

impl WorkflowTables {
    #[must_use]
    pub fn for_app_id(app_id: &Uuid) -> Self {
        let app_schema = app_schema_for(app_id);
        let schema = quote_ident(&app_schema);
        Self {
            app_id: *app_id,
            app_schema,
            runs: format!("{schema}.{}", quote_ident("__zeroship_workflow_runs")),
            steps: format!("{schema}.{}", quote_ident("__zeroship_workflow_steps")),
            signals: format!("{schema}.{}", quote_ident("__zeroship_workflow_signals")),
            subscriptions: format!("{schema}.{}", quote_ident("__zeroship_workflow_subscriptions")),
            blobs: format!("{schema}.{}", quote_ident("__zeroship_workflow_blobs")),
        }
    }

    #[must_use]
    pub fn qualified(&self, name: &str) -> String {
        let table = format!("__zeroship_workflow_{name}");
        format!("{}.{}", quote_ident(&self.app_schema), quote_ident(&table))
    }

    #[must_use]
    pub fn all(&self) -> [&str; 5] {
        [&self.runs, &self.steps, &self.signals, &self.subscriptions, &self.blobs]
    }

    #[must_use]
    pub fn regclass_literal(&self, suffix: &str) -> String {
        format!("{}.{}", self.app_schema, format!("__zeroship_workflow_{suffix}")).replace('\'', "''")
    }
}

#[must_use]
pub fn app_schema_for(app_id: &Uuid) -> String {
    format!("app_{}", app_id.as_hyphenated())
}

#[must_use]
pub fn quote_ident(ident: &str) -> String {
    let escaped = ident.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

#[derive(Debug)]
pub struct PgTx {
    conn: Client,
    tables: WorkflowTables,
}

async fn open_conn(url: &str) -> Result<Client, WorkflowError> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "workflow PgStore connection error");
        }
    })
    .detach();
    Ok(client)
}

fn provision_sql(tables: &WorkflowTables) -> String {
    let schema = quote_ident(&tables.app_schema);
    format!(
        r#"
CREATE SCHEMA IF NOT EXISTS {schema};

CREATE TABLE IF NOT EXISTS {runs} (
  id text PRIMARY KEY,
  workflow_name text NOT NULL,
  app_id uuid NOT NULL,
  deploy_id text NOT NULL,
  state text NOT NULL,
  input jsonb,
  output jsonb,
  error jsonb,
  output_kind text NOT NULL DEFAULT 'inline',
  output_hash char(64),
  output_size bigint,
  output_content_type text,
  input_hash char(64),
  input_size bigint,
  input_content_type text,
  journal_bytes bigint NOT NULL DEFAULT 0,
  blob_bytes bigint NOT NULL DEFAULT 0,
  wake_at timestamptz,
  claimed_by text,
  claim_epoch integer NOT NULL DEFAULT 0,
  lease_expires timestamptz,
  dispatch_nonce text,
  last_dispatch_at timestamptz,
  concurrency smallint NOT NULL DEFAULT 1,
  next_ordinal integer NOT NULL DEFAULT 0,
  stuck_strikes smallint NOT NULL DEFAULT 0,
  waiting_step_key text,
  paused_from_status text,
  signal_epoch integer NOT NULL DEFAULT 0,
  parent_run_id text,
  parent_wait_step_key text,
  parent_cascade boolean NOT NULL DEFAULT false,
  tree_depth smallint NOT NULL DEFAULT 0,
  cancel_requested boolean NOT NULL DEFAULT false,
  compensation_target text,
  compensation_outcome text,
  restart_count smallint NOT NULL DEFAULT 0,
  restarted_at timestamptz,
  restarted_from_ordinal integer,
  restarted_by text,
  dedup_key text,
  started_at timestamptz NOT NULL,
  terminal_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT workflow_runs_state_check CHECK (state IN ('queued', 'running', 'sleeping', 'waiting', 'paused', 'stalled', 'compensating', 'completed', 'failed', 'cancelled')),
  CONSTRAINT workflow_runs_compensation_target_check CHECK (compensation_target IS NULL OR compensation_target IN ('failed', 'cancelled')),
  CONSTRAINT workflow_runs_compensation_outcome_check CHECK (compensation_outcome IS NULL OR compensation_outcome IN ('completed', 'partial')),
  CONSTRAINT workflow_runs_output_check CHECK ((output_kind = 'inline' AND output_hash IS NULL) OR (output_kind = 'blob' AND output_hash IS NOT NULL AND output_size IS NOT NULL AND output IS NULL AND output_hash ~ '^[0-9a-f]{{64}}$')),
  CONSTRAINT workflow_runs_input_check CHECK (input IS NOT NULL OR input_hash IS NOT NULL),
  CONSTRAINT workflow_runs_parent_check CHECK ((parent_run_id IS NULL) = (parent_wait_step_key IS NULL)),
  CONSTRAINT workflow_runs_parent_run_id_fkey FOREIGN KEY (parent_run_id) REFERENCES {runs}(id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX IF NOT EXISTS workflow_runs_app_id_workflow_name_dedup_key_key ON {runs} (app_id, workflow_name, dedup_key);
CREATE INDEX IF NOT EXISTS workflow_runs_wake_due_idx ON {runs} (wake_at) WHERE state IN ('sleeping', 'waiting', 'compensating') AND wake_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS workflow_runs_lease_sweep_idx ON {runs} (lease_expires) WHERE claimed_by IS NOT NULL;
CREATE INDEX IF NOT EXISTS workflow_runs_live_children_idx ON {runs} (parent_run_id) WHERE parent_run_id IS NOT NULL AND state NOT IN ('completed', 'failed', 'cancelled', 'stalled');
CREATE INDEX IF NOT EXISTS workflow_runs_cancel_requested_idx ON {runs} (id) WHERE cancel_requested;
CREATE INDEX IF NOT EXISTS workflow_runs_terminal_retention_idx ON {runs} (terminal_at, tree_depth DESC, id) WHERE state IN ('completed', 'failed', 'cancelled', 'stalled') AND terminal_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS {blobs} (
  hash char(64) PRIMARY KEY,
  size bigint NOT NULL,
  content_type text NOT NULL,
  refcount integer NOT NULL DEFAULT 0,
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  last_referenced_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT workflow_blobs_hash_check CHECK (hash ~ '^[0-9a-f]{{64}}$')
);
CREATE INDEX IF NOT EXISTS workflow_blobs_gc_idx ON {blobs} (last_referenced_at) WHERE refcount = 0;

CREATE TABLE IF NOT EXISTS {steps} (
  run_id text NOT NULL,
  ordinal integer NOT NULL,
  name text NOT NULL,
  name_occurrence integer NOT NULL DEFAULT 0,
  kind text NOT NULL,
  state text NOT NULL,
  attempt integer NOT NULL DEFAULT 0,
  max_attempts integer NOT NULL DEFAULT 1,
  output jsonb,
  error jsonb,
  output_kind text NOT NULL DEFAULT 'inline',
  output_hash char(64),
  output_size bigint,
  output_content_type text,
  wake_at timestamptz,
  signal_type text,
  max_signal_age_ms bigint,
  consumed_signal_id text,
  child_run_id text,
  batch_id text NOT NULL,
  batch_width smallint NOT NULL DEFAULT 1,
  started_at timestamptz NOT NULL DEFAULT now(),
  finished_at timestamptz,
  compensation_state text,
  compensation_attempt integer NOT NULL DEFAULT 0,
  compensation_max_attempts integer NOT NULL DEFAULT 1,
  compensation_wake_at timestamptz,
  compensation_error jsonb,
  compensation_batch_id text,
  compensation_finished_at timestamptz,
  PRIMARY KEY (run_id, ordinal),
  CONSTRAINT workflow_steps_run_id_fkey FOREIGN KEY (run_id) REFERENCES {runs}(id) ON DELETE CASCADE,
  CONSTRAINT workflow_steps_kind_check CHECK (kind IN ('run', 'sideEffect', 'sleep', 'wait_signal', 'child')),
  CONSTRAINT workflow_steps_state_check CHECK (state IN ('running', 'completed', 'failed')),
  CONSTRAINT workflow_steps_output_check CHECK ((output_kind = 'inline' AND output_hash IS NULL) OR (output_kind = 'blob' AND output_hash IS NOT NULL AND output_size IS NOT NULL AND output IS NULL AND output_hash ~ '^[0-9a-f]{{64}}$')),
  CONSTRAINT workflow_steps_compensation_state_check CHECK (compensation_state IS NULL OR compensation_state IN ('pending', 'running', 'completed', 'failed')),
  CONSTRAINT workflow_steps_compensation_kind_check CHECK (compensation_state IS NULL OR kind = 'run')
);
CREATE UNIQUE INDEX IF NOT EXISTS workflow_steps_run_id_name_name_occurrence_key ON {steps} (run_id, name, name_occurrence);
CREATE INDEX IF NOT EXISTS workflow_steps_running_wake_idx ON {steps} (run_id, wake_at) WHERE state = 'running' AND wake_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS workflow_steps_compensation_frontier_idx ON {steps} (run_id, ordinal DESC) WHERE compensation_state IN ('pending', 'running');
CREATE INDEX IF NOT EXISTS workflow_steps_compensation_wake_idx ON {steps} (run_id, compensation_wake_at) WHERE compensation_state = 'running' AND compensation_wake_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS {signals} (
  id text PRIMARY KEY,
  run_id text NOT NULL,
  type text NOT NULL,
  payload jsonb,
  created_at timestamptz NOT NULL DEFAULT now(),
  consumed_by text,
  origin text NOT NULL DEFAULT 'app',
  delivery text NOT NULL DEFAULT 'direct',
  topic text,
  broadcast_id text,
  idempotency_key text,
  provider text,
  CONSTRAINT workflow_signals_run_id_fkey FOREIGN KEY (run_id) REFERENCES {runs}(id) ON DELETE CASCADE,
  CONSTRAINT workflow_signals_origin_check CHECK (origin IN ('app', 'ingress', 'system')),
  CONSTRAINT workflow_signals_delivery_check CHECK (delivery IN ('direct', 'topic')),
  CONSTRAINT workflow_signals_topic_check CHECK ((delivery = 'topic') = (topic IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS workflow_signals_pending_idx ON {signals} (run_id, type) WHERE consumed_by IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS workflow_signals_ext_idem_uidx ON {signals} (run_id, type, idempotency_key) WHERE idempotency_key IS NOT NULL AND delivery <> 'topic';
CREATE UNIQUE INDEX IF NOT EXISTS workflow_signals_bcast_run_uidx ON {signals} (broadcast_id, run_id) WHERE broadcast_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS {subscriptions} (
  id text PRIMARY KEY,
  app_id uuid NOT NULL,
  topic text NOT NULL,
  run_id text NOT NULL,
  signal_name text NOT NULL,
  type_filter text,
  ordinal integer NOT NULL,
  max_age_ms bigint,
  created_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz,
  CONSTRAINT workflow_subscriptions_run_id_fkey FOREIGN KEY (run_id) REFERENCES {runs}(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX IF NOT EXISTS workflow_subscriptions_run_id_ordinal_key ON {subscriptions} (run_id, ordinal);
CREATE INDEX IF NOT EXISTS workflow_subscriptions_app_topic_idx ON {subscriptions} (app_id, topic);
"#,
        runs = tables.runs,
        steps = tables.steps,
        signals = tables.signals,
        subscriptions = tables.subscriptions,
        blobs = tables.blobs,
    )
}

async fn reassert_table_revokes<C>(conn: &C, tables: &WorkflowTables) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let all_tables = tables.all().join(", ");
    let app_role = format!("app_{}_role", tables.app_id.as_hyphenated());
    let schema_role = format!("app_{}_role", tables.app_schema);
    let sql = format!(
        "REVOKE ALL ON TABLE {all_tables} FROM PUBLIC; \
         DO $$ \
         BEGIN \
           IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {role_literal}) THEN \
             EXECUTE 'REVOKE ALL ON TABLE {escaped_tables} FROM ' || quote_ident({role_literal}); \
           END IF; \
           IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {schema_role_literal}) THEN \
             EXECUTE 'REVOKE ALL ON TABLE {escaped_tables} FROM ' || quote_ident({schema_role_literal}); \
           END IF; \
         END \
         $$;",
        role_literal = sql_string_literal(&app_role),
        schema_role_literal = sql_string_literal(&schema_role),
        escaped_tables = all_tables.replace('\'', "''"),
    );
    conn.batch_execute(&sql).await?;
    Ok(())
}

#[must_use]
pub fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[async_trait(?Send)]
impl WorkflowStore for PgStore {
    type Tx = PgTx;

    async fn begin(&self) -> Result<Self::Tx, WorkflowError> {
        let conn = open_conn(&self.db_url).await?;
        conn.batch_execute("BEGIN").await?;
        Ok(PgTx {
            conn,
            tables: self.tables.clone(),
        })
    }
}

#[async_trait(?Send)]
impl WorkflowTx for PgTx {
    async fn commit(self) -> Result<(), WorkflowError> {
        self.conn.batch_execute("COMMIT").await?;
        Ok(())
    }

    async fn lock_run_for_apply(
        &mut self,
        run_id: &str,
    ) -> Result<Option<RunLockRow>, WorkflowError> {
        let sql = format!(
            "SELECT app_id, deploy_id, claimed_by, state, dispatch_nonce, stuck_strikes, \
                    tree_depth, compensation_target, error \
               FROM {runs} \
              WHERE id = $1 \
              FOR UPDATE",
            runs = self.tables.runs
        );
        let rows = self
            .conn
            .query(&sql, &[&run_id])
            .await?;
        Ok(rows.first().map(|row| RunLockRow {
            app_id: row.get("app_id"),
            deploy_id: row.get("deploy_id"),
            claimed_by: row.get("claimed_by"),
            state: row.get("state"),
            dispatch_nonce: row.get("dispatch_nonce"),
            stuck_strikes: row.get("stuck_strikes"),
            tree_depth: row.get("tree_depth"),
            compensation_target: row.get("compensation_target"),
            current_error: row.get("error"),
        }))
    }

    async fn apply_compensation_outcome(
        &mut self,
        run_id: &str,
        dispatch_nonce: &str,
        ordinal: i32,
        name: &str,
        name_occurrence: i32,
        state: &'static str,
        error: Option<&Value>,
    ) -> Result<(), WorkflowError> {
        let sql = format!(
            "UPDATE {steps} s \
                SET compensation_state = $5, \
                    compensation_attempt = compensation_attempt + 1, \
                    compensation_wake_at = NULL, \
                    compensation_error = $6, \
                    compensation_batch_id = $7, \
                    compensation_finished_at = now() \
              WHERE s.run_id = $1 \
                AND s.ordinal = $2 \
                AND s.name = $3 \
                AND s.name_occurrence = $4 \
                AND s.kind = 'run' \
                AND s.compensation_state IN ('pending','running') \
                AND NOT EXISTS ( \
                    SELECT 1 \
                      FROM {steps} higher \
                     WHERE higher.run_id = s.run_id \
                       AND higher.ordinal > s.ordinal \
                       AND higher.compensation_state IN ('pending','running') \
                )",
            steps = self.tables.steps
        );
        self.conn
            .execute(
                &sql,
                &[
                    &run_id,
                    &ordinal,
                    &name,
                    &name_occurrence,
                    &state,
                    &error,
                    &dispatch_nonce,
                ],
            )
            .await?;
        Ok(())
    }

    async fn pending_compensation_count(&mut self, run_id: &str) -> Result<i64, WorkflowError> {
        let sql = format!(
            "SELECT COUNT(*)::bigint AS n \
               FROM {steps} \
              WHERE run_id = $1 AND compensation_state = 'pending'",
            steps = self.tables.steps
        );
        let row = self
            .conn
            .query_one(&sql, &[&run_id])
            .await?;
        Ok(row.get("n"))
    }

    async fn compensation_progress(
        &mut self,
        run_id: &str,
    ) -> Result<CompensationProgress, WorkflowError> {
        compensation_progress_on_conn(&self.conn, &self.tables, run_id).await
    }

    async fn next_compensation_wake_at(
        &mut self,
        run_id: &str,
    ) -> Result<Option<DateTime<Utc>>, WorkflowError> {
        let sql = format!(
            "SELECT \
                EXISTS ( \
                    SELECT 1 FROM {steps} \
                     WHERE run_id = $1 AND compensation_state = 'pending' \
                ) AS has_pending, \
                MIN(compensation_wake_at) FILTER (WHERE compensation_state = 'running') AS running_wake \
               FROM {steps} \
              WHERE run_id = $1",
            steps = self.tables.steps
        );
        let row = self
            .conn
            .query_one(&sql, &[&run_id])
            .await?;
        if row.get::<_, bool>("has_pending") {
            return Ok(Some(Utc::now()));
        }
        Ok(row
            .get::<_, Option<DateTime<Utc>>>("running_wake")
            .or_else(|| Some(Utc::now())))
    }

    async fn update_compensating_run(
        &mut self,
        config: &WorkflowEngineConfig,
        run_id: &str,
        dispatch_nonce: &str,
        update: &CompensatingRunUpdate,
    ) -> Result<u64, WorkflowError> {
        let sql = format!(
            "UPDATE {runs} \
                SET state = $1, \
                    output = NULL, \
                    error = $2, \
                    wake_at = $3, \
                    terminal_at = CASE \
                        WHEN $1 IN ('completed','failed','cancelled','stalled') THEN now() \
                        ELSE NULL \
                    END, \
                    waiting_step_key = NULL, \
                    compensation_outcome = $4, \
                    paused_from_status = NULL, \
                    stuck_strikes = 0, \
                    output_kind = 'inline', \
                    output_hash = NULL, \
                    output_size = NULL, \
                    output_content_type = NULL, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $5 \
                AND claimed_by = $6 \
                AND dispatch_nonce = $7 \
                AND state = 'compensating'",
            runs = self.tables.runs
        );
        self.conn
            .execute(
                &sql,
                &[
                    &update.state,
                    &update.error,
                    &update.wake_at,
                    &update.compensation_outcome,
                    &run_id,
                    &config.owner_id,
                    &dispatch_nonce,
                ],
            )
            .await
            .map_err(WorkflowError::from)
    }

    async fn prepare_child_spawn(
        &mut self,
        config: &WorkflowEngineConfig,
        parent_run_id: &str,
        app_id: &Uuid,
        deploy_id: &str,
        parent_tree_depth: i16,
        checkpoint: &mut StepCheckpoint,
    ) -> Result<Result<(), String>, WorkflowError> {
        prepare_child_spawn(
            &self.conn,
            &self.tables,
            config,
            parent_run_id,
            app_id,
            deploy_id,
            parent_tree_depth,
            checkpoint,
        )
        .await
    }

    async fn insert_resolved_step(
        &mut self,
        config: &WorkflowEngineConfig,
        checkpoint: &StepCheckpoint,
        run_id: &str,
        batch_id: &str,
        batch_width: i16,
    ) -> Result<StepWriteOutcome, WorkflowError> {
        insert_resolved_step_on_conn(
            &self.conn,
            &self.tables,
            config,
            checkpoint,
            run_id,
            batch_id,
            batch_width,
        )
        .await
    }

    async fn upsert_subscription(
        &mut self,
        app_id: &Uuid,
        run_id: &str,
        checkpoint: &StepCheckpoint,
    ) -> Result<(), WorkflowError> {
        let Some(topic) = checkpoint.topic.as_ref().filter(|topic| !topic.is_empty()) else {
            return Ok(());
        };
        let id = typed_id::new_workflow_subscription_id();
        let sql = format!(
            "INSERT INTO {subscriptions} \
            (id, app_id, topic, run_id, signal_name, type_filter, ordinal, max_age_ms, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (run_id, ordinal) DO UPDATE SET \
            app_id = EXCLUDED.app_id, \
            topic = EXCLUDED.topic, \
            signal_name = EXCLUDED.signal_name, \
            type_filter = EXCLUDED.type_filter, \
            max_age_ms = EXCLUDED.max_age_ms, \
            expires_at = EXCLUDED.expires_at",
            subscriptions = self.tables.subscriptions
        );
        self.conn
            .execute(
                &sql,
                &[
                    &id,
                    app_id,
                    topic,
                    &run_id,
                    &checkpoint.name,
                    &checkpoint.signal_type,
                    &checkpoint.ordinal,
                    &checkpoint.max_signal_age_ms,
                    &checkpoint.wake_at,
                ],
            )
            .await?;
        Ok(())
    }

    async fn delete_subscription(
        &mut self,
        run_id: &str,
        ordinal: i32,
    ) -> Result<(), WorkflowError> {
        let sql = format!(
            "DELETE FROM {subscriptions} \
          WHERE run_id = $1 AND ordinal = $2",
            subscriptions = self.tables.subscriptions
        );
        self.conn
            .execute(&sql, &[&run_id, &ordinal])
            .await?;
        Ok(())
    }

    async fn mark_signal_consumed(
        &mut self,
        run_id: &str,
        signal_id: &str,
    ) -> Result<(), WorkflowError> {
        let sql = format!(
            "UPDATE {signals} \
                            SET consumed_by = $1 \
                          WHERE id = $2 AND consumed_by IS NULL",
            signals = self.tables.signals
        );
        self.conn
            .execute(&sql, &[&run_id, &signal_id])
            .await?;
        Ok(())
    }

    async fn write_paused_run(
        &mut self,
        config: &WorkflowEngineConfig,
        run_id: &str,
        dispatch_nonce: &str,
        update: &PausedRunUpdate,
    ) -> Result<u64, WorkflowError> {
        let sql = format!(
            "UPDATE {runs} \
                SET state = 'paused', \
                    wake_at = $1, \
                    terminal_at = NULL, \
                    next_ordinal = GREATEST(next_ordinal, $2), \
                    waiting_step_key = $3, \
                    paused_from_status = $4, \
                    stuck_strikes = 0, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $5 \
                AND claimed_by = $6 \
                AND dispatch_nonce = $7 \
                AND state = 'paused'",
            runs = self.tables.runs
        );
        self.conn
            .execute(
                &sql,
                &[
                    &update.wake_at,
                    &update.next_ordinal,
                    &update.waiting_step_key,
                    &update.paused_from_status,
                    &run_id,
                    &config.owner_id,
                    &dispatch_nonce,
                ],
            )
            .await
            .map_err(WorkflowError::from)
    }

    async fn run_output_journal_bytes(
        &mut self,
        output: &Option<Value>,
        output_ref: Option<&WorkflowOutputRef>,
    ) -> Result<i64, WorkflowError> {
        run_output_journal_bytes(&self.conn, output, output_ref).await
    }

    async fn transition_run(
        &mut self,
        config: &WorkflowEngineConfig,
        run_id: &str,
        dispatch_nonce: &str,
        update: &TransitionRunUpdate,
    ) -> Result<u64, WorkflowError> {
        let sql = format!(
            "UPDATE {runs} \
                SET state = $1, \
                    output = $2, \
                    error = $3, \
                    wake_at = $4, \
                    terminal_at = CASE \
                        WHEN $1 IN ('completed','failed','cancelled','stalled') THEN now() \
                        ELSE NULL \
                    END, \
                    next_ordinal = GREATEST(next_ordinal, $5), \
                    waiting_step_key = $6, \
                    paused_from_status = NULL, \
                    stuck_strikes = $7, \
                    output_kind = $11, \
                    output_hash = $12, \
                    output_size = $13, \
                    output_content_type = $14, \
                    journal_bytes = journal_bytes + $15, \
                    blob_bytes = blob_bytes + $16, \
                    compensation_target = $17, \
                    compensation_outcome = $18, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $8 \
                AND claimed_by = $9 \
                AND dispatch_nonce = $10 \
                AND state = 'running'",
            runs = self.tables.runs
        );
        self.conn
            .execute(
                &sql,
                &[
                    &update.state,
                    &update.output,
                    &update.error,
                    &update.wake_at,
                    &update.next_ordinal,
                    &update.waiting_step_key,
                    &update.stuck_strikes,
                    &run_id,
                    &config.owner_id,
                    &dispatch_nonce,
                    &update.output_kind,
                    &update.output_hash,
                    &update.output_size,
                    &update.output_content_type,
                    &update.run_journal_delta,
                    &update.blob_bytes_delta,
                    &update.compensation_target,
                    &update.compensation_outcome,
                ],
            )
            .await
            .map_err(WorkflowError::from)
    }

    async fn upsert_blob_ref(
        &mut self,
        output_ref: &WorkflowOutputRef,
    ) -> Result<(), WorkflowError> {
        upsert_workflow_blob_ref(&self.conn, &self.tables, output_ref).await
    }

    async fn emit_child_terminal_signal(
        &mut self,
        child_run_id: &str,
        terminal: ChildTerminalPayload<'_>,
    ) -> Result<(), WorkflowError> {
        emit_child_terminal_hook_on_conn(&self.conn, &self.tables, child_run_id, terminal).await
    }

    async fn cascade_cancel_children(
        &mut self,
        parent_run_id: &str,
    ) -> Result<u64, WorkflowError> {
        cascade_cancel_children_on_conn(&self.conn, &self.tables, parent_run_id).await
    }
}

async fn prepare_child_spawn<C>(
    conn: &C,
    tables: &WorkflowTables,
    config: &WorkflowEngineConfig,
    parent_run_id: &str,
    app_id: &Uuid,
    deploy_id: &str,
    parent_tree_depth: i16,
    checkpoint: &mut StepCheckpoint,
) -> Result<Result<(), String>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let existing_step_sql = format!(
        "SELECT child_run_id, signal_type \
               FROM {steps} \
              WHERE run_id = $1 \
                AND ordinal = $2 \
                AND kind = 'child' \
                AND state = 'running' \
              FOR UPDATE",
        steps = tables.steps
    );
    let existing_step = conn
        .query(&existing_step_sql, &[&parent_run_id, &checkpoint.ordinal])
        .await?;
    if let Some(row) = existing_step.first() {
        checkpoint.child_run_id = row.get("child_run_id");
        checkpoint.signal_type = row
            .get::<_, Option<String>>("signal_type")
            .or_else(|| Some(child_signal_type(checkpoint.ordinal)));
        return Ok(Ok(()));
    }

    let Some(child_workflow_name) = checkpoint.child_workflow_name.clone() else {
        return Ok(Err("child workflow name is missing".to_string()));
    };
    if child_workflow_name.is_empty() || child_workflow_name.len() > 128 {
        return Ok(Err("child workflow name must be 1-128 bytes".to_string()));
    }
    if child_workflow_name.starts_with("__zs.") {
        return Ok(Err("child workflow name uses a reserved prefix".to_string()));
    }

    let child_depth = parent_tree_depth.saturating_add(1);
    if child_depth > config.max_child_depth.max(0) {
        return Ok(Err(format!(
            "child workflow depth exceeds maxChildDepth ({} > {})",
            child_depth,
            config.max_child_depth.max(0)
        )));
    }

    let live_descendants = live_descendant_count(conn, tables, parent_run_id).await?;
    if live_descendants >= config.max_live_descendants.max(0) {
        return Ok(Err(format!(
            "child workflow tree exceeds maxLiveDescendants ({} >= {})",
            live_descendants,
            config.max_live_descendants.max(0)
        )));
    }

    let child_input = checkpoint.child_input.clone().unwrap_or(Value::Null);
    let input_journal_bytes = json_column_size(conn, &child_input).await?;
    let child_key = child_dedup_key(parent_run_id, checkpoint.ordinal);
    let child_run_id = typed_id::new_workflow_run_id();
    let parent_wait_step_key = child_signal_type(checkpoint.ordinal);
    let cascade = checkpoint
        .child_options
        .as_ref()
        .is_some_and(|options| options.cascade);
    let insert_child_sql = format!(
        "INSERT INTO {runs} \
                (id, workflow_name, app_id, deploy_id, state, input, journal_bytes, dedup_key, \
                 wake_at, parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, now(), $8, $9, $10, $11, now()) \
             ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING \
             RETURNING id",
        runs = tables.runs
    );
    let rows = conn
        .query(
            &insert_child_sql,
            &[
                &child_run_id,
                &child_workflow_name,
                app_id,
                &deploy_id,
                &child_input,
                &input_journal_bytes,
                &child_key,
                &parent_run_id,
                &parent_wait_step_key,
                &cascade,
                &child_depth,
            ],
        )
        .await?;
    let actual_child_id = if let Some(row) = rows.first() {
        row.get("id")
    } else {
        let select_child_sql = format!(
            "SELECT id \
               FROM {runs} \
              WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3 \
              LIMIT 1",
            runs = tables.runs
        );
        conn.query_one(
            &select_child_sql,
            &[app_id, &child_workflow_name, &child_key],
        )
        .await?
        .get("id")
    };

    checkpoint.child_run_id = Some(actual_child_id);
    checkpoint.signal_type = Some(parent_wait_step_key);
    Ok(Ok(()))
}

async fn live_descendant_count<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<i64, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "WITH RECURSIVE ancestors AS ( \
                 SELECT id, parent_run_id \
                   FROM {runs} \
                  WHERE id = $1 \
                 UNION ALL \
                 SELECT p.id, p.parent_run_id \
                   FROM {runs} p \
                   JOIN ancestors a ON a.parent_run_id = p.id \
             ), root AS ( \
                 SELECT id FROM ancestors WHERE parent_run_id IS NULL LIMIT 1 \
             ), tree AS ( \
                 SELECT id FROM root \
                 UNION ALL \
                 SELECT c.id \
                   FROM {runs} c \
                   JOIN tree t ON c.parent_run_id = t.id \
             ) \
             SELECT COUNT(*)::bigint AS n \
               FROM {runs} r \
               JOIN tree t ON t.id = r.id \
              WHERE r.id <> (SELECT id FROM root) \
                AND r.state NOT IN ('completed','failed','cancelled','stalled')",
        runs = tables.runs
    );
    let row = conn
        .query_one(&sql, &[&run_id])
        .await?;
    Ok(row.get("n"))
}

fn child_cancelled_error() -> Value {
    serde_json::json!({
        "type": "ChildCancelledError",
        "message": "child workflow was cancelled",
        "retryable": false,
    })
}

pub async fn emit_child_terminal_hook_on_conn<C>(
    conn: &C,
    tables: &WorkflowTables,
    child_run_id: &str,
    terminal: ChildTerminalPayload<'_>,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let parent_sql = format!(
        "SELECT parent_run_id, parent_wait_step_key \
               FROM {runs} \
              WHERE id = $1 \
                AND parent_run_id IS NOT NULL \
                AND parent_wait_step_key IS NOT NULL",
        runs = tables.runs
    );
    let rows = conn
        .query(&parent_sql, &[&child_run_id])
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let parent_run_id: String = row.get("parent_run_id");
    let parent_wait_step_key: String = row.get("parent_wait_step_key");
    let ok = terminal.state == "completed";
    let error = if ok {
        None
    } else if terminal.state == "cancelled" {
        Some(child_cancelled_error())
    } else {
        terminal.error
    };
    let output_ref = terminal.output_ref.map(|value| {
        serde_json::json!({
            "hash": value.hash,
            "size": value.size,
            "contentType": value.content_type,
        })
    });
    let payload = serde_json::json!({
        "ok": ok,
        "output": if ok { terminal.output } else { None },
        "outputRef": output_ref,
        "error": error,
        "state": terminal.state,
        "childRunId": child_run_id,
    });
    let signal_id = typed_id::new_workflow_signal_id();
    let signal_sql = format!(
        "INSERT INTO {signals} \
            (id, run_id, type, payload, origin, delivery, idempotency_key, created_at) \
         VALUES ($1, $2, $3, $4, 'system', 'direct', $3, now()) \
         ON CONFLICT (run_id, type, idempotency_key) \
         WHERE idempotency_key IS NOT NULL AND delivery <> 'topic' \
         DO NOTHING",
        signals = tables.signals
    );
    conn.execute(
        &signal_sql,
        &[
            &signal_id,
            &parent_run_id,
            &parent_wait_step_key,
            &payload,
        ],
    )
    .await?;
    let wake_parent_sql = format!(
        "UPDATE {runs} \
            SET wake_at = now() \
          WHERE id = $1 \
            AND state IN ('running','sleeping','waiting')",
        runs = tables.runs
    );
    conn.execute(
        &wake_parent_sql,
        &[&parent_run_id],
    )
    .await?;
    Ok(())
}

pub async fn cascade_cancel_children_on_conn<C>(
    conn: &C,
    tables: &WorkflowTables,
    parent_run_id: &str,
) -> Result<u64, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "UPDATE {runs} \
            SET cancel_requested = true, wake_at = now() \
          WHERE parent_run_id = $1 \
            AND parent_cascade \
            AND state NOT IN ('completed','failed','cancelled','stalled')",
        runs = tables.runs
    );
    conn.execute(
        &sql,
        &[&parent_run_id],
    )
    .await
    .map_err(WorkflowError::from)
}

pub async fn insert_resolved_step_on_conn<C>(
    conn: &C,
    tables: &WorkflowTables,
    config: &WorkflowEngineConfig,
    checkpoint: &StepCheckpoint,
    run_id: &str,
    batch_id: &str,
    batch_width: i16,
) -> Result<StepWriteOutcome, WorkflowError>
where
    C: GenericClient + Sync,
{
    let existing_sql = format!(
        "SELECT state, name, kind \
               FROM {steps} \
              WHERE run_id = $1 AND ordinal = $2 \
              FOR UPDATE",
        steps = tables.steps
    );
    let existing = conn
        .query(&existing_sql, &[&run_id, &checkpoint.ordinal])
        .await?;
    let resolves_running = if let Some(row) = existing.first() {
        let state: String = row.get("state");
        let name: String = row.get("name");
        let kind: String = row.get("kind");
        let resolves_running = state == "running"
            && matches!(checkpoint.state.as_str(), "completed" | "failed")
            && name == checkpoint.name
            && kind == checkpoint.kind;
        if !resolves_running {
            return Ok(StepWriteOutcome::Noop);
        }
        true
    } else {
        false
    };

    let output_ref = checkpoint.output_ref.as_ref();
    let output_kind = if output_ref.is_some() { "blob" } else { "inline" };
    let output_value = if output_ref.is_some() {
        None
    } else {
        checkpoint.output.clone()
    };
    let output_hash = output_ref.map(|value| value.hash.clone());
    let output_size = output_ref.map(|value| value.size);
    let output_content_type = output_ref
        .and_then(|value| value.content_type.clone())
        .or_else(|| output_ref.map(|_| "application/json".to_string()));
    let blob_bytes_delta = output_ref.map_or(0, |value| value.size.max(0));
    let compensation_state = if checkpoint.kind == "run" && checkpoint.state == "completed" {
        checkpoint.compensation_state.as_deref()
    } else {
        None
    };
    let compensation_max_attempts = checkpoint.compensation_max_attempts.max(1);

    let delta = checkpoint_journal_bytes(conn, checkpoint).await?;
    if delta > 0 {
        let run_accounting_sql = format!(
            "SELECT app_id, journal_bytes \
                   FROM {runs} \
                  WHERE id = $1 \
                  FOR UPDATE",
            runs = tables.runs
        );
        let rows = conn
            .query(&run_accounting_sql, &[&run_id])
            .await?;
        let Some(row) = rows.first() else {
            return Err(WorkflowError::Db(format!(
                "workflow run {run_id} not found for journal accounting"
            )));
        };
        let current: i64 = row.get("journal_bytes");
        let limits = config.journal_limits;
        if cap_exceeded(current, delta, limits.run_max_bytes) {
            mark_run_state_cap_exceeded(
                conn,
                tables,
                run_id,
                current,
                delta,
                limits.run_max_bytes,
            )
            .await?;
            return Ok(StepWriteOutcome::CapExceeded);
        }
    }

    let changed = if resolves_running {
        let update_step_sql = format!(
            "UPDATE {steps} \
                SET state = $4, \
                    output = $5, \
                    error = $6, \
                    wake_at = $7, \
                    signal_type = $8, \
                    max_signal_age_ms = $9, \
                    consumed_signal_id = $10, \
                    output_kind = $12, \
                    output_hash = $13, \
                    output_size = $14, \
                    output_content_type = $15, \
                    child_run_id = COALESCE(child_run_id, $16), \
                    compensation_state = $17, \
                    compensation_max_attempts = $18, \
                    finished_at = now() \
              WHERE run_id = $1 \
                AND ordinal = $2 \
                AND name = $3 \
                AND kind = $11 \
                AND state = 'running'",
            steps = tables.steps
        );
        conn.execute(
            &update_step_sql,
            &[
                &run_id,
                &checkpoint.ordinal,
                &checkpoint.name,
                &checkpoint.state,
                &output_value,
                &checkpoint.error,
                &checkpoint.wake_at,
                &checkpoint.signal_type,
                &checkpoint.max_signal_age_ms,
                &checkpoint.consumed_signal_id,
                &checkpoint.kind,
                &output_kind,
                &output_hash,
                &output_size,
                &output_content_type,
                &checkpoint.child_run_id,
                &compensation_state,
                &compensation_max_attempts,
            ],
        )
        .await?
    } else {
        let insert_step_sql = format!(
            "INSERT INTO {steps} \
            (run_id, ordinal, name, name_occurrence, kind, state, output, error, \
             output_kind, output_hash, output_size, output_content_type, \
             wake_at, signal_type, max_signal_age_ms, consumed_signal_id, \
             child_run_id, batch_id, batch_width, compensation_state, compensation_max_attempts, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, now()) \
         ON CONFLICT (run_id, ordinal) DO NOTHING",
            steps = tables.steps
        );
        conn.execute(
            &insert_step_sql,
            &[
                &run_id,
                &checkpoint.ordinal,
                &checkpoint.name,
                &checkpoint.name_occurrence,
                &checkpoint.kind,
                &checkpoint.state,
                &output_value,
                &checkpoint.error,
                &output_kind,
                &output_hash,
                &output_size,
                &output_content_type,
                &checkpoint.wake_at,
                &checkpoint.signal_type,
                &checkpoint.max_signal_age_ms,
                &checkpoint.consumed_signal_id,
                &checkpoint.child_run_id,
                &batch_id,
                &batch_width,
                &compensation_state,
                &compensation_max_attempts,
            ],
        )
        .await?
    };
    if changed > 0 {
        if let Some(output_ref) = output_ref {
            upsert_workflow_blob_ref(conn, tables, output_ref).await?;
        }
    }
    if changed > 0 && (delta > 0 || blob_bytes_delta > 0) {
        let update_run_bytes_sql = format!(
            "UPDATE {runs} \
                SET journal_bytes = journal_bytes + $2, \
                    blob_bytes = blob_bytes + $3 \
              WHERE id = $1",
            runs = tables.runs
        );
        conn.execute(
            &update_run_bytes_sql,
            &[&run_id, &delta, &blob_bytes_delta],
        )
        .await?;
    }

    Ok(if changed > 0 {
        StepWriteOutcome::Wrote
    } else {
        StepWriteOutcome::Noop
    })
}

async fn checkpoint_journal_bytes<C>(
    conn: &C,
    checkpoint: &StepCheckpoint,
) -> Result<i64, WorkflowError>
where
    C: GenericClient + Sync,
{
    if checkpoint.output_ref.is_some() {
        let rows = conn
            .query(
                "SELECT (COALESCE(pg_column_size($1::jsonb), 0))::bigint AS bytes",
                &[&checkpoint.error],
            )
            .await?;
        let error_bytes: i64 = rows[0].get("bytes");
        return Ok(error_bytes + BLOB_REF_JOURNAL_BYTES);
    }
    let rows = conn
        .query(
            "SELECT (COALESCE(pg_column_size($1::jsonb), 0) \
                    + COALESCE(pg_column_size($2::jsonb), 0))::bigint AS bytes",
            &[&checkpoint.output, &checkpoint.error],
        )
        .await?;
    Ok(rows[0].get("bytes"))
}

async fn run_output_journal_bytes<C>(
    conn: &C,
    output: &Option<Value>,
    output_ref: Option<&WorkflowOutputRef>,
) -> Result<i64, WorkflowError>
where
    C: GenericClient + Sync,
{
    if output_ref.is_some() {
        return Ok(BLOB_REF_JOURNAL_BYTES);
    }
    let rows = conn
        .query(
            "SELECT (COALESCE(pg_column_size($1::jsonb), 0))::bigint AS bytes",
            &[output],
        )
        .await?;
    Ok(rows[0].get("bytes"))
}

async fn upsert_workflow_blob_ref<C>(
    conn: &C,
    tables: &WorkflowTables,
    output_ref: &WorkflowOutputRef,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let content_type = output_ref
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    let sql = format!(
        "INSERT INTO {blobs} \
            (hash, size, content_type, refcount, last_referenced_at) \
         VALUES ($1, $2, $3, 1, now()) \
         ON CONFLICT (hash) DO UPDATE SET \
            size = EXCLUDED.size, \
            content_type = EXCLUDED.content_type, \
            refcount = {blobs}.refcount + 1, \
            last_referenced_at = now()",
        blobs = tables.blobs
    );
    conn.execute(
        &sql,
        &[&output_ref.hash, &output_ref.size, &content_type],
    )
    .await?;
    Ok(())
}

async fn mark_run_state_cap_exceeded<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    current: i64,
    delta: i64,
    cap: i64,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let error = state_cap_error(current, delta, cap);
    let sql = format!(
        "UPDATE {runs} \
            SET state = 'failed', \
                output = NULL, \
                error = $2, \
                output_kind = 'inline', \
                output_hash = NULL, \
                output_size = NULL, \
                output_content_type = NULL, \
                wake_at = NULL, \
                terminal_at = now(), \
                waiting_step_key = NULL, \
                paused_from_status = NULL, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $1",
        runs = tables.runs
    );
    conn.execute(
        &sql,
        &[&run_id, &error],
    )
    .await?;
    Ok(())
}

pub async fn json_column_size<C>(conn: &C, value: &Value) -> Result<i64, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT pg_column_size($1::jsonb)::bigint AS bytes",
            &[value],
        )
        .await?;
    Ok(rows[0].get("bytes"))
}

pub async fn compensation_progress_on_conn<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<CompensationProgress, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "SELECT \
                COUNT(*) FILTER (WHERE compensation_state IS NOT NULL)::bigint AS total, \
                COUNT(*) FILTER (WHERE compensation_state = 'completed')::bigint AS completed, \
                COUNT(*) FILTER (WHERE compensation_state = 'failed')::bigint AS failed, \
                COUNT(*) FILTER (WHERE compensation_state = 'pending')::bigint AS pending, \
                COUNT(*) FILTER (WHERE compensation_state = 'running')::bigint AS running \
               FROM {steps} \
              WHERE run_id = $1",
        steps = tables.steps
    );
    let row = conn
        .query_one(&sql, &[&run_id])
        .await?;
    Ok(CompensationProgress {
        total: row.get("total"),
        completed: row.get("completed"),
        failed: row.get("failed"),
        pending: row.get("pending"),
        running: row.get("running"),
    })
}
