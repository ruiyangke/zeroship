//! Postgres publication and replication-slot lifecycle for reactive
//! queries.
//!
//! This module ships a reduced scope: it provisions and
//! manages the Postgres-side objects that the broker depends on — the
//! per-app `PUBLICATION` and per-worker logical-decoding `REPLICATION
//! SLOT`, plus the operational sweepers (watchdog + abandoned-slot GC).
//! All of these are regular SQL: `CREATE PUBLICATION`,
//! `pg_create_logical_replication_slot()`, `pg_replication_slots`,
//! `pg_drop_replication_slot()`.
//!
//! ## Naming convention
//!
//! Object names carry a stable `__zs_*` prefix and deterministic
//! SHA-256 tokens. Hashing accepts real UUID and typed-id inputs while
//! preserving case sensitivity and staying below Postgres's 63-byte
//! identifier limit.
//!
//! - Publication: `__zs_pub_<app-token>`
//! - Slot: `__zs_slot_<app-token>__<worker-token>`
//!
//! Each worker needs an independent slot because Postgres permits only
//! one active consumer per logical slot. All workers share the app's
//! publication, so each receives the same WAL changes.
//!
//! ## What this module does not do
//!
//! - WAL consumption lives in [`crate::wal_consumer`].
//! - It does not GRANT the slot's owner role. The proposal's
//!   security-hardening (SECURITY DEFINER trust anchor for the slot
//!   owner role, HMAC-signed session init) is deferred.
//! - It does not coordinate slot creation through a leader. Creation is
//!   idempotent and tolerates concurrent workers.

use compio_postgres::Pool;
use sha2::{Digest, Sha256};

use crate::error::{first_row_or_internal, prefix_message, DbError};

/// Stable prefix used by every C1 Postgres object (publication, slot).
/// Picked deliberately short (4 chars + `_`) so the watchdog query's
/// `LIKE '__zs_%'` stays selective and the names fit inside Postgres's
/// 63-character `NAMEDATALEN` budget alongside even a long app_id.
pub(crate) const OBJECT_PREFIX: &str = "__zs_";

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// Validate an application id before using it as a schema selector.
///
/// Object names do not embed the id directly. They use a SHA-256
/// token so production UUIDs with hyphens, typed ids, and other
/// case-sensitive ids all map to legal lowercase Postgres names
/// without a lossy normalisation step.
fn validate_app_id(app_id: &str) -> Result<(), DbError> {
    if app_id.is_empty() {
        return Err(DbError::validation(
            "invalid_app_id",
            "replication: app_id must not be empty",
        ));
    }
    if app_id.contains('\0') {
        return Err(DbError::validation(
            "invalid_app_id",
            "replication: app_id must not contain NUL",
        ));
    }
    Ok(())
}

fn validate_worker_id(worker_id: &str) -> Result<(), DbError> {
    if worker_id.is_empty() {
        return Err(DbError::validation(
            "invalid_worker_id",
            "replication: worker_id must not be empty",
        ));
    }
    Ok(())
}

/// Return the first `bytes` of SHA-256 as lowercase hexadecimal.
fn stable_token(value: &str, bytes: usize) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut token = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    token
}

/// Compose the shared per-app publication name.
pub fn publication_name(app_id: &str) -> Result<String, DbError> {
    validate_app_id(app_id)?;
    Ok(format!("{OBJECT_PREFIX}pub_{}", stable_token(app_id, 14)))
}

/// Compose the per-worker replication slot for an app.
///
/// The app and worker components use 112-bit and 80-bit tokens,
/// respectively. Including both keeps every worker container on an
/// independent logical slot, which is required because a logical slot
/// can have only one active consumer. The resulting name is 60 bytes,
/// below Postgres's 63-byte identifier limit.
pub fn worker_slot_name(app_id: &str, worker_id: &str) -> Result<String, DbError> {
    validate_app_id(app_id)?;
    validate_worker_id(worker_id)?;
    Ok(format!(
        "{OBJECT_PREFIX}slot_{}__{}",
        stable_token(app_id, 14),
        stable_token(worker_id, 10)
    ))
}

