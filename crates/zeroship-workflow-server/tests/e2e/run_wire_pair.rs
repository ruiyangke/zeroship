//! The creator-facing run calls, driven by the REAL client against the REAL
//! service, so the two halves of the wire format are compared by construction.
//!
//! # What this covers that neither side's own suite can
//!
//! `http_runs.rs` builds every request by hand and reads every reply by hand,
//! so it asserts the service against the test's idea of the format.
//! `zeroship-workflow-client`'s suite answers `WorkerCoordinator` from a
//! hand-rolled peer, so it asserts the client against the test's idea of the
//! format. Each is internally consistent, and a field one side renamed would
//! leave both green. Here the producer of the request is
//! `WorkerCoordinator`'s own serialization and the consumer is the service's
//! own extractor -- and for the reply, the other way round -- so a rename on
//! either side has nowhere to hide.
//!
//! # Why the spawned process rather than an in-process app
//!
//! `WorkerCoordinator` speaks over `cyper`, so it needs a real socket; the
//! in-process `test::init_service` harness `http_runs.rs` uses has none. Of the
//! two harnesses that do serve one, `ServerProcess` is both the more faithful
//! and the simpler: an in-process `ntex` test server would still have to build
//! `SharedState` inside a `Send` factory from loose primitives, because the
//! state is `!Send`, and that rebuilt composition is exactly what the real
//! binary already assembles -- with its real policy source, its real journal
//! and its real body limits.
#![allow(
    clippy::future_not_send,
    reason = "HTTP and database fixtures stay on the ntex compio runtime"
)]

use crate::support::{
    holds, journal, leased_task, platform, provision, run_journal, server_process,
};

use ntex::client::Client;
use serde_json::json;
use std::{num::NonZeroU32, rc::Rc, sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_peers::{service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        AssignedScope, ConflictPolicy, CreatorStartOptions, FailureCode, PayloadReservation,
        ReadStepOutput, ReadTaskPayload, ReservePayload, ResolveTaskExecutable,
        RegisterWorker, RequestId, RestartOptions, RestartRun, RunFailure, RunId, RunOperation,
        Revision, RunScope, RunState, SignalOptions, SignalRun, StartRun, TransitionRun, WorkerId,
        WorkerState, WorkflowOutputRef,
    },
    workflow_jobs::{DeploymentId, JobOperation, JobSpec},
    workflow_policy::AppPolicy,
};
use zeroship_data_orm::binding::DbBinding;
use zeroship_core::schema_name::SchemaName;
use zeroship_workflow_client::{Options as ClientOptions, RunError, WorkerCoordinator};
use zeroship_workflow::service::delivery::{AcceptedJob, AppJournal, ClaimedTask};
use zeroship_workflow_manager::recovery::Options as RecoveryOptions;
use zeroship_workflow_manager::{Options as QueueOptions, Queue};
use zeroship_workflow_server::coordinator::{connect_eligibility, Coordinator, Options};

struct Fixture {
    platform: platform::Platform,
    /// Held for the lifetime of the case: dropping it kills the service.
    _server: server_process::ServerProcess,
    client: WorkerCoordinator,
    scope: AssignedScope,
    app: AppId,
    /// The service's queue, open in this process so a case can publish a
    /// committed creator intent the way the service's own publication does.
    queue: Queue,
}

