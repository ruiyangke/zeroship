//! The Redpanda adapter against the one broker a worktree shares.
//!
//! Every case joins [`zeroship_testkit::redpanda::broker()`] rather than
//! starting a broker of its own: several brokers starting at once exhaust the
//! host's global `fs.aio-max-nr`, and Seastar refuses to start under that
//! condition. Each case mints its own topic and consumer group, so cases share
//! the broker without sharing data.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::executor::block_on;
use serde_json::json;
use zeroship_stream::adapters;
use zeroship_stream::{StreamConfig, StreamOffset, StreamRegistry};

/// The brokers string of the worktree's shared broker.
fn brokers() -> String {
    zeroship_testkit::redpanda::broker().brokers()
}

/// The transport settings for `group` against `topic`.
fn config(brokers: &str, topic: &str, group: &str, client: &str) -> StreamConfig {
    StreamConfig::from(json!({
        "brokers": brokers,
        "topic": topic,
        "group.id": group,
        "client_id": client,
        "message_timeout_ms": 10000,
        "publish_timeout_ms": 10000,
        "poll_timeout_ms": 250,
        "auto_offset_reset": "earliest"
    }))
}

/// Publish `events` under a throwaway consumer group and drop the transport.
///
/// The broker auto-creates the topic on the first produce. A consumer that
/// subscribed before that fetched metadata for a topic that did not exist yet
/// and reports it unknown on its first poll, so the reader is built afterwards
/// and subscribes to the topic that now exists. The publisher's own group is
/// discarded so its consumer never competes with the reader's, because a
/// transport builds a consumer alongside its producer.
async fn publish(
    registry: &StreamRegistry,
    brokers: &str,
    topic: &str,
    group: &str,
    client: &str,
    events: &[(&str, u32)],
) {
    let publisher = registry
        .build(
            "redpanda",
            &config(brokers, topic, &format!("{group}-publisher"), client),
        )
        .expect("redpanda publisher builds");
    for (key, seq) in events {
        let payload = format!("{key}:{seq}");
        publisher
            .publish(topic, key.as_bytes(), payload.as_bytes())
            .await
            .expect("publish test event");
    }
    drop(publisher);
}

#[test]
fn redpanda_roundtrip_preserves_per_key_order_and_commits_offsets() {
    let brokers = brokers();

    block_on(async move {
        let suffix = unique_suffix();
        let topic = format!("zeroship-stream-roundtrip-{suffix}");
        let group = format!("zeroship-stream-roundtrip-group-{suffix}");
        let client = format!("zeroship-stream-test-{suffix}");

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);

        let events = [
            ("subject-a", 0),
            ("subject-b", 0),
            ("subject-a", 1),
            ("subject-c", 0),
            ("subject-b", 1),
            ("subject-a", 2),
            ("subject-c", 1),
            ("subject-b", 2),
            ("subject-a", 3),
        ];
        let mut expected = BTreeMap::<String, Vec<u32>>::new();
        for (key, seq) in events {
            expected.entry(key.to_string()).or_default().push(seq);
        }
        publish(
            &registry,
            &brokers,
            &topic,
            &group,
            &client,
            &events,
        )
        .await;

        let transport = registry
            .build("redpanda", &config(&brokers, &topic, &group, &client))
            .expect("redpanda transport builds");
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while received.len() < events.len() && Instant::now() < deadline {
            let records = transport.poll(events.len()).await.expect("poll records");
            received.extend(records);
        }
        assert_eq!(
            received.len(),
            events.len(),
            "expected to consume every published record"
        );

        let mut actual = BTreeMap::<String, Vec<u32>>::new();
        for record in &received {
            let key = String::from_utf8(record.key.clone()).expect("utf8 key");
            let payload = String::from_utf8(record.payload.clone()).expect("utf8 payload");
            let (payload_key, seq) = payload.split_once(':').expect("payload key:seq");
            assert_eq!(payload_key, key, "payload key mirrors Kafka record key");
            let seq = seq.parse::<u32>().expect("sequence number");
            actual.entry(key).or_default().push(seq);
        }
        assert_eq!(actual, expected, "per-key order must be preserved");

        let offsets: Vec<_> = received.iter().map(StreamOffset::from).collect();
        transport
            .commit(&offsets)
            .await
            .expect("commit consumed offsets");
        drop(transport);

        let verifier = registry
            .build("redpanda", &config(&brokers, &topic, &group, &client))
            .expect("redpanda verifier transport builds");
        let after_commit = verifier
            .poll(events.len())
            .await
            .expect("poll after commit");
        assert!(
            after_commit.is_empty(),
            "committed offsets should prevent replay for the same consumer group"
        );
    });
}

#[test]
fn redpanda_rewind_replays_from_retained_beginning_after_commit() {
    let brokers = brokers();

    block_on(async move {
        let suffix = unique_suffix();
        let topic = format!("zeroship-stream-rewind-{suffix}");
        let group = format!("zeroship-stream-rewind-group-{suffix}");
        let client = format!("zeroship-stream-rewind-test-{suffix}");

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);

        // The rewind case cares about offsets, so publish three records with
        // explicit sequence payloads rather than the keyed events above.
        let events = [("subject-a", 0), ("subject-a", 1), ("subject-a", 2)];
        publish(
            &registry,
            &brokers,
            &topic,
            &group,
            &client,
            &events,
        )
        .await;

        let transport = registry
            .build("redpanda", &config(&brokers, &topic, &group, &client))
            .expect("redpanda transport builds");
        let mut first = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while first.len() < 3 && Instant::now() < deadline {
            first.extend(transport.poll(3).await.expect("initial poll"));
        }
        assert_eq!(first.len(), 3, "expected to consume every published record");
        let offsets: Vec<_> = first.iter().map(StreamOffset::from).collect();
        transport.commit(&offsets).await.expect("commit offsets");
        drop(transport);

        let verifier = registry
            .build("redpanda", &config(&brokers, &topic, &group, &client))
            .expect("redpanda verifier transport builds");
        assert!(
            verifier
                .poll(3)
                .await
                .expect("poll after commit")
                .is_empty(),
            "committed group should not replay before rewind"
        );

        verifier.rewind().await.expect("rewind redpanda group");
        let mut replay = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while replay.len() < 3 && Instant::now() < deadline {
            replay.extend(verifier.poll(3).await.expect("poll after rewind"));
        }
        assert_eq!(
            replay.iter().map(|r| r.offset).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "rewind must seek back to the retained beginning"
        );
    });
}

fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis();
    format!("{}-{now}", std::process::id())
}