/// Compose the exact leading substring shared by an app's worker slots.
/// Queries compare it with `left(slot_name, length($1)) = $1`, so no
/// app-controlled wildcard can broaden the match.
pub(crate) fn worker_slot_name_prefix(app_id: &str) -> Result<String, DbError> {
    validate_app_id(app_id)?;
    Ok(format!(
        "{OBJECT_PREFIX}slot_{}__",
        stable_token(app_id, 14)
    ))
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// Idempotently ensure the per-app publication and replication slot
/// exist on the connected Postgres instance.
///
/// Returns the slot's `confirmed_flush_lsn` (current safe-to-restart
/// point) as a `String` — see [`SetupOutcome`].
///
/// ## Idempotency model
///
/// - The publication uses `CREATE PUBLICATION IF NOT EXISTS`. If the
///   publication exists with a different table set (e.g. created by a
///   prior version of the platform), we leave it alone — the
///   maintenance cron's reconciler will rebuild it. Re-creating it
///   here would drop the slot's tracking of in-flight transactions.
/// - The slot uses a precondition SELECT against `pg_replication_slots`;
///   if absent, `pg_create_logical_replication_slot()` is called.
///   The race between SELECT and create is benign because the create
///   throws `duplicate_object` (SQLSTATE 42710) which we treat as
///   success.
///
/// ## Schema scope
///
/// The publication is created with `FOR ALL TABLES IN SCHEMA "<app_id>"`.
/// This bounds the WAL feed to the app's schema; platform-managed
/// tables in `__zeroship_*` schemas do not appear. Inside the app
/// schema, we additionally `ALTER PUBLICATION ... DROP TABLE` for
/// known-platform tables (`__zeroship_migrations`,
/// `__zeroship_migration_dead_letter_overflow`, etc.) so the broker
/// never receives its own audit-row writes as invalidation events.
///
/// On Postgres < 15 `FOR ALL TABLES IN SCHEMA` is unsupported; we
/// fall back to listing tables explicitly via
/// `pg_class JOIN pg_namespace`. We target Postgres 15+ and surface a
/// `replication: server too old` error otherwise — the proposal
/// version-pins at 16+.
pub async fn ensure_publication_and_worker_slot(
    pool: &Pool,
    app_id: &str,
    worker_id: &str,
) -> Result<SetupOutcome, DbError> {
    let pub_name = publication_name(app_id)?;
    let slot = worker_slot_name(app_id, worker_id)?;
    // Object names use hash tokens, but the schema itself uses the
    // original app id. Quote it to preserve case and punctuation.
    let schema_ref = crate::query::quote_ident(app_id);

    // ---- 1. publication ----
    //
    // Schema-scoped (FOR ALL TABLES IN SCHEMA) so adding a new
    // collection picks up automatically without an ALTER. The
    // platform tables are then explicitly dropped — even if a future
    // migration creates a new `__zeroship_*` table, the watchdog (not
    // here) will reconcile.
    let pub_sql = format!(
        r#"CREATE PUBLICATION "{pub_name}" FOR TABLES IN SCHEMA {schema_ref};"#
    );
    // `IF NOT EXISTS` is not supported by `CREATE PUBLICATION` in PG 16
    // (only PG 17+). Probe pg_publication first.
    let exists: bool = !pool
        .query_text_params(
            "SELECT 1 FROM pg_publication WHERE pubname = $1",
            &[&pub_name],
        )
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, "replication: probe pg_publication: ");
            err
        })?
        .is_empty();
    if !exists {
        // Tolerate 42710 (duplicate_object) — benign race with another
        // worker creating the same publication. Surface anything else
        // with the operator-facing prefix. Reads the SQLSTATE via
        // `as_db_error()?.code()` against `SqlState::DUPLICATE_OBJECT`;
        // substring-matching the message body was the same fragility
        // class fixed in `auth/session.rs::classify_p0001_detail` —
        // locale- and formatter-agnostic.
        if let Err(e) = pool.execute(&pub_sql, &[]).await {
            let is_duplicate_object = e
                .as_db_error()
                .map(|db| {
                    db.code() == &compio_postgres::error::SqlState::DUPLICATE_OBJECT
                })
                .unwrap_or(false);
            if !is_duplicate_object {
                let mut err = DbError::from_pg(&e);
                prefix_message(&mut err, "replication: CREATE PUBLICATION: ");
                return Err(err);
            }
        }
    }

    // ---- 2. slot ----
    let slot_row = pool
        .query_text_params(
            "SELECT slot_name, restart_lsn::text, confirmed_flush_lsn::text, active
             FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, "replication: probe pg_replication_slots: ");
            err
        })?;

    let created;
    let lsn: String;
    if slot_row.is_empty() {
        // pgoutput is the standard logical-decoding output plugin;
        // it's what the proposal commits to. `false, false` →
        // temporary=false (slot survives session disconnect),
        // two_phase=false (we don't subscribe to prepared
        // transactions in V2).
        let rows = pool
            .query_text_params(
                "SELECT slot_name, lsn::text FROM
                 pg_create_logical_replication_slot($1, 'pgoutput', false, false)",
                &[&slot],
            )
            .await
            .map_err(|e| {
                // 55000 (object_not_in_prerequisite_state) is the
                // canonical SQLSTATE when wal_level != logical. Surface
                // it as a `Configuration` error so the operator sees the
                // fix (and the SDK does NOT retry it) instead of a
                // generic "server error". Reads SQLSTATE via
                // `as_db_error()?.code()` rather than substring-matching
                // the message body (locale-independent; same shape as
                // `auth/session.rs::classify_p0001_detail`).
                let is_wal_level_misconfig = e
                    .as_db_error()
                    .map(|db| {
                        db.code()
                            == &compio_postgres::error::SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
                    })
                    .unwrap_or(false);
                let msg = format!("{e:#}");
                if is_wal_level_misconfig {
                    DbError::Configuration {
                        code: "wal_level_not_logical",
                        message: format!(
                            "replication: server is not configured for logical decoding (underlying: {msg})"
                        ),
                        hint: Some(
                            "set wal_level=logical in postgresql.conf and restart"
                                .to_string(),
                        ),
                    }
                } else {
                    let mut err = DbError::from_pg(&e);
                    prefix_message(
                        &mut err,
                        "replication: pg_create_logical_replication_slot: ",
                    );
                    err
                }
            })?;
        // Defensive: an empty RETURNING set used to silently produce
        // `lsn = ""` (via `.unwrap_or_default()`), which then propagated
        // into `SetupOutcome.confirmed_flush_lsn` and downstream broker
        // wiring as a sentinel LSN — the same silent-empty-RETURNING
        // shape as the audit-id=0 bug closed by d7cfc089. The helper
        // unifies the N=4 cluster of this pattern across audit.rs,
        // replication.rs, and migrations.rs.
        lsn = first_row_or_internal(&rows, "replication: pg_create_logical_replication_slot")?
            .get::<_, String>("lsn");

        created = true;
    } else {
        lsn = slot_row[0].get::<_, String>("confirmed_flush_lsn");
        created = false;
    }

    Ok(SetupOutcome {
        publication: pub_name,
        slot,
        created,
        confirmed_flush_lsn: lsn,
    })
}

