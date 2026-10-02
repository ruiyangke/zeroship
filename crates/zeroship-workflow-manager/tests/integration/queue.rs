#![allow(
    clippy::future_not_send,
    reason = "native fixtures stay on their compio runtime"
)]

use crate::support;

use std::{cell::Cell, future::ready, time::Duration};
use crate::support::{Admin, Backend, Fixture};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, RunId, VerifyAssignment, WorkerId},
    workflow_jobs::{
        Delivery, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec, JournalSettlement,
        SettlementReceipt,
    },
};
use zeroship_data_orm::{
    orm::{Database, Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{Claimant, Error, Options, Queue};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let fixture = Fixture::new(Backend::Sqlite).await;
            Box::pin($contract(&fixture)).await;
        }

        #[compio::test]
        async fn $postgres() {
            let fixture = Fixture::new(Backend::Postgres).await;
            Box::pin($contract(&fixture)).await;
        }
    };
}

case!(
    sqlite_submission_and_competing_claims,
    postgres_submission_and_competing_claims,
    submission_and_competing_claims
);
case!(
    sqlite_authorized_submission_scope_replay_and_rollback,
    postgres_authorized_submission_scope_replay_and_rollback,
    authorized_submission_scope_replay_and_rollback
);
case!(
    sqlite_authorized_submission_bounds_pending_authorization,
    postgres_authorized_submission_bounds_pending_authorization,
    authorized_submission_bounds_pending_authorization
);
case!(
    sqlite_concurrent_scope_registration_preserves_identity_and_queue,
    postgres_concurrent_scope_registration_preserves_identity_and_queue,
    concurrent_scope_registration_preserves_identity_and_queue
);
case!(
    sqlite_delayed_jobs_and_redelivery,
    postgres_delayed_jobs_and_redelivery,
    delayed_jobs_and_redelivery
);
case!(
    sqlite_atomic_receipt_and_replayed_receipts,
    postgres_atomic_receipt_and_replayed_receipts,
    atomic_receipt_and_replayed_receipts
);
case!(
    sqlite_revocation_rolls_back_mutations,
    postgres_revocation_rolls_back_mutations,
    revocation_rolls_back_mutations
);
case!(
    sqlite_bounds_and_privileges,
    postgres_bounds_and_privileges,
    bounds_and_privileges
);
case!(
    sqlite_refused_kind_at_the_head_does_not_hide_the_rows_behind_it,
    postgres_refused_kind_at_the_head_does_not_hide_the_rows_behind_it,
    refused_kind_at_the_head
);
case!(
    sqlite_journal_sweep_is_left_to_the_claimant_owning_the_journal,
    postgres_journal_sweep_is_left_to_the_claimant_owning_the_journal,
    journal_sweep_is_left_to_the_lane
);

async fn queue(fixture: &Fixture, options: Options) -> Queue {
    Queue::connect(
        fixture.binding(),
        fixture.url(),
        options,
        support::synthetic_holds(),
    )
    .await
    .unwrap()
}

async fn now(fixture: &Fixture) -> i64 {
    match &fixture.admin {
        Admin::Postgres(admin) => admin
            .query(
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)",
                &[],
            )
            .await
            .unwrap()[0]
            .get(0),
        Admin::Sqlite(admin) => admin
            .query_row(
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)",
                [],
                |row| row.get(0),
            )
            .unwrap(),
    }
}

async fn until(fixture: &Fixture, deadline: i64) {
    compio::time::timeout(Duration::from_secs(5), async {
        while now(fixture).await <= deadline {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("database clock reached the requested deadline");
}

async fn assignment(fixture: &Fixture, app: &AppId) -> Assignment {
    Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: (now(fixture).await + 120_000).try_into().unwrap(),
    }
}

fn identity(assignment: &Assignment) -> VerifyAssignment {
    VerifyAssignment {
        app_id: assignment.app_id.clone(),
        worker_id: assignment.worker_id.clone(),
        assignment_revision: assignment.revision,
    }
}

fn job(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    }
}

/// A row whose sweep moves payload objects: `collect_job` deletes retired ones,
/// so running it reaches the object store as well as the journal.
fn collection(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Collect {},
        available_at: 0.try_into().unwrap(),
    }
}

/// A row the maintenance dispatch settles, carrying no creator code.
fn sweep(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Reconcile {},
        available_at: 0.try_into().unwrap(),
    }
}

/// An app whose queue holds a creator row at the head of the dispatch order and
/// a maintenance row behind it. Returns them in that order.
async fn head_then_sweep(queue: &Queue, fixture: &Fixture, app: &AppId) -> (JobSpec, JobSpec) {
    queue.register_scope(app).await.unwrap();
    let head = job(app);
    let behind = sweep(app);
    queue.submit(&head).await.unwrap();
    queue.submit(&behind).await.unwrap();
    assert!(
        dispatch_order(fixture, &head.id).await < dispatch_order(fixture, &behind.id).await,
        "the creator row must sit at the head for this to measure anything"
    );
    (head, behind)
}

async fn dispatch_order(fixture: &Fixture, id: &JobId) -> i64 {
    stored(fixture, id).await.expect("the job was submitted")["dispatch_order"]
        .as_i64()
        .expect("dispatch order is an integer")
}

fn settlement(delivery: &Delivery) -> JournalSettlement {
    support::settlement(delivery, JobOutcome::Completed {})
}

async fn stored(fixture: &Fixture, id: &JobId) -> Option<Value> {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .find(value!({"id":id.as_str()}), value!({"limit":1}))
        .await
        .unwrap()
    else {
        panic!("job query returned a count");
    };
    rows.into_iter().next()
}

