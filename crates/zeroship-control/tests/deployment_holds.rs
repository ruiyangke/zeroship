#![recursion_limit = "256"]
#![allow(
    clippy::future_not_send,
    reason = "native test services own their compio connections"
)]

#[path = "../../zeroship-workflow-server/tests/support/platform.rs"]
mod platform;

#[path = "common/deployments.rs"]
mod deployment_commands;
#[path = "deployment_holds/queue.rs"]
mod queue_holds;
#[path = "deployment_holds/collector.rs"]
mod collector;
#[path = "deployment_holds/publication.rs"]
mod publication;

use ntex::{
    client::Client,
    http::StatusCode,
    web::{
        self, test,
        types::{Json, State},
    },
};
use serde_json::{Value, json};
use std::{
    num::NonZeroU32,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroship_authn::service_replay::SharedClientReplayStore;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore, LocalWorkflowBlobStore, WorkflowBlobStore};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
    deployment_hold_api::{self, DeploymentHoldApi},
};
use zeroship_core::{
    app_id::AppId,
    organization_id::OrganizationId,
    project_id::ProjectId,
    schema_name::SchemaName,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{ServiceEndpoint, endpoints, verify_service_call},
    service_peers::{
        CONTROL_SERVICE_NAME, ServiceAuth, ServiceKeyring, WORKER_SERVICE_NAME,
        WORKFLOW_SERVICE_NAME, service_issuer, worker_enroller_issuer,
    },
    typed_id,
    workflow_coordination::{
        AUDIENCE, AssignScope, Assignment, Failure, FailureCode, RegisterWorker, RequestId,
        VerifyAssignment, WorkerId, WorkerState,
    },
};
use zeroship_data_orm::{
    ConnectOptions, binding::DbBinding, encryption::ProjectKeySource, orm::Database,
};
use zeroship_workflow::{
    WorkflowServiceError,
    deployment_holds::{
        DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldRequest, HoldScope, HoldState,
        RemoteDeploymentHolds,
    },
};
use zeroship_workflow_client::{
    ControlCoordinator, Error as CoordinationError, Options, WorkerCoordinator,
};
use zeroship_workflow_server::{
    WorkflowHttpState,
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    coordinator::{Coordinator, Options as CoordinatorOptions},
};

fn signer(issuer: ServiceIssuer, key: ServiceSigningKey) -> Arc<ServiceAuth> {
    Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(issuer, key, ServiceTrustBundle::new()).unwrap(),
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}

fn origin(server: &test::TestServer) -> String {
    format!("http://{}/", server.addr())
}

struct Fixture {
    platform: platform::Platform,
    state: Arc<AppState>,
    /// The deployment unit's enroller: the only credential that may enrol a
    /// worker instance, recorded in `zeroship.worker_enrollers` as the
    /// operator's import would leave it.
    enroller: Arc<ServiceAuth>,
    /// A bare `svc/worker` ROLE key that Control's peer bundle still trusts:
    /// the stale shared credential no process holds any more. Every endpoint
    /// here must refuse it at role arity.
    worker_role: Arc<ServiceAuth>,
    workflow_role: Arc<ServiceAuth>,
    control_url: String,
}

impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::new().await;
        let control_url =
            platform
                .runtime_url
                .replacen("zeroship_workflow@", "zeroship_control@", 1);
        let registry = Registry::new(&control_url).await.unwrap();
        let control_pg = Arc::new(platform::connect(&control_url).await);
        let worker_role = signer(
            service_issuer(WORKER_SERVICE_NAME).unwrap(),
            ServiceSigningKey::generate(),
        );
        let workflow_role = signer(
            service_issuer(WORKFLOW_SERVICE_NAME).unwrap(),
            ServiceSigningKey::generate(),
        );
        let (worker_issuer, worker_key) = worker_role.signing_identity().unwrap();
        let mut peers = ServiceTrustBundle::new();
        peers
            .trust_signing_key(worker_issuer, worker_key.key_id(), worker_key)
            .unwrap();
        let (workflow_issuer, workflow_key) = workflow_role.signing_identity().unwrap();
        peers
            .trust_signing_key(workflow_issuer, workflow_key.key_id(), workflow_key)
            .unwrap();
        let service_auth = Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(
                service_issuer(CONTROL_SERVICE_NAME).unwrap(),
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(ServiceAssertionVerifier::new(
                peers,
                Arc::new(SharedClientReplayStore::new(control_pg.clone())),
            )),
        ));
        let blob_root = platform.work.path().join("blobs");
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).unwrap());
        let workflow_blob_store: Arc<dyn WorkflowBlobStore> =
            Arc::new(LocalWorkflowBlobStore::new(blob_root).unwrap());
        let state = Arc::new(AppState {
            service_auth,
            env_store: EnvStore::new(registry.clone(), "deployment-hold-test-master-key").unwrap(),
            stripe_store: StripeStore::new(registry.clone()),
            registry,
            blob_store,
            workflow_blob_store,
            control_key: SecretString::new("deployment-hold-test-control-key".into()),
            master_key: SecretString::new("deployment-hold-test-master-key".into()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".into(),
            gateway_url: "http://127.0.0.1:1".into(),
            worker_urls: Vec::new(),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(100, 10))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(100, 10))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            worker_enrolment: zeroship_control::worker_enrolment::EnrolmentEnvelope::parse(
                "127.0.0.0/8",
                "8080",
                false,
            )
            .unwrap(),
            deploy_tmp_dir: platform.work.path().join("deploy"),
            control_pg,
            app_base_domain: "zeroship.localhost".into(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".into(),
            static_policies: zeroship_authz::load_platform_policies().unwrap(),
            auth_provider: zeroship_control::platform_auth_provider(
                "https://auth.zeroship.test/oauth2",
                None,
            ),
            provider_registry: zeroship_control::metering::provider::builtin_registry(),
            billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
            billing_stream: None,
            tax_provider: zeroship_control::tax::build_tax_provider(
                &zeroship_control::tax::TaxProviderConfig::native(),
            )
            .unwrap(),
            notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
            mailer: Arc::new(zeroship_mailer::RecordingMailer::new()),
            pairwise_salt: [0; 32],
            projected_charge_cache: Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        });
        zeroship_control::plan_catalog::seed_plans(&state.registry)
            .await
            .unwrap();
        let enroller_key = ServiceSigningKey::generate();
        let enroller_id = typed_id::new_worker_enroller_id();
        let inserted = platform
            .admin
            .execute(
                "INSERT INTO zeroship.worker_enrollers (id, public_key, execution_zone_id, status) \
                 VALUES ($1, $2, 'ezn_default000000000000000000', 'active')",
                &[&enroller_id, &enroller_key.verifying_key_bytes().to_vec()],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        let enroller = signer(worker_enroller_issuer(&enroller_id).unwrap(), enroller_key);
        Self {
            platform,
            state,
            enroller,
            worker_role,
            workflow_role,
            control_url,
        }
    }

    async fn deployment(&self, name: &str) -> (AppId, String, String) {
        let organization = OrganizationId::mint();
        let project = ProjectId::mint();
        let app = AppId::mint();
        let email = format!("{name}@zeroship.test");
        let inserted = self
            .platform
            .admin
            .execute(
                "INSERT INTO zeroship.organizations(id,slug,name,billing_email) \
                 VALUES($1,$2::citext,$2,$3::citext)",
                &[&organization.as_str(), &name, &email],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        let inserted = self
            .platform
            .admin
            .execute(
                "INSERT INTO zeroship.projects(id,organization_id,slug,name) \
                 VALUES($1,$2,'default','Deployment Hold Project')",
                &[&project.as_str(), &organization.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        let inserted = self
            .platform
            .admin
            .execute(
                "INSERT INTO zeroship.apps(id,name,plan_id,project_id,organization_id) \
                 VALUES($1,$2,$3,$4,$5)",
                &[
                    &app.as_str(),
                    &name,
                    &zeroship_control::plan_catalog::free_plan_id(),
                    &project.as_str(),
                    &organization.as_str(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        let deployment = typed_id::generate("dep");
        let mut manifest = zeroship_bundle::Manifest::default();
        let hash =
            zeroship_bundle::deployment_manifest_hash(&serde_json::to_vec(&manifest).unwrap())
                .unwrap();
        manifest.deploy_hash = Some(hash.clone());
        let manifest_json = serde_json::to_string(&manifest).unwrap();
        let inserted = self
            .platform
            .admin
            .execute(
                "INSERT INTO zeroship.app_deploys(id,app_id,deploy_hash,manifest_json) \
                 VALUES($1,$2,$3,$4)",
                &[&deployment, &app.as_str(), &hash, &manifest_json],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        (app, deployment, hash)
    }

    /// A platform user that can author deploy commands.
    async fn actor(&self) -> zeroship_core::UserId {
        let actor = zeroship_core::UserId::mint();
        let email = format!("{}@zeroship.test", actor.as_str());
        let inserted = self
            .platform
            .admin
            .execute(
                "INSERT INTO zeroship.users(id,email,name) VALUES($1,$2::citext,'Deploy actor')",
                &[&actor.as_str(), &email],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        actor
    }

    async fn coordinator(&self) -> test::TestServer {
        let url = self.platform.runtime_url.clone();
        let catalog_url = self.control_url.clone();
        let control = self.state.service_auth.clone();
        test::server(move || {
            let url = url.clone();
            let catalog_url = catalog_url.clone();
            let control = control.clone();
            async move {
                let registry = Arc::new(platform::connect(&url).await);
                let replay = Arc::new(SharedClientReplayStore::new(registry.clone()));
                let (issuer, key) = control.signing_identity().unwrap();
                let mut peers = ServiceTrustBundle::new();
                peers.trust_signing_key(issuer, key.key_id(), key).unwrap();
                let state = Rc::new(WorkflowHttpState {
                    policy_source: None,
                    service: Coordinator::connect(
                        &url,
                        CoordinatorOptions {
                            worker_ttl: Duration::from_secs(120),
                            assignment_ttl: Duration::from_secs(120),
                            ..CoordinatorOptions::default()
                        },
                        Rc::new(zeroship_workflow_manager::retention::CatalogClient::new(
                            zeroship_workflow_manager::deployments::DeploymentHolds::new(
                                database(&catalog_url).await,
                            )
                            .unwrap(),
                        )),
                    )
                    .await
                    .unwrap(),
                    auth: Arc::new(WorkflowAuth::new(
                        Arc::new(ServiceAssertionVerifier::new(peers, replay.clone())),
                        Arc::new(PostgresWorkerRegistry::new(registry)),
                        replay,
                    )),
                });
                web::App::new()
                    .state(state)
                    .configure(zeroship_workflow_server::configure)
            }
        })
        .await
    }

    async fn control(&self, coordinator_url: String) -> test::TestServer {
        let state = self.state.clone();
        let url = self.control_url.clone();
        test::server(move || {
            let state = state.clone();
            let url = url.clone();
            let coordinator_url = coordinator_url.clone();
            async move {
                let coordinator = ControlCoordinator::new(
                    &coordinator_url,
                    state.service_auth.clone(),
                    Options::default(),
                )
                .unwrap();
                let api =
                    Rc::new(DeploymentHoldApi::new(database(&url).await, coordinator).unwrap());
                web::App::new()
                    .state(state)
                    .state(api)
                    .service(
                        web::resource(endpoints::CONTROL_WORKER_ENROL.path_template()).route(
                            web::post().to(zeroship_control::internal::enrol_worker_instance),
                        ),
                    )
                    .configure(deployment_hold_api::configure)
            }
        })
        .await
    }

    async fn enrolled_worker(
        &self,
        http: &Client,
        control_url: &str,
    ) -> (WorkerId, Arc<ServiceAuth>) {
        let key = ServiceSigningKey::generate();
        let token = control_header(&self.enroller);
        let (status, response) = post(
            http,
            control_url,
            endpoints::CONTROL_WORKER_ENROL,
            Some(&token),
            &json!({"port":8080,"public_key":key.public_jwk_x()}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let worker = WorkerId::parse(response["instance_id"].as_str().unwrap()).unwrap();
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap();
        (worker, signer(issuer, key))
    }

    async fn rows(&self) -> Vec<(String, String, i64, String)> {
        self.platform
            .admin
            .query(
                "SELECT id,holder_id,generation,state FROM zeroship.app_deploy_holds ORDER BY id",
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.get("id"),
                    row.get("holder_id"),
                    row.get("generation"),
                    row.get("state"),
                )
            })
            .collect()
    }
}

async fn database(url: &str) -> Database {
    Database::connect(
        DbBinding::new(
            "platform",
            "control-deployments",
            SchemaName::new("zeroship").unwrap(),
        ),
        ConnectOptions::new(url.to_owned(), ProjectKeySource::unavailable()).connection_authority(),
        zeroship_workflow_manager::deployments::collections().unwrap(),
    )
    .await
    .unwrap()
}

fn control_header(auth: &ServiceAuth) -> String {
    auth.authorization_for(&service_issuer(CONTROL_SERVICE_NAME).unwrap())
        .unwrap()
}

async fn post(
    http: &Client,
    url: &str,
    endpoint: ServiceEndpoint,
    token: Option<&str>,
    body: &Value,
) -> (StatusCode, Value) {
    compio::time::timeout(Duration::from_secs(10), async {
        let mut request = http.post(format!(
            "{}{}",
            url.trim_end_matches('/'),
            endpoint.path_template()
        ));
        if let Some(token) = token {
            request = request.header("authorization", token);
        }
        let response = request.send_json(body).await.unwrap();
        let status = response.status();
        let body = serde_json::from_slice(&response.body().await.unwrap()).unwrap();
        (status, body)
    })
    .await
    .expect("deployment hold HTTP exchange completed")
}

fn generation(value: i64) -> HoldGeneration {
    value.try_into().unwrap()
}

async fn placement(control: &ControlCoordinator, worker: &WorkerId, app: &AppId) -> Assignment {
    control
        .assign(&AssignScope {
            request_id: RequestId::mint(),
            app_id: app.clone(),
            worker_id: worker.clone(),
            expected_revision: None,
        })
        .await
        .unwrap()
}

#[ntex::test]
async fn signed_deployment_holds_preserve_app_scope_across_worker_replacement() {
    let fixture = Fixture::new().await;
    let (app, deploy, hash) = fixture.deployment("holds-owner").await;
    let (foreign, foreign_deploy, _) = fixture.deployment("holds-foreign").await;
    let coordinator_server = fixture.coordinator().await;
    let control_server = fixture.control(origin(&coordinator_server)).await;
    let coordinator = ControlCoordinator::new(
        &origin(&coordinator_server),
        fixture.state.service_auth.clone(),
        Options::default(),
    )
    .unwrap();
    let http = Client::new().await;
    let (worker, auth) = fixture
        .enrolled_worker(&http, &origin(&control_server))
        .await;
    let (replacement, replacement_auth) = fixture
        .enrolled_worker(&http, &origin(&control_server))
        .await;
    let registration = RegisterWorker {
        capacity: NonZeroU32::new(3).unwrap(),
        state: WorkerState::Ready,
    };
    for auth in [&auth, &replacement_auth] {
        WorkerCoordinator::new(
            &origin(&coordinator_server),
            auth.clone(),
            Options::default(),
        )
        .unwrap()
        .register(&registration)
        .await
        .unwrap();
    }
    let assignment = placement(&coordinator, &worker, &app).await;
    let request = HoldRequest {
        app_id: app.clone(),
        assignment_revision: assignment.revision,
        deploy_id: deploy.clone(),
        generation: generation(1),
    };
    let body = serde_json::to_value(&request).unwrap();
    let endpoint = endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE;
    let fake = signer(
        ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            WorkerId::mint().as_str()
        ))
        .unwrap(),
        ServiceSigningKey::generate(),
    );
    let wrong_key = signer(
        auth.signing_identity().unwrap().0.clone(),
        ServiceSigningKey::generate(),
    );
    for token in [
        None,
        Some(control_header(&fixture.worker_role)),
        Some(control_header(&fixture.state.service_auth)),
        Some(control_header(&fake)),
        Some(control_header(&wrong_key)),
    ] {
        let (status, failure) = post(
            &http,
            &origin(&control_server),
            endpoint,
            token.as_deref(),
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(failure, json!({"code":"unauthenticated"}));
    }
    for (auth, changed) in [
        (&replacement_auth, body.clone()),
        (
            &auth,
            json!({"appId":foreign,"assignmentRevision":assignment.revision,"deployId":foreign_deploy,"generation":1}),
        ),
        (
            &auth,
            json!({"appId":app,"assignmentRevision":assignment.revision.get()+1,"deployId":deploy,"generation":1}),
        ),
        (
            &auth,
            json!({"appId":app,"assignmentRevision":assignment.revision,"deployId":foreign_deploy,"generation":1}),
        ),
    ] {
        let token = control_header(auth);
        let (status, failure) = post(
            &http,
            &origin(&control_server),
            endpoint,
            Some(&token),
            &changed,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(failure, json!({"code":"denied"}));
    }
    for (field, value) in [
        ("holderId", json!(typed_id::generate("dhl"))),
        ("workerId", json!(worker)),
        ("generation", json!(0)),
        ("databaseUrl", json!("customer-secret")),
    ] {
        let mut invalid = body.clone();
        invalid[field] = value;
        let token = control_header(&auth);
        let (status, failure) = post(
            &http,
            &origin(&control_server),
            endpoint,
            Some(&token),
            &invalid,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(failure, json!({"code":"invalid"}));
    }
    assert!(fixture.rows().await.is_empty());
    let remote = RemoteDeploymentHolds::new(
        &origin(&control_server),
        auth.clone(),
        &assignment,
        Options::default(),
    )
    .unwrap();
    let response = compio::time::timeout(
        Duration::from_secs(10),
        http.post(format!(
            "{}{}",
            origin(&control_server).trim_end_matches('/'),
            endpoint.path_template()
        ))
        .header("authorization", control_header(&auth))
        .send_json(&body),
    )
    .await
    .expect("hold acquisition replied before discarding its receipt")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    let original_rows = fixture.rows().await;
    assert_eq!(original_rows.len(), 1);
    assert!(typed_id::parse_with_prefix(&original_rows[0].0, "dhr").is_ok());
    let first = remote.acquire(&deploy, generation(1)).await.unwrap();
    assert_eq!(first.app_id, app);
    assert_eq!(first.deploy_hash, hash);
    assert_eq!(first.holder_id, HoldScope::for_app(app.clone()).holder());
    assert_eq!(remote.acquire(&deploy, generation(1)).await.unwrap(), first);
    assert_eq!(fixture.rows().await, original_rows);

    let replacement_assignment = placement(&coordinator, &replacement, &app).await;
    let verification = VerifyAssignment {
        app_id: app.clone(),
        worker_id: worker.clone(),
        assignment_revision: assignment.revision,
    };
    let verified = coordinator.verify_assignment(&verification).await.unwrap();
    assert_eq!(verified.app_id, assignment.app_id);
    assert_eq!(verified.worker_id, assignment.worker_id);
    assert_eq!(verified.revision, assignment.revision);
    assert!(verified.expires_at <= assignment.expires_at);
    let revoked = fixture
        .platform
        .admin
        .execute(
            "UPDATE zeroship.worker_instances SET status='gone' WHERE id=$1",
            &[&worker.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(revoked, 1);
    assert!(matches!(
        coordinator.verify_assignment(&verification).await,
        Err(CoordinationError::Refused(FailureCode::Denied))
    ));
    let token = control_header(&auth);
    let (status, failure) = post(
        &http,
        &origin(&control_server),
        endpoint,
        Some(&token),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(failure, json!({"code":"unauthenticated"}));
    let remote = RemoteDeploymentHolds::new(
        &origin(&control_server),
        replacement_auth.clone(),
        &replacement_assignment,
        Options::default(),
    )
    .unwrap();
    assert_eq!(remote.acquire(&deploy, generation(1)).await.unwrap(), first);
    assert_eq!(fixture.rows().await, original_rows);
    let released = remote.release(&deploy, generation(1)).await.unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_eq!(released.holder_id, first.holder_id);
    assert_eq!(
        remote.release(&deploy, generation(1)).await.unwrap(),
        released
    );
    let held = remote.acquire(&deploy, generation(2)).await.unwrap();
    assert_eq!(held.state, HoldState::Held);
    assert_eq!(held.holder_id, first.holder_id);
    assert!(matches!(
        remote.release(&deploy, generation(1)).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let rows = fixture.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, original_rows[0].0);
    assert_eq!(rows[0].2, 2);

    let token = control_header(&replacement_auth);
    let request = HoldRequest {
        generation: generation(2),
        assignment_revision: replacement_assignment.revision,
        ..request
    };
    let body = serde_json::to_value(&request).unwrap();
    let (status, receipt) = post(
        &http,
        &origin(&control_server),
        endpoint,
        Some(&token),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_value::<HoldReceipt>(receipt).unwrap(),
        held
    );
    let (status, failure) = post(
        &http,
        &origin(&control_server),
        endpoint,
        Some(&token),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(failure, json!({"code":"unauthenticated"}));
}

struct VerificationGate {
    verifier: ServiceAssertionVerifier,
    expected: VerifyAssignment,
    calls: AtomicUsize,
    reject_at: AtomicUsize,
    shorten_at: AtomicUsize,
    mutation_expected: AtomicBool,
    probe: Arc<compio_postgres::Client>,
}

async fn verify_gate(
    request: web::HttpRequest,
    gate: State<Arc<VerificationGate>>,
    body: Json<VerifyAssignment>,
) -> web::HttpResponse {
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    if verify_service_call(
        &gate.verifier,
        authorization,
        AUDIENCE,
        endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
    )
    .await
    .is_err()
    {
        return web::HttpResponse::Unauthorized().json(&Failure {
            code: FailureCode::Unauthenticated,
        });
    }
    assert_eq!(body.into_inner(), gate.expected);
    let call = gate.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let written: bool = gate
        .probe
        .query_one("SELECT is_called FROM zeroship.hold_write_probe", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        written,
        call >= 3 && gate.mutation_expected.load(Ordering::SeqCst),
        "authorization observed the wrong hold mutation phase"
    );
    if gate.reject_at.load(Ordering::SeqCst) == call {
        return web::HttpResponse::Forbidden().json(&Failure {
            code: FailureCode::Denied,
        });
    }
    let expires = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
        + if gate.shorten_at.load(Ordering::SeqCst) == call {
            1_000
        } else {
            60_000
        };
    web::HttpResponse::Ok().json(&Assignment {
        app_id: gate.expected.app_id.clone(),
        worker_id: gate.expected.worker_id.clone(),
        revision: gate.expected.assignment_revision,
        expires_at: expires.try_into().unwrap(),
    })
}

#[ntex::test]
async fn failed_final_placement_authorization_rolls_back_hold_generations() {
    let fixture = Fixture::new().await;
    let (app, deploy, _) = fixture.deployment("holds-rollback").await;
    let worker = WorkerId::mint();
    let expected = VerifyAssignment {
        app_id: app.clone(),
        worker_id: worker.clone(),
        assignment_revision: 1.try_into().unwrap(),
    };
    let (issuer, key) = fixture.state.service_auth.signing_identity().unwrap();
    let mut peers = ServiceTrustBundle::new();
    peers.trust_signing_key(issuer, key.key_id(), key).unwrap();
    fixture
        .platform
        .admin
        .batch_execute(
            "CREATE SEQUENCE zeroship.hold_write_probe;
             GRANT USAGE ON SEQUENCE zeroship.hold_write_probe TO zeroship_control;
             CREATE FUNCTION zeroship.probe_hold_write() RETURNS trigger
             LANGUAGE plpgsql AS $$
             BEGIN
                 PERFORM nextval('zeroship.hold_write_probe');
                 RETURN NEW;
             END $$;
             CREATE TRIGGER probe_hold_write
             BEFORE INSERT OR UPDATE ON zeroship.app_deploy_holds
             FOR EACH ROW EXECUTE FUNCTION zeroship.probe_hold_write();",
        )
        .await
        .unwrap();
    let probe_url = fixture
        .control_url
        .replacen("zeroship_control@", "postgres@", 1);
    let gate = Arc::new(VerificationGate {
        verifier: ServiceAssertionVerifier::new(peers, Arc::new(InMemoryReplayStore::new())),
        expected,
        calls: AtomicUsize::new(0),
        reject_at: AtomicUsize::new(3),
        shorten_at: AtomicUsize::new(0),
        mutation_expected: AtomicBool::new(true),
        probe: Arc::new(platform::connect(&probe_url).await),
    });
    let factory = gate.clone();
    let server = test::server(move || {
        let state = factory.clone();
        async move {
            web::App::new().state(state).service(
                web::resource(endpoints::WORKFLOW_VERIFY_ASSIGNMENT.path_template())
                    .route(web::post().to(verify_gate)),
            )
        }
    })
    .await;
    let api = DeploymentHoldApi::new(
        database(&fixture.control_url).await,
        ControlCoordinator::new(
            &origin(&server),
            fixture.state.service_auth.clone(),
            Options::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut request = HoldRequest {
        app_id: app,
        assignment_revision: 1.try_into().unwrap(),
        deploy_id: deploy,
        generation: generation(1),
    };
    for (acquire, reject, next_generation) in [
        (true, true, 1),
        (true, false, 1),
        (false, true, 1),
        (false, false, 1),
        (true, true, 2),
        (true, false, 2),
    ] {
        let before = fixture.rows().await;
        request.generation = generation(next_generation);
        gate.calls.store(0, Ordering::SeqCst);
        gate.reject_at
            .store(if reject { 3 } else { 0 }, Ordering::SeqCst);
        fixture
            .platform
            .admin
            .batch_execute("ALTER SEQUENCE zeroship.hold_write_probe RESTART WITH 1")
            .await
            .unwrap();
        let result = if acquire {
            api.acquire(&worker, &request).await
        } else {
            api.release(&worker, &request).await
        };
        assert_eq!(gate.calls.load(Ordering::SeqCst), 3);
        if reject {
            assert!(
                matches!(
                    result,
                    Err(zeroship_workflow_manager::deployments::Error::PermissionDenied)
                ),
                "{result:?}"
            );
            assert_eq!(
                fixture.rows().await,
                before,
                "final authorization failure committed a hold mutation"
            );
        } else {
            let receipt = result.unwrap();
            assert_eq!(receipt.generation, request.generation);
            assert_eq!(
                receipt.state,
                if acquire {
                    HoldState::Held
                } else {
                    HoldState::Released
                }
            );
            let after = fixture.rows().await;
            assert_eq!(after.len(), 1);
            assert_eq!(after[0].2, next_generation);
            if let Some(before) = before.first() {
                assert_eq!(after[0].0, before.0);
            }
        }
    }

    let blocker = platform::connect(&probe_url).await;
    blocker
        .batch_execute(
            "CREATE FUNCTION zeroship.gate_hold_commit() RETURNS trigger
             LANGUAGE plpgsql AS $$
             BEGIN
                 PERFORM pg_advisory_xact_lock(73921863);
                 RETURN NEW;
             END $$;
             CREATE CONSTRAINT TRIGGER gate_hold_commit
             AFTER INSERT OR UPDATE ON zeroship.app_deploy_holds
             DEFERRABLE INITIALLY DEFERRED
             FOR EACH ROW EXECUTE FUNCTION zeroship.gate_hold_commit();",
        )
        .await
        .unwrap();
    let blocker_pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    for (shorten_at, acquire, next_generation) in [(2, false, 2), (3, true, 3)] {
        blocker
            .query_one("SELECT pg_advisory_lock(73921863)", &[])
            .await
            .unwrap();
        let before = fixture.rows().await;
        request.generation = generation(next_generation);
        gate.calls.store(0, Ordering::SeqCst);
        gate.reject_at.store(0, Ordering::SeqCst);
        gate.shorten_at.store(shorten_at, Ordering::SeqCst);
        gate.mutation_expected.store(true, Ordering::SeqCst);
        fixture
            .platform
            .admin
            .batch_execute("ALTER SEQUENCE zeroship.hold_write_probe RESTART WITH 1")
            .await
            .unwrap();
        let (result, transaction_pid) = compio::time::timeout(Duration::from_secs(3), async {
            futures::join!(
                async {
                    if acquire {
                        api.acquire(&worker, &request).await
                    } else {
                        api.release(&worker, &request).await
                    }
                },
                blocked_hold_commit(&fixture.platform.admin, blocker_pid)
            )
        })
        .await
        .expect("shortened placement must bound the caller's wait for an in-flight commit");
        assert_eq!(gate.calls.load(Ordering::SeqCst), 3);
        assert!(
            matches!(
                result,
                Err(zeroship_workflow_manager::deployments::Error::Timeout)
            ),
            "{result:?}"
        );
        assert_eq!(
            fixture.rows().await,
            before,
            "the deferred commit barrier must retain the uncommitted mutation"
        );
        assert!(
            blocker
                .query_one("SELECT pg_advisory_unlock(73921863)", &[])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        compio::time::timeout(Duration::from_secs(3), async {
            loop {
                let active: bool = fixture
                    .platform
                    .admin
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
                         WHERE pid=$1 AND xact_start IS NOT NULL)",
                        &[&transaction_pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if !active {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the dispatched hold commit must settle after releasing its barrier");
        let committed = fixture.rows().await;
        assert_eq!(committed.len(), 1);
        assert_eq!(committed[0].0, before[0].0);
        assert_eq!(committed[0].2, next_generation);
        assert_eq!(committed[0].3, if acquire { "held" } else { "released" });
        gate.calls.store(0, Ordering::SeqCst);
        gate.shorten_at.store(0, Ordering::SeqCst);
        gate.mutation_expected.store(false, Ordering::SeqCst);
        fixture
            .platform
            .admin
            .batch_execute("ALTER SEQUENCE zeroship.hold_write_probe RESTART WITH 1")
            .await
            .unwrap();
        let receipt = if acquire {
            api.acquire(&worker, &request).await
        } else {
            api.release(&worker, &request).await
        }
        .unwrap();
        assert_eq!(gate.calls.load(Ordering::SeqCst), 3);
        assert_eq!(receipt.app_id, request.app_id);
        assert_eq!(receipt.deploy_id, request.deploy_id);
        assert_eq!(receipt.generation, request.generation);
        assert_eq!(
            receipt.state,
            if acquire {
                HoldState::Held
            } else {
                HoldState::Released
            }
        );
        assert_eq!(fixture.rows().await, committed);
    }
}

async fn blocked_hold_commit(observer: &compio_postgres::Client, blocker_pid: i32) -> i32 {
    loop {
        let waiting = observer
            .query(
                "SELECT pid FROM pg_stat_activity
                 WHERE usename='zeroship_control' AND wait_event_type='Lock'
                 AND $1=ANY(pg_blocking_pids(pid))",
                &[&blocker_pid],
            )
            .await
            .unwrap();
        if let Some(transaction) = waiting.first() {
            assert_eq!(waiting.len(), 1);
            return transaction.get(0);
        }
        compio::time::sleep(Duration::from_millis(1)).await;
    }
}
