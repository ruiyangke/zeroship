#![allow(
    clippy::future_not_send,
    reason = "native test services own their compio connections"
)]

use crate::support::deployments as deployment_commands;
use crate::support::platform;

mod app_facts;
mod collector;
mod journal_holds;
mod publication;
mod queue_holds;
mod retention_executor;
mod shared_catalog;
use zeroship_workflow_testkit::journal;

use ntex::{
    client::Client,
    http::StatusCode,
    web::{self, test},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{Value, json};
use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use zeroship_authn::service_replay::SharedClientReplayStore;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
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
        ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey, ServiceTrustBundle,
        TransportAssertionVerifier,
    },
    service_identity::{ServiceEndpoint, endpoints},
    service_peers::{
        CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, ServiceAuth, ServiceKeyring,
        WORKER_SERVICE_NAME, WORKFLOW_SERVICE_NAME, service_issuer,
    },
    typed_id,
    worker_join::{join_proof_message, mint_join_token, JoinTokenGrant, DEFAULT_EXECUTION_ZONE},
    workflow_coordination::{FailureCode, WorkerId, AUDIENCE},
};
use zeroship_data_orm::{
    ConnectOptions, binding::DbBinding, encryption::ProjectKeySource, orm::Database,
};
use zeroship_workflow::{
    WorkflowServiceError,
    deployment_holds::{
        DeploymentHoldClient, HoldGeneration, HoldRequest, HoldScope, HoldState,
        RemoteDeploymentHolds,
    },
};
use zeroship_workflow_client::{
    ControlCoordinator, Error as CoordinationError, Options,
};
use zeroship_workflow_server::{
    WorkflowHttpState,
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    coordinator::{Coordinator, Options as CoordinatorOptions},
    runs::RunService,
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
    /// A join token minted by a signer this deployment trusts: what a worker
    /// presents to join, recorded in `zeroship.worker_join_signers` as the
    /// operator's import would leave it.
    join_token: String,
    /// A bare `svc/worker` ROLE key that Control's peer bundle still trusts:
    /// the stale shared credential no process holds any more. Every endpoint
    /// here must refuse it at role arity.
    worker_role: Arc<ServiceAuth>,
    workflow_role: Arc<ServiceAuth>,
    /// A platform role whose key this deployment TRUSTS and which holds no
    /// deployment-hold grant.
    ///
    /// The third principal, and it is trusted on purpose: a refusal of an
    /// untrusted key says nothing about which principals a hold endpoint admits,
    /// because the credential never reaches the comparison. This one verifies
    /// and is still refused, so the refusal is about the principal.
    gateway_role: Arc<ServiceAuth>,
    control_url: String,
}

