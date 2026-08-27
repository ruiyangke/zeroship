//! Operator-owned abandoned logical replication slot cleanup.
//!
//! PostgreSQL 16 has no timestamp for when a slot became inactive. This
//! reaper therefore requires two independent facts before it drops a slot:
//!
//! 1. the slot is inactive for a full hour of continuous local observation;
//! 2. the worker process named by the slot no longer holds its session lease.
//!
//! The worker lease is two PostgreSQL session advisory locks derived from the
//! worker token. It survives idle traffic and replication reconnect backoff,
//! and PostgreSQL releases it automatically when a crashed worker's database
//! session disappears. Reapers take the same pair non-blockingly before they
//! age or drop a dead worker's slots. A separate fleet-wide session lock
//! elects one observer, so concurrent workers neither multiply full-catalog
//! scans nor reset each other's in-memory inactivity clocks. The final slot
//! drop is non-forcing and treats concurrent attach and drop races as benign.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};
use sha2::{Digest, Sha256};

use crate::error::{prefix_message, DbError};

/// A slot must remain inactive and have no live worker lease for this long.
pub const ABANDONED_INACTIVITY_THRESHOLD: Duration = Duration::from_secs(60 * 60);

/// Production workers inspect managed slots once per minute.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

const MANAGED_SLOT_PREFIX: &str = "__zs_slot_";

