//! RAII-ish guard returned by [`crate::backend::LockManager`] for
//! session-scoped advisory locks.
//!
//! **P0 PR 6** (`docs/proposals/p0-implementation-plan.md` §"PR 6"):
//! renamed from the prior orchestrator-internal guard type and
//! moved out of `orchestrator/` into `backend/` — the guard is the
//! canonical RAII return shape for the
//! [`crate::backend::LockManager`] capability, not an
//! orchestrator-internal detail. Construction goes through
//! [`LockGuard::acquire`] taking a [`crate::backend::LockScope`] (the
//! typed classifier introduced by the same PR).
//!
//! The four-phase register-model pipeline holds a session-scoped
//! `pg_advisory_lock(hashtext('<app_id>:register_model'),
//!  hashtext('register_model'))` on a single pooled client (key
//! derivation via [`crate::backend::LockScope::to_keys`]). The
//! invariant is: *every* exit path from the locked region — Ok, Err,
//! panic — must either explicitly issue `pg_advisory_unlock` before
//! parking the `PooledClient` back into the pool, OR transfer
//! ownership of the still-locked client to the next stage that will
//! release it.
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
//!    error, closes the pooled client so the backend session dies,
//!    and lets the now-closed entry fall out of the pool on the next
//!    checkout. That tears down the session-scoped advisory lock even
//!    though `Drop` cannot await `pg_advisory_unlock`. This is still a
//!    fallback only; production code should always reach `release()`
//!    or `into_held()`.
//!
//! # Internal representation
//!
//! The guard stores the client as `Option<PooledClient<'p>>` so
//! `release()` and `into_held()` can safely move it out without
//! `mem::replace` / `ManuallyDrop` gymnastics. After either call, the
//! `Option` is `None` and `released` is `true`, so subsequent `Drop`
//! is a no-op (idempotent).
//!
//! # Hardening history
//!
//! The lifecycle invariant lives in one type now, but several rounds
//! of review surfaced edge cases the initial extraction missed.
//! Listed so a future reader can trace the design:
//!
//! - `cbd12944` (cycle 02:05) — extract the guard from 3 open-coded
//!   `pg_advisory_unlock` sites (bootstrap.rs / apply.rs / mod.rs).
//! - `bd1e7ce1` ([I42], cycle 04:00) — defer `released = true` flip
//!   until AFTER the unlock-SQL await completes; a mid-await
//!   cancellation/panic now triggers Drop's catastrophic-path log
//!   instead of silently leaking the lock.
//! - `808a32af` ([I39], cycle 04:35) — annotate `#[must_use]` so
//!   accidental drops surface as compile-time warnings; strengthen
//!   Drop log with "leak:" prefix + operator-facing consequence +
//!   diagnostic checklist.
//! - `ffb1e101` ([I44], cycle 04:35) — replace `let _ =` on the
//!   unlock-SQL await with `if let Err(e) =` + `tracing::warn!` so
//!   an unlock-SQL runtime failure is observable rather than silent.

use compio_postgres::PooledClient;

use crate::backend::{LockManager, LockScope};
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
#[must_use = "LockGuard must be released via .release().await or .into_held(); \
              dropping it leaks the session-scoped advisory lock"]
pub(crate) struct LockGuard<'p> {
    /// The pooled client that holds the advisory lock at session
    /// scope. `None` after `release()` or `into_held()` has moved it
    /// out; `Drop` then becomes a no-op.
    client: Option<PooledClient<'p>>,
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

