use super::*;
use zeroship_workflow::{operations::RunOperation, service::WorkerIdentity};

/// Settle the fixture's run after it accepted a cascading child. Returns the
/// committed first propagation page and the child run.
pub(super) async fn cascade(fixture: &Fixture) -> (JobSpec, String) {
    let worker = WorkerIdentity::new("slot-propagation".into()).unwrap();
    let parent = fixture.service.poll(&worker).await.unwrap().unwrap();
    fixture
        .service
        .complete(
            &worker,
            &parent.id,
            &parent.token,
            WorkflowExecution::from_runtime_value(json!({"outcomes":[{
                "kind":"Child", "ordinal":0, "name":"child", "childWorkflowName":"Example",
                "options":{"cascade":true}, "input":{}
            }]}))
            .unwrap(),
        )
        .await
        .unwrap();
    let run = parent.invocation.run_id;
    fixture
        .app
        .transition(&RequestId::mint(), &run, RunOperation::Cancel)
        .await
        .unwrap();
    let pending = fixture.app.pending_jobs(None, 100).await.unwrap();
    let frontier = pending
        .iter()
        .filter_map(|job| match &job.operation {
            JobOperation::Advance {
                run_id, revision, ..
            } if run_id.as_str() == run => Some((revision.get(), job.clone())),
            _ => None,
        })
        .max_by_key(|(revision, _)| *revision)
        .unwrap()
        .1;
    let mut lease = fixture.lease.clone();
    lease.delivery.job = frontier;
    assert!(matches!(
        fixture.app.accept_job(&lease).await.unwrap(),
        JobAcceptance::Settled(_)
    ));
    let page = fixture
        .app
        .pending_jobs(None, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|job| matches!(job.operation, JobOperation::Propagate { .. }))
        .expect("settlement commits the first propagation page");
    let child = child_runs(fixture, &run).await.remove(0);
    (page, child)
}

async fn child_runs(fixture: &Fixture, parent: &str) -> Vec<String> {
    let tx = fixture.service.begin().await.unwrap();
    let Output::Rows { rows, .. } = tx
        .database()
        .collection("__zeroship_workflow_runs")
        .unwrap()
        .find(
            value!({"app_id":fixture.app.app_id().as_str(), "parent_id":parent}),
            value!({}),
        )
        .await
        .unwrap()
    else {
        panic!("run rows")
    };
    tx.commit().await.unwrap();
    rows.iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

pub(super) async fn control(fixture: &Fixture, run: &str) -> String {
    let tx = fixture.service.begin().await.unwrap();
    let Output::Rows { rows, .. } = tx
        .database()
        .collection("__zeroship_workflow_runs")
        .unwrap()
        .find(
            value!({"app_id":fixture.app.app_id().as_str(), "id":run}),
            value!({}),
        )
        .await
        .unwrap()
    else {
        panic!("run rows")
    };
    tx.commit().await.unwrap();
    rows[0]["control"].as_str().unwrap().to_owned()
}

#[compio::test]
async fn propagation_lost_ack_replays_without_executor_or_storage() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let (page, child) = cascade(&fixture).await;
    assert_eq!(control(&fixture, &child).await, "none");
    let mut lease = fixture.lease.clone();
    lease.delivery.job = page;
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        Box::pin(slot.run(&fixture.app, lease.clone()))
            .await
            .unwrap()
    else {
        panic!("a propagation page settles without execution")
    };
    assert_eq!(creator.outcome, JobOutcome::Completed {});
    assert_eq!(manager.outcome, creator.outcome);
    assert_eq!(control(&fixture, &child).await, "cancel");
    assert_eq!(fixture.metadata.requests.borrow().len(), 2);
    assert_eq!(
        fixture.metadata.requests.borrow()[0],
        fixture.metadata.requests.borrow()[1]
    );
    let pending = fixture.app.pending_jobs(None, 100).await.unwrap();
    lease.delivery.attempt = 2.try_into().unwrap();
    lease.expires = Instant::now();
    let DeliveryOutcome::Settled {
        creator: replay, ..
    } = Box::pin(slot.run(&fixture.app, lease)).await.unwrap()
    else {
        panic!("committed propagation replay")
    };
    assert_eq!(creator, replay);
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(fixture.metadata.renewals.get(), 0);
    assert_eq!(fixture.app.pending_jobs(None, 100).await.unwrap(), pending);
    assert!(fixture
        .metadata
        .requests
        .borrow()
        .iter()
        .all(|request| request.successors.is_empty()));
}

#[compio::test]
async fn propagation_refuses_invalid_page_bounds_before_delivery() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    for page_size in [0, u32::MAX] {
        let mut options = fixture.slot(Duration::from_secs(5)).options;
        options.propagation = zeroship_workflow::service::propagation::PropagationOptions { page_size };
        assert!(matches!(
            DeliverySlot::new(
                fixture.metadata.clone(),
                Rc::new(Executor {
                    probe: fixture.probe.clone(),
                    service: fixture.service.clone(),
                }),
                fixture.objects.clone(),
                options,
            ),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
}
