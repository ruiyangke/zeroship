//! The creator-facing run calls, over the wire, against this service's journal.
#![expect(
    clippy::future_not_send,
    reason = "HTTP fixtures use the owning ntex compio runtime"
)]

use crate::support::{holds, journal, platform, run_journal, zone};

use futures::future::LocalBoxFuture;
use ntex::{
    http::StatusCode,
    web::{self, test},
};
use std::{
    num::NonZeroU32,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionMinter, ServiceAssertionVerifier, ServiceIssuer,
        ServiceSigningKey, ServiceTrustBundle,
    },
    service_identity::endpoints,
    workflow_coordination::{
        AssignedScope, DeliveredSignal, PayloadLocation, ReadStepOutput, RegisterWorker, RequestId,
        RestartOptions, RestartRun, RestartedRun, RunFailure, RunId, RunOperation, RunScope,
        RunState, RunStatus, SignalOptions, SignalRun, StartedRun, TransitionRun, WorkerId,
        WorkerState, AUDIENCE,
    },
    workflow_jobs::DeploymentId,
    workflow_policy::AppPolicy,
};
use zeroship_storage::StorageBackendConfig;
use zeroship_workflow_manager::{
    coordinator::Placed,
    policy::{PolicyObservation, PolicySource},
    recovery::{Options as RecoveryOptions, ScopeState},
    Error as NativeError,
};
use zeroship_workflow_server::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    coordinator::{connect_eligibility, Coordinator, Options},
    payloads::ServicePayloads,
    runs::RunService,
    SharedState, WorkflowHttpState,
};

/// One observation, so binding installs a real lease rather than a stub.
#[derive(Debug)]
struct Source(PolicyObservation);
impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<PolicyObservation, NativeError>> {
        Box::pin(async move {
            if self.0.app_id() == app {
                Ok(self.0.clone())
            } else {
                Err(NativeError::Denied)
            }
        })
    }
    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, NativeError> {
        Ok(observation.expires_at())
    }
}

pub(crate) struct Fixture {
    pub(crate) platform: platform::Platform,
    pub(crate) state: SharedState,
    pub(crate) scope: AssignedScope,
    pub(crate) issuer: ServiceIssuer,
    pub(crate) key: ServiceSigningKey,
    pub(crate) app: AppId,
    pub(crate) worker: WorkerId,
}

impl Fixture {
    pub(crate) async fn new() -> Self {
        // The case declares an operator zone and places an app in it, so it
        // works in a clone no sibling observes: placement considers every
        // eligible worker in the app's zone, and the zone is deployment-global.
        let platform = platform::Platform::fresh_database().await;
        let (zone, signer) = zone::declare_zone(&platform).await;
        let worker = WorkerId::mint();
        let key = ServiceSigningKey::generate();
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap();
        platform.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault',$5,now() + interval '1 hour')", &[&worker.as_str(), &vec![1_u8], &key.verifying_key_bytes().to_vec(), &signer, &zone.as_str()]).await.unwrap();

        let eligibility = Rc::new(
            connect_eligibility(&platform.runtime_url, Options::default())
                .await
                .unwrap(),
        );
        let service = Coordinator::connect(
            &platform.runtime_url,
            Options::default(),
            holds::client(),
            eligibility,
        )
        .await
        .unwrap();
        service
            .manager
            .register(
                &worker,
                &RegisterWorker {
                    capacity: NonZeroU32::new(1).unwrap(),
                    state: WorkerState::Ready,
                },
            )
            .await
            .unwrap();
        let app = AppId::mint();
        platform.seed_app_in(&app, Some(zone.as_str())).await;
        let Placed::Assigned(assignment) = service.manager.place(&app).await.unwrap() else {
            panic!("the app has one eligible worker");
        };

        let mut peers = ServiceTrustBundle::new();
        peers
            .trust(&issuer, key.key_id(), key.verifying_key_bytes())
            .unwrap();
        let replay = Arc::new(InMemoryReplayStore::default());
        let auth = Arc::new(WorkflowAuth::new(
            Arc::new(ServiceAssertionVerifier::new(peers, replay.clone())),
            Arc::new(PostgresWorkerRegistry::new(Arc::new(
                platform::connect(&platform.runtime_url).await,
            ))),
            replay,
        ));
        let source = Rc::new(Source(
            PolicyObservation::new(
                app.clone(),
                7.try_into().unwrap(),
                AppPolicy::default(),
                zone.clone(),
                false,
                Instant::now() + Duration::from_secs(600),
            )
            .unwrap(),
        ));
        // The production constructor, so a host that dropped the queue binding
        // would fail these cases rather than pass on a fixture that added it
        // back.
        let runs = Rc::new(
            RunService::connect_over(
                &platform.runtime_url,
                &service,
                service.recovery(RecoveryOptions::default()).unwrap(),
                Options::default().startup_timeout(),
            )
            .await
            .unwrap(),
        );
        // The real store, on the platform fixture's own work directory, so a
        // started run's input becomes a real object this case can read back.
        let payloads = ServicePayloads::open(&StorageBackendConfig::Local(
            platform.work.path().join("payloads"),
        ))
        .unwrap();
        let state = Rc::new(WorkflowHttpState {
            service,
            auth,
            policy_source: Some(source as Rc<dyn PolicySource>),
            runs,
            payloads,
        });
        Self {
            platform,
            state,
            scope: AssignedScope {
                app_id: assignment.app_id,
                assignment_revision: assignment.revision,
            },
            issuer,
            key,
            app,
            worker,
        }
    }

