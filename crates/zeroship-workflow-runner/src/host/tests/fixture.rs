use super::*;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::{future::LocalBoxFuture, FutureExt};
use std::sync::Arc;
use zeroship_core::{
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{endpoints, verify_service_call, ServiceEndpoint},
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{RunId, WorkerId, AUDIENCE},
    workflow_jobs::{ClaimJobs, Delivery, DeploymentId, JobId, JobOperation, JobSpec},
};
use zeroship_workflow::service::{delivery::AcceptedJob, TaskAssignment};
use zeroship_workflow_client::Options;

pub(super) struct Fixture {
    auth: Arc<ServiceAuth>,
    /// Apps the host's factory was asked to open.
    pub opened: Rc<Cell<usize>>,
    pub worker: WorkerId,
    pub calls: Rc<RefCell<Vec<Call>>>,
    pub client_options: Options,
}

impl Fixture {
    pub fn new() -> Self {
        let worker = WorkerId::mint();
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap();
        Self {
            auth: Arc::new(ServiceAuth::new(
                ServiceKeyring::from_parts(
                    issuer,
                    ServiceSigningKey::generate(),
                    ServiceTrustBundle::new(),
                )
                .unwrap(),
                Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
            )),
            opened: Rc::new(Cell::new(0)),
            worker,
            calls: Rc::new(RefCell::new(Vec::new())),
            client_options: Options::default(),
        }
    }

    pub fn host(
        &self,
        client: WorkerCoordinator,
        options: HostOptions,
    ) -> WorkerHost<Unpreparable> {
        self.try_host(client, options).unwrap()
    }

    pub fn try_host(
        &self,
        client: WorkerCoordinator,
        options: HostOptions,
    ) -> Result<WorkerHost<Unpreparable>, WorkflowServiceError> {
        WorkerHost::new(
            client,
            Unpreparable {
                opened: self.opened.clone(),
            },
            Rc::new(|_: &AppId| true),
            options,
        )
    }

    pub fn claims(&self) -> Vec<ClaimJobs> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                Call::Claim(request) => Some(request.clone()),
                Call::Release(_) => None,
            })
            .collect()
    }

    pub fn releases(&self) -> Vec<Value> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                Call::Release(body) => Some(body.clone()),
                Call::Claim(_) => None,
            })
            .collect()
    }

    /// An advance delivery to this worker for `app`, with the journal
    /// acceptance the same reply carries, as the claim route answers one.
    pub fn delivery(&self, app: &AppId) -> (Delivery, TaskAssignment, Value) {
        let delivery = Delivery {
            job: JobSpec {
                id: JobId::mint(),
                app_id: app.clone(),
                operation: JobOperation::Advance {
                    deployment_id: DeploymentId::mint(),
                    run_id: RunId::mint(),
                    generation: 1,
                    revision: 1.try_into().unwrap(),
                },
                available_at: 0.try_into().unwrap(),
            },
            worker_id: self.worker.clone(),
            attempt: 1.try_into().unwrap(),
            deadline: 1.try_into().unwrap(),
        };
        let assignment: TaskAssignment = serde_json::from_value(json!({
            "id": "task_delivered", "token": "0".repeat(64), "generation": 1, "epoch": 1,
            "deadline": 0, "leaseMs": 30_000,
            "invocation": {
                "appId": app.as_str(), "deployId": "dep_delivered", "deployHash": "a".repeat(64),
                "runId": "run_delivered", "generation": 1, "workflowName": "Example",
                "phase": "running",
                "trigger": {"input": null, "startedAt": "2026-09-30T00:00:00Z",
                    "runId": "run_delivered", "workflowName": "Example"},
                "journal": [],
            },
        }))
        .unwrap();
        let accepted = serde_json::to_value(AcceptedJob::Execute {
            assignment: Box::new(assignment.clone()),
            remaining_ms: 30_000.try_into().unwrap(),
        })
        .unwrap();
        let body = json!({
            "lease": {"delivery": delivery, "remainingMs": 30_000, "attemptRemainingMs": 60_000},
            "accepted": accepted,
        });
        (delivery, assignment, body)
    }
}

/// A claim reply holding `deliveries`, past every app it visited.
pub(super) fn batch(deliveries: &[Value], lap_complete: bool) -> Reply {
    Reply::ok(json!({"deliveries": deliveries, "after": null, "lapComplete": lap_complete}))
}

#[derive(Debug, Clone)]
pub(super) enum Call {
    Claim(ClaimJobs),
    Release(Value),
}

impl Call {
    fn endpoint(&self) -> ServiceEndpoint {
        match self {
            Self::Claim(_) => endpoints::WORKFLOW_JOB_CLAIM,
            Self::Release(_) => endpoints::WORKFLOW_JOB_RELEASE,
        }
    }

    fn parse(path: &str, body: Value) -> Self {
        if path == endpoints::WORKFLOW_JOB_CLAIM.path_template() {
            Self::Claim(serde_json::from_value(body).unwrap())
        } else if path == endpoints::WORKFLOW_JOB_RELEASE.path_template() {
            Self::Release(body)
        } else {
            panic!("unexpected worker operation: {path}")
        }
    }
}

pub(super) struct Reply {
    status: u16,
    body: Value,
    gate: Option<oneshot::Receiver<()>>,
    abandoned: bool,
    sent: Option<oneshot::Sender<()>>,
}

