use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::executor::block_on;
use serde_json::json;
use zeroship_stream::adapters;
use zeroship_stream::{PartitionEnd, StreamConfig, StreamOffset, StreamRegistry};

#[test]
fn memory_roundtrip_preserves_per_key_order_and_commits_offsets() {
    block_on(async move {
        let suffix = unique_suffix();
        let topic = format!("zeroship-memory-roundtrip-{suffix}");
        let group = format!("zeroship-memory-roundtrip-group-{suffix}");

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);
        let config = StreamConfig::from(json!({
            "topic": topic.clone(),
            "group.id": group,
            "partitions": 4
        }));

        let transport = registry
            .build("memory", &config)
            .expect("memory transport builds");

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
                .expect("publish memory test event");
        }

        let received = transport.poll(events.len()).await.expect("poll records");
        assert_eq!(
            received.len(),
            events.len(),
            "expected to consume every published memory record"
        );

        let mut actual = BTreeMap::<String, Vec<u32>>::new();
        for record in &received {
            let key = String::from_utf8(record.key.clone()).expect("utf8 key");
            let payload = String::from_utf8(record.payload.clone()).expect("utf8 payload");
            let (payload_key, seq) = payload.split_once(':').expect("payload key:seq");
            assert_eq!(payload_key, key, "payload key mirrors stream record key");
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
            .build("memory", &config)
            .expect("memory verifier transport builds");
        let after_commit = verifier.poll(events.len()).await.expect("poll after commit");
        assert!(
            after_commit.is_empty(),
            "committed offsets should prevent replay for the same memory consumer group"
        );
    });
}

#[test]
fn memory_rewind_replays_from_retained_beginning_after_commit() {
    block_on(async move {
        let suffix = unique_suffix();
        let topic = format!("zeroship-memory-rewind-{suffix}");
        let group = format!("zeroship-memory-rewind-group-{suffix}");

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);
        let config = StreamConfig::from(json!({
            "topic": topic.clone(),
            "group.id": group,
            "partitions": 1
        }));

        let transport = registry
            .build("memory", &config)
            .expect("memory transport builds");
        for seq in 0..3 {
            let payload = format!("payload-{seq}");
            transport
                .publish(&topic, b"subject-a", payload.as_bytes())
                .await
                .expect("publish memory rewind event");
        }

        let first = transport.poll(10).await.expect("initial poll");
        assert_eq!(first.len(), 3);
        let offsets: Vec<_> = first.iter().map(StreamOffset::from).collect();
        transport.commit(&offsets).await.expect("commit offsets");
        drop(transport);

        let verifier = registry
            .build("memory", &config)
            .expect("memory verifier transport builds");
        assert!(
            verifier.poll(10).await.expect("poll after commit").is_empty(),
            "committed group should not replay before rewind"
        );

        verifier.rewind().await.expect("rewind memory group");
        let replay = verifier.poll(10).await.expect("poll after rewind");
        assert_eq!(
            replay.iter().map(|r| r.offset).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "rewind must seek back to the retained beginning"
        );
    });
}

/// `end_offsets` names every partition of the topic with the offset of the
/// last record it holds at the moment of the call - the record a reader stops
/// at - and nothing for a partition that holds none. A later publish moves
/// only a later capture.
#[test]
fn memory_end_offsets_name_the_last_record_of_every_partition() {
    block_on(async move {
        let suffix = unique_suffix();
        let topic = format!("zeroship-memory-ends-{suffix}");
        let group = format!("zeroship-memory-ends-group-{suffix}");

        let mut registry = StreamRegistry::default();
        adapters::register_builtin(&mut registry);
        let config = StreamConfig::from(json!({
            "topic": topic.clone(),
            "group.id": group,
            "partitions": 4
        }));
        let transport = registry
            .build("memory", &config)
            .expect("memory transport builds");
        assert_eq!(
            transport.stall_timeout(),
            None,
            "the in-process log answers every poll at once and declares no stall bound"
        );
        assert!(
            transport
                .end_offsets()
                .await
                .expect("capture before any publish")
                .is_empty(),
            "a topic nothing has published to has no partitions"
        );

        for (key, seq) in [("subject-a", 0), ("subject-b", 0), ("subject-a", 1)] {
            transport
                .publish(&topic, key.as_bytes(), format!("{key}:{seq}").as_bytes())
                .await
                .expect("publish memory test event");
        }
        let captured = transport.end_offsets().await.expect("capture ends");

        // The truth the capture must agree with: what a reader actually gets.
        let records = transport.poll(16).await.expect("poll every record");
        assert_eq!(records.len(), 3, "every published record is readable");
        let mut last_read = BTreeMap::<i32, i64>::new();
        for record in &records {
            let last = last_read.entry(record.partition).or_insert(record.offset);
            *last = (*last).max(record.offset);
        }
        let expected: Vec<PartitionEnd> = (0..4)
            .map(|partition| PartitionEnd {
                partition,
                last_offset: last_read.get(&partition).copied(),
            })
            .collect();
        assert_eq!(
            captured, expected,
            "each partition's captured end is the offset of the last record a reader is handed"
        );
        assert!(
            captured.iter().any(|end| end.last_offset.is_none()),
            "two keys over four partitions leave an empty partition, which names no end"
        );

        transport
            .publish(&topic, b"subject-a", b"subject-a:2")
            .await
            .expect("publish after the capture");
        let later = transport.end_offsets().await.expect("capture again");
        let moved: Vec<_> = captured
            .iter()
            .zip(&later)
            .filter(|(before, after)| before != after)
            .map(|(before, after)| (before.partition, before.last_offset, after.last_offset))
            .collect();
        assert_eq!(moved.len(), 1, "one publish moves one partition's end: {moved:?}");
        let (_, before, after) = moved[0];
        assert_eq!(
            after,
            Some(before.map_or(0, |offset| offset + 1)),
            "the moved end names the newly appended record"
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
