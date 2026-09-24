//! The creator-facing run calls, over the wire, against this service's journal.
#![expect(
    clippy::future_not_send,
    reason = "HTTP fixtures use the owning ntex compio runtime"
)]

#[path = "support/holds.rs"]
mod holds;
#[allow(
    dead_code,
    reason = "the shared platform fixture also supports process tests"
)]
#[path = "support/platform.rs"]
mod platform;

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
        AssignedScope, DeliveredSignal, RegisterWorker, RequestId, RestartOptions, RestartRun,
        RunFailure, RunId, RunOperation, RunScope, RunState, RunStatus, SignalOptions, SignalRun,
        TransitionRun, WorkerId, WorkerState, AUDIENCE,
    },
    workflow_jobs::DeploymentId,
    workflow_policy::AppPolicy,
};
use zeroship_workflow_manager::{
    coordinator::Placed,
    policy::{PolicyObservation, PolicySource},
    recovery::{Options as RecoveryOptions, ScopeState},
    Error as NativeError,
};
use zeroship_workflow_server::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    coordinator::{connect_eligibility, Coordinator, Options},
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

struct Fixture {
    platform: platform::Platform,
    state: SharedState,
    scope: AssignedScope,
    issuer: ServiceIssuer,
    key: ServiceSigningKey,
    app: AppId,
}

impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::new().await;
        let worker = WorkerId::mint();
        let key = ServiceSigningKey::generate();
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap();
        platform.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault','ezn_default000000000000000000',now() + interval '1 hour')", &[&worker.as_str(), &vec![1_u8], &key.verifying_key_bytes().to_vec(), &platform.default_join_signer_id]).await.unwrap();

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
        platform.seed_app(&app).await;
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
                Instant::now() + Duration::from_secs(600),
            )
            .unwrap(),
        ));
        let runs = Rc::new(
            RunService::connect(
                &platform.runtime_url,
                service.recovery(RecoveryOptions::default()).unwrap(),
            )
            .await
            .unwrap(),
        );
        let state = Rc::new(WorkflowHttpState {
            service,
            auth,
            policy_source: Some(source as Rc<dyn PolicySource>),
            runs,
            journal: None,
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
        }
    }

    fn authorization(&self) -> String {
        format!(
            "Bearer {}",
            ServiceAssertionMinter::new(self.issuer.clone(), self.key.key_id(), &self.key)
                .unwrap()
                .mint(&ServiceIssuer::parse(AUDIENCE).unwrap())
                .unwrap()
        )
    }

    /// Put a well-formed queued run in the journal.
    ///
    /// This is the ARRANGE step, never the thing under test: every call
    /// exercised here acts on a run that already exists, so the paths under
    /// test are the request, the placement, the binding, the epoch and the
    /// journal read or write, none of which this seeding touches. The writes
    /// this file asserts go through the endpoint, because seeding around a
    /// write would prove nothing about the write.
    async fn seed_run(&self) -> RunId {
        let run = RunId::mint();
        let app = self.app.as_str().to_owned();
        let deploy = "dep_0seed00000000000000000000";
        // The manifest a `DeployRegistration` decodes from. `restart` reads the
        // active deployment before it reaches the fence, so a stub here would
        // refuse as an invalid record rather than as fenced.
        let hash = "a".repeat(64);
        let manifest =
            serde_json::json!({"id": deploy, "hash": hash, "workflows": ["demo"]}).to_string();
        self.platform.admin.execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_app_state(id,app_id) VALUES($1,$1) ON CONFLICT DO NOTHING",
            &[&app],
        ).await.unwrap();
        self.platform.admin.execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_deploys(id,app_id,hash,manifest,created_at,active,state,availability_epoch) \
             VALUES($1,$2,$4,$3,0,1,'available',0) ON CONFLICT DO NOTHING",
            &[&deploy, &app, &manifest, &hash],
        ).await.unwrap();
        self.platform.admin.execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_runs(id,app_id,workflow_name,deploy_id,generation,state,control,due_at,lease_epoch,cascade,depth,created_at,signal_epoch) \
             VALUES($1,$2,'demo',$3,0,'queued','none',0,0,0,0,0,0)",
            &[&run.as_str(), &app, &deploy],
        ).await.unwrap();
        self.platform.admin.execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_generations(id,app_id,run_id,generation,deploy_id,state,started_at) \
             VALUES($1,$2,$3,0,$4,'queued',0)",
            &[&format!("gen_{}", run.as_str()), &app, &run.as_str(), &deploy],
        ).await.unwrap();
        run
    }

    /// Register the recovery responsibility a platform deployment activation
    /// would have registered.
    ///
    /// `Recovery::establish` refuses an unactivated scope, so without this the
    /// service can fence but cannot get past its own fence. Tests that exercise
    /// acceptance call it; the ones that do not are the control for it.
    async fn ensure_recovery(&self) {
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

/// The three mutating run calls split on what each of them needs.
///
/// This is the boundary written where it cannot go stale. `signal` and
/// `transition` reach `require_open_epoch`, and the service now establishes an
/// epoch to get past it: a first attempt fences, `AppWorkflows::accept`
/// obtains an epoch above the refused one through this service's own
/// `Recovery`, and the retry is served. Both therefore answer OK.
///
/// `restart` stops EARLIER, and that is what this test pins. It resolves the
/// run's retained deployment source before it reaches the fence, and this
/// service creates no deployment hold for the runs in its journal, so it
/// refuses as unavailable without ever consulting the epoch. Establishing
/// ingress made two of these three serveable and cannot make the third: the
/// refusal that remains is a different prerequisite, not a lesser degree of
/// the same one.
///
/// The test flips again when that deployment source lands, which is what makes
/// it a handoff rather than a record of a gap.
#[ntex::test]
async fn ingress_serves_signal_and_transition_while_restart_still_lacks_its_source() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    // Served: these two reach `require_open_epoch` and now get past it.
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

    // Restart stops at its deployment source, before the fence, so the epoch
    // this test established does not reach it.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_RUN_RESTART.path_template())
            .header("authorization", fixture.authorization())
            .set_json(&RestartRun {
                request_id: RequestId::mint(),
                scope: fixture.scope.clone(),
                run_id: run.clone(),
                options: RestartOptions::default(),
            })
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let failure: RunFailure = serde_json::from_slice(&test::read_body(response).await).unwrap();
    assert!(matches!(failure, RunFailure::Unavailable {}), "{failure:?}");

    // The control: the read that needs neither an epoch nor a deployment source
    // is unaffected, so the split above is about those prerequisites and not
    // about the binding or the placement.
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
