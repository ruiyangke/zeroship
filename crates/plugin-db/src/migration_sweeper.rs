//! F1 sweeper-half — orphan `Running`-row reaper.
//!
//! ## Background — the F1 split
//!
//! When a worker crashes mid-backfill it leaves its `__zeroship_migrations`
//! audit row stuck in `status = 'running'`. Two halves close this gap:
//!
//! - **warn-half** (landed r12, lives in [`crate::migrations`] +
//!   [`crate::backend::OwnedLockGuard`]): on the *graceful* failure
//!   paths the guard emits a `tracing::warn!` with the F1 field shape
//!   `{app_id, name, transition}` so operators see a stalled row. But a
//!   *hard* crash (SIGKILL, OOM, node loss) never runs the guard's
//!   `Drop` — the row simply rots in `running` with no warn.
//!
//! - **sweeper-half** (this module): a background pass that actually
//!   transitions those orphaned rows to a terminal state. It is the
//!   reaper the warn-half can only point at.
//!
//! ## Liveness signal — the advisory lock IS the heartbeat
//!
//! A live migration run holds a *session-scoped* advisory lock on the
//! migration's [`LockScope`] (`pg_advisory_lock(hashtext("{app_id}:mig:{name}")::int4,
//! hashtext("mig:{name}")::int4)`, see [`crate::migrations`] module docs).
//! Postgres releases that lock automatically when the owning backend
//! session ends — which is *exactly* what a crash does. So:
//!
//! > **A `running` audit row whose migration advisory lock is no longer
//! > held belongs to a dead worker.**
//!
//! The sweeper exploits this directly: rather than racing on `pg_locks`
//! introspection, it calls `pg_try_advisory_lock` with the row's
//! migration keys. The single call does double duty:
//!
//! 1. **Liveness check** — if the lock is free (`got = true`) the prior
//!    owner is gone; if it is held (`got = false`) a live worker still
//!    owns the run, so the row is NOT orphaned and the sweeper skips it.
//! 2. **Concurrency guard** — winning the `try` lock means *this*
//!    sweeper, and no other, will transition the row. A second sweeper
//!    racing the same row gets `got = false` and skips. This makes the
//!    sweep idempotent + safe under concurrent sweepers with no extra
//!    bookkeeping (design brief: "only the sweeper that acquires the
//!    per-row/per-app migration lock transitions it; others skip").
//!
//! The sweeper releases the lock immediately after the transition so it
//! never blocks a *legitimate* re-run of the same migration that starts
//! moments later.
//!
//! ## Staleness threshold
//!
//! The advisory-lock check alone is racy against a worker that has *just*
//! started a run but not yet completed its first heartbeat — its lock is
//! held, so the sweeper would skip it anyway. But to avoid even acquiring
//! the lock for fresh rows (and to give a small grace for a worker that
//! transiently dropped its connection but is reconnecting), the candidate
//! SELECT also requires the row's `last_heartbeat_at` (or, if a row never
//! heart-beat, its `updated_at`) to be older than `stale_after`. §18 Q4
//! recommends 5× the 10s heartbeat interval ≈ 50s; we default to
//! [`DEFAULT_STALE_AFTER_SECS`] (5 min) — strictly more conservative, so
//! a live-but-laggy worker is never reaped out from under itself.
//!
//! ## Terminal state + observability
//!
//! Each swept row transitions to `status = 'failed'` with the terminal
//! reason [`SWEEP_REASON`] (`orphan_running_row_swept`) written to the
//! `error` column, and `owner_session_id` cleared. A `tracing::warn!`
//! fires per swept row carrying the F1 field shape `{app_id, name,
//! transition}` plus `audit_id` and a `swept = true` discriminator — the
//! same operator-grep contract the warn-half uses (see
//! [`crate::test_support`] for the shape pin). The warn doubles as the
//! `orphan_running_row_swept` metric event: operators count
//! `transition="failed" swept=true` log lines per app.
//!
//! ## Cron wiring
//!
//! [`register_sweep_schedule`] is the wiring point the control plane's
//! scheduler calls. Today it is a documented stub — the actual timer
//! firing wires in alongside §17.6's replication-slot watchdog scheduler
//! (the control plane owns *one* cluster-wide scheduler; the sweeper
//! shares it). The sweep LOGIC ([`sweep_orphan_running_rows`]) is fully
//! implemented and tested independently of the timer.
//!
//! ## Dead-code posture (cron wiring pending)
//!
//! Until the control-plane scheduler lands (§17.6 watchdog co-wiring),
//! NOTHING in a *production* build calls this module — the timer firing
//! is the stub. Every symbol is exercised by the in-crate `#[cfg(test)]`
//! unit tests and the `tests/integration.rs` PG suite (reachable because
//! `test-helpers` re-exposes the module as `pub`), so the coverage is
//! real; it is only the default release build where the surface is
//! orphaned. This mirrors the `auth/*` subtree's posture (fully built,
//! zero production callers until the control-plane wire-up flips it on)
//! — hence the module-level `allow(dead_code)`. Remove the allow in the
//! same PR that wires the scheduler.
#![allow(dead_code)]

