use std::collections::BTreeMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rdkafka::client::ClientContext;
use rdkafka::config::{ClientConfig, RDKafkaLogLevel};
use rdkafka::consumer::{BaseConsumer, CommitMode, Consumer};
use rdkafka::message::{DeliveryResult, Message};
use rdkafka::producer::{BaseProducer, BaseRecord, ProducerContext};
use rdkafka::topic_partition_list::{Offset, TopicPartitionList};
use rdkafka::types::RDKafkaRespErr;
use serde::Deserialize;

use crate::{
    PartitionEnd, StreamConfig, StreamError, StreamOffset, StreamRecord, StreamTransport,
};

/// Drain up to `max` records, then decide what to do with an error that ends
/// the drain early.
///
/// `poll_one` yields the next record: `None` when the broker has nothing right
/// now, `Some(Err(..))` on a transport error. It is called with the full
/// `poll_timeout` only for the FIRST record - once a batch has started, a
/// zero timeout keeps the cycle from blocking on a partial batch.
///
/// THE ERROR POLICY IS THE POINT OF THIS FUNCTION. An error is only surfaced
/// when NO records were collected. If any record was already handed over, the
/// batch is returned and the error is dropped.
///
/// Dropping an error reads like the wrong instinct, so the reasoning, which
/// runs the other way:
///
///   * rdkafka has already advanced its consumer position past every record it
///     handed us. Returning `Err` discards those records but does NOT put them
///     back, so the next poll resumes AFTER them.
///   * the forwarder's commit is cumulative - `commit()` takes the max offset
///     per partition and adds one. So the next cycle that succeeds commits a
///     HIGHER offset and buries the discarded records for good. This is not a
///     delay that a restart repairs; it is silent, permanent under-billing.
///   * nothing is swallowed. The condition that produced the error is still
///     there on the next call, and that call starts with an empty batch, so it
///     propagates - one cycle later, with no records at risk.
///
/// The caller gets at-least-once either way: it commits only what it processed.
fn drain_batch<F>(
    max: usize,
    poll_timeout: Duration,
    mut poll_one: F,
) -> Result<Vec<StreamRecord>, StreamError>
where
    F: FnMut(Duration) -> Option<Result<StreamRecord, StreamError>>,
{
    let mut records = Vec::with_capacity(max);
    while records.len() < max {
        let timeout = if records.is_empty() {
            poll_timeout
        } else {
            Duration::from_millis(0)
        };
        match poll_one(timeout) {
            Some(Ok(record)) => records.push(record),
            Some(Err(error)) => {
                if records.is_empty() {
                    return Err(error);
                }
                tracing::warn!(
                    error = %error,
                    records = records.len(),
                    "redpanda poll failed mid-batch; returning the records already \
                     consumed and deferring the error to the next poll"
                );
                break;
            }
            None => break,
        }
    }

    Ok(records)
}

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

