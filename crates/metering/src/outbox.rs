//! Usage-event outbox for the worker producer.
//!
//! Drained events are persisted to a worker-local redb WAL before publish. A
//! successful stream publish trims only that event's WAL sequence; a failure
//! leaves the event in place so the NEXT DRAIN of the same process replays the
//! same `event_id`.
//!
//! When the append itself fails there is no WAL copy to replay from, and
//! `Meter::drain` has already zeroed the counters, so the producer holds the
//! batch in memory (bounded by [`DEFAULT_MAX_RETAINED_EVENTS`]) and carries it
//! into the next append. Retries are verbatim: the same `event_id` the
//! forwarder dedups on, and the same `event_time` that decides which billing
//! period the usage lands in.
//!
//! It does NOT survive a process restart in the shipped configuration, and an
//! earlier version of this comment claimed it did. The default WAL path is
//! derived from `producer_source`, and both binaries that use it mint that
//! source with a fresh UUID per boot - `crates/worker/src/main.rs` builds
//! `{hostname}-{uuid}`, `crates/gateway/src/main.rs` builds
//! `gate-{hostname}-{uuid}`. A restart therefore opens a DIFFERENT, empty redb
//! file, orphaning whatever was unpublished and leaving the old file on disk
//! with nothing that reads it. Nothing in `deploy/` overrides the path;
//! `USAGE_OUTBOX_WAL_PATH` is set only by the e2e scripts.
//!
//! `zeroship-control` does not share the defect: it passes a stable constant
//! (`DEFAULT_CONTROL_USAGE_OUTBOX_WAL_PATH`).
//!
//! Making the path stable is NOT sufficient on its own and must not be done
//! alone: redb is single-writer, so co-located producers sharing one path would
//! fail to open, and a failed open currently degrades to a drain-and-drop task
//! rather than refusing to boot. Stable paths have to land together with that
//! fail-closed change, or an intermittent partial loss becomes a permanent
//! total one.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use zeroship_core::usage_event::UsageEvent;
use zeroship_stream::{StreamConfig, StreamRegistry, StreamTransport};

use crate::Meter;

pub const DEFAULT_OUTBOX_INTERVAL: Duration = Duration::from_secs(10);
pub const DEFAULT_USAGE_EVENTS_TOPIC: &str = "usage-events";

/// How many events a producer holds in memory for retry after a failed WAL
/// append. The bound is a count, not a byte budget, because that is the only
/// quantity the producer can enforce without measuring per-event heap use.
///
/// Deliberately NOT a staleness horizon. The two consumers of the usage stream
/// treat a late event differently: the spend-recompute snapshot ignores any
/// event whose `event_time` falls outside the period it is recomputing, so a
/// retry that arrives after that period settles no longer moves enforcement.
/// The provider forwarder has no such gate - it attributes and bills whatever
/// decodes. Expiring retained events on a staleness rule would therefore throw
/// away revenue that would still have been invoiced. Old retained events lose
/// their enforcement value; they keep their billing value.
///
/// Beyond this many retained events the oldest are dropped; see
/// [`UsageOutbox::retain_for_retry`] for why that direction was chosen.
pub const DEFAULT_MAX_RETAINED_EVENTS: usize = 100_000;

/// Resolved usage-stream producer settings. The source of these values is the
/// caller's concern (config-file overlay, env, CLI) — this crate only consumes
/// the resolved struct, so the billing stream is configurable from
/// `zeroship.toml` (via the `[metering]` overlay) as well as the environment.
#[derive(Debug, Clone, Default)]
pub struct UsageStreamSettings {
    /// Kafka-wire brokers. `None`/empty ⇒ the producer is disabled.
    pub brokers: Option<String>,
    /// Usage-event topic (default [`DEFAULT_USAGE_EVENTS_TOPIC`]).
    pub topic: Option<String>,
    /// Producer consumer-group id override.
    pub group_id: Option<String>,
    /// redb WAL path override.
    pub wal_path: Option<String>,
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

impl UsageStreamSettings {
    /// Read the settings from the environment (`REDPANDA_BROKERS`,
    /// `USAGE_EVENTS_TOPIC`, `REDPANDA_PRODUCER_GROUP_ID`, `USAGE_OUTBOX_WAL_PATH`).
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            brokers: env_nonempty("REDPANDA_BROKERS"),
            topic: env_nonempty("USAGE_EVENTS_TOPIC"),
            group_id: env_nonempty("REDPANDA_PRODUCER_GROUP_ID"),
            wal_path: env_nonempty("USAGE_OUTBOX_WAL_PATH"),
        }
    }

    /// Back-fill any field left `None` on `self` from `fallback` (so `self`, the
    /// higher-precedence source such as env, wins over the file overlay).
    #[must_use]
    pub fn or(mut self, fallback: Self) -> Self {
        self.brokers = self.brokers.or(fallback.brokers);
        self.topic = self.topic.or(fallback.topic);
        self.group_id = self.group_id.or(fallback.group_id);
        self.wal_path = self.wal_path.or(fallback.wal_path);
        self
    }
}

