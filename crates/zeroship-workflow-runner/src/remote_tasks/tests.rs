//! What a severed host's upload seam reports, answered from a socket this module
//! owns so the request is the one the real client serialized.

#![expect(
    clippy::future_not_send,
    reason = "the peer socket and its client stay on one compio runtime"
)]

use super::RemoteTasks;
use crate::{PayloadObjects, TaskPayloads};
use compio::io::{AsyncRead, AsyncWriteExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use zeroship_bundle::LocalDiskBlobStore;
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_peers::{ServiceAuth, ServiceKeyring},
    typed_id,
    workflow_coordination::{RequestId, WorkerId, WorkflowOutputRef},
};
use zeroship_storage::backend::OnceChunk;
use zeroship_workflow_client::{Options, WorkerCoordinator};

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

async fn drain(stream: &mut compio::net::TcpStream) {
    let mut seen = Vec::new();
    loop {
        let (read, buffer) = stream.read(Vec::with_capacity(4096)).await.unwrap();
        if read == 0 {
            return;
        }
        seen.extend_from_slice(&buffer[..read]);
        if seen.windows(4).any(|window| window == b"\r\n\r\n") {
            return;
        }
    }
}

/// Stage one upload against a peer that answers `reply`, and return the receipt.
async fn staged(reply: &Value) -> crate::payloads::UploadReceipt {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = WorkerCoordinator::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        auth(),
        Options::default(),
    )
    .unwrap();
    let body = serde_json::to_vec(reply).unwrap();
    let served = compio::runtime::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        drain(&mut stream).await;
        let mut response = format!(
            "HTTP/1.1 200 Test\r\nContent-Type: application/json\r\n\
             Connection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend(body);
        let _ = stream.write_all(response).await;
    });
    let (objects, _guard) = PayloadObjects::temporary();
    let artifacts = tempfile::tempdir().unwrap();
    let tasks = RemoteTasks::new(
        client,
        AppId::mint(),
        objects,
        Arc::new(LocalDiskBlobStore::new(artifacts.path().to_path_buf()).unwrap()),
        1024 * 1024,
        64 * 1024,
        Duration::from_secs(5),
    )
    .unwrap();
    let bytes = br#"{"ok":true}"#.to_vec();
    let reference = WorkflowOutputRef {
        hash: format!("{:x}", Sha256::digest(&bytes)),
        size: i64::try_from(bytes.len()).unwrap(),
        content_type: Some("application/json".into()),
    };
    let receipt = tasks
        .stage(
            &typed_id::generate(typed_id::WORKFLOW_DISPATCH_PREFIX),
            &"5".repeat(64).try_into().unwrap(),
            &RequestId::mint(),
            reference,
            Box::new(OnceChunk::new(bytes.into())),
        )
        .await
        .unwrap();
    served.await.unwrap();
    receipt
}

/// A reserved upload reports the confirm its settlement owes.
///
/// # What this catches
///
/// `confirm: None` is the IN-PROCESS contract: a host holding the object store
/// confirms under the lock it wrote under, so it owes nothing. A severed host
/// that answered `None` would compile and pass every existing test, and every
/// confirmation would be silently dropped -- the settlement would then carry a
/// frontier referencing rows still `uploading`, which `promote` refuses as a
/// missing payload. So the presence of the confirm is the property, and its
/// contents are checked against what the reservation answered rather than against
/// anything this host chose.
///
/// THE CONTROL differs in one variable, the reservation's own arm: a request an
/// earlier attempt already confirmed answers `Staged`, and that genuinely owes
/// nothing because there is no object left to write. So `Some` above is
/// attributable to the reservation rather than to a method that always answers
/// `Some`.
#[compio::test]
async fn a_reserved_upload_reports_the_confirm_its_settlement_owes() {
    let payload = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
    let reserved = staged(&json!({
        "kind": "reserved",
        "payloadId": payload,
        "expiresAt": 4_000_000_000_000_i64,
    }))
    .await;
    assert_eq!(reserved.id, payload);
    assert_eq!(
        reserved.confirm,
        Some(zeroship_workflow::service::delivery::PayloadConfirmation {
            payload_id: payload,
            expires_at: 4_000_000_000_000,
        }),
        "a severed host owes a confirm for every upload it reserved"
    );
}

#[compio::test]
async fn an_already_confirmed_upload_owes_nothing() {
    let payload = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
    let staged = staged(&json!({"kind": "staged", "payloadId": payload})).await;
    assert_eq!(staged.id, payload);
    assert_eq!(staged.confirm, None, "{:?}", staged.confirm);
}
