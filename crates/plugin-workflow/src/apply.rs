use chrono::Utc;
use serde_json::Value;

use crate::engine::{
    compensation_outcomes_from_step_outcomes, fold_outcomes, stalled_error,
    CompensationApplyOutcome, RunUpdate, StepCheckpoint, StepResult, WorkflowEngineConfig,
};
use crate::errors::WorkflowError;
use crate::store::{
    ChildTerminalPayload, CompensatingRunUpdate, CompensationProgress, PausedRunUpdate,
    StepWriteOutcome, TransitionRunUpdate, WorkflowStore, WorkflowTx,
};

#[allow(clippy::future_not_send)]
pub async fn apply_step_result_on_store<S>(
    store: &S,
    config: &WorkflowEngineConfig,
    mut result: StepResult,
) -> Result<bool, WorkflowError>
where
    S: WorkflowStore,
{
    let compensation_outcomes = compensation_outcomes_from_step_outcomes(&result.outcomes)
        .map_err(WorkflowError::Invalid)?;
    let (checkpoints, run_update) = fold_outcomes(&result.outcomes).map_err(WorkflowError::Invalid)?;
    result.checkpoints = checkpoints;
    result.run_update = run_update;
    result.checkpoints.sort_by_key(|s| s.ordinal);
    let batch_width = i16::try_from(result.checkpoints.len()).unwrap_or(i16::MAX);
    let mut tx = store.begin().await?;

    let Some(row) = tx.lock_run_for_apply(&result.run_id).await? else {
        tx.commit().await?;
        return Ok(false);
    };
    if row.claimed_by.as_deref() != Some(config.owner_id.as_str())
        || row.dispatch_nonce.as_deref() != Some(result.dispatch_nonce.as_str())
        || !matches!(row.state.as_str(), "running" | "paused" | "compensating")
    {
        tx.commit().await?;
        return Ok(false);
    }
    if row.state == "compensating" {
        let applied = apply_compensation_result(
            &mut tx,
            config,
            &result.run_id,
            &result.dispatch_nonce,
            row.compensation_target.as_deref(),
            row.current_error,
            &compensation_outcomes,
        )
        .await?;
        tx.commit().await?;
        return Ok(applied);
    }
    if !compensation_outcomes.is_empty() {
        tx.commit().await?;
        return Err(WorkflowError::Invalid(
            "compensation outcome received outside compensating phase".to_string(),
        ));
    }
    if matches!(result.run_update, RunUpdate::ContinuedAsNew { .. })
        && tx.pending_compensation_count(&result.run_id).await? > 0
    {
        tx.commit().await?;
        return Err(WorkflowError::CompensableCarry(
            "cannot continue as new while compensable steps are pending".to_string(),
        ));
    }

    let child_checkpoint_count = result
        .checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.kind == "child" && checkpoint.state == "running")
        .count();
    let start_many_over_cap = child_checkpoint_count > config.max_start_many_batch;

    let mut wrote_checkpoints = 0usize;
    for checkpoint in &mut result.checkpoints {
        if checkpoint.kind == "child" && checkpoint.state == "running" {
            if start_many_over_cap {
                fail_child_checkpoint_with_limit(
                    checkpoint,
                    format!(
                        "child workflow batch exceeds maxStartManyBatch ({} > {})",
                        child_checkpoint_count, config.max_start_many_batch
                    ),
                );
                result.run_update = RunUpdate::Queued;
            } else if let Err(error) = tx
                .prepare_child_spawn(
                    config,
                    &result.run_id,
                    &row.app_id,
                    &row.deploy_id,
                    row.tree_depth,
                    checkpoint,
                )
                .await?
            {
                fail_child_checkpoint_with_limit(checkpoint, error);
                result.run_update = RunUpdate::Queued;
            }
        }
        match tx
            .insert_resolved_step(
                config,
                checkpoint,
                &result.run_id,
                &result.dispatch_nonce,
                batch_width,
            )
            .await?
        {
            StepWriteOutcome::CapExceeded => {
                tx.commit().await?;
                return Ok(true);
            }
            StepWriteOutcome::Wrote => {
                wrote_checkpoints += 1;
                reconcile_step_side_effects(&mut tx, &row.app_id, &result.run_id, checkpoint)
                    .await?;
            }
            StepWriteOutcome::Noop => {
                reconcile_step_side_effects(&mut tx, &row.app_id, &result.run_id, checkpoint)
                    .await?;
            }
        }
    }

    let next_ordinal = result
        .checkpoints
        .iter()
        .map(|s| s.ordinal.saturating_add(1))
        .max()
        .unwrap_or(0);
    let made_progress = wrote_checkpoints > 0 || !matches!(result.run_update, RunUpdate::Queued);
    let zero_progress = !made_progress && row.state == "running";
    let next_stuck_strikes = if zero_progress {
        row.stuck_strikes.saturating_add(1)
    } else {
        0
    };
    if zero_progress && next_stuck_strikes >= config.stuck_strike_limit.max(1) {
        result.run_update = RunUpdate::Stalled {
            error: stalled_error(next_stuck_strikes, config.stuck_strike_limit.max(1)),
        };
    }

    let wake_at = result.run_update.wake_at();
    let waiting_step_key = result.run_update.waiting_step_key(&result.checkpoints);
    if row.state == "paused" {
        let paused_from_status = match &result.run_update {
            RunUpdate::Queued
            | RunUpdate::Completed { .. }
            | RunUpdate::ContinuedAsNew { .. }
            | RunUpdate::Failed { .. }
            | RunUpdate::Stalled { .. }
            | RunUpdate::Cancelled => "queued",
            RunUpdate::Sleeping { .. } => "sleeping",
            RunUpdate::Waiting { .. } => "waiting",
        };
        let paused_wake_at = wake_at.or_else(|| (paused_from_status == "queued").then(Utc::now));
        tx.write_paused_run(
            config,
            &result.run_id,
            &result.dispatch_nonce,
            &PausedRunUpdate {
                wake_at: paused_wake_at,
                next_ordinal,
                waiting_step_key,
                paused_from_status,
            },
        )
        .await?;
    } else {
        let mut state = result.run_update.state().to_string();
        let output_ref = result.run_update.output_ref();
        let output = if output_ref.is_some() {
            None
        } else {
            result.run_update.output()
        };
        let output_kind = if output_ref.is_some() { "blob" } else { "inline" };
        let output_hash = output_ref.as_ref().map(|value| value.hash.clone());
        let output_size = output_ref.as_ref().map(|value| value.size);
        let output_content_type = output_ref
            .as_ref()
            .and_then(|value| value.content_type.clone())
            .or_else(|| output_ref.as_ref().map(|_| "application/json".to_string()));
        let run_journal_delta = tx
            .run_output_journal_bytes(&output, output_ref.as_ref())
            .await?;
        let blob_bytes_delta = output_ref.as_ref().map_or(0, |value| value.size.max(0));
        let mut error = result.run_update.error();
        let mut compensation_target: Option<String> = None;
        let mut compensation_outcome: Option<String> = None;
        let mut wake_at = wake_at;
        if state == "failed"
            && error
                .as_ref()
                .is_some_and(should_enter_compensation_for_error)
            && tx.pending_compensation_count(&result.run_id).await? > 0
        {
            state = "compensating".to_string();
            compensation_target = Some("failed".to_string());
            compensation_outcome = None;
            wake_at = Some(Utc::now());
            let progress = tx.compensation_progress(&result.run_id).await?;
            error = Some(compensation_progress_error(error, progress, None));
        }
        if let RunUpdate::ContinuedAsNew {
            seed_input,
            seed_input_ref,
        } = &result.run_update
        {
            tx.continue_as_new(
                config,
                &result.run_id,
                &row.app_id,
                &row.workflow_name,
                seed_input.as_ref(),
                seed_input_ref.as_ref(),
            )
            .await?;
        }
        let changed = tx
            .transition_run(
                config,
                &result.run_id,
                &result.dispatch_nonce,
                &TransitionRunUpdate {
                    state: state.clone(),
                    output: output.clone(),
                    error: error.clone(),
                    wake_at,
                    next_ordinal,
                    waiting_step_key,
                    stuck_strikes: next_stuck_strikes,
                    output_kind,
                    output_hash,
                    output_size,
                    output_content_type,
                    run_journal_delta,
                    blob_bytes_delta,
                    compensation_target,
                    compensation_outcome,
                },
            )
            .await?;
        if changed > 0 {
            if let Some(output_ref) = output_ref.as_ref() {
                tx.upsert_blob_ref(output_ref).await?;
            }
            if matches!(state.as_str(), "completed" | "failed" | "cancelled" | "stalled") {
                tx.emit_child_terminal_signal(
                    &result.run_id,
                    ChildTerminalPayload {
                        state: &state,
                        output: output.clone(),
                        output_ref,
                        error: error.clone(),
                    },
                )
                .await?;
            }
            if matches!(state.as_str(), "failed" | "cancelled" | "stalled") {
                tx.cascade_cancel_children(&result.run_id).await?;
            }
        }
    }

    tx.commit().await?;
    Ok(true)
}

