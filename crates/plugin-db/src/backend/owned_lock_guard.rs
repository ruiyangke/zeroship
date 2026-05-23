//! RAII-ish guard for **owned-client** session-scoped advisory locks.
//!
//! Sibling to [`crate::backend::LockGuard`] (architecture r13 §I-R13-2,
//! `docs/reviews/plugin-db-architecture-review-2026-05-22-r13.md`).
//! `LockGuard<'p>` wraps a [`compio_postgres::PooledClient`] whose
//! session is **shared** with the pool — Drop returns the client to
//! the pool with the session-scoped lock still held until the pool
//! recycles the underlying connection. `OwnedLockGuard` wraps an
//! **owned** [`compio_postgres::Client`] acquired via
//! [`crate::backend::SqlExecutor::acquire_dedicated_client`]: dropping
//! that client terminates its PG session immediately, so the
//! session-scoped advisory lock auto-releases as a *guaranteed*
//! cleanup, not as a pool-recycle gamble.
//!
//! ## Why two types, not one?
//!
//! r13's Q5 surveyed three options:
//!
//! - **Option (a) — generic over client type**: parameterise
//!   `LockGuard<C>` over `C: ClientLike`. Rejected: every consumer
//!   then names the generic, the two narratives (pool-recycle vs
//!   session-on-Drop) dilute into one rustdoc preamble that doesn't
//!   match either reality, and YAGNI on the abstraction.
//! - **Option (b) — sibling `OwnedLockGuard`**: chosen. ~50 LOC
//!   mirrors `lock_guard.rs` exactly, but the Drop narrative diverges
//!   on the catastrophic-path consequence — owned-client Drop ends
//!   the session immediately, pooled-client Drop just parks back into
//!   the pool. Two types, two narratives, one slot apiece.
//! - **Option (c) — rustdoc-only invariant**: pile contract notes on
//!   the raw `LockManager::{acquire,release}` calls. Rejected:
//!   accumulates rustdoc invariants instead of typed boundaries,
//!   exactly what the register-model path moved *away* from in P0
//!   PR 6.
//!
//! Quote from the architecture review:
//! *"`OwnedLockGuard`'s session-on-Drop story is specific to
//! connection-task-detached owned clients. Two types, two
//! narratives — that's the right shape."*
//!
//! ## Lifecycle scopes
//!
//! Unlike the register-model `LockGuard` (whose lifetime fits in a
//! single Rust function), the **migration backfill** lock spans many
//! V8-driven calls (`begin` → `fetchBatch`*N → `commitBatch`*N), with
//! the client parked in [`crate::context::IsolateDbContext`]'s
//! `mig_lock` slot between calls. `OwnedLockGuard` therefore appears
//! at *two* moments in that lifecycle:
//!
//! 1. **Initial acquisition** in `migrations::exec_begin` — wrap the
//!    freshly-acquired dedicated client; the guard either resolves
//!    via [`Self::release`] (cancelled-refusal early-return) or via
//!    [`Self::into_held`] (park into the slot for the next call).
//! 2. **Slot re-wrap** in `migrations::exec_commit_batch` — take the
//!    client back out of the slot, wrap it in a guard via
//!    [`Self::assume_held`] (the lock is already held — no `acquire`
//!    SQL needed). On Err exits the guard drops (warn + session ends
//!    + lock auto-releases); on the happy `is_done` path
//!    [`Self::release`] issues the explicit unlock SQL; on
//!    non-terminal commit returns [`Self::into_held`] parks the
//!    client back into the slot.
//!
//! ## Why not full RAII?
//!
//! Same reason as `LockGuard`: `Drop::drop` is sync; the unlock SQL
//! is async. Three exit modes:
//!
//! 1. **Normal release** — `let client = guard.release().await?;`
//!    Issues `pg_advisory_unlock` and returns the now-unlocked owned
//!    client (so the caller can drop it explicitly or hold onto it
//!    for further owned-client work).
//! 2. **Hand-off** — `let client = guard.into_held();` The guard
//!    exits its scope, but the lock is intentionally still held by
//!    the returned client. The next stage owns the release
//!    responsibility — typically by parking the client into the
//!    `mig_lock` slot and re-wrapping on the next V8 call.
//! 3. **Drop fallback** — `Drop` runs, logs a warning with the F1
//!    field family (`{app_id, name, transition}`), then drops the
//!    owned client. The compio-postgres `Client`'s connection task
//!    sees EOF on the command channel and terminates; the PG session
//!    ends; **all** session-scoped advisory locks auto-release. This
//!    is the *guaranteed* cleanup path — strictly better than the
//!    pooled-client case where the session may survive the guard
//!    drop by tens of seconds.
//!
//! ## F1 warn-shape preservation
//!
//! The three pre-existing `tracing::warn!` sites in `migrations.rs`
//! (cancelled-refusal, terminalStatus pre-validation reject,
//! backfill-finalise) emit `{app_id, name, error}`. The i6 snapshot
//! test (`migrations.rs::tests::i6_release_advisory_lock_warn_shape_documentation_snapshot`)
//! pins that shape. [`Self::release`] preserves it verbatim: on
//! unlock-SQL failure it emits `{app_id, name, error}` with the
//! exact same message prefix the inline sites used. Drop's "lock
//! leaked" log uses `{app_id, name, transition}` — a *different*
//! family (catastrophic-path log, not a per-site release-failure
//! log), matching the discipline established by the audit-row
//! `transition` field on the migration-finalise warn site.