impl Fixture {
    async fn new() -> Self {
    let platform = platform::Platform::new().await;

        // Control's verification key, the one thing the service refuses to
        // start without. The peer itself is the shared fake in `queue_control`,
        // which `ServerProcess::start` brings up.
        let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
        let control_key = ServiceSigningKey::generate();
        let peers = platform.work.path().join("run-wire-peers.json");
        platform::write_private(
            &peers,
            serde_json::to_vec(&json!({"keys":[{
                "iss":control.as_str(),"x":control_key.public_jwk_x()
            }]}))
            .unwrap(),
        );
        let http = Client::new().await;
        let server = server_process::ServerProcess::start(
            &platform,
            &peers,
            platform.work.path(),
            "run-wire",
            &http,
        )
        .await;

        // ONE key, on both sides of the enrolment: the registry verifies the
        // assertion against the public half in `worker_instances`, and the
        // client mints under the private half. A fixture that generated them
        // separately would authenticate nothing.
        let worker = WorkerId::mint();
        let key = ServiceSigningKey::generate();
        let public = key.verifying_key_bytes().to_vec();
        platform.admin.execute(
            "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) \
             VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault','ezn_default000000000000000000',now() + interval '1 hour')",
            &[&worker.as_str(), &vec![1_u8], &public, &platform::DEFAULT_JOIN_SIGNER_ID],
        ).await.unwrap();

        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap();
        let auth = Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(issuer, key, ServiceTrustBundle::new()).unwrap(),
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ));
        let client = WorkerCoordinator::new(&server.url, auth, ClientOptions::default()).unwrap();
        assert_eq!(client.worker_id(), &worker);
        // Registration goes through the same client, so the placement below is
        // held against a worker this service has actually seen.
        client
            .register(&RegisterWorker {
                capacity: NonZeroU32::new(1).unwrap(),
                state: WorkerState::Ready,
            })
            .await
            .unwrap();

        let app = AppId::mint();
        provision::provision(&platform, &app, &AppPolicy::default()).await;
        let assignment = platform
            .seed_placement(&app, &worker, Duration::from_mins(2))
            .await;
        let queue = Queue::connect(
            DbBinding::platform(
                "workflow_manager",
                "workflow_manager",
                SchemaName::new("workflow_manager").unwrap(),
            ),
            &platform.runtime_url,
            QueueOptions::default(),
            holds::client(),
        )
        .await
        .unwrap();
        Self {
            platform,
            _server: server,
            client,
            scope: AssignedScope {
                app_id: assignment.app_id,
                assignment_revision: assignment.revision,
            },
            app,
            queue,
        }
    }

    /// Register the recovery responsibility a platform deployment activation
    /// would have registered. `Recovery::establish` refuses an unactivated
    /// scope, so `signal` and `transition` cannot get past the service's own
    /// ingress fence without it. The service owns these rows; this fixture
    /// writes them through the manager's own API against the same database
    /// rather than inventing their shape in SQL.
    async fn ensure_recovery(&self) {
        let eligibility = Rc::new(
            connect_eligibility(&self.platform.runtime_url, Options::default())
                .await
                .unwrap(),
        );
        Coordinator::connect(
            &self.platform.runtime_url,
            Options::default(),
            holds::client(),
            eligibility,
        )
        .await
        .unwrap()
        .recovery(RecoveryOptions::default())
        .unwrap()
        .ensure(&self.app, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    }

    /// One column of one journal row, so a reply is compared against what the
    /// service stored rather than against itself.
    /// A column of the seeded deployment, so a caller compares a resolved pin
    /// against the journal's own row rather than against a second derivation.
    /// A column of a dispatch row, so a caller compares a release against the
    /// journal state it produced rather than against the reply alone.
    async fn task_column(&self, task: &str, column: &str) -> String {
        self.platform
            .admin
            .query_one(
                &format!(
                    "SELECT {column} FROM workflow_manager.__zeroship_workflow_tasks \
                     WHERE app_id=$1 AND id=$2"
                ),
                &[&self.app.as_str(), &task],
            )
            .await
            .unwrap()
            .get(0)
    }

    async fn deploy_column(&self, deploy: &str, column: &str) -> String {
        self.platform
            .admin
            .query_one(
                &format!(
                    "SELECT {column} FROM workflow_manager.__zeroship_workflow_deploys \
                     WHERE app_id=$1 AND id=$2"
                ),
                &[&self.app.as_str(), &deploy],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// Park a deployment the way a damaged artifact does.
    ///
    /// `park_deployment` is what the journal itself runs on a corrupt load, and
    /// its whole effect on admissibility is this state change -- so a caller
    /// asserting the availability fence arranges the state rather than arranging
    /// a corrupt object, which would test the loader instead.
    async fn park_deployment(&self, deploy: &str) {
        let parked = self
            .platform
            .admin
            .execute(
                "UPDATE workflow_manager.__zeroship_workflow_deploys SET state='unavailable' \
                 WHERE app_id=$1 AND id=$2 AND state='available'",
                &[&self.app.as_str(), &deploy],
            )
            .await
            .unwrap();
        assert_eq!(parked, 1, "no available deployment was parked");
    }

    async fn run_column(&self, run: &RunId, column: &str) -> String {
        self.platform
            .admin
            .query_one(
                &format!(
                    "SELECT {column} FROM workflow_manager.__zeroship_workflow_runs \
                     WHERE app_id=$1 AND id=$2"
                ),
                &[&self.app.as_str(), &run.as_str()],
            )
            .await
            .unwrap()
            .get(0)
    }
}

/// Every creator-facing run call, client to service, over a real socket.
///
/// Four calls, four wire contracts, each asserted at BOTH ends: the request the
/// client serialized had to be the one the service's extractor accepts, and the
/// reply the service serialized had to be the one the client's deserializer
/// accepts. Rename a field or move an endpoint path on either side and this
/// goes red while both existing suites stay green, because each of those
/// supplies its own counterpart.
///
/// `restart_run` IS PINNED AS A REFUSAL, not as a success. Nothing holds the
/// seeded run's deployment in this fixture's journal, so `restart` resolves that
/// retained deployment and then refuses `unavailable` at `require_journal_hold`,
/// before it ever reaches the ingress fence -- the refusal `http_runs.rs` uses
/// in process as the control for its served restart. The refusal is a wire
/// contract in its own right: the service picks the status from
/// `RunFailure::status`, and the client believes a refusal body only when the
/// status it arrived with is the one that code pairs with. What this arm proves
/// is that the pairing survives the round trip. It flips to a success arm when
/// this fixture's journal holds that deployment.
#[ntex::test]
async fn the_client_and_the_service_agree_on_every_run_call() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = journal::seed_run(&fixture.platform, &fixture.app).await;
    let scope = || fixture.scope.clone();

    // READ. The reply must say what the journal says, not merely parse.
    let status = fixture
        .client
        .run_status(&RunScope {
            scope: scope(),
            run_id: run.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.run_column(&run, "state").await.parse::<RunState>(),
        Ok(status.state)
    );
    assert_eq!(status.state, RunState::Queued);
    assert!(status.output.is_none(), "{:?}", status.output);
    assert!(status.error.is_none(), "{:?}", status.error);
    assert!(
        status.continued_as_new_run_id.is_none(),
        "{:?}",
        status.continued_as_new_run_id
    );

    // The refusal contract for a read: a run this app does not have. The client
    // returns it as a refusal rather than as a transport error, which is what
    // shows the status and the body agreed.
    let missing = fixture
        .client
        .run_status(&RunScope {
            scope: scope(),
            run_id: RunId::mint(),
        })
        .await
        .expect_err("a run this app never held was answered");
    assert!(
        matches!(missing, RunError::Refused(RunFailure::NotFound { .. })),
        "{missing:?}"
    );

    fixture.ensure_recovery().await;

    // WRITE, carrying a creator value. The id the reply names must be the row
    // the journal holds, against this run and with the type the client sent.
    let delivered = fixture
        .client
        .signal_run(&SignalRun {
            request_id: RequestId::mint(),
            scope: scope(),
            run_id: run.clone(),
            options: SignalOptions {
                signal_type: "ping".to_owned(),
                payload: json!({"from": "the wire pair"}),
            },
        })
        .await
        .unwrap();
    let stored = fixture
        .platform
        .admin
        .query_one(
            "SELECT run_id,signal_type FROM workflow_manager.__zeroship_workflow_signals \
             WHERE app_id=$1 AND id=$2",
            &[&fixture.app.as_str(), &delivered.id],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>(0), run.as_str());
    assert_eq!(stored.get::<_, String>(1), "ping");

    // WRITE, carrying an enumerated operation. The state the reply names must
    // be the state the journal now holds.
    let transitioned = fixture
        .client
        .transition_run(&TransitionRun {
            request_id: RequestId::mint(),
            scope: scope(),
            run_id: run.clone(),
            operation: RunOperation::Pause,
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.run_column(&run, "state").await.parse::<RunState>(),
        Ok(transitioned.state)
    );
    assert_ne!(
        transitioned.state,
        RunState::Queued,
        "the transition left the run where it started, so it decided nothing"
    );

    // REFUSAL, pinned as such: see this test's doc comment.
    let refused = fixture
        .client
        .restart_run(&RestartRun {
            request_id: RequestId::mint(),
            scope: scope(),
            run_id: run.clone(),
            options: RestartOptions::default(),
        })
        .await
        .expect_err("restart was served, so this journal now holds its deployment");
    assert_eq!(refused, RunError::Refused(RunFailure::Unavailable {}));

    // WRITE, carrying a creator VALUE and no descriptor. The run the reply names
    // must be one the journal holds, and the object the service staged for that
    // value must be the one the run's input edge owns -- so the descriptor came
    // from the bytes rather than from anything the client could have sent.
    let started = fixture
        .client
        .start_run(&StartRun {
            request_id: RequestId::mint(),
            scope: scope(),
            workflow_name: "demo".to_owned(),
            input: json!({"order": 7}),
            options: CreatorStartOptions {
                key: Some("order-7".to_owned()),
                on_conflict: ConflictPolicy::Reject,
            },
        })
        .await
        .unwrap();
    assert_eq!(started.state, RunState::Queued);
    let staged = fixture
        .platform
        .admin
        .query_one(
            "SELECT p.hash = encode(sha256($3::bytea),'hex'), p.state FROM \
             workflow_manager.__zeroship_workflow_payload_refs e \
             JOIN workflow_manager.__zeroship_workflow_payloads p \
               ON p.app_id=e.app_id AND p.id=e.payload_id \
             WHERE e.app_id=$1 AND e.run_id=$2 AND e.slot='input'",
            &[
                &fixture.app.as_str(),
                &started.id,
                &serde_json::to_vec(&json!({"order": 7})).unwrap(),
            ],
        )
        .await
        .unwrap();
    assert!(
        staged.get::<_, bool>(0),
        "the staged input's digest is not over the value the client sent"
    );
    assert_eq!(staged.get::<_, String>(1), "referenced");

    // READ, located rather than carried. The run the start admitted returned
    // nothing, so it owns no output object -- the same absence `status` reports
    // by carrying no descriptor, and the refusal a located read answers with.
    let started_run = RunId::parse(&started.id).unwrap();
    let absent = fixture
        .client
        .read_run_output(&RunScope {
            scope: scope(),
            run_id: started_run.clone(),
        })
        .await
        .expect_err("a run that returned nothing was answered with a location");
    assert!(
        matches!(absent, RunError::Refused(RunFailure::NotFound { .. })),
        "{absent:?}"
    );
    let unrecorded = fixture
        .client
        .read_step_output(&ReadStepOutput {
            scope: scope(),
            run_id: started_run,
            name: "charge".to_owned(),
            occurrence: 0,
        })
        .await
        .expect_err("a step this run never recorded was answered");
    assert!(
        matches!(unrecorded, RunError::Refused(RunFailure::NotFound { .. })),
        "{unrecorded:?}"
    );
}

/// The task payload read, client to service, over a real socket.
///
/// Separate from the run calls because the AUTHORITY is different in kind: a run
/// call is authorized by the placement the manager holds for this worker, and
/// this one by a dispatch credential the journal minted and keeps only a hash
/// of. Sharing the run test's body would hide that, since the fixture's
/// placement would satisfy both and neither arm would say which one answered.
///
/// THREE ARMS, and the pairing is the point. The located reply proves the
/// journal was reached and answered from its own rows. The two refusals prove it
/// was reached for the right reason and refused at different STAGES: a malformed
/// credential is refused by the route before any journal call, and a well-formed
/// one naming no dispatch is refused by the journal itself. Two codes from two
/// stages is what separates "the journal said no" from "the request never got
/// there" -- which a single refusal arm cannot distinguish, and which is exactly
/// how a route registered at the wrong path or missing its grant would look.
///
/// What this does NOT cover: the bytes. They never cross this call, and opening
/// the located object is the caller's half, asserted where the object store is.
#[ntex::test]
async fn the_client_and_the_service_agree_on_a_task_payload_read() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = journal::seed_run(&fixture.platform, &fixture.app).await;
    let leased = leased_task::seed_leased_task(
        &fixture.platform,
        &fixture.app,
        &run,
        fixture.client.worker_id(),
    )
    .await;

    let located = fixture
        .client
        .read_task_payload(&ReadTaskPayload {
            app_id: fixture.app.clone(),
            task_id: leased.task.clone(),
            token: leased.token.clone(),
            reference: leased.reference.clone(),
        })
        .await
        .expect("the journal locates the object its own edge names");
    assert_eq!(located.payload_id, leased.payload);
    assert_eq!(located.reference, leased.reference);

    // Refused by the ROUTE, before a journal call: the token's own parse.
    let malformed = fixture
        .client
        .read_task_payload(&ReadTaskPayload {
            app_id: fixture.app.clone(),
            task_id: leased.task.clone(),
            token: "not-a-token".to_owned(),
            reference: leased.reference.clone(),
        })
        .await
        .expect_err("a credential that cannot be a token was accepted");
    assert!(
        matches!(
            malformed,
            zeroship_workflow_client::Error::Refused(FailureCode::Unauthenticated)
        ),
        "{malformed:?}"
    );

    // Refused by the JOURNAL: a well-formed credential naming no dispatch of
    // this app. A stale delivery is a conflict rather than a missing URL, which
    // is the mapping every delivery call shares.
    let unknown = fixture
        .client
        .read_task_payload(&ReadTaskPayload {
            app_id: fixture.app.clone(),
            // A WELL-FORMED dispatch id, so the refusal is the lookup's and not the
            // prefix parse that precedes it.
            task_id: zeroship_core::typed_id::generate(
                zeroship_core::typed_id::WORKFLOW_DISPATCH_PREFIX,
            ),
            token: leased.token,
            reference: leased.reference,
        })
        .await
        .expect_err("a dispatch this app never held was answered");
    assert!(
        matches!(
            unknown,
            zeroship_workflow_client::Error::Refused(FailureCode::Conflict)
        ),
        "{unknown:?}"
    );
}

/// The task executable resolution, client to service, over a real socket.
///
/// The ARTIFACT is deliberately not part of this contract: the reply names a
/// deployment and the caller loads it from its own object store. So what a wire
/// test can bind is the pin and the fences, and that is what this asserts --
/// against the `deploys` row the fixture seeded, read back out of the journal
/// rather than derived a second time here.
///
/// THREE ARMS, the same staging split as the payload read. A resolved pin proves
/// the journal was reached and answered from its own rows. A malformed credential
/// is refused by the ROUTE before any journal call. And a deployment the journal
/// has PARKED is refused by the journal itself -- which is the arm that matters
/// most, because it is the fence a worker loading from its own store could never
/// apply for itself, and the reason the resolution crosses at all.
#[ntex::test]
async fn the_client_and_the_service_agree_on_a_task_executable_resolution() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = journal::seed_run(&fixture.platform, &fixture.app).await;
    // Admission has to be OPEN for a pin to resolve: the journal reads an
    // `admission_generation` under the hold scope and refuses without one.
    let deploy = run_journal::seed_journal_hold(&fixture.platform, &fixture.app).await;
    let leased = leased_task::seed_leased_task(
        &fixture.platform,
        &fixture.app,
        &run,
        fixture.client.worker_id(),
    )
    .await;
    let resolve = || ResolveTaskExecutable {
        app_id: fixture.app.clone(),
        task_id: leased.task.clone(),
        token: leased.token.clone(),
    };

    let pinned = fixture
        .client
        .resolve_task_executable(&resolve())
        .await
        .expect("the journal resolves the deployment its own run row pins");
    assert_eq!(pinned.deploy_id.as_str(), deploy);
    assert_eq!(
        pinned.deploy_hash,
        fixture.deploy_column(&deploy, "hash").await
    );
    // The fences travel with the pin, and the caller does not invent them.
    assert_eq!(pinned.availability_epoch, 0);
    assert_eq!(pinned.admission_generation, 1);

    // Refused by the ROUTE, before a journal call.
    let malformed = fixture
        .client
        .resolve_task_executable(&ResolveTaskExecutable {
            token: "not-a-token".to_owned(),
            ..resolve()
        })
        .await
        .expect_err("a credential that cannot be a token was accepted");
    assert!(
        matches!(
            malformed,
            zeroship_workflow_client::Error::Refused(FailureCode::Unauthenticated)
        ),
        "{malformed:?}"
    );

    // Refused by the JOURNAL, on the fence a local load cannot see. Parking is
    // what a damaged artifact leaves behind, and a worker that resolved its own
    // pin from the object store would happily reload the corrupt bytes.
    fixture.park_deployment(&deploy).await;
    let parked = fixture
        .client
        .resolve_task_executable(&resolve())
        .await
        .expect_err("a parked deployment was still offered for replay");
    assert!(
        matches!(
            parked,
            zeroship_workflow_client::Error::Refused(FailureCode::Unavailable)
        ),
        "{parked:?}"
    );
}

/// The release and receipt pair, client to service, over a real socket.
///
/// ONE CASE FOR BOTH, because they are one recovery: a holder that cannot finish
/// either hands the task back or, after an uncertain settlement, asks what
/// committed. Driving them together is what shows the pair agrees about the same
/// delivery rather than each agreeing with the test.
///
/// THE RECEIPT IS READ TWICE, before and after the release, and both answers are
/// `None`. That is the property, not an oversight: a release commits no
/// execution, so it must not leave an outcome behind. A single read could not
/// tell "no outcome yet" from "no outcome ever", and the pair of reads is what
/// makes the absence attributable to the release.
///
/// THE RELEASE IS ALSO ASSERTED IDEMPOTENT. `release_job` returns early on a task
/// already released, so a holder whose acknowledgement was lost may repeat it --
/// which is the same uncertain-reply situation the receipt read exists for, and
/// it would be a strange pair if one half tolerated a retry and the other did
/// not.
#[ntex::test]
async fn the_client_and_the_service_agree_on_a_release_and_a_receipt() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = journal::seed_run(&fixture.platform, &fixture.app).await;
    run_journal::seed_journal_hold(&fixture.platform, &fixture.app).await;
    // NO SEEDED DISPATCH HERE, deliberately: `tasks::assign` claims a run only
    // while `runs.task_id` is null, so a hand-seeded task would hold the run and
    // the claim below would answer deferred instead of handing out work. The
    // claim creates the dispatch this case releases.
    let job = JobSpec {
        id: zeroship_core::workflow_jobs::JobId::mint(),
        app_id: fixture.app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::parse_owned(
                fixture.app.as_str().replacen("app_", "dep_", 1),
            )
            .unwrap(),
            run_id: run.clone(),
            generation: 0,
            revision: Revision::try_from(1).unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    };

    // A REAL CLAIM, because `LeasedJob` has no public constructor and should not:
    // delivery authority comes from the manager granting it, never from a struct a
    // caller fills in. So the release is driven against the job this same client
    // submitted and claimed, which exercises the claim and its journal acceptance
    // on the way.
    let submitted = fixture
        .queue
        .submit(&job)
        .await
        .expect("the queue accepts a creator advance");
    assert_eq!(submitted, job);
    let claimed = fixture
        .client
        .claim_job::<AppJournal>(&fixture.scope)
        .await
        .expect("the claim exchange answers")
        .expect("the submitted advance is claimable");
    let accepted = claimed
        .accepted
        .expect("an advance carries a journal acceptance");
    let lease = claimed.lease;
    let AcceptedJob::Execute {
        assignment,
        remaining_ms,
    } = accepted
    else {
        panic!("the journal hands out a task for an advance")
    };
    let claim = ClaimedTask {
        id: assignment.id.clone(),
        token: assignment.token.clone(),
        remaining_ms,
    };

    // Nothing has committed, so the recovery read says so as a fact. It is read
    // once the claim has made this client the job's holder, which is the only
    // worker a receipt is read for.
    let before = fixture
        .client
        .job_receipt::<AppJournal>(&job)
        .await
        .expect("an uncommitted job answers its holder rather than refusing");
    assert!(before.is_none(), "{before:?}");

    fixture
        .client
        .release_job::<AppJournal>(&lease, &claim)
        .await
        .expect("a held task is handed back");
    assert_eq!(
        fixture.task_column(&assignment.id, "state").await,
        "released"
    );

    // Idempotent: the same release again, as a holder that lost its reply sends.
    fixture
        .client
        .release_job::<AppJournal>(&lease, &claim)
        .await
        .expect("a repeated release is an acknowledgement, not a conflict");

    // And a release leaves no outcome behind.
    let after = fixture
        .client
        .job_receipt::<AppJournal>(&job)
        .await
        .expect("the receipt read still answers after a release");
    assert!(after.is_none(), "{after:?}");
}