async fn reconcile_step_side_effects<T>(
    tx: &mut T,
    app_id: &uuid::Uuid,
    run_id: &str,
    checkpoint: &StepCheckpoint,
) -> Result<(), WorkflowError>
where
    T: WorkflowTx,
{
    if checkpoint.kind == "wait_signal" && checkpoint.state == "running" {
        tx.upsert_subscription(app_id, run_id, checkpoint).await?;
    }
    if checkpoint.kind == "wait_signal"
        && matches!(checkpoint.state.as_str(), "completed" | "failed")
    {
        tx.delete_subscription(run_id, checkpoint.ordinal).await?;
    }
    if let Some(signal_id) = checkpoint.consumed_signal_id.as_ref() {
        tx.mark_signal_consumed(run_id, signal_id).await?;
    }
    Ok(())
}

pub async fn apply_compensation_result<T>(
    tx: &mut T,
    config: &WorkflowEngineConfig,
    run_id: &str,
    dispatch_nonce: &str,
    compensation_target: Option<&str>,
    current_error: Option<Value>,
    outcomes: &[CompensationApplyOutcome],
) -> Result<bool, WorkflowError>
where
    T: WorkflowTx,
{
    for outcome in outcomes {
        tx.apply_compensation_outcome(
            run_id,
            dispatch_nonce,
            outcome.ordinal,
            &outcome.name,
            outcome.name_occurrence,
            outcome.state,
            outcome.error.as_ref(),
        )
        .await?;
    }

    update_compensating_run_progress(
        tx,
        config,
        run_id,
        dispatch_nonce,
        compensation_target,
        current_error,
    )
    .await
}

