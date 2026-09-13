//! Bounded advisory-lock acquisition over `LockManager`.
//!
//! The extension trait applies retry timing and cancellation through compio while
//! the storage contract supplies backend lock operations.
//!
//! ```ignore
//! use zeroship_data_orm::lock_policy::BoundedLockAcquire;
//! backend.acquire(&client, &scope).await?;
//! ```

use crate::error::DbError;

use crate::capability::LockScope;
use crate::storage::LockManager;

/// The retry schedule as `(attempt_index, pre_wait_ms)`.
///
/// Five attempts, cumulative budget `0+50+200+500+1000 = 1750ms`. The tuple
/// carries the attempt index so the loop body can log it without re-deriving
/// the budget.
///
/// **Why a fixed schedule and not exponential growth**: the contended window is
/// operator-set (a held migration or snapshot lock), so what is wanted is a
/// hard upper bound. Five round-trips is measured against the longest
/// legitimate hold observed - about 1s for a slow `CREATE INDEX CONCURRENTLY`
/// pre-pivot - and anything past 1.75s should surface as contention so the SDK
/// can retry or circuit-break rather than wait.
const SCHEDULE: &[(u32, u64)] = &[(1, 0), (2, 50), (3, 200), (4, 500), (5, 1000)];

/// Bounded acquisition over any [`LockManager`].
///
/// Blanket-implemented, never implemented by hand: backends supply the
/// non-blocking primitive [`LockManager::try_acquire_advisory_lock`] and this
/// trait supplies the only retry policy the tree has.
pub trait BoundedLockAcquire: LockManager {
    /// Acquire a session-scoped advisory lock for `scope`, bounded by a retry
    /// loop over `try_acquire_advisory_lock` - never the indefinitely-waiting
    /// `acquire_advisory_lock`.
    ///
    /// **Security \[I43\]**: the shape this replaced called
    /// `acquire_advisory_lock`, which on PostgreSQL issues `pg_advisory_lock` -
    /// a server-side wait with no timeout. An app holding its own session lock
    /// indefinitely could stall every subsequent operation using that scope
    /// until the holding session terminated. The bound caps the wait and
    /// surfaces [`DbError::LockContention`] (wire code `lock_not_available`) so
    /// the caller decides how to react.
    ///
    /// **Cancel-safety**: every await inside the loop is safe to cancel.
    /// `try_acquire_advisory_lock` mutates server-side state only when it
    /// returns `Ok(true)`, and the inter-attempt sleep is cancellable, so
    /// dropping the future mid-await leaks no state on either side.
    #[allow(async_fn_in_trait)]
    async fn acquire(&self, client: &Self::Client, scope: &LockScope) -> Result<(), DbError> {
        self.try_acquire_with_backoff(client, scope).await
    }

