pub mod pg;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::{StepCheckpoint, WorkflowEngineConfig, WorkflowOutputRef};
use crate::errors::WorkflowError;

#[derive(Debug, Clone)]
pub struct RunLockRow {
    pub app_id: Uuid,
    pub deploy_id: String,
    pub claimed_by: Option<String>,
    pub state: String,
    pub dispatch_nonce: Option<String>,
    pub stuck_strikes: i16,
    pub tree_depth: i16,
    pub compensation_target: Option<String>,
    pub current_error: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepWriteOutcome {
    Wrote,
    Noop,
    CapExceeded,
}

#[derive(Debug, Clone, Copy)]
pub struct CompensationProgress {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub pending: i64,
    pub running: i64,
}

impl CompensationProgress {
    pub fn remaining(self) -> i64 {
        self.pending + self.running
    }

    pub fn terminal_outcome(self) -> &'static str {
        if self.failed > 0 {
            "partial"
        } else {
            "completed"
        }
    }
}

#[derive(Debug, Clone)]
pub struct PausedRunUpdate {
    pub wake_at: Option<DateTime<Utc>>,
    pub next_ordinal: i32,
    pub waiting_step_key: Option<String>,
    pub paused_from_status: &'static str,
}

#[derive(Debug, Clone)]
pub struct TransitionRunUpdate {
    pub state: String,
    pub output: Option<Value>,
    pub error: Option<Value>,
    pub wake_at: Option<DateTime<Utc>>,
    pub next_ordinal: i32,
    pub waiting_step_key: Option<String>,
    pub stuck_strikes: i16,
    pub output_kind: &'static str,
    pub output_hash: Option<String>,
    pub output_size: Option<i64>,
    pub output_content_type: Option<String>,
    pub run_journal_delta: i64,
    pub blob_bytes_delta: i64,
    pub compensation_target: Option<String>,
    pub compensation_outcome: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompensatingRunUpdate {
    pub state: String,
    pub error: Value,
    pub wake_at: Option<DateTime<Utc>>,
    pub compensation_outcome: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChildTerminalPayload<'a> {
    pub state: &'a str,
    pub output: Option<Value>,
    pub output_ref: Option<WorkflowOutputRef>,
    pub error: Option<Value>,
}

#[async_trait(?Send)]
pub trait WorkflowStore {
    type Tx: WorkflowTx;

    async fn begin(&self) -> Result<Self::Tx, WorkflowError>;
}

#[async_trait(?Send)]
pub trait WorkflowTx {
    async fn commit(self) -> Result<(), WorkflowError>
    where
        Self: Sized;

    async fn lock_run_for_apply(
        &mut self,
        run_id: &str,
    ) -> Result<Option<RunLockRow>, WorkflowError>;

    async fn apply_compensation_outcome(
        &mut self,
        run_id: &str,
        dispatch_nonce: &str,
        ordinal: i32,
        name: &str,
        name_occurrence: i32,
        state: &'static str,
        error: Option<&Value>,
    ) -> Result<(), WorkflowError>;

    async fn pending_compensation_count(&mut self, run_id: &str) -> Result<i64, WorkflowError>;

    async fn compensation_progress(
        &mut self,
        run_id: &str,
    ) -> Result<CompensationProgress, WorkflowError>;

    async fn next_compensation_wake_at(
        &mut self,
        run_id: &str,
    ) -> Result<Option<DateTime<Utc>>, WorkflowError>;

    async fn update_compensating_run(
        &mut self,
        config: &WorkflowEngineConfig,
        run_id: &str,
        dispatch_nonce: &str,
        update: &CompensatingRunUpdate,
    ) -> Result<u64, WorkflowError>;

    async fn prepare_child_spawn(
        &mut self,
        config: &WorkflowEngineConfig,
        parent_run_id: &str,
        app_id: &Uuid,
        deploy_id: &str,
        parent_tree_depth: i16,
        checkpoint: &mut StepCheckpoint,
    ) -> Result<Result<(), String>, WorkflowError>;

    async fn insert_resolved_step(
        &mut self,
        checkpoint: &StepCheckpoint,
        run_id: &str,
        batch_id: &str,
        batch_width: i16,
    ) -> Result<StepWriteOutcome, WorkflowError>;

    async fn upsert_subscription(
        &mut self,
        app_id: &Uuid,
        run_id: &str,
        checkpoint: &StepCheckpoint,
    ) -> Result<(), WorkflowError>;

    async fn delete_subscription(
        &mut self,
        run_id: &str,
        ordinal: i32,
    ) -> Result<(), WorkflowError>;

    async fn mark_signal_consumed(
        &mut self,
        run_id: &str,
        signal_id: &str,
    ) -> Result<(), WorkflowError>;

    async fn write_paused_run(
        &mut self,
        config: &WorkflowEngineConfig,
        run_id: &str,
        dispatch_nonce: &str,
        update: &PausedRunUpdate,
    ) -> Result<u64, WorkflowError>;

    async fn run_output_journal_bytes(
        &mut self,
        output: &Option<Value>,
        output_ref: Option<&WorkflowOutputRef>,
    ) -> Result<i64, WorkflowError>;

    async fn transition_run(
        &mut self,
        config: &WorkflowEngineConfig,
        run_id: &str,
        dispatch_nonce: &str,
        update: &TransitionRunUpdate,
    ) -> Result<u64, WorkflowError>;

    async fn upsert_blob_ref(
        &mut self,
        output_ref: &WorkflowOutputRef,
    ) -> Result<(), WorkflowError>;

    async fn emit_child_terminal_signal(
        &mut self,
        child_run_id: &str,
        terminal: ChildTerminalPayload<'_>,
    ) -> Result<(), WorkflowError>;

    async fn cascade_cancel_children(&mut self, parent_run_id: &str)
        -> Result<u64, WorkflowError>;
}
