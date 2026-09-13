//! Control publishes closed schedule metadata without assigning an execution worker.
#![allow(
    clippy::future_not_send,
    reason = "the HTTP process fixture and native clients share the ntex compio runtime"
)]

#[path = "support/platform.rs"]
mod platform;
#[allow(
    dead_code,
    reason = "shared process fixture supports other failure probes"
)]
#[path = "support/server_process.rs"]
mod server_process;

use compio::io::{AsyncRead, AsyncWriteExt};
use ntex::{client::Client, http::StatusCode};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey, ServiceTrustBundle,
        TransportAssertionVerifier,
    },
    service_identity::{endpoints, ServiceEndpoint},
    service_peers::{service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME},
    workflow_coordination::{FailureCode, AUDIENCE},
    workflow_jobs::{DeploymentId, JobOperation, JobSpec},
    workflow_schedules::{
        ActivateSchedules, RegisterSchedules, ScheduleCatchUp, ScheduleDescriptor, ScheduleOverlap,
        ScheduleTiming,
    },
};
use zeroship_workflow_client::{ControlCoordinator, Error, Options};

struct Signer {
    issuer: ServiceIssuer,
    key: ServiceSigningKey,
}
impl Signer {
    fn new(issuer: ServiceIssuer) -> Self {
        Self {
            issuer,
            key: ServiceSigningKey::generate(),
        }
    }
    fn token(&self, audience: &str) -> String {
        format!(
            "Bearer {}",
            ServiceAssertionMinter::new(self.issuer.clone(), self.key.key_id(), &self.key)
                .unwrap()
                .mint(&ServiceIssuer::parse(audience).unwrap())
                .unwrap()
        )
    }
}

struct Fixture {
    platform: platform::Platform,
    http: Client,
    server: server_process::ServerProcess,
    control: Arc<ServiceAuth>,
    client: ControlCoordinator,
    rejected: Vec<Signer>,
}
impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::new().await;
        let signer = Signer::new(service_issuer(CONTROL_SERVICE_NAME).unwrap());
        let rejected = [
            "spiffe://zeroship.ai/svc/worker",
            "spiffe://zeroship.ai/svc/gateway",
            "spiffe://zeroship.ai/svc/workflow",
            "spiffe://zeroship.ai/svc/control/control-a",
        ]
        .into_iter()
        .map(|issuer| Signer::new(ServiceIssuer::parse(issuer).unwrap()))
        .collect::<Vec<_>>();
        let keys = std::iter::once(&signer)
            .chain(rejected.iter())
            .map(|signer| {
                json!({
                    "iss":signer.issuer.as_str(),"x":signer.key.public_jwk_x()
                })
            })
            .collect::<Vec<_>>();
        let peers = platform.work.path().join("schedule-peers.json");
        platform::write_private(&peers, serde_json::to_vec(&json!({"keys":keys})).unwrap());
        let http = Client::new().await;
        let server = server_process::ServerProcess::start(
            &platform.runtime_url,
            &peers,
            platform.work.path(),
            "schedules",
            &http,
        )
        .await;
        let control = Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(signer.issuer, signer.key, ServiceTrustBundle::new())
                .unwrap(),
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ));
        let client =
            ControlCoordinator::new(&server.url, control.clone(), Options::default()).unwrap();
        Self {
            platform,
            http,
            server,
            control,
            client,
            rejected,
        }
    }
    fn token(&self) -> String {
        self.control
            .authorization_for(&ServiceIssuer::parse(AUDIENCE).unwrap())
            .unwrap()
    }
    async fn post(
        &self,
        endpoint: ServiceEndpoint,
        token: &str,
        body: &Value,
    ) -> (StatusCode, Value) {
        let response = self
            .http
            .post(format!("{}{}", self.server.url, endpoint.path_template()))
            .header("authorization", token)
            .send_json(body)
            .await
            .unwrap();
        let status = response.status();
        let body = response.body().await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }
}

fn registration() -> RegisterSchedules {
    RegisterSchedules {
        app_id: AppId::mint(),
        deployment_id: DeploymentId::mint(),
        schedules: ["later", "earlier"]
            .into_iter()
            .map(|name| ScheduleDescriptor {
                name: name.into(),
                workflow_name: "scheduled-work".into(),
                schedule: ScheduleTiming::Cron {
                    cron_expr: "0 0 1 1 *".into(),
                    tz: "UTC".into(),
                },
                overlap: ScheduleOverlap::Allow,
                catch_up: ScheduleCatchUp::Skip,
            })
            .collect(),
    }
}

fn activation(registration: &RegisterSchedules, revision: i64) -> ActivateSchedules {
    ActivateSchedules {
        app_id: registration.app_id.clone(),
        deployment_id: registration.deployment_id.clone(),
        revision: revision.try_into().unwrap(),
    }
}

