# Adding A Stream Transport

Usage events move through the durable stream abstraction in `crates/stream`.
The stream registry is intentionally Kafka-family shaped: transports provide
partition offsets, consumer groups, and partition-key ordering. A new transport
is a self-contained adapter plus one registration entry in
`crates/zeroship-stream/src/adapters/mod.rs`.

## Stream Contract

Implement `StreamTransport` in `crates/zeroship-stream/src/transport.rs`:

```rust
#[async_trait::async_trait(?Send)]
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

    async fn rewind(&self) -> Result<(), StreamError> {
        Ok(())
    }
}
```

The required semantics are:

- **Partition offsets.** `StreamRecord.offset` is the record's Kafka-style
  offset. `StreamOffset` is the inclusive last processed offset; `commit`
  commits `offset + 1` as the next record to consume.
- **Consumer groups.** Each configured group owns its committed positions in the
  transport. In steady state, one consumer in a group processes a partition.
- **Partition-key ordering.** Records published with the same `partition_key`
  stay on the same partition in publish order. The usage outbox uses the app id
  as the key so one app's usage has a stable order.
- **Manual commit.** The event forwarder commits only after the meter provider
  accepts the batch or the batch is quarantined as a permanent provider reject.
- **Retained rewind.** Spend recompute uses a dedicated consumer group and calls
  `rewind()` before a full-period snapshot. A transport used for enforcement
  must rewind its assigned partitions to the retained beginning.

Offset-less or at-most-once buses do not meet this contract unless the adapter
adds equivalent durable positions, consumer grouping, and per-key ordering.

## Config And Factory

Factories receive an opaque `StreamConfig`. Parse and validate the transport's
own config shape in the adapter:

```rust
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AcmeStreamConfig {
    brokers: String,
    topic: String,
    #[serde(rename = "group.id", alias = "group_id")]
    group_id: String,
}

pub fn factory(config: &StreamConfig) -> Result<Arc<dyn StreamTransport>, StreamError> {
    let config: AcmeStreamConfig = config.parse()?;
    if config.brokers.trim().is_empty() {
        return Err(StreamError::Config("acme: brokers required".to_string()));
    }
    if config.topic.trim().is_empty() {
        return Err(StreamError::Config("acme: topic required".to_string()));
    }
    if config.group_id.trim().is_empty() {
        return Err(StreamError::Config("acme: group.id required".to_string()));
    }
    Ok(Arc::new(AcmeStream::new(config)?))
}
```

Validation should fail closed. A transport without a topic, group id, or broker
address should refuse to boot rather than fall back to a lossy mode.

## Registration

Add the adapter file under `crates/zeroship-stream/src/adapters/`, then register it:

```rust
pub mod acme;

pub fn register_builtin(registry: &mut StreamRegistry) {
    registry.register("acme", acme::factory);
}
```

The control plane builds the selected transport from `--stream-transport` /
`ZEROSHIP_CONTROL_STREAM_TRANSPORT` and `--stream-config` / `ZEROSHIP_CONTROL_STREAM_CONFIG`.

## Worked Examples

`redpanda` is the production transport in
`crates/zeroship-stream/src/adapters/redpanda.rs`.

- Config: `brokers`, `topic`, `group_id`, optional `client_id`, publish and poll
  timeouts, and `auto_offset_reset`.
- Producer: `acks=all` and idempotence enabled.
- Consumer: auto commit and auto offset store disabled.
- `commit` writes the highest processed offset per partition as `offset + 1`.
- `rewind` seeks assigned partitions to the beginning for retained snapshots.

Example config:

```json
{
  "brokers": "127.0.0.1:9092",
  "topic": "usage-events",
  "group_id": "zeroship-control-forwarder",
  "client_id": "zeroship-control"
}
```

`memory` is the in-process transport in
`crates/zeroship-stream/src/adapters/memory.rs`.

- Config: `topic`, `group_id`, and `partitions`.
- It stores topic logs and committed offsets in process memory.
- It preserves per-key ordering with a stable hash to partition mapping.
- It is useful for tests and local fixtures, not as a durable production buffer.

## Verification

Run the stream crate tests after adding a transport:

```bash
nix develop --command cargo test -p zeroship-stream
```

Add a focused roundtrip test that proves publish, poll, commit, and rewind
semantics for the new transport.
