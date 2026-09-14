use super::*;
use crate::service::store::Transaction;
use zeroship_data_orm::{
    orm::{Operation, Output},
    value,
};

paired!(
    sqlite_consumed_checkpoints_pin_their_continuation_members,
    postgres_consumed_checkpoints_pin_their_continuation_members,
    pinned_members
);

async fn purge(
    tx: &Transaction,
    table: &str,
    app: &AppId,
    id: &str,
) -> Result<Output, WorkflowServiceError> {
    Ok(tx
        .database()
        .collection(&format!("__zeroship_workflow_{table}"))?
        .execute(Operation::Purge {
            filter: value!({"app_id":app.as_str(), "id":id}),
            many: false,
        })
        .await?)
}

/// Accepted and result checkpoint references keep their members, and each
/// member keeps its generation, until the reference itself is removed.
async fn pinned_members(store: Rc<OrmStore>) {
    let (service, app_id, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app_id.clone());
    let worker = WorkerIdentity::new("continuation-references".into()).unwrap();
    let owner = parent(&service, &scope, &worker, None).await;
    let (child, accepted) = accepted(&scope, &owner).await;
    let successor = continue_run(&service, &scope, &worker, &child).await;
    finish_run(&service, &worker, &successor, "done").await;
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, owner);
    assert_eq!(task.invocation.journal[0].output, Some(json!("done")));
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    let steps = journal_rows(
        &tx,
        "steps",
        json!({"app_id":app_id.as_str(), "run_id":owner.as_str()}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].text("child_member_id").unwrap(), accepted);
    let result = steps[0].text("child_result_member_id").unwrap();
    assert_ne!(result, accepted);

    let before = all_state(&service, &app_id).await;
    for (table, id) in [
        ("continuation_members", accepted.as_str()),
        ("continuation_members", result.as_str()),
        ("generations", accepted.as_str()),
        ("generations", result.as_str()),
    ] {
        // A refused statement aborts its transaction, so each attempt owns one.
        let tx = service.begin().await.unwrap();
        assert!(
            purge(&tx, table, &app_id, id).await.is_err(),
            "{table} {id} must stay pinned"
        );
        drop(tx);
    }
    assert_eq!(all_state(&service, &app_id).await, before);

    // Control: once the checkpoint itself is gone, both members are
    // removable. The transaction is discarded to keep the journal intact.
    let tx = service.begin().await.unwrap();
    purge(&tx, "steps", &app_id, &steps[0].text("id").unwrap())
        .await
        .unwrap();
    for id in [accepted.as_str(), result.as_str()] {
        purge(&tx, "continuation_members", &app_id, id)
            .await
            .unwrap();
        let remaining = journal_rows(
            &tx,
            "continuation_members",
            json!({"app_id":app_id.as_str(), "id":id}),
        )
        .await;
        assert!(remaining.is_empty(), "{id} must be removable once unpinned");
    }
    drop(tx);
    assert_eq!(all_state(&service, &app_id).await, before);
}
