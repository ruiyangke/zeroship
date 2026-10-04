//! App-scoped deployment retention over the authenticated Control API.
#![allow(
    clippy::future_not_send,
    reason = "test sockets stay on the compio runtime"
)]

use compio::io::{AsyncRead, AsyncWriteExt};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{authorize, endpoints, verify_identity, PeerCredentials, ServiceEndpoint},
    service_peers::{ServiceAuth, ServiceKeyring},
    typed_id,
    workflow_coordination::{WorkerId, AUDIENCE},
};
use zeroship_workflow::{
    deployment_holds::{
        DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldRequest, HoldScope, HoldState,
        RemoteDeploymentHolds,
    },
    WorkflowServiceError,
};

const CONTROL_AUDIENCE: &str = "spiffe://zeroship.ai/svc/control";
const WORKFLOW_ISSUER: &str = "spiffe://zeroship.ai/svc/workflow";
use zeroship_workflow_client::Options;

fn auth(issuer: ServiceIssuer) -> Arc<ServiceAuth> {
    let keyring = ServiceKeyring::from_parts(
        issuer,
        ServiceSigningKey::generate(),
        ServiceTrustBundle::new(),
    )
    .unwrap();
    Arc::new(ServiceAuth::new(
        keyring,
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}

struct Fixture {
    service: Arc<ServiceAuth>,
    app: AppId,
    deploy_id: String,
    generation: HoldGeneration,
}

impl Fixture {
    fn new() -> Self {
        Self {
            service: auth(ServiceIssuer::parse(WORKFLOW_ISSUER).unwrap()),
            app: AppId::mint(),
            deploy_id: typed_id::generate("dep"),
            generation: 3.try_into().unwrap(),
        }
    }

    fn client(&self, url: &str) -> RemoteDeploymentHolds {
        RemoteDeploymentHolds::asserted(
            url,
            self.service.clone(),
            self.app.clone(),
            Options::default(),
        )
        .unwrap()
    }

    fn request(&self) -> Value {
        json!({
            "appId": self.app,
            "deployId": self.deploy_id,
            "generation": self.generation,
        })
    }

    fn receipt(&self, state: HoldState) -> HoldReceipt {
        HoldReceipt {
            app_id: self.app.clone(),
            deploy_id: self.deploy_id.clone(),
            holder_id: HoldScope::for_app(self.app.clone()).holder().to_owned(),
            generation: self.generation,
            state,
            deploy_hash: "a".repeat(64),
        }
    }
}

fn endpoint(state: HoldState) -> ServiceEndpoint {
    match state {
        HoldState::Held => endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
        HoldState::Released => endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
    }
}

async fn change(
    fixture: &Fixture,
    client: &RemoteDeploymentHolds,
    state: HoldState,
) -> Result<HoldReceipt, WorkflowServiceError> {
    match state {
        HoldState::Held => client.acquire(&fixture.deploy_id, fixture.generation).await,
        HoldState::Released => client.release(&fixture.deploy_id, fixture.generation).await,
    }
}

fn response(status: u16, body: &impl serde::Serialize) -> Vec<u8> {
    let body = serde_json::to_vec(body).unwrap();
    let mut bytes = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend(body);
    bytes
}

struct ObservedRequest {
    header: String,
    body: Value,
}

impl ObservedRequest {
    fn bearer(&self) -> &str {
        self.header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().strip_prefix("Bearer "))
                    .flatten()
            })
            .expect("workflow service assertion")
    }

    fn assert_operation(&self, fixture: &Fixture, state: HoldState) {
        assert_eq!(
            self.header.lines().next().unwrap(),
            format!("POST {} HTTP/1.1", endpoint(state).path_template())
        );
        assert_eq!(self.body, fixture.request());
        let request: HoldRequest = serde_json::from_value(self.body.clone()).unwrap();
        assert_eq!(request.app_id, fixture.app);
        assert_eq!(request.deploy_id, fixture.deploy_id);
        assert_eq!(request.generation, fixture.generation);
    }
}

async fn read_request(stream: &mut compio::net::TcpStream) -> ObservedRequest {
    let mut request = Vec::new();
    loop {
        let compio::BufResult(read, bytes) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0);
        request.extend_from_slice(&bytes[..read]);
        assert!(request.len() < 16 * 1024);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = std::str::from_utf8(&request[..end]).unwrap();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if request.len() >= end + 4 + length {
                return ObservedRequest {
                    header: header.to_owned(),
                    body: serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap(),
                };
            }
        }
    }
}

async fn exchange(
    fixture: &Fixture,
    state: HoldState,
    status: u16,
    body: Value,
) -> Result<HoldReceipt, WorkflowServiceError> {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = fixture.client(&format!("http://{}", listener.local_addr().unwrap()));
    let server = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        request.assert_operation(fixture, state);
        assert!(!request.bearer().is_empty());
        stream.write_all(response(status, &body)).await.0.unwrap();
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let ((), result) = futures::join!(server, change(fixture, &client, state));
        result
    })
    .await
    .expect("deployment hold exchange completed")
}