pub async fn update_compensating_run_progress<T>(
    tx: &mut T,
    config: &WorkflowEngineConfig,
    run_id: &str,
    dispatch_nonce: &str,
    compensation_target: Option<&str>,
    current_error: Option<Value>,
) -> Result<bool, WorkflowError>
where
    T: WorkflowTx,
{
    let progress = tx.compensation_progress(run_id).await?;
    let (next_state, wake_at, outcome) = if progress.remaining() > 0 {
        let wake_at = tx.next_compensation_wake_at(run_id).await?;
        ("compensating".to_string(), wake_at, None)
    } else {
        let target = compensation_target.unwrap_or("failed");
        (
            target.to_string(),
            None,
            Some(progress.terminal_outcome().to_string()),
        )
    };
    let error = compensation_progress_error(current_error, progress, outcome.as_deref());
    let changed = tx
        .update_compensating_run(
            config,
            run_id,
            dispatch_nonce,
            &CompensatingRunUpdate {
                state: next_state.clone(),
                error: error.clone(),
                wake_at,
                compensation_outcome: outcome,
            },
        )
        .await?;
    if changed > 0 && matches!(next_state.as_str(), "failed" | "cancelled") {
        tx.emit_child_terminal_signal(
            run_id,
            ChildTerminalPayload {
                state: &next_state,
                output: None,
                output_ref: None,
                error: Some(error.clone()),
            },
        )
        .await?;
        tx.cascade_cancel_children(run_id).await?;
    }
    Ok(changed > 0)
}

fn compensation_progress_error(
    base: Option<Value>,
    progress: CompensationProgress,
    outcome: Option<&str>,
) -> Value {
    let mut error = match base {
        Some(Value::Object(map)) => Value::Object(map),
        Some(value) => serde_json::json!({
            "type": "Error",
            "message": "workflow failed during compensation",
            "cause": value,
        }),
        None => serde_json::json!({
            "type": "Error",
            "message": "workflow compensation is running",
        }),
    };
    let mut compensation = serde_json::json!({
        "total": progress.total,
        "completed": progress.completed,
        "failed": progress.failed,
    });
    if let Some(outcome) = outcome {
        compensation["outcome"] = Value::String(outcome.to_string());
    }
    if let Some(obj) = error.as_object_mut() {
        obj.insert("compensation".to_string(), compensation);
    }
    error
}