use compio_postgres::Pool;

use crate::backend::LockScope;
use crate::error::DbError;

/// Default staleness grace before a `running` row becomes a sweep
/// candidate. §18 Q4 recommends 5× a 10s heartbeat (≈50s); we pick a
/// strictly more conservative 5 min so a live-but-laggy worker is never
/// reaped. The control-plane scheduler MAY pass a different value to
/// [`sweep_orphan_running_rows`]; this constant is only the default the
/// stub schedule would use.
pub const DEFAULT_STALE_AFTER_SECS: i64 = 300;

/// Default cadence the (stubbed) cron would fire the sweep at. §18 Q4
/// recommends 60s. Not consumed by [`sweep_orphan_running_rows`] (which
/// is a single pass); it documents the intended scheduler cadence for
/// whoever wires the timer.
pub const DEFAULT_SWEEP_INTERVAL_SECS: i64 = 60;

/// Terminal `error` marker written to a swept row. Operators grep this
/// to distinguish a crash-reaped row from a DDL that genuinely ran and
/// failed. Stable string — part of the operator contract.
pub const SWEEP_REASON: &str = "orphan_running_row_swept";

/// One row the sweep examined or transitioned. Returned so the
/// scheduler / tests can assert on what happened without re-querying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweptRow {
    /// `id` of the `__zeroship_migrations` row.
    pub audit_id: i64,
    /// `collection` the backfill targeted.
    pub collection: String,
    /// `change_kind` — the migration name component of the lock scope.
    pub name: String,
    /// Outcome of the sweep for this candidate.
    pub outcome: SweepOutcome,
}

/// What the sweeper did with a candidate row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepOutcome {
    /// The advisory lock was free → owner dead → row transitioned to
    /// `failed` with [`SWEEP_REASON`].
    Swept,
    /// The advisory lock was still held by a live worker → skipped.
    LiveLockHeld,
    /// The lock was free but the UPDATE matched no row — another
    /// sweeper / a late `commitBatch` drove it terminal between the
    /// candidate SELECT and our UPDATE. Benign; counted separately so a
    /// test can prove the idempotent race path.
    AlreadyTerminal,
}

/// Candidate row from the staleness SELECT, before the lock check.
struct Candidate {
    audit_id: i64,
    collection: String,
    name: String,
}

