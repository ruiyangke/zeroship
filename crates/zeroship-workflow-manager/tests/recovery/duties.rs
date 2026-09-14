use super::*;

case!(
    sqlite_recovery_rejects_damaged_duty_pair,
    postgres_recovery_rejects_damaged_duty_pair,
    damaged_pair
);
case!(
    sqlite_recovery_duties_keep_independent_receipts,
    postgres_recovery_duties_keep_independent_receipts,
    independent_receipts
);
case!(
    sqlite_recovery_rejects_wrong_pending_duty,
    postgres_recovery_rejects_wrong_pending_duty,
    pending_kind
);
case!(
    sqlite_recovery_registers_duties_atomically,
    postgres_recovery_registers_duties_atomically,
    registration_rollback
);

const fn other(kind: DutyKind) -> DutyKind {
    match kind {
        DutyKind::Reconcile => DutyKind::Collect,
        DutyKind::Collect => DutyKind::Reconcile,
    }
}

async fn change(fixture: &Fixture, app: &AppId, kind: &str, patch: Value) {
    let output = fixture
        .database()
        .await
        .collection("recovery_duties")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str(),"kind":kind}),
            patch,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(output, Output::Count(1)));
}

async fn corrupt_scalar(fixture: &Fixture, app: &AppId, kind: &str, field: &str, value: &Value) {
    if field != "id" {
        change(fixture, app, kind, value!({field:value})).await;
        return;
    }
    let id = value.as_str().unwrap();
    match &fixture.admin {
        Admin::Sqlite(admin) => {
            let changed = admin
                .execute(
                    "UPDATE recovery_duties SET id=?1 WHERE app_id=?2 AND kind=?3",
                    (id, app.as_str(), kind),
                )
                .unwrap();
            assert_eq!(changed, 1);
        }
        Admin::Postgres(admin) => {
            let changed = admin
                .execute(
                    "UPDATE workflow_manager.recovery_duties SET id=$1 WHERE app_id=$2 AND kind=$3",
                    &[&id, &app.as_str(), &kind],
                )
                .await
                .unwrap();
            assert_eq!(changed, 1);
        }
    }
}

async fn provenance(fixture: &Fixture, app: &AppId) -> Vec<Value> {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection("recovery_scopes")
        .unwrap()
        .find(value!({"id":app.as_str()}), value!({"limit":1}))
        .await
        .unwrap()
    else {
        panic!("expected scope rows")
    };
    rows
}