fn should_enter_compensation_for_error(error: &Value) -> bool {
    !matches!(
        error.get("type").and_then(Value::as_str),
        Some("NondeterministicError" | "StalledError")
    )
}

fn child_limit_error(message: impl Into<String>) -> Value {
    serde_json::json!({
        "type": "LimitExceededError",
        "message": message.into(),
        "retryable": false,
    })
}

fn fail_child_checkpoint_with_limit(checkpoint: &mut StepCheckpoint, message: String) {
    checkpoint.state = "failed".to_string();
    checkpoint.output = None;
    checkpoint.output_ref = None;
    checkpoint.error = Some(child_limit_error(message));
    checkpoint.wake_at = None;
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use uuid::Uuid;

    use super::apply_step_result_on_store;
    use crate::engine::{
        fold_outcomes, RunUpdate, StepCheckpoint, StepOutcome, StepResult, WorkflowEngineConfig,
        WorkflowOutputRef,
    };
    use crate::errors::WorkflowError;
    use crate::store::{
        ChildTerminalPayload, CompensatingRunUpdate, CompensationProgress, PausedRunUpdate,
        RunLockRow, StepWriteOutcome, TransitionRunUpdate, WorkflowStore, WorkflowTx,
    };

    #[derive(Debug, Default)]
    struct MemState {
        app_id: Uuid,
        deploy_id: String,
        owner_id: String,
        dispatch_nonce: String,
        state: String,
        steps_written: usize,
        transitioned_state: Option<String>,
        transitioned_output: Option<Value>,
        pending_compensation_count: i64,
        continued_as_new_run_id: Option<String>,
        continued_seed_input: Option<Value>,
        continued_seed_input_ref: Option<WorkflowOutputRef>,
        committed: bool,
    }

    #[derive(Clone, Debug)]
    struct MemStore {
        state: Rc<RefCell<MemState>>,
    }

    #[derive(Debug)]
    struct MemTx {
        state: Rc<RefCell<MemState>>,
    }

    #[async_trait(?Send)]
    impl WorkflowStore for MemStore {
        type Tx = MemTx;

        async fn begin(&self) -> Result<Self::Tx, WorkflowError> {
            Ok(MemTx {
                state: Rc::clone(&self.state),
            })
        }
    }

    #[async_trait(?Send)]
    impl WorkflowTx for MemTx {
        async fn commit(self) -> Result<(), WorkflowError> {
            self.state.borrow_mut().committed = true;
            Ok(())
        }

        async fn lock_run_for_apply(
            &mut self,
            _run_id: &str,
        ) -> Result<Option<RunLockRow>, WorkflowError> {
            let state = self.state.borrow();
            Ok(Some(RunLockRow {
                app_id: state.app_id,
                workflow_name: "TestWorkflow".to_string(),
                deploy_id: state.deploy_id.clone(),
                claimed_by: Some(state.owner_id.clone()),
                state: state.state.clone(),
                dispatch_nonce: Some(state.dispatch_nonce.clone()),
                stuck_strikes: 0,
                tree_depth: 0,
                compensation_target: None,
                current_error: None,
            }))
        }

        async fn apply_compensation_outcome(
            &mut self,
            _run_id: &str,
            _dispatch_nonce: &str,
            _ordinal: i32,
            _name: &str,
            _name_occurrence: i32,
            _state: &'static str,
            _error: Option<&Value>,
        ) -> Result<(), WorkflowError> {
            Ok(())
        }

        async fn pending_compensation_count(&mut self, _run_id: &str) -> Result<i64, WorkflowError> {
            Ok(self.state.borrow().pending_compensation_count)
        }

        async fn compensation_progress(
            &mut self,
            _run_id: &str,
        ) -> Result<CompensationProgress, WorkflowError> {
            Ok(CompensationProgress {
                total: 0,
                completed: 0,
                failed: 0,
                pending: 0,
                running: 0,
            })
        }

        async fn next_compensation_wake_at(
            &mut self,
            _run_id: &str,
        ) -> Result<Option<DateTime<Utc>>, WorkflowError> {
            Ok(None)
        }

        async fn update_compensating_run(
            &mut self,
            _config: &WorkflowEngineConfig,
            _run_id: &str,
            _dispatch_nonce: &str,
            _update: &CompensatingRunUpdate,
        ) -> Result<u64, WorkflowError> {
            Ok(0)
        }

        async fn prepare_child_spawn(
            &mut self,
            _config: &WorkflowEngineConfig,
            _parent_run_id: &str,
            _app_id: &Uuid,
            _deploy_id: &str,
            _parent_tree_depth: i16,
            _checkpoint: &mut StepCheckpoint,
        ) -> Result<Result<(), String>, WorkflowError> {
            Ok(Ok(()))
        }

        async fn continue_as_new(
            &mut self,
            _config: &WorkflowEngineConfig,
            _current_run_id: &str,
            _app_id: &Uuid,
            _workflow_name: &str,
            seed_input: Option<&Value>,
            seed_input_ref: Option<&WorkflowOutputRef>,
        ) -> Result<String, WorkflowError> {
            let mut state = self.state.borrow_mut();
            state.continued_as_new_run_id = Some("run_successor".to_string());
            state.continued_seed_input = seed_input.cloned();
            state.continued_seed_input_ref = seed_input_ref.cloned();
            Ok("run_successor".to_string())
        }

        async fn insert_resolved_step(
            &mut self,
            _config: &WorkflowEngineConfig,
            _checkpoint: &StepCheckpoint,
            _run_id: &str,
            _batch_id: &str,
            _batch_width: i16,
        ) -> Result<StepWriteOutcome, WorkflowError> {
            self.state.borrow_mut().steps_written += 1;
            Ok(StepWriteOutcome::Wrote)
        }

        async fn upsert_subscription(
            &mut self,
            _app_id: &Uuid,
            _run_id: &str,
            _checkpoint: &StepCheckpoint,
        ) -> Result<(), WorkflowError> {
            Ok(())
        }

        async fn delete_subscription(
            &mut self,
            _run_id: &str,
            _ordinal: i32,
        ) -> Result<(), WorkflowError> {
            Ok(())
        }

        async fn mark_signal_consumed(
            &mut self,
            _run_id: &str,
            _signal_id: &str,
        ) -> Result<(), WorkflowError> {
            Ok(())
        }

        async fn write_paused_run(
            &mut self,
            _config: &WorkflowEngineConfig,
            _run_id: &str,
            _dispatch_nonce: &str,
            _update: &PausedRunUpdate,
        ) -> Result<u64, WorkflowError> {
            Ok(0)
        }

        async fn run_output_journal_bytes(
            &mut self,
            _output: &Option<Value>,
            _output_ref: Option<&WorkflowOutputRef>,
        ) -> Result<i64, WorkflowError> {
            Ok(0)
        }

        async fn transition_run(
            &mut self,
            _config: &WorkflowEngineConfig,
            _run_id: &str,
            _dispatch_nonce: &str,
            update: &TransitionRunUpdate,
        ) -> Result<u64, WorkflowError> {
            let mut state = self.state.borrow_mut();
            state.transitioned_state = Some(update.state.clone());
            state.transitioned_output = update.output.clone();
            Ok(1)
        }

        async fn upsert_blob_ref(
            &mut self,
            _output_ref: &WorkflowOutputRef,
        ) -> Result<(), WorkflowError> {
            Ok(())
        }

        async fn emit_child_terminal_signal(
            &mut self,
            _child_run_id: &str,
            _terminal: ChildTerminalPayload<'_>,
        ) -> Result<(), WorkflowError> {
            Ok(())
        }

        async fn cascade_cancel_children(
            &mut self,
            _parent_run_id: &str,
        ) -> Result<u64, WorkflowError> {
            Ok(0)
        }
    }

    #[test]
    fn fold_outcomes_is_canonical_for_completed_step() {
        let (checkpoints, run_update) = fold_outcomes(&[
            StepOutcome::StepCompleted {
                ordinal: 0,
                name: "charge".to_string(),
                name_occurrence: 0,
                step_kind: "run".to_string(),
                compensable: true,
                compensation_max_attempts: 2,
                output: Some(serde_json::json!({"ok": true})),
                output_ref: None,
            },
            StepOutcome::RunCompleted {
                output: Some(serde_json::json!({"done": true})),
                output_ref: None,
            },
        ])
        .expect("fold");

        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].compensation_state.as_deref(), Some("pending"));
        assert!(matches!(run_update, RunUpdate::Completed { .. }));
    }

    #[compio::test]
    async fn apply_step_result_uses_store_port() {
        let app_id = Uuid::new_v4();
        let store = MemStore {
            state: Rc::new(RefCell::new(MemState {
                app_id,
                deploy_id: "dep_test".to_string(),
                owner_id: "owner-a".to_string(),
                dispatch_nonce: "wfd_test".to_string(),
                state: "running".to_string(),
                ..MemState::default()
            })),
        };
        let config = WorkflowEngineConfig {
            owner_id: "owner-a".to_string(),
            ..WorkflowEngineConfig::default()
        };
        let result: StepResult = serde_json::from_value(serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "outcomes": [
                {
                    "kind": "StepCompleted",
                    "ordinal": 0,
                    "name": "step",
                    "output": {"ok": true}
                },
                {
                    "kind": "RunCompleted",
                    "output": {"done": true}
                }
            ]
        }))
        .expect("step result");

        let applied = apply_step_result_on_store(&store, &config, result)
            .await
            .expect("apply");
        let state = store.state.borrow();
        assert!(applied);
        assert!(state.committed);
        assert_eq!(state.steps_written, 1);
        assert_eq!(state.transitioned_state.as_deref(), Some("completed"));
        assert_eq!(state.transitioned_output, Some(serde_json::json!({"done": true})));
    }

    #[compio::test]
    async fn apply_continue_as_new_creates_successor_before_terminal_transition() {
        let app_id = Uuid::new_v4();
        let store = MemStore {
            state: Rc::new(RefCell::new(MemState {
                app_id,
                deploy_id: "dep_test".to_string(),
                owner_id: "owner-a".to_string(),
                dispatch_nonce: "wfd_test".to_string(),
                state: "running".to_string(),
                ..MemState::default()
            })),
        };
        let config = WorkflowEngineConfig {
            owner_id: "owner-a".to_string(),
            ..WorkflowEngineConfig::default()
        };
        let result: StepResult = serde_json::from_value(serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "outcomes": [
                {
                    "kind": "ContinueAsNew",
                    "input": {"generation": 1}
                }
            ]
        }))
        .expect("step result");

        let applied = apply_step_result_on_store(&store, &config, result)
            .await
            .expect("apply");
        let state = store.state.borrow();
        assert!(applied);
        assert!(state.committed);
        assert_eq!(state.continued_as_new_run_id.as_deref(), Some("run_successor"));
        assert_eq!(
            state.continued_seed_input,
            Some(serde_json::json!({"generation": 1}))
        );
        assert_eq!(state.steps_written, 0);
        assert_eq!(state.transitioned_state.as_deref(), Some("completed"));
        assert!(state.transitioned_output.is_none());
    }

    #[compio::test]
    async fn apply_continue_as_new_rejects_pending_compensation_before_writes() {
        let app_id = Uuid::new_v4();
        let store = MemStore {
            state: Rc::new(RefCell::new(MemState {
                app_id,
                deploy_id: "dep_test".to_string(),
                owner_id: "owner-a".to_string(),
                dispatch_nonce: "wfd_test".to_string(),
                state: "running".to_string(),
                pending_compensation_count: 1,
                ..MemState::default()
            })),
        };
        let config = WorkflowEngineConfig {
            owner_id: "owner-a".to_string(),
            ..WorkflowEngineConfig::default()
        };
        let result: StepResult = serde_json::from_value(serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "outcomes": [
                {
                    "kind": "ContinueAsNew",
                    "input": {"generation": 1}
                }
            ]
        }))
        .expect("step result");

        let err = apply_step_result_on_store(&store, &config, result)
            .await
            .expect_err("pending compensation must reject continue-as-new");
        assert!(matches!(err, WorkflowError::CompensableCarry(_)));
        let state = store.state.borrow();
        assert!(state.committed);
        assert_eq!(state.steps_written, 0);
        assert!(state.continued_as_new_run_id.is_none());
        assert!(state.transitioned_state.is_none());
    }
}
