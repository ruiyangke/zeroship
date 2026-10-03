//! A delivery's outcome, release and receipt answer to the worker holding it.
//!
//! Every body here is built as JSON rather than through the client's envelopes,
//! because what is under test is what the ROUTE does with a body a worker chose.

use super::*;
use std::cell::Cell;
use zeroship_core::workflow_jobs::BroadcastId;

fn released(delivery: &Delivery, task: &ClaimedTask) -> Value {
    json!({"delivery": delivery, "task": task})
}

/// What a worker that wants to decide its own job sends: an outcome, a successor,
/// or both, beside a delivery it really holds.
fn forged(delivery: &Delivery, successor: &JobSpec) -> [Value; 4] {
    [
        json!({"delivery": delivery, "outcome": {"kind":"completed"}, "successors": [successor]}),
        json!({"delivery": delivery, "outcome": {"kind":"completed"}}),
        json!({"delivery": delivery, "successors": [successor]}),
        json!({"delivery": delivery, "successors": []}),
    ]
}

/// A worker holding a delivery cannot name its outcome or publish a successor
/// through the settlement. A body that reports no execution is settled only from
/// a receipt the journal holds, and with the outcome that receipt records.
///
/// Two jobs, one per answer the journal can give. The first is an advance whose
/// task the journal handed out and nothing has committed, so it holds no receipt.
/// The second is an advance on a run the journal has never seen, which its
/// acceptance already decided -- rejected -- so the worker naming `completed` is
/// contradicted by the journal rather than merely unsupported by it.
#[ntex::test]
async fn a_settlement_names_no_outcome_and_publishes_no_successor() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let (delivery, _task) = fixture.held(&job).await;
    let successor = fixture.job();
    let before = fixture.job_snapshot(&job).await;
    assert_eq!(before.len(), 1);
    for body in forged(&delivery, &successor) {
        let answer = fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &body).await;
        assert!(
            fixture.job_snapshot(&successor).await.is_empty(),
            "a settlement published a successor the worker named: {answer:?}"
        );
        assert_eq!(
            fixture.job_snapshot(&job).await,
            before,
            "a settlement recorded an outcome the worker named: {answer:?}"
        );
        assert_eq!(
            answer,
            (StatusCode::BAD_REQUEST, json!({"code":"invalid"})),
            "{body}"
        );
    }
    // THE CONTROL: the same delivery, in the one shape that carries no execution,
    // is parsed and reaches the journal, which holds no receipt to settle it with.
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
            .await,
        (StatusCode::CONFLICT, json!({"code":"conflict"})),
    );
    assert_eq!(fixture.job_snapshot(&job).await, before);
    assert!(fixture.job_snapshot(&successor).await.is_empty());

    let decided = fixture.job();
    fixture.submit(&decided).await;
    let (delivery, accepted) = fixture.claimed(&decided).await;
    let Some(AcceptedJob::Settled { receipt }) = accepted else {
        panic!("the journal decides an advance for a run it does not hold at acceptance")
    };
    assert_eq!(receipt.outcome, JobOutcome::Rejected {});
    let before = fixture.job_snapshot(&decided).await;
    for body in forged(&delivery, &successor) {
        assert_eq!(
            fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &body).await,
            (StatusCode::BAD_REQUEST, json!({"code":"invalid"})),
            "{body}"
        );
        assert_eq!(fixture.job_snapshot(&decided).await, before);
    }
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], json!({"kind":"rejected"}));
    assert_eq!(
        fixture.job_column(&decided, "outcome").await.as_deref(),
        Some(r#"{"kind":"rejected"}"#)
    );
    assert!(fixture.job_snapshot(&successor).await.is_empty());
}

/// A holder whose execution committed but whose settlement did not is settled,
/// on its next attempt, with the outcome the journal committed.
///
/// The gap is produced rather than seeded: the queue loses its UPDATE privilege
/// on `jobs` for the length of one settlement, so the journal half commits and
/// the queue half fails, which is the failure between two stores with no shared
/// transaction that this arm exists to recover.
#[ntex::test]
async fn a_committed_execution_is_settled_with_the_journal_outcome() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let (delivery, task) = fixture.held(&job).await;
    fixture
        .platform
        .admin
        .batch_execute("REVOKE UPDATE ON workflow_manager.jobs FROM zeroship_workflow")
        .await
        .unwrap();
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_SETTLE, &executed(&delivery, &task))
        .await;
    fixture
        .platform
        .admin
        .batch_execute("GRANT UPDATE ON workflow_manager.jobs TO zeroship_workflow")
        .await
        .unwrap();
    assert_ne!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fixture.task_state(&task.id).await,
        vec!["completed".to_owned()],
        "the journal half committed"
    );
    assert_eq!(
        fixture.job_column(&job, "state").await.as_deref(),
        Some("leased"),
        "the queue half did not"
    );

    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let receipt: SettlementReceipt = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(receipt.job_id, job.id);
    assert_eq!(receipt.attempt, delivery.attempt);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert_eq!(
        fixture.job_column(&job, "state").await.as_deref(),
        Some("settled")
    );
    assert_eq!(
        fixture.job_column(&job, "outcome").await.as_deref(),
        Some(r#"{"kind":"completed"}"#)
    );
    // An exact retry replays the stored receipt.
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
            .await,
        (StatusCode::OK, body)
    );
}

