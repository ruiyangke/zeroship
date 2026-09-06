//! Billing event forwarder: stream → provider meter → stream offset commit.
//!
//! v7 scope is deliberately narrow. This component does not update Redis, does
//! not fold usage into Postgres, and does not run enforcement. The provider is
//! the billing exactness boundary; the stream offset is committed only after the
//! meter-capable provider(s) have accepted or permanently rejected the batch.

use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use zeroship_stream::{StreamOffset, StreamRecord, StreamTransport};

use uuid::Uuid;

use crate::metering::provider::{BillingStack, ProviderError, UsageEvent};

pub const DEFAULT_BATCH_MAX: usize = 500;
pub const DEFAULT_IDLE_SLEEP_MS: u64 = 200;
pub const DEFAULT_RETRY_BACKOFF_MS: u64 = 500;
pub const DEFAULT_MAX_RETRY_BACKOFF_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct EventForwarderConfig {
    pub batch_max: usize,
    pub idle_sleep: Duration,
    pub retry_backoff: Duration,
    pub max_retry_backoff: Duration,
}

impl Default for EventForwarderConfig {
    fn default() -> Self {
        Self {
            batch_max: DEFAULT_BATCH_MAX,
            idle_sleep: Duration::from_millis(DEFAULT_IDLE_SLEEP_MS),
            retry_backoff: Duration::from_millis(DEFAULT_RETRY_BACKOFF_MS),
            max_retry_backoff: Duration::from_millis(DEFAULT_MAX_RETRY_BACKOFF_MS),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EventForwarderCycle {
    pub polled: usize,
    pub ingested: usize,
    pub deduped: Option<usize>,
    pub dead_lettered: usize,
    pub committed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDeadLetter {
    pub provider_id: String,
    pub event: UsageEvent,
    pub reason: String,
    pub partition: i32,
    pub offset: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeDeadLetter {
    pub provider_id: String,
    pub partition: i32,
    pub offset: i64,
    pub key: Vec<u8>,
    pub payload: Vec<u8>,
    pub error: String,
}

#[async_trait::async_trait(?Send)]
pub trait DeadLetterSink: Send + Sync {
    async fn record_provider_reject(&self, entry: ProviderDeadLetter)
        -> Result<(), EventForwarderError>;

    async fn record_decode_error(&self, entry: DecodeDeadLetter)
        -> Result<(), EventForwarderError>;
}

#[derive(Clone)]
pub struct PgDeadLetterSink {
    conn: Arc<compio_postgres::Client>,
}

impl std::fmt::Debug for PgDeadLetterSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgDeadLetterSink")
            .finish_non_exhaustive()
    }
}

impl PgDeadLetterSink {
    #[must_use]
    pub fn new(conn: Arc<compio_postgres::Client>) -> Self {
        Self { conn }
    }
}

#[async_trait::async_trait(?Send)]
impl DeadLetterSink for PgDeadLetterSink {
    #[allow(clippy::future_not_send)]
    async fn record_provider_reject(
        &self,
        entry: ProviderDeadLetter,
    ) -> Result<(), EventForwarderError> {
        let id = zeroship_core::typed_id::new_provider_dead_letter_id();
        let subject = serde_json::to_value(&entry.event.subject)
            .map_err(|e| EventForwarderError::DeadLetter(e.to_string()))?;
        let dims = serde_json::to_value(&entry.event.dims)
            .map_err(|e| EventForwarderError::DeadLetter(e.to_string()))?;
        let event_json = serde_json::to_value(&entry.event)
            .map_err(|e| EventForwarderError::DeadLetter(e.to_string()))?;
        let provider_json = serde_json::json!({
            "provider": entry.provider_id,
            "reason": entry.reason,
            "partition": entry.partition,
            "offset": entry.offset,
        });
        let value = i64::try_from(entry.event.value).map_err(|_| {
            EventForwarderError::DeadLetter(format!(
                "usage event {} value {} exceeds i64::MAX",
                entry.event.event_id, entry.event.value
            ))
        })?;

        self.conn
            .query(
                "INSERT INTO zeroship.provider_dead_letter \
                   (id, provider_id, event_id, source, subject, meter, event_time, value, dims, reason) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (provider_id, event_id) DO NOTHING",
                &[
                    &id,
                    &entry.provider_id,
                    &entry.event.event_id,
                    &entry.event.source,
                    &subject,
                    &entry.event.meter,
                    &entry.event.event_time,
                    &value,
                    &dims,
                    &entry.reason,
                ],
            )
            .await
            .map_err(|e| EventForwarderError::DeadLetter(e.to_string()))?;

        let finding_id = zeroship_core::typed_id::new_reconcile_finding_id();
        let entity_id = format!("{}:{}", entry.provider_id, entry.event.event_id);
        let dedup_key = format!("provider_reject:{entity_id}");
        self.conn
            .query(
                "INSERT INTO zeroship.billing_reconciliation_findings \
                   (id, kind, severity, entity_id, our_value, stripe_value, dedup_key) \
                 VALUES ($1, 'provider_reject'::text::zeroship.reconciliation_finding_kind, \
                         'high'::text::zeroship.reconciliation_finding_severity, $2, $3, $4, $5) \
                 ON CONFLICT (dedup_key) DO NOTHING",
                &[&finding_id, &entity_id, &event_json, &provider_json, &dedup_key],
            )
            .await
            .map_err(|e| EventForwarderError::DeadLetter(e.to_string()))?;

        Ok(())
    }

    #[allow(clippy::future_not_send)]
    async fn record_decode_error(
        &self,
        entry: DecodeDeadLetter,
    ) -> Result<(), EventForwarderError> {
        let id = zeroship_core::typed_id::new_provider_dead_letter_id();
        let event_id = decode_dead_letter_event_id(entry.partition, entry.offset);
        let subject = serde_json::json!({
            "stream_key_base64": BASE64.encode(&entry.key),
        });
        let dims = serde_json::json!({
            "stream_partition": entry.partition,
            "stream_offset": entry.offset,
            "raw_payload_base64": BASE64.encode(&entry.payload),
            "decode_error": entry.error,
        });
        self.conn
            .query(
                "INSERT INTO zeroship.provider_dead_letter \
                   (id, provider_id, event_id, source, subject, meter, event_time, value, dims, reason) \
                 VALUES ($1, $2, $3, 'stream', $4, 'decode_error', 0, 0, $5, 'decode_error') \
                 ON CONFLICT (provider_id, event_id) DO NOTHING",
                &[&id, &entry.provider_id, &event_id, &subject, &dims],
            )
            .await
            .map_err(|e| EventForwarderError::DeadLetter(e.to_string()))?;

        let finding_id = zeroship_core::typed_id::new_reconcile_finding_id();
        let entity_id = format!(
            "{}:{}:{}",
            entry.provider_id, entry.partition, entry.offset
        );
        let dedup_key = format!("decode_error:{entity_id}");
        let our_value = serde_json::json!({
            "partition": entry.partition,
            "offset": entry.offset,
            "raw_payload_base64": BASE64.encode(&entry.payload),
        });
        let provider_value = serde_json::json!({
            "provider": entry.provider_id,
            "reason": "decode_error",
            "error": entry.error,
        });
        self.conn
            .query(
                "INSERT INTO zeroship.billing_reconciliation_findings \
                   (id, kind, severity, entity_id, our_value, stripe_value, dedup_key) \
                 VALUES ($1, 'provider_reject'::text::zeroship.reconciliation_finding_kind, \
                         'high'::text::zeroship.reconciliation_finding_severity, $2, $3, $4, $5) \
                 ON CONFLICT (dedup_key) DO NOTHING",
                &[&finding_id, &entity_id, &our_value, &provider_value, &dedup_key],
            )
            .await
            .map_err(|e| EventForwarderError::DeadLetter(e.to_string()))?;

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventForwarderError {
    #[error("stream: {0}")]
    Stream(#[from] zeroship_stream::StreamError),
    #[error("decode usage event at partition {partition} offset {offset}: {source}")]
    Decode {
        partition: i32,
        offset: i64,
        source: serde_json::Error,
    },
    #[error("billing stack has no meter-capable provider")]
    NoMeterProvider,
    #[error("provider {provider_id}: {source}")]
    Provider {
        provider_id: String,
        source: ProviderError,
    },
    #[error("dead-letter: {0}")]
    DeadLetter(String),
}

/// Resolves the OWNING creator for an app so the forwarder can attribute usage
/// events to the right provider customer. The worker producer only has the
/// server-injected `app_id` and stamps `subject.creator = nil`; the control
/// plane owns the app→creator mapping (the app's project, its organization, and
/// that organization's owner), so it must enrich the creator here before
/// forwarding to a per-creator provider.
#[async_trait::async_trait(?Send)]
pub trait CreatorResolver: Send + Sync {
    async fn creator_for_app(&self, app_id: Uuid) -> Result<Option<Uuid>, EventForwarderError>;
}

/// Postgres-backed [`CreatorResolver`], over
/// [`crate::organizations::app_owner_lateral`].
pub struct PgCreatorResolver {
    conn: Arc<compio_postgres::Client>,
}

impl std::fmt::Debug for PgCreatorResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCreatorResolver").finish_non_exhaustive()
    }
}

impl PgCreatorResolver {
    #[must_use]
    pub fn new(conn: Arc<compio_postgres::Client>) -> Self {
        Self { conn }
    }
}

#[async_trait::async_trait(?Send)]
impl CreatorResolver for PgCreatorResolver {
    async fn creator_for_app(&self, app_id: Uuid) -> Result<Option<Uuid>, EventForwarderError> {
        let rows = self
            .conn
            .query(
                &format!(
                    "SELECT app_owner.user_id FROM zeroship.apps a {lateral} WHERE a.id = $1",
                    lateral = crate::organizations::app_owner_lateral(),
                ),
                &[&app_id],
            )
            .await
            .map_err(|e| EventForwarderError::DeadLetter(format!("creator resolve: {e}")))?;
        Ok(rows.first().and_then(|r| r.get::<_, Option<Uuid>>("user_id")))
    }
}

/// Run forever. Transient provider/stream failures retry with bounded backoff;
/// offsets are not committed for transient failures.
#[allow(clippy::future_not_send)]
pub async fn run(
    stream: Arc<dyn StreamTransport>,
    stack: Arc<BillingStack>,
    dead_letters: Arc<dyn DeadLetterSink>,
    creator_resolver: Arc<dyn CreatorResolver>,
    cfg: EventForwarderConfig,
) {
    tracing::info!(
        stream = stream.id(),
        batch_max = cfg.batch_max,
        meter_provider = stack.meter_id(),
        "control event_forwarder cron starting"
    );

    let mut backoff = cfg.retry_backoff;
    loop {
        match run_cycle(
            stream.as_ref(),
            &stack,
            dead_letters.as_ref(),
            creator_resolver.as_ref(),
            &cfg,
        )
        .await
        {
            Ok(cycle) => {
                if cycle.polled > 0 {
                    tracing::info!(
                        polled = cycle.polled,
                        ingested = cycle.ingested,
                        deduped = ?cycle.deduped,
                        dead_lettered = cycle.dead_lettered,
                        committed = cycle.committed,
                        "control event_forwarder batch completed"
                    );
                }
                backoff = cfg.retry_backoff;
                if cycle.polled == 0 {
                    compio::time::sleep(cfg.idle_sleep).await;
                }
            }
            Err(err) => {
                tracing::error!(error = %err, backoff_ms = backoff.as_millis(), "control event_forwarder cycle failed");
                compio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(cfg.max_retry_backoff);
            }
        }
    }
}

/// Run one poll/forward/commit cycle.
#[allow(clippy::future_not_send)]
pub async fn run_cycle(
    stream: &dyn StreamTransport,
    stack: &BillingStack,
    dead_letters: &dyn DeadLetterSink,
    creator_resolver: &dyn CreatorResolver,
    cfg: &EventForwarderConfig,
) -> Result<EventForwarderCycle, EventForwarderError> {
    let max = cfg.batch_max.max(1);
    let records = stream.poll(max).await?;
    if records.is_empty() {
        return Ok(EventForwarderCycle::default());
    }

    let mut events = Vec::with_capacity(records.len());
    let mut event_records = Vec::with_capacity(records.len());
    let mut creator_cache = std::collections::HashMap::<Uuid, Option<Uuid>>::new();
    let mut cycle = EventForwarderCycle {
        polled: records.len(),
        ..EventForwarderCycle::default()
    };
    for record in &records {
        match decode_record(record) {
            Ok(mut event) => {
                // Attribute the event to its owning creator: the worker stamps
                // `creator = nil` (it only has the app id), so a per-creator
                // provider (openmeter/lago/stripe_meters) would otherwise bill
                // EVERY app's usage to one nil customer. Resolve app→creator and
                // dead-letter events we cannot attribute rather than mis-bill.
                if event.subject.creator.is_nil() {
                    let creator = match event.subject.app {
                        Some(app_id) => {
                            if let Some(cached) = creator_cache.get(&app_id) {
                                *cached
                            } else {
                                let resolved = creator_resolver.creator_for_app(app_id).await?;
                                creator_cache.insert(app_id, resolved);
                                resolved
                            }
                        }
                        None => None,
                    };
                    match creator {
                        Some(creator_id) => event.subject.creator = creator_id,
                        None => {
                            dead_letters
                                .record_provider_reject(ProviderDeadLetter {
                                    provider_id: stack.meter.id().to_string(),
                                    event: event.clone(),
                                    reason: "no owning creator for app; cannot attribute usage"
                                        .to_string(),
                                    partition: record.partition,
                                    offset: record.offset,
                                })
                                .await?;
                            cycle.dead_lettered += 1;
                            tracing::warn!(
                                provider = stack.meter.id(),
                                app_id = ?event.subject.app,
                                partition = record.partition,
                                offset = record.offset,
                                "usage event has no owning creator; quarantined"
                            );
                            continue;
                        }
                    }
                }
                events.push(event);
                event_records.push(record);
            }
            Err(EventForwarderError::Decode {
                partition,
                offset,
                source,
            }) => {
                let error = source.to_string();
                dead_letters
                    .record_decode_error(DecodeDeadLetter {
                        provider_id: stack.meter.id().to_string(),
                        partition,
                        offset,
                        key: record.key.clone(),
                        payload: record.payload.clone(),
                        error: error.clone(),
                    })
                    .await?;
                cycle.dead_lettered += 1;
                tracing::warn!(
                    provider = stack.meter.id(),
                    partition,
                    offset,
                    error = %error,
                    "usage event decode failed; quarantined raw stream record"
                );
            }
            Err(other) => return Err(other),
        }
    }

    let meter = stack
        .meter
        .as_meter()
        .ok_or(EventForwarderError::NoMeterProvider)?;

    if !events.is_empty() {
        match meter.ingest(&events).await {
            Ok(ack) => {
                cycle.ingested += ack.accepted;
                if let Some(deduped) = ack.deduped {
                    cycle.deduped = Some(cycle.deduped.unwrap_or(0) + deduped);
                }
            }
            Err(err) if err.is_permanent_reject() => {
                let reason = err.reject_reason();
                for (event, record) in events.iter().zip(event_records.iter()) {
                    dead_letters
                        .record_provider_reject(ProviderDeadLetter {
                            provider_id: stack.meter.id().to_string(),
                            event: event.clone(),
                            reason: reason.clone(),
                            partition: record.partition,
                            offset: record.offset,
                        })
                        .await?;
                    cycle.dead_lettered += 1;
                }
                tracing::warn!(
                    provider = stack.meter.id(),
                    reason = %reason,
                    dead_lettered = events.len(),
                    "billing provider permanently rejected usage batch; committed after quarantine"
                );
            }
            Err(source) => {
                return Err(EventForwarderError::Provider {
                    provider_id: stack.meter.id().to_string(),
                    source,
                });
            }
        }
    }

    // Forward-then-commit ordering is deliberately at-least-once and a commit
    // failure here is SAFE, not a lost/skipped batch: the events were already
    // ingested by the provider above, commits are cumulative (a later cycle's
    // commit covers these offsets), and on a crash before the next commit the
    // consumer resumes from the last committed offset and re-forwards — the
    // provider dedups by `event_id` (its `DedupContract`). So a transient commit
    // error costs at most a re-forward, never a billing gap.
    let offsets: Vec<_> = records.iter().map(StreamOffset::from).collect();
    stream.commit(&offsets).await?;
    cycle.committed = records.len();
    Ok(cycle)
}

fn decode_record(record: &StreamRecord) -> Result<UsageEvent, EventForwarderError> {
    serde_json::from_slice(&record.payload).map_err(|source| EventForwarderError::Decode {
        partition: record.partition,
        offset: record.offset,
        source,
    })
}

fn decode_dead_letter_event_id(partition: i32, offset: i64) -> String {
    format!("decode:{partition}:{offset}")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::sync::{Arc, Mutex};

    use crate::metering::provider::{
        AggregateQuery, Capabilities, DedupContract, DedupKey, DedupTtl, IngestAck, Meter,
        MeteringProvider, UsageSubject,
    };
    use uuid::Uuid;
    use zeroship_stream::{StreamError, StreamOffset, StreamRecord};

    use super::*;

    #[derive(Debug, Default)]
    struct FakeStream {
        records: Mutex<Vec<StreamRecord>>,
        committed: Mutex<Vec<StreamOffset>>,
    }

    #[async_trait::async_trait(?Send)]
    impl StreamTransport for FakeStream {
        fn id(&self) -> &str {
            "fake"
        }

        async fn publish(
            &self,
            _topic: &str,
            _partition_key: &[u8],
            _payload: &[u8],
        ) -> Result<(), StreamError> {
            Ok(())
        }

        async fn poll(&self, max: usize) -> Result<Vec<StreamRecord>, StreamError> {
            let records = self.records.lock().expect("fake stream records poisoned");
            Ok(records.iter().take(max).cloned().collect())
        }

        async fn commit(&self, offsets: &[StreamOffset]) -> Result<(), StreamError> {
            self.committed
                .lock()
                .expect("fake stream commits poisoned")
                .extend_from_slice(offsets);
            Ok(())
        }

        async fn rewind(&self) -> Result<(), StreamError> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct RecordingMeter {
        seen: Mutex<HashSet<String>>,
        accepted_ids: Mutex<Vec<String>>,
        attempted_ids: Mutex<Vec<String>>,
        ingested_events: Mutex<Vec<UsageEvent>>,
        permanent_reject: bool,
    }

    #[async_trait::async_trait(?Send)]
    impl Meter for RecordingMeter {
        async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
            if self.permanent_reject {
                return Err(ProviderError::permanent_reject(400, "unknown subject"));
            }
            let mut seen = self.seen.lock().expect("seen poisoned");
            let mut accepted_ids = self.accepted_ids.lock().expect("accepted poisoned");
            let mut attempted_ids = self.attempted_ids.lock().expect("attempts poisoned");
            let mut ingested_events = self.ingested_events.lock().expect("ingested poisoned");
            let mut accepted = 0;
            let mut deduped = 0;
            for event in batch {
                attempted_ids.push(event.event_id.clone());
                ingested_events.push(event.clone());
                if seen.insert(event.event_id.clone()) {
                    accepted_ids.push(event.event_id.clone());
                    accepted += 1;
                } else {
                    deduped += 1;
                }
            }
            Ok(IngestAck {
                accepted,
                deduped: Some(deduped),
            })
        }

        async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
            Ok(0)
        }
    }

    impl MeteringProvider for RecordingMeter {
        fn id(&self) -> &str {
            "recording"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::METER
        }

        fn as_meter(&self) -> Option<&dyn Meter> {
            Some(self)
        }

        fn dedup(&self) -> DedupContract {
            DedupContract {
                key: DedupKey::SourceAndId,
                ttl: DedupTtl::Unbounded,
            }
        }
    }

    #[derive(Debug, Default)]
    struct RecordingDeadLetters {
        entries: Mutex<Vec<ProviderDeadLetter>>,
        decode_entries: Mutex<Vec<DecodeDeadLetter>>,
    }

    #[async_trait::async_trait(?Send)]
    impl DeadLetterSink for RecordingDeadLetters {
        async fn record_provider_reject(
            &self,
            entry: ProviderDeadLetter,
        ) -> Result<(), EventForwarderError> {
            self.entries
                .lock()
                .expect("dead letters poisoned")
                .push(entry);
            Ok(())
        }

        async fn record_decode_error(
            &self,
            entry: DecodeDeadLetter,
        ) -> Result<(), EventForwarderError> {
            self.decode_entries
                .lock()
                .expect("decode dead letters poisoned")
                .push(entry);
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct MockCreatorResolver {
        mapping: std::collections::HashMap<Uuid, Uuid>,
    }

    #[async_trait::async_trait(?Send)]
    impl CreatorResolver for MockCreatorResolver {
        async fn creator_for_app(&self, app_id: Uuid) -> Result<Option<Uuid>, EventForwarderError> {
            Ok(self.mapping.get(&app_id).copied())
        }
    }

    #[compio::test]
    async fn run_cycle_ingests_batch_and_commits_redelivered_offsets_idempotently() {
        let ids = vec!["evt_1".to_string(), "evt_2".to_string(), "evt_3".to_string()];
        let stream = fake_stream(ids.iter().map(String::as_str).collect());
        let meter = Arc::new(RecordingMeter::default());
        let meter_provider: Arc<dyn MeteringProvider> = meter.clone();
        let stack = BillingStack::with_meter_for_tests(meter_provider);
        let dead_letters = Arc::new(RecordingDeadLetters::default());
        let cfg = EventForwarderConfig::default();

        let first = run_cycle(&stream, &stack, dead_letters.as_ref(), &MockCreatorResolver::default(), &cfg)
            .await
            .expect("first cycle");
        let second = run_cycle(&stream, &stack, dead_letters.as_ref(), &MockCreatorResolver::default(), &cfg)
            .await
            .expect("redelivery cycle");

        assert_eq!(first.polled, 3);
        assert_eq!(first.ingested, 3);
        assert_eq!(first.committed, 3);
        assert_eq!(second.polled, 3);
        assert_eq!(second.ingested, 0);
        assert_eq!(second.deduped, Some(3));
        assert_eq!(
            meter
                .accepted_ids
                .lock()
                .expect("accepted ids poisoned")
                .as_slice(),
            ids.as_slice()
        );
        assert_eq!(
            stream
                .committed
                .lock()
                .expect("commits poisoned")
                .len(),
            6,
            "the fake redelivers the same records; each successful cycle commits them"
        );
        assert!(
            dead_letters
                .entries
                .lock()
                .expect("dead letters poisoned")
                .is_empty()
        );
    }

    #[compio::test]
    async fn permanent_provider_reject_dead_letters_batch_and_commits_offsets() {
        let stream = fake_stream(vec!["evt_bad"]);
        let meter = Arc::new(RecordingMeter {
            permanent_reject: true,
            ..RecordingMeter::default()
        });
        let meter_provider: Arc<dyn MeteringProvider> = meter;
        let stack = BillingStack::with_meter_for_tests(meter_provider);
        let dead_letters = Arc::new(RecordingDeadLetters::default());
        let cfg = EventForwarderConfig::default();

        let cycle = run_cycle(&stream, &stack, dead_letters.as_ref(), &MockCreatorResolver::default(), &cfg)
            .await
            .expect("permanent rejects are quarantined");

        assert_eq!(cycle.polled, 1);
        assert_eq!(cycle.dead_lettered, 1);
        assert_eq!(cycle.committed, 1);
        let entries = dead_letters.entries.lock().expect("dead letters poisoned");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event.event_id, "evt_bad");
        assert_eq!(entries[0].provider_id, "recording");
        assert!(entries[0].reason.contains("unknown subject"));
        assert_eq!(
            stream
                .committed
                .lock()
                .expect("commits poisoned")
                .as_slice(),
            &[StreamOffset {
                partition: 0,
                offset: 0,
            }]
        );
    }

    #[compio::test]
    async fn decode_error_dead_letters_record_and_commits_offsets_without_wedging() {
        let good_a = usage_event("evt_good_a");
        let good_b = usage_event("evt_good_b");
        let records = vec![
            StreamRecord {
                partition: 0,
                offset: 0,
                key: good_a.creator_subject().into_bytes(),
                payload: serde_json::to_vec(&good_a).expect("event serializes"),
            },
            StreamRecord {
                partition: 0,
                offset: 1,
                key: b"poison".to_vec(),
                payload: b"{not-json".to_vec(),
            },
            StreamRecord {
                partition: 0,
                offset: 2,
                key: good_b.creator_subject().into_bytes(),
                payload: serde_json::to_vec(&good_b).expect("event serializes"),
            },
        ];
        let stream = FakeStream {
            records: Mutex::new(records),
            committed: Mutex::new(Vec::new()),
        };
        let meter = Arc::new(RecordingMeter::default());
        let meter_provider: Arc<dyn MeteringProvider> = meter.clone();
        let stack = BillingStack::with_meter_for_tests(meter_provider);
        let dead_letters = Arc::new(RecordingDeadLetters::default());
        let cfg = EventForwarderConfig::default();

        let cycle = run_cycle(&stream, &stack, dead_letters.as_ref(), &MockCreatorResolver::default(), &cfg)
            .await
            .expect("decode poison is quarantined");

        assert_eq!(cycle.polled, 3);
        assert_eq!(cycle.ingested, 2);
        assert_eq!(cycle.dead_lettered, 1);
        assert_eq!(cycle.committed, 3);
        assert_eq!(
            meter
                .accepted_ids
                .lock()
                .expect("accepted ids poisoned")
                .as_slice(),
            &["evt_good_a".to_string(), "evt_good_b".to_string()]
        );
        let decode_entries = dead_letters
            .decode_entries
            .lock()
            .expect("decode dead letters poisoned");
        assert_eq!(decode_entries.len(), 1);
        assert_eq!(decode_entries[0].partition, 0);
        assert_eq!(decode_entries[0].offset, 1);
        assert_eq!(decode_entries[0].payload, b"{not-json".to_vec());
        assert_eq!(
            stream
                .committed
                .lock()
                .expect("commits poisoned")
                .as_slice(),
            &[
                StreamOffset {
                    partition: 0,
                    offset: 0,
                },
                StreamOffset {
                    partition: 0,
                    offset: 1,
                },
                StreamOffset {
                    partition: 0,
                    offset: 2,
                }
            ]
        );
    }

    fn fake_stream(ids: Vec<&str>) -> FakeStream {
        let records = ids
            .into_iter()
            .enumerate()
            .map(|(offset, id)| {
                let event = usage_event(id);
                StreamRecord {
                    partition: 0,
                    offset: offset as i64,
                    key: event.creator_subject().into_bytes(),
                    payload: serde_json::to_vec(&event).expect("event serializes"),
                }
            })
            .collect();
        FakeStream {
            records: Mutex::new(records),
            committed: Mutex::new(Vec::new()),
        }
    }

    fn usage_event(event_id: &str) -> UsageEvent {
        UsageEvent {
            event_id: event_id.to_string(),
            source: "worker-a".to_string(),
            subject: UsageSubject {
                app: Some(Uuid::parse_str("aaaaaaaa-aaaa-7aaa-aaaa-aaaaaaaaaaaa").unwrap()),
                creator: Uuid::parse_str("bbbbbbbb-bbbb-7bbb-bbbb-bbbbbbbbbbbb").unwrap(),
            },
            meter: "compute_units".to_string(),
            value: 10,
            event_time: 1_783_468_800,
            dims: BTreeMap::new(),
        }
    }

    fn nil_creator_event(event_id: &str, app: Uuid) -> UsageEvent {
        UsageEvent {
            event_id: event_id.to_string(),
            source: "worker-a".to_string(),
            subject: UsageSubject {
                app: Some(app),
                creator: Uuid::nil(), // the worker stamps nil; control must enrich
            },
            meter: "requests".to_string(),
            value: 10,
            event_time: 1_783_468_800,
            dims: BTreeMap::new(),
        }
    }

    fn fake_stream_of(events: Vec<UsageEvent>) -> FakeStream {
        let records = events
            .into_iter()
            .enumerate()
            .map(|(offset, event)| StreamRecord {
                partition: 0,
                offset: offset as i64,
                key: event.event_id.clone().into_bytes(),
                payload: serde_json::to_vec(&event).expect("event serializes"),
            })
            .collect();
        FakeStream {
            records: Mutex::new(records),
            committed: Mutex::new(Vec::new()),
        }
    }

    #[compio::test]
    async fn run_cycle_enriches_nil_creator_from_app_owner_and_deadletters_orphans() {
        let app_owned = Uuid::parse_str("aaaaaaaa-aaaa-7aaa-aaaa-aaaaaaaaaaaa").unwrap();
        let app_orphan = Uuid::parse_str("cccccccc-cccc-7ccc-cccc-cccccccccccc").unwrap();
        let owner = Uuid::parse_str("dddddddd-dddd-7ddd-dddd-dddddddddddd").unwrap();
        let stream = fake_stream_of(vec![
            nil_creator_event("evt_owned", app_owned),
            nil_creator_event("evt_orphan", app_orphan),
        ]);
        let meter = Arc::new(RecordingMeter::default());
        let meter_provider: Arc<dyn MeteringProvider> = meter.clone();
        let stack = BillingStack::with_meter_for_tests(meter_provider);
        let dead_letters = Arc::new(RecordingDeadLetters::default());
        let mut mapping = std::collections::HashMap::new();
        mapping.insert(app_owned, owner); // app_orphan has NO owner
        let resolver = MockCreatorResolver { mapping };
        let cfg = EventForwarderConfig::default();

        let cycle = run_cycle(&stream, &stack, dead_letters.as_ref(), &resolver, &cfg)
            .await
            .expect("cycle");

        // The owned app's event is forwarded WITH the resolved creator; the
        // orphan (no owner) is quarantined, never forwarded with a nil customer.
        assert_eq!(cycle.ingested, 1);
        assert_eq!(cycle.dead_lettered, 1);
        let ingested = meter.ingested_events.lock().expect("ingested poisoned");
        assert_eq!(ingested.len(), 1);
        assert_eq!(ingested[0].event_id, "evt_owned");
        assert_eq!(
            ingested[0].subject.creator, owner,
            "nil creator must be enriched to the app's owning creator before forwarding"
        );
        let dl = dead_letters.entries.lock().expect("dl poisoned");
        assert_eq!(dl.len(), 1);
        assert_eq!(dl[0].event.event_id, "evt_orphan");
        assert!(dl[0].reason.contains("no owning creator"));
    }
}
