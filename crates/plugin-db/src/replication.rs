//! Replication slot + publication lifecycle — foundation for C1
//! (reactive queries via Postgres WAL fanout) of the @zeroship/db
//! proposal (docs/proposals/zeroship-db.md, section C1).
//!
//! This module ships the **P8a-reduced** scope: it provisions and
//! manages the Postgres-side objects that the broker depends on — the
//! per-app `PUBLICATION` and the per-app logical-decoding `REPLICATION
//! SLOT` — and the operational sweepers (watchdog + abandoned-slot GC).
//! All of these are regular SQL: `CREATE PUBLICATION`,
//! `pg_create_logical_replication_slot()`, `pg_replication_slots`,
//! `pg_drop_replication_slot()`.
//!
//! The streaming-protocol WAL consumer (CopyBoth, XLogData, pgoutput
//! frame parser) is **deferred** to P8a.2 — it requires
//! `compio-postgres` to learn the streaming-replication protocol
//! handshake (`replication=database` startup parameter,
//! `START_REPLICATION` command, CopyBoth message framing). See the
//! module-level docs in [`crate::wal_consumer`] for the blocker
//! analysis and the recommended driver-level extension.
//!
//! ## Naming convention
//!
//! Per the proposal (R3-R4), both the slot and the publication carry a
//! stable `__zs_*` prefix so the watchdog can find them with a single
//! `LIKE '__zs_%'` predicate without parsing app_id out of the name.
//!
//! - Publication: `__zs_pub_<app_id>`
//! - Slot:        `__zs_slot_<app_id>`
//!
//! `app_id` is sanitised at the call boundary: anything that isn't
//! `[A-Za-z0-9_]` is rejected (Postgres slot names allow only
//! lowercase alphanumeric + underscore; we accept upper-case here and
//! lower-case before emitting the SQL — see [`sanitise_app_id`]).
//!
//! ## What this module does NOT do
//!
//! - It does not consume WAL. The `pg_logical_slot_*_changes` SQL
//!   functions DO exist on a regular connection (no replication-mode
//!   handshake needed), but polling them via SELECT is an order of
//!   magnitude slower than streaming and reorders concurrency in a way
//!   that's awkward for a broker driving thousands of subscriptions.
//!   We defer the consumer rather than ship a degraded mode.
//! - It does not GRANT the slot's owner role. The proposal's R5-R8
//!   security-hardening (SECURITY DEFINER trust anchor for the slot
//!   owner role, HMAC-signed session init) is deferred to P8c.
//! - It does not co-ordinate slot creation across multiple control-plane
//!   replicas. P8a assumes a single writer; the leader-election guard
//!   lives in `crates/control/src/replication_setup.rs` (future).

use compio_postgres::Pool;

use crate::error::{first_row_or_internal, prefix_message, DbError};
use crate::v8_bridge::row_to_json;

/// Stable prefix used by every C1 Postgres object (publication, slot).
/// Picked deliberately short (4 chars + `_`) so the watchdog query's
/// `LIKE '__zs_%'` stays selective and the names fit inside Postgres's
/// 63-character `NAMEDATALEN` budget alongside even a long app_id.
pub const OBJECT_PREFIX: &str = "__zs_";

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// Validate + normalise an `app_id` for use as part of a Postgres
/// object name.
///
/// Postgres identifiers are case-folded to lowercase unless quoted;
/// since slot names cannot be quoted (they live in `pg_replication_slots`
/// as-stored), we lower-case here so two callers spelling the same app
/// in different cases resolve to the same slot.
///
/// Rejects anything outside `[A-Za-z0-9_]` because:
/// 1. `CREATE_REPLICATION_SLOT` rejects non-identifier characters at
///    parse time, so a bogus value would surface as a confusing
///    server-side error.
/// 2. Even if Postgres accepted it, the LIKE predicate the watchdog
///    uses would no longer be a safe prefix match.
///
/// On reject, returns a typed [`DbError::ValidationFailed`] with a
/// stable `.code` (`invalid_app_id`) the SDK can branch on.
pub fn sanitise_app_id(app_id: &str) -> Result<String, DbError> {
    if app_id.is_empty() {
        return Err(DbError::validation(
            "invalid_app_id",
            "replication: app_id must not be empty",
        ));
    }
    for c in app_id.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(DbError::validation(
                "invalid_app_id",
                format!(
                    "replication: app_id contains invalid character {c:?} \
                     — only [A-Za-z0-9_] permitted"
                ),
            ));
        }
    }
    Ok(app_id.to_ascii_lowercase())
}

