//! A real worker process, stopped the way an orchestrator stops it, retires
//! its own joined instance on the way out.
//!
//! The in-process arms in `internal_service_auth_test` bind the endpoint; this
//! one binds the seam in the worker binary that calls it - after the server
//! has drained, on the graceful path only - against the real Control, the real
//! database and the join the worker performed at boot.

use crate::workflow_fleet::Fleet;

async fn database(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .expect("fleet database connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// SIGTERM moves the worker's row from `active` to `gone`, and leaves the
/// signer that admitted it exactly as it was.
///
/// The signer is the paired control: retirement is the INSTANCE declaring its
/// own exit, so the signer stays active and could admit the replacement a
/// restart draws. A shutdown path that revoked its signer, or that wrote
/// nothing at all, fails one of the two.
#[compio::test]
async fn a_worker_stopped_gracefully_retires_its_own_instance() {
    let mut fleet = Fleet::with_advance(false);
    let pg = database(&fleet.database.url()).await;
    let instances: Vec<(String, String, String)> = pg
        .query(
            "SELECT id, status, join_signer_id FROM zeroship.worker_instances \
              ORDER BY registered_at",
            &[],
        )
        .await
        .expect("read the joined instances")
        .into_iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    assert_eq!(
        instances.len(),
        1,
        "the fleet's single worker joins exactly once: {instances:?}"
    );
    let (instance_id, before, signer_id) = instances[0].clone();
    assert_eq!(before, "active");

    let exit = fleet.terminate("worker");

    let after: String = pg
        .query_one(
            "SELECT status FROM zeroship.worker_instances WHERE id = $1",
            &[&instance_id],
        )
        .await
        .expect("the retired row is retained")
        .get(0);
    let signer: String = pg
        .query_one(
            "SELECT status FROM zeroship.worker_join_signers WHERE id = $1",
            &[&signer_id],
        )
        .await
        .expect("the signer row")
        .get(0);
    assert!(exit.success(), "a SIGTERM is a clean exit, got {exit}; see {}", fleet.logs.display());
    assert_eq!(
        after, "gone",
        "a gracefully stopped worker must retire its own instance; see {}",
        fleet.logs.display()
    );
    assert_eq!(
        signer, "active",
        "retiring an instance never touches the signer that admitted it"
    );
}