#[compio::test]
async fn lost_reply_retries_preserve_generation_with_fresh_control_assertions() {
    let fixture = Fixture::new();
    let (issuer, key) = fixture.service.signing_identity().unwrap();
    let mut trust = ServiceTrustBundle::new();
    trust.trust_signing_key(issuer, key.key_id(), key).unwrap();
    let verifier = ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()));
    let unenrolled = ServiceAssertionVerifier::new(
        ServiceTrustBundle::new(),
        Arc::new(InMemoryReplayStore::new()),
    );
    let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = fixture.client(&format!("http://{}", listener.local_addr().unwrap()));
    let server = async {
        let mut prior = None;
        for (state, lose_reply) in [
            (HoldState::Held, true),
            (HoldState::Held, false),
            (HoldState::Released, false),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            request.assert_operation(&fixture, state);
            let assertion = request.bearer();
            assert_ne!(prior.as_deref(), Some(assertion));
            let wrong_audience = PeerCredentials::new(Some(assertion), None, AUDIENCE);
            assert!(verify_identity(&verifier, &wrong_audience).await.is_err());
            let credentials = PeerCredentials::new(Some(assertion), None, CONTROL_AUDIENCE);
            assert!(verify_identity(&unenrolled, &credentials).await.is_err());
            let identity = verify_identity(&verifier, &credentials).await.unwrap();
            assert!(identity.matches_principal(issuer.principal()));
            assert!(authorize(&identity, endpoint(state)));
            assert!(verify_identity(&verifier, &credentials).await.is_err());
            prior = Some(assertion.to_owned());
            if !lose_reply {
                stream
                    .write_all(response(200, &fixture.receipt(state)))
                    .await
                    .0
                    .unwrap();
            }
        }
    };
    let calls = async {
        assert!(matches!(
            change(&fixture, &client, HoldState::Held).await,
            Err(WorkflowServiceError::Unavailable(_))
        ));
        for state in [HoldState::Held, HoldState::Released] {
            assert_eq!(
                change(&fixture, &client, state).await.unwrap(),
                fixture.receipt(state)
            );
        }
    };
    compio::time::timeout(Duration::from_secs(10), async {
        futures::join!(server, calls);
    })
    .await
    .expect("authenticated retries completed");
}

#[compio::test]
async fn receipts_cannot_substitute_scope_deployment_generation_or_state() {
    let fixture = Fixture::new();
    for state in [HoldState::Held, HoldState::Released] {
        let receipt = fixture.receipt(state);
        assert_eq!(
            exchange(&fixture, state, 200, json!(receipt))
                .await
                .unwrap(),
            receipt
        );
        let other_state = match state {
            HoldState::Held => HoldState::Released,
            HoldState::Released => HoldState::Held,
        };
        for (field, replacement) in [
            ("appId", json!(AppId::mint())),
            ("deployId", json!(typed_id::generate("dep"))),
            ("holderId", json!(typed_id::generate("dhl"))),
            ("generation", json!(fixture.generation.next().unwrap())),
            ("state", json!(other_state)),
            ("deployHash", json!("not-a-bundle-hash")),
            ("deployHash", json!("A".repeat(64))),
            ("journal", json!({"schema":"customer"})),
        ] {
            let mut body = json!(receipt);
            body[field] = replacement;
            let error = exchange(&fixture, state, 200, body).await.unwrap_err();
            assert!(
                matches!(error, WorkflowServiceError::Unavailable(_)),
                "receipt field {field}: {error:?}"
            );
        }
    }
}

#[test]
fn request_contract_rejects_caller_supplied_authority_and_unscoped_metadata() {
    let fixture = Fixture::new();
    let valid = fixture.request();
    serde_json::from_value::<HoldRequest>(valid.clone()).unwrap();
    for (field, value) in [
        ("holderId", json!(typed_id::generate("dhl"))),
        ("workerId", json!(WorkerId::mint())),
        ("deployHash", json!("a".repeat(64))),
        ("schema", json!("customer")),
        ("generation", json!(0)),
        ("appId", json!("not-an-app")),
    ] {
        let mut body = valid.clone();
        body[field] = value;
        assert!(
            serde_json::from_value::<HoldRequest>(body).is_err(),
            "{field}"
        );
    }
    for field in ["appId", "deployId", "generation"] {
        let mut body = valid.clone();
        body.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<HoldRequest>(body).is_err(),
            "{field}"
        );
    }
}