/// A release gives back the task of the worker whose credential signed it, and
/// no other worker's, even one presenting that task's own valid token.
#[ntex::test]
async fn a_release_answers_to_the_worker_that_signed_it() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let (delivery, task) = fixture.held(&job).await;
    let other = fixture.other_worker().await;
    let body = released(&delivery, &task);
    assert_eq!(
        fixture
            .post_as(&other, endpoints::WORKFLOW_JOB_RELEASE, &body)
            .await,
        (StatusCode::FORBIDDEN, json!({"code":"denied"})),
    );
    assert_eq!(
        fixture.task_state(&task.id).await,
        vec!["leased".to_owned()]
    );
    let (status, reply) = fixture.post(endpoints::WORKFLOW_JOB_RELEASE, &body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(
        fixture.task_state(&task.id).await,
        vec!["released".to_owned()]
    );
}

/// The execution half of a settlement commits for the worker whose credential
/// signed it, and no other worker's, even one presenting the task's own token.
#[ntex::test]
async fn an_execution_settles_for_the_worker_that_signed_it() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let (delivery, task) = fixture.held(&job).await;
    let other = fixture.other_worker().await;
    let body = executed(&delivery, &task);
    let before = fixture.job_snapshot(&job).await;
    assert_eq!(
        fixture
            .post_as(&other, endpoints::WORKFLOW_JOB_SETTLE, &body)
            .await,
        (StatusCode::FORBIDDEN, json!({"code":"denied"})),
    );
    assert_eq!(
        fixture.task_state(&task.id).await,
        vec!["leased".to_owned()]
    );
    assert_eq!(fixture.job_snapshot(&job).await, before);
    let (status, reply) = fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["outcome"], json!({"kind":"completed"}));
    assert_eq!(
        fixture.task_state(&task.id).await,
        vec!["completed".to_owned()]
    );
}

/// A job's receipt is read by the worker the queue last delivered it to, before
/// and after it commits, and by no other worker.
#[ntex::test]
async fn a_receipt_is_read_by_the_job_holder_alone() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let (delivery, task) = fixture.held(&job).await;
    let other = fixture.other_worker().await;
    let query = json!({"job": job});
    let refused = (StatusCode::CONFLICT, json!({"code":"conflict"}));
    assert_eq!(
        fixture
            .post_as(&other, endpoints::WORKFLOW_JOB_RECEIPT, &query)
            .await,
        refused
    );
    assert_eq!(
        fixture.post(endpoints::WORKFLOW_JOB_RECEIPT, &query).await,
        (StatusCode::OK, Value::Null),
        "the holder is answered that nothing has committed"
    );
    let (status, reply) = fixture
        .post(endpoints::WORKFLOW_JOB_SETTLE, &executed(&delivery, &task))
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(
        fixture.post(endpoints::WORKFLOW_JOB_RECEIPT, &query).await,
        (
            StatusCode::OK,
            json!({"job": job, "outcome": {"kind":"completed"}})
        ),
        "the holder reads the committed receipt"
    );
    assert_eq!(
        fixture
            .post_as(&other, endpoints::WORKFLOW_JOB_RECEIPT, &query)
            .await,
        refused
    );
    // A job the queue never delivered to anyone has no holder to answer.
    assert_eq!(
        fixture
            .post(
                endpoints::WORKFLOW_JOB_RECEIPT,
                &json!({"job": fixture.job()})
            )
            .await,
        refused
    );
}

/// An unplaced worker's claim for an app it holds no placement on performs no
/// policy I/O: Control is never asked about the app, so the refusal cannot say
/// whether that app exists.
#[ntex::test]
async fn an_unplaced_claim_never_asks_control_about_the_app() {
    let fixture = Fixture::new().await;
    let foreign = AppId::mint();
    let other = fixture.other_worker().await;
    let scope = AssignedScope {
        app_id: foreign,
        assignment_revision: 1.try_into().unwrap(),
    };
    let before = fixture.server.control_facts_requests();
    assert_eq!(
        fixture
            .post_as(&other, endpoints::WORKFLOW_JOB_CLAIM, &scope)
            .await
            .0,
        StatusCode::FORBIDDEN,
        "placement is refused before the app's delivery ceiling is read"
    );
    assert_eq!(
        fixture.server.control_facts_requests(),
        before,
        "the claim observed a foreign app"
    );
}