/// Connection settings for the redpanda transport.
///
/// # This transport is PLAINTEXT and UNAUTHENTICATED, by construction
///
/// There is no TLS and no SASL here, and two separate things stop it - so an
/// operator who needs an encrypted or authenticated broker link cannot get one
/// by configuration alone, and will not find out by trying.
///
/// They are not two barriers on one path; they cover DIFFERENT vectors, which
/// is the part worth keeping straight. (1) stops a config file introducing a
/// security key. (2) stops CODE doing it - `RedpandaConfig` appears nowhere
/// outside this file and both `ClientConfig::new()` call sites are below, so a
/// future `producer_config.set("security.protocol", ..)` would sail past serde
/// and hit (2) instead. Neither one covers the other's vector.
///
/// 1. **The fields below are the whole surface**, and the struct is
///    `deny_unknown_fields`. Passing `security.protocol`, `sasl.mechanism` or
///    any other librdkafka security key is a deserialization ERROR, not an
///    ignored key. This is the CONFIG vector only.
/// 2. **librdkafka is not built with them.** The workspace pins
///    `rdkafka = { default-features = false, features = ["cmake-build"] }`.
///    In rdkafka 0.36.2 `ssl`, `sasl` and `gssapi` are separate opt-in
///    features (verified against the registry source), so the linked C library
///    has no TLS or SASL support at all. Even if a key reached it, the protocol
///    would be unsupported at runtime.
///
/// The usage stream therefore carries per-app billing events in the clear, and
/// any party who can reach the broker port can both read them and inject them.
/// That is acceptable only on a trusted network. The compose deployment puts
/// the broker on an internal address (`redpanda:9092`) and does not publish it,
/// which is what makes the posture survivable today - not anything in this
/// crate.
///
/// # If you are here to add TLS, do NOT start by restoring default features
///
/// That is the obvious move and it is wrong twice: `default = ["libz",
/// "tokio"]` in rdkafka 0.36.2, so re-enabling defaults pulls TOKIO back into
/// a workspace whose whole runtime posture is compio, AND it
/// still does not enable `ssl`, because ssl was never a default feature. The
/// absence of TLS here is not a side effect of dropping tokio; it was simply
/// never turned on.
///
/// The actual change is: add `ssl` (or `ssl-vendored`) and `sasl` to the
/// rdkafka features, add the corresponding fields here, and thread them into
/// the `ClientConfig` builders below for BOTH the producer and the consumer.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedpandaConfig {
    pub brokers: String,
    pub topic: String,
    #[serde(rename = "group.id", alias = "group_id")]
    pub group_id: String,
    #[serde(default)]
    pub client_id: Option<String>,
    /// librdkafka's PRODUCER setting (`message.timeout.ms`): the longest a
    /// produced record may go unacknowledged before the producer gives up on
    /// it. Producer-only. The transport's stall bound is its own setting,
    /// [`Self::stall_timeout_ms`]; tuning either does not move the other.
    #[serde(default = "default_message_timeout_ms")]
    pub message_timeout_ms: u64,
    #[serde(default = "default_publish_timeout_ms")]
    pub publish_timeout_ms: u64,
    #[serde(default = "default_poll_timeout_ms")]
    pub poll_timeout_ms: u64,
    /// The transport's stall bound, reported as
    /// [`StreamTransport::stall_timeout`]. It is the longest either consumer
    /// of the stream may go without progress before it treats the broker as
    /// stalled: the recompute scan, whose `rewind` group join and seek,
    /// `end_offsets` metadata and watermark queries, and gaps between the polls
    /// that advance a partition are all held to it; and the usage outbox in
    /// `zeroship-metering`, whose batch publish treats a broker that
    /// acknowledges nothing for this long as stalled. This is not
    /// a librdkafka setting, so it takes this struct's plain `snake_case`
    /// spelling, and it is independent of [`Self::message_timeout_ms`], which
    /// stays the producer's `message.timeout.ms`. Defaults to 30 seconds.
    #[serde(default = "default_stall_timeout_ms")]
    pub stall_timeout_ms: u64,
    #[serde(default = "default_auto_offset_reset")]
    pub auto_offset_reset: String,
    /// librdkafka's `session.timeout.ms`: how long the broker waits for a
    /// heartbeat from this member before removing it from the group. The
    /// broker refuses a value below its `group.min.session.timeout.ms` at join
    /// time. Exposed so a test can lower it enough to lower
    /// `max.poll.interval.ms` with it, because librdkafka requires the two to
    /// be ordered (see [`Self::validate`]).
    #[serde(
        default = "default_session_timeout_ms",
        rename = "session.timeout.ms",
        alias = "session_timeout_ms"
    )]
    pub session_timeout_ms: u64,
    /// librdkafka's `max.poll.interval.ms`: the longest the application may go
    /// between `poll` calls before librdkafka treats the member as failed,
    /// leaves the group, and queues a [`crate::StreamError::PollExceeded`] for
    /// the next poll. The recompute cron keeps one consumer across its cycles
    /// and may idle longer than this between them.
    #[serde(
        default = "default_max_poll_interval_ms",
        rename = "max.poll.interval.ms",
        alias = "max_poll_interval_ms"
    )]
    pub max_poll_interval_ms: u64,
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

const fn default_stall_timeout_ms() -> u64 {
    30_000
}

fn default_auto_offset_reset() -> String {
    "earliest".to_string()
}