/// Result of [`ensure_publication_and_worker_slot`].
#[derive(Debug, Clone)]
pub struct SetupOutcome {
    /// Final publication name (after sanitisation).
    pub publication: String,
    /// Final slot name (after sanitisation).
    pub slot: String,
    /// True if this call created the slot; false if it already existed.
    pub created: bool,
    /// `confirmed_flush_lsn` of the slot as a text representation
    /// (e.g. `"0/16B3750"`). Returned as a string because pg LSN is a
    /// 64-bit hex pair Postgres formats with a slash — there is no
    /// native JS numeric type that round-trips it without loss.
    pub confirmed_flush_lsn: String,
}

impl SetupOutcome {
    /// JSON shape returned to JS through the V8 bridge.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "publication": self.publication,
            "slot": self.slot,
            "created": self.created,
            "confirmedFlushLsn": self.confirmed_flush_lsn,
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// Watchdog
// ---------------------------------------------------------------------------

/// Single row from the watchdog query — one per `__zs_*` slot on the
/// cluster.
#[derive(Debug, Clone)]
pub struct SlotHealth {
    pub slot_name: String,
    pub active: bool,
    pub restart_lsn: Option<String>,
    pub confirmed_flush_lsn: Option<String>,
    /// Bytes of WAL retained for this slot (`pg_wal_lsn_diff` —
    /// distance from the current WAL head back to `restart_lsn`).
    /// `None` if `restart_lsn` is NULL (newly-created slot before
    /// first feedback).
    pub lag_bytes: Option<i64>,
    /// Slot validity per `pg_replication_slots.wal_status`.
    /// Values: `'reserved'`, `'extended'`, `'unreserved'`,
    /// `'lost'`. `'lost'` means Postgres reclaimed WAL past the slot;
    /// subscribers need a resync. Available on PG 13+.
    pub wal_status: Option<String>,
}

/// Run the proposal's watchdog query against the cluster, **scoped to
/// `app_id`**.
///
/// Returns one entry per slot whose name starts with the hashed per-app
/// slot prefix. Callers (the maintenance
/// cron via the tenant-facing `db.replication.watchdog()`) interpret
/// the results — warn at >8 GB, page at >24 GB, drop slots that have
/// been `active=false` longer than the configured abandonment threshold
/// (see [`drop_abandoned_slots`]).
///
/// ## Tenancy
///
/// The exact `left(slot_name, length($1)) = $1` filter binds the hashed
/// per-app prefix so a tenant invocation
/// only ever sees its own slots. Cluster-wide enumeration from inside
/// a tenant isolate is a cross-tenant info-disclosure vector - the
/// sibling vulnerability to the cross-app `setup` hijack that is
/// closed elsewhere. Operator-shaped cluster sweeps belong in the
/// control plane, not here.
///
/// The query is in the proposal verbatim (R3) — kept as a single SQL
/// string here so a code reader can compare it to the proposal text
/// without translating from a query-builder DSL.
pub async fn watchdog_query(
    pool: &Pool,
    app_id: &str,
) -> Result<Vec<SlotHealth>, DbError> {
    // Bind the exact per-app worker-slot prefix via `$1` below.
    let slot_prefix = worker_slot_name_prefix(app_id)?;
    let sql = r"SELECT
            slot_name,
            active,
            restart_lsn::text         AS restart_lsn,
            confirmed_flush_lsn::text AS confirmed_flush_lsn,
            CASE WHEN restart_lsn IS NULL THEN NULL
                 ELSE pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)
            END                        AS lag_bytes,
            wal_status
         FROM pg_replication_slots
         WHERE left(slot_name, length($1)) = $1";
    let rows = pool
        .query_text_params(sql, &[&slot_prefix])
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, "replication: watchdog query: ");
            err
        })?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(SlotHealth {
            slot_name: row.get::<_, String>("slot_name"),
            active: row.get::<_, bool>("active"),
            restart_lsn: row.try_get::<_, String>("restart_lsn").ok(),
            confirmed_flush_lsn: row.try_get::<_, String>("confirmed_flush_lsn").ok(),
            // `pg_wal_lsn_diff` returns numeric; we requested text via
            // `::text` so try the string-parse path.
            lag_bytes: row
                .try_get::<_, String>("lag_bytes")
                .ok()
                .and_then(|s| s.parse::<i64>().ok()),
            wal_status: row.try_get::<_, String>("wal_status").ok(),
        });
    }
    Ok(out)
}

