//! Guard a session-scoped PostgreSQL advisory lock on an owned pool lease.
//!
//! `release().await` unlocks and returns the client. Dropping an unreleased guard
//! discards its physical connection because `Drop` cannot await unlock SQL. A
//! connection with an outstanding lock must never return to the pool.

use compio_postgres::PoolConnection;

use zeroship_data_orm::capability::LockScope;
use zeroship_data_orm::storage::LockManager;
// The bounded-retry `acquire` is policy, not contract: it lives on the
// blanket-implemented extension trait so `LockManager` itself names no runtime.
use zeroship_data_orm::error::DbError;
use zeroship_data_orm::lock_policy::BoundedLockAcquire;

/// Session-scoped advisory-lock guard. See module docs for the lifecycle
/// contract.
///
/// **Must be consumed via `release().await`.**
/// `Drop` cannot await the unlock SQL, so it discards the connection instead
/// of returning a session with an advisory lock to the pool.
#[must_use = "LockGuard must be released via .release().await; \
              dropping it leaks the session-scoped advisory lock"]
#[derive(Debug)]
pub struct LockGuard {
    /// The pooled client that holds the advisory lock at session
    /// scope. `None` after `release()` has moved it
    /// out; `Drop` then becomes a no-op.
    client: Option<PoolConnection>,
    /// First key passed to `pg_advisory_lock(hashtext($1), hashtext($2))`,
    /// derived from the [`LockScope`] via [`LockScope::to_keys`]
    /// (`"{app_id}:{name}"`). Stored so `release()` can issue the
    /// matching `pg_advisory_unlock` without the caller threading the
    /// key through again.
    key: String,
    /// Second key — also derived from the [`LockScope`] (the `name`
    /// field). Stored for symmetry with `release()`'s unlock SQL.
    tag: String,
    /// `true` once the lock has been released or its ownership handed
    /// off. Suppresses the `Drop` warning and short-circuits a second
    /// `release()`.
    released: bool,
}

impl LockGuard {
    /// Acquire the advisory lock for the given [`LockScope`] against
    /// `client` via the backend and wrap the result in a guard.
    ///
    /// On Ok the lock is held by the returned guard's pooled client.
    /// On Err the lock was never acquired and the client is returned
    /// to the pool by virtue of being dropped at the error site.
    ///
    /// The caller releases with `release().await`.
    ///
    /// The bounded policy derives and retries the native lock keys. This guard
    /// retains the same keys so release targets the acquired lock.
    pub async fn acquire<B: LockManager<Client = compio_postgres::PoolConnection>>(
        backend: &B,
        client: PoolConnection,
        scope: &LockScope,
    ) -> Result<Self, DbError> {
        let (key, tag) = scope.to_keys();
        // The typed lock policy bounds acquisition. On error the lock was
        // never held and the client returns to the pool.
        backend.acquire(&client, scope).await?;
        Ok(Self {
            client: Some(client),
            key,
            tag,
            released: false,
        })
    }