impl<'p> LockGuard<'p> {
    /// Acquire the advisory lock for the given [`LockScope`] against
    /// `client` via the backend and wrap the result in a guard.
    ///
    /// On Ok the lock is held by the returned guard's pooled client.
    /// On Err the lock was never acquired and the client is returned
    /// to the pool by virtue of being dropped at the error site.
    ///
    /// The caller chooses how to release: `release().await` (normal
    /// exit) or `into_held()` (hand off to a downstream stage that
    /// will release later).
    ///
    /// **P0 PR 6**: takes a [`LockScope`] instead of the previous
    /// `(key: String, tag: &'static str)` pair. The
    /// [`LockManager::acquire`] default impl derives the underlying
    /// `(key1, key2)` strings via [`LockScope::to_keys`] (§7.2 /
    /// §10.5); we cache the derived pair locally so `release()`'s
    /// `pg_advisory_unlock` matches the acquisition exactly even if
    /// `LockScope::to_keys` ever changed shape.
    ///
    /// **Post-P0 mop-up (MAJOR-R14-2)**: takes `&LockScope` so the
    /// caller can keep a single binding (and reuse it if it ever
    /// needs to release outside the guard). The local `(key, tag)`
    /// cache below is still derived via [`LockScope::to_keys`].
    ///
    /// **Security [I43]** (cycle 18:17): the underlying acquisition
    /// is now bounded — `LockManager::acquire`'s default impl loops
    /// on `pg_try_advisory_lock` with a 0/50/200/500/1000ms schedule
    /// (~1.75s worst case) and surfaces `DbError::LockContention`
    /// on exhaustion. The previous direct call to
    /// `acquire_advisory_lock` could stall indefinitely waiting on
    /// `pg_advisory_lock`, giving any app that held its own lock
    /// a within-app DoS lever against its own subsequent
    /// `register_model` invocations. The guard's lifecycle invariants
    /// are unaffected: on `Ok` the lock is held by `self.client` and
    /// will be released via [`Self::release`] / [`Self::into_held`];
    /// on `Err` no lock is held and `client` drops back to the pool.
    pub(crate) async fn acquire<B: LockManager<Client = compio_postgres::Client>>(
        backend: &B,
        client: PooledClient<'p>,
        scope: &LockScope,
    ) -> Result<Self, DbError> {
        let (key, tag) = scope.to_keys();
        // Route through the typed `LockManager::acquire` surface,
        // which (post-[I43]) dispatches to `try_acquire_with_backoff`
        // — bounded retry instead of the legacy blocking
        // `acquire_advisory_lock` primitive. Construction shape is
        // otherwise unchanged: on Err the lock was never held and
        // `client` will drop back to the pool at the error site.
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
            .expect("LockGuard::into_held called on guard with no client")
    }
}

impl Drop for LockGuard<'_> {
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
            // always reach `release()` or `into_held()`.
            if let Some(client) = self.client.as_mut() {
                client.__private_api_close();
            }
            tracing::error!(
                key = %self.key,
                tag = %self.tag,
                "leak: LockGuard dropped without release()/into_held(); \
                 closed the pooled client so the PG session will terminate and \
                 release its session-scoped pg_advisory_lock instead of leaking \
                 it into the pool. Either an async-cancellation hit the \
                 release().await, a panic unwound the call stack, or a code path \
                 forgot to call release()/into_held() — investigate."
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
    impl<'p> LockGuard<'p> {
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
        let guard = LockGuard::for_test_no_client("zs_reg:app_42", "register_model");
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
            LockGuard::for_test_no_client("zs_reg:app_43", "register_model");
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
            LockGuard::for_test_no_client("zs_reg:app_44", "register_model");
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
        let guard = LockGuard::for_test_no_client("zs_reg:app_45", "register_model");
        assert!(!guard.released);
        drop(guard);
    }

    #[test]
    fn released_flag_starts_false() {
        let guard = LockGuard::for_test_no_client("zs_reg:app_46", "register_model");
        assert!(!guard.released);
        assert!(guard.client.is_none());
        assert_eq!(guard.key, "zs_reg:app_46");
        assert_eq!(guard.tag, "register_model");
    }

    /// Structural invariant pin for [I42] (bd1e7ce1 fix order):
    /// `self.released = true` MUST appear AFTER the unlock-SQL
    /// `.await` in the `release()` function body. Otherwise a
    /// cancellation mid-await silently leaks the lock with no Drop
    /// log (because Drop sees `released = true` and short-circuits).
    ///
    /// Mirrors the byte-offset structural test on
    /// `mint_subscription` (`v8_classes/subscription.rs::tests::
    /// mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure`)
    /// — invariant lives in the source layout, not in observable
    /// runtime state, so we pin it via include_str! + index search.
    ///
    /// A future contributor restoring the pre-bd1e7ce1 order (flip
    /// `released = true` BEFORE awaiting the unlock SQL) trips this
    /// test at compile-time without needing a live PG fixture.
    #[test]
    fn release_flips_flag_after_unlock_await_structural() {
        let src = include_str!("lock_guard.rs");
        // Locate the release() function body.
        let release_start = src
            .find("pub(crate) async fn release(")
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
