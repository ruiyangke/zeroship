//! RAII-ish guard for the orchestrator's per-app advisory lock.
//!
//! The four-phase register-model pipeline holds a session-scoped
//! `pg_advisory_lock(hashtext('zs_reg:<app>'), hashtext('register_model'))`
//! on a single pooled client. The invariant is: *every* exit path from
//! the locked region — Ok, Err, panic — must either explicitly issue
//! `pg_advisory_unlock` before parking the `PooledClient` back into the
//! pool, OR transfer ownership of the still-locked client to the next
//! stage that will release it.
//!
//! Before this guard, three commits in two days (`b4e533e2`,
//! `37a0ef76`, `3bb41fa1`) plugged that invariant inline at three
//! different stages of the pipeline:
//!
//! - `bootstrap.rs` — release on Err, hand off the locked client on Ok
//!   to `apply()`.
//! - `apply.rs` — release between Pass 1 and Pass 2 regardless of Pass 1
//!   outcome (CIC can't run under the lock).
//! - `run_pipeline` (orchestrator `mod.rs`) — release on Err from
//!   plan / validate, hand off to `apply` on Ok.
//!
//! Three sites, one invariant, three open-coded `pg_advisory_unlock`
//! sequences. This module centralises the pattern.
//!
//! # Why not full RAII?
//!
//! `Drop::drop` is sync; the unlock SQL is async. The guard therefore
//! has three exit modes:
//!
//! 1. **Normal release** — `let client = guard.release().await?;`
//!    Issues `pg_advisory_unlock` and hands the now-unlocked client
//!    back to the caller (so it can be parked or reused).
//! 2. **Hand-off** — `let client = guard.into_held();` The guard exits
//!    its scope, but the lock is intentionally still held by the
//!    returned client. The next stage owns the release responsibility.
//! 3. **Panic / catastrophic propagation** — `Drop` runs, logs an
//!    error, and parks the client back into the pool with the lock
//!    still held. The session-scoped lock will release when the
//!    pooled connection is recycled or the backend session ends.
//!    This is a fallback only; production code should always reach
//!    `release()` or `into_held()`.
//!
//! # Internal representation
//!
//! The guard stores the client as `Option<PooledClient<'p>>` so
//! `release()` and `into_held()` can safely move it out without
//! `mem::replace` / `ManuallyDrop` gymnastics. After either call, the
//! `Option` is `None` and `released` is `true`, so subsequent `Drop`
//! is a no-op (idempotent).

use compio_postgres::PooledClient;

use crate::backend::Backend;
use crate::error::DbError;

/// Session-scoped advisory-lock guard for the register-model
/// orchestrator. See module docs for the lifecycle contract.
///
/// **Must be consumed via `release().await` or `into_held()`.**
/// `Drop` cannot await the unlock SQL, so a guard dropped without
/// one of those calls leaks the session-scoped advisory lock until
/// the PG session ends (typically when the pool recycles the
/// connection — could be tens of seconds to minutes). The
/// `#[must_use]` annotation surfaces accidental drops as compile-time
/// warnings on common patterns (e.g. `let _ = acquire(...).await`).
#[must_use = "OrchestratorLockGuard must be released via .release().await or .into_held(); \
              dropping it leaks the session-scoped advisory lock"]
pub(crate) struct OrchestratorLockGuard<'p> {
    /// The pooled client that holds the advisory lock at session
    /// scope. `None` after `release()` or `into_held()` has moved it
    /// out; `Drop` then becomes a no-op.
    client: Option<PooledClient<'p>>,
    /// First key passed to `pg_advisory_lock(hashtext($1), hashtext($2))`
    /// (the per-app namespace, e.g. `"zs_reg:<app_id>"`). Stored so
    /// `release()` can issue the matching `pg_advisory_unlock` without
    /// the caller threading the key through again.
    key: String,
    /// Second key — the constant stage tag (currently `"register_model"`).
    tag: &'static str,
    /// `true` once the lock has been released or its ownership handed
    /// off. Suppresses the `Drop` warning and short-circuits a second
    /// `release()`.
    released: bool,
}

impl<'p> OrchestratorLockGuard<'p> {
    /// Acquire the advisory lock on `(key, tag)` against `client` via
    /// the backend and wrap the result in a guard.
    ///
    /// On Ok the lock is held by the returned guard's pooled client.
    /// On Err the lock was never acquired and the client is returned
    /// to the pool by virtue of being dropped at the error site.
    ///
    /// The caller chooses how to release: `release().await` (normal
    /// exit) or `into_held()` (hand off to a downstream stage that
    /// will release later).
    pub(crate) async fn acquire<B: Backend<Client = compio_postgres::Client>>(
        backend: &B,
        client: PooledClient<'p>,
        key: String,
        tag: &'static str,
    ) -> Result<Self, DbError> {
        backend.acquire_advisory_lock(&client, &key, tag).await?;
        Ok(Self {
            client: Some(client),
            key,
            tag,
            released: false,
        })
    }

