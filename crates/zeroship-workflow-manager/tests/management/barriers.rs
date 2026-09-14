use super::*;

case!(
    sqlite_management_barriers_filter_before_candidate_limit,
    postgres_management_barriers_filter_before_candidate_limit,
    prelimit_barriers
);
case!(
    sqlite_management_barriers_reject_damaged_native_linkage,
    postgres_management_barriers_reject_damaged_native_linkage,
    corrupted_barriers
);
case!(
    sqlite_management_settlement_rolls_back_and_rechecks_enrollment,
    postgres_management_settlement_rolls_back_and_rechecks_enrollment,
    settlement_authority
);
case!(
    sqlite_management_replay_keeps_independent_request_anchor,
    postgres_management_replay_keeps_independent_request_anchor,
    request_anchor
);
case!(
    sqlite_management_allowed_backlog_progresses_across_pages,
    postgres_management_allowed_backlog_progresses_across_pages,
    allowed_backlog
);

async fn prelimit_barriers(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let run = RunId::mint();
    host.queue.register_scope(&app).await.unwrap();
    let deployment = host.source.publish(&app, "advance").await;
    enqueue_blocked_advances(&host, &app, &run, &deployment.deployment_id).await;
    let first = command(&app, &run, RunOperation::Pause);
    let second = command(&app, &run, RunOperation::Cancel);
    host.manage(&first).await.unwrap();
    host.manage(&second).await.unwrap();
    let first_job = host.job(&first).await;
    let second_job = host.job(&second).await;
    let recovery = ordinary(&app);
    host.queue.submit(&recovery).await.unwrap();
    let authority = assignment(&app);
    let first_delivery = host
        .queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(first_delivery.job, first_job);
    let recovered = host
        .queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(recovered.job, recovery);
    host.queue
        .settle(
            &authority,
            &Settlement {
                delivery: recovered,
                outcome: JobOutcome::Completed {},
                successors: vec![],
            },
        )
        .await
        .unwrap();
    assert!(host.queue.claim(&authority).await.unwrap().is_none());
    // A propagation page executes nothing for this run, so the pending
    // blocking commands cannot delay the cancellation it may be finishing.
    let page = JobSpec {
        operation: JobOperation::Propagate {
            propagation_id: zeroship_core::workflow_jobs::PropagationId::mint(),
            revision: 1.try_into().unwrap(),
        },
        ..ordinary(&app)
    };
    host.queue.submit(&page).await.unwrap();
    let delivered = host
        .queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(delivered.job, page);
    host.queue
        .settle(
            &authority,
            &Settlement {
                delivery: delivered,
                outcome: JobOutcome::Completed {},
                successors: vec![],
            },
        )
        .await
        .unwrap();
    assert!(host.queue.claim(&authority).await.unwrap().is_none());
    let ack = Settlement {
        delivery: first_delivery,
        outcome: JobOutcome::Management {
            outcome: ManagementOutcome::Conflict {},
        },
        successors: vec![],
    };
    host.queue.settle(&authority, &ack).await.unwrap();
    let second_delivery = host
        .queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(second_delivery.job, second_job);
    let third = command(&app, &run, RunOperation::Pause);
    host.manage(&third).await.unwrap();
    let before = snapshot(&host).await;
    host.queue.settle(&authority, &ack).await.unwrap();
    assert_eq!(snapshot(&host).await, before);
    assert!(host.queue.claim(&authority).await.unwrap().is_none());
    host.queue
        .settle(
            &authority,
            &Settlement {
                delivery: second_delivery,
                outcome: JobOutcome::Management {
                    outcome: ManagementOutcome::Denied {},
                },
                successors: vec![],
            },
        )
        .await
        .unwrap();
    settle(
        &host,
        &authority,
        &host.job(&third).await,
        ManagementOutcome::NotFound {},
    )
    .await;
    let advance = host.queue.claim(&authority).await.unwrap().unwrap();
    assert!(matches!(
        advance.delivery().job.operation,
        JobOperation::Advance { .. }
    ));
}