/// Run a single sweep pass over `app_id`'s `__zeroship_migrations` table.
///
/// Transitions every orphaned `running` row (stale heartbeat AND no live
/// advisory-lock holder) to `failed` with [`SWEEP_REASON`]. Returns one
/// [`SweptRow`] per *candidate* examined (including skipped ones) so the
/// caller can emit metrics / assertions.
///
/// `stale_after_secs` is the grace before a `running` row is eligible;
/// pass [`DEFAULT_STALE_AFTER_SECS`] for the production default.
///
/// ## Idempotency + concurrency
///
/// Safe to call repeatedly and concurrently. The `pg_try_advisory_lock`
/// gate ensures at most one sweeper transitions any given row, and the
/// UPDATE's `WHERE status = 'running'` clause makes a doubled transition
/// a no-op ([`SweepOutcome::AlreadyTerminal`]).
///
/// Runs entirely under the caller-supplied [`Pool`], which in production
/// is the **platform-role** connection (§17.5) — the sweeper is a
/// control-plane maintenance task, not creator SQL.
pub async fn sweep_orphan_running_rows(
    pool: &Pool,
    app_id: &str,
    stale_after_secs: i64,
) -> Result<Vec<SweptRow>, DbError> {
    let candidates = select_stale_running(pool, app_id, stale_after_secs).await?;
    let mut results = Vec::with_capacity(candidates.len());

    for cand in candidates {
        // Derive the SAME `(key1, key2)` the migration run held, via the
        // canonical `LockScope::migration` constructor. This guarantees
        // the sweeper's `pg_try_advisory_lock` keys are byte-identical to
        // the run's `pg_advisory_lock` keys — any drift here would make
        // the sweeper think every row is orphaned (lock keys never match
        // → always free).
        let scope = LockScope::migration(app_id, &cand.name);
        let (key1, key2) = scope.to_keys();

        // Atomic liveness-check + concurrency-guard. `got = true` means
        // the lock was free (owner dead) AND we now hold it.
        let got = try_advisory_lock(pool, &key1, &key2).await?;
        if !got {
            results.push(SweptRow {
                audit_id: cand.audit_id,
                collection: cand.collection,
                name: cand.name,
                outcome: SweepOutcome::LiveLockHeld,
            });
            continue;
        }

        // We own the lock — transition the row, then ALWAYS release
        // (even on UPDATE error) so a legitimate re-run isn't blocked.
        let transition = transition_to_failed(pool, app_id, cand.audit_id).await;
        // Release is best-effort: the session would auto-release the lock
        // at connection close anyway, but the sweeper reuses the pooled
        // connection so we unlock eagerly. A unlock failure is logged but
        // does not fail the sweep (the row is already transitioned).
        if let Err(e) = advisory_unlock(pool, &key1, &key2).await {
            tracing::warn!(
                app_id = %app_id,
                name = %cand.name,
                collection = %cand.collection,
                audit_id = cand.audit_id,
                unlock_err = %e,
                "migration sweeper: pg_advisory_unlock failed after sweep; \
                 lock auto-releases on session end"
            );
        }

        let transitioned = transition?;
        let outcome = if transitioned {
            emit_swept_warn(app_id, &cand.name, &cand.collection, cand.audit_id);
            SweepOutcome::Swept
        } else {
            // Lock was free but the row was already terminal — a
            // concurrent sweeper or a late commitBatch won the race.
            // Benign; no warn (nothing was orphaned by us).
            SweepOutcome::AlreadyTerminal
        };

        results.push(SweptRow {
            audit_id: cand.audit_id,
            collection: cand.collection,
            name: cand.name,
            outcome,
        });
    }

    Ok(results)
}

/// SELECT the orphan candidates: `phase='backfill'` rows still in
/// `running` whose last liveness signal is older than `stale_after_secs`.
///
/// A row that never heart-beat (`last_heartbeat_at IS NULL`) falls back
/// to `updated_at` — `insert_backfill_running` always stamps
/// `last_heartbeat_at = NOW()`, so the NULL arm is belt-and-braces for
/// rows written by an older code path.
async fn select_stale_running(
    pool: &Pool,
    app_id: &str,
    stale_after_secs: i64,
) -> Result<Vec<Candidate>, DbError> {
    validate_app_id(app_id)?;
    // `stale_after_secs` is interpolated into an INTERVAL via `make_interval`
    // bound as a parameter — never string-spliced. We clamp to >= 0 so a
    // negative value can't make every row a candidate.
    let stale = stale_after_secs.max(0);
    let sql = format!(
        r#"SELECT id, collection, change_kind
             FROM "{app_id}"."__zeroship_migrations"
            WHERE phase = 'backfill'
              AND status = 'running'
              AND COALESCE(last_heartbeat_at, updated_at)
                    < NOW() - make_interval(secs => $1::double precision)"#
    );
    let stale_s = stale.to_string();
    let rows = pool
        .query_text_params(&sql, &[stale_s.as_str()])
        .await
        .map_err(|e| crate::error::coded_sql("migration sweeper: select stale running", e))?;
    Ok(rows
        .iter()
        .map(|r| Candidate {
            audit_id: r.get::<_, i64>("id"),
            collection: r.get::<_, String>("collection"),
            name: r.get::<_, String>("change_kind"),
        })
        .collect())
}

/// `pg_try_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)`.
/// Returns `true` if the lock was free and is now held by this session.
async fn try_advisory_lock(pool: &Pool, key1: &str, key2: &str) -> Result<bool, DbError> {
    let sql = "SELECT pg_try_advisory_lock(hashtext($1)::int4, hashtext($2)::int4) AS got";
    let rows = pool
        .query_text_params(sql, &[key1, key2])
        .await
        .map_err(|e| crate::error::coded_sql("migration sweeper: pg_try_advisory_lock", e))?;
    Ok(rows
        .first()
        .map(|r| r.try_get::<_, bool>("got").unwrap_or(false))
        .unwrap_or(false))
}