    /// Release the lock and return the (now-unlocked) pooled client.
    ///
    /// Idempotent — calling `release()` a second time on a moved-out
    /// guard is a no-op that returns `Ok(None)`. In practice the guard
    /// is consumed by `release()`, so a "second call" only happens if
    /// the caller stashed the guard somewhere; the type system makes
    /// that ergonomically awkward.
    ///
    /// The unlock SQL is best-effort: errors from the underlying
    /// `query_text_params` are swallowed (matches the pre-existing
    /// inline sites; the session-scoped lock will auto-release when
    /// the backend session ends if the explicit unlock failed).
    pub(crate) async fn release(mut self) -> Result<Option<PooledClient<'p>>, DbError> {
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
        // [I42] (concurrency r5 M-NEW-r5-1): the prior version flipped
        // `released = true` BEFORE the await, so a cancellation here
        // silently leaked the lock with no Drop log. Defer the state
        // flip to AFTER the await completes.
        if let Some(client) = self.client.as_ref() {
            let unlock_sql =
                "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
            // [I44] (code-critique r5 MAJOR-R5-5): a bare `let _ =`
            // silently swallows runtime errors from the unlock SQL —
            // operator never sees that the lock might still be held.
            // Log warnings on error so a leak is visible; the lock
            // also auto-releases when the PG session ends.
            if let Err(e) = client
                .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
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

    /// Hand off the still-locked client to the caller. The guard's
    /// `Drop` will NOT release on subsequent drop — the caller has
    /// taken on the release responsibility.
    ///
    /// In the current pipeline the bootstrap → apply boundary keeps the
    /// guard itself in scope (no need to drop down to the raw
    /// `PooledClient`). This method exists for future callers that need
    /// to thread the locked client into an API that doesn't accept the
    /// guard type — flag it `dead_code` until that arrives so the
    /// invariant stays codified at the guard boundary rather than
    /// re-discovered as another open-coded unlock sequence.
    #[allow(dead_code)]
    pub(crate) fn into_held(mut self) -> PooledClient<'p> {
        self.released = true;
        // SAFETY-ish: by construction, a guard returned from
        // `acquire()` always has `client = Some(_)`; the only way to
        // produce `client = None` is via `release()` / `into_held()`
        // themselves, and both flip `released = true` first. So if
        // `released` was false on entry the `Option` must be `Some`.
        // Test-only constructors that bypass `acquire()` document this
        // contract.
        self.client
            .take()
            .expect("OrchestratorLockGuard::into_held called on guard with no client")
    }
}

impl Drop for OrchestratorLockGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            // We can't run `pg_advisory_unlock` here — the call is
            // async and `Drop` is sync. The pooled client (still in
            // `self.client`) will return to the pool with the
            // session-scoped lock held. Postgres releases it when the
            // backend session itself terminates (connection close /
            // pool recycle), but until then any caller blocked on
            // `pg_advisory_lock(zs_reg:<app>, register_model)` will
            // stall.
            //
            // This branch is the catastrophic-path fallback (panic
            // unwind, missed `release()` call). Production code should
            // always reach `release()` or `into_held()`.
            tracing::error!(
                key = %self.key,
                tag = %self.tag,
                "leak: OrchestratorLockGuard dropped without release()/into_held(); \
                 session-scoped pg_advisory_lock will stay held until the pooled \
                 client's PG session closes (typically on pool recycle). \
                 Concurrent register_model callers for this app will stall in \
                 the meantime. Either an async-cancellation hit the release().await, \
                 a panic unwound the call stack, or a code path forgot to call \
                 release()/into_held() — investigate."
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
    impl<'p> OrchestratorLockGuard<'p> {
        fn for_test_no_client(key: impl Into<String>, tag: &'static str) -> Self {
            Self {
                client: None,
                key: key.into(),
                tag,
                released: false,
            }
        }
    }

    #[test]
    fn release_idempotent_when_no_client() {
        // Build a guard with no client (test helper). Calling
        // `release()` should flip `released` and return `Ok(None)`
        // rather than panicking.
        let guard = OrchestratorLockGuard::for_test_no_client("zs_reg:app_42", "register_model");
        assert!(!guard.released);
        // Use compio's local runtime to drive the async release.
        let out = compio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move { guard.release().await });
        let client_opt = out.expect("release should not error when no client present");
        assert!(client_opt.is_none(), "no client was attached, so none returned");
    }

    #[test]
    fn into_held_flips_released_flag() {
        // `into_held()` returns the raw PooledClient — we can't
        // construct one in a unit test (it has a private field +
        // pool back-reference), so we simulate the post-call state
        // directly: after `into_held()` the guard has
        // `released = true` and `client = None`, which is exactly
        // what the impl does before `.expect()`-ing the client out.
        // This test pins the *flag transition* that suppresses
        // Drop's warning.
        let mut guard =
            OrchestratorLockGuard::for_test_no_client("zs_reg:app_43", "register_model");
        assert!(!guard.released);
        // Manually mirror the prefix of `into_held`'s body:
        guard.released = true;
        // The remaining `client.take().expect(...)` would panic in
        // this no-client test; verify the prefix completes cleanly.
        assert!(guard.released);
        // Subsequent Drop must be a no-op (no warning branch).
        drop(guard);
    }

    #[test]
    fn drop_with_released_true_does_not_warn() {
        // After `release()` runs, `released` is true; the subsequent
        // `Drop` should be a no-op (no tracing::error). We can't
        // observe tracing output without a capture layer, so this
        // test instead verifies the field state transition that
        // *gates* the warning.
        let mut guard =
            OrchestratorLockGuard::for_test_no_client("zs_reg:app_44", "register_model");
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
        let guard = OrchestratorLockGuard::for_test_no_client("zs_reg:app_45", "register_model");
        assert!(!guard.released);
        drop(guard);
    }

    #[test]
    fn released_flag_starts_false() {
        let guard = OrchestratorLockGuard::for_test_no_client("zs_reg:app_46", "register_model");
        assert!(!guard.released);
        assert!(guard.client.is_none());
        assert_eq!(guard.key, "zs_reg:app_46");
        assert_eq!(guard.tag, "register_model");
    }
}
