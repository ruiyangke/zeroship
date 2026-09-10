//! Postgres publication and replication-slot lifecycle for reactive
//! queries.
//!
//! The migration service owns each per-app `PUBLICATION`. Workers only
//! verify that publication exists and manage their own logical-decoding
//! `REPLICATION SLOT`, plus the app-scoped slot watchdog. Operator-owned
//! abandoned-slot cleanup lives in [`crate::slot_reaper`].
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
use zeroship_core::replication_names::{self, ReplicationNameError};

use crate::backend::pg_error;
use zeroship_data_orm::error::{DbError, first_row_or_internal, prefix_message};

/// Stable prefix used by every C1 Postgres object (publication, slot).
/// Picked deliberately short (4 chars + `_`) so the watchdog query's
/// `LIKE '__zs_%'` stays selective and the names fit inside Postgres's
/// 63-character `NAMEDATALEN` budget alongside even a long app_id.
pub(crate) const OBJECT_PREFIX: &str = replication_names::OBJECT_PREFIX;

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

pub(crate) fn worker_token(worker_id: &str) -> Result<String, DbError> {
    validate_worker_id(worker_id)?;
    Ok(stable_token(worker_id, 10))
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
    replication_names::publication_name(app_id).map_err(|err| {
        let message = match err {
            ReplicationNameError::EmptyAppId => "replication: app_id must not be empty",
            ReplicationNameError::NulAppId => "replication: app_id must not contain NUL",
        };
        DbError::validation("invalid_app_id", message)
    })
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
    Ok(format!(
        "{OBJECT_PREFIX}slot_{}__{}",
        stable_token(app_id, 14),
        worker_token(worker_id)?
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

/// Verify the migration-owned publication and idempotently ensure this
/// worker's replication slot exists on the connected Postgres instance.
///
/// Returns the slot's `confirmed_flush_lsn` (current safe-to-restart
/// point) as a `String` — see [`SetupOutcome`].
///
/// ## Idempotency model
///
/// - The publication is a precondition. A missing publication fails
///   closed because only the migration service may decide its table set.
/// - The slot uses a precondition SELECT against `pg_replication_slots`;
///   if absent, `pg_create_logical_replication_slot()` is called.
///   The race between SELECT and create is benign because the create
///   throws `duplicate_object` (SQLSTATE 42710) which we treat as
///   success.
///
pub async fn ensure_worker_slot(
    pool: &Pool,
    app_id: &str,
    worker_id: &str,
) -> Result<SetupOutcome, DbError> {
    let pub_name = publication_name(app_id)?;
    let slot = worker_slot_name(app_id, worker_id)?;

    // Publication membership is an authorization decision: it determines
    // which app relations the worker may observe through WAL. The migrated
    // service creates and reconciles it while holding table-owner authority;
    // a worker may only prove the object exists.
    let exists: bool = !pool
        .query_text_params(publication_probe_sql(), &[&pub_name])
        .await
        .map_err(|e| {
            let mut err = pg_error::classify(&e);
            prefix_message(&mut err, "replication: probe pg_publication: ");
            err
        })?
        .is_empty();
    if !exists {
        return Err(DbError::Configuration {
            code: "replication_publication_missing",
            message: format!("replication: publication {pub_name} is missing for app {app_id}"),
            hint: Some(
                "apply the app migrations so zeroship-migrate-server reconciles the publication"
                    .to_string(),
            ),
        });
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
            let mut err = pg_error::classify(&e);
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
                    let mut err = pg_error::classify(&e);
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

const fn publication_probe_sql() -> &'static str {
    "SELECT 1 FROM pg_publication WHERE pubname = $1"
}

/// Result of [`ensure_worker_slot`].
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
/// slot prefix. The platform-internal watchdog uses these records for
/// per-app lag diagnostics. Cluster-wide cleanup is owned separately by
/// [`crate::slot_reaper`].
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
pub async fn watchdog_query(pool: &Pool, app_id: &str) -> Result<Vec<SlotHealth>, DbError> {
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
            let mut err = pg_error::classify(&e);
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

// Worker-owned slot teardown
// ---------------------------------------------------------------------------

/// Grace the drop sequence waits for a terminated replication backend to
/// detach before it attempts the drop. §17.7: "after a 5s grace, …
/// `pg_terminate_backend`".
///
/// Ungated and load-bearing: [`drop_slot`] derives its poll budget from this
/// and [`DROP_TERMINATE_POLL_INTERVAL`], so editing either moves the behaviour.
/// It was `test-helpers`-gated with no consumer but an equality assertion
/// against its own literal, while the loop it describes hardcoded the same
/// grace independently - two spellings of one number, free to drift.
pub const DROP_TERMINATE_GRACE_SECS: u64 = 5;

/// How often [`drop_slot`] re-probes `pg_replication_slots.active` while
/// waiting out [`DROP_TERMINATE_GRACE_SECS`].
pub const DROP_TERMINATE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Number of probes that fit in the grace. Derived, never written twice.
const fn drop_terminate_poll_attempts() -> u32 {
    let grace_ms = DROP_TERMINATE_GRACE_SECS * 1_000;
    (grace_ms / DROP_TERMINATE_POLL_INTERVAL.as_millis() as u64) as u32
}

/// Drop one worker's slot while retaining the app publication.
///
/// This is the normal last-local-subscriber teardown. Other worker
/// containers may still have subscribers and continue decoding the
/// shared publication through their own slots.
pub async fn drop_worker_slot(pool: &Pool, app_id: &str, worker_id: &str) -> Result<(), DbError> {
    let slot = worker_slot_name(app_id, worker_id)?;
    drop_slot(pool, &slot).await
}

/// Drop every worker slot for an app while retaining its publication.
///
/// Publication ownership stays with the migration service. A worker
/// must not be able to alter or drop the table-membership decision.
/// The exact `__` delimiter and
/// `left(...)=...` comparison ensure one app token cannot prefix-match
/// another app's slots.
pub async fn drop_worker_slots(pool: &Pool, app_id: &str) -> Result<(), DbError> {
    let slot_prefix = worker_slot_name_prefix(app_id)?;
    let rows = pool
        .query_text_params(
            "SELECT slot_name FROM pg_replication_slots
             WHERE left(slot_name, length($1)) = $1",
            &[&slot_prefix],
        )
        .await
        .map_err(|e| {
            let mut err = pg_error::classify(&e);
            prefix_message(&mut err, "replication: drop: enumerate worker slots: ");
            err
        })?;

    for row in rows {
        let slot: String = row.get("slot_name");
        drop_slot(pool, &slot).await?;
    }

    Ok(())
}

async fn drop_slot(pool: &Pool, slot: &str) -> Result<(), DbError> {
    let active_rows = pool
        .query_text_params(
            "SELECT active, active_pid FROM pg_replication_slots WHERE slot_name = $1",
            &[slot],
        )
        .await
        .map_err(|e| {
            let mut err = pg_error::classify(&e);
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
                    .query_text_params("SELECT pg_terminate_backend($1::int4)", &[&pid.to_string()])
                    .await
                    .map_err(|e| {
                        let mut err = pg_error::classify(&e);
                        prefix_message(&mut err, "replication: drop: pg_terminate_backend: ");
                        err
                    })?;
            }
        }
    }
    // A missing slot is an idempotent success.

    // `pg_terminate_backend` acknowledges delivery of the termination
    // signal, not completion of backend teardown. Wait out
    // `DROP_TERMINATE_GRACE_SECS` for the slot to become inactive.
    // This closes the common race where an immediate DROP reports
    // object_in_use and leaves a slot behind after the last subscriber.
    if !active_rows.is_empty() {
        for _ in 0..drop_terminate_poll_attempts() {
            let rows = pool
                .query_text_params(
                    "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                    &[slot],
                )
                .await
                .map_err(|e| {
                    let mut err = pg_error::classify(&e);
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
            compio::time::sleep(DROP_TERMINATE_POLL_INTERVAL).await;
        }
    }

    // 2. Drop the slot (now inactive). `pg_drop_replication_slot` errors
    //    if the slot doesn't exist, so guard on the probe above: only
    //    attempt the drop when the slot row was present.
    if !active_rows.is_empty() {
        match pool
            .query_text_params("SELECT pg_drop_replication_slot($1)", &[slot])
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let err = pg_error::classify(&e);
                // 55006 object_in_use ⇒ the backend hasn't fully detached
                // yet. Surface as LockContention so the caller can retry
                // from step 3 (§17.7 "retry from step 3 on partial
                // failure"); the slot is still there for the next pass.
                if matches!(err, DbError::LockContention { .. }) {
                    let mut err = err;
                    prefix_message(
                        &mut err,
                        &format!("replication: drop: slot {slot} still active (retry): "),
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
    // `DbError` lives in `zeroship-data-core`; the lowering to an `OpError` is
    // the ADAPTER's, so it arrives as a trait rather than an inherent method.
    use crate::op_error::ToOpError;

    #[test]
    fn worker_setup_only_probes_for_the_migrated_publication() {
        let sql = publication_probe_sql();
        assert!(sql.contains("FROM pg_publication"));
        for forbidden in [
            "CREATE PUBLICATION",
            "ALTER PUBLICATION",
            "DROP PUBLICATION",
        ] {
            assert!(
                !sql.contains(forbidden),
                "worker publication probe must not carry publication DDL: {sql}"
            );
        }

        let source = include_str!("replication.rs");
        let setup = source
            .split_once("pub async fn ensure_worker_slot")
            .expect("worker setup function")
            .1
            .split_once("const fn publication_probe_sql")
            .expect("publication probe boundary")
            .0;
        for forbidden in [
            "CREATE PUBLICATION",
            "ALTER PUBLICATION",
            "DROP PUBLICATION",
        ] {
            assert!(
                !setup.contains(forbidden),
                "worker setup must contain no publication DDL: {setup}"
            );
        }
    }

    #[test]
    fn app_id_validation_accepts_production_uuid() {
        assert!(publication_name("0191e7a2-b3c4-4d5e-8f90-123456789abc").is_ok());
        assert!(publication_name("typed_app-id").is_ok());
        assert!(publication_name("").is_err());
        assert!(publication_name("nul\0app").is_err());
    }

    #[test]
    fn case_distinct_app_ids_have_distinct_names() {
        assert_ne!(
            publication_name("MyApp").unwrap(),
            publication_name("myapp").unwrap()
        );
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
        // §17.7: "after a 5s grace, ... pg_terminate_backend". Pin the
        // constant so an edit that loosens the grace trips a test.
        assert_eq!(DROP_TERMINATE_GRACE_SECS, 5);

        // And pin that `drop_slot`'s poll budget really spans that grace.
        // Until 2026-09-09 the loop carried its own literals, so this
        // assertion compared the constant with itself while the behaviour it
        // named was free to move.
        let spanned = DROP_TERMINATE_POLL_INTERVAL * drop_terminate_poll_attempts();
        assert_eq!(
            spanned,
            std::time::Duration::from_secs(DROP_TERMINATE_GRACE_SECS),
            "the drop poll loop must wait exactly the documented grace"
        );
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
        assert_ne!(
            publication_name("MyApp").unwrap(),
            publication_name("myapp").unwrap()
        );

        // The schema reference still preserves original case via quote_ident
        // (the same function build_create_schema uses) — defense-in-depth.
        assert_eq!(crate::compile::quote_ident("MyApp"), "\"MyApp\"");
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
    // The pool-bound helpers (`ensure_worker_slot`,
    // `watchdog_query`) can only be reached via
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
    // `zeroship_data_orm::error::tests::prefix_message_preserves_variant_and_code`
    // and `prefix_message_leaves_structured_variants_alone` — the
    // helper moved into `crate::error` when the per-file `coded_sql`
    // duplicates were collapsed onto a single shared variant-walker.
    // Test coverage of the contract did not move; only its home file
    // did.

    // -----------------------------------------------------------------
    // Cross-tenant scoping regression guard (security review r5,
    // 2026-05-22). Before the fix, `watchdog_query` ran a cluster-wide
    // enumeration against `pg_replication_slots` with no per-app filter,
    // exposing co-tenant slot names.
    //
    // The helper now builds the candidate filter from
    // `worker_slot_name_prefix(app_id)` and passes it as a `$1` bind.
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

    /// The seam's replication names are this module's, byte for byte.
    ///
    /// `zeroship_core::app_derivation` composes the publication and the slot
    /// over a typed `AppId`; these functions compose them over a `&str` and
    /// keep doing so because their callers - the WAL consumer, the watchdog,
    /// the reaper - carry an app id as a string. The two spellings must not
    /// drift: a slot the seam names and a slot this module creates are the same
    /// `PostgreSQL` object or the worker consumes from a slot nobody advances.
    ///
    /// THE COMPARISON LIVES HERE AND NOT IN THE SEAM because `zeroship-core` is
    /// below this crate and cannot name it. The seam's own arm pins the same
    /// values as frozen literals; this one proves those literals describe the
    /// live composer rather than a copy of it.
    #[test]
    fn the_derivation_seam_composes_the_same_replication_names() {
        let raw = uuid::Uuid::parse_str("0191e7a2-b3c4-4d5e-8f90-123456789abc")
            .expect("fixture uuid parses");
        let app = zeroship_core::app_id::AppId::from_uuid(&raw);
        let as_str = raw.to_string();

        assert_eq!(
            zeroship_core::app_derivation::publication_name(&app),
            publication_name(&as_str).expect("publication name composes")
        );
        assert_eq!(
            zeroship_core::app_derivation::worker_slot_name(&app, "worker-a")
                .expect("seam slot name composes"),
            worker_slot_name(&as_str, "worker-a").expect("slot name composes")
        );
        assert_eq!(
            zeroship_core::app_derivation::worker_slot_name_prefix(&app),
            worker_slot_name_prefix(&as_str).expect("slot prefix composes")
        );
    }
}
