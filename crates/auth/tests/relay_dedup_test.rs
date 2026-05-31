//! Relay `MessageID` idempotency: the reserve/confirm dedup split (sub-spec
//! §7.1 / §8 never-silent-drop). Live PG (`AUTH_DB_URL`); skips without it.
//!
//! The security bug this guards (security review finding 2): the dedup sentinel
//! must be committed only at a TERMINAL outcome, never before a retryable (503)
//! gate. If it were committed on the first sighting (the old `mark_seen`), a
//! transient-fault 503 followed by a Postmark retry of the SAME MessageID would
//! find the sentinel and be silently dropped — the message is never forwarded.
//! With the probe/commit split, a 503 path leaves NO sentinel, so the retry is
//! processed.

use compio_postgres::{connect, NoTls};
use zeroship_auth::store::relay;

#[allow(clippy::future_not_send)]
async fn pg_or_skip() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("conn err: {e}");
        }
    })
    .detach();
    Some(client)
}

/// The core regression: a transient 503 (no commit) followed by a retry must
/// still forward. We model the handler's two-phase dedup directly:
///   1. probe `already_seen` (read-only) — fresh ⇒ proceed
///   2. a transient gate returns 503 → we do NOT commit
///   3. Postmark retries the SAME MessageID → probe is STILL fresh ⇒ proceed
///      (it is NOT deduped away), and only NOW (a terminal forward) do we commit
///   4. a later replay → probe sees the committed sentinel ⇒ dropped
#[compio::test]
async fn transient_503_then_retry_is_not_deduped_away() {
    let Some(client) = pg_or_skip().await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };
    let message_id = format!("mid-{}", uuid::Uuid::new_v4().simple());

    // 1. First sighting — probe is fresh.
    assert!(
        !relay::already_seen(&client, &message_id)
            .await
            .expect("probe 1"),
        "first sighting must be fresh"
    );

    // 2. A transient gate (e.g. rate-limit spike / DB blip) returns 503: the
    //    handler does NOT commit the sentinel. (Nothing to do here — the point
    //    is that no commit_seen ran.)

    // 3. Postmark retries the SAME MessageID. The probe MUST still be fresh —
    //    the transient 503 did not poison the dedup. (Pre-fix, the first
    //    sighting committed the sentinel, so this would be `true` ⇒ silent drop
    //    ⇒ the message is never forwarded.)
    assert!(
        !relay::already_seen(&client, &message_id)
            .await
            .expect("probe 2 (retry)"),
        "a transient-503 retry must NOT be deduped away (never-silent-drop)"
    );

    // ... the retry now passes the gates and forwards (terminal) — commit.
    relay::commit_seen(&client, &message_id)
        .await
        .expect("commit on terminal forward");

    // 4. A genuine replay AFTER the terminal forward IS deduped.
    assert!(
        relay::already_seen(&client, &message_id)
            .await
            .expect("probe 3 (replay)"),
        "a replay after a terminal commit must be deduped"
    );

    // cleanup
    let key = format!("relay_seen:{message_id}");
    client
        .execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[&key])
        .await
        .expect("cleanup");
}

/// `commit_seen` is idempotent (re-commit refreshes the sentinel, never errors)
/// and a fresh id probes false until committed.
#[compio::test]
async fn commit_is_idempotent_and_probe_tracks_it() {
    let Some(client) = pg_or_skip().await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };
    let message_id = format!("mid-{}", uuid::Uuid::new_v4().simple());

    assert!(!relay::already_seen(&client, &message_id).await.expect("probe"));
    relay::commit_seen(&client, &message_id).await.expect("commit 1");
    assert!(relay::already_seen(&client, &message_id).await.expect("probe 2"));
    // Re-commit must not error (terminal paths may commit twice across retries).
    relay::commit_seen(&client, &message_id).await.expect("commit 2 idempotent");
    assert!(relay::already_seen(&client, &message_id).await.expect("probe 3"));

    let key = format!("relay_seen:{message_id}");
    client
        .execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[&key])
        .await
        .expect("cleanup");
}