const fn default_session_timeout_ms() -> u64 {
    45_000
}

const fn default_max_poll_interval_ms() -> u64 {
    300_000
}

impl RedpandaConfig {
    fn validate(&self) -> Result<(), StreamError> {
        require_non_empty("redpanda: brokers", &self.brokers)?;
        require_non_empty("redpanda: topic", &self.topic)?;
        require_non_empty("redpanda: group.id", &self.group_id)?;
        require_non_zero("redpanda: message_timeout_ms", self.message_timeout_ms)?;
        require_non_zero("redpanda: publish_timeout_ms", self.publish_timeout_ms)?;
        require_non_zero("redpanda: poll_timeout_ms", self.poll_timeout_ms)?;
        require_non_zero("redpanda: stall_timeout_ms", self.stall_timeout_ms)?;
        require_non_zero("redpanda: session.timeout.ms", self.session_timeout_ms)?;
        require_non_zero("redpanda: max.poll.interval.ms", self.max_poll_interval_ms)?;
        // librdkafka refuses to build a classic-protocol consumer where
        // `max.poll.interval.ms` is below `session.timeout.ms`; surfaced here so
        // the caller gets the constraint by name instead of a client-creation
        // error, and so an operator lowering one knows to lower the other.
        if self.max_poll_interval_ms < self.session_timeout_ms {
            return Err(StreamError::Config(format!(
                "redpanda: max.poll.interval.ms ({}) must be >= session.timeout.ms ({})",
                self.max_poll_interval_ms, self.session_timeout_ms
            )));
        }
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

/// This adapter's [`StreamTransport::id`] and [`crate::StreamRegistry::register`] key.
///
/// The one spelling, named once so a caller that must recognize this
/// specific adapter (rather than go through the registry) compares against
/// it instead of a repeated string literal.
pub const ID: &str = "redpanda";

pub struct RedpandaTransport {
    producer: BaseProducer<DeliveryContext>,
    consumer: Arc<Mutex<BaseConsumer>>,
    topic: String,
    publish_timeout: Duration,
    poll_timeout: Duration,
    /// `stall_timeout_ms`: the transport's STALL bound, reported as
    /// [`StreamTransport::stall_timeout`] and shared by both consumers of the
    /// stream. The recompute scan holds `rewind`'s group join and seek,
    /// `end_offsets`' metadata and watermark queries, and the gap between the
    /// polls that advance a partition to it; the usage outbox in
    /// `zeroship-metering` holds its batch publish to it, treating a broker
    /// that acknowledges nothing for this long as stalled. Independent of
    /// `message_timeout_ms`, which stays the producer's librdkafka
    /// `message.timeout.ms`.
    ///
    /// Every one of the calls this bounds is SYNCHRONOUS and blocks the thread
    /// that runs it until librdkafka answers or the bound expires. This
    /// transport runs each of them on a compio blocking-pool thread (see
    /// [`run_blocking`]), so the caller's executor thread keeps serving other
    /// work - a scan on Control's compio runtime does - while librdkafka waits.
    stall_timeout: Duration,
}

/// Run `blocking` on a compio blocking-pool thread and await its result.
///
/// The consumer calls this transport makes are synchronous librdkafka calls:
/// `poll`, `commit`, `rewind`'s group join and seek, and `end_offsets`'s
/// metadata and watermark queries all block the calling thread until the
/// broker answers or the call's own bound expires. Running one on the thread
/// that polled this future would block the executor thread the caller's scan
/// runs on - Control's compio runtime - for up to the stall bound. The
/// blocking pool keeps that thread serving other work while the call waits.
/// A panic in `blocking` is resumed on the caller, so the transport keeps the
/// panic it always propagated.
async fn run_blocking<T: Send + 'static>(blocking: impl FnOnce() -> T + Send + 'static) -> T {
    compio::runtime::spawn_blocking(blocking)
        .await
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

impl RedpandaTransport {
    /// Lock this transport's consumer and run `call` on a compio
    /// blocking-pool thread, so a synchronous librdkafka wait inside `call`
    /// does not hold the caller's executor thread. Every consumer-side method
    /// takes this path.
    ///
    /// The mutex is acquired and released on the blocking thread, so its
    /// `!Send` guard never crosses the thread hop; a poisoned lock maps to
    /// [`StreamError::Unavailable`] the same way it did when the lock was
    /// taken on the caller. `call` therefore needs a compio runtime under the
    /// caller: without one `spawn_blocking` has nothing to offload onto.
    ///
    /// An ABANDONED call - the caller dropped the returned future, as the
    /// recompute's `unless_stalled` does at the stall bound - still runs to
    /// completion under the consumer lock, and any records it consumes are
    /// discarded. So only a caller that rewinds the consumer afterwards may
    /// abandon one: the discarded records are re-read from the retained
    /// beginning, not lost. The recompute rewinds every cycle for exactly this
    /// reason; a caller that intends to continue from its current position
    /// must wait for the call instead.
    async fn with_consumer<T, F>(&self, call: F) -> Result<T, StreamError>
    where
        T: Send + 'static,
        F: FnOnce(&BaseConsumer) -> Result<T, StreamError> + Send + 'static,
    {
        let consumer = Arc::clone(&self.consumer);
        run_blocking(move || {
            let consumer = consumer
                .lock()
                .map_err(|_| StreamError::Unavailable("redpanda consumer mutex poisoned"))?;
            call(&consumer)
        })
        .await
    }
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
            .set("session.timeout.ms", config.session_timeout_ms.to_string())
            .set(
                "max.poll.interval.ms",
                config.max_poll_interval_ms.to_string(),
            )
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
            consumer: Arc::new(Mutex::new(consumer)),
            topic: config.topic,
            publish_timeout: Duration::from_millis(config.publish_timeout_ms),
            poll_timeout: Duration::from_millis(config.poll_timeout_ms),
            stall_timeout: Duration::from_millis(config.stall_timeout_ms),
        })
    }
}

