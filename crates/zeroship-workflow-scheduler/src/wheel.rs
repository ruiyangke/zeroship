use zeroship_core::app_id::AppId;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use flume::{Receiver, Sender};
use crate::store::{TimerRow, WorkflowSchedulerStore, WorkflowSchedulerStoreError};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TimerEntry {
    pub run_id: String,
    pub app_id: AppId,
    pub wake_at: DateTime<Utc>,
    pub generation: i64,
}

impl From<TimerRow> for TimerEntry {
    fn from(row: TimerRow) -> Self {
        Self {
            run_id: row.run_id,
            app_id: row.app_id,
            wake_at: row.wake_at,
            generation: row.generation,
        }
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .wake_at
            .cmp(&self.wake_at)
            .then_with(|| other.run_id.cmp(&self.run_id))
            .then_with(|| other.generation.cmp(&self.generation))
    }
}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
pub struct WakeHandle {
    tx: Sender<()>,
    rx: Receiver<()>,
}

impl WakeHandle {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = flume::bounded(1);
        Self { tx, rx }
    }

    pub fn wake(&self) {
        let _ = self.tx.try_send(());
    }

    async fn wait_for_timeout(&self, duration: Duration) {
        match compio::time::timeout(duration, self.rx.recv_async()).await {
            Ok(Ok(())) | Ok(Err(_)) | Err(_) => {}
        }
        while self.rx.try_recv().is_ok() {}
    }
}

impl Default for WakeHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct TimerWheel {
    heap: BinaryHeap<TimerEntry>,
    wake: WakeHandle,
}

impl TimerWheel {
    #[must_use]
    pub fn new(wake: WakeHandle) -> Self {
        Self {
            heap: BinaryHeap::new(),
            wake,
        }
    }

    pub fn push(&mut self, entry: TimerEntry) {
        let earlier = self.heap.peek().is_none_or(|current| {
            entry.wake_at < current.wake_at
                || (entry.wake_at == current.wake_at && entry.run_id < current.run_id)
        });
        self.heap.push(entry);
        if earlier {
            self.wake.wake();
        }
    }

    #[allow(clippy::future_not_send)]
    pub async fn load_from_store(
        &mut self,
        store: &WorkflowSchedulerStore,
        horizon: DateTime<Utc>,
        limit: i64,
    ) -> Result<usize, WorkflowSchedulerStoreError> {
        let rows = store.load_window(horizon, limit).await?;
        let count = rows.len();
        for row in rows {
            self.push(row.into());
        }
        Ok(count)
    }

    pub fn pop_due(&mut self, now: DateTime<Utc>) -> Option<TimerEntry> {
        if self.heap.peek().is_some_and(|entry| entry.wake_at <= now) {
            return self.heap.pop();
        }
        None
    }

    #[must_use]
    pub fn duration_until_next(&self, now: DateTime<Utc>) -> Option<Duration> {
        let next = self.heap.peek()?;
        if next.wake_at <= now {
            return Some(Duration::from_millis(0));
        }
        let millis = (next.wake_at - now).num_milliseconds().max(0);
        Some(Duration::from_millis(millis as u64))
    }

    pub async fn wait_for_wake_or_timeout(&self, duration: Duration) {
        self.wake.wait_for_timeout(duration).await;
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}
