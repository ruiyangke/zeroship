use async_trait::async_trait;
use chrono::{DateTime, Utc};
use compio_postgres::{Client, GenericClient, NoTls};
use serde_json::Value;
use sha2::{Digest, Sha256};
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
const FREE_WORKFLOW_JOURNAL_MAX_BYTES: i64 = 100 * 1024 * 1024;
const PAID_WORKFLOW_JOURNAL_MAX_BYTES: i64 = 1024 * 1024 * 1024;
const RUN_JOURNAL_LIMIT_FIELD: &str = "workflow_journal_max_bytes";
const APP_JOURNAL_LIMIT_FIELD: &str = "workflow_app_journal_max_bytes";

#[derive(Clone, Debug)]
pub struct PgStore {
    db_url: String,
}

impl PgStore {
    #[must_use]
    pub fn new(db_url: impl Into<String>) -> Self {
        Self {
            db_url: db_url.into(),
        }
    }
}

#[derive(Debug)]
pub struct PgTx {
    conn: Client,
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

#[async_trait(?Send)]
impl WorkflowStore for PgStore {
    type Tx = PgTx;

    async fn begin(&self) -> Result<Self::Tx, WorkflowError> {
        let conn = open_conn(&self.db_url).await?;
        conn.batch_execute("BEGIN").await?;
        Ok(PgTx { conn })
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
        let rows = self
            .conn
            .query(
                "SELECT app_id, deploy_id, claimed_by, state, dispatch_nonce, stuck_strikes, \
                    tree_depth, compensation_target, error \
               FROM zeroship.workflow_runs \
              WHERE id = $1 \
              FOR UPDATE",
                &[&run_id],
            )
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
        self.conn
            .execute(
                "UPDATE zeroship.workflow_steps s \
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
                      FROM zeroship.workflow_steps higher \
                     WHERE higher.run_id = s.run_id \
                       AND higher.ordinal > s.ordinal \
                       AND higher.compensation_state IN ('pending','running') \
                )",
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
        let row = self
            .conn
            .query_one(
                "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND compensation_state = 'pending'",
                &[&run_id],
            )
            .await?;
        Ok(row.get("n"))
    }

    async fn compensation_progress(
        &mut self,
        run_id: &str,
    ) -> Result<CompensationProgress, WorkflowError> {
        compensation_progress(&self.conn, run_id).await
    }