async fn damaged_pair(fixture: &Fixture, kind: DutyKind) {
    let (recovery, _) = host(fixture).await;
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    recovery
        .ensure(&app, &deployment, 1.try_into().unwrap())
        .await
        .unwrap();
    let original = snapshot(fixture, &app, kind).await;
    let other_duty = snapshot(fixture, &app, other(kind)).await;
    let scope = provenance(fixture, &app).await;
    for (field, damaged) in [("id", value!("invalid")), ("next_due_at", value!(-1))] {
        corrupt_scalar(fixture, &app, kind.as_str(), field, &damaged).await;
        assert_eq!(recovery.dispatch(&app, kind).await, Err(Error::Storage));
        assert_eq!(
            recovery
                .ensure(&app, &deployment, 2.try_into().unwrap())
                .await,
            Err(Error::Storage)
        );
        assert_eq!(provenance(fixture, &app).await, scope);
        corrupt_scalar(fixture, &app, kind.as_str(), field, &original[field]).await;
    }
    change(fixture, &app, kind.as_str(), value!({"kind":"unknown"})).await;
    assert_eq!(recovery.dispatch(&app, kind).await, Err(Error::Storage));
    assert_eq!(
        recovery
            .ensure(&app, &deployment, 2.try_into().unwrap())
            .await,
        Err(Error::Storage)
    );
    change(fixture, &app, "unknown", value!({"kind":kind.as_str()})).await;
    let db = fixture.database().await;
    db.collection("recovery_duties")
        .unwrap()
        .execute(Operation::Purge {
            filter: value!({"app_id":app.as_str(),"kind":kind.as_str()}),
            many: false,
        })
        .await
        .unwrap();
    assert_eq!(
        recovery
            .ensure(&app, &deployment, 2.try_into().unwrap())
            .await,
        Err(Error::Storage)
    );
    assert_eq!(provenance(fixture, &app).await, scope);
    assert_eq!(snapshot(fixture, &app, other(kind)).await, other_duty);
    assert_eq!(job_count(fixture, &app).await, 0);
    db.collection("recovery_duties")
        .unwrap()
        .insert(original.clone())
        .await
        .unwrap();
    recovery
        .ensure(&app, &deployment, 2.try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(snapshot(fixture, &app, kind).await, original);
    assert_eq!(snapshot(fixture, &app, other(kind)).await, other_duty);
}

async fn independent_receipts(fixture: &Fixture, kind: DutyKind) {
    let (recovery, queue) = host(fixture).await;
    let app = AppId::mint();
    recovery
        .ensure(&app, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    let first = recovery.dispatch(&app, kind).await.unwrap().unwrap();
    let second = recovery.dispatch(&app, other(kind)).await.unwrap().unwrap();
    set_deadline(fixture, &app, kind, i64::MAX).await;
    set_deadline(fixture, &app, other(kind), i64::MAX).await;
    let other_duty = snapshot(fixture, &app, other(kind)).await;
    let owner = assignment(&app);
    let delivery = queue.claim(&owner).await.unwrap().unwrap();
    assert_eq!(delivery.delivery().job, first);
    let waiting = Settlement {
        delivery: delivery.delivery().clone(),
        outcome: JobOutcome::Waiting {},
        successors: vec![],
    };
    let receipt = queue.settle(&owner, &waiting).await.unwrap();
    assert_eq!(snapshot(fixture, &app, other(kind)).await, other_duty);
    assert!(recovery.due(other(kind), None).await.unwrap().is_empty());
    let next = recovery.dispatch(&app, kind).await.unwrap().unwrap();
    assert_ne!(next.id, first.id);
    let next_duty = snapshot(fixture, &app, kind).await;
    assert_eq!(queue.settle(&owner, &waiting).await.unwrap(), receipt);
    assert_eq!(snapshot(fixture, &app, kind).await, next_duty);
    assert_eq!(snapshot(fixture, &app, other(kind)).await, other_duty);
    let delivery = queue.claim(&owner).await.unwrap().unwrap();
    assert_eq!(delivery.delivery().job, second);
    let completion = Settlement {
        delivery: delivery.delivery().clone(),
        outcome: JobOutcome::Completed {},
        successors: vec![],
    };
    queue.settle(&owner, &completion).await.unwrap();
    assert_eq!(snapshot(fixture, &app, kind).await, next_duty);
    assert_eq!(snapshot(fixture, &app, other(kind)).await, other_duty);
    queue.settle(&owner, &completion).await.unwrap();
    assert_eq!(snapshot(fixture, &app, kind).await, next_duty);
    assert_eq!(snapshot(fixture, &app, other(kind)).await, other_duty);
}

async fn pending_kind(fixture: &Fixture, kind: DutyKind) {
    let (recovery, _) = host(fixture).await;
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    recovery
        .ensure(&app, &deployment, 1.try_into().unwrap())
        .await
        .unwrap();
    let expected = recovery.dispatch(&app, kind).await.unwrap().unwrap();
    let wrong = recovery.dispatch(&app, other(kind)).await.unwrap().unwrap();
    change(
        fixture,
        &app,
        kind.as_str(),
        value!({"pending_job_id":wrong.id.as_str()}),
    )
    .await;
    let before = snapshot(fixture, &app, kind).await;
    assert_eq!(recovery.dispatch(&app, kind).await, Err(Error::Storage));
    assert_eq!(
        recovery
            .ensure(&app, &deployment, 2.try_into().unwrap())
            .await,
        Err(Error::Storage)
    );
    assert_eq!(snapshot(fixture, &app, kind).await, before);
    assert_eq!(
        recovery.dispatch(&app, other(kind)).await.unwrap(),
        Some(wrong)
    );
    change(
        fixture,
        &app,
        kind.as_str(),
        value!({"pending_job_id":expected.id.as_str()}),
    )
    .await;
    assert_eq!(recovery.dispatch(&app, kind).await.unwrap(), Some(expected));
    let foreign = AppId::mint();
    recovery
        .ensure(&foreign, &deployment, 1.try_into().unwrap())
        .await
        .unwrap();
    let foreign_job = recovery.dispatch(&foreign, kind).await.unwrap().unwrap();
    let before = snapshot(fixture, &app, kind).await;
    assert!(fixture
        .database()
        .await
        .collection("recovery_duties")
        .unwrap()
        .update(
            value!({"app_id":app.as_str(),"kind":kind.as_str()}),
            value!({"pending_job_id":foreign_job.id.as_str()}),
        )
        .await
        .is_err());
    assert_eq!(snapshot(fixture, &app, kind).await, before);
}

async fn registration_rollback(fixture: &Fixture, _kind: DutyKind) {
    let (recovery, _) = host(fixture).await;
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    registration_fault(fixture, true).await;
    assert!(recovery
        .ensure(&app, &deployment, 1.try_into().unwrap())
        .await
        .is_err());
    assert!(provenance(fixture, &app).await.is_empty());
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        assert!(recovery.due(kind, None).await.unwrap().is_empty());
    }
    assert_eq!(job_count(fixture, &app).await, 0);
    registration_fault(fixture, false).await;
    recovery
        .ensure(&app, &deployment, 1.try_into().unwrap())
        .await
        .unwrap();
    for kind in [DutyKind::Reconcile, DutyKind::Collect] {
        assert_eq!(recovery.due(kind, None).await.unwrap(), vec![app.clone()]);
    }
}

async fn registration_fault(fixture: &Fixture, install: bool) {
    match &fixture.admin {
        Admin::Sqlite(admin) => admin.execute_batch(if install {
            "CREATE TRIGGER registration_fault BEFORE INSERT ON recovery_duties WHEN NEW.kind='collect' BEGIN SELECT RAISE(ABORT,'duty fault'); END;"
        } else {"DROP TRIGGER registration_fault;"}).unwrap(),
        Admin::Postgres(admin) => admin.batch_execute(if install {
            "CREATE FUNCTION workflow_manager.registration_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'duty fault'; END $$;
            CREATE TRIGGER registration_fault BEFORE INSERT ON workflow_manager.recovery_duties FOR EACH ROW WHEN (NEW.kind='collect') EXECUTE FUNCTION workflow_manager.registration_fault();"
        } else {"DROP TRIGGER registration_fault ON workflow_manager.recovery_duties; DROP FUNCTION workflow_manager.registration_fault();"}).await.unwrap(),
    }
}

case!(
    sqlite_recovery_validates_settled_pending_receipts,
    postgres_recovery_validates_settled_pending_receipts,
    settled_pending
);

async fn patch_job(fixture: &Fixture, job: &JobSpec, field: &str, value: &Value) {
    let output = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":job.id.as_str(),"app_id":job.app_id.as_str()}),
            patch: value!({field:value}),
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(output, Output::Count(1)));
}

