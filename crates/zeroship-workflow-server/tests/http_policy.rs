//! Policy leases bind enrolled keys and placement without reading creator storage.
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

use futures::{
    channel::oneshot,
    future::{select, Either, LocalBoxFuture},
    FutureExt,
};
use ntex::{
    http::StatusCode,
    web::{self, test},
};
use std::{
    cell::{Cell, RefCell},
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
    service_peers::{service_issuer, WORKER_SERVICE_NAME},
    workflow_coordination::{
        AssignScope, AssignedScope, Failure, FailureCode, RegisterWorker, RequestId, WorkerId,
        WorkerState, AUDIENCE,
    },
    workflow_policy::{AppPolicy, EstablishIngress, PolicyLease, PolicyLeaseRequest},
};
use zeroship_workflow_manager::{
    policy::{PolicyObservation, PolicySource},
    Error as NativeError,
};
use zeroship_workflow_server::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    coordinator::{connect_eligibility, Coordinator, Options},
    SharedState, WorkflowHttpState,
};

struct Signer {
    issuer: ServiceIssuer,
    key: ServiceSigningKey,
}
impl Signer {
    fn authorization(&self) -> String {
        format!(
            "Bearer {}",
            ServiceAssertionMinter::new(self.issuer.clone(), self.key.key_id(), &self.key)
                .unwrap()
                .mint(&ServiceIssuer::parse(AUDIENCE).unwrap())
                .unwrap()
        )
    }
}

#[derive(Debug)]
struct SourceGate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
#[derive(Debug)]
struct Source {
    observations: [PolicyObservation; 2],
    available: Cell<bool>,
    observed: Cell<usize>,
    gate: RefCell<Option<SourceGate>>,
}
impl Source {
    fn arm(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, observed) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        assert!(self
            .gate
            .replace(Some(SourceGate {
                entered,
                release: blocked
            }))
            .is_none());
        (observed, release)
    }
}
impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<PolicyObservation, NativeError>> {
        Box::pin(async move {
            self.observed.set(self.observed.get() + 1);
            if !self.available.get() {
                return Err(NativeError::Unavailable);
            }
            let observation = self
                .observations
                .iter()
                .find(|observation| observation.app_id() == app)
                .cloned()
                .ok_or(NativeError::Unavailable)?;
            let gate = self.gate.borrow_mut().take();
            if let Some(gate) = gate {
                gate.entered.send(()).unwrap();
                gate.release.await.unwrap();
            }
            Ok(observation)
        })
    }
    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, NativeError> {
        let current = self
            .observations
            .iter()
            .find(|current| current.same_observation(observation))
            .ok_or(NativeError::Unavailable)?;
        if !self.available.get() || current.expires_at() <= Instant::now() {
            return Err(NativeError::Unavailable);
        }
        Ok(current.expires_at())
    }
}

struct Fixture {
    platform: platform::Platform,
    worker: WorkerId,
    signer: Signer,
    role: Signer,
    source: Rc<Source>,
    state: SharedState,
    scope: AssignedScope,
}
impl Fixture {
    async fn new(policy: AppPolicy, configured: bool) -> Self {
        let platform = platform::Platform::new().await;
        let worker = WorkerId::mint();
        let signer = Signer {
            issuer: ServiceIssuer::parse(&format!(
                "spiffe://zeroship.ai/svc/worker/{}",
                worker.as_str()
            ))
            .unwrap(),
            key: ServiceSigningKey::generate(),
        };
        platform.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,enroller_id) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4)", &[&worker.as_str(), &vec![1_u8], &signer.key.verifying_key_bytes().to_vec(), &platform.default_enroller_id]).await.unwrap();
        // Production eligibility: Control's own rows under the manager's grants.
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
        let assignment = service
            .manager
            .assign(&AssignScope {
                request_id: RequestId::mint(),
                app_id: app.clone(),
                worker_id: worker.clone(),
                expected_revision: None,
            })
            .await
            .unwrap();
        let expires = Instant::now() + Duration::from_secs(60);
        let source = Rc::new(Source {
            observations: [
                PolicyObservation::new(app, 7.try_into().unwrap(), policy.clone(), expires)
                    .unwrap(),
                PolicyObservation::new(AppId::mint(), 7.try_into().unwrap(), policy, expires)
                    .unwrap(),
            ],
            available: Cell::new(true),
            observed: Cell::new(0),
            gate: RefCell::new(None),
        });
        let role = Signer {
            issuer: service_issuer(WORKER_SERVICE_NAME).unwrap(),
            key: ServiceSigningKey::generate(),
        };
        let mut peers = ServiceTrustBundle::new();
        peers
            .trust(
                &role.issuer,
                role.key.key_id(),
                role.key.verifying_key_bytes(),
            )
            .unwrap();
        let replay = Arc::new(InMemoryReplayStore::default());
        let auth = Arc::new(WorkflowAuth::new(
            Arc::new(ServiceAssertionVerifier::new(peers, replay.clone())),
            Arc::new(PostgresWorkerRegistry::new(Arc::new(
                platform::connect(&platform.runtime_url).await,
            ))),
            replay,
        ));
        let state = Rc::new(WorkflowHttpState {
            service,
            auth,
            policy_source: configured.then(|| source.clone() as Rc<dyn PolicySource>),
        });
        Self {
            platform,
            worker,
            signer,
            role,
            source,
            state,
            scope: AssignedScope {
                app_id: assignment.app_id,
                assignment_revision: assignment.revision,
            },
        }
    }
    fn request(&self) -> test::TestRequest {
        test::TestRequest::post()
            .uri(endpoints::WORKFLOW_POLICY_LEASE.path_template())
            .header("authorization", self.signer.authorization())
            .set_json(&plain(&self.scope))
    }
    async fn replace_key(&self, public: [u8; 32]) {
        // Enrollment freezes keys; model an administrator replacing the enrolled
        // row without bypassing its production immutability trigger.
        assert_eq!(self.platform.admin.execute("WITH previous AS (DELETE FROM zeroship.worker_instances WHERE id=$1 RETURNING id,ring_key,advertise_host,advertise_port,registered_at,enroller_id) INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,registered_at,status,enroller_id) SELECT id,ring_key,$2,advertise_host,advertise_port,registered_at,'active',enroller_id FROM previous", &[&self.worker.as_str(), &public.to_vec()]).await.unwrap(), 1);
    }
}