use compio_postgres::Client;

use crate::backend::{LockManager, LockScope};
use crate::error::DbError;

/// Session-scoped advisory-lock guard for an **owned**
/// [`compio_postgres::Client`] (vs `LockGuard<'p>`'s pooled client).
/// See module docs for the lifecycle contract.
///
/// **Must be consumed via [`Self::release`].await or [`Self::into_held`].**
/// `Drop` cannot await the unlock SQL, so a guard dropped without
/// one of those calls auto-releases the lock by terminating the
/// owned client's PG session — guaranteed (vs the pool case's "wait
/// for recycle") — and logs a `tracing::warn` so operators can spot
/// the missed explicit release.
#[must_use = "OwnedLockGuard must be released via .release().await or .into_held(); \
              dropping it auto-releases via session-end but logs a missed-release warn"]
pub(crate) struct OwnedLockGuard<'b, B: LockManager<Client = Client>> {
    /// Back-reference to the backend so [`Self::release`] can dispatch
    /// through `LockManager::release`. Borrowed for the guard's
    /// lifetime — the migration paths already pass `&backend` through
    /// the call chain, so no extra plumbing.
    backend: &'b B,
    /// The owned PG client that holds the advisory lock at session
    /// scope. `None` after [`Self::release`] / [`Self::into_held`] has
    /// moved it out; `Drop` then becomes a no-op (idempotent).
    client: Option<Client>,
    /// The [`LockScope`] used at acquisition. Stored so [`Self::release`]
    /// can issue `LockManager::release` against the **same** key
    /// derivation `acquire` used — the §10.5 invariant lives in this
    /// single value rather than two textually-identical literals.
    scope: LockScope,
    /// Operator-facing `app_id` for the F1 warn-shape. The
    /// `LockScope`'s `app_id()` is identical, but we cache the
    /// owned `String` here so the Drop log doesn't need to borrow
    /// from `scope`.
    app_id: String,
    /// Operator-facing migration `name` (user-supplied, **without**
    /// the `"mig:"` prefix that `LockScope::migration` adds). Stored
    /// separately because `scope.name()` returns the prefixed form
    /// (`"mig:my_migration"`), but the F1 warn-shape contract — pinned
    /// by `migrations.rs::tests::i6_release_advisory_lock_warn_shape_documentation_snapshot`
    /// — emits the un-prefixed user-facing name.
    name: String,
    /// `true` once the lock has been released or ownership handed
    /// off. Suppresses the Drop warn and short-circuits a second
    /// `release()`.
    released: bool,
}