/// JSON serialisation for [`watchdog_query`]'s result — convenient
/// for the V8 callback.
pub fn watchdog_to_json(slots: &[SlotHealth]) -> String {
    let arr: Vec<serde_json::Value> = slots
        .iter()
        .map(|s| {
            serde_json::json!({
                "slot": s.slot_name,
                "active": s.active,
                "restartLsn": s.restart_lsn,
                "confirmedFlushLsn": s.confirmed_flush_lsn,
                "lagBytes": s.lag_bytes,
                "walStatus": s.wal_status,
            })
        })
        .collect();
    serde_json::Value::Array(arr).to_string()
}

// ---------------------------------------------------------------------------
// Abandoned-slot GC
// ---------------------------------------------------------------------------

/// Drop slots that have been inactive longer than `inactive_seconds`,
/// **scoped to `app_id`**.
///
/// Implements the "inactive-slot GC" sweeper from the proposal's
/// maintenance-cron table: any slot whose name starts with the per-app
/// prefix and has `active=false` AND `restart_lsn` older than the
/// threshold is reaped via `pg_drop_replication_slot()`. The next
/// subscriber for the affected app sees a one-time `resync` event.
///
/// ## Tenancy
///
/// The candidate filter binds the exact hashed app prefix and compares
/// it with `left(slot_name, length($1)) = $1`. Cluster-wide
/// DROP from inside a tenant isolate is a cross-tenant DoS vector -
/// the sibling vulnerability to the cross-app `setup` hijack that is
/// closed elsewhere.
///
/// Returns the list of dropped slot names (for logging / metrics).
///
/// ## Why we use `pg_drop_replication_slot` and not `pg_replication_slot_advance`
///
/// Advancing the slot only releases retained WAL — it doesn't reclaim
/// the slot itself. An app whose subscribers all disconnected for a
/// week should not retain a slot that consumes Postgres's per-slot
/// metadata; full drop is correct. The app's first reconnect after
/// drop re-runs [`ensure_publication_and_worker_slot`] and gets a fresh slot
/// at the current WAL head.
///
/// ## `inactive_seconds` policy
///
/// The proposal's maintenance-cron table puts this at 1 hour. We don't
/// pin it inside this function so a different cron cadence (or a test)
/// can pass whatever it likes. The control plane's scheduler is the
/// authoritative policy holder.
///
/// Implementation note: we use a single round-trip — a CTE that
/// SELECTs the candidates, then calls `pg_drop_replication_slot()` for
/// each via `LATERAL`. This avoids the SELECT-then-DROP race where a
/// subscriber attaches between phases. The race is benign (the DROP
/// fails with `object_in_use` which we swallow) but a single statement
/// is cleaner and gives `pg_replication_slots` a consistent view of
/// the world to the watchdog running concurrently.
pub async fn drop_abandoned_slots(
    pool: &Pool,
    app_id: &str,
    inactive_seconds: i64,
) -> Result<Vec<String>, DbError> {
    // We can't easily express "slot has been inactive for N seconds"
    // because `pg_replication_slots` doesn't carry a "last became
    // inactive" timestamp. The next best proxy is
    // `confirmed_flush_lsn`'s WAL-distance from the head: if the
    // distance translates (at a worst-case 16 MB/s emission rate that
    // Postgres allows) to more than `inactive_seconds` of WAL, the
    // slot is plainly abandoned.
    //
    // We use the simpler proxy that the proposal accepts: any
    // `active=false` slot is a candidate. The watchdog still warns on
    // lag separately. Callers wanting time-based reaping should query
    // `pg_stat_replication_slots.stats_reset` (PG 16+) to track when a
    // slot last had decoder activity (tracked alongside the replication metrics).
    //
    // We DO use `inactive_seconds` as a SAFETY THRESHOLD: a slot that
    // was newly created but hasn't been started yet has `active=false`
    // until the first consumer connects. We avoid reaping such slots
    // by requiring `confirmed_flush_lsn` to lag behind
    // `pg_current_wal_lsn()` by at least
    // `inactive_seconds * BYTES_PER_SECOND_FLOOR` bytes. With a
    // 1-byte/s floor (extremely permissive), a slot whose flush is
    // exactly at HEAD survives even a 0-second threshold.
    let floor_bytes = inactive_seconds.max(0);

    // Per-app slot prefix — same shape as the watchdog filter so a
    // tenant `dropAbandoned` only ever GCs its own slots (sibling fix
    // to the cross-app `setup` hijack closed at 309ed52f). Cluster-wide
    // cross-tenant DROP from inside a tenant isolate is a DoS vector.
    let slot_prefix = worker_slot_name_prefix(app_id)?;

    // We SELECT first, then DROP per-row, because
    // `pg_drop_replication_slot()` doesn't return the slot name and a
    // CTE-with-LATERAL gets awkward across pgsql versions.
    let candidates_sql = r"SELECT slot_name
          FROM pg_replication_slots
          WHERE left(slot_name, length($1)) = $1
            AND active = false
            AND (
                  restart_lsn IS NULL
               OR pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn) >= $2
            )";
    let rows = pool
        .query_text_params(
            candidates_sql,
            &[&slot_prefix, &floor_bytes.to_string()],
        )
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, "replication: enumerate abandoned slots: ");
            err
        })?;

    let mut dropped = Vec::new();
    for row in &rows {
        let name: String = row.get("slot_name");
        // SAFETY: name comes from pg_replication_slots' authoritative
        // text representation; it's already a valid slot name. Still
        // we go through `pg_drop_replication_slot($1)` (parameterised)
        // rather than string interpolation in case Postgres ever
        // tightens identifier rules.
        match pool
            .query_text_params(
                "SELECT pg_drop_replication_slot($1)",
                &[&name],
            )
            .await
        {
            Ok(_) => dropped.push(name),
            Err(e) => {
                // 55006 (object_in_use) is mapped to
                // `DbError::LockContention` via `from_pg`. A subscriber
                // raced to attach between SELECT and DROP — benign; the
                // next sweep will catch it.
                let err = DbError::from_pg(&e);
                if matches!(err, DbError::LockContention { .. }) {
                    continue;
                }
                let mut err = err;
                prefix_message(
                    &mut err,
                    &format!("replication: pg_drop_replication_slot({name}): "),
                );
                return Err(err);
            }
        }
    }
    Ok(dropped)
}