    async fn next_compensation_wake_at(
        &mut self,
        run_id: &str,
    ) -> Result<Option<DateTime<Utc>>, WorkflowError> {
        let row = self
            .conn
            .query_one(
                "SELECT \
                EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_steps \
                     WHERE run_id = $1 AND compensation_state = 'pending' \
                ) AS has_pending, \
                MIN(compensation_wake_at) FILTER (WHERE compensation_state = 'running') AS running_wake \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1",
                &[&run_id],
            )
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
        self.conn
            .execute(
                "UPDATE zeroship.workflow_runs \
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
        checkpoint: &StepCheckpoint,
        run_id: &str,
        batch_id: &str,
        batch_width: i16,
    ) -> Result<StepWriteOutcome, WorkflowError> {
        insert_resolved_step(&self.conn, checkpoint, run_id, batch_id, batch_width).await
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
        self.conn
            .execute(
                "INSERT INTO zeroship.workflow_subscriptions \
            (id, app_id, topic, run_id, signal_name, type_filter, ordinal, max_age_ms, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (run_id, ordinal) DO UPDATE SET \
            app_id = EXCLUDED.app_id, \
            topic = EXCLUDED.topic, \
            signal_name = EXCLUDED.signal_name, \
            type_filter = EXCLUDED.type_filter, \
            max_age_ms = EXCLUDED.max_age_ms, \
            expires_at = EXCLUDED.expires_at",
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
        self.conn
            .execute(
                "DELETE FROM zeroship.workflow_subscriptions \
          WHERE run_id = $1 AND ordinal = $2",
                &[&run_id, &ordinal],
            )
            .await?;
        Ok(())
    }

    async fn mark_signal_consumed(
        &mut self,
        run_id: &str,
        signal_id: &str,
    ) -> Result<(), WorkflowError> {
        self.conn
            .execute(
                "UPDATE zeroship.workflow_signals \
                            SET consumed_by = $1 \
                          WHERE id = $2 AND consumed_by IS NULL",
                &[&run_id, &signal_id],
            )
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
        self.conn
            .execute(
                "UPDATE zeroship.workflow_runs \
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
        self.conn
            .execute(
                "UPDATE zeroship.workflow_runs \
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
        upsert_workflow_blob_ref(&self.conn, output_ref).await
    }

    async fn emit_child_terminal_signal(
        &mut self,
        child_run_id: &str,
        terminal: ChildTerminalPayload<'_>,
    ) -> Result<(), WorkflowError> {
        emit_child_terminal_hook(&self.conn, child_run_id, terminal).await
    }

    async fn cascade_cancel_children(
        &mut self,
        parent_run_id: &str,
    ) -> Result<u64, WorkflowError> {
        cascade_cancel_children(&self.conn, parent_run_id).await
    }
}

async fn prepare_child_spawn<C>(
    conn: &C,
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
    let existing_step = conn
        .query(
            "SELECT child_run_id, signal_type \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
                AND ordinal = $2 \
                AND kind = 'child' \
                AND state = 'running' \
              FOR UPDATE",
            &[&parent_run_id, &checkpoint.ordinal],
        )
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

    let live_descendants = live_descendant_count(conn, parent_run_id).await?;
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
    let rows = conn
        .query(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, journal_bytes, dedup_key, \
                 wake_at, parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, now(), $8, $9, $10, $11, now()) \
             ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING \
             RETURNING id",
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
        conn.query_one(
            "SELECT id \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3 \
              LIMIT 1",
            &[app_id, &child_workflow_name, &child_key],
        )
        .await?
        .get("id")
    };

    checkpoint.child_run_id = Some(actual_child_id);
    checkpoint.signal_type = Some(parent_wait_step_key);
    Ok(Ok(()))
}

async fn live_descendant_count<C>(conn: &C, run_id: &str) -> Result<i64, WorkflowError>
where
    C: GenericClient + Sync,
{
    let row = conn
        .query_one(
            "WITH RECURSIVE ancestors AS ( \
                 SELECT id, parent_run_id \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1 \
                 UNION ALL \
                 SELECT p.id, p.parent_run_id \
                   FROM zeroship.workflow_runs p \
                   JOIN ancestors a ON a.parent_run_id = p.id \
             ), root AS ( \
                 SELECT id FROM ancestors WHERE parent_run_id IS NULL LIMIT 1 \
             ), tree AS ( \
                 SELECT id FROM root \
                 UNION ALL \
                 SELECT c.id \
                   FROM zeroship.workflow_runs c \
                   JOIN tree t ON c.parent_run_id = t.id \
             ) \
             SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs r \
               JOIN tree t ON t.id = r.id \
              WHERE r.id <> (SELECT id FROM root) \
                AND r.state NOT IN ('completed','failed','cancelled','stalled')",
            &[&run_id],
        )
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

async fn emit_child_terminal_hook<C>(
    conn: &C,
    child_run_id: &str,
    terminal: ChildTerminalPayload<'_>,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT parent_run_id, parent_wait_step_key \
               FROM zeroship.workflow_runs \
              WHERE id = $1 \
                AND parent_run_id IS NOT NULL \
                AND parent_wait_step_key IS NOT NULL",
            &[&child_run_id],
        )
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
    conn.execute(
        "INSERT INTO zeroship.workflow_signals \
            (id, run_id, type, payload, origin, delivery, idempotency_key, created_at) \
         VALUES ($1, $2, $3, $4, 'system', 'direct', $3, now()) \
         ON CONFLICT (run_id, type, idempotency_key) \
         WHERE idempotency_key IS NOT NULL AND delivery <> 'topic' \
         DO NOTHING",
        &[
            &signal_id,
            &parent_run_id,
            &parent_wait_step_key,
            &payload,
        ],
    )
    .await?;
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET wake_at = now() \
          WHERE id = $1 \
            AND state IN ('running','sleeping','waiting')",
        &[&parent_run_id],
    )
    .await?;
    Ok(())
}

async fn cascade_cancel_children<C>(conn: &C, parent_run_id: &str) -> Result<u64, WorkflowError>
where
    C: GenericClient + Sync,
{
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET cancel_requested = true, wake_at = now() \
          WHERE parent_run_id = $1 \
            AND parent_cascade \
            AND state NOT IN ('completed','failed','cancelled','stalled')",
        &[&parent_run_id],
    )
    .await
    .map_err(WorkflowError::from)
}

async fn insert_resolved_step<C>(
    conn: &C,
    checkpoint: &StepCheckpoint,
    run_id: &str,
    batch_id: &str,
    batch_width: i16,
) -> Result<StepWriteOutcome, WorkflowError>
where
    C: GenericClient + Sync,
{
    let existing = conn
        .query(
            "SELECT state, name, kind \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = $2 \
              FOR UPDATE",
            &[&run_id, &checkpoint.ordinal],
        )
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
        let rows = conn
            .query(
                "SELECT app_id, journal_bytes \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1 \
                  FOR UPDATE",
                &[&run_id],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(WorkflowError::Db(format!(
                "workflow run {run_id} not found for journal accounting"
            )));
        };
        let app_id: Uuid = row.get("app_id");
        let current: i64 = row.get("journal_bytes");
        let limits = limits_for_app(conn, &app_id).await?;
        if cap_exceeded(current, delta, limits.run_max_bytes) {
            mark_run_state_cap_exceeded(conn, run_id, current, delta, limits.run_max_bytes).await?;
            return Ok(StepWriteOutcome::CapExceeded);
        }
    }

