//! Durable-workflow scheduler tier.
//!
//! The scheduler owns only its private `workflow_scheduler` schema and the
//! timer authority built on top of it. The workflow journal and apply path stay
//! outside this crate.

use std::time::Duration;

use chrono::Utc;

pub mod store;
pub mod wheel;

pub use store::{
    FiredTimer, InflightTimer, TimerRow, WorkflowSchedulerStore, WorkflowSchedulerStoreError,
};
pub use wheel::{TimerEntry, TimerWheel, WakeHandle};

pub const DEFAULT_TICK_SECS: u64 = 1;
pub const WORKFLOW_ADVANCE_PATH: &str = "/__zeroship/internal/workflow-advance";

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub near_horizon_ms: i64,
    pub max_loaded_timers: i64,
    pub max_due_per_tick: usize,
    pub inflight_ttl_ms: i64,
    pub empty_sleep_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            near_horizon_ms: 60_000,
            max_loaded_timers: 1_024,
            max_due_per_tick: 64,
            inflight_ttl_ms: 120_000,
            empty_sleep_ms: 1_000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Store(#[from] WorkflowSchedulerStoreError),
}

/// Move due timer rows into `workflow_scheduler.inflight`.
///
/// This is intentionally small in C1: later workflow dispatch/apply layers build
/// on the returned inflight rows without changing the store contract.
#[allow(clippy::future_not_send)]
pub async fn fire_once(
    store: &WorkflowSchedulerStore,
    wheel: &mut TimerWheel,
    config: &SchedulerConfig,
) -> Result<Vec<FiredTimer>, SchedulerError> {
    let now = Utc::now();
    let horizon = now + chrono::Duration::milliseconds(config.near_horizon_ms);
    wheel
        .load_from_store(store, horizon, config.max_loaded_timers)
        .await?;

    let deadline = now + chrono::Duration::milliseconds(config.inflight_ttl_ms);
    let mut fired = Vec::new();
    while fired.len() < config.max_due_per_tick {
        let Some(entry) = wheel.pop_due(Utc::now()) else {
            break;
        };
        if let Some(timer) = store.move_timer_to_inflight(&entry, deadline).await? {
            fired.push(timer);
        }
    }
    Ok(fired)
}

/// Run the timer wheel forever.
#[allow(clippy::future_not_send)]
pub async fn run(store: WorkflowSchedulerStore, config: SchedulerConfig) -> Result<(), SchedulerError> {
    store.provision().await?;
    let wake = WakeHandle::new();
    let mut wheel = TimerWheel::new(wake.clone());
    loop {
        let fired = fire_once(&store, &mut wheel, &config).await?;
        if !fired.is_empty() {
            continue;
        }

        let sleep_for = wheel
            .duration_until_next(Utc::now())
            .unwrap_or_else(|| Duration::from_millis(config.empty_sleep_ms));
        wheel.wait_for_wake_or_timeout(sleep_for).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use uuid::Uuid;

    fn test_db_url() -> Option<String> {
        std::env::var("ZEROSHIP_SCHEDULER_TEST_DB")
            .ok()
            .or_else(|| std::env::var("CONTROL_TEST_DB").ok())
    }

    async fn store(label: &str) -> Option<WorkflowSchedulerStore> {
        let Some(db_url) = test_db_url() else {
            eprintln!("skip: ZEROSHIP_SCHEDULER_TEST_DB/CONTROL_TEST_DB not set");
            return None;
        };
        let store = WorkflowSchedulerStore::new(db_url);
        store.provision().await.expect("provision scheduler store");
        store
            .clear_for_tests()
            .await
            .unwrap_or_else(|err| panic!("clear scheduler store for {label}: {err}"));
        Some(store)
    }

    #[compio::test]
    #[serial]
    async fn store_register_fire_and_ack_next_timer() {
        let Some(store) = store("register-fire-ack").await else {
            return;
        };
        let run_id = format!("run_{}", Uuid::new_v4().simple());
        let app_id = Uuid::new_v4();
        let wake_at = Utc::now() - chrono::Duration::milliseconds(10);

        let registered = store
            .register_timer(&run_id, app_id, wake_at)
            .await
            .expect("register timer");
        assert_eq!(registered.generation, 0);

        let mut wheel = TimerWheel::new(WakeHandle::new());
        let fired = fire_once(&store, &mut wheel, &SchedulerConfig::default())
            .await
            .expect("fire once");
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].run_id, run_id);
        assert_eq!(store.timer(&run_id).await.expect("load timer"), None);
        assert!(store.inflight(&run_id).await.expect("load inflight").is_some());

        let next_wake = Utc::now() + chrono::Duration::milliseconds(100);
        let next = store
            .ack_register_next(&run_id, app_id, next_wake)
            .await
            .expect("ack next");
        assert_eq!(next.generation, fired[0].dispatch_generation + 1);
        assert_eq!(store.inflight(&run_id).await.expect("load inflight"), None);
    }

    #[compio::test]
    #[serial]
    async fn store_reconcile_moves_inflight_back_to_timer() {
        let Some(store) = store("reconcile-move").await else {
            return;
        };
        let run_id = format!("run_{}", Uuid::new_v4().simple());
        let app_id = Uuid::new_v4();
        let wake_at = Utc::now() - chrono::Duration::milliseconds(10);
        let deadline = Utc::now() + chrono::Duration::milliseconds(1_000);

        let row = store
            .register_timer(&run_id, app_id, wake_at)
            .await
            .expect("register timer");
        let entry = TimerEntry::from(row);
        let fired = store
            .move_timer_to_inflight(&entry, deadline)
            .await
            .expect("move to inflight")
            .expect("timer fired");
        assert_eq!(fired.dispatch_generation, 0);

        let moved = store
            .reconcile_inflight_to_timer(&run_id, wake_at)
            .await
            .expect("reconcile move")
            .expect("timer restored");
        assert_eq!(moved.generation, 1);
        assert!(store.inflight(&run_id).await.expect("load inflight").is_none());
    }

    #[compio::test]
    async fn wheel_orders_by_wake_run_and_generation() {
        let wake = WakeHandle::new();
        let mut wheel = TimerWheel::new(wake);
        let app_id = Uuid::new_v4();
        let base = Utc::now();
        wheel.push(TimerEntry {
            run_id: "run_c".to_string(),
            app_id,
            wake_at: base + chrono::Duration::milliseconds(20),
            generation: 0,
        });
        wheel.push(TimerEntry {
            run_id: "run_b".to_string(),
            app_id,
            wake_at: base,
            generation: 1,
        });
        wheel.push(TimerEntry {
            run_id: "run_a".to_string(),
            app_id,
            wake_at: base,
            generation: 2,
        });

        assert_eq!(wheel.pop_due(base).expect("run a").run_id, "run_a");
        assert_eq!(wheel.pop_due(base).expect("run b").run_id, "run_b");
        assert!(wheel.pop_due(base).is_none());
        assert_eq!(
            wheel
                .pop_due(base + chrono::Duration::milliseconds(20))
                .expect("run c")
                .run_id,
            "run_c"
        );
    }
}
