//! The server this suite dials is one its own fixture started.
//!
//! Every DSN the suite connects with comes from `support::test_url()`. The
//! property held here is about WHO SERVES that address: a container this
//! checkout's fixture leased through `zeroship-testkit-server`, owned by the user
//! running the tests, and the very server the session lands in. A suite that
//! dials an address something else provisioned - a compose service, a
//! hand-started container, a server on another port - passes or fails on a
//! server nobody in the run started, and this test is red for it.
//!
//! The daemon's own port table names the container behind the dialled port, so
//! the check reads no fixture state: it asks Docker who publishes the port the
//! DSN names, and asks that container which cluster it runs.

use crate::support;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The lease directories every shared server of this checkout lives under, in
/// the canonical form a container's lease label carries.
fn this_checkouts_leases() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("compio-postgres lives under libs/");
    std::fs::canonicalize(root)
        .expect("canonicalize the checkout root")
        .join("target/zeroship-testkit")
}

/// Why a listed container is not a server this checkout's fixture started, or
/// `None` when it is one.
fn not_ours(labels: &HashMap<String, String>, leases: &Path, uid: u32) -> Option<String> {
    let Some(dir) = labels.get(zeroship_testkit_server::DIR_LABEL) else {
        return Some(format!(
            "it carries no {} label, so no shared-server lease started it",
            zeroship_testkit_server::DIR_LABEL
        ));
    };
    if !Path::new(dir).starts_with(leases) {
        return Some(format!(
            "its lease directory {dir} is not under this checkout's {}",
            leases.display()
        ));
    }
    match labels.get(zeroship_testkit_server::UID_LABEL) {
        Some(owner) if *owner == uid.to_string() => None,
        other => Some(format!(
            "it was started by uid {other:?}, not by the uid {uid} running these tests"
        )),
    }
}

#[compio::test]
async fn the_server_the_suite_dials_is_a_container_its_fixture_started() {
    let url = support::test_url();
    let config: compio_postgres::Config = url.parse().expect("the suite DSN parses");
    let hosts = config.get_hosts();
    assert!(
        matches!(
            hosts,
            [compio_postgres::config::Host::Tcp(host)] if host == "127.0.0.1" || host == "localhost"
        ),
        "the suite DSN must name the loopback port its fixture published, got {hosts:?}"
    );
    let port = *config
        .get_ports()
        .first()
        .expect("the suite DSN names the published port");

    let listed = zeroship_testkit_server::container_publishing(port)
        .expect("ask the daemon who publishes the suite's port")
        .unwrap_or_else(|| {
            panic!(
                "no running container publishes {port}, the port the suite dials, so its \
                 server is not one a fixture started"
            )
        });
    if let Some(reason) = not_ours(
        &listed.labels,
        &this_checkouts_leases(),
        zeroship_testkit_server::uid(),
    ) {
        panic!(
            "the suite dials port {port}, published by container {}, which is not a server \
             this checkout's fixture started: {reason}",
            listed.id
        );
    }

    // The cluster identity a session on the suite's DSN reports.
    let (client, connection) = compio_postgres::connect(&url, support::suite_tls())
        .await
        .unwrap_or_else(|error| support::postgres_unreachable(&url, &error));
    let driver = compio::runtime::spawn(async move {
        let _ = connection.run().await;
    });
    let dialled: String = client
        .query_one_scalar(
            "SELECT system_identifier::text FROM pg_control_system()",
            &[],
        )
        .await
        .expect("read the cluster's system identifier");
    drop(client);
    let _ = driver.await;
    assert!(
        !dialled.is_empty() && dialled.bytes().all(|byte| byte.is_ascii_digit()),
        "the dialled server reported no system identifier: {dialled:?}"
    );
    let inside = zeroship_testkit_server::exec_in_container(
        &listed.id,
        &[
            "psql",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-tAc",
            "SELECT system_identifier FROM pg_control_system()",
        ],
    )
    .expect("read the system identifier inside the fixture's container");
    assert!(
        inside.success(),
        "psql in {} failed: {}",
        listed.id,
        inside.stderr_text()
    );
    assert_eq!(
        String::from_utf8_lossy(&inside.stdout).trim(),
        dialled,
        "the session the suite's DSN opens lands in a different cluster from the one \
         container {} runs",
        listed.id
    );
}

/// The rejection controls for the check above: a port no container publishes
/// is answered with nothing rather than with some container, and each label
/// shape that is not this checkout's fixture is refused for its own reason.
#[test]
fn the_ownership_check_refuses_what_its_fixture_did_not_start() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a loopback port");
    let unpublished = held.local_addr().expect("held port").port();
    assert!(
        zeroship_testkit_server::container_publishing(unpublished)
            .expect("ask the daemon who publishes a port this process holds")
            .is_none(),
        "a port this process holds was attributed to a container"
    );

    let leases = Path::new("/checkout/target/zeroship-testkit");
    let ours = HashMap::from([
        (
            zeroship_testkit_server::DIR_LABEL.to_owned(),
            "/checkout/target/zeroship-testkit/compio-postgres-0123456789ab".to_owned(),
        ),
        (
            zeroship_testkit_server::UID_LABEL.to_owned(),
            "1000".to_owned(),
        ),
    ]);
    assert_eq!(
        not_ours(&ours, leases, 1000),
        None,
        "the fixture's own labels are accepted"
    );

    let mut unlabelled = ours.clone();
    unlabelled.remove(zeroship_testkit_server::DIR_LABEL);
    let mut foreign = ours.clone();
    foreign.insert(
        zeroship_testkit_server::DIR_LABEL.to_owned(),
        "/other-checkout/target/zeroship-testkit/compio-postgres-0123456789ab".to_owned(),
    );
    let mut stranger = ours;
    stranger.insert(zeroship_testkit_server::UID_LABEL.to_owned(), "0".to_owned());
    for (case, labels, says) in [
        (
            "an unlabelled container",
            unlabelled,
            "no shared-server lease",
        ),
        (
            "another checkout's server",
            foreign,
            "not under this checkout",
        ),
        ("another user's server", stranger, "not by the uid"),
    ] {
        let reason = not_ours(&labels, leases, 1000)
            .unwrap_or_else(|| panic!("{case} was accepted as this checkout's fixture"));
        assert!(
            reason.contains(says),
            "{case} was refused for the wrong reason: {reason}"
        );
    }
}