/// `pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)`.
async fn advisory_unlock(pool: &Pool, key1: &str, key2: &str) -> Result<(), DbError> {
    let sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
    pool.query_text_params(sql, &[key1, key2])
        .await
        .map_err(|e| crate::error::coded_sql("migration sweeper: pg_advisory_unlock", e))?;
    Ok(())
}

/// Transition a single orphaned row to `failed` with [`SWEEP_REASON`].
/// `WHERE status = 'running'` makes a doubled call a no-op; returns
/// `true` iff this call flipped the row.
///
/// Mirrors [`crate::audit::finalise_backfill`]'s terminal-transition
/// shape (clears `owner_session_id`, stamps `updated_at`) but uses an
/// unconditional `error = $2` write (not `COALESCE`) so the sweep marker
/// always lands — a crashed row's prior `error` is uninteresting noise.
async fn transition_to_failed(pool: &Pool, app_id: &str, audit_id: i64) -> Result<bool, DbError> {
    validate_app_id(app_id)?;
    let sql = format!(
        r#"UPDATE "{app_id}"."__zeroship_migrations"
              SET status = 'failed',
                  error = $2,
                  owner_session_id = NULL,
                  updated_at = NOW()
            WHERE id = $1::bigint AND status = 'running'
            RETURNING id"#
    );
    let id_s = audit_id.to_string();
    let rows = pool
        .query_text_params(&sql, &[id_s.as_str(), SWEEP_REASON])
        .await
        .map_err(|e| crate::error::coded_sql("migration sweeper: transition to failed", e))?;
    Ok(!rows.is_empty())
}

/// Emit the F1 warn-family event for a swept row.
///
/// Pulled into a standalone fn (not inlined at the call site) so the
/// **field-name contract** can be pinned by an in-crate `#[cfg(test)]`
/// test under [`crate::test_support`]'s capture layer WITHOUT a live
/// Postgres — the DB transition is tested separately in
/// `tests/integration.rs`.
///
/// Field shape `{app_id, name, transition}` is the operator-grep
/// contract shared with the 8 warn-half sites in `migrations.rs` /
/// `context.rs` (`OwnedLockGuard::{drop,release}`, the `finalise_backfill`
/// hybrid site, etc.). `swept = true` is the discriminator + the
/// `orphan_running_row_swept` metric event; `transition = "failed"`
/// mirrors the string-literal style the audit_id-slot warn sites use.
fn emit_swept_warn(app_id: &str, name: &str, collection: &str, audit_id: i64) {
    tracing::warn!(
        app_id = %app_id,
        name = %name,
        collection = %collection,
        audit_id = audit_id,
        transition = "failed",
        reason = SWEEP_REASON,
        swept = true,
        "migration sweeper: transitioned orphaned 'running' audit \
         row to 'failed' — owning worker is dead (advisory lock was \
         released); investigate the crashed migration"
    );
}

/// Validate an `app_id` used as a schema name. Same `[A-Za-z0-9_-]`
/// allowlist the audit layer enforces — belt-and-braces against an
/// internal caller passing something weird into the interpolated DDL.
fn validate_app_id(app_id: &str) -> Result<(), DbError> {
    if app_id.is_empty() {
        return Err(DbError::validation(
            "invalid_app_id",
            "migration sweeper: app_id must not be empty",
        ));
    }
    for c in app_id.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(DbError::validation(
                "invalid_app_id",
                format!(
                    "migration sweeper: app_id contains invalid character {c:?} \
                     — only [A-Za-z0-9_-] permitted"
                ),
            ));
        }
    }
    Ok(())
}

/// Cron wiring point — registers the orphan-row sweep with the
/// control-plane scheduler.
///
/// **STATUS: cron wiring pending.** The actual timer firing wires in
/// alongside §17.6's replication-slot watchdog scheduler — the control
/// plane owns exactly one cluster-wide scheduler (a per-worker sweeper
/// would have N workers racing on the same rows; the `try` lock makes
/// that *safe* but wasteful). When that scheduler lands, this function
/// becomes its registration hook: the scheduler calls
/// [`sweep_orphan_running_rows`] every [`DEFAULT_SWEEP_INTERVAL_SECS`]
/// per app under the platform-role pool.
///
/// Today it is a no-op stub returning the cadence the scheduler should
/// use, so the control plane can adopt the wiring without a signature
/// change. The sweep LOGIC is fully implemented + tested via
/// [`sweep_orphan_running_rows`] directly.
///
/// Mirrors the §17.6 watchdog's "logic implemented, timer deferred"
/// posture — see [`crate::replication::drop_abandoned_slots`] and its
/// `replication_ops::replication_watchdog_dispatch` wrapper, which are
/// likewise driven by the (future) cluster scheduler rather than a
/// self-spawned timer.
pub fn register_sweep_schedule() -> SweepSchedule {
    SweepSchedule {
        interval_secs: DEFAULT_SWEEP_INTERVAL_SECS,
        stale_after_secs: DEFAULT_STALE_AFTER_SECS,
    }
}