/// Wait until a manager session queues behind a lock the administrator holds.
/// The predicate names the live waiter and its blocker rather than the
/// statement text, because the server serves the activity snapshot it cached
/// at first access for the rest of the administrator's transaction.
async fn blocked_manager(admin: &compio_postgres::Client, predicate: &str) {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM pg_locks l \
         JOIN pg_stat_activity a ON a.pid=l.pid \
         WHERE a.usename='workflow_manager_test' AND NOT l.granted AND ({predicate}))"
    );
    compio::time::timeout(Duration::from_secs(3), async {
        while !admin.query(&sql, &[]).await.unwrap()[0].get::<_, bool>(0) {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("manager operation must reach the database lock");
}

#[expect(
    clippy::too_many_lines,
    reason = "submission, rollback and receipt replay share the same job identity"
)]
async fn authorized_submission_scope_replay_and_rollback(fixture: &Fixture) {
    let queue = queue(fixture, Options::default()).await;
    let app = AppId::mint();
    let foreign = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    queue.register_scope(&foreign).await.unwrap();
    let authority = assignment(fixture, &app).await;
    let foreign_spec = job(&foreign);
    let checks = Cell::new(0);
    assert_eq!(
        queue
            .submit_authorized(&identity(&authority), &foreign_spec, |_| {
                checks.set(checks.get() + 1);
                ready(Ok(authority.clone()))
            })
            .await,
        Err(Error::Denied)
    );
    assert_eq!(checks.get(), 0);
    assert!(stored(fixture, &foreign_spec.id).await.is_none());

    let spec = job(&app);
    checks.set(0);
    assert_eq!(
        queue
            .submit_authorized(&identity(&authority), &spec, |tx| {
                checks.set(checks.get() + 1);
                submission_authorization(tx, &authority, &spec, checks.get(), 1)
            })
            .await,
        Err(Error::Denied)
    );
    assert_eq!(checks.get(), 1);
    assert!(queue
        .deployment_intents(&app, None, 1)
        .await
        .unwrap()
        .is_empty());
    assert_authorization_rolled_back(fixture, &app).await;

    // Prepare retention independently so the following callbacks observe the
    // transaction that inserts the job and its final authorization check.
    queue
        .ensure_deployment(&app, spec.deployment_id().unwrap())
        .await
        .unwrap();
    for reject_at in [1, 2] {
        checks.set(0);
        assert_eq!(
            queue
                .submit_authorized(&identity(&authority), &spec, |tx| {
                    checks.set(checks.get() + 1);
                    submission_authorization(tx, &authority, &spec, checks.get(), reject_at)
                })
                .await,
            Err(Error::Denied)
        );
        assert_eq!(checks.get(), reject_at);
        assert!(stored(fixture, &spec.id).await.is_none());
        assert_authorization_rolled_back(fixture, &app).await;
    }

    let expired = Assignment {
        expires_at: 0.try_into().unwrap(),
        ..authority.clone()
    };
    assert_eq!(
        queue
            .submit_authorized(&identity(&expired), &spec, |_| ready(Ok(expired.clone())))
            .await,
        Err(Error::Denied)
    );
    for observed in [
        expired,
        Assignment {
            app_id: foreign,
            ..authority.clone()
        },
        Assignment {
            worker_id: WorkerId::mint(),
            ..authority.clone()
        },
        Assignment {
            revision: 2.try_into().unwrap(),
            ..authority.clone()
        },
    ] {
        assert_eq!(
            queue
                .submit_authorized(&identity(&authority), &spec, |_| ready(
                    Ok(observed.clone())
                ))
                .await,
            Err(Error::Denied)
        );
        assert!(stored(fixture, &spec.id).await.is_none());
    }

    assert_eq!(
        queue
            .submit_authorized(&identity(&authority), &spec, |_| ready(Ok(
                authority.clone()
            )))
            .await,
        Ok(spec.clone())
    );
    let original = stored(fixture, &spec.id).await.unwrap();
    assert_eq!(original["state"], value!("ready"));
    checks.set(0);
    assert_eq!(
        queue
            .submit_authorized(&identity(&authority), &spec, |tx| {
                checks.set(checks.get() + 1);
                let spec = &spec;
                let authority = authority.clone();
                async move {
                    assert_transaction_job(&tx, spec, "ready").await?;
                    Ok(authority)
                }
            })
            .await,
        Ok(spec.clone())
    );
    assert_eq!(checks.get(), 2);
    assert_eq!(stored(fixture, &spec.id).await, Some(original.clone()));
    assert_eq!(
        queue
            .submit_authorized(&identity(&authority), &spec, |_| ready(Err(Error::Denied)))
            .await,
        Err(Error::Denied)
    );
    let changed = JobSpec {
        available_at: 1.try_into().unwrap(),
        ..spec.clone()
    };
    assert_eq!(
        queue
            .submit_authorized(&identity(&authority), &changed, |_| ready(Ok(
                authority.clone()
            )))
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(stored(fixture, &spec.id).await, Some(original));
}

async fn submission_authorization(
    tx: Database,
    authority: &Assignment,
    spec: &JobSpec,
    check: i64,
    reject_at: i64,
) -> Result<Assignment, Error> {
    assert!((1..=2).contains(&check));
    let Output::Rows { rows, .. } = tx
        .collection("jobs")?
        .find(
            value!({"app_id":spec.app_id.as_str(),"id":spec.id.as_str()}),
            value!({"limit":1}),
        )
        .await?
    else {
        panic!("submission authorization query returned a count");
    };
    if check == 1 {
        assert!(rows.is_empty(), "initial authorization precedes insertion");
    } else {
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], value!("ready"));
    }
    let written = tx
        .collection("queue_scopes")?
        .execute(Operation::Update {
            filter: value!({"id":authority.app_id.as_str(),"lock_version":check-1}),
            patch: value!({"$inc":{"lock_version":1}}),
            many: true,
        })
        .await?;
    assert!(
        matches!(written, Output::Count(1)),
        "authorization must share the insertion transaction"
    );
    if check == reject_at {
        Err(Error::Denied)
    } else {
        Ok(authority.clone())
    }
}

async fn authorized_submission_bounds_pending_authorization(fixture: &Fixture) {
    for short_assignment in [false, true] {
        let queue = queue(
            fixture,
            Options {
                transaction_timeout: if short_assignment {
                    Duration::from_secs(30)
                } else {
                    Duration::from_secs(1)
                },
                ..Options::default()
            },
        )
        .await;
        let app = AppId::mint();
        queue.register_scope(&app).await.unwrap();
        let spec = job(&app);
        queue
            .ensure_deployment(&app, spec.deployment_id().unwrap())
            .await
            .unwrap();
        let authority = assignment(fixture, &app).await;
        let mut observed = authority.clone();
        if short_assignment {
            observed.expires_at = (now(fixture).await + 1_000).try_into().unwrap();
        }
        let checks = Cell::new(0);
        let result = compio::time::timeout(
            Duration::from_secs(5),
            queue.submit_authorized(&identity(&authority), &spec, |tx| {
                checks.set(checks.get() + 1);
                let check = checks.get();
                let observed = observed.clone();
                let spec = &spec;
                async move {
                    if check == 1 {
                        Ok(observed)
                    } else {
                        assert_eq!(check, 2);
                        assert_transaction_job(&tx, spec, "ready").await?;
                        std::future::pending().await
                    }
                }
            }),
        )
        .await
        .expect("submission must honor transaction and observed assignment budgets");
        assert_eq!(result, Err(Error::Timeout));
        assert_eq!(checks.get(), 2, "timeout must occur after insertion");
        assert!(stored(fixture, &spec.id).await.is_none());
        assert_eq!(
            queue
                .submit_authorized(&identity(&authority), &spec, |_| ready(Ok(
                    authority.clone()
                )))
                .await,
            Ok(spec)
        );
    }
}

