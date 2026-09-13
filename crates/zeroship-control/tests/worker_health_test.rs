//! The health monitor's red test, and it is deliberately a PAIR.
//!
//! WHY A PAIR: either arm alone passes against the wrong thing. An arm that only
//! checks "the monitor said unhealthy" is satisfied by a monitor that ALSO wrote
//! `gone` into the row -- the catastrophic implementation, since `gone` is
//! terminal and control holds no DELETE, so a transient blip would permanently
//! evict a healthy worker. An arm that only checks "the row still says active"
//! is satisfied by a monitor that observed nothing at all. Together they pin the
//! one shape that is correct: the two sources DISAGREE, the monitor is right
//! about liveness, and the column is untouched.
//!
//! THE ARMS OBSERVE THE PROBE RESULT AND THE ROW, NEVER A LOG LINE. A monitor
//! that logs "unhealthy" and changes nothing prints exactly what a working
//! monitor prints. That substitution survived three of four arms when it was
//! tried against the worker's startup refusal elsewhere in this redesign, so the
//! assertions below read `SweepReport::liveness_of` and a `SELECT status`, and
//! nothing here inspects tracing output.
//!
//! THE CONTROL DIFFERS IN ONE VARIABLE. Both instances are inserted by the same
//! helper, in the same table, with the same status, and are probed by the same
//! sweep. The only difference is whether a process is listening at the derived
//! address. Without the live half, "reports unhealthy" would be indistinguishable
//! from a monitor that reports unhealthy for everything.
//!
//! ISOLATION: this module asserts only on the instance ids IT created and deletes
//! only those rows. `live_db.rs` shares one process and one database across every
//! module in the target, so a fleet-wide DELETE here would be a cross-module
//! hazard rather than a cleanup.

use std::sync::Arc;

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use zeroship_control::worker_health::{self, HealthView, Liveness};

use crate::common;

/// A listener that answers every request with 200. This is the LIVE half of the
/// pair: a real socket at the address the row advertises.
async fn start_healthy_worker() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind healthy worker");
    let port = listener.local_addr().expect("local_addr").port();
    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            compio::runtime::spawn(async move {
                serve_ok(stream).await;
            })
            .detach();
        }
    })
    .detach();
    port
}

async fn serve_ok(mut stream: TcpStream) {
    // One read is enough: the probe sends a single small GET and waits for the
    // response, so there is no request this needs to accumulate across reads.
    let buf = vec![0u8; 1024];
    let compio::BufResult(read, _buf) = stream.read(buf).await;
    if !matches!(read, Ok(n) if n > 0) {
        return;
    }
    let _ = stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
        .await
        .0;
}

/// A port with nothing behind it. Binding and immediately dropping yields a port
/// the kernel is not serving; another process could in principle claim it before
/// the probe runs, which would turn this arm green for the wrong reason -- so the
/// arm asserts unhealthy, the failure direction that a stray listener cannot fake
/// into passing.
async fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind dead");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

async fn connect_pg() -> Arc<compio_postgres::Client> {
    let db_url = common::require_control_db();
    let (client, connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    Arc::new(client)
}

/// A typed id matching `worker_instances_id_shape`, minted by the one minter.
///
/// Composing a body by hand pins BOTH the width and the alphabet, so it stops
/// satisfying the CHECK the moment either moves - and it fails at insert time,
/// not at compile time.
fn fresh_instance_id() -> String {
    zeroship_core::typed_id::generate(zeroship_core::typed_id::WORKER_INSTANCE_PREFIX)
}

/// Insert an enrolled instance exactly as enrolment would leave it: `active`,
/// with a derived address. The test writes this row directly rather than driving
/// the enrolment endpoint, because what is under test is the monitor's treatment
/// of a row, not how the row came to exist.
async fn insert_active_instance(pg: &compio_postgres::Client, id: &str, port: i32) {
    let ring_key = vec![7u8; 32];
    let public_key = vec![9u8; 32];
    let host: std::net::IpAddr = "127.0.0.1".parse().expect("loopback parses");
    pg.execute(
        "INSERT INTO zeroship.worker_instances \
         (id, ring_key, public_key, advertise_host, advertise_port, status) \
         VALUES ($1, $2, $3, $4, $5, 'active')",
        &[&id, &ring_key, &public_key, &host, &port],
    )
    .await
    .expect("insert worker instance");
}

/// Read the column back. This is the second half of the pair: the fact the
/// monitor must NOT have written.
async fn status_of(pg: &compio_postgres::Client, id: &str) -> Option<String> {
    let rows = pg
        .query(
            "SELECT status FROM zeroship.worker_instances WHERE id = $1",
            &[&id],
        )
        .await
        .expect("read status");
    rows.first().map(|row| row.get(0))
}

async fn delete_instance(pg: &compio_postgres::Client, id: &str) {
    let _ = pg
        .execute(
            "DELETE FROM zeroship.worker_instances WHERE id = $1",
            &[&id],
        )
        .await;
}

#[compio::test]
async fn the_monitor_observes_liveness_without_writing_it_into_status() {
    let pg = connect_pg().await;

    let live_port = start_healthy_worker().await;
    let dead = dead_port().await;

    let live_id = fresh_instance_id();
    let dead_id = fresh_instance_id();
    insert_active_instance(&pg, &live_id, i32::from(live_port)).await;
    insert_active_instance(&pg, &dead_id, i32::from(dead)).await;

    // Both rows read `active` BEFORE the sweep. Without this the "still active"
    // assertion below could pass against a row that was never active.
    assert_eq!(
        status_of(&pg, &live_id).await.as_deref(),
        Some("active"),
        "the live instance starts out declared active"
    );
    assert_eq!(
        status_of(&pg, &dead_id).await.as_deref(),
        Some("active"),
        "the dead instance starts out declared active"
    );

    let view = HealthView::new();
    let client = cyper::Client::new();
    let report = worker_health::tick(&pg, &client, &view)
        .await
        .expect("sweep reads the registry");

    // (i) THE MONITOR IS RIGHT ABOUT LIVENESS, and the two sources disagree.
    assert_eq!(
        report.liveness_of(&dead_id),
        Some(Liveness::Unhealthy),
        "the sweep must report the killed instance unhealthy"
    );
    assert_eq!(
        report.liveness_of(&live_id),
        Some(Liveness::Healthy),
        "the control: a listening instance must report healthy, or 'unhealthy' \
         above proves nothing about the probe"
    );

    // The monitor's own state carries the same two facts, since that is what the
    // eligible set will read rather than the report.
    assert_eq!(view.latest(&dead_id), Some(Liveness::Unhealthy));
    assert_eq!(view.latest(&live_id), Some(Liveness::Healthy));

    // (ii) THE COLUMN IS UNTOUCHED. This is the arm that fails if the monitor
    // ever writes what it saw into `status` -- including the catastrophic case of
    // writing the terminal `gone` on a probe timeout.
    assert_eq!(
        status_of(&pg, &dead_id).await.as_deref(),
        Some("active"),
        "the unhealthy instance's row must STILL read active: the monitor keeps \
         what it saw, the table keeps what control declared"
    );
    assert_eq!(
        status_of(&pg, &live_id).await.as_deref(),
        Some("active"),
        "the healthy instance's row is equally untouched"
    );

    delete_instance(&pg, &live_id).await;
    delete_instance(&pg, &dead_id).await;
}