    /// The retry loop itself. Loops on
    /// [`LockManager::try_acquire_advisory_lock`] over `SCHEDULE`, returning
    /// [`DbError::LockContention`] on exhaustion.
    ///
    /// A SQL-level failure - connection drop, server error - is surfaced
    /// immediately rather than retried: the caller sees the typed `DbError`
    /// exactly as the primitive produced it.
    #[allow(async_fn_in_trait)]
    async fn try_acquire_with_backoff(
        &self,
        client: &Self::Client,
        scope: &LockScope,
    ) -> Result<(), DbError> {
        let (k1, k2) = scope.to_keys();
        for (attempt, pre_wait_ms) in SCHEDULE.iter().copied() {
            if pre_wait_ms > 0 {
                compio::time::sleep(std::time::Duration::from_millis(pre_wait_ms)).await;
            }
            match self.try_acquire_advisory_lock(client, &k1, &k2).await {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    // Trace each retry so an operator correlating a
                    // slow-operation report against `pg_locks` can see the
                    // wait pattern from the client side too.
                    tracing::warn!(
                        scope_app_id = %scope.app_id(),
                        scope_name = %scope.name(),
                        attempt,
                        pre_wait_ms,
                        "advisory lock contended; retrying after backoff (security [I43] bounded loop)"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        // One structured trace at exhaustion, so the operator log carries both
        // the per-retry warns and a final summary.
        tracing::warn!(
            scope_app_id = %scope.app_id(),
            scope_name = %scope.name(),
            "advisory lock contention bounded-retry exhausted (5 attempts, ~1.75s); \
             returning LockContention to caller (security [I43])"
        );
        Err(DbError::LockContention {
            message: format!(
                "advisory lock held by another acquirer (scope={}/{}); \
                 bounded retry of 5 attempts at 0/50/200/500/1000ms exhausted. \
                 Hint: retry or check for a stuck holder of this operation scope.",
                scope.app_id(),
                scope.name(),
            ),
        })
    }
}

impl<T: LockManager + ?Sized> BoundedLockAcquire for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// **Security [I43]**: exhaust the bounded-retry loop against a mock whose
    /// Perpetual contention exhausts the configured schedule and preserves the
    /// typed error through wire mapping.
    #[test]
    fn try_acquire_with_backoff_exhaustion_yields_lock_contention() {
        struct MockClient;

        struct ContendingMock {
            attempts: Cell<u32>,
        }

        impl LockManager for ContendingMock {
            type Client = MockClient;
            async fn acquire_advisory_lock(
                &self,
                _client: &Self::Client,
                _key1: &str,
                _key2: &str,
            ) -> Result<(), DbError> {
                unreachable!(
                    "the typed surface must route through try_acquire_with_backoff, \
                     never the blocking primitive"
                )
            }

            async fn try_acquire_advisory_lock(
                &self,
                _client: &Self::Client,
                _key1: &str,
                _key2: &str,
            ) -> Result<bool, DbError> {
                self.attempts.set(self.attempts.get() + 1);
                Ok(false)
            }

            async fn release_advisory_lock(
                &self,
                _client: &Self::Client,
                _key1: &str,
                _key2: &str,
            ) -> Result<(), DbError> {
                unreachable!("not exercised by try_acquire_with_backoff")
            }
        }

        let mock = ContendingMock {
            attempts: Cell::new(0),
        };
        let client = MockClient;
        let scope = LockScope::GlobalApp {
            app_id: "app_contention_test".into(),
            name: "snapshot_restore".into(),
        };

        // Driven on a fresh compio runtime: the test tolerates the ~1.75s
        // real-time worst case. The alternative - injecting a sleep hook -
        // would couple the schedule to a test-only API.
        let result = compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async { mock.try_acquire_with_backoff(&client, &scope).await });

        assert_eq!(
            mock.attempts.get(),
            5,
            "bounded retry loop must execute exactly 5 attempts \
             (schedule = 0/50/200/500/1000ms)"
        );

        let err = result.expect_err("perpetual contention must error");
        match &err {
            DbError::LockContention { message } => {
                assert!(
                    message.contains("app_contention_test"),
                    "message must name the scope app_id, got: {message}"
                );
                assert!(
                    message.contains("snapshot_restore"),
                    "message must name the scope name, got: {message}"
                );
                assert!(
                    message.contains("5 attempts"),
                    "message must document the retry budget, got: {message}"
                );
            }
            other => panic!("expected DbError::LockContention, got {other:?}"),
        }

        // The third assertion this test used to make - that the contention error
        // lowers to the JS code `lock_not_available` - is NOT here, and its
        // absence is the tier boundary rather than a gap. `to_op_error` is the
        // adapter's `ToOpError`, which was lifted OUT of this crate for the
        // same reason the policy above could not stay on the contract: a domain
        // type may not name a delivery mechanism. Naming it here would put
        // `zeroship_runtime` in data-core's test build.
        //
        // The lowering is covered where the lowering lives:
        // `zeroship-data-v8/src/op_error.rs` table-tests
        // `DbError::LockContention` -> `"lock_not_available"` directly.
    }

    /// The schedule's cumulative budget is the security bound the [I43] comment
    /// cites, so pin the arithmetic independently of the loop. A contributor
    /// adding a sixth attempt has to change this number deliberately.
    #[test]
    fn schedule_budget_is_five_attempts_and_1750ms() {
        assert_eq!(SCHEDULE.len(), 5, "the bound is five attempts");
        let total: u64 = SCHEDULE.iter().map(|(_, ms)| ms).sum();
        assert_eq!(total, 1750, "cumulative pre-wait budget is 1750ms");
        assert_eq!(
            SCHEDULE.first().map(|(_, ms)| *ms),
            Some(0),
            "the first attempt must not sleep"
        );
        let indices: Vec<u32> = SCHEDULE.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![1, 2, 3, 4, 5], "attempt indices are 1-based");
    }
}
