use std::collections::BTreeMap;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rdkafka::client::ClientContext;
use rdkafka::config::{ClientConfig, RDKafkaLogLevel};
use rdkafka::consumer::{BaseConsumer, CommitMode, Consumer};
use rdkafka::message::{DeliveryResult, Message};
use rdkafka::producer::{BaseProducer, BaseRecord, ProducerContext};
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use serde::Deserialize;

use crate::{StreamConfig, StreamError, StreamOffset, StreamRecord, StreamTransport};

type DeliveryAck = Result<(i32, i64), String>;
type DeliverySender = mpsc::SyncSender<DeliveryAck>;

#[derive(Clone, Debug)]
struct DeliveryContext;

impl ClientContext for DeliveryContext {}

impl ProducerContext for DeliveryContext {
    type DeliveryOpaque = Box<DeliverySender>;

    fn delivery(&self, delivery_result: &DeliveryResult<'_>, tx: Self::DeliveryOpaque) {
        let result = match delivery_result {
            Ok(message) => Ok((message.partition(), message.offset())),
            Err((error, _message)) => Err(error.to_string()),
        };
        let _ = tx.send(result);
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedpandaConfig {
    pub brokers: String,
    pub topic: String,
    #[serde(rename = "group.id", alias = "group_id")]
    pub group_id: String,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default = "default_message_timeout_ms")]
    pub message_timeout_ms: u64,
    #[serde(default = "default_publish_timeout_ms")]
    pub publish_timeout_ms: u64,
    #[serde(default = "default_poll_timeout_ms")]
    pub poll_timeout_ms: u64,
    #[serde(default = "default_auto_offset_reset")]
    pub auto_offset_reset: String,
}

fn default_message_timeout_ms() -> u64 {
    30_000
}

fn default_publish_timeout_ms() -> u64 {
    30_000
}

fn default_poll_timeout_ms() -> u64 {
    100
}

fn default_auto_offset_reset() -> String {
    "earliest".to_string()
}

impl RedpandaConfig {
    fn validate(&self) -> Result<(), StreamError> {
        require_non_empty("redpanda: brokers", &self.brokers)?;
        require_non_empty("redpanda: topic", &self.topic)?;
        require_non_empty("redpanda: group.id", &self.group_id)?;
        require_non_zero("redpanda: message_timeout_ms", self.message_timeout_ms)?;
        require_non_zero("redpanda: publish_timeout_ms", self.publish_timeout_ms)?;
        require_non_zero("redpanda: poll_timeout_ms", self.poll_timeout_ms)?;
        Ok(())
    }
}

fn require_non_empty(name: &str, value: &str) -> Result<(), StreamError> {
    if value.trim().is_empty() {
        Err(StreamError::Config(format!("{name} is required")))
    } else {
        Ok(())
    }
}

fn require_non_zero(name: &str, value: u64) -> Result<(), StreamError> {
    if value == 0 {
        Err(StreamError::Config(format!("{name} must be > 0")))
    } else {
        Ok(())
    }
}

pub struct RedpandaTransport {
    producer: BaseProducer<DeliveryContext>,
    consumer: Mutex<BaseConsumer>,
    topic: String,
    publish_timeout: Duration,
    poll_timeout: Duration,
}

pub fn factory(config: &StreamConfig) -> Result<std::sync::Arc<dyn StreamTransport>, StreamError> {
    let config: RedpandaConfig = config.parse()?;
    config.validate()?;
    Ok(std::sync::Arc::new(RedpandaTransport::new(config)?))
}

impl RedpandaTransport {
    pub fn new(config: RedpandaConfig) -> Result<Self, StreamError> {
        let mut producer_config = ClientConfig::new();
        producer_config
            .set("bootstrap.servers", config.brokers.trim())
            .set("acks", "all")
            .set("enable.idempotence", "true")
            .set("message.timeout.ms", config.message_timeout_ms.to_string());
        if let Some(client_id) = config.client_id.as_deref().filter(|s| !s.trim().is_empty()) {
            producer_config.set("client.id", format!("{client_id}-producer"));
        }
        let producer = producer_config
            .create_with_context(DeliveryContext)
            .map_err(StreamError::from)?;

        let mut consumer_config = ClientConfig::new();
        consumer_config
            .set("bootstrap.servers", config.brokers.trim())
            .set("group.id", config.group_id.trim())
            .set("enable.auto.commit", "false")
            .set("enable.auto.offset.store", "false")
            .set("enable.partition.eof", "false")
            .set("auto.offset.reset", config.auto_offset_reset.trim())
            .set_log_level(RDKafkaLogLevel::Warning);
        if let Some(client_id) = config.client_id.as_deref().filter(|s| !s.trim().is_empty()) {
            consumer_config.set("client.id", format!("{client_id}-consumer"));
        }
        let consumer: BaseConsumer = consumer_config.create().map_err(StreamError::from)?;
        consumer
            .subscribe(&[config.topic.as_str()])
            .map_err(StreamError::from)?;

        Ok(Self {
            producer,
            consumer: Mutex::new(consumer),
            topic: config.topic,
            publish_timeout: Duration::from_millis(config.publish_timeout_ms),
            poll_timeout: Duration::from_millis(config.poll_timeout_ms),
        })
    }
}