impl Fixture {
    async fn new() -> Self {
        // The fixture installs catalog triggers and reads every catalog session,
        // installation-global subjects, so it gets a database of its own.
        let platform = platform::Platform::fresh_database().await;
        let control_url = platform.role_url("zeroship_control").to_string();
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
        let gateway_role = signer(
            service_issuer(GATEWAY_SERVICE_NAME).unwrap(),
            ServiceSigningKey::generate(),
        );
        let (gateway_issuer, gateway_key) = gateway_role.signing_identity().unwrap();
        peers
            .trust_signing_key(gateway_issuer, gateway_key.key_id(), gateway_key)
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
        let state = Arc::new(AppState {
            service_auth,
            env_store: EnvStore::new(registry.clone(), "deployment-hold-test-master-key").unwrap(),
            stripe_store: StripeStore::new(registry.clone()),
            registry,
            blob_store,
            control_key: SecretString::new("deployment-hold-test-control-key".into()),
            master_key: SecretString::new("deployment-hold-test-master-key".into()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".into(),
            worker_urls: Vec::new(),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(100, 10))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(100, 10))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            worker_enrolment: zeroship_control::worker_join::EnrolmentEnvelope::parse(
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
        let signer_key = ServiceSigningKey::generate();
        let signer_id = typed_id::new_join_signer_id();
        let inserted = platform
            .admin
            .execute(
                "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
                 VALUES ($1, $2, 'active')",
                &[&signer_id, &signer_key.verifying_key_bytes().to_vec()],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        let inserted = platform
            .admin
            .execute(
                "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
                 VALUES ($1, 'ezn_default000000000000000000')",
                &[&signer_id],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        let join_token = mint_join_token(
            &signer_id,
            &signer_key,
            &service_issuer(CONTROL_SERVICE_NAME).unwrap(),
            &JoinTokenGrant {
                zone: DEFAULT_EXECUTION_ZONE.to_owned(),
                lifetime: Duration::from_secs(600),
                uses: 16,
                confirm: None,
            },
        )
        .unwrap();
        Self {
            platform,
            state,
            join_token,
            worker_role,
            workflow_role,
            gateway_role,
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
        // Under the platform fixture's own work directory, which outlives the
        // server this composes.
        let objects = self.platform.work.path().join("payloads");
        let server = test::server(move || {
            let url = url.clone();
            let catalog_url = catalog_url.clone();
            let control = control.clone();
            let objects = objects.clone();
            async move {
                let registry = Arc::new(platform::connect(&url).await);
                let replay = Arc::new(SharedClientReplayStore::new(registry.clone()));
                let (issuer, key) = control.signing_identity().unwrap();
                let mut peers = ServiceTrustBundle::new();
                peers.trust_signing_key(issuer, key.key_id(), key).unwrap();
                let service = Coordinator::connect(
                    &url,
                    CoordinatorOptions::default(),
                    Rc::new(zeroship_workflow_manager::retention::CatalogClient::new(
                        zeroship_workflow_manager::deployments::DeploymentHolds::new(
                            database(&catalog_url).await,
                        )
                        .unwrap(),
                    )),
                )
                .await
                .unwrap();
                // The journal shares the coordinator's database and login, and
                // the platform migrations this fixture runs already install its
                // schema; opening it provisions nothing.
                let runs = Rc::new(
                    RunService::connect(
                        &url,
                        service
                            .recovery(zeroship_workflow_manager::recovery::Options::default())
                            .unwrap(),
                    )
                    .await
                    .expect("open the journal this service serves"),
                );
                let state = Rc::new(WorkflowHttpState {
                    policy_source: None,
                    // This fixture drives deployment holds, not journal
                    // provisioning; a manager without a journal client simply
                    // does not ensure schemas.
                    service,
                    runs,
                    // The store `start` stages a run input into. Nothing here
                    // starts a run, so this is composed and never written.
                    payloads: zeroship_workflow_server::payloads::ServicePayloads::open(
                        &zeroship_storage::StorageBackendConfig::Local(objects.clone()),
                    )
                    .expect("open the payload store this service composes"),
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
        .await;
        crate::support::live::register_listener(server.addr());
        server
    }

    /// Control's hold routes over the fixture registry's retention executor,
    /// composed as the binary composes them: one API every serving thread
    /// shares.
    async fn control(&self) -> test::TestServer {
        let state = self.state.clone();
        let api = Arc::new(DeploymentHoldApi::new(state.registry.retention().clone()));
        let server = test::server(move || {
            let state = state.clone();
            let api = api.clone();
            async move {
                web::App::new()
                    .state(state)
                    .state(api)
                    .service(
                        web::resource("/internal/workers/join").route(
                            web::post().to(zeroship_control::internal::join_worker_instance),
                        ),
                    )
                    .configure(deployment_hold_api::configure)
                    // The other route `svc/workflow` reaches on this service.
                    // It shares this harness because it shares the credential
                    // and the issuer gate the harness exists to exercise.
                    .configure(zeroship_control::app_facts_api::configure)
            }
        })
        .await;
        crate::support::live::register_listener(server.addr());
        server
    }

    async fn joined_worker(
        &self,
        http: &Client,
        control_url: &str,
    ) -> (WorkerId, Arc<ServiceAuth>) {
        let key = ServiceSigningKey::generate();
        let public = key.verifying_key_bytes();
        // The join proof: the same bytes `crates/zeroship-worker/src/join.rs`
        // signs, made with the key being registered. Presenting the token
        // without it registers nothing.
        let proof = key.sign_detached(&join_proof_message(&self.join_token, &public, 8080));
        let (status, response) = post_to(
            http,
            control_url,
            "/internal/workers/join",
            Some(&format!("Bearer {}", self.join_token)),
            &json!({
                "port": 8080,
                "public_key": URL_SAFE_NO_PAD.encode(public),
                "proof": URL_SAFE_NO_PAD.encode(proof),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{response}");
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
        DbBinding::platform(
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
    post_to(http, url, endpoint.path_template(), token, body).await
}

/// The same exchange against a literal path, for the JOIN route: joining is not
/// behind the service-assertion allowlist, so it has no `ServiceEndpoint` to
/// name it.
async fn post_to(
    http: &Client,
    url: &str,
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> (StatusCode, Value) {
    compio::time::timeout(Duration::from_secs(10), async {
        let mut request = http.post(format!("{}{}", url.trim_end_matches('/'), path));
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
