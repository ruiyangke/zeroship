//! In-process memory stream transport.
//!
//! This S8 adapter proves the stream L1 seam: adding a new transport required
//! this adapter file plus the `register_builtin` entry in `adapters/mod.rs` and
//! a focused roundtrip test, with no `StreamTransport`, registry, forwarder, or
//! pipeline edits.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;

use crate::{StreamConfig, StreamError, StreamOffset, StreamRecord, StreamTransport};

const DEFAULT_PARTITIONS: usize = 8;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    pub topic: String,
    #[serde(rename = "group.id", alias = "group_id")]
    pub group_id: String,
    #[serde(default = "default_partitions")]
    pub partitions: usize,
}

fn default_partitions() -> usize {
    DEFAULT_PARTITIONS
}

impl MemoryConfig {
    fn validate(&self) -> Result<(), StreamError> {
        require_non_empty("memory: topic", &self.topic)?;
        require_non_empty("memory: group.id", &self.group_id)?;
        if self.partitions == 0 {
            return Err(StreamError::Config(
                "memory: partitions must be > 0".to_string(),
            ));
        }
        if self.partitions > i32::MAX as usize {
            return Err(StreamError::Config(
                "memory: partitions must fit in i32".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct MemoryTransport {
    topic: String,
    group_id: String,
    consumer_id: u64,
    partitions: usize,
    broker: &'static Mutex<MemoryBroker>,
}

pub fn factory(config: &StreamConfig) -> Result<Arc<dyn StreamTransport>, StreamError> {
    let config: MemoryConfig = config.parse()?;
    config.validate()?;
    Ok(Arc::new(MemoryTransport {
        topic: config.topic,
        group_id: config.group_id,
        consumer_id: next_consumer_id(),
        partitions: config.partitions,
        broker: broker(),
    }))
}

#[async_trait(?Send)]
impl StreamTransport for MemoryTransport {
    fn id(&self) -> &str {
        "memory"
    }

    async fn publish(
        &self,
        topic: &str,
        partition_key: &[u8],
        payload: &[u8],
    ) -> Result<(), StreamError> {
        require_non_empty("memory publish topic", topic)?;
        if partition_key.is_empty() {
            return Err(StreamError::Config(
                "memory publish partition_key is required for per-subject ordering".into(),
            ));
        }
        let mut broker = self
            .broker
            .lock()
            .map_err(|_| StreamError::Unavailable("memory broker mutex poisoned"))?;
        broker.publish(topic, partition_key, payload, self.partitions)
    }

    async fn poll(&self, max: usize) -> Result<Vec<StreamRecord>, StreamError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut broker = self
            .broker
            .lock()
            .map_err(|_| StreamError::Unavailable("memory broker mutex poisoned"))?;
        Ok(broker.poll(&self.topic, &self.group_id, self.consumer_id, max))
    }

    async fn commit(&self, offsets: &[StreamOffset]) -> Result<(), StreamError> {
        let mut broker = self
            .broker
            .lock()
            .map_err(|_| StreamError::Unavailable("memory broker mutex poisoned"))?;
        broker.commit(&self.topic, &self.group_id, self.consumer_id, offsets)
    }

    async fn rewind(&self) -> Result<(), StreamError> {
        let mut broker = self
            .broker
            .lock()
            .map_err(|_| StreamError::Unavailable("memory broker mutex poisoned"))?;
        broker.rewind(&self.topic, &self.group_id);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct MemoryBroker {
    topics: HashMap<String, TopicLog>,
    committed_next: HashMap<(String, String, i32), i64>,
    cursor_next: HashMap<(String, String, u64, i32), i64>,
}

impl MemoryBroker {
    fn publish(
        &mut self,
        topic: &str,
        partition_key: &[u8],
        payload: &[u8],
        partitions: usize,
    ) -> Result<(), StreamError> {
        let partition = partition_for(partition_key, partitions)?;
        let topic_log = self
            .topics
            .entry(topic.to_string())
            .or_insert_with(|| TopicLog::new(partitions));
        topic_log.ensure_partitions(partitions);
        let log = topic_log
            .partitions
            .get_mut(partition as usize)
            .ok_or_else(|| StreamError::Config("memory: invalid partition".to_string()))?;
        let offset = i64::try_from(log.len()).map_err(|_| {
            StreamError::Config(format!("memory: offset overflow in topic {topic}"))
        })?;
        log.push(StoredRecord {
            key: partition_key.to_vec(),
            payload: payload.to_vec(),
            offset,
        });
        Ok(())
    }

    fn poll(
        &mut self,
        topic: &str,
        group_id: &str,
        consumer_id: u64,
        max: usize,
    ) -> Vec<StreamRecord> {
        let Some(topic_log) = self.topics.get(topic) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(max);
        for (partition, log) in topic_log.partitions.iter().enumerate() {
            let partition = i32::try_from(partition).expect("memory partition fits i32");
            let cursor_key = (
                topic.to_string(),
                group_id.to_string(),
                consumer_id,
                partition,
            );
            let next = self
                .cursor_next
                .get(&cursor_key)
                .or_else(|| {
                    self.committed_next
                        .get(&(topic.to_string(), group_id.to_string(), partition))
                })
                .copied()
                .unwrap_or(0);
            for record in log.iter().skip(next.max(0) as usize) {
                self.cursor_next
                    .insert(cursor_key.clone(), record.offset.saturating_add(1));
                out.push(StreamRecord {
                    partition,
                    offset: record.offset,
                    key: record.key.clone(),
                    payload: record.payload.clone(),
                });
                if out.len() == max {
                    return out;
                }
            }
        }
        out
    }

    fn commit(
        &mut self,
        topic: &str,
        group_id: &str,
        consumer_id: u64,
        offsets: &[StreamOffset],
    ) -> Result<(), StreamError> {
        for offset in offsets {
            let next = offset.offset.checked_add(1).ok_or_else(|| {
                StreamError::Config(format!(
                    "memory commit offset overflow for partition {}",
                    offset.partition
                ))
            })?;
            self.committed_next
                .entry((topic.to_string(), group_id.to_string(), offset.partition))
                .and_modify(|stored| *stored = (*stored).max(next))
                .or_insert(next);
            self.cursor_next
                .entry((
                    topic.to_string(),
                    group_id.to_string(),
                    consumer_id,
                    offset.partition,
                ))
                .and_modify(|stored| *stored = (*stored).max(next))
                .or_insert(next);
        }
        Ok(())
    }

    fn rewind(&mut self, topic: &str, group_id: &str) {
        self.committed_next
            .retain(|(stored_topic, stored_group, _partition), _| {
                stored_topic != topic || stored_group != group_id
            });
        self.cursor_next
            .retain(|(stored_topic, stored_group, _consumer, _partition), _| {
                stored_topic != topic || stored_group != group_id
            });
    }
}

#[derive(Debug, Default)]
struct TopicLog {
    partitions: Vec<Vec<StoredRecord>>,
}

impl TopicLog {
    fn new(partitions: usize) -> Self {
        Self {
            partitions: vec![Vec::new(); partitions],
        }
    }

    fn ensure_partitions(&mut self, partitions: usize) {
        if self.partitions.len() < partitions {
            self.partitions.resize_with(partitions, Vec::new);
        }
    }
}

#[derive(Debug, Clone)]
struct StoredRecord {
    key: Vec<u8>,
    payload: Vec<u8>,
    offset: i64,
}

fn broker() -> &'static Mutex<MemoryBroker> {
    static BROKER: OnceLock<Mutex<MemoryBroker>> = OnceLock::new();
    BROKER.get_or_init(|| Mutex::new(MemoryBroker::default()))
}

fn next_consumer_id() -> u64 {
    static NEXT_CONSUMER_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_CONSUMER_ID.fetch_add(1, Ordering::Relaxed)
}

fn partition_for(partition_key: &[u8], partitions: usize) -> Result<i32, StreamError> {
    let mut hasher = DefaultHasher::new();
    partition_key.hash(&mut hasher);
    let partition = (hasher.finish() as usize) % partitions;
    i32::try_from(partition)
        .map_err(|_| StreamError::Config("memory: partition exceeds i32".to_string()))
}

fn require_non_empty(name: &str, value: &str) -> Result<(), StreamError> {
    if value.trim().is_empty() {
        Err(StreamError::Config(format!("{name} is required")))
    } else {
        Ok(())
    }
}