/// Build a usage-event outbox (redpanda transport + redb WAL) from resolved
/// [`UsageStreamSettings`], or `None` when no brokers are configured (producer
/// disabled). Shared by the worker and gateway producers. The WAL path defaults
/// per-`producer_source` so a worker and a gateway on the same host never
/// contend for one single-writer redb file.
pub fn build_usage_outbox(
    producer_source: &str,
    settings: &UsageStreamSettings,
) -> Result<Option<(UsageOutbox, OutboxConfig)>, String> {
    let Some(brokers) = settings
        .brokers
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let topic = settings
        .topic
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_USAGE_EVENTS_TOPIC)
        .to_string();
    let group_id = settings
        .group_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("zeroship-producer-{producer_source}"));
    let raw_config = serde_json::json!({
        "brokers": brokers,
        "topic": topic,
        "group_id": group_id,
        "client_id": format!("zeroship-{producer_source}"),
    });

    let mut registry = StreamRegistry::default();
    zeroship_stream::adapters::register_builtin(&mut registry);
    let stream = registry
        .build("redpanda", &StreamConfig::new(raw_config))
        .map_err(|e| e.to_string())?;
    let config = OutboxConfig {
        topic: topic.clone(),
        interval: DEFAULT_OUTBOX_INTERVAL,
    };
    let wal_path = settings
        .wal_path
        .clone()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_wal_path(producer_source));
    let outbox = UsageOutbox::new(stream, topic, wal_path).map_err(|e| e.to_string())?;
    Ok(Some((outbox, config)))
}

/// Where the WAL lives when the caller sets no explicit path.
///
/// Extracted so the derivation is testable without building a transport or
/// writing a redb file into the working directory. The choice of
/// `producer_source` as the key is what decides whether a restarted process
/// finds its predecessor's unpublished events - see the module docs and
/// `restart_with_a_new_boot_source_gets_a_different_wal`.
fn default_wal_path(producer_source: &str) -> PathBuf {
    let safe: String = producer_source
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    PathBuf::from(format!(".zeroship/usage-outbox-{safe}.redb"))
}

#[derive(Debug, Clone)]
pub struct OutboxConfig {
    pub topic: String,
    pub interval: Duration,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            topic: DEFAULT_USAGE_EVENTS_TOPIC.to_string(),
            interval: DEFAULT_OUTBOX_INTERVAL,
        }
    }
}

#[derive(Clone)]
pub struct UsageOutbox {
    stream: Arc<dyn StreamTransport>,
    topic: String,
    wal: Arc<UsageWal>,
    /// Events whose WAL append failed, held verbatim until an append succeeds.
    /// Shared across clones because every clone publishes into the same WAL and
    /// must see the same backlog. Oldest first.
    retained: Arc<Mutex<VecDeque<UsageEvent>>>,
    max_retained: usize,
}

impl std::fmt::Debug for UsageOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageOutbox")
            .field("stream", &self.stream.id())
            .field("topic", &self.topic)
            .field("wal_path", &self.wal.path)
            .field("retained", &self.retained_len())
            .finish()
    }
}