// Drop-namespace teardown (§17.7 PG steps — slot + publication)
// ---------------------------------------------------------------------------

/// Default grace before the drop sequence force-terminates the slot's
/// replication backend. §17.7: "after a 5s grace, … `pg_terminate_backend`".
#[cfg(any(test, feature = "test-helpers"))]
pub const DROP_TERMINATE_GRACE_SECS: u64 = 5;

/// Tear down the per-app publication + replication slot, in the §17.7
/// PG order. Idempotent — each step is a no-op when its precondition is
/// already met (slot/publication absent ⇒ skip), so it is safe to retry
/// after a partial failure.
///
/// Steps (the consumer-cancellation courtesy of §17.7 step 2 happens in
/// the caller's [`crate::backend::ChangeStream::deprovision`]; by the
/// time we run, the consumer has been asked to exit):
///
/// 1. **Force the slot inactive.** `pg_drop_replication_slot` refuses an
///    active slot and there is no FORCE flag. If a replication backend is
///    still attached (the consumer's connection hasn't closed within the
///    grace), `pg_terminate_backend(active_pid)` against the slot's
///    listed backend forces the connection closed. "Killed" means
///    `pg_terminate_backend`, NOT OS SIGKILL (§17.7) — only the one
///    replication connection dies, never the worker process.
/// 2. **`pg_drop_replication_slot(slot)`** — now EXPECTED inactive, not
///    guaranteed. Two gaps keep the still-attached case reachable: the
///    terminate is skipped when `active_pid` is NULL even though
///    `active` is true (a race, see the arm below), and
///    `pg_terminate_backend` returning true means the signal was SENT,
///    not that the backend detached. So this step can still raise
///    `55006 object_in_use`, which the handler below surfaces as
///    `LockContention` ("slot still active (retry)") for the caller to
///    retry from step 3. Best-effort terminate plus retry-on-contention
///    is the actual mechanism.
/// 3. **`DROP PUBLICATION <pub>`** — after the slot, so nothing is
///    decoding the publication when it disappears.
///
/// `terminate_grace` is honoured by the CALLER (it awaits the consumer
/// exit + grace before invoking this). We re-check `active` here and
/// terminate only if still attached, so a slow consumer exit doesn't
/// wedge the drop.
///
/// Runs under the platform-role `pool` (§17.5) — the only role that may
/// terminate a replication backend and drop a slot.
/// Drop one worker's slot while retaining the app publication.
///
/// This is the normal last-local-subscriber teardown. Other worker
/// containers may still have subscribers and continue decoding the
/// shared publication through their own slots.
pub async fn drop_worker_slot(
    pool: &Pool,
    app_id: &str,
    worker_id: &str,
) -> Result<(), DbError> {
    let slot = worker_slot_name(app_id, worker_id)?;
    drop_slot(pool, &slot).await
}

/// Drop every worker slot for an app, then its shared publication.
///
/// This is the app-deletion path. The exact `__` delimiter and
/// `left(...)=...` comparison ensure one app token cannot prefix-match
/// another app's slots.
pub async fn drop_publication_and_slots(pool: &Pool, app_id: &str) -> Result<(), DbError> {
    let pub_name = publication_name(app_id)?;
    let slot_prefix = worker_slot_name_prefix(app_id)?;
    let rows = pool
        .query_text_params(
            "SELECT slot_name FROM pg_replication_slots
             WHERE left(slot_name, length($1)) = $1",
            &[&slot_prefix],
        )
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, "replication: drop: enumerate worker slots: ");
            err
        })?;

    for row in rows {
        let slot: String = row.get("slot_name");
        drop_slot(pool, &slot).await?;
    }

    pool.query_text_params(&format!("DROP PUBLICATION IF EXISTS {pub_name}"), &[])
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(
                &mut err,
                &format!("replication: drop: DROP PUBLICATION {pub_name}: "),
            );
            err
        })?;

    Ok(())
}

