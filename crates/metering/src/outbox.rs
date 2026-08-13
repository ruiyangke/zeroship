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
//! It DOES survive a process restart. It did not until 2026-08-07, and the
//! reason is worth keeping because the mechanism was not the obvious one.
//!
//! redb's `Database::create` opens an existing file rather than truncating it,
//! and `load_pending` already ran on every publish cycle, so the replay
//! machinery was complete and working the whole time. What broke was the KEY:
//! the default WAL path was derived from `producer_source`, and both binaries
//! mint that source with a fresh uuid per boot. A restart therefore aimed a
//! working replay at a NEW, empty file and orphaned the old one on disk with
//! nothing left that could read it. Nothing in `deploy/` overrode the path.
//!
//! The fix separates the two identities that had been one string, because they
//! have opposite requirements - see [`WalIdentity`], which is a newtype
//! precisely so a producer source cannot be passed where a WAL key belongs.
//! `zeroship-control` never had the defect: it passes a stable constant
//! (`DEFAULT_CONTROL_USAGE_OUTBOX_WAL_PATH`).
//!
//! A stable path could NOT land on its own, and this is the part that makes
//! the two changes one change. redb is single-writer, so co-located producers
//! sharing a path fail to open - and a failed open used to degrade to a
//! drain-and-drop task rather than refusing to boot. Stabilising the path
//! alone would therefore have converted an intermittent partial loss into a
//! permanent total one. The worker and gateway now treat a build failure as
//! fatal when brokers are configured, so the two properties hold together or
//! not at all.

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

/// Treat a blank value as absent.
///
/// This takes the ALREADY-READ value. It used to be `env_nonempty(key: &str)`
/// and perform the read itself, which is the exact shape Section 4.5 of
/// `docs/proposals/2026-08-11-config-name-alignment.md` names: a helper that
/// accepts a `&str` name is still a read, and no scan of this file could
/// attribute it to `REDPANDA_BROKERS` or to anything else, because the four
/// names lived at the CALL sites and the read lived here. Moving the read up
/// to `from_env` puts a literal next to each `declared_env!`, so the source
/// gate sees four named, classified reads instead of one anonymous one.
///
/// Note this is now a pure predicate on an `Option<String>`, so it also serves
/// a value that did not come from the environment at all.
fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