impl UsageOutbox {
    pub fn new(
        stream: Arc<dyn StreamTransport>,
        topic: impl Into<String>,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self, OutboxWalError> {
        Ok(Self {
            stream,
            topic: topic.into(),
            wal: Arc::new(UsageWal::open(wal_path)?),
            retained: Arc::new(Mutex::new(VecDeque::new())),
            max_retained: DEFAULT_MAX_RETAINED_EVENTS,
        })
    }

    /// Override how many events this producer retains in memory after a failed
    /// WAL append.
    #[must_use]
    pub fn with_max_retained_events(mut self, max_retained: usize) -> Self {
        self.max_retained = max_retained;
        self
    }

    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Whether any event is waiting in memory for a WAL append to succeed. A
    /// caller that only publishes when it has fresh events must also publish
    /// when this is true, or a backlog left by a failed append never drains on
    /// an idle node.
    #[must_use]
    pub fn has_pending_retry(&self) -> bool {
        self.retained_len() > 0
    }

    #[must_use]
    pub fn retained_len(&self) -> usize {
        self.lock_retained().len()
    }

    /// A poisoned lock means some other caller panicked mid-publish. Recovering
    /// the buffer is better than propagating the panic: the alternative kills
    /// the outbox task and drops every retained event.
    fn lock_retained(&self) -> std::sync::MutexGuard<'_, VecDeque<UsageEvent>> {
        self.retained.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the retry backlog and put `events` after it, oldest first.
    fn batch_with_retained(&self, events: &[UsageEvent]) -> Vec<UsageEvent> {
        let mut retained = self.lock_retained();
        let mut batch = Vec::with_capacity(retained.len() + events.len());
        batch.extend(retained.drain(..));
        batch.extend_from_slice(events);
        batch
    }

    /// Hold a batch whose WAL append failed so the next append can carry it.
    ///
    /// The events are kept verbatim. `event_id` is the idempotency key the
    /// forwarder dedups on, and `event_time` decides which billing period the
    /// usage lands in, so a retry has to present the same event the first
    /// attempt did - re-measuring or re-stamping would move usage between
    /// periods.
    ///
    /// Over `max_retained` the oldest events are dropped. Dropping is a real
    /// loss and it under-bills; it is bounded and logged, where an unbounded
    /// buffer would grow until the process died and lose everything. Oldest
    /// rather than newest because the newest window is what spend enforcement
    /// acts on and is the one still certain to fall inside the open period.
    fn retain_for_retry(&self, batch: Vec<UsageEvent>) {
        let mut retained = self.lock_retained();
        retained.extend(batch);
        let overflow = retained.len().saturating_sub(self.max_retained);
        if overflow > 0 {
            let dropped_value: u64 = retained
                .drain(..overflow)
                .map(|event| event.value)
                .fold(0, u64::saturating_add);
            tracing::error!(
                dropped = overflow,
                dropped_value,
                retained = retained.len(),
                max_retained = self.max_retained,
                topic = %self.topic,
                "meter outbox retry buffer full; dropped oldest usage events (under-billing)"
            );
        }
    }

    /// Durably append usage events to the local WAL without touching the
    /// configured stream. Producers on latency-sensitive request paths can
    /// acknowledge once this returns and leave publishing to one background
    /// drainer.
    pub fn enqueue_events(&self, events: &[UsageEvent]) -> Result<(), OutboxWalError> {
        self.wal.append(events)
    }

    /// Persist a drained window, then publish every unacked WAL event. Each
    /// `UsageEvent` is one stream record because the forwarder decodes each
    /// record payload as a single event.
    ///
    /// `events` is joined by any backlog a previous failed append left in
    /// memory, so a caller that has nothing new still flushes that backlog.
    pub async fn publish_events(&self, events: &[UsageEvent]) -> OutboxPublishResult {
        let batch = self.batch_with_retained(events);
        if let Err(error) = self.enqueue_events(&batch) {
            // The caller drained its counters to build `events`, so this batch
            // is the only copy that exists. Hold it for the next append instead
            // of letting the window die with this call.
            let error = error.to_string();
            let failed = batch
                .iter()
                .map(|event| OutboxFailure {
                    event_id: event.event_id.clone(),
                    app_id: event.subject.app,
                    meter: event.meter.clone(),
                    error: format!("wal append: {error}"),
                })
                .collect();
            let attempted = batch.len();
            self.retain_for_retry(batch);
            tracing::error!(
                attempted,
                retained = self.retained_len(),
                error = %error,
                "meter outbox WAL append failed; retaining events for retry"
            );
            return OutboxPublishResult {
                attempted,
                published: 0,
                failed,
            };
        }
        let pending = match self.wal.load_pending() {
            Ok(pending) => pending,
            Err(error) => {
                // Not retained, unlike the append failure above: the append
                // committed, so the WAL owns these events and replays them on
                // the next call. Retaining as well would publish each twice.
                let error = error.to_string();
                tracing::error!(
                    error = %error,
                    "meter outbox WAL read failed; events stay in the WAL for the next attempt"
                );
                return OutboxPublishResult {
                    attempted: batch.len(),
                    published: 0,
                    failed: batch
                        .iter()
                        .map(|event| OutboxFailure {
                            event_id: event.event_id.clone(),
                            app_id: event.subject.app,
                            meter: event.meter.clone(),
                            error: format!("wal read: {error}"),
                        })
                        .collect(),
                };
            }
        };
        let mut result = OutboxPublishResult {
            attempted: pending.len(),
            ..OutboxPublishResult::default()
        };

        for pending_event in pending {
            let event = &pending_event.event;
            let Some(app_id) = event.subject.app else {
                let failure = OutboxFailure {
                    event_id: event.event_id.clone(),
                    app_id: None,
                    meter: event.meter.clone(),
                    error: "usage event missing subject.app for partition key".to_string(),
                };
                tracing::warn!(
                    event_id = %failure.event_id,
                    meter = %failure.meter,
                    error = %failure.error,
                    "meter outbox publish failed"
                );
                result.failed.push(failure);
                continue;
            };

            let payload = match serde_json::to_vec(event) {
                Ok(payload) => payload,
                Err(error) => {
                    let failure = OutboxFailure {
                        event_id: event.event_id.clone(),
                        app_id: Some(app_id),
                        meter: event.meter.clone(),
                        error: format!("serialize usage event: {error}"),
                    };
                    tracing::warn!(
                        event_id = %failure.event_id,
                        app_id = %app_id,
                        meter = %failure.meter,
                        error = %failure.error,
                        "meter outbox publish failed"
                    );
                    result.failed.push(failure);
                    continue;
                }
            };

            let key = app_id.to_string();
            match self
                .stream
                .publish(&self.topic, key.as_bytes(), &payload)
                .await
            {
                Ok(()) => {
                    match self.wal.remove(pending_event.seq) {
                        Ok(()) => {
                            result.published += 1;
                        }
                        Err(error) => {
                            let failure = OutboxFailure {
                                event_id: event.event_id.clone(),
                                app_id: Some(app_id),
                                meter: event.meter.clone(),
                                error: format!("wal trim: {error}"),
                            };
                            tracing::error!(
                                stream = self.stream.id(),
                                topic = %self.topic,
                                event_id = %failure.event_id,
                                app_id = %app_id,
                                meter = %failure.meter,
                                error = %failure.error,
                                "meter outbox published but failed to trim WAL; event will replay"
                            );
                            result.failed.push(failure);
                        }
                    }
                }
                Err(error) => {
                    let failure = OutboxFailure {
                        event_id: event.event_id.clone(),
                        app_id: Some(app_id),
                        meter: event.meter.clone(),
                        error: error.to_string(),
                    };
                    tracing::warn!(
                        stream = self.stream.id(),
                        topic = %self.topic,
                        event_id = %failure.event_id,
                        app_id = %app_id,
                        meter = %failure.meter,
                        error = %failure.error,
                        "meter outbox publish failed"
                    );
                    result.failed.push(failure);
                }
            }
        }

        result
    }
}

const WAL_EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("usage_events");
const WAL_META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const NEXT_SEQ_KEY: &str = "next_seq";

#[derive(Debug)]
struct UsageWal {
    db: Database,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct PendingWalEvent {
    seq: u64,
    event: UsageEvent,
}

#[derive(Debug, thiserror::Error)]
pub enum OutboxWalError {
    #[error("{0}")]
    Redb(String),
    #[error("serialize usage event {event_id}: {source}")]
    Serialize {
        event_id: String,
        source: serde_json::Error,
    },
    #[error("decode WAL event at seq {seq}: {source}")]
    Decode { seq: u64, source: serde_json::Error },
}

impl UsageWal {
    fn open(path: impl AsRef<Path>) -> Result<Self, OutboxWalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                OutboxWalError::Redb(format!(
                    "create usage outbox WAL dir '{}': {e}",
                    parent.display()
                ))
            })?;
        }
        let db = Database::create(&path).map_err(|e| {
            OutboxWalError::Redb(format!("open usage outbox WAL '{}': {e}", path.display()))
        })?;
        Ok(Self { db, path })
    }

    fn append(&self, events: &[UsageEvent]) -> Result<(), OutboxWalError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::with_capacity(events.len());
        for event in events {
            let payload = serde_json::to_vec(event).map_err(|source| {
                OutboxWalError::Serialize {
                    event_id: event.event_id.clone(),
                    source,
                }
            })?;
            encoded.push(payload);
        }

        let tx = self
            .db
            .begin_write()
            .map_err(|e| OutboxWalError::Redb(format!("wal append begin_write: {e}")))?;
        let mut next_seq = {
            let meta = tx
                .open_table(WAL_META)
                .map_err(|e| OutboxWalError::Redb(format!("wal append open meta: {e}")))?;
            let guard = meta
                .get(NEXT_SEQ_KEY)
                .map_err(|e| OutboxWalError::Redb(format!("wal append read next_seq: {e}")))?;
            let next_seq = guard.as_ref().map(|v| v.value()).unwrap_or(0);
            next_seq
        };
        {
            let mut table = tx
                .open_table(WAL_EVENTS)
                .map_err(|e| OutboxWalError::Redb(format!("wal append open events: {e}")))?;
            for payload in &encoded {
                table
                    .insert(next_seq, payload.as_slice())
                    .map_err(|e| OutboxWalError::Redb(format!("wal append insert: {e}")))?;
                next_seq = next_seq.checked_add(1).ok_or_else(|| {
                    OutboxWalError::Redb("wal append sequence overflow".to_string())
                })?;
            }
        }
        {
            let mut meta = tx
                .open_table(WAL_META)
                .map_err(|e| OutboxWalError::Redb(format!("wal append reopen meta: {e}")))?;
            meta.insert(NEXT_SEQ_KEY, next_seq)
                .map_err(|e| OutboxWalError::Redb(format!("wal append write next_seq: {e}")))?;
        }
        tx.commit()
            .map_err(|e| OutboxWalError::Redb(format!("wal append commit: {e}")))
    }

    fn load_pending(&self) -> Result<Vec<PendingWalEvent>, OutboxWalError> {
        let tx = self
            .db
            .begin_read()
            .map_err(|e| OutboxWalError::Redb(format!("wal read begin_read: {e}")))?;
        let table = match tx.open_table(WAL_EVENTS) {
            Ok(table) => table,
            Err(e) if is_missing_table(&e) => return Ok(Vec::new()),
            Err(e) => return Err(OutboxWalError::Redb(format!("wal read open events: {e}"))),
        };
        let mut out = Vec::new();
        let iter = table
            .iter()
            .map_err(|e| OutboxWalError::Redb(format!("wal read iter: {e}")))?;
        for entry in iter {
            let (seq, payload) =
                entry.map_err(|e| OutboxWalError::Redb(format!("wal read row: {e}")))?;
            let seq = seq.value();
            let event = serde_json::from_slice(payload.value()).map_err(|source| {
                OutboxWalError::Decode { seq, source }
            })?;
            out.push(PendingWalEvent { seq, event });
        }
        Ok(out)
    }

    fn remove(&self, seq: u64) -> Result<(), OutboxWalError> {
        let tx = self
            .db
            .begin_write()
            .map_err(|e| OutboxWalError::Redb(format!("wal trim begin_write: {e}")))?;
        {
            let mut table = tx
                .open_table(WAL_EVENTS)
                .map_err(|e| OutboxWalError::Redb(format!("wal trim open events: {e}")))?;
            table
                .remove(seq)
                .map_err(|e| OutboxWalError::Redb(format!("wal trim remove: {e}")))?;
        }
        tx.commit()
            .map_err(|e| OutboxWalError::Redb(format!("wal trim commit: {e}")))
    }
}