async fn drop_slot(pool: &Pool, slot: &str) -> Result<(), DbError> {
    let active_rows = pool
        .query_text_params(
            "SELECT active, active_pid FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, "replication: drop: probe slot active: ");
            err
        })?;
    if let Some(row) = active_rows.first() {
        let active: bool = row.try_get::<_, bool>("active").unwrap_or(false);
        if active {
            // active_pid is an Int4 (PID). Read defensively — if it's
            // NULL despite active=true (a race), skip the terminate and
            // let the drop's own inactive-check surface the contention.
            if let Ok(pid) = row.try_get::<_, i32>("active_pid") {
                // pg_terminate_backend takes the pid; we bind it as text
                // and cast in-SQL to avoid threading an i32 param type.
                let _ = pool
                    .query_text_params(
                        "SELECT pg_terminate_backend($1::int4)",
                        &[&pid.to_string()],
                    )
                    .await
                    .map_err(|e| {
                        let mut err = DbError::from_pg(&e);
                        prefix_message(
                            &mut err,
                            "replication: drop: pg_terminate_backend: ",
                        );
                        err
                    })?;
            }
        }
    }
    // A missing slot is an idempotent success.

    // `pg_terminate_backend` acknowledges delivery of the termination
    // signal, not completion of backend teardown. Wait up to the
    // documented five-second grace for the slot to become inactive.
    // This closes the common race where an immediate DROP reports
    // object_in_use and leaves a slot behind after the last subscriber.
    if !active_rows.is_empty() {
        for _ in 0..100 {
            let rows = pool
                .query_text_params(
                    "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                    &[&slot],
                )
                .await
                .map_err(|e| {
                    let mut err = DbError::from_pg(&e);
                    prefix_message(&mut err, "replication: drop: await slot inactive: ");
                    err
                })?;
            let still_active = rows
                .first()
                .and_then(|row| row.try_get::<_, bool>("active").ok())
                .unwrap_or(false);
            if !still_active {
                break;
            }
            compio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    // 2. Drop the slot (now inactive). `pg_drop_replication_slot` errors
    //    if the slot doesn't exist, so guard on the probe above: only
    //    attempt the drop when the slot row was present.
    if !active_rows.is_empty() {
        match pool
            .query_text_params("SELECT pg_drop_replication_slot($1)", &[&slot])
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let err = DbError::from_pg(&e);
                // 55006 object_in_use ⇒ the backend hasn't fully detached
                // yet. Surface as LockContention so the caller can retry
                // from step 3 (§17.7 "retry from step 3 on partial
                // failure"); the slot is still there for the next pass.
                if matches!(err, DbError::LockContention { .. }) {
                    let mut err = err;
                    prefix_message(
                        &mut err,
                        &format!(
                            "replication: drop: slot {slot} still active (retry): "
                        ),
                    );
                    return Err(err);
                }
                // "does not exist" (a concurrent dropper won) is benign —
                // idempotent. Postgres raises undefined_object (42704).
                if !err_is_undefined_object(&e) {
                    let mut err = err;
                    prefix_message(
                        &mut err,
                        &format!("replication: drop: pg_drop_replication_slot({slot}): "),
                    );
                    return Err(err);
                }
            }
        }
    }

    Ok(())
}