#[compio::test]
async fn asserted_client_requires_the_service_role_and_a_secure_unambiguous_origin() {
    let fixture = Fixture::new();
    for issuer in [
        CONTROL_AUDIENCE.to_owned(),
        "spiffe://zeroship.ai/svc/worker".to_owned(),
        format!("spiffe://zeroship.ai/svc/worker/{}", WorkerId::mint().as_str()),
        // An instance credential of the right service is still refused: the
        // journal belongs to the service, not to one of its replicas.
        format!("spiffe://zeroship.ai/svc/workflow/{}", WorkerId::mint().as_str()),
    ] {
        assert_eq!(
            RemoteDeploymentHolds::asserted(
                "https://control.example",
                auth(ServiceIssuer::parse(&issuer).unwrap()),
                fixture.app.clone(),
                Options::default(),
            )
            .unwrap_err(),
            WorkflowServiceError::PermissionDenied,
            "{issuer}"
        );
    }
    assert_eq!(
        RemoteDeploymentHolds::asserted(
            "https://control.example",
            Arc::new(ServiceAuth::unconfigured()),
            fixture.app.clone(),
            Options::default(),
        )
        .unwrap_err(),
        WorkflowServiceError::Unauthenticated
    );
    for url in [
        "https://control.example",
        "https://control.example:8443/",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
    ] {
        let client = fixture.client(url);
        assert_eq!(client.scope().app(), &fixture.app);
    }
    for url in [
        "http://control.example",
        "http://localhost",
        "http://192.0.2.1",
        "file:///secret",
        "https://user:secret@control.example",
        "https://control.example/prefix",
        "https://control.example?secret=x",
        "https://control.example#fragment",
        "not-a-url",
    ] {
        assert!(matches!(
            RemoteDeploymentHolds::asserted(
                url,
                fixture.service.clone(),
                fixture.app.clone(),
                Options::default()
            ),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
    for options in [
        Options {
            timeout: Duration::ZERO,
            ..Options::default()
        },
        Options {
            max_request_bytes: 0,
            ..Options::default()
        },
        Options {
            max_response_bytes: 0,
            ..Options::default()
        },
    ] {
        assert!(matches!(
            RemoteDeploymentHolds::asserted(
                "https://control.example",
                fixture.service.clone(),
                fixture.app.clone(),
                options
            ),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
    let client = fixture.client("http://127.0.0.1:1");
    for deployment in [
        "not-a-deployment".to_owned(),
        AppId::mint().as_str().to_owned(),
    ] {
        assert!(matches!(
            client.acquire(&deployment, fixture.generation).await,
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        assert!(matches!(
            client.release(&deployment, fixture.generation).await,
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
}

/// The journal-holding service's client is admitted by its role alone, names no
/// placement on the wire, and holds the journal scope for its app.
///
/// Every arm but the last is a refusal, and the last one is the control: the
/// role itself is admitted, so the refusals above are about WHICH signer rather
/// than about the constructor refusing everything.
#[compio::test]
async fn the_asserted_client_takes_the_service_role_and_names_no_placement() {
    let fixture = Fixture::new();
    let app = fixture.app.clone();
    let service = auth(ServiceIssuer::parse(WORKFLOW_ISSUER).unwrap());
    let client = RemoteDeploymentHolds::asserted(
        "https://control.example",
        service.clone(),
        app.clone(),
        Options::default(),
    )
    .unwrap();
    assert_eq!(client.scope(), &HoldScope::for_app(app.clone()));

    let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = RemoteDeploymentHolds::asserted(
        &format!("http://{}", listener.local_addr().unwrap()),
        service,
        app.clone(),
        Options::default(),
    )
    .unwrap();
    let server = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert_eq!(
            request.header.lines().next().unwrap(),
            format!(
                "POST {} HTTP/1.1",
                endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE.path_template()
            )
        );
        assert_eq!(
            request.body,
            json!({
                "appId": app,
                "deployId": fixture.deploy_id,
                "generation": fixture.generation,
            }),
            "an asserted hold request carries no placement field at all"
        );
        assert!(!request.bearer().is_empty());
        stream
            .write_all(response(200, &fixture.receipt(HoldState::Held)))
            .await
            .0
            .unwrap();
    };
    let call = async {
        assert_eq!(
            client
                .acquire(&fixture.deploy_id, fixture.generation)
                .await
                .unwrap(),
            fixture.receipt(HoldState::Held)
        );
    };
    compio::time::timeout(Duration::from_secs(10), async {
        futures::join!(server, call);
    })
    .await
    .expect("asserted hold exchange completed");
}

/// The journal holder is the app, so the same app has one holder across service
/// replicas and a different app has another.
#[compio::test]
async fn the_journal_holder_is_app_scoped() {
    let fixture = Fixture::new();
    let first = fixture.client("https://control.example");
    let second = fixture.client("https://control.example");
    let other_app = Fixture::new().client("https://control.example");
    assert_eq!(first.scope().holder(), second.scope().holder());
    assert_ne!(first.scope().holder(), other_app.scope().holder());
    assert!(typed_id::parse_with_prefix(first.scope().holder(), "dhl").is_ok());
}

#[compio::test]
async fn control_refusals_remain_closed_and_do_not_echo_remote_metadata() {
    let fixture = Fixture::new();
    assert_eq!(
        exchange(&fixture, HoldState::Held, 403, json!({"code":"denied"})).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    for body in [
        json!({"code":"denied", "schema":"customer-private-schema"}),
        json!({"code":"unknown", "message":"customer-private-schema"}),
    ] {
        let error = exchange(&fixture, HoldState::Held, 403, body)
            .await
            .unwrap_err();
        assert!(matches!(error, WorkflowServiceError::Unavailable(_)));
        assert!(!error.to_string().contains("customer-private-schema"));
    }
}