    let changed = if resolves_running {
        conn.execute(
            "UPDATE zeroship.workflow_steps \
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
        conn.execute(
            "INSERT INTO zeroship.workflow_steps \
            (run_id, ordinal, name, name_occurrence, kind, state, output, error, \
             output_kind, output_hash, output_size, output_content_type, \
             wake_at, signal_type, max_signal_age_ms, consumed_signal_id, \
             child_run_id, batch_id, batch_width, compensation_state, compensation_max_attempts, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, now()) \
         ON CONFLICT (run_id, ordinal) DO NOTHING",
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
            upsert_workflow_blob_ref(conn, output_ref).await?;
        }
    }
    if changed > 0 && (delta > 0 || blob_bytes_delta > 0) {
        conn.execute(
            "UPDATE zeroship.workflow_runs \
                SET journal_bytes = journal_bytes + $2, \
                    blob_bytes = blob_bytes + $3 \
              WHERE id = $1",
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
    output_ref: &WorkflowOutputRef,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let content_type = output_ref
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    conn.execute(
        "INSERT INTO zeroship.workflow_blobs \
            (hash, size, content_type, refcount, last_referenced_at) \
         VALUES ($1, $2, $3, 1, now()) \
         ON CONFLICT (hash) DO UPDATE SET \
            size = EXCLUDED.size, \
            content_type = EXCLUDED.content_type, \
            refcount = zeroship.workflow_blobs.refcount + 1, \
            last_referenced_at = now()",
        &[&output_ref.hash, &output_ref.size, &content_type],
    )
    .await?;
    Ok(())
}

async fn mark_run_state_cap_exceeded<C>(
    conn: &C,
    run_id: &str,
    current: i64,
    delta: i64,
    cap: i64,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let error = state_cap_error(current, delta, cap);
    conn.execute(
        "UPDATE zeroship.workflow_runs \
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
        &[&run_id, &error],
    )
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct WorkflowJournalLimits {
    pub run_max_bytes: i64,
    pub app_max_bytes: i64,
}

pub async fn limits_for_app<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<WorkflowJournalLimits, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT a.plan_id, p.name, p.runtime_limits_json \
               FROM zeroship.apps a \
               LEFT JOIN zeroship.plans p ON p.id = a.plan_id \
              WHERE a.id = $1",
            &[app_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(WorkflowError::Db(format!(
            "app {app_id} not found for workflow journal limits"
        )));
    };

    let plan_id: String = row.get("plan_id");
    let plan_name: Option<String> = row.get("name");
    let runtime_limits: Option<Value> = row.get("runtime_limits_json");
    let default = default_journal_cap(&plan_id, plan_name.as_deref());
    let run_max_bytes = runtime_limits
        .as_ref()
        .and_then(|json| positive_i64_field(json, RUN_JOURNAL_LIMIT_FIELD))
        .unwrap_or(default);
    let app_max_bytes = runtime_limits
        .as_ref()
        .and_then(|json| positive_i64_field(json, APP_JOURNAL_LIMIT_FIELD))
        .unwrap_or(default);

    Ok(WorkflowJournalLimits {
        run_max_bytes,
        app_max_bytes,
    })
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

async fn compensation_progress<C>(
    conn: &C,
    run_id: &str,
) -> Result<CompensationProgress, WorkflowError>
where
    C: GenericClient + Sync,
{
    let row = conn
        .query_one(
            "SELECT \
                COUNT(*) FILTER (WHERE compensation_state IS NOT NULL)::bigint AS total, \
                COUNT(*) FILTER (WHERE compensation_state = 'completed')::bigint AS completed, \
                COUNT(*) FILTER (WHERE compensation_state = 'failed')::bigint AS failed, \
                COUNT(*) FILTER (WHERE compensation_state = 'pending')::bigint AS pending, \
                COUNT(*) FILTER (WHERE compensation_state = 'running')::bigint AS running \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1",
            &[&run_id],
        )
        .await?;
    Ok(CompensationProgress {
        total: row.get("total"),
        completed: row.get("completed"),
        failed: row.get("failed"),
        pending: row.get("pending"),
        running: row.get("running"),
    })
}

fn default_journal_cap(plan_id: &str, plan_name: Option<&str>) -> i64 {
    if plan_name == Some("free") || plan_id == free_plan_id() {
        FREE_WORKFLOW_JOURNAL_MAX_BYTES
    } else {
        PAID_WORKFLOW_JOURNAL_MAX_BYTES
    }
}

fn positive_i64_field(json: &Value, field: &str) -> Option<i64> {
    let value = json.get(field)?;
    match value {
        Value::Number(n) => n.as_i64().filter(|v| *v > 0),
        Value::String(s) => s.parse::<i64>().ok().filter(|v| *v > 0),
        _ => None,
    }
}

fn free_plan_id() -> String {
    let uuid = derive_uuid("zeroship:plan:free:v1", "builtin");
    typed_id::from_uuid_string(typed_id::PLAN_PREFIX, &uuid.to_string())
        .expect("derived uuid is a valid uuid string")
}

fn derive_uuid(label: &str, host: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    hasher.update([0u8]);
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0F) | 0x80;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    Uuid::from_bytes(bytes)
}
