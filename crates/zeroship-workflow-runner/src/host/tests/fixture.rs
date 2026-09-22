use super::*;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::future::LocalBoxFuture;
use zeroship_core::{
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{endpoints, verify_service_call, ServiceEndpoint},
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{RegisteredWorker, ScopePage, WorkerId, AUDIENCE},
};
use zeroship_workflow_client::Options;

pub(super) struct Fixture {
    auth: Arc<ServiceAuth>,
    pub policies: Arc<HostPolicies>,
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
            policies: Arc::new(HostPolicies::default()),
            worker,
            calls: Rc::new(RefCell::new(Vec::new())),
            client_options: Options::default(),
        }
    }

    pub fn registration(&self, request: &RegisterWorker) -> Reply {
        Reply::ok(json!(RegisteredWorker {
            worker_id: self.worker.clone(),
            capacity: request.capacity,
            state: request.state,
            expires_at: 0.try_into().unwrap(),
        }))
    }

    pub fn host(
        &self,
        client: WorkerCoordinator,
        options: HostOptions,
    ) -> WorkerHost<UnexpectedCreator> {
        WorkerHost::new(
            client,
            self.policies.clone(),
            UnexpectedCreator,
            ReadyApps::default(),
            options,
        )
        .unwrap()
    }

    pub fn registrations(&self) -> Vec<RegisterWorker> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                Call::Register(request) => Some(request.clone()),
                Call::Assignments(_) => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(super) enum Call {
    Register(RegisterWorker),
    Assignments(ScopePage),
}

impl Call {
    fn endpoint(&self) -> ServiceEndpoint {
        match self {
            Self::Register(_) => endpoints::WORKFLOW_REGISTER,
            Self::Assignments(_) => endpoints::WORKFLOW_ASSIGNMENTS,
        }
    }

    fn parse(path: &str, body: Value) -> Self {
        if path == endpoints::WORKFLOW_REGISTER.path_template() {
            Self::Register(serde_json::from_value(body).unwrap())
        } else if path == endpoints::WORKFLOW_ASSIGNMENTS.path_template() {
            Self::Assignments(serde_json::from_value(body).unwrap())
        } else {
            panic!("unexpected worker metadata operation: {path}")
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

    pub fn failure(status: u16, code: &str) -> Self {
        Self {
            status,
            body: json!({"code":code}),
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
            fixture.client_options,
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

pub(super) struct UnexpectedCreator;

impl CreatorFactory for UnexpectedCreator {
    async fn open(
        &self,
        _: &AssignedScope,
        _: &PolicyBinding,
        _: Rc<dyn zeroship_workflow::service::IngressEpochs>,
    ) -> Result<CreatorRuntime, WorkflowServiceError> {
        panic!("empty placement fixture cannot authorize creator setup")
    }
}
