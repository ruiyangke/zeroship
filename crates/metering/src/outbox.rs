//! Usage-event outbox for the worker producer.
//!
//! Drained events are persisted to a worker-local redb WAL before publish. A
//! successful stream publish trims only that event's WAL sequence; failures leave
//! the event in place so the next drain or process restart replays the same
//! `event_id`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use zeroship_core::usage_event::UsageEvent;
use zeroship_stream::{StreamConfig, StreamRegistry, StreamTransport};

use crate::Meter;

pub const DEFAULT_OUTBOX_INTERVAL: Duration = Duration::from_secs(10);
pub const DEFAULT_USAGE_EVENTS_TOPIC: &str = "usage-events";

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
        .unwrap_or_else(|| {
            let safe: String = producer_source
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            PathBuf::from(format!(".zeroship/usage-outbox-{safe}.redb"))
        });
    let outbox = UsageOutbox::new(stream, topic, wal_path).map_err(|e| e.to_string())?;
    Ok(Some((outbox, config)))
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
}

impl std::fmt::Debug for UsageOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageOutbox")
            .field("stream", &self.stream.id())
            .field("topic", &self.topic)
            .field("wal_path", &self.wal.path)
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
        })
    }

    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Persist a drained window, then publish every unacked WAL event. Each
    /// `UsageEvent` is one stream record because the forwarder decodes each
    /// record payload as a single event.
    pub async fn publish_events(&self, events: &[UsageEvent]) -> OutboxPublishResult {
        let append_failures = match self.wal.append(events) {
            Ok(()) => Vec::new(),
            Err(error) => {
                let error = error.to_string();
                tracing::error!(
                    attempted = events.len(),
                    error = %error,
                    "meter outbox WAL append failed; refusing unprotected publish"
                );
                return OutboxPublishResult {
                    attempted: events.len(),
                    published: 0,
                    failed: events
                        .iter()
                        .map(|event| OutboxFailure {
                            event_id: event.event_id.clone(),
                            app_id: event.subject.app,
                            meter: event.meter.clone(),
                            error: format!("wal append: {error}"),
                        })
                        .collect(),
                };
            }
        };
        let pending = match self.wal.load_pending() {
            Ok(pending) => pending,
            Err(error) => {
                let error = error.to_string();
                tracing::error!(
                    error = %error,
                    "meter outbox WAL read failed; refusing unprotected publish"
                );
                return OutboxPublishResult {
                    attempted: events.len(),
                    published: 0,
                    failed: append_failures
                        .into_iter()
                        .chain(events.iter().map(|event| OutboxFailure {
                            event_id: event.event_id.clone(),
                            app_id: event.subject.app,
                            meter: event.meter.clone(),
                            error: format!("wal read: {error}"),
                        }))
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
            if events.is_empty() {
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

    fn assert_event(events: &[UsageEvent], app_id: Uuid, meter: &str, value: u64) {
        let event = events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .expect("usage event exists");
        assert_eq!(event.value, value);
    }
}