impl UsageStreamSettings {
    /// Read the settings from the environment (`REDPANDA_BROKERS`,
    /// `USAGE_EVENTS_TOPIC`, `REDPANDA_PRODUCER_GROUP_ID`, `USAGE_OUTBOX_WAL_PATH`).
    #[must_use]
    pub fn from_env() -> Self {
        // All four are class `external`: the broker address, the topic and the
        // consumer-group id are the surrounding deployment's Kafka-family
        // contract, and none of them is a zeroship-canonical name.
        Self {
            brokers: nonempty(zeroship_core::declared_env!(
                external,
                "REDPANDA_BROKERS",
                crate::MeteringConsumer
            )),
            topic: nonempty(zeroship_core::declared_env!(
                external,
                "USAGE_EVENTS_TOPIC",
                crate::MeteringConsumer
            )),
            group_id: nonempty(zeroship_core::declared_env!(
                external,
                "REDPANDA_PRODUCER_GROUP_ID",
                crate::MeteringConsumer
            )),
            wal_path: nonempty(zeroship_core::declared_env!(
                external,
                "USAGE_OUTBOX_WAL_PATH",
                crate::MeteringConsumer
            )),
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
/// disabled). Shared by the worker and gateway producers.
///
/// Takes the producer identity and the WAL identity SEPARATELY, and they must
/// stay separate. `producer_source` names this live producer and carries a
/// per-boot uuid so two running producers never collide on a client or group
/// id. [`WalIdentity`] names the file this producer's unpublished events
/// survive in, and must be identical across restarts of the same producer or
/// the restarted process opens an empty WAL and orphans them.
///
/// An `Err` here means brokers WERE configured and the producer could not be
/// built - most often because the WAL would not open, which on a stable path
/// is what a co-located second producer looks like (redb is single-writer).
/// Callers must treat that as fatal rather than degrading to a drain-and-drop
/// task: dropping every event forever is strictly worse than the intermittent
/// loss the drop was standing in for.
pub fn build_usage_outbox(
    producer_source: &str,
    wal: &WalIdentity,
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
        .unwrap_or_else(|| default_wal_path(wal));
    let outbox = UsageOutbox::new(stream, topic, wal_path).map_err(|e| e.to_string())?;
    Ok(Some((outbox, config)))
}

/// The stable key a producer's WAL file is named for.
///
/// A NEWTYPE rather than a `&str`, and that is the whole point of it. The
/// original defect was not that anyone chose a bad path - it was that
/// `build_usage_outbox` took one string and used it for BOTH the producer
/// identity and the WAL name, and those two have opposite requirements:
///
///   * the producer/client id must be unique per LIVE producer, so the worker
///     and gateway mint it with a fresh uuid per boot;
///   * the WAL name must survive exactly the event that changes that uuid.
///
/// With one `&str` parameter the call sites satisfied the first and silently
/// broke the second, and no test could bind them because the mistake was in
/// what the caller passed, not in what the callee did. Making the WAL
/// parameter a type that a producer source cannot coerce into means the call
/// site is checked by the compiler on every future edit instead of by a test
/// that has to remember to look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalIdentity(String);

impl WalIdentity {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Build the stable WAL key for a producer.
///
/// `role` is what keeps co-located producers off one file, which matters
/// because redb is single-writer: a gateway and a worker on one host must not
/// name the same path. `host` is whatever the binary uses to distinguish
/// itself from a peer on another machine (hostname, else bind address).
///
/// Both parts are sanitised the same way the path is, so a host carrying a `/`
/// or a `..` cannot walk out of the WAL directory.
#[must_use]
pub fn wal_identity(role: &str, host: &str) -> WalIdentity {
    WalIdentity(format!(
        "{}-{}",
        sanitise_path_component(role),
        sanitise_path_component(host)
    ))
}

fn sanitise_path_component(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Where the WAL lives when the caller sets no explicit path.
///
/// Extracted so the derivation is testable without building a transport or
/// writing a redb file into the working directory. Takes a [`WalIdentity`]
/// rather than a bare string so the key cannot silently become the per-boot
/// producer source again - see `a_restart_reuses_the_previous_boots_wal`.
fn default_wal_path(wal: &WalIdentity) -> PathBuf {
    PathBuf::from(format!(".zeroship/usage-outbox-{}.redb", wal.as_str()))
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
    fn a_restart_reuses_the_previous_boots_wal() {
        // This used to assert INEQUALITY and carried a note saying that if it
        // ever held, the restart-loses-usage defect was fixed and the test
        // should assert equality instead. This is that rewrite.
        //
        // The WAL identity is deliberately NOT the producer source. The source
        // still carries a per-boot uuid, because producer/client ids must not
        // collide between two live producers; the WAL must survive exactly the
        // event that changes that uuid.
        let boot_a = default_wal_path(&wal_identity("worker", "host"));
        let boot_b = default_wal_path(&wal_identity("worker", "host"));
        assert_eq!(
            boot_a, boot_b,
            "two boots of one worker must resolve to the same WAL file, or the \
             restarted process opens an empty one and orphans whatever the \
             previous boot had not yet published"
        );

        // Distinctness still has to hold where it always did: redb is
        // single-writer, so co-located producers must not name one file.
        assert_ne!(
            boot_a,
            default_wal_path(&wal_identity("gate", "host")),
            "a gateway and a worker on one host must not share a file"
        );
        assert_ne!(
            boot_a,
            default_wal_path(&wal_identity("worker", "other-host")),
            "two hosts must not share a file"
        );
    }

    #[test]
    fn a_wal_identity_carries_no_per_boot_component() {
        // The defect was not that the path was recreated - redb's
        // `Database::create` opens an existing file. It was that the KEY
        // changed every boot, so a working replay path pointed at a new empty
        // file. This asserts the property that was actually missing, so the
        // regression cannot come back by someone threading a fresh uuid into
        // the identity again.
        //
        // Deliberately calls the seam twice rather than comparing one call to
        // a literal: a hardcoded expected string would still pass if the
        // function grew a random component AND the literal were updated to
        // match one sample.
        // POSITIVE CONTROL first. `contains_uuid_shape` returning false is the
        // pass condition below, so a detector that never fires would make this
        // test green against the exact code it exists to reject. Prove it
        // fires on what the worker actually used to pass - `{host}-{uuid}` -
        // in both the raw and path-sanitised spellings.
        let boot_uuid = Uuid::new_v4().to_string();
        assert!(
            contains_uuid_shape(&format!("worker-host-{boot_uuid}")),
            "detector missed a raw per-boot source; the negative results below \
             would be meaningless"
        );
        assert!(
            contains_uuid_shape(&sanitise_path_component(&format!(
                "worker-host-{boot_uuid}"
            ))),
            "detector missed the sanitised spelling, which is the form that \
             actually reached the WAL path"
        );
        // And that it is not simply always-true.
        assert!(!contains_uuid_shape("worker-host"));

        for role in ["worker", "gate"] {
            let first = wal_identity(role, "host");
            let second = wal_identity(role, "host");
            assert_eq!(
                first, second,
                "wal_identity({role}, host) is not stable across calls"
            );
            // Not "does it contain THIS uuid" - a freshly minted uuid can never
            // appear in a string derived from other inputs, so that assertion
            // would pass against any implementation, including the broken one.
            // Look for the SHAPE instead.
            assert!(
                !contains_uuid_shape(first.as_str()),
                "identity {:?} embeds something uuid-shaped; the per-boot \
                 component belongs in the producer source, not the WAL key",
                first.as_str()
            );
        }
    }

    /// True when `s` contains 32 hex digits in uuid layout, with `-` or `_`
    /// separators (the path sanitiser rewrites `-` to `_`, so a uuid that
    /// reached the identity would show up in the underscored form).
    fn contains_uuid_shape(s: &str) -> bool {
        let bytes: Vec<char> = s.chars().collect();
        let groups = [8usize, 4, 4, 4, 12];
        (0..bytes.len()).any(|start| {
            let mut i = start;
            for (g, len) in groups.iter().enumerate() {
                if g > 0 {
                    match bytes.get(i) {
                        Some('-' | '_') => i += 1,
                        _ => return false,
                    }
                }
                for _ in 0..*len {
                    match bytes.get(i) {
                        Some(c) if c.is_ascii_hexdigit() => i += 1,
                        _ => return false,
                    }
                }
            }
            true
        })
    }

    #[test]
    fn a_reopened_wal_still_holds_the_unpublished_events() {
        // The half of the fix that the path change exists to enable. Without
        // this, a stable path would be necessary but unproven: the claim is
        // that a NEW `UsageWal` over the SAME file sees what the previous one
        // wrote and never published.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("restart.redb");

        let first = UsageWal::open(&path).expect("open wal");
        first
            .append(&[usage_event(Uuid::now_v7(), "requests", 7)])
            .expect("append");
        assert_eq!(first.load_pending().expect("pending").len(), 1);
        drop(first);

        let second = UsageWal::open(&path).expect("reopen wal");
        let pending = second.load_pending().expect("pending after reopen");
        assert_eq!(
            pending.len(),
            1,
            "a reopened WAL lost the unpublished event, so a stable path would \
             buy nothing"
        );
        assert_eq!(pending[0].event.meter, "requests");
        assert_eq!(pending[0].event.value, 7);
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

    fn usage_event(app_id: Uuid, meter: &str, value: u64) -> UsageEvent {
        UsageEvent {
            event_id: Uuid::now_v7().to_string(),
            source: "test".to_string(),
            subject: zeroship_core::usage_event::UsageSubject {
                app: Some(app_id),
                creator: Uuid::now_v7(),
            },
            meter: meter.to_string(),
            value,
            event_time: 0,
            dims: Default::default(),
        }
    }

    fn assert_event(events: &[UsageEvent], app_id: Uuid, meter: &str, value: u64) {
        let event = events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .expect("usage event exists");
        assert_eq!(event.value, value);
    }
}
