//! What the remote backend refuses to send, and where its bytes come from.
//!
//! Every case answers the real [`WorkerCoordinator`] from a socket this module
//! owns, so the request the backend sends is the one the client serialized and
//! the reply it reads is one this test wrote. What that buys over a hand-built
//! client is the SERIALIZATION: a start whose options grew a descriptor field
//! would put it on the wire here, where the peer records the body.

#![expect(
    clippy::future_not_send,
    reason = "the peer socket and its client stay on one compio runtime"
)]

use super::RemoteBackend;
use crate::PayloadObjects;
use compio::io::{AsyncRead, AsyncWriteExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_peers::{ServiceAuth, ServiceKeyring},
    typed_id,
    workflow_coordination::{AssignedScope, RunId, WorkerId, WorkflowOutputRef},
};
use zeroship_workflow::{
    backend::WorkflowBackend,
    operations::{ConflictPolicy, StartOptions},
    WorkflowServiceError,
};
use zeroship_workflow_client::{Options, WorkerCoordinator};

const READ_LIMIT: usize = 64 * 1024;

fn auth() -> Arc<ServiceAuth> {
    let issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        WorkerId::mint().as_str()
    ))
    .unwrap();
    Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(
            issuer,
            ServiceSigningKey::generate(),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}

fn reference(bytes: &[u8]) -> WorkflowOutputRef {
    WorkflowOutputRef {
        hash: format!("{:x}", Sha256::digest(bytes)),
        size: i64::try_from(bytes.len()).unwrap(),
        content_type: Some("application/json".into()),
    }
}

/// One canned exchange, and what the backend asked for to get it.
struct Observed {
    path: String,
    body: Value,
}

/// Serve exactly one request with `status` and `reply`, and hand the test both
/// the backend and what the peer saw.
///
/// `None` from the peer means the backend never sent anything, which is how an
/// in-process refusal is told apart from a served one.
async fn peer<T>(
    status: u16,
    reply: &Value,
    objects: PayloadObjects,
    call: impl AsyncFnOnce(RemoteBackend) -> T,
) -> (T, Option<Observed>) {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = WorkerCoordinator::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        auth(),
        Options::default(),
    )
    .unwrap();
    let scope = AssignedScope {
        app_id: app(),
        assignment_revision: 1.try_into().unwrap(),
    };
    let backend = RemoteBackend::new(client, scope, objects, READ_LIMIT).unwrap();
    let body = serde_json::to_vec(reply).unwrap();
    let served = compio::runtime::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let observed = request(&mut stream).await;
        let mut response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\n\
             Connection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend(body);
        let _ = stream.write_all(response).await;
        observed
    });
    let result = compio::time::timeout(Duration::from_secs(10), call(backend))
        .await
        .expect("the remote backend call hung");
    // A refusal taken in process sends nothing, so the peer is still waiting on
    // its first connection. Give it a bounded moment and then say so rather than
    // hanging, because "nothing arrived" is the assertion some cases make.
    let waited = futures::future::select(
        served,
        Box::pin(compio::time::sleep(Duration::from_millis(250))),
    )
    .await;
    let observed = match waited {
        futures::future::Either::Left((observed, _)) => Some(observed.unwrap()),
        futures::future::Either::Right(((), served)) => {
            let _ = served.cancel().await;
            None
        }
    };
    (result, observed)
}

/// The app every object in a case's store belongs to, and the app its scope
/// names. One per test thread, so a key written under it is reachable only
/// through that case's own backend.
fn app() -> AppId {
    APP.with(Clone::clone)
}

thread_local! {
    static APP: AppId = AppId::mint();
}

async fn request(stream: &mut compio::net::TcpStream) -> Observed {
    let mut bytes = Vec::new();
    loop {
        let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0, "the peer saw a closed connection");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 256 * 1024);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header = std::str::from_utf8(&bytes[..end]).unwrap();
        let length: usize = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        if bytes.len() < end + 4 + length {
            continue;
        }
        return Observed {
            path: header
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned(),
            body: serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap(),
        };
    }
}