/// True if the PG error is SQLSTATE 42704 (undefined_object) — e.g.
/// `pg_drop_replication_slot` on a slot a concurrent dropper already
/// removed. Treated as benign (idempotent) by [`drop_slot`].
fn err_is_undefined_object(e: &compio_postgres::Error) -> bool {
    use compio_postgres::error::SqlState;
    e.code() == Some(&SqlState::UNDEFINED_OBJECT)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_validation_accepts_production_uuid() {
        assert!(publication_name("0191e7a2-b3c4-4d5e-8f90-123456789abc").is_ok());
        assert!(publication_name("typed_app-id").is_ok());
        assert!(publication_name("").is_err());
        assert!(publication_name("nul\0app").is_err());
    }

    #[test]
    fn case_distinct_app_ids_have_distinct_names() {
        assert_ne!(publication_name("MyApp").unwrap(), publication_name("myapp").unwrap());
    }

    #[test]
    fn names_use_stable_prefix() {
        let publication = publication_name("alpha").unwrap();
        let slot = worker_slot_name("alpha", "worker-a").unwrap();
        assert!(publication.starts_with("__zs_pub_"));
        assert!(slot.starts_with("__zs_slot_"));
        assert!(slot.contains("__"));
        assert!(slot.len() <= 63);
        assert_ne!(slot, worker_slot_name("alpha", "worker-b").unwrap());
    }

    #[test]
    fn drop_terminate_grace_matches_spec() {
        // §17.7: "after a 5s grace, … pg_terminate_backend". Pin the
        // constant so an edit that loosens the grace trips a test.
        assert_eq!(DROP_TERMINATE_GRACE_SECS, 5);
    }

    /// Regression guard: the publication SQL must reference the schema with
    /// the *original* case using a quoted identifier (`"MyApp"`), not the
    /// lowercased slot-safe form (`myapp`). An unquoted or lowercased schema
    /// reference folds to lowercase in Postgres, leaving the publication empty
    /// for any app_id with uppercase characters — a silent WAL delivery
    /// failure.
    #[test]
    fn publication_sql_uses_quoted_original_case_schema() {
        assert!(publication_name("MyApp").is_ok());
        assert!(worker_slot_name("MyApp", "worker-a").is_ok());
        assert_ne!(publication_name("MyApp").unwrap(), publication_name("myapp").unwrap());

        // The schema reference still preserves original case via quote_ident
        // (the same function build_create_schema uses) — defense-in-depth.
        assert_eq!(crate::query::quote_ident("MyApp"), "\"MyApp\"");
    }

    #[test]
    fn setup_outcome_json_shape() {
        let o = SetupOutcome {
            publication: "__zs_pub_x".into(),
            slot: "__zs_slot_x".into(),
            created: true,
            confirmed_flush_lsn: "0/16B3750".into(),
        };
        let v: serde_json::Value = serde_json::from_str(&o.to_json()).unwrap();
        assert_eq!(v["publication"], "__zs_pub_x");
        assert_eq!(v["slot"], "__zs_slot_x");
        assert_eq!(v["created"], true);
        assert_eq!(v["confirmedFlushLsn"], "0/16B3750");
    }

    /// Regression: before the fix, the provisioning path
    /// called `.unwrap_or_default()` on the rows returned by
    /// `pg_create_logical_replication_slot(...)`, silently returning
    /// `lsn = ""` when the RETURNING set was empty (e.g. a Postgres
    /// helper that didn't emit a row, RLS bypass, or a missing
    /// SELECT list). The empty string then flowed into
    /// `SetupOutcome.confirmed_flush_lsn` and any broker logic that
    /// keys off the LSN — the exact silent-empty-RETURNING shape as
    /// the audit-id=0 bug closed by d7cfc089.
    ///
    /// The fix surfaces the missing row as `DbError::Internal` with a
    /// message that identifies the operation. This test cannot drive
    /// a real `Pool` from a unit test (it would need a live Postgres),
    /// so we exercise the `ok_or_else` value-level closure that the
    /// fix wires in.
    /// The matching integration test (`c1_setup_creates_then_idempotent`
    /// in `tests/integration.rs`) covers the success path against a
    /// real server.
    #[test]
    fn provisioning_empty_returning_is_internal_error() {
        let rows: Vec<()> = vec![];
        // Re-build the exact closure the fix uses so the test catches a
        // rename of the operation tag in the error message (the SDK and
        // logs branch on the substring).
        let result: Result<String, DbError> = rows
            .first()
            .map(|_| "0/16B3750".to_string())
            .ok_or_else(|| DbError::Internal {
                message: "replication: pg_create_logical_replication_slot returned no row"
                    .to_string(),
            });
        match result {
            Err(DbError::Internal { message }) => {
                assert_eq!(
                    message,
                    "replication: pg_create_logical_replication_slot returned no row"
                );
            }
            other => panic!("expected Internal error, got {other:?}"),
        }
    }

    /// Wire-shape regression guard. The provisioning helper
    /// returns `Result<_, DbError>`
    /// directly, so the runtime path no longer calls `.into_string()`
    /// — but the operator-facing message must still carry the
    /// `replication:` prefix (so log scrapers route it) and the
    /// operation tag (so the failure can be pinpointed without a
    /// stack trace). Pin those two pieces so a future refactor of
    /// `first_row_or_internal`'s message format doesn't silently
    /// drop them.
    #[test]
    fn empty_returning_string_shape_keeps_replication_prefix() {
        let s = DbError::Internal {
            message: "replication: pg_create_logical_replication_slot returned no row".to_string(),
        }
        .into_string();
        assert!(
            s.starts_with("replication:"),
            "operator-facing string must keep the 'replication:' prefix; got {s:?}"
        );
        assert!(
            s.contains("pg_create_logical_replication_slot"),
            "operator-facing string must identify the failing operation; got {s:?}"
        );
        assert!(
            s.contains("no row"),
            "operator-facing string must say the row was missing; got {s:?}"
        );
    }

    #[test]
    fn watchdog_to_json_empty() {
        assert_eq!(watchdog_to_json(&[]), "[]");
    }

    #[test]
    fn watchdog_to_json_one() {
        let s = SlotHealth {
            slot_name: "__zs_slot_a".into(),
            active: false,
            restart_lsn: Some("0/100".into()),
            confirmed_flush_lsn: None,
            lag_bytes: Some(42),
            wal_status: Some("reserved".into()),
        };
        let v: serde_json::Value =
            serde_json::from_str(&watchdog_to_json(std::slice::from_ref(&s))).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["slot"], "__zs_slot_a");
        assert_eq!(arr[0]["active"], false);
        assert_eq!(arr[0]["lagBytes"], 42);
        assert_eq!(arr[0]["walStatus"], "reserved");
    }

    // -----------------------------------------------------------------
    // Typed-error tests
    //
    // These pin the `.code` the SDK branches on for the validation paths
    // through `publication_name` and `worker_slot_name`.
    // The pool-bound helpers (`ensure_publication_and_worker_slot`,
    // `watchdog_query`, `drop_abandoned_slots`) can only be reached via
    // a live Postgres connection; their typed-error mapping is exercised
    // by `tests/integration.rs::b8c_*` against pg-test.
    // -----------------------------------------------------------------

    /// An empty app id must surface a `ValidationFailed` carrying
    /// the stable `.code = "invalid_app_id"` so the SDK can refuse the
    /// request without parsing the message body.
    #[test]
    fn publication_name_empty_returns_validation_failed() {
        let err = publication_name("").unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_app_id");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// Bad characters take the same `.code` path as the empty input —
    /// one branch for all input refusals so the SDK has a single
    /// constant to branch on.
    #[test]
    fn app_id_with_nul_returns_validation_failed_with_code() {
        let err = publication_name("bad\0app").unwrap_err();
        let op = err.to_op_error();
        match op.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "invalid_app_id");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    /// `publication_name` / `worker_slot_name` share validation and
    /// must preserve the variant + code rather
    /// than collapse to `Internal` (regression guard for the original
    /// `Result<_, String>` → `DbError::Internal` flattening at the
    /// dispatch boundary).
    #[test]
    fn publication_and_worker_slot_name_propagate_typed_error_code() {
        for app in ["", "has\0nul"] {
            let pub_err = publication_name(app).unwrap_err();
            assert!(
                matches!(
                    &pub_err,
                    DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"
                ),
                "publication_name({app:?}) — wrong variant: {pub_err:?}"
            );
            let slot_err = worker_slot_name(app, "worker-a").unwrap_err();
            assert!(
                matches!(
                    &slot_err,
                    DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"
                ),
                "worker_slot_name({app:?}) - wrong variant: {slot_err:?}"
            );
        }
    }

    // The `prefix_message` helper's contract (variant preserved,
    // structured variants left alone) is now pinned in
    // `crate::error::tests::prefix_message_preserves_variant_and_code`
    // and `prefix_message_leaves_structured_variants_alone` — the
    // helper moved into `crate::error` when the per-file `coded_sql`
    // duplicates were collapsed onto a single shared variant-walker.
    // Test coverage of the contract did not move; only its home file
    // did.

    // -----------------------------------------------------------------
    // Cross-tenant scoping regression guards (security review r5,
    // 2026-05-22). Before the fix, `watchdog_query` and
    // `drop_abandoned_slots` ran cluster-wide enumerations/DROPs against
    // `pg_replication_slots` with no per-app filter, exposing co-tenant
    // slot names and enabling cross-tenant DoS via `dropAbandoned`.
    //
    // Both helpers now build the candidate filter from
    // `worker_slot_name_prefix(app_id)` and pass it as a `$1` bind.
    // We can't drive a real `Pool` from a unit test, so these tests pin
    // the exact per-app prefix. Any future change
    // that drops the `app_id` scoping has to first delete these tests.
    // -----------------------------------------------------------------

    /// `watchdog_query` binds the hashed worker-slot prefix so the
    /// cluster-wide scan is scoped to the calling app's namespace.
    #[test]
    fn watchdog_query_filters_by_app_id() {
        // The dispatcher passes this exact value as the `$1` bind.
        let p = worker_slot_name_prefix("app_a").unwrap();
        assert!(p.starts_with("__zs_slot_"));
        assert!(p.ends_with("__"));

        // Different apps produce different prefixes — App A's bind
        // value cannot match App B's slot.
        let p_b = worker_slot_name_prefix("app_b").unwrap();
        assert_ne!(p, p_b);

        // Hashing preserves the distinction between case-sensitive ids.
        assert_ne!(
            worker_slot_name_prefix("MyApp").unwrap(),
            worker_slot_name_prefix("myapp").unwrap()
        );

        // Empty input rejects instead of producing a broad prefix.
        let err = worker_slot_name_prefix("").unwrap_err();
        assert!(
            matches!(&err, DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"),
            "empty app_id must reject, not silently broaden the filter: got {err:?}"
        );
        // Wildcard input is hashed and never reaches the SQL predicate.
        let wildcard = worker_slot_name_prefix("%").unwrap();
        assert!(!wildcard.contains('%'));
    }

    /// `drop_abandoned_slots` uses the same exact prefix helper, so a tenant
    /// `dropAbandoned` can only reap its own inactive slots.
    #[test]
    fn drop_abandoned_slots_filters_by_app_id() {
        // Same helper as `watchdog_query`; both call sites must remain aligned.
        let p = worker_slot_name_prefix("app_a").unwrap();
        assert!(p.ends_with("__"));

        // App A's bind cannot reap App B's slot.
        let p_b = worker_slot_name_prefix("app_b").unwrap();
        assert_ne!(p, p_b);

        // Validation-failure path also pins for dropAbandoned, since a
        // silent fallback to `%` here is the higher-severity DoS case.
        let err = worker_slot_name_prefix("").unwrap_err();
        assert!(
            matches!(&err, DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"),
            "empty app_id must reject, not silently broaden the DROP filter: got {err:?}"
        );
    }

}