impl Reply {
    pub const fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            gate: None,
            abandoned: false,
            sent: None,
        }
    }

    pub fn gated(mut self, blocked: oneshot::Receiver<()>) -> Self {
        assert!(self.gate.replace(blocked).is_none());
        self
    }

    pub const fn abandoned(mut self) -> Self {
        self.abandoned = true;
        self
    }

    pub fn sent(mut self, notification: oneshot::Sender<()>) -> Self {
        assert!(self.sent.replace(notification).is_none());
        self
    }

    async fn send(self, mut stream: compio::net::TcpStream) {
        if let Some(blocked) = self.gate {
            blocked.await.unwrap();
        }
        let body = serde_json::to_vec(&self.body).unwrap();
        let mut response = format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", self.status, body.len()).into_bytes();
        response.extend(body);
        let result = async {
            stream.write_all(response).await.0?;
            stream.flush().await
        }
        .await;
        if self.abandoned {
            assert!(
                result.is_ok()
                    || result.is_err_and(|error| matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    )),
                "abandoned response failed for an unrelated reason"
            );
        } else {
            result.unwrap();
        }
        if let Some(sent) = self.sent {
            sent.send(()).unwrap();
        }
    }
}

pub(super) fn peer<'a>(
    fixture: &'a Fixture,
    mut respond: impl FnMut(&Call) -> Reply + 'a,
    test: impl AsyncFnOnce(WorkerCoordinator) + 'a,
) -> LocalBoxFuture<'a, ()> {
    Box::pin(async move {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WorkerCoordinator::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            fixture.auth.clone(),
            fixture.client_options.clone(),
        )
        .unwrap();
        let (issuer, key) = fixture.auth.signing_identity().unwrap();
        let mut trust = ServiceTrustBundle::new();
        trust.trust_signing_key(issuer, key.key_id(), key).unwrap();
        let verifier = ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()));
        let (done, completed) = oneshot::channel();
        let server = async {
            let mut completed = completed;
            let mut replies = Vec::new();
            loop {
                let (mut stream, _) =
                    match futures::future::select(completed, listener.accept().boxed_local()).await
                    {
                        Either::Left((result, _)) => {
                            result.unwrap();
                            break;
                        }
                        Either::Right((socket, remaining)) => {
                            completed = remaining;
                            socket.unwrap()
                        }
                    };
                let Some(request) = request(&mut stream).await else {
                    continue;
                };
                let call = Call::parse(&request.path, request.body);
                verify_service_call(
                    &verifier,
                    Some(&request.authorization),
                    AUDIENCE,
                    call.endpoint(),
                )
                .await
                .unwrap();
                assert!(
                    verify_service_call(
                        &verifier,
                        Some(&request.authorization),
                        AUDIENCE,
                        call.endpoint()
                    )
                    .await
                    .is_err(),
                    "assertion reuse must be rejected"
                );
                fixture.calls.borrow_mut().push(call.clone());
                let reply = respond(&call);
                replies.push(compio::runtime::spawn(reply.send(stream)));
            }
            for reply in replies {
                reply.await.unwrap();
            }
        };
        compio::time::timeout(Duration::from_secs(15), async {
            futures::join!(server, async {
                test(client).await;
                done.send(()).unwrap();
            });
        })
        .await
        .expect("worker host HTTP fixture hung");
    })
}

struct Request {
    path: String,
    authorization: String,
    body: Value,
}

async fn request(stream: &mut compio::net::TcpStream) -> Option<Request> {
    let mut bytes = Vec::new();
    loop {
        let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
        // Shutdown may cancel a periodic exchange before its request is fully
        // written. Incomplete requests grant no authority and are not dispatched.
        let read = match read {
            Ok(0) => return None,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                return None
            }
            other => other.unwrap(),
        };
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 16 * 1024);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header = std::str::from_utf8(&bytes[..end]).unwrap();
        let mut lines = header.lines();
        let mut start = lines.next().unwrap().split_whitespace();
        assert_eq!(start.next(), Some("POST"));
        let path = start.next().unwrap().to_owned();
        let mut length = None;
        let mut authorization = None;
        for line in lines {
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                assert!(length.is_none());
                length = Some(value.trim().parse::<usize>().unwrap());
            }
            if name.eq_ignore_ascii_case("authorization") {
                assert!(authorization.is_none());
                authorization = Some(value.trim().to_owned());
            }
        }
        let length = length.unwrap();
        if bytes.len() < end + 4 + length {
            continue;
        }
        assert_eq!(bytes.len(), end + 4 + length);
        return Some(Request {
            path,
            authorization: authorization.unwrap(),
            body: serde_json::from_slice(&bytes[end + 4..]).unwrap(),
        });
    }
}

/// A factory that refuses every app, counting what it was asked to open.
pub(super) struct Unpreparable {
    opened: Rc<Cell<usize>>,
}

impl CreatorFactory for Unpreparable {
    /// `WorkerHost` pairs a factory with the crossed transport, so this is the
    /// only shape it accepts.
    type Journal = ();

    fn open<'a>(
        &'a self,
        _: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<CreatorRuntime<()>, WorkflowServiceError>> {
        self.opened.set(self.opened.get() + 1);
        Box::pin(std::future::ready(Err(WorkflowServiceError::Unavailable(
            "this fixture prepares no app".into(),
        ))))
    }
}
