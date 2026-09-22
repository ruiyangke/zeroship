#![expect(
    clippy::future_not_send,
    reason = "collection fixtures use compio journals"
)]

use super::*;
use crate::service::{collection::CollectionOptions, AppWorkflows, TaskAssignment};
use std::time::{Duration, Instant};
use zeroship_core::{
    workflow_coordination::WorkerId,
    workflow_jobs::{Delivery, JobId, JobLease, JobOperation, JobOutcome, JobSpec},
};
use zeroship_data_orm::orm::{FindOptions, FromRow};

mod authority;
pub(in crate::service::tests) mod fixture;
mod pages;
mod references;
mod retirement;
mod rollback;
use fixture::*;

macro_rules! paired {
    ($sqlite:ident, $postgres:ident, $case:path) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($case(Rc::new(
                sqlite_store(&directory.path().join("journal.sqlite")).await,
            )))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let database = PostgresFixture::start().await;
            Box::pin($case(Rc::new(database.store.clone()))).await;
        }
    };
}

paired!(
    sqlite_collect_pages_preserve_cutoff_and_scope,
    postgres_collect_pages_preserve_cutoff_and_scope,
    pages::pages
);
paired!(
    sqlite_collect_replay_requires_exact_closed_page,
    postgres_collect_replay_requires_exact_closed_page,
    pages::replay
);
paired!(
    sqlite_collect_failed_and_malformed_prefixes_do_not_trap_suffix,
    postgres_collect_failed_and_malformed_prefixes_do_not_trap_suffix,
    pages::fairness
);
paired!(
    sqlite_collect_expired_replay_needs_no_storage,
    postgres_collect_expired_replay_needs_no_storage,
    pages::expired_replay
);
paired!(
    sqlite_collect_concurrent_sweeps_advance_one_app_scan_once,
    postgres_collect_concurrent_sweeps_advance_one_app_scan_once,
    pages::lost_update
);
paired!(
    sqlite_collect_policy_replacement_cancels_delete,
    postgres_collect_policy_replacement_cancels_delete,
    authority::replacement
);
paired!(
    sqlite_collect_original_policy_deadline_cannot_be_extended,
    postgres_collect_original_policy_deadline_cannot_be_extended,
    authority::original_expiry
);
paired!(
    sqlite_collect_delivery_expiry_retains_reserved_intent,
    postgres_collect_delivery_expiry_retains_reserved_intent,
    authority::delivery_expiry
);
paired!(
    sqlite_collect_stale_delete_confirmation_cannot_extend_newer_tombstone,
    postgres_collect_stale_delete_confirmation_cannot_extend_newer_tombstone,
    authority::stale_confirmation
);
paired!(
    sqlite_collect_keeps_references_and_history_when_admission_disabled,
    postgres_collect_keeps_references_and_history_when_admission_disabled,
    references::retained
);
paired!(
    sqlite_collect_tombstone_resweeps_late_upload_without_reopening_promotion,
    postgres_collect_tombstone_resweeps_late_upload_without_reopening_promotion,
    references::late_upload
);
paired!(
    sqlite_collect_racing_resweeps_leave_one_final_tombstone,
    postgres_collect_racing_resweeps_leave_one_final_tombstone,
    retirement::concurrent_resweep
);
paired!(
    sqlite_collect_final_tombstone_leaves_the_payload_quota,
    postgres_collect_final_tombstone_leaves_the_payload_quota,
    retirement::quota
);
paired!(
    sqlite_collect_final_tombstone_lets_the_app_retire,
    postgres_collect_final_tombstone_lets_the_app_retire,
    retirement::retirement
);
paired!(
    sqlite_collect_lost_delete_reply_recovers_in_later_duty,
    postgres_collect_lost_delete_reply_recovers_in_later_duty,
    rollback::lost_reply
);
paired!(
    sqlite_collect_unsettled_page_resumes_on_its_frozen_plan,
    postgres_collect_unsettled_page_resumes_on_its_frozen_plan,
    pages::frozen_plan
);
