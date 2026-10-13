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
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
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

/// Create `topic` with exactly `partitions` partitions through the admin API,
/// rather than relying on the broker's auto-create: the shared broker's
/// `redpanda.auto_create_topics_enabled` always creates a topic with exactly
/// one partition, which cannot exercise a per-partition defect.
async fn create_topic(brokers: &str, topic: &str, partitions: i32) {
    let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .create()
        .expect("redpanda admin client builds");
    let new_topic = NewTopic::new(topic, partitions, TopicReplication::Fixed(1));
    let results = admin
        .create_topics([&new_topic], &AdminOptions::new())
        .await
        .expect("create_topics request reaches the broker");
    for result in results {
        result.unwrap_or_else(|(topic, error)| {
            panic!("creating topic '{topic}' with {partitions} partitions failed: {error:?}")
        });
    }
}

/// Publish one record to a specific partition of `topic`, bypassing the
/// transport's key-hash partitioner: the end-offsets case needs to know
/// exactly which partitions hold data and which stay empty, not whatever a
/// hash happens to pick.
fn publish_to_partition(brokers: &str, topic: &str, partition: i32, key: &str, payload: &str) {
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .create()
        .expect("redpanda producer builds");
    producer
        .send(
            BaseRecord::to(topic)
                .partition(partition)
                .key(key.as_bytes())
                .payload(payload.as_bytes()),
        )
        .expect("queue the record for the named partition");
    producer
        .flush(Duration::from_secs(20))
        .expect("the producer drains its queue");
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

/// Against the real broker, `end_offsets` names every partition with the
/// offset of the last record a reader is handed, which is the high watermark
/// less one, and a record published after the capture moves only a later
/// capture. A topic nothing has published to has no partitions at all, and a
/// partition of an existing topic nothing has published to names no last
/// record.
///
/// The topic is created explicitly with several partitions through the admin
/// API and one of them is left empty. A single-partition topic (what the
/// broker's auto-create always gives) cannot tell the conditional
/// `then_some` form apart from an unconditional `Some(high - 1)`: whichever
/// partition holds the published records always has a high watermark past
/// its low one, so the mutated form answers identically there. Only a
/// partition that stays empty, where the low and high watermarks are equal,
/// diverges: the real code names no last record there, and the mutated one
/// claims an offset one below zero.
#[test]
fn redpanda_end_offsets_name_the_last_record_of_every_partition() {
    let brokers = brokers();

    block_on(async move {
        const PARTITIONS: i32 = 3;
        const EMPTY_PARTITION: i32 = PARTITIONS - 1;

        let suffix = unique_suffix();
        let topic = format!("zeroship-stream-ends-{suffix}");
        let group = format!("zeroship-stream-ends-group-{suffix}");
        let client = format!("zeroship-stream-ends-test-{suffix}");

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);

        let unpublished = registry
            .build(
                "redpanda",
                &config(&brokers, &format!("{topic}-unpublished"), &group, &client),
            )
            .expect("redpanda transport builds");
        let nothing = unpublished
            .end_offsets()
            .await
            .expect("capture the ends of a topic nothing has published to");
        assert!(
            nothing.is_empty(),
            "a topic nothing has published to has no partitions to end: {nothing:?}"
        );
        drop(unpublished);

        create_topic(&brokers, &topic, PARTITIONS).await;
        publish_to_partition(&brokers, &topic, 0, "subject-a", "subject-a:0");
        publish_to_partition(&brokers, &topic, 0, "subject-a", "subject-a:1");
        publish_to_partition(&brokers, &topic, 1, "subject-b", "subject-b:0");
        // EMPTY_PARTITION is never published to: the case this test exists for.

        let transport = registry
            .build("redpanda", &config(&brokers, &topic, &group, &client))
            .expect("redpanda transport builds");
        let captured = transport.end_offsets().await.expect("capture ends");
        assert_eq!(
            captured.len(),
            usize::try_from(PARTITIONS).expect("partition count fits usize"),
            "every partition the topic was created with is named: {captured:?}"
        );
        let empty = captured
            .iter()
            .find(|end| end.partition == EMPTY_PARTITION)
            .unwrap_or_else(|| panic!("the empty partition is named: {captured:?}"));
        assert_eq!(
            empty.last_offset, None,
            "a partition nothing was published to names no last record: {captured:?}"
        );
        assert_eq!(
            captured
                .iter()
                .filter(|end| end.last_offset.is_some())
                .count(),
            2,
            "exactly the two written-to partitions name a last record: {captured:?}"
        );

        // The truth the capture must agree with: what a reader actually gets.
        let published = 3;
        let mut records = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while records.len() < published && Instant::now() < deadline {
            records.extend(transport.poll(published).await.expect("poll records"));
        }
        assert_eq!(records.len(), published, "every published record is readable");
        let mut last_read = BTreeMap::<i32, i64>::new();
        for record in &records {
            let last = last_read.entry(record.partition).or_insert(record.offset);
            *last = (*last).max(record.offset);
        }
        for end in &captured {
            assert_eq!(
                end.last_offset,
                last_read.get(&end.partition).copied(),
                "partition {}'s captured end is the offset of the last record a reader is \
                 handed",
                end.partition
            );
        }

        // A later publish to the partition that was empty at capture moves
        // only that partition's end, from naming no record to naming the new
        // one.
        publish_to_partition(&brokers, &topic, EMPTY_PARTITION, "subject-c", "subject-c:0");
        let later = transport.end_offsets().await.expect("capture again");
        let moved: Vec<_> = captured
            .iter()
            .zip(&later)
            .filter(|(before, after)| before != after)
            .map(|(before, after)| (before.partition, before.last_offset, after.last_offset))
            .collect();
        assert_eq!(moved.len(), 1, "one publish moves one partition's end: {moved:?}");
        let (moved_partition, before, after) = moved[0];
        assert_eq!(moved_partition, EMPTY_PARTITION);
        assert_eq!(before, None, "the moved partition was the empty one");
        assert_eq!(after, Some(0), "the moved end names the newly appended record");
    });
}

fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis();
    format!("{}-{now}", std::process::id())
}