impl std::fmt::Debug for RedpandaTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedpandaTransport")
            .field("topic", &self.topic)
            .field("publish_timeout", &self.publish_timeout)
            .field("poll_timeout", &self.poll_timeout)
            .field("stall_timeout", &self.stall_timeout)
            .finish_non_exhaustive()
    }
}

#[async_trait(?Send)]
impl StreamTransport for RedpandaTransport {
    fn id(&self) -> &str {
        ID
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

        let poll_timeout = self.poll_timeout;
        self.with_consumer(move |consumer| {
            // The whole body is `drain_batch` so the batching AND the
            // error-vs-records policy are reachable from a test. Everything left
            // inline here is the rdkafka message->StreamRecord field copy, which
            // has no branches.
            drain_batch(max, poll_timeout, |timeout| {
                consumer.poll(timeout).map(|result| {
                    result
                        .map(|message| StreamRecord {
                            partition: message.partition(),
                            offset: message.offset(),
                            key: message.key().unwrap_or_default().to_vec(),
                            payload: message.payload().unwrap_or_default().to_vec(),
                        })
                        .map_err(StreamError::from)
                })
            })
        })
        .await
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

        self.with_consumer(move |consumer| {
            consumer
                .commit(&topic_partitions, CommitMode::Sync)
                .map_err(StreamError::from)
        })
        .await
    }