#[compio::test]
async fn postgres_authorized_submission_rechecks_revocation_after_app_lock() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let queue = queue(&fixture, Options::default()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(&fixture, &app).await;
    let spec = job(&app);
    let revoked = Cell::new(false);
    let checks = Cell::new(0);
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    admin.batch_execute("BEGIN").await.unwrap();
    let locked = admin
        .query(
            "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(locked.len(), 1);
    // The administrator's transaction materializes its statistics snapshot on
    // first access and serves it for the rest of the transaction. Touch the
    // activity view before the manager blocks, so a wait keyed on the cached
    // statement text would never see the manager reach the lock.
    admin
        .query("SELECT count(*) FROM pg_stat_activity", &[])
        .await
        .unwrap();
    let release = async {
        blocked_manager(
            admin,
            "l.locktype='transactionid' \
             AND pg_backend_pid()=ANY(pg_blocking_pids(a.pid))",
        )
        .await;
        assert_eq!(checks.get(), 0, "authorization must follow the app lock");
        revoked.set(true);
        admin.batch_execute("ROLLBACK").await.unwrap();
    };
    let selector = identity(&authority);
    let submit = queue.submit_authorized(&selector, &spec, |_| {
        checks.set(checks.get() + 1);
        assert!(
            revoked.get(),
            "callback must observe revocation after waiting"
        );
        ready(Err(Error::Denied))
    });
    let (result, ()) = futures::join!(submit, release);
    assert_eq!(result, Err(Error::Denied));
    assert_eq!(checks.get(), 1);
    assert!(stored(&fixture, &spec.id).await.is_none());
    assert_eq!(
        queue
            .submit_authorized(&identity(&authority), &spec, |_| ready(Ok(
                authority.clone()
            )))
            .await,
        Ok(spec)
    );
}

#[compio::test]
async fn postgres_blocked_candidate_read_uses_fresh_lease() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let queue = queue(
        &fixture,
        Options {
            lease: Duration::from_millis(200),
            ..Options::default()
        },
    )
    .await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let spec = job(&app);
    queue.submit(&spec).await.unwrap();
    let authority = assignment(&fixture, &app).await;
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    admin
        .batch_execute("BEGIN; LOCK TABLE workflow_manager.jobs IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let release = async {
        blocked_manager(admin, "l.relation='workflow_manager.jobs'::regclass").await;
        compio::time::sleep(Duration::from_millis(400)).await;
        admin.batch_execute("COMMIT").await.unwrap();
    };
    let (delivery, ()) = futures::join!(queue.claim(&authority), release);
    let grant = delivery.unwrap().unwrap();
    assert!(grant.lease().unwrap().remaining_ms.get() > 0);
    let delivery = grant.delivery().clone();
    assert_eq!(delivery.job, spec);
    assert!(delivery.deadline.get() > now(&fixture).await);
    assert_eq!(
        stored(&fixture, &spec.id).await.unwrap()["lease_deadline"],
        value!(delivery.deadline.get())
    );
}

#[compio::test]
async fn postgres_shortened_authority_bounds_commit_wait_and_receipt_replays() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let queue = queue(&fixture, Options::default()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let spec = job(&app);
    queue.submit(&spec).await.unwrap();
    let authority = assignment(&fixture, &app).await;
    let delivery = queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    let command = settlement(&delivery);
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!()
    };
    admin
        .batch_execute(
            "CREATE FUNCTION workflow_manager.block_settlement_commit() RETURNS trigger \
         LANGUAGE plpgsql AS $$ BEGIN \
           IF NEW.state = 'settled' THEN PERFORM pg_advisory_xact_lock(90316); END IF; \
           RETURN NEW; END $$; \
         CREATE CONSTRAINT TRIGGER block_settlement_commit \
           AFTER UPDATE ON workflow_manager.jobs DEFERRABLE INITIALLY DEFERRED \
           FOR EACH ROW EXECUTE FUNCTION workflow_manager.block_settlement_commit(); \
         SELECT pg_advisory_lock(90316)",
        )
        .await
        .unwrap();
    let checks = Cell::new(0);
    let shortened_expiry = Cell::new(0);
    let (send, response) = futures::channel::oneshot::channel();
    let settle = async {
        let result = queue
            .settle_authorized(
                &identity(&authority),
                &command,
                |_| {
                    checks.set(checks.get() + 1);
                    let final_check = checks.get() > 1;
                    let mut observed = authority.clone();
                    let fixture = &fixture;
                    let shortened_expiry = &shortened_expiry;
                    async move {
                        if final_check {
                            let expires = now(fixture).await + 200;
                            shortened_expiry.set(expires);
                            observed.expires_at = expires.try_into().unwrap();
                        }
                        Ok(observed)
                    }
                },
                |_| ready(Ok(authority.worker_id.clone())),
            )
            .await;
        let _ = send.send(result);
    };
    let release = async {
        blocked_manager(admin, "l.locktype='advisory'").await;
        let result = compio::time::timeout(Duration::from_secs(1), response).await;
        admin
            .query("SELECT pg_advisory_unlock(90316)", &[])
            .await
            .unwrap();
        result
            .expect("final verified expiry must bound the commit reply wait")
            .unwrap()
    };
    let ((), result) = futures::join!(settle, release);
    assert_eq!(result, Err(Error::Timeout));
    until(&fixture, shortened_expiry.get()).await;
    let expired_authority = Assignment {
        expires_at: shortened_expiry.get().try_into().unwrap(),
        ..authority.clone()
    };
    let receipt = queue
        .settle_authorized(
            &identity(&expired_authority),
            &command,
            |_| ready(Err(Error::Denied)),
            |_| ready(Ok(authority.worker_id.clone())),
        )
        .await
        .unwrap();
    assert_eq!(receipt.job_id, spec.id);
    assert_eq!(receipt.app_id, app);
    assert_eq!(receipt.attempt, delivery.attempt);
    assert_eq!(receipt.outcome, *command.outcome());
    assert_eq!(
        queue.settle(&expired_authority, &command).await.unwrap(),
        receipt
    );
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .find(value!({"app_id":app.as_str()}), value!({}))
        .await
        .unwrap()
    else {
        panic!("job query returned a count");
    };
    assert_eq!(rows.len(), 1, "a settlement publishes no successor row");
    assert_eq!(
        stored(&fixture, &spec.id).await.unwrap()["state"],
        value!("settled")
    );
}