/// A worker whose placement on the app was released cannot keep reading the
/// job's receipt, even though the queue still names it as the last holder.
#[ntex::test]
async fn a_released_placement_cannot_read_a_receipt() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let _ = fixture.held(&job).await;
    let query = json!({"job": job});
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_RECEIPT, &query)
            .await,
        (StatusCode::OK, Value::Null),
        "the live holder is answered that nothing has committed"
    );
    fixture
        .platform
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET released=true WHERE app_id=$1 AND worker_id=$2",
            &[&job.app_id.as_str(), &fixture.worker.id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_RECEIPT, &query)
            .await
            .0,
        StatusCode::FORBIDDEN,
        "a released placement must not keep reading outcomes"
    );
}

/// A delivery re-claimed by another worker after the first holder's lease lapsed:
/// the earlier holder may neither settle, release, nor read the job's receipt.
#[ntex::test]
async fn a_lease_handover_refuses_the_earlier_holder() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let (delivery, task) = fixture.held(&job).await;
    let replacement = fixture.other_worker().await;
    let reassignment = fixture
        .platform
        .seed_placement(&job.app_id, &replacement.id, Duration::from_secs(30))
        .await;
    fixture
        .platform
        .admin
        .execute(
            "UPDATE workflow_manager.jobs SET lease_deadline=0 WHERE app_id=$1 AND id=$2",
            &[&job.app_id.as_str(), &job.id.as_str()],
        )
        .await
        .unwrap();
    let (status, body) = fixture
        .post_as(
            &replacement,
            endpoints::WORKFLOW_JOB_CLAIM,
            &AssignedScope {
                app_id: job.app_id.clone(),
                assignment_revision: reassignment.revision,
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The earlier holder holds neither the latest delivery nor its task.
    let refused = (StatusCode::CONFLICT, json!({"code":"conflict"}));
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_RECEIPT, &json!({"job": job}))
            .await,
        refused
    );
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
            .await,
        refused
    );
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &executed(&delivery, &task))
            .await,
        refused
    );
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_RELEASE, &released(&delivery, &task))
            .await,
        refused
    );
}