    async fn rewind(&self) -> Result<(), StreamError> {
        let poll_timeout = self.poll_timeout;
        let stall_timeout = self.stall_timeout;
        self.with_consumer(move |consumer| {
            // A freshly-subscribed group consumer has no assignment until it has
            // polled AND the group rebalance has settled - that can take longer
            // than a single poll cycle against a real broker. Poll until
            // partitions are assigned, bounded by the transport's stall timeout
            // (`stall_timeout_ms`), so a group join that never lands ends the
            // wait rather than blocking it.
            let deadline = Instant::now() + stall_timeout;
            let mut assignment = consumer.assignment().map_err(StreamError::from)?;
            while assignment.count() == 0 && Instant::now() < deadline {
                let _ = consumer.poll(poll_timeout);
                assignment = consumer.assignment().map_err(StreamError::from)?;
            }
            if assignment.count() == 0 {
                // Nothing assigned to this member yet (another member holds the
                // partitions, or the rebalance has not landed). The next recompute
                // cycle rewinds once this consumer is assigned - not an error.
                return Ok(());
            }

            // `seek()` transiently returns `Local: Erroneous state` (__STATE) when a
            // just-assigned partition has no established fetch position yet - the
            // exact cold-start the enforcement recompute hits on its first cycle.
            // Poll to advance the partition into a seekable state and retry, bounded.
            // seek(Beginning) resets the fetch position, so records consumed by these
            // priming polls are re-read by the caller's subsequent poll - no loss.
            loop {
                let mut last_err = None;
                for elem in assignment.elements() {
                    if let Err(err) = consumer.seek(
                        elem.topic(),
                        elem.partition(),
                        Offset::Beginning,
                        poll_timeout,
                    ) {
                        last_err = Some(err);
                    }
                }
                match last_err {
                    None => return Ok(()),
                    Some(err) if Instant::now() >= deadline => return Err(StreamError::from(err)),
                    Some(_) => {
                        let _ = consumer.poll(poll_timeout);
                    }
                }
            }
        })
        .await
    }

    /// The topic's partitions come from the broker's metadata, not from this
    /// consumer's assignment, so a partition another group member holds, or
    /// one whose assignment has not landed yet, is still named. Each
    /// partition's end is its high watermark, the offset the next appended
    /// record will take; the last record sits one below it. That holds because
    /// this transport's producer is idempotent but not transactional, so no
    /// transaction marker occupies an offset a consumer is never handed.
    ///
    /// The metadata and watermark queries share one deadline, the stall
    /// timeout, so the capture as a whole is bounded however many partitions
    /// the topic has.
    async fn end_offsets(&self) -> Result<Vec<PartitionEnd>, StreamError> {
        let topic = self.topic.clone();
        let stall_timeout = self.stall_timeout;
        let mut ends = self
            .with_consumer(move |consumer| {
                let deadline = Instant::now() + stall_timeout;
                let remaining = || deadline.saturating_duration_since(Instant::now());

                let metadata = consumer
                    .fetch_metadata(Some(&topic), remaining())
                    .map_err(StreamError::from)?;
                let Some(metadata_topic) = metadata.topics().iter().find(|t| t.name() == topic)
                else {
                    return Ok(Vec::new());
                };
                match metadata_topic.error() {
                    None => {}
                    Some(RDKafkaRespErr::RD_KAFKA_RESP_ERR_UNKNOWN_TOPIC_OR_PART) => {
                        return Ok(Vec::new())
                    }
                    Some(error) => {
                        return Err(StreamError::Kafka(format!(
                            "metadata for topic '{topic}': {error:?}"
                        )));
                    }
                }

                let mut ends = Vec::with_capacity(metadata_topic.partitions().len());
                for partition in metadata_topic.partitions() {
                    let (low, high) = consumer
                        .fetch_watermarks(&topic, partition.id(), remaining())
                        .map_err(StreamError::from)?;
                    ends.push(PartitionEnd {
                        partition: partition.id(),
                        last_offset: (high > low).then_some(high - 1),
                    });
                }
                Ok(ends)
            })
            .await?;
        // Sorting needs no broker, so it runs after the consumer lock is
        // released.
        ends.sort_unstable_by_key(|end| end.partition);
        Ok(ends)
    }