impl std::fmt::Debug for RedpandaTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedpandaTransport")
            .field("topic", &self.topic)
            .field("publish_timeout", &self.publish_timeout)
            .field("poll_timeout", &self.poll_timeout)
            .finish_non_exhaustive()
    }
}

#[async_trait(?Send)]
impl StreamTransport for RedpandaTransport {
    fn id(&self) -> &str {
        "redpanda"
    }

    async fn publish(
        &self,
        topic: &str,
        partition_key: &[u8],
        payload: &[u8],
    ) -> Result<(), StreamError> {
        require_non_empty("redpanda publish topic", topic)?;
        if partition_key.is_empty() {
            return Err(StreamError::Config(
                "redpanda publish partition_key is required for per-subject ordering".into(),
            ));
        }

        let (tx, rx) = mpsc::sync_channel(1);
        let record = BaseRecord::with_opaque_to(topic, Box::new(tx))
            .key(partition_key)
            .payload(payload);
        self.producer
            .send(record)
            .map_err(|(error, _record)| StreamError::from(error))?;

        let deadline = Instant::now() + self.publish_timeout;
        loop {
            self.producer.poll(Duration::from_millis(10));
            match rx.try_recv() {
                Ok(Ok(_partition_offset)) => return Ok(()),
                Ok(Err(error)) => return Err(StreamError::Kafka(error)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(StreamError::Kafka(
                        "producer delivery callback disconnected".into(),
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return Err(StreamError::PublishTimeout {
                            timeout_ms: self.publish_timeout.as_millis() as u64,
                        });
                    }
                }
            }
        }
    }

    async fn poll(&self, max: usize) -> Result<Vec<StreamRecord>, StreamError> {
        if max == 0 {
            return Ok(Vec::new());
        }

        let consumer = self
            .consumer
            .lock()
            .map_err(|_| StreamError::Unavailable("redpanda consumer mutex poisoned"))?;
        let mut records = Vec::with_capacity(max);
        while records.len() < max {
            let timeout = if records.is_empty() {
                self.poll_timeout
            } else {
                Duration::from_millis(0)
            };
            match consumer.poll(timeout) {
                Some(Ok(message)) => records.push(StreamRecord {
                    partition: message.partition(),
                    offset: message.offset(),
                    key: message.key().unwrap_or_default().to_vec(),
                    payload: message.payload().unwrap_or_default().to_vec(),
                }),
                Some(Err(error)) => return Err(StreamError::from(error)),
                None => break,
            }
        }

        Ok(records)
    }

    async fn commit(&self, offsets: &[StreamOffset]) -> Result<(), StreamError> {
        if offsets.is_empty() {
            return Ok(());
        }

        let mut latest_by_partition = BTreeMap::<i32, i64>::new();
        for offset in offsets {
            latest_by_partition
                .entry(offset.partition)
                .and_modify(|stored| *stored = (*stored).max(offset.offset))
                .or_insert(offset.offset);
        }

        let mut topic_partitions = TopicPartitionList::with_capacity(latest_by_partition.len());
        for (partition, offset) in latest_by_partition {
            let next_offset = offset.checked_add(1).ok_or_else(|| {
                StreamError::Config(format!(
                    "redpanda commit offset overflow for partition {partition}"
                ))
            })?;
            topic_partitions
                .add_partition_offset(&self.topic, partition, Offset::Offset(next_offset))
                .map_err(StreamError::from)?;
        }

        let consumer = self
            .consumer
            .lock()
            .map_err(|_| StreamError::Unavailable("redpanda consumer mutex poisoned"))?;
        consumer
            .commit(&topic_partitions, CommitMode::Sync)
            .map_err(StreamError::from)
    }
}