/// Cadence descriptor returned by [`register_sweep_schedule`]. Carries
/// the policy the (future) control-plane scheduler should apply when it
/// fires [`sweep_orphan_running_rows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepSchedule {
    /// How often the scheduler should run a sweep pass.
    pub interval_secs: i64,
    /// Staleness grace to pass to [`sweep_orphan_running_rows`].
    pub stale_after_secs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_reason_is_stable_contract_string() {
        // Operators grep `error = 'orphan_running_row_swept'`; a rename
        // would silently break runbooks. Pin the literal.
        assert_eq!(SWEEP_REASON, "orphan_running_row_swept");
    }

    #[test]
    fn register_sweep_schedule_returns_q4_recommended_cadence() {
        let sched = register_sweep_schedule();
        // §18 Q4: 60s sweep cadence.
        assert_eq!(sched.interval_secs, 60);
        // 5 min staleness default — strictly more conservative than the
        // 50s (5× heartbeat) floor Q4 mentions.
        assert_eq!(sched.stale_after_secs, 300);
        assert_eq!(sched.stale_after_secs, DEFAULT_STALE_AFTER_SECS);
    }

    #[test]
    fn validate_app_id_rejects_injection() {
        assert!(validate_app_id("app_demo").is_ok());
        assert!(validate_app_id("app-123").is_ok());
        assert!(validate_app_id("a\"; DROP SCHEMA x; --").is_err());
        assert!(validate_app_id("").is_err());
    }

    #[test]
    fn migration_lock_keys_match_run_acquisition_shape() {
        // The sweeper's liveness check MUST derive the identical
        // `(key1, key2)` the run held, or it would see every row as
        // orphaned. Pin the shape against the `LockScope::migration`
        // contract (`{app_id}:mig:{name}` / `mig:{name}`).
        let scope = LockScope::migration("app_demo", "backfill_users");
        let (k1, k2) = scope.to_keys();
        assert_eq!(k1, "app_demo:mig:backfill_users");
        assert_eq!(k2, "mig:backfill_users");
    }

    /// F1 warn-shape contract pin. The DB transition is tested in
    /// `tests/integration.rs` (`sweeper_transitions_stale_running_row_to_failed`);
    /// here we pin the operator-grep FIELD shape without a live PG by
    /// driving the extracted [`emit_swept_warn`] under the
    /// `tracing-subscriber` capture layer. A field rename (the r12 drift
    /// that renamed `audit_err`→`error` in the warn-half) would fail
    /// this test pre-commit.
    #[test]
    fn sweeper_emits_f1_shape_warn() {
        use crate::test_support::capture;

        let ((), events) = capture(|| {
            emit_swept_warn("app_demo", "backfill_users", "users", 42);
        });

        assert_eq!(events.len(), 1, "expected exactly one warn event");
        let ev = &events[0];
        assert_eq!(ev.level, tracing::Level::WARN);
        // F1 field-shape contract: `{app_id, name, transition}` + the
        // sweep-specific discriminators.
        assert_eq!(ev.fields.get("app_id").map(String::as_str), Some("app_demo"));
        assert_eq!(ev.fields.get("name").map(String::as_str), Some("backfill_users"));
        assert_eq!(ev.fields.get("collection").map(String::as_str), Some("users"));
        assert_eq!(ev.fields.get("audit_id").map(String::as_str), Some("42"));
        assert_eq!(ev.fields.get("transition").map(String::as_str), Some("failed"));
        assert_eq!(
            ev.fields.get("reason").map(String::as_str),
            Some("orphan_running_row_swept"),
            "metric/marker field must equal SWEEP_REASON"
        );
        assert_eq!(ev.fields.get("swept").map(String::as_str), Some("true"));
        assert!(
            ev.target.starts_with("zeroship_plugin_db::migration_sweeper"),
            "warn must originate from the sweeper module, got: {}",
            ev.target
        );
    }
}