    fn stall_timeout(&self) -> Option<Duration> {
        Some(self.stall_timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(offset: i64) -> StreamRecord {
        StreamRecord {
            partition: 0,
            offset,
            key: Vec::new(),
            payload: Vec::new(),
        }
    }

    /// Drives `drain_batch` off a scripted sequence, which is the same
    /// function `poll` runs - the rdkafka closure it wraps has no branches, so
    /// nothing about the batching or the error policy is left untested.
    fn drain(script: Vec<Option<Result<StreamRecord, StreamError>>>, max: usize) -> (Result<Vec<StreamRecord>, StreamError>, usize) {
        let mut it = script.into_iter();
        let mut calls = 0usize;
        let out = drain_batch(max, Duration::from_millis(5), |_timeout| {
            calls += 1;
            it.next().flatten()
        });
        (out, calls)
    }

    #[test]
    fn an_error_after_partial_success_keeps_the_records_already_consumed() {
        // THE DEFECT. rdkafka has already advanced its position past every
        // record handed to us, and the forwarder's commit is CUMULATIVE
        // (max offset + 1). So discarding these does not merely delay them:
        // the next successful cycle commits a HIGHER offset and buries them
        // permanently. Billing under-reports and nothing logs it.
        let (out, _) = drain(
            vec![
                Some(Ok(record(100))),
                Some(Ok(record(101))),
                Some(Err(StreamError::Unavailable("broker went away"))),
            ],
            10,
        );

        let records = out.expect("a batch that already yielded records must not surface as Err");
        assert_eq!(
            records.iter().map(|r| r.offset).collect::<Vec<_>>(),
            vec![100, 101],
            "records consumed before the error were dropped; they are unrecoverable \
             once a later cycle commits past them"
        );
    }

    #[test]
    fn an_error_with_nothing_consumed_still_propagates() {
        // The other half, and the reason this is not just "swallow errors":
        // with no records in hand there is nothing to lose by surfacing the
        // error, and the caller needs to see it. If this regressed to Ok(vec![])
        // the forwarder would treat a dead broker as an idle topic forever.
        let (out, _) = drain(
            vec![Some(Err(StreamError::Unavailable("broker went away")))],
            10,
        );
        assert!(
            out.is_err(),
            "an error with no records collected must reach the caller"
        );
    }

    #[test]
    fn a_full_batch_stops_at_max_without_polling_again() {
        // Guards the loop bound itself: `max` must cap the batch, and the
        // drain must not make a further poll call after reaching it (an extra
        // call would consume a record it then never returns - the same class
        // of loss as the test above, by a different route).
        let (out, calls) = drain(
            vec![
                Some(Ok(record(1))),
                Some(Ok(record(2))),
                Some(Ok(record(3))),
            ],
            2,
        );
        assert_eq!(out.expect("ok").len(), 2);
        assert_eq!(calls, 2, "drain polled {calls} times for a max of 2");
    }

    #[test]
    fn an_empty_broker_yields_an_empty_batch() {
        let (out, calls) = drain(vec![None], 10);
        assert!(out.expect("ok").is_empty());
        assert_eq!(calls, 1);
    }

    /// The stall bound a caller holds the broker to is the operator's
    /// configured `stall_timeout_ms`, not `message_timeout_ms` - which is the
    /// producer's librdkafka `message.timeout.ms` - and not one of the other
    /// timeouts. The two are set to different values in both orders so a swap
    /// shows, and `publish_timeout_ms`/`poll_timeout_ms` differ again.
    ///
    /// Building the transport opens no socket: librdkafka connects lazily and
    /// `subscribe` only records the subscription, so no broker is needed.
    #[test]
    fn the_stall_timeout_follows_its_own_setting_not_the_message_timeout() {
        let config = |message_timeout_ms: u64, stall_timeout_ms: u64| {
            crate::StreamConfig::from(serde_json::json!({
                "brokers": "127.0.0.1:1",
                "topic": "t",
                "group.id": "g",
                "message_timeout_ms": message_timeout_ms,
                "stall_timeout_ms": stall_timeout_ms,
                "publish_timeout_ms": 4321,
                "poll_timeout_ms": 25,
            }))
        };
        for (message_timeout_ms, stall_timeout_ms) in [(1234, 98_765), (98_765, 1234)] {
            let transport = factory(&config(message_timeout_ms, stall_timeout_ms))
                .expect("redpanda transport builds without a live broker");
            assert_eq!(
                transport.stall_timeout(),
                Some(Duration::from_millis(stall_timeout_ms)),
                "the stall timeout must follow stall_timeout_ms ({stall_timeout_ms}), \
                 not message_timeout_ms ({message_timeout_ms})"
            );
        }
    }

    /// A config that sets neither timeout reports the transport's default
    /// stall bound, and that default is today's effective bound - the value
    /// `message_timeout_ms` defaults to - so leaving `stall_timeout_ms` unset
    /// reports the same bound.
    #[test]
    fn an_unset_stall_timeout_reports_todays_effective_bound() {
        let transport = factory(&config_with(serde_json::json!({
            "brokers": "127.0.0.1:1",
            "topic": "t",
            "group.id": "g",
        })))
        .expect("redpanda transport builds without a live broker");
        assert_eq!(
            transport.stall_timeout(),
            Some(Duration::from_millis(default_message_timeout_ms())),
            "an unset stall_timeout_ms must report today's effective bound"
        );
    }

    /// A zero `stall_timeout_ms` is refused by name, the way the other
    /// timeouts are: a bound of zero is a stalled call before it starts.
    #[test]
    fn a_zero_stall_timeout_is_refused_by_name() {
        let refused = factory(&config_with(serde_json::json!({
            "brokers": "127.0.0.1:1",
            "topic": "t",
            "group.id": "g",
            "stall_timeout_ms": 0,
        })));
        match refused {
            Err(StreamError::Config(message)) => assert!(
                message.contains("stall_timeout_ms"),
                "the refusal names the setting: {message}"
            ),
            Err(other) => panic!("a zero stall_timeout_ms must be refused by config: {other:?}"),
            Ok(_) => panic!("a zero stall_timeout_ms must be refused"),
        }
    }

    /// Hand control back to the runtime once, so a spawned spinner advances a
    /// fixed number of ticks with no dependence on a real timer.
    async fn yield_to_runtime() {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;
    }

    /// A transport pointed at a TCP listener that lets the OS accept the
    /// connection and never replies: the broker address that never answers.
    /// The listener is returned with the transport so the caller keeps it alive
    /// for the whole call.
    fn silent_transport(
        stall_timeout_ms: u64,
        poll_timeout_ms: u64,
    ) -> (std::net::TcpListener, std::sync::Arc<dyn StreamTransport>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a silent broker");
        let brokers = listener.local_addr().expect("silent broker address");
        let transport = factory(&config_with(serde_json::json!({
            "brokers": brokers.to_string(),
            "topic": "never-answers",
            "group.id": "stalled-runtime",
            "stall_timeout_ms": stall_timeout_ms,
            "poll_timeout_ms": poll_timeout_ms,
        })))
        .expect("redpanda transport builds without a live broker");
        (listener, transport)
    }

    /// Await `call` while a spawned spinner yields a fixed number of times on
    /// the same compio runtime, and assert the spinner finished BEFORE the call
    /// did. A call that runs on the caller's thread blocks the runtime for its
    /// whole bound, so the spinner cannot run at all until the call returns and
    /// then observes it already done; one that runs on a blocking thread leaves
    /// the runtime serving. Deterministic: the spinner is ticked by yields, not
    /// a sleep, and the call is bounded by its own configured timeout.
    async fn assert_runtime_serves_during<T>(call: impl std::future::Future<Output = T>) -> T {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        const TICKS: usize = 64;
        let ticks = Arc::new(AtomicUsize::new(0));
        let call_returned = Arc::new(AtomicBool::new(false));
        let served_while_stalled = Arc::new(AtomicBool::new(false));

        let spinner = {
            let ticks = Arc::clone(&ticks);
            let call_returned = Arc::clone(&call_returned);
            let served_while_stalled = Arc::clone(&served_while_stalled);
            compio::runtime::spawn(async move {
                for _ in 0..TICKS {
                    yield_to_runtime().await;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
                served_while_stalled.store(
                    !call_returned.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
            })
        };

        let outcome = call.await;
        call_returned.store(true, Ordering::SeqCst);
        spinner.await.expect("the spinner task");

        assert_eq!(
            ticks.load(Ordering::SeqCst),
            TICKS,
            "the spinner ran to completion"
        );
        assert!(
            served_while_stalled.load(Ordering::SeqCst),
            "another task on the runtime must make progress while the consumer call is \
             stalled; it did not run until the blocking call returned"
        );
        outcome
    }

    /// A stalled `end_offsets` against a broker that never answers leaves the
    /// runtime serving other tasks and ends at the stall bound. The bound is
    /// the transport's own, so a stalled capture still surfaces as a Kafka
    /// failure rather than hanging.
    #[compio::test]
    async fn a_stalled_end_offsets_leaves_the_runtime_serving_other_tasks() {
        let (_listener, transport) = silent_transport(300, 25);
        let outcome = assert_runtime_serves_during(transport.end_offsets()).await;
        assert!(
            matches!(outcome, Err(StreamError::Kafka(_))),
            "a broker that never answers must end the capture at the stall bound with a \
             Kafka failure: {outcome:?}"
        );
    }

    /// A stalled `poll` likewise runs off the caller's thread: while it waits
    /// out its poll timeout on a broker that never answers, the runtime serves
    /// another task. The poll timeout is raised so the wait is long enough to
    /// observe, and the call still ends at that bound.
    #[compio::test]
    async fn a_stalled_poll_leaves_the_runtime_serving_other_tasks() {
        let (_listener, transport) = silent_transport(300, 300);
        let outcome = assert_runtime_serves_during(transport.poll(64)).await;
        assert!(
            matches!(outcome, Ok(ref records) if records.is_empty()),
            "a poll against a broker that never answers returns nothing at the bound, \
             not an error: {outcome:?}"
        );
    }

    fn config_with(json: serde_json::Value) -> crate::StreamConfig {
        crate::StreamConfig::from(json)
    }

    /// librdkafka's local `max.poll.interval.ms` eviction arrives as a
    /// `MessageConsumption(PollExceeded)`, and that exact variant AND code pair
    /// is the ONLY thing mapped to [`StreamError::PollExceeded`]: a caller that
    /// treats the variant as an expected rejoin still has to fail on a broker
    /// that is genuinely broken, and those arrive under the same
    /// `MessageConsumption` variant with a different code, or under
    /// `MessageConsumptionFatal` carrying the same poll code.
    #[test]
    fn only_the_max_poll_code_maps_to_poll_exceeded() {
        let poll_exceeded = rdkafka::error::KafkaError::MessageConsumption(
            rdkafka::error::RDKafkaErrorCode::PollExceeded,
        );
        assert!(
            matches!(
                StreamError::from(poll_exceeded),
                StreamError::PollExceeded
            ),
            "the max.poll.interval.ms code must map to the rejoin variant"
        );

        let transport_failure = rdkafka::error::KafkaError::MessageConsumption(
            rdkafka::error::RDKafkaErrorCode::BrokerTransportFailure,
        );
        assert!(
            matches!(
                StreamError::from(transport_failure),
                StreamError::Kafka(_)
            ),
            "every other consumption error must stay a genuine Kafka failure"
        );

        let fatal_poll_exceeded = rdkafka::error::KafkaError::MessageConsumptionFatal(
            rdkafka::error::RDKafkaErrorCode::PollExceeded,
        );
        assert!(
            matches!(
                StreamError::from(fatal_poll_exceeded),
                StreamError::Kafka(_)
            ),
            "a fatal consumption error keeps its identity even when it carries the poll code"
        );
    }

    /// The consumer interval settings reach the client: a config whose
    /// `max.poll.interval.ms` is below its `session.timeout.ms` is refused by
    /// name (librdkafka would refuse it at client creation), and a valid
    /// ordered pair builds. No socket is opened either way.
    #[test]
    fn max_poll_interval_must_not_be_below_the_session_timeout() {
        let base = |session: u64, max_poll: u64| {
            config_with(serde_json::json!({
                "brokers": "127.0.0.1:1",
                "topic": "t",
                "group.id": "g",
                "session.timeout.ms": session,
                "max.poll.interval.ms": max_poll,
            }))
        };

        let refused = factory(&base(6_000, 5_999));
        match refused {
            Err(StreamError::Config(message)) => assert!(
                message.contains("max.poll.interval.ms"),
                "the refusal names the setting: {message}"
            ),
            Err(other) => panic!(
                "the adapter must refuse max.poll.interval.ms below session.timeout.ms by \
                 config, not leave it to librdkafka: {other:?}"
            ),
            Ok(_) => panic!(
                "the adapter must refuse max.poll.interval.ms below session.timeout.ms"
            ),
        }

        factory(&base(6_000, 6_000))
            .expect("a max.poll interval at the session timeout builds");
    }
}