/// A creator-supplied `input_ref` never reaches the service.
///
/// The trait hands this backend a whole `StartOptions`, descriptor field and
/// all, so the fence has to be here. It REFUSES rather than dropping, and the
/// refusal is observable twice: the call answers `InvalidRequest`, and the peer
/// saw NO REQUEST AT ALL -- so the descriptor did not reach the wire and then get
/// ignored, it never left this process.
///
/// The control is the same call with no descriptor. It is what shows the peer was
/// reachable and the request serializable, so "nothing arrived" above is the
/// refusal and not a broken fixture -- and its recorded body is where a
/// descriptor would show up if `CreatorStartOptions` ever grew a field for one.
#[compio::test]
async fn a_remote_start_refuses_a_creator_supplied_payload_descriptor() {
    let (objects, _dir) = PayloadObjects::temporary();
    let run = RunId::mint();
    let named = StartOptions {
        input_ref: Some(reference(b"someone else's object")),
        key: Some("order-7".into()),
        on_conflict: ConflictPolicy::Reject,
    };
    let (refused, observed) = peer(
        200,
        &json!({"id":run.as_str(),"state":"queued"}),
        objects.clone(),
        async |backend| {
            backend
                .start("orders".into(), json!({"order":7}), named.clone())
                .await
        },
    )
    .await;
    assert!(
        matches!(refused, Err(WorkflowServiceError::InvalidRequest(_))),
        "{refused:?}"
    );
    assert!(
        observed.is_none(),
        "a start naming a payload object reached the service"
    );

    // CONTROL: one variable differs, the descriptor.
    let (started, observed) = peer(
        200,
        &json!({"id":run.as_str(),"state":"queued"}),
        objects,
        async |backend| {
            backend
                .start(
                    "orders".into(),
                    json!({"order":7}),
                    StartOptions {
                        input_ref: None,
                        ..named
                    },
                )
                .await
        },
    )
    .await;
    assert_eq!(started.unwrap().id, run.as_str());
    let observed = observed.expect("the control start never reached the peer");
    assert_eq!(observed.path, "/v1/runs/start");
    assert_eq!(observed.body["input"], json!({"order":7}));
    assert_eq!(
        observed.body["options"],
        json!({"key":"order-7","onConflict":"reject"})
    );
    assert!(
        !serde_json::to_string(&observed.body)
            .unwrap()
            .contains("nputRef"),
        "the start request carried a payload descriptor: {}",
        observed.body
    );
}

