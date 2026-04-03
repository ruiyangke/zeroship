//! In-memory event log implementation + background event logger.
//!
//! The in-memory implementation stores events in a Vec behind a Mutex.
//! For production, replace with an append-only file or database-backed impl.

use crate::core::event_log::{EventLog, EventLogError, MeterEvent};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

/// In-memory event log — for development and testing.
pub struct InMemoryEventLog {
    events: Mutex<Vec<MeterEvent>>,
    next_id: AtomicU64,
}

impl InMemoryEventLog {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }
}

impl EventLog for InMemoryEventLog {
    fn append(&self, mut event: MeterEvent) -> Result<u64, EventLogError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        event.id = id;
        if event.timestamp == SystemTime::UNIX_EPOCH {
            event.timestamp = SystemTime::now();
        }
        self.events.lock().unwrap().push(event);
        Ok(id)
    }

    fn query(
        &self,
        app_id: &str,
        since: SystemTime,
        until: SystemTime,
        limit: usize,
    ) -> Result<Vec<MeterEvent>, EventLogError> {
        let events = self.events.lock().unwrap();
        let filtered: Vec<MeterEvent> = events
            .iter()
            .filter(|e| {
                e.app_id == app_id && e.timestamp >= since && e.timestamp <= until
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(filtered)
    }

    fn latest_id(&self) -> Result<u64, EventLogError> {
        Ok(self.next_id.load(Ordering::Relaxed).saturating_sub(1))
    }

    fn close(&self) -> Result<(), EventLogError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event_log::EventKind;

    fn make_event(app_id: &str, kind: EventKind) -> MeterEvent {
        MeterEvent {
            id: 0,
            timestamp: SystemTime::UNIX_EPOCH,
            app_id: app_id.to_string(),
            kind,
            payload: serde_json::Value::Null,
        }
    }

    #[test]
    fn append_and_query() {
        let log = InMemoryEventLog::new();
        let id = log
            .append(make_event("app1", EventKind::RequestCompleted))
            .unwrap();
        assert_eq!(id, 1);

        let events = log
            .query("app1", SystemTime::UNIX_EPOCH, SystemTime::now(), 100)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].app_id, "app1");
    }

    #[test]
    fn latest_id_tracks_appends() {
        let log = InMemoryEventLog::new();
        assert_eq!(log.latest_id().unwrap(), 0);
        log.append(make_event("a", EventKind::QuotaDenied))
            .unwrap();
        assert_eq!(log.latest_id().unwrap(), 1);
        log.append(make_event("b", EventKind::RateLimited))
            .unwrap();
        assert_eq!(log.latest_id().unwrap(), 2);
    }

    #[test]
    fn query_filters_by_app() {
        let log = InMemoryEventLog::new();
        log.append(make_event("app1", EventKind::RequestCompleted))
            .unwrap();
        log.append(make_event("app2", EventKind::RequestCompleted))
            .unwrap();

        let events = log
            .query("app1", SystemTime::UNIX_EPOCH, SystemTime::now(), 100)
            .unwrap();
        assert_eq!(events.len(), 1);
    }
}