async fn enqueue_blocked_advances(
    host: &Host,
    app: &AppId,
    run: &RunId,
    deployment: &DeploymentId,
) {
    for _ in 0..257 {
        let spec = JobSpec {
            operation: JobOperation::Advance {
                deployment_id: deployment.clone(),
                run_id: run.clone(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            ..ordinary(app)
        };
        host.queue.submit(&spec).await.unwrap();
    }
}

async fn damaged(
    host: &Host,
    authority: &Assignment,
    table: &str,
    id: &str,
    changes: Value,
    restore: Value,
) {
    patch(&host.database, table, id, changes).await;
    let before = snapshot(host).await;
    let acquired = host.holds.acquired.get();
    assert!(matches!(
        host.queue.claim(authority).await,
        Err(Error::Storage)
    ));
    assert_eq!(snapshot(host).await, before);
    assert_eq!(host.holds.acquired.get(), acquired);
    patch(&host.database, table, id, restore).await;
}

async fn corrupted_barriers(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let request = command(&app, &RunId::mint(), RunOperation::Pause);
    host.manage(&request).await.unwrap();
    let spec = host.job(&request).await;
    let authority = assignment(&app);
    let order = single(
        &host.database,
        "management_scopes",
        value!({"app_id":app.as_str()}),
    )
    .await;
    let stored = single(
        &host.database,
        "management",
        value!({"id":spec.id.as_str()}),
    )
    .await;
    let foreign_run = RunId::mint();
    host.database.collection("management_scopes").unwrap().insert(value!({"id":typed_id::generate("wmo"),"app_id":app.as_str(),"run_id":foreign_run.as_str(),"accepted_revision":0,"settled_revision":0})).await.unwrap();
    for (field, damaged_value) in [
        ("blocks_execution", value!(false)),
        (
            "outcome",
            value!(serde_json::to_string(&ManagementOutcome::NotFound {}).unwrap()),
        ),
        ("run_id", value!(foreign_run.as_str())),
        ("revision", value!(2)),
        (
            "request_id",
            value!(zeroship_core::workflow_coordination::RequestId::mint().as_str()),
        ),
        (
            "request_digest",
            value!(zeroship_bundle::sha256_hex(b"wrong request")),
        ),
    ] {
        damaged(
            &host,
            &authority,
            "management",
            spec.id.as_str(),
            Value::Object([(field.into(), damaged_value)].into()),
            Value::Object([(field.into(), stored[field].clone())].into()),
        )
        .await;
    }
    let job = single(&host.database, "jobs", value!({"id":spec.id.as_str()})).await;
    for (field, value) in [
        ("operation_kind", value!("collect")),
        ("run_id", Value::Null),
        ("management_request_id", Value::Null),
        ("state", value!("settled")),
    ] {
        damaged(
            &host,
            &authority,
            "jobs",
            spec.id.as_str(),
            Value::Object([(field.into(), value)].into()),
            Value::Object([(field.into(), job[field].clone())].into()),
        )
        .await;
    }
    for (field, value) in [
        ("accepted_revision", value!(0)),
        ("settled_revision", value!(1)),
    ] {
        damaged(
            &host,
            &authority,
            "management_scopes",
            order["id"].as_str().unwrap(),
            Value::Object([(field.into(), value)].into()),
            Value::Object([(field.into(), order[field].clone())].into()),
        )
        .await;
    }
    substitute_link(&host, &authority, &spec, stored).await;
    settle(&host, &authority, &spec, ManagementOutcome::NotFound {}).await;
    assert_eq!(host.holds.acquired.get(), 0);
}

async fn substitute_link(host: &Host, authority: &Assignment, spec: &JobSpec, original: Value) {
    let other = ordinary(&spec.app_id);
    host.queue.submit(&other).await.unwrap();
    let commands = host.database.collection("management").unwrap();
    commands
        .delete(value!({"id":spec.id.as_str()}))
        .await
        .unwrap();
    assert!(matches!(
        host.queue.claim(authority).await,
        Err(Error::Storage)
    ));
    let mut substituted = original.clone();
    substituted["id"] = value!(other.id.as_str());
    commands.insert(substituted).await.unwrap();
    assert!(matches!(
        host.queue.claim(authority).await,
        Err(Error::Storage)
    ));
    commands
        .delete(value!({"id":other.id.as_str()}))
        .await
        .unwrap();
    commands.insert(original).await.unwrap();
}

async fn settlement_authority(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let request = command(&app, &RunId::mint(), RunOperation::Pause);
    host.manage(&request).await.unwrap();
    let authority = assignment(&app);
    let delivery = host
        .queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    let successor = ordinary(&app);
    let settlement = Settlement {
        delivery,
        outcome: JobOutcome::Management {
            outcome: ManagementOutcome::NotFound {},
        },
        successors: vec![successor],
    };
    let before = snapshot(&host).await;
    let checks = Cell::new(0);
    let refused = host
        .queue
        .settle_authorized(
            &(&authority).into(),
            &settlement,
            |_| {
                checks.set(checks.get() + 1);
                ready(if checks.get() == 1 {
                    Ok(authority.clone())
                } else {
                    Err(Error::Denied)
                })
            },
            |_| ready(Ok(authority.worker_id.clone())),
        )
        .await;
    assert_eq!(refused, Err(Error::Denied));
    assert_eq!(checks.get(), 2);
    assert_eq!(snapshot(&host).await, before);
    let receipt = host.queue.settle(&authority, &settlement).await.unwrap();
    replay_authority(&host, &authority, &settlement, &receipt).await;
}

async fn replay_authority(
    host: &Host,
    authority: &Assignment,
    settlement: &Settlement,
    receipt: &zeroship_core::workflow_jobs::SettlementReceipt,
) {
    let before = snapshot(host).await;
    let replay_checks = Cell::new(0);
    assert_eq!(
        host.queue
            .settle_authorized(
                &authority.into(),
                settlement,
                |_| ready(Err(Error::Denied)),
                |_| {
                    replay_checks.set(replay_checks.get() + 1);
                    ready(Err(Error::Denied))
                }
            )
            .await,
        Err(Error::Denied)
    );
    assert_eq!(replay_checks.get(), 1);
    assert_eq!(snapshot(host).await, before);
    assert_eq!(
        host.queue.settle(authority, settlement).await.unwrap(),
        *receipt
    );
    let row = single(
        &host.database,
        "management",
        value!({"id":settlement.delivery.job.id.as_str()}),
    )
    .await;
    patch(
        &host.database,
        "management",
        settlement.delivery.job.id.as_str(),
        value!({"outcome":null}),
    )
    .await;
    replay_checks.set(0);
    assert_eq!(
        host.queue
            .settle_authorized(
                &authority.into(),
                settlement,
                |_| ready(Ok(authority.clone())),
                |_| {
                    replay_checks.set(replay_checks.get() + 1);
                    ready(Ok(authority.worker_id.clone()))
                }
            )
            .await,
        Err(Error::Storage)
    );
    assert_eq!(
        replay_checks.get(),
        0,
        "linkage reads must precede final enrollment callback"
    );
    patch(
        &host.database,
        "management",
        settlement.delivery.job.id.as_str(),
        value!({"outcome":row["outcome"]}),
    )
    .await;
    assert_eq!(host.holds.acquired.get(), 0);
}

async fn request_anchor(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let request = command(&app, &RunId::mint(), RunOperation::Pause);
    host.manage(&request).await.unwrap();
    let spec = host.job(&request).await;
    settle(
        &host,
        &assignment(&app),
        &spec,
        ManagementOutcome::NotFound {},
    )
    .await;
    let receipt = host.manage(&request).await.unwrap();
    for (table, field) in [
        ("management", "request_id"),
        ("jobs", "management_request_id"),
    ] {
        patch(
            &host.database,
            table,
            spec.id.as_str(),
            Value::Object(
                [(
                    field.into(),
                    value!(zeroship_core::workflow_coordination::RequestId::mint().as_str()),
                )]
                .into(),
            ),
        )
        .await;
        let before = snapshot(&host).await;
        assert_eq!(host.manage(&request).await, Err(Error::Storage));
        assert_eq!(
            host.coordinator
                .management_receipt(&app, &request.request_id)
                .await,
            Err(Error::Storage)
        );
        assert_eq!(snapshot(&host).await, before);
        patch(
            &host.database,
            table,
            spec.id.as_str(),
            Value::Object([(field.into(), value!(request.request_id.as_str()))].into()),
        )
        .await;
        assert_eq!(host.manage(&request).await.unwrap(), receipt);
    }
}

async fn allowed_backlog(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let run = RunId::mint();
    let mut first = None;
    let backlog = 257;
    for _ in 0..backlog {
        let request = command(&app, &run, RunOperation::Pause);
        host.manage(&request).await.unwrap();
        if first.is_none() {
            first = Some(host.job(&request).await);
        }
    }
    let recovery = ordinary(&app);
    host.queue.submit(&recovery).await.unwrap();
    let authority = assignment(&app);
    let granted = host.queue.claim(&authority).await.unwrap().unwrap();
    assert_eq!(granted.delivery().job, first.unwrap());
    let granted = host.queue.claim(&authority).await.unwrap().unwrap();
    assert_eq!(granted.delivery().job, recovery);
    assert_eq!(host.holds.acquired.get(), 0);
    let mut pending = rows(&host.database, "management", value!({})).await;
    assert_eq!(pending.len(), backlog);
    assert_eq!(snapshot(&host).await["management"].len(), backlog);
    let last = pending.pop().unwrap();
    damaged(
        &host,
        &authority,
        "management",
        last["id"].as_str().unwrap(),
        value!({"blocks_execution":false}),
        value!({"blocks_execution":true}),
    )
    .await;
}
