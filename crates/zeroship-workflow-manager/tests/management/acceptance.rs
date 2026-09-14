use super::*;

case!(
    sqlite_latest_acceptance_freezes_selection_and_replays_without_source,
    postgres_latest_acceptance_freezes_selection_and_replays_without_source,
    frozen_selection
);
case!(
    sqlite_latest_acceptance_failure_leaves_no_command,
    postgres_latest_acceptance_failure_leaves_no_command,
    refused_acceptance
);
case!(
    sqlite_concurrent_latest_acceptance_rechecks_raw_receipt,
    postgres_concurrent_latest_acceptance_rechecks_raw_receipt,
    concurrent_acceptance
);

async fn frozen_selection(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let first = host.source.publish(&app, "selected").await;
    let second = host.source.publish(&app, "subsequent").await;
    host.source.select(&app, Some(&first.deploy_hash)).await;
    let request = latest(&app, &RunId::mint());
    let (entered, resume) = host.holds.gate();
    let accepting = host.manage(&request);
    let rotate = async {
        entered.await.unwrap();
        host.source.select(&app, Some(&second.deploy_hash)).await;
        resume.send(()).unwrap();
    };
    let (accepted, ()) = futures::join!(accepting, rotate);
    let accepted = accepted.unwrap();
    let spec = host.job(&request).await;
    assert!(
        matches!(&spec.operation, JobOperation::Management { command:ManagementCommand::RestartLatest { deployment_id }, .. } if deployment_id == &first.deployment_id)
    );
    assert_eq!(host.holds.acquired.get(), 1);
    host.source.select(&app, None).await;
    let before = snapshot(&host).await;
    assert_eq!(host.manage(&request).await.unwrap(), accepted);
    assert_eq!(snapshot(&host).await, before);
    assert_eq!(host.holds.acquired.get(), 1);
    let changed = ManageRun {
        command: ManagementOperation::Restart {
            options: RestartOptions {
                from: None,
                deploy: None,
            },
        },
        ..request.clone()
    };
    assert_eq!(host.manage(&changed).await, Err(Error::Conflict));
    let authority = assignment(&app);
    let settlement = settle(&host, &authority, &spec, ManagementOutcome::NotFound {}).await;
    host.queue
        .release_deployment(&app, &first.deployment_id)
        .await
        .unwrap();
    let closed = host.manage(&request).await.unwrap();
    assert_eq!(closed.outcome, Some(ManagementOutcome::NotFound {}));
    let before = snapshot(&host).await;
    host.queue.settle(&authority, &settlement).await.unwrap();
    assert_eq!(host.manage(&request).await.unwrap(), closed);
    assert_eq!(snapshot(&host).await, before);
}

async fn no_accepted_work(host: &Host) {
    for table in ["management", "jobs", "management_scopes"] {
        assert!(rows(&host.database, table, value!({})).await.is_empty());
    }
    for row in rows(&host.database, "queue_scopes", value!({})).await {
        assert_eq!(row["dispatch_cursor"], value!(0));
    }
}

async fn refused_acceptance(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let request = latest(&app, &RunId::mint());
    assert_eq!(host.manage(&request).await, Err(Error::Unavailable));
    no_accepted_work(&host).await;
    assert_eq!(host.holds.acquired.get(), 0);
    let deployment = host.source.publish(&app, "retry").await;
    host.source
        .select(&app, Some(&deployment.deploy_hash))
        .await;
    host.holds.fail.set(true);
    assert_eq!(host.manage(&request).await, Err(Error::Unavailable));
    no_accepted_work(&host).await;
    host.holds.fail.set(false);
    host.holds.wrong_hash.set(true);
    assert_eq!(host.manage(&request).await, Err(Error::Conflict));
    no_accepted_work(&host).await;
    host.holds.wrong_hash.set(false);
    let intent = single(
        &host.database,
        "deployment_holds",
        value!({"deployment_id":deployment.deployment_id.as_str()}),
    )
    .await;
    patch(
        &host.database,
        "deployment_holds",
        intent["id"].as_str().unwrap(),
        value!({"deploy_hash":deployment.deploy_hash}),
    )
    .await;
    let accepted = host.manage(&request).await.unwrap();
    assert_eq!(accepted.outcome, None);
    let scope = single(
        &host.database,
        "management_scopes",
        value!({"app_id":app.as_str()}),
    )
    .await;
    assert_eq!(scope["accepted_revision"], value!(1));
    assert_eq!(scope["settled_revision"], value!(0));
}

async fn concurrent_acceptance(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let first = host.source.publish(&app, "first observer").await;
    let second = host.source.publish(&app, "competing observer").await;
    host.source.select(&app, Some(&first.deploy_hash)).await;
    let request = latest(&app, &RunId::mint());
    let (entered, resume) = host.holds.gate();
    let waiting = host.manage(&request);
    let racing = async {
        entered.await.unwrap();
        host.source.select(&app, Some(&second.deploy_hash)).await;
        let winner = host.manage(&request).await.unwrap();
        host.source.select(&app, None).await;
        resume.send(()).unwrap();
        winner
    };
    let (waiting, winner) = futures::join!(waiting, racing);
    assert_eq!(waiting.unwrap(), winner);
    assert!(
        matches!(&host.job(&request).await.operation, JobOperation::Management { command:ManagementCommand::RestartLatest { deployment_id }, .. } if deployment_id == &second.deployment_id)
    );
    assert_eq!(
        rows(&host.database, "management", value!({})).await.len(),
        1
    );
    assert_eq!(rows(&host.database, "jobs", value!({})).await.len(), 1);
    let order = single(&host.database, "management_scopes", value!({})).await;
    assert_eq!(order["accepted_revision"], value!(1));
    assert_eq!(host.holds.acquired.get(), 2);
}