async fn rejects_before_body(fixture: &Fixture, endpoint: ServiceEndpoint, token: Option<&str>) {
    let mut stream = compio::net::TcpStream::connect(fixture.server.address)
        .await
        .unwrap();
    let mut header = format!(
        "POST {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 64\r\n",
        endpoint.path_template(),
    );
    if let Some(token) = token {
        header.push_str("Authorization: ");
        header.push_str(token);
        header.push_str("\r\n");
    }
    header.push_str("\r\n");
    stream.write_all(header.into_bytes()).await.0.unwrap();
    let response = compio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        loop {
            let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
            let read = read.unwrap();
            if read == 0 {
                return String::from_utf8(bytes).unwrap();
            }
            bytes.extend_from_slice(&buffer[..read]);
            assert!(bytes.len() <= 4096);
        }
    })
    .await
    .expect("unauthorized schedule request waited for its withheld body");
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(response.to_ascii_lowercase().contains("connection: close"));
}

async fn authentication(fixture: &Fixture, registration: &RegisterSchedules) {
    for endpoint in [
        endpoints::WORKFLOW_SCHEDULE_REGISTER,
        endpoints::WORKFLOW_SCHEDULE_ACTIVATE,
    ] {
        rejects_before_body(fixture, endpoint, None).await;
        for signer in &fixture.rejected {
            rejects_before_body(fixture, endpoint, Some(&signer.token(AUDIENCE))).await;
        }
        let wrong_key = Signer::new(service_issuer(CONTROL_SERVICE_NAME).unwrap());
        rejects_before_body(fixture, endpoint, Some(&wrong_key.token(AUDIENCE))).await;
        let wrong_audience = fixture
            .control
            .authorization_for(&service_issuer(CONTROL_SERVICE_NAME).unwrap())
            .unwrap();
        rejects_before_body(fixture, endpoint, Some(&wrong_audience)).await;
    }
    let token = fixture.token();
    let body = json!(registration);
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_SCHEDULE_REGISTER, &token, &body)
            .await,
        (StatusCode::OK, body.clone())
    );
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_SCHEDULE_REGISTER, &token, &body)
            .await,
        (StatusCode::UNAUTHORIZED, json!({"code":"unauthenticated"}))
    );
    assert_eq!(
        fixture
            .client
            .register_schedules(registration)
            .await
            .unwrap(),
        *registration
    );
}

