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

declare_entity_id! {
    /// A creator-owned dependency propagation obligation paged by durable jobs.
    PropagationId,
    typed_id::WORKFLOW_PROPAGATION_PREFIX,
    propagation_id_tests,
}

impl JobId {
    /// Build the job id a caller has already derived from the work the job
    /// names, rather than minting a fresh one.
    ///
    /// The one exception to the vocabulary's rule that an id arrives by minting
    /// or by parsing, and it is narrow on purpose: the argument is 128 raw bits,
    /// so no other entity's id and no free-text value can reach it, and the
    /// printed result is an ordinary `wjb_<base36>` that [`JobId::parse`]
    /// accepts.
    ///
    /// # What this id does NOT carry
    ///
    /// A minted body is a `UUIDv7`, so minted ids sort in creation order under
    /// bytewise collation. A derived body is not a timestamp and does not.
    /// Every cursor over a job id is therefore a keyset over a total order and
    /// nothing more: `pending_jobs` and `publish_pending` page one consistent
    /// ordering, and the reconciliation sweep bounds a cycle by the greatest id
    /// pending when it started, which is still an upper bound on everything
    /// that existed at that moment. No reader may infer WHEN a job was recorded
    /// from where it sorts.
    #[must_use]
    pub fn derived(body: [u8; 16]) -> Self {
        Self {
            text: format!(
                "{}_{}",
                Self::PREFIX,
                typed_id::uuid_to_base36(&uuid::Uuid::from_bytes(body))
            ),
        }
    }
}

#[cfg(test)]
mod derived_job_id_tests {
    use super::JobId;

    /// A derived id is a `JobId`, not an id-shaped string: it parses, it keeps
    /// the prefix, and the two extremes of the 128-bit range round trip rather
    /// than overflowing the fixed-width body.
    #[test]
    fn a_derived_body_parses_as_a_job_id() {
        let mut middle = [0u8; 16];
        for (index, byte) in middle.iter_mut().enumerate() {
            *byte = u8::try_from(index).expect("index below 16") * 17;
        }
        for body in [[0u8; 16], [0xffu8; 16], middle] {
            let derived = JobId::derived(body);
            let parsed = JobId::parse(derived.as_str()).expect("a derived id must parse");
            assert_eq!(parsed, derived);
            assert!(
                derived.as_str().starts_with(&format!("{}_", JobId::PREFIX)),
                "{}",
                derived.as_str()
            );
        }
    }

    /// The derivation is injective over the body, so two identities cannot
    /// share a key. PAIRED WITH A CONTROL: the same body twice must give the
    /// same id, or the inequality above would hold for a minter.
    #[test]
    fn distinct_bodies_give_distinct_ids_and_equal_bodies_agree() {
        let mut one = [7u8; 16];
        let other = one;
        assert_eq!(JobId::derived(one), JobId::derived(other));
        one[15] ^= 1;
        assert_ne!(JobId::derived(one), JobId::derived(other));
    }
}
