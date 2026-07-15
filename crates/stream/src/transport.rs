use async_trait::async_trait;

use crate::StreamError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRecord {
    pub partition: i32,
    /// Kafka record offset. Passing this value to `commit` commits `offset + 1`
    /// as the next record to consume for the same partition.
    pub offset: i64,
    pub key: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamOffset {
    pub partition: i32,
    /// Inclusive last-processed record offset. Transports commit `offset + 1`
    /// to their authoritative offset store.
    pub offset: i64,
}

impl From<&StreamRecord> for StreamOffset {
    fn from(value: &StreamRecord) -> Self {
        Self {
            partition: value.partition,
            offset: value.offset,
        }
    }
}

/// Kafka-family stream contract:
///
/// - partition offsets are monotonic and stored by the transport;
/// - consumer groups assign each partition to one consumer in steady state;
/// - `partition_key` controls partition placement and preserves per-key order.
#[async_trait(?Send)]
pub trait StreamTransport: Send + Sync {
    fn id(&self) -> &str;

    async fn publish(
        &self,
        topic: &str,
        partition_key: &[u8],
        payload: &[u8],
    ) -> Result<(), StreamError>;

    async fn poll(&self, max: usize) -> Result<Vec<StreamRecord>, StreamError>;

    async fn commit(&self, offsets: &[StreamOffset]) -> Result<(), StreamError>;

    /// Rewind this consumer to the retained beginning of its assigned stream.
    ///
    /// The billing event-forwarder never calls this; it relies on normal
    /// consumer-group offsets. The control spend recompute cron uses a
    /// dedicated consumer group and rewinds before each full-period snapshot so
    /// reruns overwrite the same `usage_aggregates` totals instead of adding or
    /// reading only a tail.
    async fn rewind(&self) -> Result<(), StreamError>;
}
