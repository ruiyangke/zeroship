//! Persistent scheduling and bounded execution owned by a customer host.

#![expect(
    clippy::future_not_send,
    reason = "workflow slots and storage run on their owning compio thread"
)]

use super::{RunnerOutcome, RunnerSlot, TaskExecutor, WorkerTasks};
use crate::{service::WorkflowService, WorkflowServiceError};
use futures::{
    future::{Either, LocalBoxFuture, Shared},
    Future, FutureExt,
};
use serde::{Deserialize, Serialize};
use std::{rc::Rc, task::Poll, time::Duration};

/// Host limits shared by local and deployed workflow workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerOptions {
    /// Concurrent execution slots on this compio thread.
    pub task_slots: usize,
    /// Timeout for polling and for each execution or completion attempt.
    pub execution_timeout_ms: u64,
    /// Delay after a journal poll finds no eligible work.
    pub idle_poll_ms: u64,
    /// Delay before retrying a failed task attempt.
    pub error_backoff_ms: u64,
    /// Delay between scheduling and collection sweeps.
    pub maintenance_interval_ms: u64,
    /// Timeout for each maintenance operation.
    pub maintenance_timeout_ms: u64,
    /// Maximum upload records examined per collection sweep.
    pub payload_collection_batch: usize,
}
impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            task_slots: 1,
            execution_timeout_ms: 30_000,
            idle_poll_ms: 50,
            error_backoff_ms: 1_000,
            maintenance_interval_ms: 1_000,
            maintenance_timeout_ms: 5_000,
            payload_collection_batch: 64,
        }
    }
}
impl WorkerOptions {
    fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.task_slots == 0
            || self.execution_timeout_ms == 0
            || self.idle_poll_ms == 0
            || self.error_backoff_ms == 0
            || self.maintenance_interval_ms == 0
            || self.maintenance_timeout_ms == 0
            || self.payload_collection_batch == 0
            || self.payload_collection_batch > super::super::payloads::MAX_COLLECTION_BATCH
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow worker limits".into(),
            ));
        }
        Ok(())
    }
}

/// Drives durable work independently of request isolates or request traffic.
/// The host owns the compio thread and awaits this object's lifecycle.
pub struct WorkflowWorker {
    service: WorkflowService,
    slots: Vec<RunnerSlot>,
    options: WorkerOptions,
}
impl std::fmt::Debug for WorkflowWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowWorker")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}
impl WorkflowWorker {
    /// Construct the customer's worker from its bound task authority and executor.
    ///
    /// # Errors
    /// Rejects invalid limits and missing customer payload or executable storage.
    pub fn new(
        tasks: Rc<WorkerTasks>,
        executor: Rc<dyn TaskExecutor>,
        options: WorkerOptions,
    ) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        if tasks.service.snapshots.is_none() || tasks.service.payload_storage.is_none() {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow worker requires customer payload and executable storage".into(),
            ));
        }
        let service = tasks.service.clone();
        let timeout = Duration::from_millis(options.execution_timeout_ms);
        let mut slots = Vec::with_capacity(options.task_slots);
        for _ in 1..options.task_slots {
            slots.push(RunnerSlot::new(tasks.clone(), executor.clone(), timeout)?);
        }
        slots.push(RunnerSlot::new(tasks, executor, timeout)?);
        Ok(Self {
            service,
            slots,
            options,
        })
    }

    /// Drive scheduling, payload maintenance and bounded execution until shutdown.
    /// Returns only after active executions have stopped and their claims have
    /// been released or left to expire. An unresponsive executor keeps its slot.
    ///
    /// Dropping this future cancels active work. Call `drain` before discarding
    /// the host, or call `run_until` again to drain before reusing the slots.
    pub async fn run_until(&mut self, shutdown: impl Future<Output = ()>) {
        let shutdown = shutdown.boxed_local().shared();
        let executions = futures::future::join_all(
            self.slots
                .iter_mut()
                .map(|slot| run_slot(slot, self.options, shutdown.clone())),
        );
        let maintenance = maintain(&self.service, self.options, shutdown);
        futures::join!(executions, maintenance);
    }

    /// Stop and join executions retained after an interrupted host future.
    pub async fn drain(&mut self) {
        futures::future::join_all(self.slots.iter_mut().map(RunnerSlot::drain_interrupted)).await;
    }
}

type Shutdown<'a> = Shared<LocalBoxFuture<'a, ()>>;

async fn run_slot(slot: &mut RunnerSlot, options: WorkerOptions, shutdown: Shutdown<'_>) {
    loop {
        let result =
            match futures::future::select(shutdown.clone(), slot.run_once().boxed_local()).await {
                Either::Left(((), work)) => {
                    drop(work);
                    break;
                }
                Either::Right((result, _)) => result,
            };
        let delay = match result {
            Ok(RunnerOutcome::Idle) => options.idle_poll_ms,
            Ok(RunnerOutcome::Completed(_) | RunnerOutcome::Interrupted(_)) => {
                // Ready work must share its thread with maintenance and other slots.
                let mut yielded = false;
                futures::future::poll_fn(|cx| {
                    if yielded {
                        Poll::Ready(())
                    } else {
                        yielded = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                continue;
            }
            Err(error) => {
                // Customer errors and database connection details stay out of host logs.
                tracing::warn!(code = error.code(), "workflow task attempt failed");
                options.error_backoff_ms
            }
        };
        if stopped_after(shutdown.clone(), delay).await {
            break;
        }
    }
    slot.drain_interrupted().await;
}

async fn maintain(service: &WorkflowService, options: WorkerOptions, shutdown: Shutdown<'_>) {
    loop {
        let tick = async {
            match compio::time::timeout(
                Duration::from_millis(options.maintenance_timeout_ms),
                service.tick_schedules(),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(code = error.code(), "workflow scheduling failed"),
                Err(_) => tracing::warn!("workflow scheduling timed out"),
            }
            match compio::time::timeout(
                Duration::from_millis(options.maintenance_timeout_ms),
                service.collect_payloads(options.payload_collection_batch),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(code = error.code(), "workflow payload collection failed");
                }
                Err(_) => tracing::warn!("workflow payload collection timed out"),
            }
        }
        .boxed_local();
        if let Either::Left(((), tick)) = futures::future::select(shutdown.clone(), tick).await {
            drop(tick);
            break;
        }
        if stopped_after(shutdown.clone(), options.maintenance_interval_ms).await {
            break;
        }
    }
}

async fn stopped_after(shutdown: Shutdown<'_>, delay_ms: u64) -> bool {
    matches!(
        futures::future::select(
            shutdown,
            compio::time::sleep(Duration::from_millis(delay_ms)).boxed_local()
        )
        .await,
        Either::Left(_)
    )
}
