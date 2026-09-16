#![expect(
    clippy::future_not_send,
    reason = "restart preparation tests retain compio-local transactions"
)]

use super::super::*;
use crate::{
    operations::{RestartOptions, RunState},
    service::{app, control::restart, policy::PolicyAuthority, AppWorkflows},
};
use fixture::{ready, Fixture};

mod damage;
mod fixture;
mod precedence;

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:path) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            let store = sqlite_store(&directory.path().join("workflow.sqlite")).await;
            Box::pin($contract(Rc::new(store))).await;
        }

        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_restart_exact_binding_retains_transaction_and_target,
    postgres_restart_exact_binding_retains_transaction_and_target,
    transaction_and_target
);
case!(
    sqlite_restart_exact_binding_refuses_foreign_transaction,
    postgres_restart_exact_binding_refuses_foreign_transaction,
    foreign_transaction
);
case!(
    sqlite_restart_exact_binding_rejects_changed_expected_registration,
    postgres_restart_exact_binding_rejects_changed_expected_registration,
    damage::changed_expected
);
case!(
    sqlite_restart_exact_binding_retries_damaged_local_metadata,
    postgres_restart_exact_binding_retries_damaged_local_metadata,
    damage::damaged_local
);
case!(
    sqlite_ordinary_latest_restart_requires_the_exact_held_target,
    postgres_ordinary_latest_restart_requires_the_exact_held_target,
    damage::ordinary_latest
);
case!(
    sqlite_restart_preparation_keeps_lifecycle_before_target_selection,
    postgres_restart_preparation_keeps_lifecycle_before_target_selection,
    precedence::lifecycle
);
case!(
    sqlite_restart_preparation_keeps_target_before_counter_exhaustion,
    postgres_restart_preparation_keeps_target_before_counter_exhaustion,
    precedence::counters
);

fn attempt(scope: &AppWorkflows) -> (AppWorkflows, PolicyAuthority) {
    let authority = scope.capture_policy().authority().unwrap().clone();
    let bound = scope.clone().with_authority(authority.clone()).unwrap();
    (bound, authority)
}

async fn transaction_and_target(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let before = fixture.snapshot().await;
    assert!(!before["job_publications"].is_empty());
    assert_ne!(fixture.original.id, fixture.replacement.id);

    let (scope, authority) = attempt(&fixture.scope());
    let mut tx = scope.service.begin().await.unwrap();
    app::lock_app(&mut tx, &fixture.owner).await.unwrap();
    let now = tx.now().await.unwrap();
    let draft = ready(
        restart::prepare_draft(
            &mut tx,
            &fixture.owner,
            &fixture.run,
            &RestartOptions::default(),
            &authority.policy,
            now,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let plan = ready(draft.bind_exact(&fixture.original).await.unwrap()).unwrap();
    let staged = plan.apply().await.unwrap();
    assert_eq!(staged.state, RunState::Queued);
    assert_eq!(staged.run_id, fixture.run);
    assert_eq!(staged.pinned_to, fixture.original.id);
    fixture
        .assert_generation_in(&tx, 1, &fixture.original.id)
        .await;
    // Application has released the borrow, but only the caller can commit it.
    drop(tx);
    assert_eq!(fixture.snapshot().await, before);

    let applied = fixture.apply_exact(&fixture.original).await.unwrap();
    assert_eq!(applied.pinned_to, fixture.original.id);
    fixture.assert_generation(1, &fixture.original.id).await;
    assert_eq!(fixture.active().await, fixture.replacement);

    let ordinary = fixture
        .scope()
        .restart(&RequestId::mint(), &fixture.run, RestartOptions::default())
        .await
        .unwrap();
    assert_eq!(ordinary.pinned_to, fixture.replacement.id);
    fixture.assert_generation(2, &fixture.replacement.id).await;
}

async fn foreign_transaction(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let before = fixture.snapshot().await;
    let foreign = fixture.journal.fixture_app(fixture.foreign.clone());
    let (scope, authority) = attempt(&foreign);
    let mut tx = scope.service.begin().await.unwrap();
    app::lock_app(&mut tx, &fixture.foreign).await.unwrap();
    let now = tx.now().await.unwrap();
    assert!(matches!(
        restart::prepare_draft(
            &mut tx,
            &fixture.owner,
            &fixture.run,
            &RestartOptions::default(),
            &authority.policy,
            now,
        )
        .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    drop(tx);
    assert_eq!(fixture.snapshot().await, before);
    fixture.apply_exact(&fixture.original).await.unwrap();
    fixture.assert_generation(1, &fixture.original.id).await;
}
