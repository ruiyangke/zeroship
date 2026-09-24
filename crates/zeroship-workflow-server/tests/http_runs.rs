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
        AssignedScope, RegisterWorker, RequestId, RestartOptions, RestartRun, RunFailure, RunId,
        RunOperation, RunScope, RunState, RunStatus, SignalOptions, SignalRun, TransitionRun,
        WorkerId, WorkerState, AUDIENCE,
    },
    workflow_policy::AppPolicy,
};
use zeroship_workflow_manager::{
    coordinator::Placed,
    policy::{PolicyObservation, PolicySource},
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
        let runs = Rc::new(RunService::connect(&platform.runtime_url).await.unwrap());
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
    /// Seeded rather than started, because `start` requires an open ingress
    /// epoch this service does not establish. That makes this the ARRANGE step
    /// for a READ: `status` answers about a run that already exists, so the
    /// path under test is the request, the placement, the binding and the
    /// journal read, none of which this seeding touches. Seeding around a write
    /// would prove nothing about the write, which is why only `status` is
    /// exercised this way.
    async fn seed_run(&self) -> RunId {
        let run = RunId::mint();
        let app = self.app.as_str().to_owned();
        let deploy = "dep_seed0000000000000000000000";
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

/// None of the three mutating run calls can be served yet, and each names what
/// it still needs.
///
/// This is the boundary written where it cannot go stale. `signal` and
/// `transition` reach `require_open_epoch` and refuse as `IngressFenced`: this
/// service installs no ingress epoch on the snapshot it binds an app under, so
/// there is no epoch to exceed. The refusal is retryable by construction and
/// carries the epoch a caller must get above; it is never a durable customer
/// refusal.
///
/// `restart` stops EARLIER, and that is the finding worth pinning. It resolves
/// the run's retained deployment source before it reaches the fence, and this
/// service creates no deployment hold for the runs in its journal, so it
/// refuses as unavailable without ever consulting the epoch. Ingress
/// establishment alone will make two of these three serveable; `restart` needs
/// a deployment source as well.
///
/// The test flips when either prerequisite lands, which is what makes it a
/// handoff rather than a record of a gap.
#[ntex::test]
async fn the_mutating_run_calls_refuse_and_name_what_they_still_need() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;

    // Fenced: these two reach `require_open_epoch` and stop there.
    let fenced: [(&str, serde_json::Value); 2] = [
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
    assert_eq!(fenced.len(), 2, "an empty sweep is not evidence");
    for (path, body) in fenced {
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(path)
                .header("authorization", fixture.authorization())
                .set_json(&body)
                .to_request(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::PRECONDITION_FAILED,
            "{path} did not refuse as fenced"
        );
        let failure: RunFailure = serde_json::from_slice(&test::read_body(response).await).unwrap();
        assert!(
            matches!(failure, RunFailure::IngressFenced { .. }),
            "{path}: {failure:?}"
        );
    }

    // Restart stops at its deployment source, before the fence.
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
    // is unaffected, so these refusals are about those prerequisites and not
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