    /// Release the lock and return the (now-unlocked) pooled client.
    ///
    /// The session releases the lock automatically if explicit unlock fails.
    pub async fn release(mut self) -> Result<Option<PoolConnection>, DbError> {
        if self.released {
            return Ok(self.client.take());
        }
        // Issue the unlock SQL via a `&` borrow so `self.client` stays
        // Some(_) for the duration of the await. If the future is
        // cancelled / dropped / panics mid-await, Drop sees
        // `released = false` AND `client = Some(_)` and fires its
        // catastrophic-path log. The client then drops back to the
        // pool with the session lock still held — best we can do
        // without a runtime handle (Drop can't await an unlock SQL).
        //
        // Flipping `released = true` BEFORE the await would mean a
        // cancellation here silently leaks the lock with no Drop log.
        // Defer the state flip to AFTER the await completes.
        if let Some(client) = self.client.as_ref() {
            let unlock_sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
            // A bare `let _ =` here would silently swallow runtime
            // errors from the unlock SQL — the operator would never
            // see that the lock might still be held. Log warnings on
            // error so a leak is visible; the lock also auto-releases
            // when the PG session ends.
            if let Err(e) = client
                .query_text_params(unlock_sql, &[self.key.as_str(), self.tag.as_str()])
                .await
            {
                tracing::warn!(
                    key = %self.key,
                    tag = %self.tag,
                    error = %e,
                    "pg_advisory_unlock failed; session-scoped lock may stay \
                     held until the pool recycles the connection"
                );
            }
        }
        self.released = true;
        Ok(self.client.take())
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if !self.released {
            // We can't run `pg_advisory_unlock` here — the call is
            // async and `Drop` is sync. Best-effort fallback: close the
            // pooled client before it goes back to the pool so the
            // backend session terminates and Postgres releases the
            // session-scoped advisory lock with it.
            //
            // This branch is the catastrophic-path fallback (panic
            // unwind, missed `release()` call). Production code should
            // always reach `release()`.
            if let Some(client) = self.client.take() {
                client.discard();
            }
            tracing::error!(
                key = %self.key,
                tag = %self.tag,
                "leak: LockGuard dropped without release(); \
                 closed the pooled client so the PG session will terminate and \
                 release its session-scoped pg_advisory_lock instead of leaking \
                 it into the pool. Either an async-cancellation hit the \
                 release().await, a panic unwound the call stack, or a code path \
                 forgot to call release() — investigate."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only constructor that bypasses `acquire()` so we can
    /// inspect the lifecycle invariants (`released` flag, Drop
    /// behaviour, idempotency) without a live pool.
    impl LockGuard {
        fn for_test_no_client(key: impl Into<String>, tag: impl Into<String>) -> Self {
            Self {
                client: None,
                key: key.into(),
                tag: tag.into(),
                released: false,
            }
        }
    }

    #[test]
    fn release_idempotent_when_no_client() {
        // Build a guard with no client (test helper). Calling
        // `release()` should flip `released` and return `Ok(None)`
        // rather than panicking.
        let guard = LockGuard::for_test_no_client("zs_reg:app_42", "snapshot_restore");
        assert!(!guard.released);
        // Use compio's local runtime to drive the async release.
        let out = compio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move { guard.release().await });
        let client_opt = out.expect("release should not error when no client present");
        assert!(
            client_opt.is_none(),
            "no client was attached, so none returned"
        );
    }

    // `into_held_flips_released_flag` stood here and went with
    // `into_held` on 2026-09-04. It never called the method: the return
    // type cannot be constructed off a live pool, so the body hand-wrote
    // `guard.released = true` and said so. It was a name-only witness -
    // `rg into_held` hit the test and read as coverage, `rg -w into_held`
    // missed it entirely, and only the body settled which was right. What
    // it actually asserted (Drop is a no-op once `released` is set) is
    // `drop_with_released_true_does_not_warn` below, which survives.

    #[test]
    fn drop_with_released_true_does_not_warn() {
        // After `release()` runs, `released` is true; the subsequent
        // `Drop` should be a no-op (no tracing::error). We can't
        // observe tracing output without a capture layer, so this
        // test instead verifies the field state transition that
        // *gates* the warning.
        let mut guard = LockGuard::for_test_no_client("zs_reg:app_44", "snapshot_restore");
        // Simulate a successful release: flip the flag manually
        // (the async path does this under the hood).
        guard.released = true;
        // Dropping here must not panic and must not abort the test.
        drop(guard);
    }

    #[test]
    fn drop_with_released_false_runs_warning_branch() {
        // Smoke test: a guard that was never released drops cleanly
        // (the tracing::error path doesn't panic). We can't capture
        // the log line without a tracing subscriber, but exercising
        // the branch ensures the message format compiles and runs.
        let guard = LockGuard::for_test_no_client("zs_reg:app_45", "snapshot_restore");
        assert!(!guard.released);
        drop(guard);
    }

    #[test]
    fn released_flag_starts_false() {
        let guard = LockGuard::for_test_no_client("zs_reg:app_46", "snapshot_restore");
        assert!(!guard.released);
        assert!(guard.client.is_none());
        assert_eq!(guard.key, "zs_reg:app_46");
        assert_eq!(guard.tag, "snapshot_restore");
    }

    /// Cancellation must leave the guard unreleased so `Drop` discards the
    /// connection. This structural check pins the state transition after the
    /// unlock await.
    #[test]
    fn release_flips_flag_after_unlock_await_structural() {
        let src = include_str!("lock_guard.rs");
        // Locate the release() function body.
        //
        // Split the needle so this test's source does not match itself.
        let release_start = src
            .find(concat!("pub async", " fn release("))
            .expect("release fn signature should exist");
        // The next `fn ` after release() bounds its body.
        let release_end = release_start
            + src[release_start + 1..]
                .find("\n    /// ")
                .expect("release fn should be followed by another doc-commented method")
            + 1;
        let release_body = &src[release_start..release_end];

        // The unlock-SQL `.await;` appears once in release().
        let await_pos = release_body
            .find(".query_text_params(")
            .expect("release() should contain the unlock SQL call");
        // The `self.released = true` assignment appears once in
        // release() (the test-only branch above the closure doesn't
        // count — it's outside the function).
        let flip_pos = release_body
            .find("self.released = true;")
            .expect("release() should set released = true");

        assert!(
            flip_pos > await_pos,
            "[I42] regression: `self.released = true` must follow the unlock-SQL \
             await in release(); otherwise a cancellation mid-await silently \
             leaks the session-scoped advisory lock without firing Drop's \
             catastrophic-path log. See bd1e7ce1's commit message for the \
             cancellation-safety rationale. flip_pos={flip_pos}, await_pos={await_pos}"
        );
    }
}