    pub(crate) fn authorization(&self) -> String {
        format!(
            "Bearer {}",
            ServiceAssertionMinter::new(self.issuer.clone(), self.key.key_id(), &self.key)
                .unwrap()
                .mint(&ServiceIssuer::parse(AUDIENCE).unwrap())
                .unwrap()
        )
    }

    /// The queued run every case here acts on, seeded by the shared fixture
    /// the wire-pair suite seeds from too.
    pub(crate) async fn seed_run(&self) -> RunId {
        journal::seed_run(&self.platform, &self.app).await
    }

    /// Register the recovery responsibility a platform deployment activation
    /// would have registered.
    ///
    /// `Recovery::establish` refuses an unactivated scope, so without this the
    /// service can fence but cannot get past its own fence. Tests that exercise
    /// acceptance call it; the ones that do not are the control for it.
    pub(crate) async fn ensure_recovery(&self) {
        self.state
            .service
            .recovery(RecoveryOptions::default())
            .unwrap()
            .ensure(&self.app, &DeploymentId::mint(), 1.try_into().unwrap())
            .await
            .unwrap();
    }

    /// A signal over the wire, distinguishable from every other by its type.
    fn signal_request(&self, run: &RunId, signal_type: &str) -> test::TestRequest {
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_SIGNAL.path_template())
            .header("authorization", self.authorization())
            .set_json(&SignalRun {
                request_id: RequestId::mint(),
                scope: self.scope.clone(),
                run_id: run.clone(),
                options: SignalOptions {
                    signal_type: signal_type.to_owned(),
                    payload: serde_json::json!({}),
                },
            })
    }

    /// A restart over the wire, so two arms can differ in the journal alone.
    ///
    /// Every call mints its own request id: a second attempt is a new request
    /// rather than a replay of the first, so the reply it gets is decided again
    /// and not read back out of the request receipt.
    fn restart_request(&self, run: &RunId) -> test::TestRequest {
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_RESTART.path_template())
            .header("authorization", self.authorization())
            .set_json(&RestartRun {
                request_id: RequestId::mint(),
                scope: self.scope.clone(),
                run_id: run.clone(),
                options: RestartOptions::default(),
            })
    }

    /// The ingress epoch the MANAGER holds for this app. Establishment is the
    /// only thing that moves it, so it reports establishments rather than
    /// acceptances.
    async fn manager_epoch(&self) -> i64 {
        self.platform
            .admin
            .query_one(
                "SELECT ingress_epoch FROM workflow_manager.recovery_scopes WHERE id=$1",
                &[&self.app.as_str()],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// The highest epoch the JOURNAL has fenced, which is what acceptance
    /// rechecks a held epoch against.
    async fn closed_epoch(&self) -> i64 {
        self.platform
            .admin
            .query_one(
                "SELECT closed_epoch FROM workflow_manager.__zeroship_workflow_app_state \
                 WHERE app_id=$1",
                &[&self.app.as_str()],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// Abandon this app's recovery scope, the way Control's terminal deletion
    /// does. `lease_epoch_in` then refuses every establishment, which is what
    /// makes "was an epoch already held" observable from outside.
    async fn abandon_scope(&self) {
        let updated = self
            .platform
            .admin
            .execute(
                "UPDATE workflow_manager.recovery_scopes SET state=$2 WHERE id=$1",
                &[&self.app.as_str(), &ScopeState::Abandoned.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(updated, 1, "no recovery scope to abandon");
    }

    /// Fence every epoch up to and including `epoch`, the way a settled Close
    /// does.
    async fn close_epoch(&self, epoch: i64) {
        let updated = self
            .platform
            .admin
            .execute(
                "UPDATE workflow_manager.__zeroship_workflow_app_state SET closed_epoch=$2 \
                 WHERE app_id=$1",
                &[&self.app.as_str(), &epoch],
            )
            .await
            .unwrap();
        assert_eq!(updated, 1, "no app state row to close an epoch in");
    }

    /// Runs of one workflow this app holds, so an absence claim about a refused
    /// start is measured rather than assumed.
    async fn runs_of_workflow(&self, name: &str) -> i64 {
        self.platform
            .admin
            .query_one(
                "SELECT count(*) FROM workflow_manager.__zeroship_workflow_runs \
                 WHERE app_id=$1 AND workflow_name=$2",
                &[&self.app.as_str(), &name],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// Give `run`'s current generation a referenced output object, the shape
    /// `promote` and `attach` leave behind once a completion commits, and return
    /// the payload key.
    ///
    /// Written in SQL because nothing in this process executes a run. The
    /// columns a located read depends on are the ones asserted against the reply,
    /// so a row this fixture wrote differently from production would show up as a
    /// reply that disagrees with the row rather than as a silent pass.
    async fn seed_output_object(&self, run: &RunId, bytes: &[u8]) -> String {
        let payload = zeroship_core::typed_id::generate(
            zeroship_core::typed_id::WORKFLOW_PAYLOAD_PREFIX,
        );
        let edge = zeroship_core::typed_id::generate("wjr");
        let size = i64::try_from(bytes.len()).unwrap();
        let inserted = self.platform.admin.execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_payloads\
             (id,app_id,run_id,generation,request_id,hash,size,content_type,state,created_at,expires_at) \
             VALUES($1,$2,$3,0,$4,encode(sha256($5::bytea),'hex'),$6,'application/json','referenced',0,0)",
            &[
                &payload.as_str(),
                &self.app.as_str(),
                &run.as_str(),
                &RequestId::mint().as_str(),
                &bytes,
                &size,
            ],
        ).await.unwrap();
        assert_eq!(inserted, 1);
        let inserted = self.platform.admin.execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_payload_refs\
             (id,app_id,run_id,generation,slot,ordinal,payload_id) \
             VALUES($1,$2,$3,0,'output',0,$4)",
            &[
                &edge.as_str(),
                &self.app.as_str(),
                &run.as_str(),
                &payload.as_str(),
            ],
        ).await.unwrap();
        assert_eq!(inserted, 1);
        payload
    }

    /// Signals of one type this app holds, so an absence claim is measured
    /// rather than assumed.
    async fn signals_of_type(&self, signal_type: &str) -> i64 {
        self.platform
            .admin
            .query_one(
                "SELECT count(*) FROM workflow_manager.__zeroship_workflow_signals \
                 WHERE app_id=$1 AND signal_type=$2",
                &[&self.app.as_str(), &signal_type],
            )
            .await
            .unwrap()
            .get(0)
    }
}

/// `status` answers over the wire from the service's own journal.
///
/// This is the whole step-3 path in one exchange: an authenticated worker, a
/// placement the manager holds for exactly that worker, an app bound from the
/// service's own registry under a freshly observed policy, and a read of the
/// journal in `workflow_manager` under the grant the migration derived. The
/// reply is compared against what the journal actually holds rather than
/// against a status code.
#[ntex::test]
async fn status_answers_from_the_service_journal() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_STATUS.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RunScope {
                scope: fixture.scope.clone(),
                run_id: run.clone(),
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let status: RunStatus = serde_json::from_slice(&test::read_body(response).await).unwrap();

    // The effect, not the code: the reply must say what the journal says.
    let stored = fixture
        .platform
        .admin
        .query_one(
            "SELECT state FROM workflow_manager.__zeroship_workflow_runs WHERE app_id=$1 AND id=$2",
            &[&fixture.app.as_str(), &run.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>(0), "queued");
    assert_eq!(status.state, RunState::Queued);
    // A run that returned nothing owns no output object and reports none.
    assert!(status.output.is_none(), "{:?}", status.output);
    assert!(status.error.is_none(), "{:?}", status.error);

    // The control, differing in one variable: a run this app does not have.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_STATUS.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RunScope {
                scope: fixture.scope.clone(),
                run_id: RunId::mint(),
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let failure: RunFailure = serde_json::from_slice(&test::read_body(response).await).unwrap();
    assert!(
        matches!(failure, RunFailure::NotFound { .. }),
        "{failure:?}"
    );
}

/// A run call is served for the app the MANAGER holds a placement for, not the
/// one the body names.
///
/// The worker is never read from the body, so the only thing a caller can lie
/// about is the placement revision. A stale one is refused before any journal
/// read, which is what makes the placement lookup an authorization rather than
/// a formality.
#[ntex::test]
async fn a_run_call_without_a_live_placement_is_refused() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    let mut stale = fixture.scope.clone();
    stale.assignment_revision = fixture
        .scope
        .assignment_revision
        .get()
        .checked_add(1)
        .and_then(|next| next.try_into().ok())
        .expect("a revision above the one the manager holds");
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_STATUS.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RunScope {
                scope: stale,
                run_id: run.clone(),
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // The control: the same run, the same credential, the placement the manager
    // actually holds.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_STATUS.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RunScope {
                scope: fixture.scope.clone(),
                run_id: run,
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// The three mutating run calls, and the one journal row that decides the third.
///
/// `signal` and `transition` reach `require_open_epoch`, and the service
/// establishes an epoch to get past it: a first attempt fences,
/// `AppWorkflows::accept` obtains an epoch above the refused one through this
/// service's own `Recovery`, and the retry is served.
///
/// `restart` asks for one thing more, and it is a ROW rather than a capability.
/// It resolves the run's deployment before the fence and ends that resolution in
/// `require_journal_hold`, which needs a `held` intent under
/// `HoldScope::for_app`. `seed_run` already writes the active, available
/// `deploys` row every other check on that path reads, so the two arms below are
/// the same app, the same run, the same credential and the same deployment,
/// differing in that intent alone: refused without it, served with it, and the
/// reply's state and pinned deployment compared against the seeded row.
///
/// Differing in exactly one row is what makes the served arm evidence.
/// `RunFailure::Unavailable {}` names no cause, and `active_deploy`,
/// `exact_target` and the hold all answer with it, so an arm that withheld
/// anything else would refuse for a reason this test could not tell apart.
#[ntex::test]
async fn restart_is_served_only_with_the_journals_deployment_hold() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    // Served by the established epoch alone: these two reach
    // `require_open_epoch` and get past it.
    let accepted: [(&str, serde_json::Value); 2] = [
        (
            endpoints::WORKFLOW_RUN_SIGNAL.path_template(),
            serde_json::to_value(SignalRun {
                request_id: RequestId::mint(),
                scope: fixture.scope.clone(),
                run_id: run.clone(),
                options: SignalOptions {
                    signal_type: "ping".to_owned(),
                    payload: serde_json::json!({}),
                },
            })
            .unwrap(),
        ),
        (
            endpoints::WORKFLOW_RUN_TRANSITION.path_template(),
            serde_json::to_value(TransitionRun {
                request_id: RequestId::mint(),
                scope: fixture.scope.clone(),
                run_id: run.clone(),
                operation: RunOperation::Pause,
            })
            .unwrap(),
        ),
    ];
    assert_eq!(accepted.len(), 2, "an empty sweep is not evidence");
    for (path, body) in accepted {
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(path)
                .header("authorization", fixture.authorization())
                .set_json(&body)
                .to_request(),
        )
        .await;
        let status = response.status();
        let body = test::read_body(response).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path} was not served: {}",
            String::from_utf8_lossy(&body)
        );
    }

    // The control arm: no hold intent exists yet, so restart refuses before the
    // fence, with the epoch above it open and its deployment available.
    let response = test::call_service(&app, fixture.restart_request(&run).to_request()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let failure: RunFailure = serde_json::from_slice(&test::read_body(response).await).unwrap();
    assert!(matches!(failure, RunFailure::Unavailable {}), "{failure:?}");

    // The served arm: the one row added, and the same request again.
    let deploy = run_journal::seed_journal_hold(&fixture.platform, &fixture.app).await;
    let response = test::call_service(&app, fixture.restart_request(&run).to_request()).await;
    let status = response.status();
    let body = test::read_body(response).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let restarted: RestartedRun = serde_json::from_slice(&body).unwrap();
    assert_eq!(restarted.run_id, run.as_str());
    assert_eq!(restarted.state, RunState::Queued);
    assert!(
        restarted.restarted_from_ordinal.is_none(),
        "a restart that named no target retains no prefix: {:?}",
        restarted.restarted_from_ordinal
    );
    assert_eq!(restarted.pinned_to.as_str(), deploy);

    // The effect, not the code: the journal holds the generation the restart
    // opened, pinned to the deployment the reply named.
    let stored = fixture
        .platform
        .admin
        .query_one(
            "SELECT generation,state,deploy_id FROM workflow_manager.__zeroship_workflow_runs \
             WHERE app_id=$1 AND id=$2",
            &[&fixture.app.as_str(), &run.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, i64>(0), 1);
    assert_eq!(stored.get::<_, String>(1), "queued");
    assert_eq!(stored.get::<_, String>(2), deploy);

    // The control for the binding and the placement: the read that needs
    // neither an epoch nor a hold answers throughout, so the arms above are
    // about those prerequisites and not about either of these.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_STATUS.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RunScope {
                scope: fixture.scope.clone(),
                run_id: run,
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// A signal accepted over the wire is in the journal, under the identity the
/// reply named.
///
/// A refusal that stops refusing is not evidence that the write happened, so
/// this compares the reply against a direct read of the row rather than
/// against a status code. The control differs in one variable: the same
/// exchange without an established recovery scope writes no signal at all.
#[ntex::test]
async fn a_signal_served_over_the_wire_is_in_the_journal() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    // The control first, while no responsibility is registered: establishment
    // has nothing to establish against, so acceptance cannot get past the
    // fence and nothing is written.
    let response =
        test::call_service(&app, fixture.signal_request(&run, "before").to_request()).await;
    assert_ne!(
        response.status(),
        StatusCode::OK,
        "a signal was served with no recovery responsibility registered"
    );
    assert_eq!(fixture.signals_of_type("before").await, 0);

    fixture.ensure_recovery().await;
    let response =
        test::call_service(&app, fixture.signal_request(&run, "ping").to_request()).await;
    let status = response.status();
    let body = test::read_body(response).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let delivered: DeliveredSignal = serde_json::from_slice(&body).unwrap();

    // The effect, not the code: the row the reply named must be the one the
    // journal holds, against this run and with this type.
    let stored = fixture
        .platform
        .admin
        .query_one(
            "SELECT run_id,signal_type,origin FROM workflow_manager.__zeroship_workflow_signals \
             WHERE app_id=$1 AND id=$2",
            &[&fixture.app.as_str(), &delivered.id],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>(0), run.as_str());
    assert_eq!(stored.get::<_, String>(1), "ping");
    // The run endpoint delivers through `AppWorkflows::signal`, the platform's
    // own path, not the creator seam in `ingress`; the two record different
    // origins and only this one is reachable over the wire.
    assert_eq!(stored.get::<_, String>(2), "app");
}

/// An established epoch survives the NEXT request's policy reinstall.
///
/// `RunService::app` reinstalls the policy snapshot on every request, and
/// `PolicySnapshot::lease` carries no ingress epoch of its own, so an install
/// that did not carry the held epoch forward would erase it between requests.
///
/// The manager's `ingress_epoch` is NOT the observable, because it cannot tell
/// these two apart: `lease_epoch_in` returns the open epoch unchanged whenever
/// it already exceeds the one the caller names, so a request that lost its
/// epoch and re-established would leave that column exactly where a request
/// that kept its epoch leaves it.
///
/// So the scope is ABANDONED after the first request instead, which makes
/// establishment refuse. A second request that is still served can only have
/// been served on the epoch the first one established, carried across the
/// reinstall. With the carry-forward removed, the second request fences, tries
/// to establish, and the abandoned scope refuses it.
///
/// The third arm is the control, differing in one variable: close that same
/// epoch in the journal, and the carried value stops satisfying the fence. The
/// request is then refused - which is what proves the second arm's success
/// came from the carried epoch, and not from abandonment being inert.
#[ntex::test]
async fn an_established_epoch_survives_the_next_requests_policy_reinstall() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    let response =
        test::call_service(&app, fixture.signal_request(&run, "first").to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let established = fixture.manager_epoch().await;
    assert!(
        established > 0,
        "acceptance did not establish an epoch at all"
    );

    // From here no establishment can succeed, so anything still served is
    // served on the epoch already installed.
    fixture.abandon_scope().await;
    let response =
        test::call_service(&app, fixture.signal_request(&run, "second").to_request()).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the reinstall erased the epoch, so the second request had to establish again"
    );

    // The control: the same request, the same abandoned scope, one variable
    // changed - the journal no longer admits the epoch being carried.
    fixture.close_epoch(established).await;
    let response =
        test::call_service(&app, fixture.signal_request(&run, "third").to_request()).await;
    assert_ne!(
        response.status(),
        StatusCode::OK,
        "an abandoned scope admitted work for a closed epoch, so neither check above constrains anything"
    );
    assert_eq!(fixture.signals_of_type("third").await, 0);
}

/// The carried-forward epoch is a claim rechecked against the journal, never a
/// grant that keeps admitting.
///
/// This is what licenses carrying an epoch across reinstalls at all.
/// `require_open_epoch` reads `closed_epoch` out of app state inside the
/// caller's own transaction and admits only while the held epoch exceeds it,
/// so closing the epoch in the journal makes the carried value stop admitting
/// with no policy reinstall involved. Acceptance then has to establish above
/// the closed one, which is the move this test observes.
///
/// It does NOT assert the endpoint refuses, because it must not: `accept`
/// establishes a newer epoch and retries once, so the self-healing is the
/// correct visible behaviour. The refusal happens and is repaired inside one
/// request, and the epoch having moved is what shows it happened.
#[ntex::test]
async fn closing_the_journals_epoch_retires_the_carried_forward_one() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    let response =
        test::call_service(&app, fixture.signal_request(&run, "before").to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let established = fixture.manager_epoch().await;
    assert!(established > 0, "acceptance did not establish an epoch");
    assert_eq!(
        fixture.closed_epoch().await,
        0,
        "the journal had already closed an epoch before this test closed one"
    );

    // Close exactly the epoch the service holds. The manager issued it, so
    // establishing above it is a legitimate request rather than an invented
    // one.
    fixture.close_epoch(established).await;
    let response =
        test::call_service(&app, fixture.signal_request(&run, "after").to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        fixture.manager_epoch().await > established,
        "the closed epoch kept admitting, so nothing rechecked it"
    );
}

/// `start` admits a run from the creator's VALUE, and the extractor refuses a
/// body that names a payload object.
///
/// The service stages the value into the store it owns and composes the
/// descriptor from the bytes it serialized, so `input_ref` has no field on the
/// wire at all. That is the fence this asserts, at the endpoint rather than at
/// the type: a body carrying `inputRef` beside the creator options is a 400 from
/// the extractor, before any journal work.
///
/// The CONTROL is the same body without that field. It must be SERVED, and its
/// effect measured in the journal: the run row exists, and it owns an input
/// payload edge whose object this service wrote. Without that arm the refusal
/// above would be consistent with a `start` that never worked at all.
#[ntex::test]
async fn start_stages_the_creator_value_and_refuses_a_named_payload_object() {
    let fixture = Box::pin(Fixture::new()).await;
    // For the deploy row `active_deploy` reads. Its manifest declares `demo`.
    let _seeded = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    let body = |options: serde_json::Value| {
        serde_json::json!({
            "requestId": RequestId::mint(),
            "scope": fixture.scope,
            "workflowName": "demo",
            "input": {"order": 7},
            "options": options,
        })
    };
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_START.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&body(serde_json::json!({
                "inputRef": {"hash": "a".repeat(64), "size": 3, "contentType": "application/json"},
            })))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let failure: RunFailure = serde_json::from_slice(&test::read_body(response).await).unwrap();
    assert!(
        matches!(failure, RunFailure::InvalidRequest { .. }),
        "{failure:?}"
    );
    // Nothing was admitted by the refused call.
    assert_eq!(fixture.runs_of_workflow("demo").await, 1, "the seeded run");

    // CONTROL: one variable differs, the descriptor the body named.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_START.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&body(serde_json::json!({"key": "order-7"})))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let started: StartedRun = serde_json::from_slice(&test::read_body(response).await).unwrap();
    assert_eq!(started.state, RunState::Queued);

    // The effect in the journal: the run this service admitted, and the payload
    // object its own staging wrote for the value the body carried. The digest is
    // compared against the bytes rather than against itself, so a descriptor the
    // service composed over anything else fails here.
    let expected = serde_json::to_vec(&serde_json::json!({"order": 7})).unwrap();
    let stored = fixture
        .platform
        .admin
        .query_one(
            "SELECT r.state, p.hash = encode(sha256($3::bytea),'hex'), p.size, p.state FROM \
             workflow_manager.__zeroship_workflow_runs r \
             JOIN workflow_manager.__zeroship_workflow_payload_refs e \
               ON e.app_id=r.app_id AND e.run_id=r.id AND e.slot='input' \
             JOIN workflow_manager.__zeroship_workflow_payloads p \
               ON p.app_id=e.app_id AND p.id=e.payload_id \
             WHERE r.app_id=$1 AND r.id=$2",
            &[&fixture.app.as_str(), &started.id, &expected],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>(0), "queued");
    assert!(
        stored.get::<_, bool>(1),
        "the staged object's digest is not over the value the body carried"
    );
    assert_eq!(
        usize::try_from(stored.get::<_, i64>(2)).unwrap(),
        expected.len()
    );
    assert_eq!(stored.get::<_, String>(3), "referenced");
}

/// An output read answers with what LOCATES the payload, and with no bytes.
///
/// The reply is compared against the journal row it came from: the payload key
/// the object store is addressed by, and the descriptor those bytes must satisfy.
/// Nothing in the reply carries content, which is what lets a caller holding the
/// same store open the object itself on a budget this exchange never has to fit.
///
/// The CONTROL is a run whose generation owns no output edge. It is refused as
/// missing, the same absence `status` reports by carrying no descriptor -- so the
/// located reply above is the edge answering rather than the handler defaulting.
#[ntex::test]
async fn a_run_output_read_locates_the_payload_and_carries_no_bytes() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    let bare = fixture.seed_run().await;
    let bytes = br#"{"value":"final"}"#;
    let payload = fixture.seed_output_object(&run, bytes).await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_OUTPUT.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RunScope {
                scope: fixture.scope.clone(),
                run_id: run,
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = test::read_body(response).await;
    let located: PayloadLocation = serde_json::from_slice(&body).unwrap();
    assert_eq!(located.payload_id, payload);
    assert_eq!(
        located.reference.size,
        i64::try_from(bytes.len()).unwrap(),
        "{located:?}"
    );
    // The bytes are in the store, not in this reply.
    assert!(
        !String::from_utf8_lossy(&body).contains("final"),
        "the located reply carried the payload's content"
    );

    // CONTROL: one variable differs, the output edge the generation owns.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_OUTPUT.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RunScope {
                scope: fixture.scope.clone(),
                run_id: bare,
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // And a step this run never recorded is refused the same way, which is what
    // shows the step endpoint reaches the journal rather than answering by shape.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_STEP_OUTPUT.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&ReadStepOutput {
                scope: fixture.scope.clone(),
                run_id: RunId::parse(&payload_run(&fixture).await).unwrap(),
                name: "charge".to_owned(),
                occurrence: 0,
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The run the fixture seeded first, read back out of the journal so the step
/// arm names a run this app actually holds rather than a minted one.
async fn payload_run(fixture: &Fixture) -> String {
    fixture
        .platform
        .admin
        .query_one(
            "SELECT id FROM workflow_manager.__zeroship_workflow_runs \
             WHERE app_id=$1 ORDER BY id LIMIT 1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0)
}
