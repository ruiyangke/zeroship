//! `Notification::process_id` must name the NOTIFYING backend.
//!
//! `tests/integration.rs` already proves a notification is DELIVERED to an idle
//! listener, and asserts its channel and payload. It does not look at
//! `process_id`, and nothing else in the suite does either -- the only
//! `process_id()` calls elsewhere are `Client::process_id`, which is a
//! different value entirely (the caller's own backend).
//!
//! That matters because the accessor documents a specific claim -- "the process
//! ID of the notifying backend process" -- and a caller who never compares it
//! against anything cannot tell a correct value from a wrong one.
//!
//! What the mutations actually established, measured 2026-08-23: replacing the
//! populated field with a constant `0` turns the first test RED, so the field
//! is load-bearing. The other plausible wrong answer -- reporting the
//! LISTENER's own backend PID -- I could NOT construct, because the
//! connection's own process id is not in scope where the `Notification` is
//! built. The `assert_ne!` against the listener's PID therefore guards a
//! refactor that would bring it into scope; it is not evidence about today's
//! code, and should not be read as such.
//!
//! Lives in its own target rather than in `integration.rs` so it is legible on
//! its own and does not depend on that file's fixtures.

use compio_postgres::{AsyncMessage, Client, Error, NoTls};
use futures_util::StreamExt;
use std::time::Duration;

#[allow(dead_code)]
mod common;

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

fn test_url() -> Option<String> {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
}

async fn connect_client(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    Ok(client)
}

async fn backend_pid(client: &Client) -> i32 {
    client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the backend pid")
        .get::<_, i32>(0)
}

/// The PID on a notification is the notifier's, not the listener's.
#[compio::test]
async fn a_notification_carries_the_notifying_backends_process_id() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };

    // Listener. The async sink has to be taken before `run()` is spawned.
    let (listener, mut listener_connection) = compio_postgres::connect(&url, NoTls)
        .await
        .expect("connect the listener");
    let mut notifications = listener_connection.notifications();
    compio::runtime::spawn(async move {
        if let Err(error) = listener_connection.run().await {
            eprintln!("listener connection error: {error}");
        }
    })
    .detach();

    let channel = common::test_object_name("zs_notify_pid");
    listener
        .batch_execute(&format!("LISTEN {channel}"))
        .await
        .expect("LISTEN");

    // A SECOND connection sends the NOTIFY, so the two PIDs are genuinely
    // different values and the comparison below can fail.
    let notifier = connect_client(&url).await.expect("connect the notifier");
    let notifier_pid = backend_pid(&notifier).await;
    let listener_pid = backend_pid(&listener).await;
    assert_ne!(
        notifier_pid, listener_pid,
        "two connections must be two backends, or this test cannot discriminate"
    );

    notifier
        .batch_execute(&format!("NOTIFY {channel}, 'payload'"))
        .await
        .expect("NOTIFY");

    let received = compio::time::timeout(DELIVERY_TIMEOUT, notifications.next())
        .await
        .expect("the notification was never delivered");

    match received {
        Some(AsyncMessage::Notification(notification)) => {
            assert_eq!(
                notification.process_id(),
                notifier_pid,
                "process_id must name the backend that sent the NOTIFY"
            );
            assert_ne!(
                notification.process_id(),
                listener_pid,
                "process_id named the LISTENER, which is the plausible wrong answer"
            );
            assert_eq!(notification.channel(), channel);
            assert_eq!(notification.payload(), "payload");
        }
        other => panic!("expected a Notification, got {other:?}"),
    }
}

/// `NOTIFY chan` with no payload yields an EMPTY payload, not a missing one.
///
/// The wire always carries the payload string, so the empty case is the one
/// where a driver that treated "absent" and "empty" as interchangeable would
/// still look correct in the test above.
#[compio::test]
async fn a_notification_without_a_payload_delivers_an_empty_string() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };

    let (listener, mut listener_connection) = compio_postgres::connect(&url, NoTls)
        .await
        .expect("connect the listener");
    let mut notifications = listener_connection.notifications();
    compio::runtime::spawn(async move {
        if let Err(error) = listener_connection.run().await {
            eprintln!("listener connection error: {error}");
        }
    })
    .detach();

    let channel = common::test_object_name("zs_notify_empty");
    listener
        .batch_execute(&format!("LISTEN {channel}"))
        .await
        .expect("LISTEN");

    let notifier = connect_client(&url).await.expect("connect the notifier");
    notifier
        .batch_execute(&format!("NOTIFY {channel}"))
        .await
        .expect("NOTIFY with no payload");

    let received = compio::time::timeout(DELIVERY_TIMEOUT, notifications.next())
        .await
        .expect("the notification was never delivered");

    match received {
        Some(AsyncMessage::Notification(notification)) => {
            assert_eq!(
                notification.payload(),
                "",
                "a NOTIFY with no payload carries an empty payload"
            );
            assert_eq!(notification.channel(), channel);
        }
        other => panic!("expected a Notification, got {other:?}"),
    }
}