async fn concurrent_scope_registration_preserves_identity_and_queue(fixture: &Fixture) {
    let hosts = futures::future::join_all((0..4).map(|_| queue(fixture, Options::default()))).await;
    let database = fixture.database().await;
    for round in 0..32 {
        let app = AppId::mint();
        register_scopes_together(fixture, &hosts, &app, round).await;
        let initial = registered_scope(&database, &app).await;
        assert_eq!(initial["id"], value!(app.as_str()));
        assert_eq!(initial["lock_version"], value!(0));
        let spec = job(&app);
        hosts[0].submit(&spec).await.unwrap();
        let authority = assignment(fixture, &app).await;
        let delivery = hosts[1]
            .claim(&authority)
            .await
            .unwrap()
            .unwrap()
            .delivery()
            .clone();
        assert_eq!(delivery.job, spec);
        assert_eq!(delivery.attempt.get(), 1);
        let written = database
            .collection("queue_scopes")
            .unwrap()
            .execute(Operation::Update {
                filter: value!({"id":app.as_str()}),
                patch: value!({"lock_version":7}),
                many: true,
            })
            .await
            .unwrap();
        assert!(matches!(written, Output::Count(1)));
        let before_scope = registered_scope(&database, &app).await;
        assert_eq!(before_scope["lock_version"], value!(7));
        let before_job = stored(fixture, &spec.id).await.unwrap();
        assert_eq!(before_scope["id"], initial["id"]);
        assert_eq!(before_job["state"], value!("leased"));
        register_scopes_together(fixture, &hosts, &app, round).await;
        assert_eq!(registered_scope(&database, &app).await, before_scope);
        assert_eq!(stored(fixture, &spec.id).await.unwrap(), before_job);
        assert!((hosts[2].claim(&authority).await.unwrap()).is_none());
        let command = settlement(&delivery);
        let receipt = hosts[3].settle(&authority, &command).await.unwrap();
        assert_eq!(
            hosts[0].settle(&authority, &command).await.unwrap(),
            receipt
        );
    }
}

async fn register_scopes_together(fixture: &Fixture, hosts: &[Queue], app: &AppId, round: usize) {
    let registrations =
        futures::future::join_all(hosts.iter().map(|host| host.register_scope(app)));
    let results = if let Admin::Postgres(admin) = &fixture.admin {
        admin
            .batch_execute("BEGIN; LOCK TABLE workflow_manager.queue_scopes IN SHARE MODE")
            .await
            .unwrap();
        let release = async {
            compio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let blocked: i64 = admin.query(
                        "SELECT count(DISTINCT l.pid) FROM pg_locks l WHERE NOT l.granted \
                         AND l.mode='RowExclusiveLock' AND l.relation='workflow_manager.queue_scopes'::regclass",
                        &[],
                    ).await.unwrap()[0].get(0);
                    if usize::try_from(blocked).unwrap() == hosts.len() {
                        break;
                    }
                    compio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("independent registrations must reach the shared table barrier");
            admin.batch_execute("COMMIT").await.unwrap();
        };
        let (results, ()) = futures::join!(registrations, release);
        results
    } else {
        registrations.await
    };
    for (host, result) in results.into_iter().enumerate() {
        result.unwrap_or_else(|error| panic!("registration round {round}, host {host}: {error:?}"));
    }
}