// The two-int advisory-lock namespace is disjoint from the one-bigint space
// used for worker leases. These constants spell "zsrs" / "v1" in hex.
const FLEET_REAPER_LOCK_NAMESPACE: i32 = 0x7a73_7273;
const FLEET_REAPER_LOCK_KEY: i32 = 0x7631_0001;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SlotFingerprint {
    restart_lsn: Option<String>,
    confirmed_flush_lsn: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SlotSnapshot {
    slot_name: String,
    worker_token: String,
    active: bool,
    fingerprint: SlotFingerprint,
}

#[derive(Clone, Debug)]
struct InactiveObservation {
    first_seen: Instant,
    fingerprint: SlotFingerprint,
}

#[derive(Debug, Default)]
struct InactivityTracker {
    observations: HashMap<String, InactiveObservation>,
}

impl InactivityTracker {
    fn retain_present(&mut self, present: &HashSet<String>) {
        self.observations
            .retain(|slot_name, _| present.contains(slot_name));
    }

    fn clear(&mut self, slot_name: &str) {
        self.observations.remove(slot_name);
    }

    fn observe(
        &mut self,
        slot: &SlotSnapshot,
        worker_unowned: bool,
        now: Instant,
        threshold: Duration,
    ) -> bool {
        if slot.active || !worker_unowned {
            self.clear(&slot.slot_name);
            return false;
        }

        match self.observations.get_mut(&slot.slot_name) {
            Some(observation) if observation.fingerprint == slot.fingerprint => now
                .checked_duration_since(observation.first_seen)
                .is_some_and(|elapsed| elapsed >= threshold),
            Some(observation) => {
                observation.first_seen = now;
                observation.fingerprint.clone_from(&slot.fingerprint);
                false
            }
            None => {
                self.observations.insert(
                    slot.slot_name.clone(),
                    InactiveObservation {
                        first_seen: now,
                        fingerprint: slot.fingerprint.clone(),
                    },
                );
                false
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkerLeaseKeys {
    first: i64,
    second: i64,
}

fn worker_lease_keys(worker_token: &str) -> WorkerLeaseKeys {
    let mut hasher = Sha256::new();
    hasher.update(b"zeroship:cdc-worker-lease:v1\0");
    hasher.update(worker_token.as_bytes());
    let digest = hasher.finalize();
    WorkerLeaseKeys {
        first: i64::from_be_bytes(digest[0..8].try_into().expect("eight digest bytes")),
        second: i64::from_be_bytes(digest[8..16].try_into().expect("eight digest bytes")),
    }
}

fn managed_worker_token(slot_name: &str) -> Option<&str> {
    let remainder = slot_name.strip_prefix(MANAGED_SLOT_PREFIX)?;
    let (app_token, worker_token) = remainder.split_once("__")?;
    let valid_hex = |value: &str, len: usize| {
        value.len() == len
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if valid_hex(app_token, 28) && valid_hex(worker_token, 20) {
        Some(worker_token)
    } else {
        None
    }
}

fn pg_error(context: &str, error: &compio_postgres::Error) -> DbError {
    let mut error = DbError::from_pg(error);
    prefix_message(&mut error, context);
    error
}

async fn query_try_lock(client: &Client, key: i64) -> Result<bool, DbError> {
    let row = client
        .query_one("SELECT pg_try_advisory_lock($1) AS acquired", &[&key])
        .await
        .map_err(|error| pg_error("slot reaper: acquire worker lease: ", &error))?;
    Ok(row.get("acquired"))
}

async fn query_unlock(client: &Client, key: i64) -> Result<(), DbError> {
    let row = client
        .query_one("SELECT pg_advisory_unlock($1) AS released", &[&key])
        .await
        .map_err(|error| pg_error("slot reaper: release worker lease: ", &error))?;
    let released: bool = row.get("released");
    if !released {
        return Err(DbError::Internal {
            message: "slot reaper: advisory worker lease was not held by this session".to_string(),
        });
    }
    Ok(())
}

async fn try_become_fleet_leader(client: &Client) -> Result<bool, DbError> {
    let row = client
        .query_one(
            "SELECT pg_try_advisory_lock($1::INT4, $2::INT4) AS acquired",
            &[&FLEET_REAPER_LOCK_NAMESPACE, &FLEET_REAPER_LOCK_KEY],
        )
        .await
        .map_err(|error| pg_error("slot reaper: acquire fleet leader lock: ", &error))?;
    Ok(row.get("acquired"))
}

async fn try_acquire_worker_lease(client: &Client, keys: WorkerLeaseKeys) -> Result<bool, DbError> {
    if !query_try_lock(client, keys.first).await? {
        return Ok(false);
    }

    match query_try_lock(client, keys.second).await {
        Ok(true) => Ok(true),
        Ok(false) => {
            query_unlock(client, keys.first).await?;
            Ok(false)
        }
        Err(error) => {
            let _ = query_unlock(client, keys.first).await;
            Err(error)
        }
    }
}

async fn release_worker_lease(client: &Client, keys: WorkerLeaseKeys) -> Result<(), DbError> {
    let second = query_unlock(client, keys.second).await;
    let first = query_unlock(client, keys.first).await;
    second.and(first)
}

async fn enumerate_managed_slots(client: &Client) -> Result<Vec<SlotSnapshot>, DbError> {
    let rows = client
        .query(
            "SELECT slot_name, active, restart_lsn::text, confirmed_flush_lsn::text
               FROM pg_replication_slots
              WHERE slot_type = 'logical'
                AND plugin = 'pgoutput'
                AND database = current_database()
                AND temporary = false
                AND left(slot_name, 10) = '__zs_slot_'
              ORDER BY slot_name",
            &[],
        )
        .await
        .map_err(|error| pg_error("slot reaper: enumerate managed slots: ", &error))?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let slot_name: String = row.get("slot_name");
            let worker_token = managed_worker_token(&slot_name)?.to_string();
            Some(SlotSnapshot {
                slot_name,
                worker_token,
                active: row.get("active"),
                fingerprint: SlotFingerprint {
                    restart_lsn: row.get("restart_lsn"),
                    confirmed_flush_lsn: row.get("confirmed_flush_lsn"),
                },
            })
        })
        .collect())
}

async fn drop_inactive_managed_slot(client: &Client, slot_name: &str) -> Result<bool, DbError> {
    debug_assert!(managed_worker_token(slot_name).is_some());
    match client
        .query(
            "SELECT pg_drop_replication_slot($1)
              WHERE EXISTS (
                    SELECT 1
                      FROM pg_replication_slots
                     WHERE slot_name = $1
                       AND slot_type = 'logical'
                       AND plugin = 'pgoutput'
                       AND database = current_database()
                       AND temporary = false
                       AND active = false
              )",
            &[&slot_name],
        )
        .await
    {
        Ok(rows) => Ok(!rows.is_empty()),
        Err(error) if is_benign_drop_sqlstate(error.code()) => Ok(false),
        Err(error) => Err(pg_error(
            &format!("slot reaper: drop managed slot {slot_name}: "),
            &error,
        )),
    }
}

fn is_benign_drop_sqlstate(code: Option<&SqlState>) -> bool {
    matches!(
        code,
        Some(code)
            if code == &SqlState::OBJECT_IN_USE || code == &SqlState::UNDEFINED_OBJECT
    )
}

/// Result of one operator sweep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// This process owns the fleet-wide observer lock for this sweep.
    pub is_leader: bool,
    /// Exact current-format managed slots examined by this sweep.
    pub inspected: usize,
    /// Slots dropped after the full inactivity and ownership checks.
    pub dropped: Vec<String>,
}

/// One process-wide worker lease and abandoned-slot observation state.
pub struct OperatorSlotReaper {
    client: Client,
    own_worker_token: String,
    is_leader: bool,
    tracker: InactivityTracker,
    threshold: Duration,
}

impl std::fmt::Debug for OperatorSlotReaper {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperatorSlotReaper")
            .field("own_worker_token", &self.own_worker_token)
            .field("is_leader", &self.is_leader)
            .field("tracked_slots", &self.tracker.observations.len())
            .field("threshold", &self.threshold)
            .finish_non_exhaustive()
    }
}

impl OperatorSlotReaper {
    /// Connect a dedicated maintenance session and acquire this worker's
    /// crash-released process lease before the worker accepts traffic.
    pub async fn connect(db_url: &str, worker_id: &str) -> Result<Self, DbError> {
        Self::connect_with_threshold(db_url, worker_id, ABANDONED_INACTIVITY_THRESHOLD).await
    }

    async fn connect_with_threshold(
        db_url: &str,
        worker_id: &str,
        threshold: Duration,
    ) -> Result<Self, DbError> {
        let own_worker_token = crate::replication::worker_token(worker_id)?;
        let (client, connection) = compio_postgres::connect(db_url, NoTls)
            .await
            .map_err(|error| pg_error("slot reaper: connect maintenance session: ", &error))?;
        compio::runtime::spawn(async move {
            if let Err(error) = connection.run().await {
                tracing::warn!(%error, "slot reaper maintenance connection ended");
            }
        })
        .detach();

        let keys = worker_lease_keys(&own_worker_token);
        if !try_acquire_worker_lease(&client, keys).await? {
            return Err(DbError::Configuration {
                code: "cdc_worker_lease_conflict",
                message: "slot reaper: another process already owns this worker identity"
                    .to_string(),
                hint: Some(
                    "start each worker process with a unique CDC worker identity".to_string(),
                ),
            });
        }

        Ok(Self {
            client,
            own_worker_token,
            is_leader: false,
            tracker: InactivityTracker::default(),
            threshold,
        })
    }

    /// Inspect and conditionally reap managed slots using monotonic time.
    pub async fn sweep(&mut self) -> Result<SweepReport, DbError> {
        self.sweep_at(Instant::now()).await
    }

    async fn sweep_at(&mut self, now: Instant) -> Result<SweepReport, DbError> {
        if !self.is_leader {
            self.is_leader = try_become_fleet_leader(&self.client).await?;
            if !self.is_leader {
                return Ok(SweepReport::default());
            }
        }

        let snapshots = enumerate_managed_slots(&self.client).await?;
        let present = snapshots
            .iter()
            .map(|slot| slot.slot_name.clone())
            .collect::<HashSet<_>>();
        self.tracker.retain_present(&present);

        let mut by_worker = HashMap::<String, Vec<SlotSnapshot>>::new();
        for slot in snapshots {
            by_worker
                .entry(slot.worker_token.clone())
                .or_default()
                .push(slot);
        }

        let inspected = by_worker.values().map(Vec::len).sum();
        let mut report = SweepReport {
            is_leader: true,
            inspected,
            dropped: Vec::new(),
        };

        for (worker_token, slots) in by_worker {
            let worker_is_live =
                worker_token == self.own_worker_token || slots.iter().any(|slot| slot.active);
            if worker_is_live {
                for slot in &slots {
                    self.tracker.clear(&slot.slot_name);
                }
                continue;
            }

            let keys = worker_lease_keys(&worker_token);
            if !try_acquire_worker_lease(&self.client, keys).await? {
                for slot in &slots {
                    self.tracker.clear(&slot.slot_name);
                }
                continue;
            }

            let mut group_error = None;
            for slot in &slots {
                if self.tracker.observe(slot, true, now, self.threshold) {
                    match drop_inactive_managed_slot(&self.client, &slot.slot_name).await {
                        Ok(true) => report.dropped.push(slot.slot_name.clone()),
                        Ok(false) => {}
                        Err(error) => {
                            group_error = Some(error);
                            break;
                        }
                    }
                    self.tracker.clear(&slot.slot_name);
                }
            }

            let release = release_worker_lease(&self.client, keys).await;
            if let Some(error) = group_error {
                let _ = release;
                return Err(error);
            }
            release?;
        }

        Ok(report)
    }

    /// Test-only constructor with an injected elapsed-time policy.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub async fn connect_for_tests(
        db_url: &str,
        worker_id: &str,
        threshold: Duration,
    ) -> Result<Self, DbError> {
        Self::connect_with_threshold(db_url, worker_id, threshold).await
    }

    /// Test-only sweep at an injected monotonic instant.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub async fn sweep_at_for_tests(&mut self, now: Instant) -> Result<SweepReport, DbError> {
        self.sweep_at(now).await
    }

    /// Number of slots currently aging toward the inactivity threshold.
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn tracked_slots_for_tests(&self) -> usize {
        self.tracker.observations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inactive(slot_name: &str, worker_token: &str, lsn: &str) -> SlotSnapshot {
        SlotSnapshot {
            slot_name: slot_name.to_string(),
            worker_token: worker_token.to_string(),
            active: false,
            fingerprint: SlotFingerprint {
                restart_lsn: Some(lsn.to_string()),
                confirmed_flush_lsn: Some(lsn.to_string()),
            },
        }
    }

    #[test]
    fn managed_slot_parser_accepts_only_the_current_exact_format() {
        let names = [
            "__zs_slot_0123456789abcdef0123456789ab__0123456789abcdef0123",
            "__zs_slot_0123456789abcdef0123456789ab__not-hex",
            "__zs_shared_0123456789abcdef0123456789ab__0123456789abcdef0123",
            "__zs_slot_0123456789ABCDEF0123456789AB__0123456789abcdef0123",
        ];
        assert!(
            !names.is_empty(),
            "test must examine at least one slot name"
        );
        assert_eq!(managed_worker_token(names[0]), Some("0123456789abcdef0123"));
        assert_eq!(managed_worker_token(names[1]), None);
        assert_eq!(managed_worker_token(names[2]), None);
        assert_eq!(managed_worker_token(names[3]), None);
    }

    #[test]
    fn stable_inactive_slot_becomes_due_only_at_the_elapsed_threshold() {
        let slots = [inactive(
            "__zs_slot_0123456789abcdef0123456789ab__0123456789abcdef0123",
            "0123456789abcdef0123",
            "0/100",
        )];
        assert!(!slots.is_empty(), "test must exercise an inactive slot");
        let slot = &slots[0];
        let start = Instant::now();
        let mut tracker = InactivityTracker::default();
        assert!(!tracker.observe(slot, true, start, ABANDONED_INACTIVITY_THRESHOLD));
        assert!(!tracker.observe(
            slot,
            true,
            start + ABANDONED_INACTIVITY_THRESHOLD - Duration::from_secs(1),
            ABANDONED_INACTIVITY_THRESHOLD,
        ));
        assert!(tracker.observe(
            slot,
            true,
            start + ABANDONED_INACTIVITY_THRESHOLD,
            ABANDONED_INACTIVITY_THRESHOLD,
        ));
    }

    #[test]
    fn active_or_worker_owned_slot_resets_inactivity() {
        let start = Instant::now();
        let mut slot = inactive(
            "__zs_slot_0123456789abcdef0123456789ab__0123456789abcdef0123",
            "0123456789abcdef0123",
            "0/100",
        );
        let fixtures = [slot.clone()];
        assert!(!fixtures.is_empty(), "test must exercise a slot");
        let mut tracker = InactivityTracker::default();
        assert!(!tracker.observe(&slot, true, start, Duration::from_secs(10)));
        slot.active = true;
        assert!(!tracker.observe(
            &slot,
            true,
            start + Duration::from_secs(20),
            Duration::from_secs(10),
        ));
        slot.active = false;
        assert!(!tracker.observe(
            &slot,
            false,
            start + Duration::from_secs(40),
            Duration::from_secs(10),
        ));
        assert!(tracker.observations.is_empty());
    }

    #[test]
    fn lsn_progress_and_disappearance_reset_observation() {
        let start = Instant::now();
        let mut slot = inactive(
            "__zs_slot_0123456789abcdef0123456789ab__0123456789abcdef0123",
            "0123456789abcdef0123",
            "0/100",
        );
        let fixtures = [slot.clone()];
        assert!(!fixtures.is_empty(), "test must exercise a slot");
        let mut tracker = InactivityTracker::default();
        assert!(!tracker.observe(&slot, true, start, Duration::from_secs(10)));
        slot.fingerprint.confirmed_flush_lsn = Some("0/200".to_string());
        assert!(!tracker.observe(
            &slot,
            true,
            start + Duration::from_secs(20),
            Duration::from_secs(10),
        ));

        tracker.retain_present(&HashSet::new());
        assert!(tracker.observations.is_empty());
    }

    #[test]
    fn worker_lease_uses_two_independent_stable_keys() {
        let first = worker_lease_keys("0123456789abcdef0123");
        let same = worker_lease_keys("0123456789abcdef0123");
        let other = worker_lease_keys("1123456789abcdef0123");
        assert_eq!(first, same);
        assert_ne!(first.first, first.second);
        assert_ne!(first, other);
    }

    #[test]
    fn concurrent_attach_and_drop_sqlstates_are_benign() {
        let codes = [SqlState::OBJECT_IN_USE, SqlState::UNDEFINED_OBJECT];
        assert!(!codes.is_empty(), "test must examine at least one SQLSTATE");
        for code in codes {
            assert!(
                is_benign_drop_sqlstate(Some(&code)),
                "race code was fatal: {code:?}"
            );
        }
        assert!(!is_benign_drop_sqlstate(Some(&SqlState::DISK_FULL)));
        assert!(!is_benign_drop_sqlstate(None));
    }
}
