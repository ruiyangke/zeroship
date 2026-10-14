//! The address the workflow coordinator announces once its bind succeeds.
//!
//! A `--listen 127.0.0.1:0` run has no address to reserve, so the one on the
//! listening line is the process's own statement of what it bound. This case
//! reads that line and requires the process to own a LISTEN socket on it.

use crate::support::{platform, server_process::ServerProcess};

use serde_json::json;
use zeroship_core::service_assertion::ServiceSigningKey;
use zeroship_core::service_peers::{service_issuer, CONTROL_SERVICE_NAME};

/// A zero-port coordinator announces the port it BOUND, not the zero it was
/// given, and the process really listens there.
#[ntex::test]
async fn a_zero_port_server_announces_the_port_it_bound() {
    let fixture = platform::Platform::fresh_database().await;
    // The peer document the server loads must publish Control's key; the
    // key's own bytes are not exercised here, so a fresh one is what the
    // document names.
    let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let control_key = ServiceSigningKey::generate();
    let peers = fixture.work.path().join("zero-port-peers.json");
    platform::write_private(
        &peers,
        serde_json::to_vec(&json!({"keys":[{
            "iss":control.as_str(),"x":control_key.public_jwk_x()
        }]}))
        .unwrap(),
    );
    let server = ServerProcess::start_on_an_ephemeral_port(
        &fixture,
        &peers,
        fixture.work.path(),
        "zero-port",
    )
    .await;

    assert_ne!(
        server.address.port(),
        0,
        "a `--listen 127.0.0.1:0` coordinator announced the requested port, not the bound one"
    );
    assert!(
        zeroship_testkit::listen::child_listens(server.child_id(), server.address.port()),
        "the coordinator announced port {} but does not own a listening socket on it",
        server.address.port()
    );
}
