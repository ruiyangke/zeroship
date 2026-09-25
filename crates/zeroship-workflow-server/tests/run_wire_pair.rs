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

#[path = "support/app_facts.rs"]
mod app_facts;
#[path = "support/holds.rs"]
mod holds;
#[path = "support/journal.rs"]
mod journal;
#[path = "support/platform.rs"]
mod platform;
#[path = "support/policy.rs"]
mod policy_fixture;
#[allow(
    dead_code,
    reason = "the shared process fixture also supports host failure tests"
)]
#[path = "support/server_process.rs"]
mod server_process;

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
        AssignedScope, RegisterWorker, RequestId, RestartOptions, RestartRun, RunFailure, RunId,
        RunOperation, RunScope, RunState, SignalOptions, SignalRun, TransitionRun, WorkerId,
        WorkerState,
    },
    workflow_jobs::DeploymentId,
    workflow_policy::AppPolicy,
};
use zeroship_workflow_client::{Options as ClientOptions, RunError, WorkerCoordinator};
use zeroship_workflow_manager::recovery::Options as RecoveryOptions;
use zeroship_workflow_server::coordinator::{connect_eligibility, Coordinator, Options};

struct Fixture {
    platform: platform::Platform,
    /// Held for the lifetime of the case: dropping it kills the service.
    _server: server_process::ServerProcess,
    client: WorkerCoordinator,
    scope: AssignedScope,
    app: AppId,
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
            &platform.runtime_url,
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
            &[&worker.as_str(), &vec![1_u8], &public, &platform.default_join_signer_id],
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
        policy_fixture::provision(&platform, &app, &AppPolicy::default()).await;
        let assignment = platform
            .seed_placement(&app, &worker, Duration::from_mins(2))
            .await;
        Self {
            platform,
            _server: server,
            client,
            scope: AssignedScope {
                app_id: assignment.app_id,
                assignment_revision: assignment.revision,
            },
            app,
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
/// `restart_run` IS PINNED AS A REFUSAL, not as a success. `RunService`
/// installs no deployments source, so `restart` resolves a retained deployment
/// it cannot reach and refuses `unavailable` before it ever reaches the ingress
/// fence -- the same split `http_runs.rs` pins in process. The refusal is a
/// wire contract in its own right: the service picks the status from
/// `RunFailure::status`, and the client believes a refusal body only when the
/// status it arrived with is the one that code pairs with. What this arm proves
/// is that the pairing survives the round trip. It flips to a success arm when
/// that deployment source lands.
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
        .expect_err("restart was served, so its deployment source has landed");
    assert_eq!(refused, RunError::Refused(RunFailure::Unavailable {}));
}