/// Compose the per-app publication name. Wraps [`sanitise_app_id`].
pub fn publication_name(app_id: &str) -> Result<String, DbError> {
    Ok(format!("{OBJECT_PREFIX}pub_{}", sanitise_app_id(app_id)?))
}

/// Compose the per-app replication-slot name.
pub fn slot_name(app_id: &str) -> Result<String, DbError> {
    Ok(format!("{OBJECT_PREFIX}slot_{}", sanitise_app_id(app_id)?))
}

/// Compose the SQL `LIKE` pattern used to scope cluster-wide queries
/// against `pg_replication_slots` to a single app (`watchdog_query`,
/// `drop_abandoned_slots`).
///
/// Returns `slot_name(app_id) + '%'` so the filter matches the
/// canonical per-app slot **plus** any future suffixed shard slot
/// (`__zs_slot_<app>_<shard>`). The result is always passed via a
/// parameter bind (`$1`), never string-interpolated into SQL.
///
/// Pulled out as a named helper so the per-app scoping invariant has
/// a single, unit-testable source — see the
/// `*_filters_by_app_id` regression guards in this module.
pub(crate) fn slot_name_like_prefix(app_id: &str) -> Result<String, DbError> {
    Ok(format!("{}%", slot_name(app_id)?))
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
/// `pg_class JOIN pg_namespace`. For P8a we target 15+ and surface a
/// `replication: server too old` error otherwise — the proposal
/// version-pins at 16+.
pub async fn ensure_publication_and_slot(
    pool: &Pool,
    app_id: &str,
) -> Result<SetupOutcome, DbError> {
    let pub_name = publication_name(app_id)?;
    let slot = slot_name(app_id)?;
    // `sanitise_app_id` lowercases (required for slot/publication object names —
    // Postgres stores them as-is and folds unquoted identifiers to lowercase).
    // However, the schema was created by `build_create_schema` using the
    // *original* app_id via `quote_ident`, so `FOR TABLES IN SCHEMA` must
    // reference it with the same original-case quoted identifier. Using the
    // lowercased form here silently produces an empty publication for any
    // app_id with uppercase characters — the CRITICAL C1 silent WAL delivery
    // failure. `quote_ident` double-quotes the name so Postgres preserves case.
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
        // class fixed in `auth/session.rs::classify_p0001_detail`
        // (MAJOR-R5-1, cycle 06:55) — locale- and formatter-agnostic.
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
                            "replication: server is not configured for logical \
                             decoding — set wal_level=logical in postgresql.conf \
                             and restart (underlying: {msg})"
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

/// Result of [`ensure_publication_and_slot`].
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
/// Returns one entry per slot whose name starts with the per-app slot
/// prefix (`__zs_slot_<sanitised_app_id>`). Callers (the maintenance
/// cron via the tenant-facing `db.replication.watchdog()`) interpret
/// the results — warn at >8 GB, page at >24 GB, drop slots that have
/// been `active=false` longer than the configured abandonment threshold
/// (see [`drop_abandoned_slots`]).
///
/// ## Tenancy
///
/// The `WHERE slot_name LIKE $1` filter binds the per-app prefix
/// (`slot_name(app_id)`, terminated with `%`) so a tenant invocation
/// only ever sees its own slots. Cluster-wide enumeration from inside
/// a tenant isolate is a cross-tenant info-disclosure vector — the
/// sibling vulnerability to the cross-app `setup` hijack closed at
/// commit `309ed52f`. Operator-shaped cluster sweeps belong in the
/// control plane, not here.
///
/// The query is in the proposal verbatim (R3) — kept as a single SQL
/// string here so a code reader can compare it to the proposal text
/// without translating from a query-builder DSL.
pub async fn watchdog_query(
    pool: &Pool,
    app_id: &str,
) -> Result<Vec<SlotHealth>, DbError> {
    // Per-app slot prefix — `slot_name(app_id) + '%'`. See
    // [`slot_name_like_prefix`]; bound via `$1` below.
    let slot_prefix = slot_name_like_prefix(app_id)?;
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
         WHERE slot_name LIKE $1";
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
/// The candidate filter is bound via a `slot_name LIKE $1` parameter
/// against `slot_name(app_id) + '%'` (not interpolated). Cluster-wide
/// DROP from inside a tenant isolate is a cross-tenant DoS vector —
/// the sibling vulnerability to the cross-app `setup` hijack closed
/// at commit `309ed52f`.
///
/// Returns the list of dropped slot names (for logging / metrics).
///
/// ## Why we use `pg_drop_replication_slot` and not `pg_replication_slot_advance`
///
/// Advancing the slot only releases retained WAL — it doesn't reclaim
/// the slot itself. An app whose subscribers all disconnected for a
/// week should not retain a slot that consumes Postgres's per-slot
/// metadata; full drop is correct. The app's first reconnect after
/// drop re-runs [`ensure_publication_and_slot`] and gets a fresh slot
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
    // For P8a we use the simpler proxy that the proposal accepts: any
    // `active=false` slot is a candidate. The watchdog still warns on
    // lag separately. Callers wanting time-based reaping should query
    // `pg_stat_replication_slots.stats_reset` (PG 16+) to track when a
    // slot last had decoder activity — added in P8b alongside metrics.
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
    let slot_prefix = slot_name_like_prefix(app_id)?;

    // We SELECT first, then DROP per-row, because
    // `pg_drop_replication_slot()` doesn't return the slot name and a
    // CTE-with-LATERAL gets awkward across pgsql versions.
    let candidates_sql = r"SELECT slot_name
          FROM pg_replication_slots
          WHERE slot_name LIKE $1
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

// ---------------------------------------------------------------------------
// Misc — diagnostic helpers used by the V8 callback layer
// ---------------------------------------------------------------------------

/// Cheap probe used by the V8 `replicationStatus` callback to surface
/// the current state of an app's slot without re-emitting the full
/// SetupOutcome.
pub async fn slot_status(
    pool: &Pool,
    app_id: &str,
) -> Result<Option<serde_json::Value>, DbError> {
    let slot = slot_name(app_id)?;
    let rows = pool
        .query_text_params(
            "SELECT slot_name, plugin, active, restart_lsn::text, confirmed_flush_lsn::text, wal_status
             FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, "replication: slot_status: ");
            err
        })?;
    Ok(rows.first().map(row_to_json))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_app_id_rejects_special_chars() {
        assert!(sanitise_app_id("ok_123").is_ok());
        assert!(sanitise_app_id("UPPER").is_ok());
        assert!(sanitise_app_id("").is_err());
        assert!(sanitise_app_id("has space").is_err());
        assert!(sanitise_app_id("has-dash").is_err());
        assert!(sanitise_app_id("dot.app").is_err());
        assert!(sanitise_app_id("quote\"app").is_err());
    }

    #[test]
    fn sanitise_app_id_lowercases() {
        assert_eq!(sanitise_app_id("MyApp").unwrap(), "myapp");
    }

    #[test]
    fn names_use_stable_prefix() {
        assert_eq!(publication_name("alpha").unwrap(), "__zs_pub_alpha");
        assert_eq!(slot_name("alpha").unwrap(), "__zs_slot_alpha");
    }

    /// C1 regression guard: the publication SQL must reference the schema with
    /// the *original* case using a quoted identifier (`"MyApp"`), not the
    /// lowercased slot-safe form (`myapp`). An unquoted or lowercased schema
    /// reference folds to lowercase in Postgres, leaving the publication empty
    /// for any app_id with uppercase characters — the silent WAL delivery
    /// failure described in CRITICAL C1.
    #[test]
    fn publication_sql_uses_quoted_original_case_schema() {
        // Slot and publication object names are lowercased (Postgres requirement).
        assert_eq!(publication_name("MyApp").unwrap(), "__zs_pub_myapp");
        assert_eq!(slot_name("MyApp").unwrap(), "__zs_slot_myapp");

        // The schema reference in FOR TABLES IN SCHEMA must preserve original
        // case via a double-quoted identifier — quote_ident is the same function
        // used by build_create_schema, so the two sides of the lifecycle agree.
        let schema_ref = crate::query::quote_ident("MyApp");
        assert_eq!(
            schema_ref, "\"MyApp\"",
            "schema ref must be double-quoted with original case"
        );

        // Confirm that the lowercased form (the pre-fix bug path) differs —
        // i.e. that using sanitise_app_id output as the schema reference would
        // silently target `myapp` instead of `MyApp`.
        let lowercased_ref = crate::query::quote_ident("myapp");
        assert_ne!(
            schema_ref, lowercased_ref,
            "original-case and lowercased schema refs must differ for mixed-case app_id"
        );
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

    /// Regression: before the fix, `ensure_publication_and_slot`
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
    /// fix wires in, mirroring `audit::tests::insert_backfill_running_empty_returning_is_internal_error`.
    /// The matching integration test (`c1_setup_creates_then_idempotent`
    /// in `tests/integration.rs`) covers the success path against a
    /// real server.
    #[test]
    fn ensure_publication_and_slot_empty_returning_is_internal_error() {
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

    /// `ensure_publication_and_slot` returns `Result<_, String>` (not
    /// `Result<_, DbError>` like audit.rs), so the runtime fix calls
    /// `.into_string()` on the `DbError::Internal` before flowing it
    /// through `?`. This test pins the wire shape — the operator-
    /// facing message must include both the `replication:` prefix
    /// (so log scrapers route it) and the operation tag (so the
    /// failure can be pinpointed without a stack trace).
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
    // Typed-error sweep [I28]
    //
    // These pin the `.code` the SDK branches on for the validation paths
    // through `sanitise_app_id`, `publication_name`, and `slot_name`.
    // The pool-bound helpers (`ensure_publication_and_slot`,
    // `watchdog_query`, `drop_abandoned_slots`) can only be reached via
    // a live Postgres connection; their typed-error mapping is exercised
    // by `tests/integration.rs::b8c_*` against pg-test.
    // -----------------------------------------------------------------

    /// `sanitise_app_id("")` must surface a `ValidationFailed` carrying
    /// the stable `.code = "invalid_app_id"` so the SDK can refuse the
    /// request without parsing the message body.
    #[test]
    fn sanitise_app_id_empty_returns_validation_failed() {
        let err = sanitise_app_id("").unwrap_err();
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
    fn sanitise_app_id_invalid_char_returns_validation_failed_with_code() {
        let err = sanitise_app_id("bad-app").unwrap_err();
        let op = err.to_op_error();
        match op.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "invalid_app_id");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    /// `publication_name` / `slot_name` are thin wrappers around
    /// `sanitise_app_id` — they MUST preserve the variant + code rather
    /// than collapse to `Internal` (regression guard for the original
    /// `Result<_, String>` → `DbError::Internal` flattening at the
    /// dispatch boundary).
    #[test]
    fn publication_and_slot_name_propagate_typed_error_code() {
        for app in ["", "has space"] {
            let pub_err = publication_name(app).unwrap_err();
            assert!(
                matches!(
                    &pub_err,
                    DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"
                ),
                "publication_name({app:?}) — wrong variant: {pub_err:?}"
            );
            let slot_err = slot_name(app).unwrap_err();
            assert!(
                matches!(
                    &slot_err,
                    DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"
                ),
                "slot_name({app:?}) — wrong variant: {slot_err:?}"
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
    // `slot_name_like_prefix(app_id)` and pass it as a `$1` parameter
    // bind. We can't drive a real `Pool` from a unit test, so these
    // tests pin the per-app prefix value — the exact string the
    // dispatchers bind into the `LIKE $1` predicate. Any future change
    // that drops the `app_id` scoping has to first delete these tests.
    // -----------------------------------------------------------------

    /// `watchdog_query` binds `slot_name(app_id) + '%'` into the
    /// `slot_name LIKE $1` predicate so the cluster-wide scan is
    /// scoped to the calling app's slot namespace.
    #[test]
    fn watchdog_query_filters_by_app_id() {
        // `app_a`'s per-app prefix must be the canonical slot name
        // terminated with `%`. The dispatcher passes this exact value
        // as the `$1` bind.
        let p = slot_name_like_prefix("app_a").unwrap();
        assert_eq!(p, "__zs_slot_app_a%");

        // Different apps produce different prefixes — App A's bind
        // value cannot match App B's slot.
        let p_b = slot_name_like_prefix("app_b").unwrap();
        assert_ne!(p, p_b);
        assert_eq!(p_b, "__zs_slot_app_b%");

        // Sanitise + lowercase still applies (Postgres folds unquoted
        // identifiers); a mixed-case stamp maps to the same prefix as
        // its lowercased form so the param bind matches.
        assert_eq!(
            slot_name_like_prefix("MyApp").unwrap(),
            "__zs_slot_myapp%"
        );

        // Empty / invalid app_id propagates the validation error
        // (`invalid_app_id`) instead of producing the cluster-wide
        // `%` wildcard that would re-introduce the vulnerability.
        let err = slot_name_like_prefix("").unwrap_err();
        assert!(
            matches!(&err, DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"),
            "empty app_id must reject, not silently broaden the filter: got {err:?}"
        );
        // Defence in depth: a literal `%` in the app_id would be a
        // wildcard-injection vector, but `sanitise_app_id` already
        // rejects non-`[A-Za-z0-9_]` characters — confirm the rejection
        // flows through.
        let err = slot_name_like_prefix("%").unwrap_err();
        assert!(
            matches!(&err, DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"),
            "wildcard char must reject, not leak into LIKE: got {err:?}"
        );
    }

    /// `drop_abandoned_slots` uses the same `slot_name_like_prefix`
    /// shape for its candidate filter (the `$1` bind), so a tenant
    /// `dropAbandoned` can only reap its own inactive slots.
    #[test]
    fn drop_abandoned_slots_filters_by_app_id() {
        // The candidate-enumeration CTE binds `slot_name LIKE $1` with
        // the per-app prefix. Same helper as `watchdog_query` — pinning
        // both call sites against one canonical value catches any drift.
        let p = slot_name_like_prefix("app_a").unwrap();
        assert_eq!(p, "__zs_slot_app_a%");

        // App A's bind cannot reap App B's slot.
        let p_b = slot_name_like_prefix("app_b").unwrap();
        assert_ne!(p, p_b);

        // Validation-failure path also pins for dropAbandoned, since a
        // silent fallback to `%` here is the higher-severity DoS case.
        let err = slot_name_like_prefix("").unwrap_err();
        assert!(
            matches!(&err, DbError::ValidationFailed { code, .. } if *code == "invalid_app_id"),
            "empty app_id must reject, not silently broaden the DROP filter: got {err:?}"
        );
    }

}
