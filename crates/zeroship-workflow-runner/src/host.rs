//! Runtime-local lifecycle for an enrolled worker pulling zone-scoped jobs.

#![expect(
    clippy::future_not_send,
    reason = "worker lifecycle futures retain their owning compio resources"
)]

use crate::{
    consumer::{ConsumerOptions, JobConsumer},
    prepared::{AppFeed, CreatorFactory, PreparedApps, PreparedOptions},
    publication::HostTransport,
};
use std::{future::Future, rc::Rc};
use zeroship_workflow::WorkflowServiceError;
use zeroship_workflow_client::WorkerCoordinator;

#[derive(Clone, Copy, Debug)]
pub struct HostOptions {
    pub consumer: ConsumerOptions,
    pub prepared: PreparedOptions,
}

pub const STACK_BYTES: usize = 16 * 1024 * 1024;

pub fn thread() -> std::thread::Builder {
    std::thread::Builder::new()
        .name("workflow-host".into())
        .stack_size(STACK_BYTES)
}

pub struct WorkerHost<F: CreatorFactory<Journal = ()>> {
    client: WorkerCoordinator,
    prepared: Rc<PreparedApps<F>>,
    consumer: JobConsumer<HostTransport, F>,
    started: bool,
}

impl<F: CreatorFactory<Journal = ()>> std::fmt::Debug for WorkerHost<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerHost")
            .field("worker", &self.client.worker_id().as_str())
            .field("prepared", &self.prepared)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl<F: CreatorFactory<Journal = ()> + 'static> WorkerHost<F> {
    /// Compose the claimer, its slots and the prepared-app cache.
    ///
    /// `feed` answers whether the version feed still lists an app; an app it
    /// stops listing leaves the cache on the next claim cycle.
    ///
    /// # Errors
    /// Refuses a cache smaller than the slots it serves, and invalid bounds,
    /// before any network I/O.
    pub fn new(
        client: WorkerCoordinator,
        factory: F,
        feed: AppFeed,
        options: HostOptions,
    ) -> Result<Self, WorkflowServiceError> {
        if options.prepared.capacity < options.consumer.slots {
            return Err(WorkflowServiceError::InvalidRequest(
                "prepared app capacity must cover every execution slot".into(),
            ));
        }
        let prepared = Rc::new(PreparedApps::new(factory, feed, options.prepared)?);
        let consumer = JobConsumer::new(
            Rc::new(HostTransport {
                client: client.clone(),
            }),
            client.worker_id().clone(),
            prepared.clone(),
            options.consumer,
        )?;
        Ok(Self {
            client,
            prepared,
            consumer,
            started: false,
        })
    }

    /// Claim and run deliveries until `shutdown`, then let the running ones
    /// finish and settle within the consumer's drain bound and cancel and join
    /// what remains. A host runs once.
    ///
    /// # Errors
    /// Refuses a host that has already run or drained.
    pub async fn run_until(
        &mut self,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), WorkflowServiceError> {
        if self.started {
            return Err(WorkflowServiceError::Conflict(
                "workflow worker lifecycle has already started or drained".into(),
            ));
        }
        self.started = true;
        self.consumer.run_until(shutdown).await;
        Ok(())
    }

    /// Join every interrupted execution. After this the host cannot run.
    pub async fn drain(&mut self) {
        self.started = true;
        self.consumer.drain().await;
    }
}

#[cfg(test)]
mod tests;