#[ntex::test]
async fn policy_route_requires_enrollment_before_parsing_body() {
    let fixture = Box::pin(Fixture::new(AppPolicy::default(), true)).await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;
    for authorization in [None, Some(fixture.role.authorization())] {
        let mut request = test::TestRequest::post()
            .uri(endpoints::WORKFLOW_POLICY_LEASE.path_template())
            .header("content-type", "application/json")
            .set_payload("{");
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        let response = test::call_service(&app, request.to_request()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let failure: Failure = serde_json::from_slice(&test::read_body(response).await).unwrap();
        assert_eq!(failure.code, FailureCode::Unauthenticated);
    }
    let response = test::call_service(&app, fixture.request().set_payload("{").to_request()).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.source.observed.get(), 0);
}

#[ntex::test]
async fn policy_route_returns_complete_disabled_policy_and_exact_authority_tuple() {
    let policy = AppPolicy {
        admission: false,
        dispatch: false,
        ingress: false,
        ..AppPolicy::default()
    };
    let fixture = Box::pin(Fixture::new(policy.clone(), true)).await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;
    let response = test::call_service(&app, fixture.request().to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let lease: PolicyLease = serde_json::from_slice(&test::read_body(response).await).unwrap();
    assert_eq!(lease.app_id, fixture.scope.app_id);
    assert_eq!(lease.worker_id, fixture.worker);
    assert_eq!(lease.signing_key_id, fixture.signer.key.key_id());
    assert_eq!(lease.assignment_revision, fixture.scope.assignment_revision);
    assert_eq!(
        lease.policy_revision,
        fixture.source.observations[0].revision()
    );
    assert_eq!(lease.policy, policy);
    assert!(u128::from(lease.remaining_ms.get()) <= Options::default().assignment_ttl.as_millis());
    assert_eq!(fixture.source.observed.get(), 1);
    for scope in [
        AssignedScope {
            app_id: fixture.source.observations[1].app_id().clone(),
            ..fixture.scope.clone()
        },
        AssignedScope {
            assignment_revision: 2.try_into().unwrap(),
            ..fixture.scope.clone()
        },
    ] {
        let response =
            test::call_service(&app, fixture.request().set_json(&plain(&scope)).to_request()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let failure: Failure = serde_json::from_slice(&test::read_body(response).await).unwrap();
        assert_eq!(failure.code, FailureCode::Denied);
        assert_eq!(
            fixture.source.observed.get(),
            1,
            "invalid placement must not consult the policy source"
        );
    }
}

#[ntex::test]
async fn policy_route_has_no_fallback_for_missing_or_unavailable_source() {
    for configured in [false, true] {
        let fixture = Box::pin(Fixture::new(AppPolicy::default(), configured)).await;
        fixture.source.available.set(false);
        let app = test::init_service(
            web::App::new()
                .state(fixture.state.clone())
                .configure(zeroship_workflow_server::configure),
        )
        .await;
        let response = test::call_service(&app, fixture.request().to_request()).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let failure: Failure = serde_json::from_slice(&test::read_body(response).await).unwrap();
        assert_eq!(failure.code, FailureCode::Unavailable);
        assert_eq!(fixture.source.observed.get(), usize::from(configured));
    }
}

#[ntex::test]
async fn enrollment_changes_while_policy_source_waits_refuse_the_old_signer() {
    let fixture = Box::pin(Fixture::new(AppPolicy::default(), true)).await;
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;
    for replace in [false, true] {
        let (observed, release) = fixture.source.arm();
        let request = test::call_service(&app, fixture.request().to_request()).boxed_local();
        let pending = match select(observed, request).await {
            Either::Left((observed, pending)) => {
                observed.unwrap();
                pending
            }
            Either::Right((response, _)) => panic!(
                "policy request bypassed its source barrier: {}",
                response.status()
            ),
        };
        if replace {
            fixture
                .replace_key(ServiceSigningKey::generate().verifying_key_bytes())
                .await;
        } else {
            fixture
                .platform
                .admin
                .execute(
                    "UPDATE zeroship.worker_instances SET status='draining' WHERE id=$1",
                    &[&fixture.worker.as_str()],
                )
                .await
                .unwrap();
        }
        release.send(()).unwrap();
        let response = compio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let failure: Failure = serde_json::from_slice(&test::read_body(response).await).unwrap();
        assert_eq!(failure.code, FailureCode::Denied);
        fixture
            .replace_key(fixture.signer.key.verifying_key_bytes())
            .await;
        let response = test::call_service(&app, fixture.request().to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let lease: PolicyLease = serde_json::from_slice(&test::read_body(response).await).unwrap();
        assert_eq!(lease.signing_key_id, fixture.signer.key.key_id());
    }
}

/// A plain policy refresh, which never establishes responsibility.
fn plain(scope: &AssignedScope) -> PolicyLeaseRequest {
    PolicyLeaseRequest {
        scope: scope.clone(),
        establish: None,
        ingress_used: false,
    }
}

/// Establishment through the authenticated route commits responsibility before
/// the lease is returned: a plain refresh reports the open epoch, establishing
/// after it opens the next one, a retry returns that same epoch, and an epoch
/// the manager never issued is refused.
#[ntex::test]
async fn policy_route_establishes_an_epoch_above_the_named_one() {
    use zeroship_core::{schema_name::SchemaName, workflow_jobs::DeploymentId};
    let fixture = Box::pin(Fixture::new(AppPolicy::default(), true)).await;
    let queue = zeroship_workflow_manager::Queue::connect(
        zeroship_data_orm::binding::DbBinding::new(
            "workflow_manager",
            "workflow_manager",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        &fixture.platform.runtime_url,
        zeroship_workflow_manager::Options::default(),
        holds::client(),
    )
    .await
    .unwrap();
    zeroship_workflow_manager::recovery::Recovery::new(
        queue,
        zeroship_workflow_manager::recovery::Options::default(),
    )
        .unwrap()
        .ensure(
            &fixture.scope.app_id,
            &DeploymentId::mint(),
            1.try_into().unwrap(),
        )
        .await
        .unwrap();
    let app = test::init_service(
        web::App::new()
            .state(fixture.state.clone())
            .configure(zeroship_workflow_server::configure),
    )
    .await;
    // `None` is a plain refresh; `Some(0)` names no refused epoch, as at startup.
    let exchange = async |after: Option<i64>| {
        let body = PolicyLeaseRequest {
            establish: after.map(|epoch| EstablishIngress {
                after: (epoch > 0).then(|| epoch.try_into().unwrap()),
            }),
            ingress_used: after.is_some(),
            ..plain(&fixture.scope)
        };
        let response =
            test::call_service(&app, fixture.request().set_json(&body).to_request()).await;
        let status = response.status();
        let body = test::read_body(response).await;
        if status == StatusCode::OK {
            let lease: PolicyLease = serde_json::from_slice(&body).unwrap();
            Ok(lease
                .ingress_epoch
                .map(zeroship_core::workflow_coordination::Revision::get))
        } else {
            let failure: Failure = serde_json::from_slice(&body).unwrap();
            Err((status, failure.code))
        }
    };
    assert_eq!(exchange(None).await, Ok(Some(1)));
    assert_eq!(exchange(Some(0)).await, Ok(Some(1)), "startup keeps the open epoch");
    assert_eq!(exchange(Some(1)).await, Ok(Some(2)));
    assert_eq!(exchange(Some(1)).await, Ok(Some(2)));
    assert_eq!(exchange(None).await, Ok(Some(2)));
    assert_eq!(
        exchange(Some(5)).await,
        Err((StatusCode::CONFLICT, FailureCode::Conflict))
    );
}