async fn registered_scope(database: &Database, app: &AppId) -> Value {
    let Output::Rows { mut rows, .. } = database
        .collection("queue_scopes")
        .unwrap()
        .find(value!({"id":app.as_str()}), value!({"limit":2}))
        .await
        .unwrap()
    else {
        panic!("scope query returned a count");
    };
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

async fn submission_and_competing_claims(fixture: &Fixture) {
    let first = queue(fixture, Options::default()).await;
    let second = queue(fixture, Options::default()).await;
    let app = AppId::mint();
    let foreign = AppId::mint();
    let spec = job(&app);
    assert_eq!(first.submit(&spec).await, Err(Error::Denied));
    let (a, b) = futures::join!(first.register_scope(&app), second.register_scope(&app));
    a.unwrap();
    b.unwrap();
    first.register_scope(&foreign).await.unwrap();
    let (a, b) = futures::join!(first.submit(&spec), second.submit(&spec));
    assert_eq!(a.unwrap(), spec);
    assert_eq!(b.unwrap(), spec);
    let mut changed = spec.clone();
    changed.operation = JobOperation::Collect {};
    assert_eq!(first.submit(&changed).await, Err(Error::Conflict));
    let authority = assignment(fixture, &app).await;
    let other_authority = assignment(fixture, &foreign).await;
    assert!((first.claim(&other_authority).await.unwrap()).is_none());
    let (a, b) = futures::join!(first.claim(&authority), second.claim(&authority));
    let deliveries: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(deliveries.len(), 1);
    let delivery = deliveries[0].delivery();
    assert_eq!(delivery.job, spec);
    assert_eq!(delivery.attempt.get(), 1);
    assert!(delivery.deadline <= authority.expires_at);
    first.register_scope(&app).await.unwrap();
    first.submit(&spec).await.unwrap();
    assert!((second.claim(&authority).await.unwrap()).is_none());
    for foreign_authority in [
        Assignment {
            worker_id: WorkerId::mint(),
            ..authority.clone()
        },
        Assignment {
            app_id: foreign.clone(),
            ..authority.clone()
        },
        Assignment {
            revision: 2.try_into().unwrap(),
            ..authority.clone()
        },
    ] {
        assert!(matches!(
            first.heartbeat(&foreign_authority, delivery).await,
            Err(Error::Denied)
        ));
        assert_eq!(
            first
                .settle(&foreign_authority, &settlement(delivery))
                .await,
            Err(Error::Denied)
        );
    }
    let mut forged = delivery.clone();
    forged.attempt = 2.try_into().unwrap();
    assert!(matches!(
        first.heartbeat(&authority, &forged).await,
        Err(Error::Conflict)
    ));
    // There is no successor field through which a caller could name a foreign
    // app, so the delivery stays leased until this holder settles it.
    assert_eq!(
        stored(fixture, &spec.id).await.unwrap()["state"],
        value!("leased")
    );
    changed = spec.clone();
    changed.app_id = foreign;
    assert_eq!(first.submit(&changed).await, Err(Error::Conflict));
}

#[expect(
    clippy::too_many_lines,
    reason = "lease expiry, replacement and lost renewal responses share queue state"
)]
async fn delayed_jobs_and_redelivery(fixture: &Fixture) {
    let queue = queue(
        fixture,
        Options {
            lease: Duration::from_millis(400),
            ..Options::default()
        },
    )
    .await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(fixture, &app).await;
    let mut spec = job(&app);
    spec.available_at = (now(fixture).await + 400).try_into().unwrap();
    queue.submit(&spec).await.unwrap();
    assert!((queue.claim(&authority).await.unwrap()).is_none());
    until(fixture, spec.available_at.get()).await;
    let delivery = queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(delivery.job, spec);
    let renewed = queue
        .heartbeat(&authority, &delivery)
        .await
        .unwrap()
        .delivery()
        .clone();
    assert!(renewed.deadline >= delivery.deadline);
    assert!(renewed.deadline <= authority.expires_at);
    until(fixture, renewed.deadline.get()).await;
    assert!(matches!(
        queue.heartbeat(&authority, &renewed).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        queue
            .settle(&authority, &settlement(&renewed))
            .await,
        Err(Error::Conflict)
    );
    let replacement = Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let replacement_authority = assignment(fixture, &app).await;
    let redelivered = replacement
        .claim(&replacement_authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(redelivered.job, spec);
    assert_eq!(redelivered.attempt.get(), renewed.attempt.get() + 1);
    assert_ne!(redelivered.worker_id, renewed.worker_id);
    assert!(matches!(
        queue.heartbeat(&authority, &renewed).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        queue
            .settle(&authority, &settlement(&renewed))
            .await,
        Err(Error::Conflict)
    );
    let expired = Assignment {
        expires_at: 0.try_into().unwrap(),
        ..replacement_authority.clone()
    };
    assert!(matches!(
        replacement.claim(&expired).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        replacement.heartbeat(&expired, &redelivered).await,
        Err(Error::Denied)
    ));

    let bounded_app = AppId::mint();
    replacement.register_scope(&bounded_app).await.unwrap();
    let bounded = Assignment {
        expires_at: (now(fixture).await + 400).try_into().unwrap(),
        ..assignment(fixture, &bounded_app).await
    };
    replacement.submit(&job(&bounded_app)).await.unwrap();
    let delivery = replacement
        .claim(&bounded)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(delivery.deadline, bounded.expires_at);

    let retry_app = AppId::mint();
    queue.register_scope(&retry_app).await.unwrap();
    let retry_authority = assignment(fixture, &retry_app).await;
    queue.submit(&job(&retry_app)).await.unwrap();
    let original = queue
        .claim(&retry_authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    compio::time::sleep(Duration::from_millis(200)).await;
    let lost_reply = queue
        .heartbeat(&retry_authority, &original)
        .await
        .unwrap()
        .delivery()
        .clone();
    assert!(lost_reply.deadline > original.deadline);
    until(fixture, original.deadline.get()).await;
    let retried = queue
        .heartbeat(&retry_authority, &original)
        .await
        .unwrap()
        .delivery()
        .clone();
    assert!(retried.deadline >= lost_reply.deadline);
    queue
        .settle(&retry_authority, &settlement(&original))
        .await
        .unwrap();
}

async fn atomic_receipt_and_replayed_receipts(fixture: &Fixture) {
    let queue = queue(fixture, Options::default()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(fixture, &app).await;
    let parent = job(&app);
    queue.submit(&parent).await.unwrap();
    let delivery = queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    let command = settlement(&delivery);
    let receipt = queue.settle(&authority, &command).await.unwrap();
    let reopened = Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options::default(),
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    // An exact retry replays the stored receipt.
    assert_eq!(reopened.settle(&authority, &command).await.unwrap(), receipt);
    assert_replay_authentication(&reopened, &authority, &command, &receipt).await;
    // A settlement that decided a different outcome does not replay, whichever
    // family the operation admits.
    let changed = support::settlement(&delivery, JobOutcome::Waiting {});
    assert_eq!(
        reopened.settle(&authority, &changed).await,
        Err(Error::Conflict)
    );
    // A settlement for a delivery the queue has since superseded is refused.
    let mut forged = delivery.clone();
    forged.attempt = 2.try_into().unwrap();
    assert_eq!(
        reopened
            .settle(&authority, &support::settlement(&forged, JobOutcome::Completed {}))
            .await,
        Err(Error::Conflict)
    );
    assert_eq!(
        stored(fixture, &parent.id).await.unwrap()["state"],
        value!("settled")
    );
    assert!((reopened.claim(&authority).await.unwrap()).is_none());
}

async fn assert_replay_authentication(
    queue: &Queue,
    authority: &Assignment,
    command: &JournalSettlement,
    receipt: &SettlementReceipt,
) {
    let expired = Assignment {
        expires_at: 0.try_into().unwrap(),
        ..authority.clone()
    };
    let identity_checked = Cell::new(false);
    assert_eq!(
        &queue
            .settle_authorized(
                &identity(&expired),
                command,
                |_| async { panic!("receipt replay must not require live placement") },
                |tx| {
                    identity_checked.set(true);
                    async move {
                        assert_transaction_job(&tx, &command.delivery().job, "settled").await?;
                        Ok(authority.worker_id.clone())
                    }
                }
            )
            .await
            .unwrap(),
        receipt
    );
    assert!(identity_checked.get());
    assert_eq!(
        queue
            .settle_authorized(
                &identity(&expired),
                command,
                |_| ready(Ok(expired.clone())),
                |_| ready(Ok(WorkerId::mint()))
            )
            .await,
        Err(Error::Denied)
    );
    assert_eq!(
        queue
            .settle_authorized(
                &identity(&expired),
                command,
                |_| ready(Ok(expired.clone())),
                |_| ready(Err(Error::Denied))
            )
            .await,
        Err(Error::Denied)
    );
}

async fn revocation_rolls_back_mutations(fixture: &Fixture) {
    let queue = queue(fixture, Options::default()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(fixture, &app).await;
    let spec = job(&app);
    queue.submit(&spec).await.unwrap();
    let checks = Cell::new(0);
    assert!(matches!(
        queue
            .claim_authorized(
                &identity(&authority),
                Claimant::Placed,
                Ok(support::delivery_ceiling()),
                |tx| {
                    checks.set(checks.get() + 1);
                    revoke_in_transaction(tx, &authority, &spec, ["ready", "leased"], checks.get())
                }
            )
            .await,
        Err(Error::Denied)
    ));
    let row = stored(fixture, &spec.id).await.unwrap();
    assert_eq!(row["state"], value!("ready"));
    assert_eq!(row["attempt"], value!(0));
    assert_authorization_rolled_back(fixture, &app).await;
    let delivery = queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    checks.set(0);
    assert!(matches!(
        queue
            .heartbeat_authorized(&identity(&authority), &delivery, |tx| {
                checks.set(checks.get() + 1);
                revoke_in_transaction(tx, &authority, &spec, ["leased", "leased"], checks.get())
            })
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(
        stored(fixture, &spec.id).await.unwrap()["lease_deadline"],
        value!(delivery.deadline.get())
    );
    assert_authorization_rolled_back(fixture, &app).await;
    let command = settlement(&delivery);
    checks.set(0);
    assert_eq!(
        queue
            .settle_authorized(
                &identity(&authority),
                &command,
                |tx| {
                    checks.set(checks.get() + 1);
                    revoke_in_transaction(
                        tx,
                        &authority,
                        &spec,
                        ["leased", "settled"],
                        checks.get(),
                    )
                },
                |_| ready(Ok(authority.worker_id.clone()))
            )
            .await,
        Err(Error::Denied)
    );
    assert_authorization_rolled_back(fixture, &app).await;
    assert_eq!(
        stored(fixture, &spec.id).await.unwrap()["state"],
        value!("leased")
    );
    cancellation_rolls_back_settlement(fixture, &queue, &authority, &command).await;
    claim_timeout_rolls_back(fixture).await;
}

async fn assert_transaction_job(tx: &Database, spec: &JobSpec, state: &str) -> Result<(), Error> {
    let Output::Rows { rows, .. } = tx
        .collection("jobs")?
        .find(
            value!({"app_id":spec.app_id.as_str(),"id":spec.id.as_str()}),
            value!({"limit":1}),
        )
        .await?
    else {
        panic!("authorization job query returned a count");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], value!(state));
    Ok(())
}

async fn revoke_in_transaction(
    tx: Database,
    authority: &Assignment,
    spec: &JobSpec,
    states: [&str; 2],
    check: i64,
) -> Result<Assignment, Error> {
    assert!((1..=2).contains(&check));
    assert_transaction_job(&tx, spec, states[usize::try_from(check - 1).unwrap()]).await?;
    let written = tx
        .collection("queue_scopes")?
        .execute(Operation::Update {
            filter: value!({"id":authority.app_id.as_str(),"lock_version":check-1}),
            patch: value!({"$inc":{"lock_version":1}}),
            many: true,
        })
        .await?;
    assert!(
        matches!(written, Output::Count(1)),
        "authorization must see its previous write in the same transaction"
    );
    if check == 1 {
        Ok(authority.clone())
    } else {
        Err(Error::Denied)
    }
}

async fn assert_authorization_rolled_back(fixture: &Fixture, app: &AppId) {
    let Output::Rows { rows, .. } = fixture
        .database()
        .await
        .collection("queue_scopes")
        .unwrap()
        .find(value!({"id":app.as_str()}), value!({"limit":1}))
        .await
        .unwrap()
    else {
        panic!("authorization scope query returned a count");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["lock_version"], value!(0));
}

async fn cancellation_rolls_back_settlement(
    fixture: &Fixture,
    queue: &Queue,
    authority: &Assignment,
    command: &JournalSettlement,
) {
    let blocked = Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options {
            transaction_timeout: Duration::from_millis(100),
            ..Options::default()
        },
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let checks = Cell::new(0);
    assert_eq!(
        blocked
            .settle_authorized(
                &identity(authority),
                command,
                |tx| {
                    checks.set(checks.get() + 1);
                    let complete = checks.get() == 1;
                    let authority = authority.clone();
                    async move {
                        if complete {
                            Ok(authority)
                        } else {
                            assert_transaction_job(&tx, &command.delivery().job, "settled").await?;
                            std::future::pending().await
                        }
                    }
                },
                |_| ready(Ok(authority.worker_id.clone()))
            )
            .await,
        Err(Error::Timeout)
    );
    // The cancelled transaction rolled the outcome write back, so the row is
    // still leased and an exact retry settles it.
    assert_eq!(
        stored(fixture, &command.delivery().job.id).await.unwrap()["state"],
        value!("leased")
    );
    assert_eq!(
        queue.settle(authority, command).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
}

async fn claim_timeout_rolls_back(fixture: &Fixture) {
    let expiring = Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options {
            lease: Duration::from_millis(200),
            transaction_timeout: Duration::from_secs(3),
            ..Options::default()
        },
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    let expiring_app = AppId::mint();
    expiring.register_scope(&expiring_app).await.unwrap();
    let expiring_authority = assignment(fixture, &expiring_app).await;
    let expiring_spec = job(&expiring_app);
    expiring.submit(&expiring_spec).await.unwrap();
    let checks = Cell::new(0);
    let timed = compio::time::timeout(
        Duration::from_secs(1),
        expiring.claim_authorized(
            &identity(&expiring_authority),
            Claimant::Placed,
            Ok(support::delivery_ceiling()),
            |_| {
                checks.set(checks.get() + 1);
                let first = checks.get() == 1;
                let authority = expiring_authority.clone();
                async move {
                    if first {
                        Ok(authority)
                    } else {
                        std::future::pending().await
                    }
                }
            },
        ),
    )
    .await
    .expect("stored delivery expiry must shorten the transaction timeout");
    assert!(matches!(timed, Err(Error::Timeout)));
    assert_eq!(
        stored(fixture, &expiring_spec.id).await.unwrap()["attempt"],
        value!(0)
    );
    assert!(expiring.claim(&expiring_authority).await.unwrap().is_some());
}

/// A claimant that refuses the kind at the head of the dispatch order still
/// reaches the rows behind it.
///
/// The claim answers one id and never tries the next, so this is the property
/// that says the refusal happened while candidates were being selected rather
/// than after one was loaded. The control differs only in the claimant: the
/// same two rows, claimed by the host that takes the head's kind, answer the
/// head.
async fn refused_kind_at_the_head(fixture: &Fixture) {
    let queue = queue(fixture, Options::default()).await;
    let restricted = AppId::mint();
    let (head, behind) = head_then_sweep(&queue, fixture, &restricted).await;
    let authority = assignment(fixture, &restricted).await;
    let claimed = queue
        .claim_authorized(
            &identity(&authority),
            Claimant::Maintenance,
            Ok(support::delivery_ceiling()),
            |_| ready(Ok(authority.clone())),
        )
        .await
        .unwrap()
        .expect("the maintenance row behind the head is claimable");
    assert_eq!(claimed.delivery().job, behind);

    // The control: the same two rows in the same order, claimed by the host
    // that takes creator work.
    let full = AppId::mint();
    let (control_head, _) = head_then_sweep(&queue, fixture, &full).await;
    let control_authority = assignment(fixture, &full).await;
    let control = queue
        .claim_authorized(
            &identity(&control_authority),
            Claimant::Placed,
            Ok(support::delivery_ceiling()),
            |_| ready(Ok(control_authority.clone())),
        )
        .await
        .unwrap()
        .expect("the head is claimable by the host that runs creator code");
    assert_eq!(control.delivery().job, control_head);

    // And the refusal itself: with the maintenance row settled, the head is all
    // that is left, and the restricted claimant answers nothing rather than it.
    queue
        .settle(&authority, &settlement(claimed.delivery()))
        .await
        .unwrap();
    assert_eq!(
        stored(fixture, &head.id).await.unwrap()["state"],
        value!("ready"),
        "the refused head must still be waiting for a claimant that takes it"
    );
    assert!(queue
        .claim_authorized(
            &identity(&authority),
            Claimant::Maintenance,
            Ok(support::delivery_ceiling()),
            |_| ready(Ok(authority.clone())),
        )
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        queue
            .claim(&authority)
            .await
            .unwrap()
            .expect("the host that runs creator code still takes the head")
            .delivery()
            .job,
        head
    );
}

/// A placed host leaves every journal sweep for the lane that owns the journal,
/// including the ones that move payload objects.
///
/// The collection row sits at the head of the dispatch order with a creator row
/// behind it. The placed claimant answers the row behind, and once that is
/// settled it answers nothing while the collection row is still `ready`. The
/// control differs only in the claimant, which takes that same row out of that
/// same queue.
///
/// Both refusal sets are asserted at the end, against the kinds this file and
/// its sibling cover. `cron` is the other sweep that moves objects, and a
/// hand-written cron row is deliverable to no claimant at all - it needs an
/// occurrence bound to a settled activation - so its control belongs where that
/// lifecycle is available, in `cron_left_to_the_journal_lane`
/// (`tests/scheduling.rs`).
async fn journal_sweep_is_left_to_the_lane(fixture: &Fixture) {
    let queue = queue(fixture, Options::default()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let bytes = collection(&app);
    let behind = job(&app);
    queue.submit(&bytes).await.unwrap();
    queue.submit(&behind).await.unwrap();
    assert!(
        dispatch_order(fixture, &bytes.id).await < dispatch_order(fixture, &behind.id).await,
        "the object-moving sweep must sit at the head for this to measure anything"
    );

    let authority = assignment(fixture, &app).await;
    let claimed = queue
        .claim(&authority)
        .await
        .unwrap()
        .expect("the creator row behind the head is claimable");
    assert_eq!(claimed.delivery().job, behind);
    queue
        .settle(&authority, &settlement(claimed.delivery()))
        .await
        .unwrap();
    assert_eq!(
        stored(fixture, &bytes.id).await.unwrap()["state"],
        value!("ready"),
        "the sweep must still be waiting for the host that owns the journal"
    );
    assert!(
        queue.claim(&authority).await.unwrap().is_none(),
        "the sweep is all that is left and a placed host answers nothing"
    );

    // The control: the same row, differing only in who claims it.
    assert_eq!(
        queue
            .claim_authorized(
                &identity(&authority),
                Claimant::Maintenance,
                Ok(support::delivery_ceiling()),
                |_| ready(Ok(authority.clone())),
            )
            .await
            .unwrap()
            .expect("the lane takes the sweep")
            .delivery()
            .job,
        bytes
    );

    // The population. The lane refuses creator work alone, and a placed host
    // refuses every sweep - so a kind classified as neither, or a tenth
    // operation added without a class, fails here rather than silently landing
    // on whichever host asked first.
    let mut lane: Vec<&str> = Claimant::Maintenance.denied().collect();
    lane.sort_unstable();
    assert_eq!(lane, ["advance"]);
    let mut placed: Vec<&str> = Claimant::Placed.denied().collect();
    placed.sort_unstable();
    assert_eq!(
        placed,
        [
            "activate",
            "close",
            "collect",
            "cron",
            "fanout",
            "management",
            "propagate",
            "reconcile",
            "release_hold",
        ]
    );
}

async fn bounds_and_privileges(fixture: &Fixture) {
    let queue = queue(fixture, Options::default()).await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(fixture, &app).await;
    let spec = job(&app);
    queue.submit(&spec).await.unwrap();
    let delivery = queue
        .claim(&authority)
        .await
        .unwrap()
        .unwrap()
        .delivery()
        .clone();
    assert_eq!(
        queue
            .settle(&authority, &settlement(&delivery))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    let small = Queue::connect(
        fixture.binding(),
        fixture.url(),
        Options {
            max_metadata_bytes: 1,
            ..Options::default()
        },
        support::synthetic_holds(),
    )
    .await
    .unwrap();
    assert_eq!(small.submit(&job(&app)).await, Err(Error::Capacity));
    for invalid in [
        Options {
            lease: Duration::ZERO,
            ..Options::default()
        },
        Options {
            transaction_timeout: Duration::ZERO,
            ..Options::default()
        },
        Options {
            max_metadata_bytes: 0,
            ..Options::default()
        },
    ] {
        assert_eq!(
            Queue::connect(
                fixture.binding(),
                fixture.url(),
                invalid,
                support::synthetic_holds()
            )
            .await
            .unwrap_err(),
            Error::Invalid
        );
    }
    assert_database_privileges(fixture).await;
}

async fn assert_database_privileges(fixture: &Fixture) {
    match &fixture.admin {
        Admin::Postgres(admin) => {
            let manager = support::connect(fixture.url()).await;
            assert!(manager
                .query("SELECT * FROM workflow_manager.jobs LIMIT 1", &[])
                .await
                .is_ok());
            assert!(manager
                .query("SELECT * FROM customer.__zeroship_workflow_history", &[])
                .await
                .is_err());
            for sql in [
                "CREATE TABLE workflow_manager.forbidden(id text PRIMARY KEY)",
                "CREATE TEMP TABLE forbidden(id text PRIMARY KEY)",
                "ALTER TABLE workflow_manager.jobs ADD COLUMN forbidden text",
                "DROP TABLE workflow_manager.jobs",
            ] {
                assert!(manager.batch_execute(sql).await.is_err(), "{sql}");
            }
            assert_eq!(
                admin
                    .query(
                        "SELECT secret FROM customer.__zeroship_workflow_history",
                        &[]
                    )
                    .await
                    .unwrap()[0]
                    .get::<_, String>(0),
                "customer-private-history"
            );
            let constraints = admin.query(
                "SELECT count(*) FROM pg_constraint WHERE contype='f' AND conrelid='workflow_manager.jobs'::regclass",&[]
            ).await.unwrap();
            assert_eq!(constraints[0].get::<_, i64>(0), 1);
        }
        Admin::Sqlite(admin) => {
            let violations: i64 = admin
                .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(violations, 0);
            let foreign_keys: i64 = admin
                .query_row(
                    "SELECT count(*) FROM pragma_foreign_key_list('jobs')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(foreign_keys, 1);
        }
    }
    assert_eq!(
        fixture.schema().as_str(),
        match &fixture.admin {
            Admin::Postgres(_) => "workflow_manager",
            Admin::Sqlite(_) => "main",
        }
    );
}

case!(
    sqlite_delivery_budget_bounds_redelivery_of_executed_attempts,
    postgres_delivery_budget_bounds_redelivery_of_executed_attempts,
    delivery_budget_bounds_redelivery
);

/// A body that never completes stops being redelivered once the job's counted
/// executions reach the app's budget. Renewal is the only evidence an attempt
/// began: a claim the journal defers never renews, so capacity pressure
/// and a paused dispatch cannot spend the budget. Nothing settles an exhausted
/// job, because no executor produced an outcome for it.
async fn delivery_budget_bounds_redelivery(fixture: &Fixture) {
    const CEILING: i64 = 2;
    let queue = queue(
        fixture,
        Options {
            lease: Duration::from_millis(300),
            ..Options::default()
        },
    )
    .await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(fixture, &app).await;
    let selector = identity(&authority);
    let spec = job(&app);
    queue.submit(&spec).await.unwrap();
    let claim = |ceiling: i64| {
        let authority = authority.clone();
        let selector = selector.clone();
        let queue = queue.clone();
        async move {
            queue
                .claim_authorized(&selector, Claimant::Placed, Ok(ceiling), move |_| {
                    ready(Ok(authority.clone()))
                })
                .await
                .unwrap()
        }
    };
    let renew = |delivery: Delivery| {
        let authority = authority.clone();
        let selector = selector.clone();
        let queue = queue.clone();
        async move {
            queue
                .heartbeat_authorized(&selector, &delivery, move |_| ready(Ok(authority.clone())))
                .await
                .unwrap()
        }
    };
    // More deferred attempts than the budget allows. Each is claimed, never
    // renewed, and left to expire; none of them may count.
    for _ in 0..=CEILING {
        let grant = claim(CEILING)
            .await
            .expect("an attempt that never began keeps the job claimable");
        assert_eq!(
            stored(fixture, &spec.id).await.unwrap()["execution_attempts"],
            value!(0)
        );
        until(fixture, grant.delivery().deadline.get()).await;
    }
    // Attempts that began. Renewing twice within one attempt must spend the
    // budget once.
    for spent in 1..=CEILING {
        let grant = claim(CEILING).await.expect("the budget admits this attempt");
        let renewed = renew(grant.delivery().clone()).await;
        let renewed = renew(renewed.delivery().clone()).await;
        assert_eq!(renewed.delivery().attempt, grant.delivery().attempt);
        assert_eq!(
            stored(fixture, &spec.id).await.unwrap()["execution_attempts"],
            value!(spent)
        );
        until(fixture, renewed.delivery().deadline.get()).await;
    }
    assert!(
        claim(CEILING).await.is_none(),
        "an exhausted job is no longer a delivery candidate"
    );
    let row = stored(fixture, &spec.id).await.unwrap();
    assert_eq!(row["state"], value!("leased"));
    assert_eq!(row["execution_attempts"], value!(CEILING));
    assert_eq!(row["outcome"], Value::Null);
    assert_eq!(row["settlement_digest"], Value::Null);
    // The budget is policy, not a property of the row: raising it re-admits the
    // same job, and no other app's queue was consulted to decide either way.
    let readmitted = claim(CEILING + 1)
        .await
        .expect("a raised budget re-admits the job");
    assert_eq!(readmitted.delivery().job, spec);
    assert!(matches!(
        queue
            .claim_authorized(&selector, Claimant::Placed, Ok(0), |_| ready(Ok(
                authority.clone()
            )))
            .await,
        Err(Error::Invalid)
    ));
}

case!(
    sqlite_delivery_grants_keep_their_original_monotonic_budget,
    postgres_delivery_grants_keep_their_original_monotonic_budget,
    delivery_grant_budget
);

async fn delivery_grant_budget(fixture: &Fixture) {
    let queue = queue(
        fixture,
        Options {
            lease: Duration::from_millis(900),
            ..Options::default()
        },
    )
    .await;
    let app = AppId::mint();
    queue.register_scope(&app).await.unwrap();
    let authority = assignment(fixture, &app).await;
    let stale = Assignment {
        expires_at: 0.try_into().unwrap(),
        ..authority.clone()
    };
    let selector = identity(&stale);
    let spec = job(&app);
    queue
        .submit_authorized(&selector, &spec, |_| ready(Ok(authority.clone())))
        .await
        .unwrap();
    assert!(matches!(queue.claim(&stale).await, Err(Error::Denied)));
    let grant = queue
        .claim_authorized(
            &selector,
            Claimant::Placed,
            Ok(support::delivery_ceiling()),
            |_| ready(Ok(authority.clone())),
        )
        .await
        .unwrap()
        .unwrap();
    let original = grant.lease().unwrap();
    assert_eq!(&original.delivery, grant.delivery());
    assert!(original.remaining_ms.get() <= 900);
    compio::time::sleep(Duration::from_millis(200)).await;
    let delayed = grant.lease().unwrap();
    assert_eq!(delayed.delivery, original.delivery);
    assert!(delayed.remaining_ms < original.remaining_ms);
    let renewed = queue
        .heartbeat_authorized(&selector, grant.delivery(), |_| {
            ready(Ok(authority.clone()))
        })
        .await
        .unwrap();
    let renewed_lease = renewed.lease().unwrap();
    assert_eq!(renewed.delivery().attempt, grant.delivery().attempt);
    assert!(renewed.delivery().deadline > grant.delivery().deadline);
    let old_after_renewal = grant.lease().unwrap();
    assert_eq!(old_after_renewal.delivery, original.delivery);
    assert!(old_after_renewal.remaining_ms <= delayed.remaining_ms);
    assert!(renewed_lease.remaining_ms > old_after_renewal.remaining_ms);
    compio::time::sleep(Duration::from_millis(
        old_after_renewal.remaining_ms.get() + 20,
    ))
    .await;
    assert!(
        grant.lease().is_err(),
        "heartbeat must not refresh a previously issued grant"
    );
    assert!(
        renewed.lease().is_ok(),
        "the new grant carries the renewed budget"
    );
    queue
        .settle(&authority, &settlement(renewed.delivery()))
        .await
        .unwrap();
}