async fn settled_pending(fixture: &Fixture, kind: DutyKind) {
    let (recovery, queue) = host(fixture).await;
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    recovery
        .ensure(&app, &deployment, 1.try_into().unwrap())
        .await
        .unwrap();
    let pending = recovery.dispatch(&app, kind).await.unwrap().unwrap();
    make_due(fixture, &app, kind).await;
    let duty = snapshot(fixture, &app, kind).await;
    patch_job(fixture, &pending, "state", &value!("settled")).await;
    assert_eq!(recovery.dispatch(&app, kind).await, Err(Error::Storage));
    assert_eq!(snapshot(fixture, &app, kind).await, duty);
    patch_job(fixture, &pending, "state", &value!("ready")).await;
    let owner = assignment(&app);
    let delivery = queue.claim(&owner).await.unwrap().unwrap();
    queue
        .settle(
            &owner,
            &Settlement {
                delivery: delivery.delivery().clone(),
                outcome: JobOutcome::Rejected {},
                successors: vec![],
            },
        )
        .await
        .unwrap();
    let record = stored_job(fixture, &pending).await.unwrap();
    let management = serde_json::to_string(&JobOutcome::Management {
        outcome: zeroship_core::workflow_coordination::ManagementOutcome::Denied {},
    })
    .unwrap();
    for (field, corrupt) in [
        ("outcome", Value::Null),
        ("outcome", value!("{}")),
        ("outcome", value!(management)),
        ("settlement_digest", Value::Null),
        ("settlement_digest", value!("broken")),
        ("settlement_digest", value!("A".repeat(64))),
    ] {
        patch_job(fixture, &pending, field, &corrupt).await;
        assert_eq!(recovery.dispatch(&app, kind).await, Err(Error::Storage));
        assert_eq!(
            recovery
                .ensure(&app, &deployment, 2.try_into().unwrap())
                .await,
            Err(Error::Storage)
        );
        assert_eq!(snapshot(fixture, &app, kind).await, duty);
        assert_eq!(job_count(fixture, &app).await, 1);
        patch_job(fixture, &pending, field, &record[field]).await;
    }
    let next = recovery.dispatch(&app, kind).await.unwrap().unwrap();
    assert_ne!(next.id, pending.id);
    assert_eq!(next.operation, operation(kind));
}