/// The payload reservation, client to service, over a real socket.
///
/// THE IDEMPOTENCE IS THE POINT, not a side property. The unique index on
/// `(app_id, task_id, request_id)` enforces nothing while `task_id` is NULL, and
/// this reservation crosses a wire where retries are expected -- so what stops a
/// retry reserving a second object is the service's own lookup on
/// `(app_id, request_id)`. Two reservations under one request id must answer with
/// the SAME payload id, and this asserts that rather than assuming it.
///
/// FOUR ARMS. A first reservation; the same request id again answering the same
/// id; the same request id with DIFFERENT bytes refused, which is what makes the
/// dedupe a comparison rather than a blind reuse; and a malformed credential
/// refused at the route before any journal call.
///
/// What this does NOT cover: the object write and the confirm. The bytes never
/// cross this call, and the confirm's compare-and-swap is asserted where a fence
/// can be committed between the two.
#[ntex::test]
async fn the_client_and_the_service_agree_on_a_payload_reservation() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = journal::seed_run(&fixture.platform, &fixture.app).await;
    let leased = leased_task::seed_leased_task(
        &fixture.platform,
        &fixture.app,
        &run,
        fixture.client.worker_id(),
    )
    .await;
    let request = RequestId::mint();
    let reference = WorkflowOutputRef {
        hash: "c".repeat(64),
        size: 7,
        content_type: Some("application/json".to_owned()),
    };
    let reserve = |request: RequestId, reference: WorkflowOutputRef| ReservePayload {
        app_id: fixture.app.clone(),
        task_id: leased.task.clone(),
        token: leased.token.clone(),
        request_id: request,
        reference,
    };

    let first = fixture
        .client
        .reserve_task_payload(&reserve(request.clone(), reference.clone()))
        .await
        .expect("a live dispatch reserves an upload");
    let PayloadReservation::Reserved {
        payload_id,
        expires_at,
    } = first
    else {
        panic!("a fresh request id has nothing staged against it")
    };
    assert!(expires_at > 0, "{expires_at}");

    // The retry contract: same request id, same object.
    let again = fixture
        .client
        .reserve_task_payload(&reserve(request.clone(), reference.clone()))
        .await
        .expect("a retried reservation answers rather than refusing");
    assert_eq!(
        again,
        PayloadReservation::Reserved {
            payload_id: payload_id.clone(),
            expires_at,
        }
    );

    // And the dedupe is a comparison: the same request id for other bytes is a
    // caller contradicting itself, not a second upload.
    let substituted = fixture
        .client
        .reserve_task_payload(&reserve(
            request,
            WorkflowOutputRef {
                hash: "d".repeat(64),
                ..reference.clone()
            },
        ))
        .await
        .expect_err("a request id was reused for another object");
    assert!(
        matches!(
            substituted,
            zeroship_workflow_client::Error::Refused(FailureCode::Conflict)
        ),
        "{substituted:?}"
    );

    let malformed = fixture
        .client
        .reserve_task_payload(&ReservePayload {
            token: "not-a-token".to_owned(),
            ..reserve(RequestId::mint(), reference)
        })
        .await
        .expect_err("a credential that cannot be a token was accepted");
    assert!(
        matches!(
            malformed,
            zeroship_workflow_client::Error::Refused(FailureCode::Unauthenticated)
        ),
        "{malformed:?}"
    );
}
