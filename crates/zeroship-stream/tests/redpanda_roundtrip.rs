//! The Redpanda adapter against a real broker owned by this test binary.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpListener};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::executor::block_on;
use serde_json::json;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};
use zeroship_stream::adapters;
use zeroship_stream::{StreamConfig, StreamOffset, StreamRegistry};

struct Redpanda {
    _container: Container<GenericImage>,
    brokers: String,
}

impl Redpanda {
    fn start() -> Self {
        let port = available_port();
        let advertised = format!("external://127.0.0.1:{port}");
        let container = GenericImage::new("docker.redpanda.com/redpandadata/redpanda", "v26.2.2")
            .with_wait_for(WaitFor::message_on_stderr("Successfully started Redpanda!"))
            .with_mapped_port(port, 19092.tcp())
            .with_cmd([
                "redpanda".to_owned(),
                "start".to_owned(),
                "--overprovisioned".to_owned(),
                "--smp".to_owned(),
                "1".to_owned(),
                "--memory".to_owned(),
                "512M".to_owned(),
                "--reserve-memory".to_owned(),
                "0M".to_owned(),
                "--node-id".to_owned(),
                "0".to_owned(),
                "--check=false".to_owned(),
                "--kafka-addr".to_owned(),
                "external://0.0.0.0:19092".to_owned(),
                "--advertise-kafka-addr".to_owned(),
                advertised,
                "--set".to_owned(),
                "redpanda.auto_create_topics_enabled=true".to_owned(),
            ])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .expect("stream tests require Docker and Redpanda");
        Self {
            _container: container,
            brokers: format!("127.0.0.1:{port}"),
        }
    }
}

fn available_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("bind an available Redpanda port")
        .local_addr()
        .expect("Redpanda listener address")
        .port()
}

static REDPANDA: OnceLock<Redpanda> = OnceLock::new();

fn brokers() -> String {
    REDPANDA.get_or_init(Redpanda::start).brokers.to_owned()
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