case!(
    sqlite_recovery_dispatches_kinds_concurrently,
    postgres_recovery_dispatches_kinds_concurrently,
    concurrent_kinds
);

async fn concurrent_kinds(fixture: &Fixture, kind: DutyKind) {
    let (first, _) = host(fixture).await;
    let (second, _) = host(fixture).await;
    let app = AppId::mint();
    first
        .ensure(&app, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    let (a, b) = futures::join!(
        first.dispatch(&app, kind),
        second.dispatch(&app, other(kind))
    );
    let first_job = a.unwrap().unwrap();
    let second_job = b.unwrap().unwrap();
    assert_ne!(first_job.id, second_job.id);
    assert_eq!(first_job.operation, operation(kind));
    assert_eq!(second_job.operation, operation(other(kind)));
    assert_eq!(job_count(fixture, &app).await, 2);
    let selected = snapshot(fixture, &app, kind).await;
    let following = snapshot(fixture, &app, other(kind)).await;
    let (reopened, _) = host(fixture).await;
    reopened
        .ensure(&app, &DeploymentId::mint(), 2.try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(snapshot(fixture, &app, kind).await, selected);
    assert_eq!(snapshot(fixture, &app, other(kind)).await, following);
    assert_eq!(
        reopened.dispatch(&app, kind).await.unwrap(),
        Some(first_job)
    );
    assert_eq!(
        reopened.dispatch(&app, other(kind)).await.unwrap(),
        Some(second_job)
    );
}

case!(
    sqlite_fanout_receipts_preserve_maintenance_duties,
    postgres_fanout_receipts_preserve_maintenance_duties,
    fanout_receipts
);

async fn fanout_receipts(fixture: &Fixture, kind: DutyKind) {
    let (recovery, queue) = host(fixture).await;
    let app = AppId::mint();
    recovery
        .ensure(&app, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    let owner = assignment(&app);
    for duty in [kind, other(kind)] {
        let pending = recovery.dispatch(&app, duty).await.unwrap().unwrap();
        let leased = queue.claim(&owner).await.unwrap().unwrap();
        assert_eq!(leased.delivery().job, pending);
        set_deadline(fixture, &app, duty, i64::MAX).await;
    }
    let selected = snapshot(fixture, &app, kind).await;
    let opposite = snapshot(fixture, &app, other(kind)).await;
    let job = JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Fanout {
            broadcast_id: zeroship_core::workflow_jobs::BroadcastId::mint(),
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    };
    queue.submit(&job).await.unwrap();
    let delivery = queue.claim(&owner).await.unwrap().unwrap();
    assert_eq!(delivery.delivery().job, job);
    let command = Settlement {
        delivery: delivery.delivery().clone(),
        outcome: JobOutcome::Waiting {},
        successors: vec![],
    };
    let receipt = queue.settle(&owner, &command).await.unwrap();
    assert_eq!(queue.settle(&owner, &command).await.unwrap(), receipt);
    assert_eq!(snapshot(fixture, &app, kind).await, selected);
    assert_eq!(snapshot(fixture, &app, other(kind)).await, opposite);
}