async fn closed_requests(fixture: &Fixture) {
    let registration = registration();
    let original = json!(registration);
    let mut unknown = original.clone();
    unknown["input"] = json!({"private":"payload"});
    let mut nested = original.clone();
    nested["schedules"][0]["schedule"]["credentials"] = json!({"token":"not metadata"});
    let mut catch_up = original.clone();
    catch_up["schedules"][0]["catchUp"]["extra"] = json!(true);
    for body in [unknown, nested, catch_up] {
        assert_eq!(
            fixture
                .post(
                    endpoints::WORKFLOW_SCHEDULE_REGISTER,
                    &fixture.token(),
                    &body
                )
                .await,
            (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
        );
    }
    let mut oversized = original;
    oversized["unknown"] = json!("x".repeat(2048));
    assert_eq!(
        fixture
            .post(
                endpoints::WORKFLOW_SCHEDULE_REGISTER,
                &fixture.token(),
                &oversized
            )
            .await,
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"code":"request_too_large"})
        )
    );
    let mut activation = json!(activation(&registration, 1));
    activation["holderId"] = json!("caller-chosen");
    assert_eq!(
        fixture
            .post(
                endpoints::WORKFLOW_SCHEDULE_ACTIVATE,
                &fixture.token(),
                &activation
            )
            .await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
    );
    activation["holderId"] = json!("x".repeat(2048));
    assert_eq!(
        fixture
            .post(
                endpoints::WORKFLOW_SCHEDULE_ACTIVATE,
                &fixture.token(),
                &activation,
            )
            .await,
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"code":"request_too_large"}),
        )
    );
    assert_eq!(
        fixture
            .platform
            .admin
            .query_one(
                "SELECT COUNT(*) FROM workflow_manager.schedule_deployments WHERE id=$1",
                &[&registration.deployment_id.as_str()],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
}

async fn accepted_occurrence(fixture: &Fixture, registration: &RegisterSchedules) -> String {
    assert_eq!(fixture.platform.admin.execute(
        "UPDATE workflow_manager.schedules SET next_at=0 WHERE app_id=$1 AND name='earlier'",
        &[&registration.app_id.as_str()],
    ).await.unwrap(), 1);
    compio::time::timeout(Duration::from_secs(30), async {
        loop {
            let rows = fixture.platform.admin.query(
                "SELECT to_jsonb(o)::text FROM workflow_manager.schedule_occurrences o WHERE app_id=$1",
                &[&registration.app_id.as_str()],
            ).await.unwrap();
            if let Some(row) = rows.first() {
                assert_eq!(rows.len(), 1);
                return row.get(0);
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("manager did not publish the due calendar without a worker")
}

async fn activation_lifecycle(fixture: &Fixture, registration: &RegisterSchedules) {
    let command = activation(registration, 1);
    let original = fixture.client.activate_schedules(&command).await.unwrap();
    assert_eq!(original.app_id, command.app_id);
    assert_eq!(original.deployment_id, command.deployment_id);
    assert_eq!(
        original.operation,
        JobOperation::Activate {
            revision: command.revision
        }
    );
    assert_eq!(
        fixture.client.activate_schedules(&command).await.unwrap(),
        original
    );
    let occurrence = accepted_occurrence(fixture, registration).await;

    let mut changed = registration.clone();
    changed.schedules[0].workflow_name = "different-workflow".into();
    assert_eq!(
        fixture.client.register_schedules(&changed).await,
        Err(Error::Refused(FailureCode::Conflict))
    );
    let mut reordered = registration.clone();
    reordered.schedules.reverse();
    assert_eq!(
        fixture.client.register_schedules(&reordered).await.unwrap(),
        reordered
    );

    let replacement = RegisterSchedules {
        app_id: registration.app_id.clone(),
        deployment_id: DeploymentId::mint(),
        schedules: vec![],
    };
    fixture
        .client
        .register_schedules(&replacement)
        .await
        .unwrap();
    let newer = fixture
        .client
        .activate_schedules(&activation(&replacement, 3))
        .await
        .unwrap();
    assert_ne!(original.id, newer.id);
    assert_eq!(
        fixture.client.activate_schedules(&command).await.unwrap(),
        original
    );
    assert_eq!(
        fixture
            .client
            .activate_schedules(&activation(&replacement, 1))
            .await,
        Err(Error::Refused(FailureCode::Conflict))
    );
    assert_eq!(
        fixture
            .client
            .activate_schedules(&activation(registration, 2))
            .await,
        Err(Error::Refused(FailureCode::Conflict))
    );
    let foreign = ActivateSchedules {
        app_id: AppId::mint(),
        ..command
    };
    assert_eq!(
        fixture.client.activate_schedules(&foreign).await,
        Err(Error::Refused(FailureCode::Denied))
    );
    assert_removed(fixture, registration, &replacement, &newer, &occurrence).await;
}

async fn assert_removed(
    fixture: &Fixture,
    original: &RegisterSchedules,
    replacement: &RegisterSchedules,
    activation: &JobSpec,
    occurrence: &str,
) {
    let active = fixture
        .platform
        .admin
        .query_one(
            "SELECT revision,activation_id FROM workflow_manager.schedule_scopes WHERE id=$1",
            &[&original.app_id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(active.get::<_, i64>(0), 3);
    assert_eq!(active.get::<_, &str>(1), activation.id.as_str());
    let schedules = fixture.platform.admin.query(
        "SELECT next_at,catch_up_until,catch_up_remaining FROM workflow_manager.schedules WHERE app_id=$1",
        &[&original.app_id.as_str()],
    ).await.unwrap();
    assert!(
        !schedules.is_empty(),
        "removed schedules retain receipt references"
    );
    for row in schedules {
        for column in 0..3 {
            assert_eq!(row.get::<_, Option<i64>>(column), None);
        }
    }
    let replayed: String = fixture
        .platform
        .admin
        .query_one(
            "SELECT to_jsonb(o)::text FROM workflow_manager.schedule_occurrences o WHERE app_id=$1",
            &[&original.app_id.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(replayed, occurrence);
    for deployment in [&original.deployment_id, &replacement.deployment_id] {
        let state: String = fixture.platform.admin.query_one(
            "SELECT state FROM workflow_manager.deployment_holds WHERE app_id=$1 AND deployment_id=$2",
            &[&original.app_id.as_str(), &deployment.as_str()],
        ).await.unwrap().get(0);
        assert_eq!(state, "held");
    }
    let no_workers = fixture
        .platform
        .admin
        .query_one(
            "SELECT (SELECT COUNT(*) FROM zeroship.worker_instances), \
                (SELECT COUNT(*) FROM workflow_manager.workers), \
                (SELECT COUNT(*) FROM workflow_manager.assignments)",
            &[],
        )
        .await
        .unwrap();
    for column in 0..3 {
        assert_eq!(no_workers.get::<_, i64>(column), 0);
    }
}

#[ntex::test]
async fn control_publishes_immutable_schedules_without_workers() {
    let fixture = Fixture::new().await;
    let registration = registration();
    authentication(&fixture, &registration).await;
    closed_requests(&fixture).await;
    activation_lifecycle(&fixture, &registration).await;
    assert_eq!(
        fixture
            .http
            .get(format!("{}/readyz", fixture.server.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}