fn is_missing_table(err: &TableError) -> bool {
    matches!(err, TableError::TableDoesNotExist(_))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutboxPublishResult {
    pub attempted: usize,
    pub published: usize,
    pub failed: Vec<OutboxFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxFailure {
    pub event_id: String,
    pub app_id: Option<uuid::Uuid>,
    pub meter: String,
    pub error: String,
}

pub fn spawn_outbox_task(meter: Arc<Meter>, outbox: UsageOutbox, config: OutboxConfig) {
    compio::runtime::spawn(async move {
        loop {
            compio::time::sleep(config.interval).await;
            let events = meter.drain();
            // An empty drain still needs a publish when a previous append
            // failed, otherwise the retained backlog never flushes on a node
            // that has gone idle.
            if events.is_empty() && !outbox.has_pending_retry() {
                continue;
            }
            let result = outbox.publish_events(&events).await;
            if result.failed.is_empty() {
                tracing::debug!(
                    events = result.published,
                    topic = %outbox.topic(),
                    "meter outbox published usage events"
                );
            } else {
                tracing::warn!(
                    attempted = result.attempted,
                    published = result.published,
                    failed = result.failed.len(),
                    topic = %outbox.topic(),
                    "meter outbox completed with publish failures"
                );
            }
        }
    })
    .detach();
}

/// Dev/no-broker mode: keep draining so counters do not grow without bound, but
/// make the under-billing posture explicit in logs.
pub fn spawn_disabled_drain_task(meter: Arc<Meter>, interval: Duration, reason: String) {
    compio::runtime::spawn(async move {
        tracing::warn!(
            reason = %reason,
            "meter outbox disabled; usage events will be drained and dropped"
        );
        loop {
            compio::time::sleep(interval).await;
            let events = meter.drain();
            if !events.is_empty() {
                tracing::warn!(
                    events = events.len(),
                    reason = %reason,
                    "meter outbox disabled; dropped usage events"
                );
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use uuid::Uuid;
    use zeroship_stream::{StreamError, StreamOffset, StreamRecord};

    use super::*;

    #[derive(Debug, Default)]
    struct FakeStream {
        published: Mutex<Vec<Published>>,
        fail_next: Mutex<usize>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Published {
        topic: String,
        key: Vec<u8>,
        event: UsageEvent,
    }

    #[async_trait::async_trait(?Send)]
    impl StreamTransport for FakeStream {
        fn id(&self) -> &str {
            "fake"
        }

        async fn publish(
            &self,
            topic: &str,
            partition_key: &[u8],
            payload: &[u8],
        ) -> Result<(), StreamError> {
            let mut fail_next = self.fail_next.lock().unwrap();
            if *fail_next > 0 {
                *fail_next -= 1;
                return Err(StreamError::Unavailable("injected publish failure"));
            }
            drop(fail_next);
            let event: UsageEvent = serde_json::from_slice(payload).map_err(StreamError::from)?;
            self.published.lock().unwrap().push(Published {
                topic: topic.to_string(),
                key: partition_key.to_vec(),
                event,
            });
            Ok(())
        }

        async fn poll(&self, _max: usize) -> Result<Vec<StreamRecord>, StreamError> {
            Ok(Vec::new())
        }

        async fn commit(&self, _offsets: &[StreamOffset]) -> Result<(), StreamError> {
            Ok(())
        }

        async fn rewind(&self) -> Result<(), StreamError> {
            Ok(())
        }
    }

    /// Pins the defect: two boots of the SAME process on the SAME host get
    /// different WAL files, so a restart never finds its predecessor's
    /// unpublished events.
    ///
    /// This asserts what the code does today, NOT what it should do. It exists
    /// because the defect was previously visible only in prose, and prose is
    /// exactly what a green suite does not check. The other restart-shaped test
    /// in this module hand-passes one path to both opens, so it proves the WAL
    /// layer replays and says nothing about whether a restart reaches the same
    /// file - its passing is why this went unnoticed.
    ///
    /// WHAT IT CATCHES, stated narrowly because the first version of this
    /// comment claimed more than the test delivers. Verified by mutation:
    /// collapsing `default_wal_path` to a constant turns it red.
    ///
    /// WHAT IT DOES NOT CATCH, and this is the likelier fix: the per-boot UUID
    /// is minted in `crates/worker/src/main.rs` and `crates/gateway/src/main.rs`,
    /// NOT here. Stop appending it there and the restart defect is fixed while
    /// this test stays green, because it hands `default_wal_path` two different
    /// source strings by hand and only ever observes the derivation. This crate
    /// cannot see those binaries, so nothing here can assert on the source they
    /// mint - a test that covers it belongs beside them.
    ///
    /// So: a red here means the DERIVATION changed. It is not, on its own, a
    /// signal that the restart defect is fixed or unfixed.
    #[test]
    fn restart_with_a_new_boot_source_gets_a_different_wal() {
        // What crates/worker/src/main.rs builds: `{hostname}-{uuid-per-boot}`.
        let boot_a = default_wal_path("worker-host-11111111111111111111111111111111");
        let boot_b = default_wal_path("worker-host-22222222222222222222222222222222");
        assert_ne!(
            boot_a, boot_b,
            "two boots on one host share a WAL path; if this now holds, the \
             restart-loses-usage defect is fixed and this test should assert \
             equality instead"
        );

        // The host part is not what separates them - only the per-boot suffix
        // is, which is what makes this a restart problem rather than a
        // multi-host one.
        let gate = default_wal_path("gate-worker-host-11111111111111111111111111111111");
        assert_ne!(gate, boot_a, "a gateway and a worker must not share a file");

        // The same source is stable across calls, so the path is a pure
        // function of the source and nothing else drifts.
        assert_eq!(
            boot_a,
            default_wal_path("worker-host-11111111111111111111111111111111"),
        );
    }

    #[test]
    fn drain_produces_usage_events_and_outbox_publishes_by_app() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let meter = Meter::with_source("worker-test");
            let app_a = Uuid::new_v4();
            let app_b = Uuid::new_v4();
            meter.increment(&app_a.to_string(), "requests", 2);
            meter.increment(&app_a.to_string(), "db_reads", 5);
            meter.increment(&app_b.to_string(), "kv_writes", 3);

            let events = meter.drain();
            assert_eq!(events.len(), 3);
            assert_event(&events, app_a, "requests", 2);
            assert_event(&events, app_a, "db_reads", 5);
            assert_event(&events, app_b, "kv_writes", 3);
            for event in &events {
                assert!(!event.event_id.is_empty());
                assert_eq!(event.source, "worker-test");
                assert!(event.event_time > 0);
            }

            let stream = Arc::new(FakeStream::default());
            let dir = tempfile::tempdir().expect("tempdir");
            let outbox = UsageOutbox::new(
                stream.clone(),
                "usage-events-test",
                dir.path().join("outbox.redb"),
            )
            .expect("open outbox WAL");
            let result = outbox.publish_events(&events).await;
            assert_eq!(result.attempted, 3);
            assert_eq!(result.published, 3);
            assert!(result.failed.is_empty());

            let published = stream.published.lock().unwrap().clone();
            assert_eq!(published.len(), events.len());
            for published in published {
                let app_id = published.event.subject.app.expect("app id present");
                assert_eq!(published.topic, "usage-events-test");
                assert_eq!(published.key, app_id.to_string().as_bytes());
                assert!(events.contains(&published.event));
            }

            assert!(
                meter.drain().is_empty(),
                "second drain without new increments yields nothing"
            );
        });
    }

    /// Proves the WAL layer replays an unpublished event when the SAME path is
    /// reopened.
    ///
    /// It does not prove that a process restart replays anything, despite the
    /// `drop` + reopen below looking like one. The path is hand-passed on both
    /// opens; production derives it from a per-boot UUID and so never reopens
    /// the same file (see the module docs). A test that used
    /// `build_usage_outbox` with two different boot sources would fail today.
    #[test]
    fn publish_failure_retains_event_and_retries_next_attempt() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let meter = Meter::with_source("worker-test");
            let app = Uuid::new_v4();
            meter.increment(&app.to_string(), "requests", 2);
            let events = meter.drain();
            assert_eq!(events.len(), 1);

            let stream = Arc::new(FakeStream {
                published: Mutex::new(Vec::new()),
                fail_next: Mutex::new(1),
            });
            let dir = tempfile::tempdir().expect("tempdir");
            let wal_path = dir.path().join("outbox.redb");
            let outbox = UsageOutbox::new(stream.clone(), "usage-events-test", &wal_path)
                .expect("open outbox WAL");

            let first = outbox.publish_events(&events).await;
            assert_eq!(first.attempted, 1);
            assert_eq!(first.published, 0);
            assert_eq!(first.failed.len(), 1);
            assert!(
                stream.published.lock().unwrap().is_empty(),
                "failed publish did not reach the stream"
            );
            drop(outbox);

            let restarted = UsageOutbox::new(stream.clone(), "usage-events-test", &wal_path)
                .expect("reopen outbox WAL");
            let second = restarted.publish_events(&[]).await;
            assert_eq!(second.attempted, 1);
            assert_eq!(second.published, 1);
            assert!(second.failed.is_empty());
            let published = stream.published.lock().unwrap().clone();
            assert_eq!(published.len(), 1);
            assert_eq!(published[0].event.event_id, events[0].event_id);

            let third = restarted.publish_events(&[]).await;
            assert_eq!(third.attempted, 0);
            assert_eq!(third.published, 0);
            assert!(third.failed.is_empty());
        });
    }

    /// Overwrite the WAL's sequence cursor through the live `Database` the
    /// outbox already holds. redb is single-writer, so the test cannot open a
    /// second handle on the same file; going through `wal.db` also means the
    /// injection needs no test-only hook in the production type.
    ///
    /// Setting the cursor to `u64::MAX` makes the next `append` overflow and
    /// abort its write transaction, which is a real failure of the real append
    /// path rather than a stubbed error.
    fn set_next_seq(wal: &UsageWal, next_seq: u64) {
        let tx = wal.db.begin_write().expect("begin seed write");
        {
            let mut meta = tx.open_table(WAL_META).expect("open meta for seeding");
            meta.insert(NEXT_SEQ_KEY, next_seq).expect("seed next_seq");
        }
        tx.commit().expect("commit seed");
    }

    /// A failed WAL append must not destroy the drained window. `Meter::drain`
    /// already zeroed the counters, so the events in flight are the only copy
    /// left anywhere.
    #[test]
    fn wal_append_failure_retains_drained_counts_for_verbatim_retry() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let meter = Meter::with_source("worker-test");
            let app = Uuid::new_v4();
            meter.increment(&app.to_string(), "requests", 7);
            meter.increment(&app.to_string(), "db_reads", 3);

            let events = meter.drain();
            assert_eq!(events.len(), 2);
            assert!(
                meter.drain().is_empty(),
                "drain zeroed the counters: these events are the only copy"
            );

            let stream = Arc::new(FakeStream::default());
            let dir = tempfile::tempdir().expect("tempdir");
            let outbox = UsageOutbox::new(
                stream.clone(),
                "usage-events-test",
                dir.path().join("outbox.redb"),
            )
            .expect("open outbox WAL");

            set_next_seq(&outbox.wal, u64::MAX);
            let first = outbox.publish_events(&events).await;

            assert_eq!(first.published, 0);
            assert_eq!(first.failed.len(), 2);
            for failure in &first.failed {
                assert!(
                    failure.error.contains("wal append sequence overflow"),
                    "the append error path was taken, not some later failure: {}",
                    failure.error
                );
            }
            assert!(
                stream.published.lock().unwrap().is_empty(),
                "nothing reached the stream"
            );
            assert!(
                outbox.wal.load_pending().expect("read wal").is_empty(),
                "the append aborted, so the WAL holds nothing: the counts survive only if retained"
            );

            // The disk recovers. No new drain happens - the counters are gone,
            // so anything published now can only come from the retained window.
            set_next_seq(&outbox.wal, 0);
            let second = outbox.publish_events(&[]).await;
            assert_eq!(second.published, 2, "the retained window is republished");
            assert!(second.failed.is_empty());

            let published: Vec<UsageEvent> = stream
                .published
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.event.clone())
                .collect();
            assert_eq!(published.len(), 2);
            for original in &events {
                assert!(
                    published.contains(original),
                    "retry is verbatim - same event_id, event_time and value: {original:?}"
                );
            }

            let third = outbox.publish_events(&[]).await;
            assert_eq!(third.attempted, 0, "retained events are not republished");
            assert_eq!(stream.published.lock().unwrap().len(), 2);
        });
    }

    /// The retain buffer is capped, and overflow drops the oldest events. That
    /// is a loss, but a bounded and logged one on a node whose WAL has been
    /// failing long enough to accumulate `max_retained_events` windows.
    #[test]
    fn retained_events_are_capped_and_drop_oldest() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let stream = Arc::new(FakeStream::default());
            let dir = tempfile::tempdir().expect("tempdir");
            let outbox = UsageOutbox::new(
                stream.clone(),
                "usage-events-test",
                dir.path().join("outbox.redb"),
            )
            .expect("open outbox WAL")
            .with_max_retained_events(2);

            set_next_seq(&outbox.wal, u64::MAX);
            let mut drained = Vec::new();
            for value in 1..=3u64 {
                let meter = Meter::with_source("worker-test");
                meter.increment(&Uuid::new_v4().to_string(), "requests", value);
                let events = meter.drain();
                assert_eq!(events.len(), 1);
                let result = outbox.publish_events(&events).await;
                assert_eq!(result.published, 0);
                drained.push(events[0].clone());
            }

            set_next_seq(&outbox.wal, 0);
            let flush = outbox.publish_events(&[]).await;
            assert_eq!(flush.published, 2, "the cap holds at two events");

            let published: Vec<UsageEvent> = stream
                .published
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.event.clone())
                .collect();
            assert!(
                !published.contains(&drained[0]),
                "the oldest retained event was dropped"
            );
            assert!(published.contains(&drained[1]));
            assert!(published.contains(&drained[2]));
        });
    }

    fn assert_event(events: &[UsageEvent], app_id: Uuid, meter: &str, value: u64) {
        let event = events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .expect("usage event exists");
        assert_eq!(event.value, value);
    }
}
