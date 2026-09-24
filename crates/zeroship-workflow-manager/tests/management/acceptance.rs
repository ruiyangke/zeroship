use super::*;

case!(
    sqlite_latest_acceptance_freezes_the_named_deployment,
    postgres_latest_acceptance_freezes_the_named_deployment,
    frozen_selection
);
case!(
    sqlite_latest_acceptance_failure_leaves_no_command,
    postgres_latest_acceptance_failure_leaves_no_command,
    refused_acceptance
);
case!(
    sqlite_latest_restart_refuses_a_deployment_control_does_not_hold,
    postgres_latest_restart_refuses_a_deployment_control_does_not_hold,
    refused_wire_deployment
);
case!(
    sqlite_a_restart_names_a_deployment_exactly_when_it_is_latest,
    postgres_a_restart_names_a_deployment_exactly_when_it_is_latest,
    deployment_presence_follows_the_deploy_policy
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
    let request = latest(&app, &RunId::mint(), &first);
    let accepted = host.manage(&request).await.unwrap();
    let spec = host.job(&request).await;
    assert!(
        matches!(&spec.operation, JobOperation::Management { command:ManagementCommand::RestartLatest { deployment_id }, .. } if deployment_id == &first.deployment_id)
    );
    assert_eq!(host.holds.acquired.get(), 1);
    let before = snapshot(&host).await;
    assert_eq!(host.manage(&request).await.unwrap(), accepted);
    assert_eq!(snapshot(&host).await, before);
    assert_eq!(host.holds.acquired.get(), 1);
    // The accepted command is what the request named, so the same request id
    // naming a different deployment is a changed request rather than a second
    // chance to choose one.
    let moved = ManageRun {
        command: ManagementOperation::Restart {
            options: RestartOptions {
                from: None,
                deploy: Some(RestartDeploy::Latest),
            },
            deployment: Some(second.clone()),
        },
        ..request.clone()
    };
    assert_eq!(host.manage(&moved).await, Err(Error::Conflict));
    assert_eq!(snapshot(&host).await, before);
    // A well-formed restart of a different shape under the same request id is
    // the same refusal, so the conflict above is the digest and not the
    // deployment field failing validation on its own.
    let changed = ManageRun {
        command: ManagementOperation::Restart {
            options: RestartOptions {
                from: None,
                deploy: Some(RestartDeploy::Started),
            },
            deployment: None,
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
    let deployment = host.source.publish(&app, "retry").await;
    let request = latest(&app, &RunId::mint(), &deployment);
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

/// The deployment arrives on the wire, so the manager's only authority over it
/// is Control's own: the hold must acquire under Control's row lock, and the
/// hash must be the one Control minted with that hold.
async fn refused_wire_deployment(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let foreign = AppId::mint();
    let held = host.source.publish(&app, "this app's code").await;
    let elsewhere = host.source.publish(&foreign, "another app's code").await;
    // Never recorded: Control's catalog has no row to lock under this app.
    let absent = RestartDeployment {
        deployment_id: DeploymentId::mint(),
        deploy_hash: held.deploy_hash.clone(),
    };
    assert_eq!(
        host.manage(&latest(&app, &RunId::mint(), &absent)).await,
        Err(Error::Denied)
    );
    no_accepted_work(&host).await;
    // Recorded, but under another app.
    assert_eq!(
        host.manage(&latest(&app, &RunId::mint(), &elsewhere)).await,
        Err(Error::Denied)
    );
    no_accepted_work(&host).await;
    // This app's own deployment, after reclamation closed hold admission.
    let reclaiming = host.source.publish(&app, "reclaiming").await;
    host.source
        .patch(
            "app_deploys",
            reclaiming.deployment_id.as_str(),
            value!({"retention_state":"reclaiming"}),
        )
        .await;
    assert_eq!(
        host.manage(&latest(&app, &RunId::mint(), &reclaiming))
            .await,
        Err(Error::Conflict)
    );
    no_accepted_work(&host).await;
    // The held deployment under a hash Control never minted for it. The hold is
    // taken on the way through, so this is the hash check refusing and not the
    // acquisition.
    let forged = RestartDeployment {
        deployment_id: held.deployment_id.clone(),
        deploy_hash: elsewhere.deploy_hash.clone(),
    };
    assert_eq!(
        host.manage(&latest(&app, &RunId::mint(), &forged)).await,
        Err(Error::Conflict)
    );
    no_accepted_work(&host).await;
    // The pair Control does hold is accepted, so the refusals above are the
    // checks and not a fixture that refuses everything.
    let accepted = host
        .manage(&latest(&app, &RunId::mint(), &held))
        .await
        .unwrap();
    assert_eq!(accepted.outcome, None);
    assert_eq!(
        rows(&host.database, "management", value!({})).await.len(),
        1
    );
}

/// The deploy policy and the named deployment are two spellings of one
/// decision, so each direction of the mismatch is refused: a latest restart
/// with nothing to replay against, and a started restart asserting a deployment
/// no arm of acceptance reads.
async fn deployment_presence_follows_the_deploy_policy(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let deployment = host.source.publish(&app, "named on the wire").await;
    let restart = |deploy, target: Option<RestartDeployment>| ManageRun {
        command: ManagementOperation::Restart {
            options: RestartOptions { from: None, deploy },
            deployment: target,
        },
        ..command(&app, &RunId::mint(), RunOperation::Pause)
    };
    for refused in [
        // Latest, named and defaulted, with no deployment to replay against.
        restart(Some(RestartDeploy::Latest), None),
        restart(None, None),
        // Started, which replays on the code the run started on, carrying one.
        restart(Some(RestartDeploy::Started), Some(deployment.clone())),
        // A deployment whose hash is not a deploy hash.
        restart(
            Some(RestartDeploy::Latest),
            Some(RestartDeployment {
                deployment_id: deployment.deployment_id.clone(),
                deploy_hash: "not a deploy hash".into(),
            }),
        ),
    ] {
        assert_eq!(host.manage(&refused).await, Err(Error::Invalid));
        no_accepted_work(&host).await;
    }
    assert_eq!(host.holds.acquired.get(), 0);
    // Both matching pairs are accepted, so the refusals above are the
    // biconditional and not a blanket refusal of restarts.
    for accepted in [
        restart(Some(RestartDeploy::Started), None),
        restart(Some(RestartDeploy::Latest), Some(deployment.clone())),
        restart(None, Some(deployment.clone())),
    ] {
        assert_eq!(host.manage(&accepted).await.unwrap().outcome, None);
    }
    assert_eq!(
        rows(&host.database, "management", value!({})).await.len(),
        3
    );
}

async fn concurrent_acceptance(fixture: &Fixture) {
    let host = Host::new(fixture).await;
    let app = AppId::mint();
    let deployment = host.source.publish(&app, "contested").await;
    let request = latest(&app, &RunId::mint(), &deployment);
    let (entered, resume) = host.holds.gate();
    let waiting = host.manage(&request);
    let racing = async {
        entered.await.unwrap();
        let winner = host.manage(&request).await.unwrap();
        resume.send(()).unwrap();
        winner
    };
    let (waiting, winner) = futures::join!(waiting, racing);
    assert_eq!(waiting.unwrap(), winner);
    assert!(
        matches!(&host.job(&request).await.operation, JobOperation::Management { command:ManagementCommand::RestartLatest { deployment_id }, .. } if deployment_id == &deployment.deployment_id)
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