/// A caller whose delivery is not the queue's latest for a job never reaches the
/// journal for it: not through a receipt read, and not through a settlement with
/// no execution -- whichever app it names and whatever kind of job.
///
/// Both tables a receipt read can touch are held under an exclusive lock for the
/// whole case: the receipt table every kind reads, and the app state row the
/// fanout, propagation, management and maintenance readers lock before they
/// read. The holder's own read is the control. It waits on the lock, which is
/// what shows the lock is on the journal path at all, and it is still waiting
/// when every refusal below has been answered.
#[ntex::test]
async fn a_caller_without_the_latest_delivery_never_reaches_the_journal() {
    let fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    let (delivery, task) = fixture.held(&job).await;
    let other = fixture.other_worker().await;
    let query = json!({"job": job});
    let fanout = JobSpec {
        operation: JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: 1.try_into().unwrap(),
        },
        ..fixture.job()
    };
    // An attempt the queue never handed out: the holder's own identity, so the
    // identity check passes and only the queue fence can refuse it.
    let superseded = Delivery {
        attempt: 2.try_into().unwrap(),
        ..delivery.clone()
    };
    // Task calls carry no delivery, so they name an app the worker holds no
    // queue row in and a valid task credential; the app fence must refuse them
    // before the policy source observes that app.
    let foreign = AppId::mint();
    let reference = zeroship_core::workflow_coordination::WorkflowOutputRef {
        hash: "0".repeat(64),
        size: 0,
        content_type: None,
    };
    let task_payload = serde_json::to_value(
        zeroship_core::workflow_coordination::ReadTaskPayload {
            app_id: foreign.clone(),
            task_id: task.id.clone(),
            token: task.token.as_str().to_owned(),
            reference: reference.clone(),
        },
    )
    .unwrap();
    let task_executable = serde_json::to_value(
        zeroship_core::workflow_coordination::ResolveTaskExecutable {
            app_id: foreign.clone(),
            task_id: task.id.clone(),
            token: task.token.as_str().to_owned(),
        },
    )
    .unwrap();
    let payload_reserve = serde_json::to_value(
        zeroship_core::workflow_coordination::ReservePayload {
            app_id: foreign.clone(),
            task_id: task.id.clone(),
            token: task.token.as_str().to_owned(),
            request_id: RequestId::mint(),
            reference,
        },
    )
    .unwrap();
    // Each probe names its own identity where a body names one at all, so what
    // refuses it is the queue fence rather than the identity check before it.
    let probes = [
        // Another worker asking for the holder's receipt.
        (&other, endpoints::WORKFLOW_JOB_RECEIPT, query.clone()),
        // Another worker naming itself on the holder's job.
        (
            &other,
            endpoints::WORKFLOW_JOB_SETTLE,
            committed(&Delivery {
                worker_id: other.id.clone(),
                ..delivery.clone()
            }),
        ),
        // The holder itself, naming an attempt the queue never handed out, on
        // the settlement's execution arm and on release.
        (
            &fixture.worker,
            endpoints::WORKFLOW_JOB_SETTLE,
            committed(&superseded),
        ),
        (
            &fixture.worker,
            endpoints::WORKFLOW_JOB_SETTLE,
            executed(&superseded, &task),
        ),
        (
            &fixture.worker,
            endpoints::WORKFLOW_JOB_RELEASE,
            released(&superseded, &task),
        ),
        // The holder, naming its own job's id under a kind whose receipt reader
        // takes the app lock.
        (
            &fixture.worker,
            endpoints::WORKFLOW_JOB_RECEIPT,
            json!({"job": JobSpec { id: job.id.clone(), ..fanout.clone() }}),
        ),
        // A kind whose receipt reader takes the app lock, on a job the queue
        // never delivered, through both routes.
        (
            &other,
            endpoints::WORKFLOW_JOB_RECEIPT,
            json!({"job": fanout}),
        ),
        (
            &other,
            endpoints::WORKFLOW_JOB_SETTLE,
            committed(&Delivery {
                job: fanout.clone(),
                worker_id: other.id.clone(),
                ..delivery.clone()
            }),
        ),
    ];
    assert!(!probes.is_empty());
    // The task routes carry no delivery; they are fenced on the worker's live
    // placement, so a foreign app is refused `Denied` before the policy source
    // is asked to observe it.
    let placement_probes = [
        (
            &fixture.worker,
            endpoints::WORKFLOW_TASK_PAYLOAD,
            task_payload,
        ),
        (
            &fixture.worker,
            endpoints::WORKFLOW_TASK_EXECUTABLE,
            task_executable,
        ),
        (
            &fixture.worker,
            endpoints::WORKFLOW_TASK_PAYLOAD_RESERVE,
            payload_reserve,
        ),
    ];
    assert!(!placement_probes.is_empty());

    let admin_url = fixture
        .platform
        .runtime_url
        .replacen("zeroship_workflow@", "postgres@", 1);
    let mut blocker = platform::connect(&admin_url).await;
    let pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let lock = blocker.transaction().await.unwrap();
    lock.batch_execute(
        "LOCK TABLE workflow_manager.__zeroship_workflow_job_receipts, \
         workflow_manager.__zeroship_workflow_app_state IN ACCESS EXCLUSIVE MODE",
    )
    .await
    .unwrap();

    let answered = Cell::new(false);
    let holder = async {
        let answer = fixture.post(endpoints::WORKFLOW_JOB_RECEIPT, &query).await;
        answered.set(true);
        answer
    };
    let refusals = async {
        compio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = fixture
                    .platform
                    .admin
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                         WHERE usename='zeroship_workflow' AND $1=ANY(pg_blocking_pids(pid)) \
                         AND query ILIKE '%job_receipts%')",
                        &[&pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if waiting {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the holder's receipt read must reach the locked journal table");
        let refused = (StatusCode::CONFLICT, json!({"code":"conflict"}));
        for (worker, endpoint, body) in &probes {
            let answer = compio::time::timeout(
                Duration::from_secs(3),
                fixture.post_as(worker, *endpoint, body),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "a caller without the delivery waited on the journal: {} {body}",
                    endpoint.path_template()
                )
            });
            assert_eq!(answer, refused, "{} {body}", endpoint.path_template());
        }
        let denied = (StatusCode::FORBIDDEN, json!({"code":"denied"}));
        for (worker, endpoint, body) in &placement_probes {
            let answer = compio::time::timeout(
                Duration::from_secs(3),
                fixture.post_as(worker, *endpoint, body),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "a task call with no placement waited on the journal: {} {body}",
                    endpoint.path_template()
                )
            });
            assert_eq!(answer, denied, "{} {body}", endpoint.path_template());
        }
        assert!(
            !answered.get(),
            "the holder's read waits on the locked journal while the refusals are answered"
        );
        lock.commit().await.unwrap();
    };
    let (answer, ()) = futures::join!(Box::pin(holder), Box::pin(refusals));
    assert_eq!(answer, (StatusCode::OK, Value::Null));
}