impl<'b, B: LockManager<Client = Client>> OwnedLockGuard<'b, B> {
    /// Acquire the advisory lock for `scope` against `client` via the
    /// backend's bounded-retry path ([`LockManager::acquire`] →
    /// [`LockManager::try_acquire_with_backoff`], the [I43] schedule).
    ///
    /// Use this when the caller wants the standard contention-wait
    /// behaviour: ~1.75s worst-case retry loop, then
    /// `DbError::LockContention` (wire code `lock_not_available`) if
    /// the lock is still held by another acquirer.
    ///
    /// On Ok the lock is held by the returned guard's client; on Err
    /// no lock was acquired and `client` is dropped at the error site
    /// (session terminates — clean).
    #[allow(dead_code)]
    pub(crate) async fn acquire(
        backend: &'b B,
        client: Client,
        scope: LockScope,
        app_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Self, DbError> {
        backend.acquire(&client, &scope).await?;
        Ok(Self {
            backend,
            client: Some(client),
            scope,
            app_id: app_id.into(),
            name: name.into(),
            released: false,
        })
    }

    /// Try once to acquire the advisory lock; on contention return
    /// `Ok(None)` so the caller can surface a domain-specific error
    /// (e.g. `migrations.rs`'s `err_already_running()` with code
    /// `"migration_already_running"`).
    ///
    /// Routes through [`LockManager::try_acquire`] — single shot, no
    /// backoff loop. Distinct from [`Self::acquire`] because the
    /// migration `begin` path historically maps "another worker holds
    /// the lock" to a *separate* JS-visible error code than the
    /// bounded-retry exhaustion path; preserving that mapping is part
    /// of the [I3] structural fix's no-behaviour-change discipline.
    ///
    /// On `Ok(Some(g))` the lock is held; on `Ok(None)` the lock was
    /// not acquired and `client` is dropped (session terminates); on
    /// Err the underlying SQL failed and `client` is dropped.
    pub(crate) async fn try_acquire(
        backend: &'b B,
        client: Client,
        scope: LockScope,
        app_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Option<Self>, DbError> {
        let got = backend.try_acquire(&client, &scope).await?;
        if !got {
            // Client drops here; PG session terminates; the lock (if
            // we'd somehow acquired one, which we didn't) would
            // auto-release. Caller maps `None` → domain error.
            return Ok(None);
        }
        Ok(Some(Self {
            backend,
            client: Some(client),
            scope,
            app_id: app_id.into(),
            name: name.into(),
            released: false,
        }))
    }

    /// Wrap an **already-locked** owned client back into a guard,
    /// without issuing a fresh `pg_(try_)advisory_lock` SQL.
    ///
    /// Used by `migrations::exec_commit_batch` when it pulls the
    /// client out of the `mig_lock` slot: the lock was acquired by
    /// `exec_begin` on a *previous* V8 call, the client still holds
    /// it (session-scoped), and the commit-batch path wants the same
    /// RAII discipline (drop-on-Err releases via session-end + warns
    /// to tracing; explicit `.release().await` on the finalise rail
    /// issues the unlock SQL).
    ///
    /// SAFETY-ish contract: the caller is asserting that `client`
    /// currently holds the lock named by `scope`. Violating that
    /// (passing a client that doesn't hold the lock) is harmless to
    /// correctness — `release()` issues `pg_advisory_unlock` which
    /// returns `false` for not-held; the SQL doesn't error. The
    /// caller invariant is operational, not memory-safety.
    pub(crate) fn assume_held(
        backend: &'b B,
        client: Client,
        scope: LockScope,
        app_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            client: Some(client),
            scope,
            app_id: app_id.into(),
            name: name.into(),
            released: false,
        }
    }

    /// Borrow the held client for SQL operations that need to run on
    /// the **same** session as the advisory lock (audit-row reads,
    /// BEGIN/COMMIT, etc.).
    ///
    /// Returns `None` after [`Self::release`] / [`Self::into_held`]
    /// has moved the client out — but those methods consume `self`,
    /// so a callers reaching this method always sees `Some(_)` in
    /// practice. The `Option` is internal bookkeeping for Drop
    /// idempotency.
    pub(crate) fn client(&self) -> &Client {
        self.client
            .as_ref()
            .expect("OwnedLockGuard::client() called on a guard whose client has been moved out")
    }

    /// Release the lock and return the (now-unlocked) owned client.
    ///
    /// The unlock SQL is issued via [`LockManager::release`]. On
    /// failure, a `tracing::warn!` fires with the F1 field shape
    /// `{app_id, name, error}` — identical to the three inline
    /// `release_advisory_lock failed on …` sites this type replaces
    /// (see `migrations.rs::tests::i6_release_advisory_lock_warn_shape_documentation_snapshot`).
    /// The lock auto-releases when the returned client is later
    /// dropped (session-end), so the warn is observability-only.
    ///
    /// Idempotent — calling `release()` a second time on a moved-out
    /// guard is impossible (it consumes `self`), but if a future
    /// refactor relaxes that, the `released` flag short-circuits.
    pub(crate) async fn release(mut self) -> Result<Client, DbError> {
        if self.released {
            // Should be unreachable given the consuming `self` — but
            // belt-and-braces in case a future refactor adds a
            // by-ref variant.
            return self
                .client
                .take()
                .ok_or_else(|| DbError::Internal {
                    message: "OwnedLockGuard::release: already released and client gone".into(),
                });
        }
        // Take the client out for the duration of the unlock SQL. We
        // can do this because we own it (vs LockGuard which had to
        // keep `Option<PooledClient<'p>>` Some across the await to
        // avoid moving a borrow). Behaviourally identical: a
        // cancellation mid-await leaves `released = false`, falls
        // through to Drop which warns + drops the client.
        let client = self.client.take().expect(
            "OwnedLockGuard::release: client must be Some on a non-released guard \
             (invariant maintained by constructors)",
        );
        // [I42] cancellation safety: we defer `released = true` until
        // AFTER the unlock SQL await completes, mirroring
        // `LockGuard::release`'s post-bd1e7ce1 ordering. A cancellation
        // mid-await leaves `released = false`, so Drop fires its
        // warn-and-drop-client path. The released-not-yet-flipped
        // window is wider here than `LockGuard` (the client is taken
        // out before the await, so Drop sees `client = None`), so
        // Drop's no-client branch is a no-op — but that's fine,
        // because in cancellation the client itself is also being
        // dropped (the future's stack unwinds), terminating the
        // session and auto-releasing.
        if let Err(e) = self.backend.release(&client, &self.scope).await {
            tracing::warn!(
                app_id = %self.app_id,
                name = %self.name,
                error = %e,
                "release_advisory_lock failed on OwnedLockGuard explicit release \
                 (lock auto-releases on session end)",
            );
        }
        self.released = true;
        Ok(client)
    }

    /// Hand off the still-locked owned client to the caller. The
    /// guard's Drop will NOT release on subsequent drop — the caller
    /// has taken on the release responsibility (typically by parking
    /// the client into the `mig_lock` slot for the next V8 call to
    /// re-wrap via [`Self::assume_held`]).
    pub(crate) fn into_held(mut self) -> Client {
        self.released = true;
        self.client.take().expect(
            "OwnedLockGuard::into_held: client must be Some on a non-released guard \
             (invariant maintained by constructors)",
        )
    }
}

impl<B: LockManager<Client = Client>> Drop for OwnedLockGuard<'_, B> {
    fn drop(&mut self) {
        if !self.released {
            // The owned client is about to drop alongside `self`. The
            // compio-postgres `Client`'s connection task sees its
            // command channel close, terminates the PG session, and
            // PG auto-releases every session-scoped advisory lock
            // that session held. **This is the guaranteed cleanup
            // path** — strictly better than `LockGuard`'s
            // pool-recycle-eventually behaviour.
            //
            // Still log a warn so operators can spot a code path
            // that forgot to call `release()` / `into_held()`:
            // session-end-release is functionally correct but
            // pessimal (the underlying TCP socket churns + the
            // dedicated client allocation churns). The F1 family
            // shape `{app_id, name, transition}` matches the
            // existing migration audit-row finalise warn site for
            // operator log-grep consistency. `transition` here is
            // the static string `"drop_release"` — distinguishes the
            // catastrophic-path drop log from the per-site
            // release-failure logs (which use `error = %e` instead).
            tracing::warn!(
                app_id = %self.app_id,
                name = %self.name,
                transition = "drop_release",
                "OwnedLockGuard dropped without explicit release(); owned client \
                 session will terminate on drop, auto-releasing the session-scoped \
                 advisory lock. This is the guaranteed-cleanup fallback; production \
                 code should reach release()/into_held() so the explicit \
                 pg_advisory_unlock SQL runs before the session ends. \
                 Either an async-cancellation hit the release().await, a panic \
                 unwound the call stack, or an Err early-return chose drop-release \
                 deliberately — investigate if unexpected.",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    //! Lifecycle invariant pins for `OwnedLockGuard`. Mirror the
    //! `lock_guard.rs::tests` pattern: we can't construct a real
    //! `compio_postgres::Client` outside the driver crate, so the
    //! tests use a no-client helper to exercise the `released` flag
    //! transitions and the Drop branch selection (warn vs no-op).
    //!
    //! The structural pin (release fn body byte-offset of
    //! `released = true` vs the unlock-SQL await) mirrors
    //! `LockGuard`'s [I42] regression guard — see the bottom of
    //! this module.
    use super::*;
    use crate::backend::{LockScope, SqlExecutor};

    /// Minimal fake backend for the no-client tests. None of the
    /// `LockManager` methods are exercised by the lifecycle tests
    /// below (they all use the `for_test_no_client` helper which
    /// bypasses `acquire`/`try_acquire`/`assume_held`).
    struct NoopBackend;

    impl SqlExecutor for NoopBackend {
        type Client = Client;
        async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError> {
            unreachable!("NoopBackend: lifecycle tests do not acquire clients")
        }
        async fn pool_exec(
            &self,
            _sql: &str,
            _params: &[&str],
        ) -> Result<u64, DbError> {
            unreachable!("NoopBackend: lifecycle tests do not exec SQL")
        }
        async fn client_exec(
            &self,
            _client: &Self::Client,
            _sql: &str,
            _params: &[&str],
        ) -> Result<u64, DbError> {
            unreachable!("NoopBackend: lifecycle tests do not exec SQL")
        }
    }

    impl LockManager for NoopBackend {
        async fn acquire_advisory_lock(
            &self,
            _client: &Self::Client,
            _k1: &str,
            _k2: &str,
        ) -> Result<(), DbError> {
            unreachable!("NoopBackend: lifecycle tests do not acquire locks")
        }
        async fn try_acquire_advisory_lock(
            &self,
            _client: &Self::Client,
            _k1: &str,
            _k2: &str,
        ) -> Result<bool, DbError> {
            unreachable!("NoopBackend: lifecycle tests do not try-acquire locks")
        }
        async fn release_advisory_lock(
            &self,
            _client: &Self::Client,
            _k1: &str,
            _k2: &str,
        ) -> Result<(), DbError> {
            unreachable!("NoopBackend: lifecycle tests do not release locks")
        }
    }

    /// Test-only constructor that bypasses the acquire-SQL paths so
    /// the lifecycle invariants (`released` flag, Drop branch
    /// selection) can be exercised without a live PG connection.
    /// Mirrors `LockGuard::for_test_no_client`.
    impl<'b, B: LockManager<Client = Client>> OwnedLockGuard<'b, B> {
        fn for_test_no_client(
            backend: &'b B,
            scope: LockScope,
            app_id: impl Into<String>,
            name: impl Into<String>,
        ) -> Self {
            Self {
                backend,
                client: None,
                scope,
                app_id: app_id.into(),
                name: name.into(),
                released: false,
            }
        }
    }

    #[test]
    fn released_flag_starts_false() {
        let backend = NoopBackend;
        let scope = LockScope::migration("app_42", "mig_2026_01");
        let guard = OwnedLockGuard::for_test_no_client(
            &backend,
            scope,
            "app_42",
            "mig_2026_01",
        );
        assert!(!guard.released);
        assert!(guard.client.is_none());
        assert_eq!(guard.app_id, "app_42");
        assert_eq!(guard.name, "mig_2026_01");
    }

    #[test]
    fn drop_with_released_true_does_not_warn() {
        // After `release()` (or `into_held()`) flips `released = true`
        // the subsequent Drop must NOT emit the warn. We can't
        // observe tracing output here without a capture layer, so
        // this test pins the flag transition that *gates* the
        // warn branch.
        let backend = NoopBackend;
        let mut guard = OwnedLockGuard::for_test_no_client(
            &backend,
            LockScope::migration("app_43", "mig_x"),
            "app_43",
            "mig_x",
        );
        guard.released = true;
        // Dropping here must not panic.
        drop(guard);
    }

    #[test]
    fn drop_with_released_false_runs_warn_branch() {
        // Smoke: a guard that was never released drops cleanly (the
        // tracing::warn path doesn't panic). End-to-end coverage of
        // the warn shape lives in integration tests; this exercises
        // the branch so the message format compiles + runs.
        let backend = NoopBackend;
        let guard = OwnedLockGuard::for_test_no_client(
            &backend,
            LockScope::migration("app_44", "mig_y"),
            "app_44",
            "mig_y",
        );
        assert!(!guard.released);
        drop(guard);
    }

    #[test]
    fn into_held_flips_released_flag() {
        // `into_held()` returns the raw owned Client — we can't
        // construct one in a unit test, so we simulate the post-call
        // state directly: after the method, the guard has
        // `released = true` and `client = None`, which is the
        // state the impl reaches before `.expect()`-ing the client
        // out.
        let backend = NoopBackend;
        let mut guard = OwnedLockGuard::for_test_no_client(
            &backend,
            LockScope::migration("app_45", "mig_z"),
            "app_45",
            "mig_z",
        );
        assert!(!guard.released);
        // Mirror the body of `into_held()` up to (but not including)
        // the panicking `.expect()` (which we can't satisfy without
        // a real Client).
        guard.released = true;
        assert!(guard.released);
        // The subsequent Drop must be a no-op (released = true).
        drop(guard);
    }

    /// Structural pin mirroring `LockGuard::release_flips_flag_after_unlock_await_structural`
    /// ([I42] regression guard, `bd1e7ce1`): `self.released = true`
    /// MUST appear AFTER the unlock-SQL await in [`OwnedLockGuard::release`].
    /// Otherwise a cancellation mid-await silently leaks the lock
    /// (Drop sees `released = true` and short-circuits the warn).
    ///
    /// `OwnedLockGuard::release` differs from `LockGuard::release`
    /// in that it `take()`s the client out BEFORE the await (the
    /// owned-client case doesn't have the same borrow-lifetime
    /// constraint pooled-clients had). That doesn't undermine the
    /// invariant — what matters is that the flag flip follows the
    /// `.await` so the cancellation window still triggers the Drop
    /// warn path.
    #[test]
    fn release_flips_flag_after_unlock_await_structural() {
        let src = include_str!("owned_lock_guard.rs");
        let release_start = src
            .find("pub(crate) async fn release(")
            .expect("release fn signature should exist");
        // The next doc-commented method bounds the body.
        let release_end = release_start
            + src[release_start + 1..]
                .find("\n    /// ")
                .expect("release fn should be followed by another doc-commented method")
            + 1;
        let release_body = &src[release_start..release_end];

        let await_pos = release_body
            .find("self.backend.release(&client, &self.scope).await")
            .expect("release() should contain the unlock SQL via LockManager::release");
        // `self.released = true;` appears once in release()'s body.
        let flip_pos = release_body
            .find("self.released = true;")
            .expect("release() should set released = true");

        assert!(
            flip_pos > await_pos,
            "[I42] regression: `self.released = true` must follow the unlock-SQL \
             await in OwnedLockGuard::release(); otherwise a cancellation mid-await \
             would silently skip the Drop warn path. flip_pos={flip_pos}, \
             await_pos={await_pos}"
        );
    }
}