/// A located step output is opened from the store, not read off the wire.
///
/// The reply names a key and a descriptor and carries no body, so the bytes the
/// call returns can only have come from the object store. The CONTROL is the same
/// reply against a store the object is absent from: it refuses, which is what
/// shows the first arm read the store rather than reconstructing anything from
/// the reply.
#[compio::test]
async fn a_remote_step_output_read_opens_the_located_object() {
    let bytes = br#"{"value":"from the store"}"#;
    let key = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
    let located = json!({
        "kind":"object",
        "payload":{"payloadId":key,"reference":reference(bytes)},
    });
    let (stocked, _stocked_dir) = PayloadObjects::temporary();
    stocked
        .put(&app(), &key, bytes, Some("application/json"))
        .await;
    let run = RunId::mint();
    let (read, observed) = peer(200, &located, stocked, async |backend| {
        backend
            .read_step_output(run.as_str().to_owned(), "charge".into(), 0)
            .await
    })
    .await;
    assert_eq!(read.unwrap(), bytes);
    let observed = observed.expect("the located read never reached the peer");
    assert_eq!(observed.path, "/v1/runs/step-output");
    assert_eq!(observed.body["name"], json!("charge"));
    assert_eq!(observed.body["occurrence"], json!(0));

    // CONTROL: the same reply, against a store holding no such object.
    let (empty, _empty_dir) = PayloadObjects::temporary();
    let (missing, _) = peer(200, &located, empty, async |backend| {
        backend
            .read_step_output(run.as_str().to_owned(), "charge".into(), 0)
            .await
    })
    .await;
    assert!(
        matches!(missing, Err(WorkflowServiceError::Unavailable(_))),
        "{missing:?}"
    );

    // And the inline arm needs no object at all: the journal held the value, so
    // the value is the whole read.
    let (inline_store, _inline_dir) = PayloadObjects::temporary();
    let (inline, _) = peer(
        200,
        &json!({"kind":"inline","value":{"value":"from the journal"}}),
        inline_store,
        async |backend| {
            backend
                .read_step_output(run.as_str().to_owned(), "charge".into(), 0)
                .await
        },
    )
    .await;
    assert_eq!(inline.unwrap(), br#"{"value":"from the journal"}"#);
}

/// A run output read locates one object, and a key that is not a payload id is
/// refused before the store is asked.
#[compio::test]
async fn a_remote_run_output_read_refuses_a_key_that_is_not_a_payload_id() {
    let bytes = br#"{"value":"final"}"#;
    let run = RunId::mint();
    // A run id is a well-formed typed id of the WRONG entity, which is the
    // substitution a key-shaped check must catch.
    let (store, _dir) = PayloadObjects::temporary();
    store
        .put(&app(), run.as_str(), bytes, Some("application/json"))
        .await;
    let (refused, observed) = peer(
        200,
        &json!({"payloadId":run.as_str(),"reference":reference(bytes)}),
        store.clone(),
        async |backend| backend.read_output(run.as_str().to_owned()).await,
    )
    .await;
    assert!(
        matches!(refused, Err(WorkflowServiceError::Unavailable(_))),
        "{refused:?}"
    );
    assert_eq!(
        observed
            .expect("the output read never reached the peer")
            .path,
        "/v1/runs/output"
    );

    // CONTROL: the same object under a payload key is served. One variable
    // differs, so the refusal above is about the key and not about the object.
    let key = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
    store
        .put(&app(), &key, bytes, Some("application/json"))
        .await;
    let (read, _) = peer(
        200,
        &json!({"payloadId":key,"reference":reference(bytes)}),
        store,
        async |backend| backend.read_output(run.as_str().to_owned()).await,
    )
    .await;
    assert_eq!(read.unwrap(), bytes);
}

/// The engine's refusal crosses back in the engine's own terms, and a transport
/// failure does not become one.
///
/// A creator branches on the code and reads the message, so a refusal that
/// arrived as `conflict` must still be a conflict here. The control is the arm
/// beside it: a reply this client will not believe becomes `Unavailable`, which
/// carries no creator-actionable claim about whether the call took effect.
#[compio::test]
async fn a_remote_refusal_keeps_its_code_and_a_broken_reply_does_not() {
    let (objects, _dir) = PayloadObjects::temporary();
    let (conflict, _) = peer(
        409,
        &json!({"code":"conflict","message":"workflow run already exists for key"}),
        objects.clone(),
        async |backend| {
            backend
                .start("orders".into(), json!({}), StartOptions::default())
                .await
        },
    )
    .await;
    match conflict {
        Err(WorkflowServiceError::Conflict(message)) => {
            assert_eq!(message, "workflow run already exists for key");
        }
        other => panic!("{other:?}"),
    }

    // CONTROL: a status and a body that disagree about which refusal it is.
    let (disagreeing, _) = peer(
        409,
        &json!({"code":"not_found","message":"workflow run"}),
        objects.clone(),
        async |backend| {
            backend
                .start("orders".into(), json!({}), StartOptions::default())
                .await
        },
    )
    .await;
    assert!(
        matches!(disagreeing, Err(WorkflowServiceError::Unavailable(_))),
        "{disagreeing:?}"
    );

    // And a receipt naming an id that is not a RUN is not a started run: the
    // caller would go on to name it in every later call for this creator.
    let (impostor, _) = peer(
        200,
        &json!({"id":AppId::mint().as_str(),"state":"queued"}),
        objects,
        async |backend| {
            backend
                .start("orders".into(), json!({}), StartOptions::default())
                .await
        },
    )
    .await;
    assert!(
        matches!(impostor, Err(WorkflowServiceError::Unavailable(_))),
        "{impostor:?}"
    );
}
