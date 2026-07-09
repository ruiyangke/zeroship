//! Usage-event outbox for the worker producer.
//!
//! S4 publishes directly to the injected stream transport. A later slice will
//! measure and optionally add the bounded redb WAL floor for the pre-ack window.

use std::sync::Arc;
use std::time::Duration;

use zeroship_core::usage_event::UsageEvent;
use zeroship_stream::StreamTransport;

use crate::Meter;

pub const DEFAULT_OUTBOX_INTERVAL: Duration = Duration::from_secs(10);
pub const DEFAULT_USAGE_EVENTS_TOPIC: &str = "usage-events";

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
}

impl std::fmt::Debug for UsageOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageOutbox")
            .field("stream", &self.stream.id())
            .field("topic", &self.topic)
            .finish()
    }
}

impl UsageOutbox {
    #[must_use]
    pub fn new(stream: Arc<dyn StreamTransport>, topic: impl Into<String>) -> Self {
        Self {
            stream,
            topic: topic.into(),
        }
    }

    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Publish a drained window. Each `UsageEvent` is one stream record because
    /// the landed forwarder decodes each record payload as a single event.
    ///
    /// TODO(S5 redb floor): append each event to a bounded local redb segment
    /// before publish and trim it from the librdkafka delivery report callback.
    /// S4 intentionally publishes directly while the fsync cost is unmeasured.
    pub async fn publish_events(&self, events: &[UsageEvent]) -> OutboxPublishResult {
        let mut result = OutboxPublishResult {
            attempted: events.len(),
            ..OutboxPublishResult::default()
        };

        for event in events {
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
                    result.published += 1;
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
            let outbox = UsageOutbox::new(stream.clone(), "usage-events-test");
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

    fn assert_event(events: &[UsageEvent], app_id: Uuid, meter: &str, value: u64) {
        let event = events
            .iter()
            .find(|event| event.subject.app == Some(app_id) && event.meter == meter)
            .expect("usage event exists");
        assert_eq!(event.value, value);
    }
}
