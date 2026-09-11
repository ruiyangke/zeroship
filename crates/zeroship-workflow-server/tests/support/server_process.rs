use ed25519_dalek::pkcs8::EncodePrivateKey;
use ntex::{client::Client, http::StatusCode};
use serde_json::json;
use std::{
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionMinter, ServiceAssertionVerifier, ServiceIssuer,
        ServiceSigningKey, ServiceTrustBundle,
    },
    service_peers::{ServiceAuth, ServiceKeyring},
    typed_id,
};
use zeroship_workflow::{
    operations::{RunState, StartOptions},
    service::{
        capability::{mint_app_capability, AppGrant, AppOperation, WORKFLOW_AUDIENCE},
        DeployRegistration, RequestId, WorkflowEndpoint,
    },
    WorkflowExecution,
};

fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

struct ServerProcess {
    child: Child,
    config: PathBuf,
    log: PathBuf,
    url: String,
}
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl ServerProcess {
    fn spawn(config: PathBuf, log: PathBuf, url: String) -> Self {
        let output = std::fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-server"))
            .env_clear()
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        Self {
            child,
            config,
            log,
            url,
        }
    }
    async fn ready(&mut self, client: &Client) {
        let result = compio::time::timeout(Duration::from_secs(30), async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(&self.log).unwrap()
                );
                if let Ok(response) = client.get(format!("{}/readyz", self.url)).send().await {
                    if response.status() == StatusCode::OK {
                        break;
                    }
                }
                compio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "workflow server did not become ready: {}",
            std::fs::read_to_string(&self.log).unwrap()
        );
    }
}

