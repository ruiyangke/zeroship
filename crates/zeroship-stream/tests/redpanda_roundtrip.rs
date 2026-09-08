//! The Redpanda adapter against a REAL broker.
//!
//! `REDPANDA_BROKERS` IS REQUIRED. These used to skip when it was unset, which
//! made the only coverage of the durable usage-event transport green on every
//! machine that had never started a broker. `brokers()` panics instead, and
//! carries the `docker run` that stands one up.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::executor::block_on;
use serde_json::json;
use zeroship_stream::adapters;
use zeroship_stream::{StreamConfig, StreamOffset, StreamRegistry};

/// The broker list these tests produce to and consume from.
///
/// NO SCRIPT IN THIS TREE PROVISIONS A STANDALONE BROKER for a `cargo test`
/// run. `tests/provision_test_backends.sh` stands up postgres and redis only,
/// and `tests/e2e_metering_billing.sh` starts a Redpanda of its own per run and
/// tears it down again, so it cannot be borrowed. The recipe below is that
/// script's, with a fixed port and container name.
///
/// # Panics
///
/// When `REDPANDA_BROKERS` is unset or empty, with that recipe.
fn brokers() -> String {
    let configured = zeroship_core::test_env_os!("REDPANDA_BROKERS")
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    assert!(
        !configured.trim().is_empty(),
        "A Redpanda broker is unreachable, and this test requires it.\n\
         \n\
         \x20 backend: Redpanda (Kafka-family), the durable usage-event stream\n\
         \x20 missing: REDPANDA_BROKERS is unset or empty\n\
         \n\
         NOTHING IN THIS REPOSITORY PROVISIONS A BROKER FOR A CARGO RUN.\n\
         `tests/provision_test_backends.sh` stands up postgres and redis only,\n\
         and `tests/e2e_metering_billing.sh` starts a broker of its own and\n\
         removes it again, so it cannot be borrowed. Start one yourself:\n\
         \n\
         \x20 docker run --name zs-stream-test-redpanda -d -p 19092:19092 \\\n\
         \x20   docker.redpanda.com/redpandadata/redpanda:latest \\\n\
         \x20   redpanda start --overprovisioned --smp 1 --memory 512M \\\n\
         \x20   --reserve-memory 0M --node-id 0 --check=false \\\n\
         \x20   --kafka-addr external://0.0.0.0:19092 \\\n\
         \x20   --advertise-kafka-addr external://127.0.0.1:19092 \\\n\
         \x20   --set redpanda.auto_create_topics_enabled=true\n\
         \x20 docker exec zs-stream-test-redpanda rpk cluster health --exit-when-healthy\n\
         \n\
         The advertised listener is load-bearing: without it the broker hands\n\
         back an address the client cannot dial, and the produce hangs rather\n\
         than failing. Auto-create is too - each test invents its own topic.\n\
         Then re-run with REDPANDA_BROKERS=127.0.0.1:19092.\n\
         \n\
         There is no environment variable that makes this a skip. A broker this\n\
         test cannot reach is a failed run, not a green one."
    );
    configured
}

#[test]
fn redpanda_roundtrip_preserves_per_key_order_and_commits_offsets() {
    let brokers = brokers();

    block_on(async move {
        let suffix = unique_suffix();
        let topic = format!("zeroship-stream-roundtrip-{suffix}");
        let group = format!("zeroship-stream-roundtrip-group-{suffix}");

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);
        let config = StreamConfig::from(json!({
            "brokers": brokers,
            "topic": topic,
            "group.id": group,
            "client_id": format!("zeroship-stream-test-{suffix}"),
            "message_timeout_ms": 10000,
            "publish_timeout_ms": 10000,
            "poll_timeout_ms": 250,
            "auto_offset_reset": "earliest"
        }));

        let transport = registry
            .build("redpanda", &config)
            .expect("redpanda transport builds");

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
            let payload = format!("{key}:{seq}");
            transport
                .publish(&topic, key.as_bytes(), payload.as_bytes())
                .await
                .expect("publish test event");
        }

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
            .build("redpanda", &config)
            .expect("redpanda verifier transport builds");
        let after_commit = verifier.poll(events.len()).await.expect("poll after commit");
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

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);
        let config = StreamConfig::from(json!({
            "brokers": brokers,
            "topic": topic.clone(),
            "group.id": group,
            "client_id": format!("zeroship-stream-rewind-test-{suffix}"),
            "message_timeout_ms": 10000,
            "publish_timeout_ms": 10000,
            "poll_timeout_ms": 250,
            "auto_offset_reset": "earliest"
        }));

        let transport = registry
            .build("redpanda", &config)
            .expect("redpanda transport builds");
        for seq in 0..3 {
            let payload = format!("payload-{seq}");
            transport
                .publish(&topic, b"subject-a", payload.as_bytes())
                .await
                .expect("publish redpanda rewind event");
        }

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
            .build("redpanda", &config)
            .expect("redpanda verifier transport builds");
        assert!(
            verifier.poll(3).await.expect("poll after commit").is_empty(),
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
