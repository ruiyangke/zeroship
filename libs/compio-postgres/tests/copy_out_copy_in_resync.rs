//! `copy_out` must abort a `COPY FROM STDIN` that it cannot feed.
//!
//! Both tests send the same extended-protocol `COPY ... FROM STDIN` through
//! `Client::copy_out`. The finding uses an existing table, so PostgreSQL enters
//! COPY-IN mode and waits for data this API cannot provide. The control changes
//! only table existence: PostgreSQL refuses that request before COPY mode and
//! its original `Sync` returns the session to `ReadyForQuery` unaided.
//!
//! Every assertion about recovery reuses the exact `Client` that made the bad
//! request. A fresh connection would hide response-slot desynchronisation.

use compio_postgres::Client;
use compio_postgres::error::SqlState;
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const ABORT_MARKER: &str = "extended query execution cannot supply COPY data";

fn test_url() -> String {
    common::test_url()
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

#[compio::test]
async fn copy_out_of_copy_from_stdin_leaves_the_session_usable() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = common::test_object_name("cpg_copy_out_wrong_direction");
        client
            .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v int)"))
            .await
            .expect("create the COPY target");

        let failure = match client.copy_out(&format!("COPY {table} FROM STDIN")).await {
            Err(error) => error,
            Ok(_) => panic!("copy_out accepted a COPY direction it cannot feed"),
        };

        let value: i32 = client
            .query_one_scalar("SELECT 42::int4", &[])
            .await
            .expect("copy_out left the session in COPY-IN mode");
        assert_eq!(value, 42);
        assert_eq!(
            failure.code(),
            Some(&SqlState::QUERY_CANCELED),
            "the copy was not aborted by this driver: {}",
            common::error_chain(&failure)
        );
        assert!(
            common::error_chain(&failure).contains(ABORT_MARKER),
            "the failure did not carry this driver's abort reason: {}",
            common::error_chain(&failure)
        );
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle),
            "the COPY abort did not settle response-slot accounting"
        );
    })
    .await
    .expect("copy_out wrong-direction recovery exceeded its watchdog");
}

/// CONTROL: changing only the target table from existing to missing makes the
/// server reject the same COPY before COPY-IN mode. No driver abort is needed,
/// so this must stay green when the production recovery arm is removed.
#[compio::test]
async fn refused_copy_out_of_copy_from_stdin_was_already_synchronised() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let missing = common::test_object_name("cpg_copy_out_missing");

        let failure = match client
            .copy_out(&format!("COPY {missing} FROM STDIN"))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("COPY unexpectedly found its missing target"),
        };
        assert_eq!(failure.code(), Some(&SqlState::UNDEFINED_TABLE));

        let value: i32 = client
            .query_one_scalar("SELECT 43::int4", &[])
            .await
            .expect("a refused COPY start desynchronised the session");
        assert_eq!(value, 43);
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle)
        );
    })
    .await
    .expect("refused COPY control exceeded its watchdog");
}