pub async fn contract(admin: &compio_postgres::Client, runtime_url: &str, dir: &Path) {
    let private = ed25519_dalek::SigningKey::from_bytes(&[71; 32])
        .to_pkcs8_der()
        .unwrap();
    let workflow_key = ServiceSigningKey::from_pkcs8_der(private.as_bytes()).unwrap();
    let control_key = ServiceSigningKey::generate();
    let key_file = dir.join("workflow.der");
    let peer_file = dir.join("peers.json");
    write_private(&key_file, private.as_bytes());
    write_private(
        &peer_file,
        serde_json::to_vec(&json!({"keys":[
            {"iss":WORKFLOW_AUDIENCE,"x":workflow_key.public_jwk_x()},
            {"iss":"spiffe://zeroship.ai/svc/control","x":control_key.public_jwk_x()},
        ]}))
        .unwrap(),
    );
    let mut servers = Vec::new();
    let client = Client::new().await;
    for name in ["first", "replica"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let config = dir.join(format!("{name}.toml"));
        write_private(
            &config,
            toml::to_string(&json!({"workflow":{
                "listen":addr.to_string(),"database_url":runtime_url,"service_key_file":key_file,
                "service_peers_file":peer_file,"payload_url":dir.join("payloads"),"http_threads":1,
            }}))
            .unwrap(),
        );
        let mut server = ServerProcess::spawn(
            config,
            dir.join(format!("{name}.log")),
            format!("http://{addr}"),
        );
        server.ready(&client).await;
        servers.push(server);
    }
    assert_eq!(
        client
            .get(format!("{}/healthz", servers[0].url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let app = AppId::mint();
    let organization = typed_id::generate("org");
    let project = typed_id::generate("prj");
    let plan = typed_id::new_plan_id();
    admin.execute("INSERT INTO zeroship.organizations (id,slug,name,billing_email) VALUES ($1,'workflow-test','Workflow','workflow@example.test')",&[&organization]).await.unwrap();
    admin.execute("INSERT INTO zeroship.projects (id,organization_id,slug,name) VALUES ($1,$2,'workflow-test','Workflow')",&[&project,&organization]).await.unwrap();
    admin.execute("INSERT INTO zeroship.plans (id,name,workflows_allowed,runtime_limits_json) VALUES ($1,'workflow-test',true,'{}')",&[&plan]).await.unwrap();
    admin.execute("INSERT INTO zeroship.apps (id,name,plan_id,project_id,organization_id,workflows_enabled) VALUES ($1,'workflow-process-test',$2,$3,$4,true)",&[&app.uuid(),&plan,&project,&organization]).await.unwrap();
    let workflow_issuer = ServiceIssuer::parse(WORKFLOW_AUDIENCE).unwrap();
    let minter = ServiceAssertionMinter::new(
        ServiceIssuer::parse("spiffe://zeroship.ai/svc/control").unwrap(),
        control_key.key_id(),
        &control_key,
    )
    .unwrap();
    let deployment = DeployRegistration {
        id: typed_id::generate("dep"),
        hash: "b".repeat(64),
        workflows: ["Example".into()].into(),
        schedules: Vec::new(),
    };
    let manifest = serde_json::to_string(&json!({"workflows":deployment.workflows})).unwrap();
    admin.batch_execute("BEGIN").await.unwrap();
    admin
        .execute(
            "UPDATE zeroship.apps SET deploy_hash=$2,manifest_json=$3 WHERE id=$1",
            &[&app.uuid(), &deployment.hash, &manifest],
        )
        .await
        .unwrap();
    admin.execute("INSERT INTO zeroship.app_deploys (id,app_id,deploy_hash,manifest_json,activated_at) VALUES ($1,$2,$3,$4,now())", &[&deployment.id,&app.uuid(),&deployment.hash,&manifest]).await.unwrap();
    admin.batch_execute("COMMIT").await.unwrap();
    let response = client
        .post(format!(
            "{}/v1/apps/{}/workflow-deploy",
            servers[0].url,
            app.as_str()
        ))
        .header(
            "authorization",
            format!("Bearer {}", minter.mint(&workflow_issuer).unwrap()),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let token = mint_app_capability(
        &control_key,
        AppGrant {
            app_id: app.clone(),
            operations: [AppOperation::Start, AppOperation::Status].into(),
        },
        now,
        120,
    )
    .unwrap();
    let endpoint = WorkflowEndpoint::new(&servers[0].url).unwrap();
    let scoped = endpoint.for_app(app.clone(), token.clone());
    let run = scoped
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker_key = ServiceSigningKey::generate();
    let instance = typed_id::generate("wkr");
    let public = worker_key.verifying_key_bytes().to_vec();
    admin.execute("INSERT INTO zeroship.worker_instances (id,ring_key,public_key,advertise_host,advertise_port,status) VALUES ($1,$2,$3,'127.0.0.1',8080,'active')",&[&instance,&vec![9u8],&public]).await.unwrap();
    let mut keyring = ServiceKeyring::from_parts(
        ServiceIssuer::parse(&format!("spiffe://zeroship.ai/svc/worker/{instance}")).unwrap(),
        worker_key,
        ServiceTrustBundle::new(),
    )
    .unwrap();
    let verifier = ServiceAssertionVerifier::new(
        keyring.take_bundle().unwrap(),
        Arc::new(InMemoryReplayStore::new()),
    );
    let worker = Arc::new(ServiceAuth::new(keyring, Arc::new(verifier)));
    let tasks = endpoint.tasks(worker.clone());
    let task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    let assertion = worker.authorization_for(&workflow_issuer).unwrap();
    for (index, status) in [(0, StatusCode::OK), (1, StatusCode::UNAUTHORIZED)] {
        let response = client
            .post(format!("{}/v1/tasks/poll", servers[index].url))
            .header("authorization", assertion.clone())
            .send_json(&json!({}))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            status,
            "service assertions must be single-use across processes"
        );
    }
    let execution: WorkflowExecution = serde_json::from_value(
        json!({"outcomes":[{"kind":"RunCompleted","output":{"done":true}}]}),
    )
    .unwrap();
    let replica = WorkflowEndpoint::new(&servers[1].url)
        .unwrap()
        .tasks(worker);
    let receipt = replica
        .complete(&task.id, &task.token, execution.clone())
        .await
        .unwrap();
    assert_eq!(
        receipt,
        tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap()
    );
    assert_eq!(
        scoped.status(&run.id).await.unwrap().state,
        RunState::Completed
    );
    let first = servers.remove(0);
    let (config, log, url) = (first.config.clone(), first.log.clone(), first.url.clone());
    drop(first);
    let mut restarted = ServerProcess::spawn(config, log, url);
    restarted.ready(&client).await;
    assert_eq!(
        scoped.status(&run.id).await.unwrap().output,
        Some(json!({"done":true}))
    );
    admin
        .batch_execute("ALTER TABLE zeroship.apps DISABLE TRIGGER workflow_policy_fence_app")
        .await
        .unwrap();
    assert_eq!(
        client
            .get(format!("{}/readyz", restarted.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    admin
        .batch_execute("ALTER TABLE zeroship.apps ENABLE TRIGGER workflow_policy_fence_app")
        .await
        .unwrap();
}
