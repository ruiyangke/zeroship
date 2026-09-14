//! Identities for workflow coordination and durable delivery.

use crate::{entity_id::declare_entity_id, typed_id};

declare_entity_id! {
    /// An enrolled worker instance named by a placement assignment.
    WorkerId,
    typed_id::WORKER_INSTANCE_PREFIX,
    worker_id_tests,
}

declare_entity_id! {
    /// A customer's workflow run selected by a management command.
    RunId,
    typed_id::WORKFLOW_RUN_PREFIX,
    run_id_tests,
}

declare_entity_id! {
    /// Stable identity for a retried workflow mutation or management command.
    RequestId,
    typed_id::WORKFLOW_REQUEST_PREFIX,
    request_id_tests,
}

declare_entity_id! {
    /// Stable identity of a logical job, preserved across delivery attempts.
    JobId,
    typed_id::WORKFLOW_JOB_PREFIX,
    job_id_tests,
}

declare_entity_id! {
    /// Immutable normal app deployment selected for a job.
    DeploymentId,
    typed_id::DEPLOYMENT_PREFIX,
    deployment_id_tests,
}

declare_entity_id! {
    /// A logical workflow schedule, retained across deployment revisions.
    ScheduleId,
    typed_id::WORKFLOW_SCHEDULE_PREFIX,
    schedule_id_tests,
}

declare_entity_id! {
    /// A creator-owned broadcast expanded by durable fanout jobs.
    BroadcastId,
    typed_id::WORKFLOW_BROADCAST_PREFIX,
    broadcast_id_tests,
}
