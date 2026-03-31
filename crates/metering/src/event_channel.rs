//! Bounded event channel — connects the request path to the background event writer.
//!
//! The request handler enqueues events via EventSender (non-blocking, drops on full).
//! The background EventWriter drains the channel and appends to the EventLog.

use appbase_core::event_log::{EventLog, EventKind, MeterEvent};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc;

/// Capacity of the event channel (spec: peak_events/s * flush_ms / 1000 * 2).
const DEFAULT_CHANNEL_CAPACITY: usize = 30_000;

/// Sender half — cloneable, used by request handlers.
#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::Sender<MeterEvent>,
}

impl EventSender {
    /// Try to enqueue an event. Non-blocking. Returns false if channel is full (event dropped).
    pub fn try_send(&self, event: MeterEvent) -> bool {
        self.tx.try_send(event).is_ok()
    }

    /// Convenience: enqueue a RequestCompleted event.
    pub fn log_request(&self, app_id: &str, payload: serde_json::Value) {
        let event = MeterEvent {
            id: 0, // assigned by the EventLog on append
            timestamp: SystemTime::now(),
            app_id: app_id.to_string(),
            kind: EventKind::RequestCompleted,
            payload,
        };
        let _ = self.try_send(event);
    }

    /// Convenience: enqueue a quota/rate limit event.
    pub fn log_enforcement(&self, app_id: &str, kind: EventKind, payload: serde_json::Value) {
        let event = MeterEvent {
            id: 0,
            timestamp: SystemTime::now(),
            app_id: app_id.to_string(),
            kind,
            payload,
        };
        let _ = self.try_send(event);
    }
}

/// Spawn the background event writer. Returns the sender for request handlers.
pub fn spawn_event_writer(
    log: Arc<dyn EventLog>,
) -> (EventSender, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<MeterEvent>(DEFAULT_CHANNEL_CAPACITY);

    let handle = tokio::spawn(async move {
        // Batch drain: collect up to 100 events per iteration
        let mut batch = Vec::with_capacity(100);
        loop {
            // Wait for at least one event
            match rx.recv().await {
                Some(event) => batch.push(event),
                None => break, // channel closed
            }
            // Drain any additional buffered events (non-blocking)
            while batch.len() < 100 {
                match rx.try_recv() {
                    Ok(event) => batch.push(event),
                    Err(_) => break,
                }
            }
            // Append batch to event log
            for event in batch.drain(..) {
                if let Err(e) = log.append(event) {
                    eprintln!("[event_writer] Failed to append event: {e}");
                }
            }
        }
    });

    (EventSender { tx }, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_logger::InMemoryEventLog;

    #[tokio::test]
    async fn events_flow_through_channel() {
        let log = Arc::new(InMemoryEventLog::new());
        let (sender, handle) = spawn_event_writer(log.clone());

        sender.log_request("app1", serde_json::json!({"cpu_ms": 5}));
        sender.log_request("app1", serde_json::json!({"cpu_ms": 10}));

        // Give the writer time to drain
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = log.query(
            "app1",
            SystemTime::UNIX_EPOCH,
            SystemTime::now(),
            100,
        ).unwrap();
        assert_eq!(events.len(), 2);

        // Cleanup
        drop(sender);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn dropped_events_on_full_channel() {
        let _log = Arc::new(InMemoryEventLog::new());
        // Use a tiny channel
        let (tx, _rx) = mpsc::channel::<MeterEvent>(1);
        let sender = EventSender { tx };

        // Fill the channel
        let ok1 = sender.try_send(MeterEvent {
            id: 0, timestamp: SystemTime::now(),
            app_id: "a".into(), kind: EventKind::RequestCompleted,
            payload: serde_json::Value::Null,
        });
        // This should fail (channel full, receiver not draining)
        let ok2 = sender.try_send(MeterEvent {
            id: 0, timestamp: SystemTime::now(),
            app_id: "a".into(), kind: EventKind::RequestCompleted,
            payload: serde_json::Value::Null,
        });
        assert!(ok1);
        assert!(!ok2);
    }
}
