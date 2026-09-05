//! [`BackendHandle`] - the per-isolate dispatch enum over the installed backend.
//!
//! # Why this is its own file
//!
//! `docs/proposals/2026-08-31-data-crate-shape.md` splits `backend/mod.rs` in
//! two: "the nine traits and those value types go to `data-core`, below both
//! vendors; **`BackendHandle` and its dispatch methods go to `data-engine`**,
//! above them." The traits went down already and `backend/mod.rs` re-exports
//! them; this file is the other half, and separating it makes that crate move a
//! file move rather than an extraction from a 1,200-line module.
//!
//! What stays behind in `backend/mod.rs` is the re-export ladder - how the
//! adapter names the tiers below it - and the `Backend` conformance marker,
//! which is this crate's own `pub` trait and is pinned here by the
//! orphan rule.
//!
//! # Why it may name both vendors
//!
//! The enum has a `Postgres(Rc<PostgresBackend>)` arm and a `Sqlite(..)` arm,
//! which is a vendor-embedding violation everywhere except the tier that
//! dispatches. `data-engine` sits ABOVE both vendor crates and is exactly that
//! tier. The routed entry points below are the point: vendor selection for
//! vector and spatial search lives HERE so the engine's call sites never
//! downcast. They are free functions taking a `TxRoute` rather than trait impls
//! on the handle, because the lane a statement runs on is part of the dispatch
//! and a handle cannot carry it - see [`routed_vector_search`].
//!
//! See #164 for why decision 4's "adding a database must require ZERO changes
//! to data-engine" overstates the rule this enum has to satisfy - a closed sum
//! necessarily gains an arm per backend; what it must not gain is SQL.

use std::rc::Rc;

use serde_json::Value;

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;
use zeroship_schema::descriptors::{GeoPoint, VectorMetric};

// `VectorIndex` and `SpatialIndex` are NOT imported here any more: the two
// `impl ... for BackendHandle` blocks that named them became the routed free
// functions below, which call the vendors' inherent methods instead. The traits
// still exist and both vendor backends still implement them - that impl is what
// an autocommit caller reaches - but this file no longer names either.
use crate::backend::{
    ChangeStream, DialectBuilder, LockManager, PostgresBackend, ScalarRead, SqlExecutor,
    SqliteBackend, UnmaskAuditRow, postgres, sqlite,
};
use crate::tx_lanes::TxConnection;
use zeroship_data_core::error::{BeginIntent, OpenSessionError};

/// The unqualified name of the per-app unmask audit table, for the whole data
/// plane.
///
/// The data plane is this table's only WRITER and holds no authority to create
/// it: the two migration apply hosts do that, each from its own dialect's DDL
/// (`zeroship_migrate_sqlite::backend::AUDIT_UNMASK_TABLE` for the dev tier,
/// `zeroship_migrate_server::provisioning::AUDIT_UNMASK_TABLE` for PostgreSQL).
/// If a creator's spelling and a writer's spelling ever part company the INSERT
/// targets a relation nothing made, and since an unmask whose audit row cannot
/// be written must not return plaintext, EVERY unmask on that app fails.
///
/// This constant exists so the data plane contributes exactly ONE spelling to
/// that agreement rather than three. It was three until 2026-09-04: two inline
/// literals in [`BackendHandle::append_unmask_audit`]'s two arms and
/// `auth::bootstrap`'s `WORKER_WRITABLE_RESERVED_TABLE`, which names the one
/// reserved relation the worker role may append to.
///
/// The agreement across the crate boundary is bound by
/// `crates/zeroship-plugin-db/tests/audit_table_parity.rs`, which is the one
/// place all three constants are nameable - a shared constant is not available,
/// because it would need `zeroship-data-engine` to depend on the migration
/// engine, and privilege follows the process: the tier that runs creator code
/// does not link the tier that changes schema.
pub const AUDIT_UNMASK_TABLE: &str = "__zeroship_audit_unmask";

/// Per-isolate backend handle — the typed enum stashed on the adapter tier's
/// `ThreadDbContext` (in `zeroship-plugin-db`, which this crate cannot name).
///
/// **Why an enum, not `Box<dyn Backend>`** (closes
/// `docs/archive/db-system-design.md` §5.5 and
/// `docs/archive/p0-implementation-plan.md`):
///
/// - `Backend` is `async fn`-in-trait. Object-safety for those traits
///   would require `Box<dyn Future>` per call — a per-CRUD-op
///   allocation on a hot path that runs ~200K times/sec under load.
/// - The associated types (`Client = compio_postgres::OwnedPooledClient`,
///   `LiveSchema = crate::diff::LiveSchema`) cannot be erased behind a
///   `dyn` without losing the concrete client type that
///   [`LockManager::acquire_advisory_lock`] and the audit-row helpers
///   take by `&Self::Client` reference.
/// - The set of backends is closed (PG and SQLite today).
///   An enum is the canonical shape for a closed sum.
///
/// The SQLite arm and its `sqlite` Cargo feature were previously
/// declared before the underlying `crate::backend::sqlite` module
/// landed, so `--features sqlite` failed to compile (E0433). The arm
/// and the feature were removed and later re-introduced atomically
/// with the `crate::backend::sqlite::SqliteBackend` impl. A build with
/// `--no-default-features` is expected to fail at compile time (no
/// backend arm) — the failure mode is meaningful, not a silent
/// miscompile.
#[derive(Debug, Clone)]
pub enum BackendHandle {
    /// Postgres backend handle. Wraps an [`Rc<PostgresBackend>`] so
    /// cloning the enum stays cheap (Rc-clone of the inner pointer);
    /// every consumer site previously holding an `Rc<PostgresBackend>`
    /// migrates to this arm one-to-one.
    Postgres(Rc<PostgresBackend>),

    /// SQLite backend handle, wrapping
    /// [`crate::backend::sqlite::SqliteBackend`]. The inner type's
    /// capability-impl behaviour follows
    /// `docs/archive/p1-sqlite-implementation-plan.md` §9.
    Sqlite(Rc<SqliteBackend>),
}

/// Vendor selection for vector search lives HERE, in the vendor tier, instead
/// of at the engine call site.
///
/// Until 2026-09-02 the engine wrote
/// `if let Some(sq) = backend.as_sqlite() { .. } else { backend.as_postgres().ok_or(..)? }`.
/// That is the downcast the crate split forbids: it puts the names of both
/// concrete backends into engine code. Both arms already called the vendor's
/// trait method with identical arguments, so the branch was only ever SELECTING
/// an impl - and selecting an impl by vendor is precisely what this tier is for.
///
/// The `backend_unsupported("vector_search")` arm the old shape carried is not
/// reproduced, because it was unreachable: [`BackendHandle`] has exactly two
/// variants and neither is `#[cfg]`-gated, so the `else` of `as_sqlite()` was
/// always `Postgres`. Verified by reading the enum, not by test.
///
/// **Why this is a free function taking a `TxRoute`, and not
/// `impl VectorIndex for BackendHandle`.** It was that impl until 2026-09-03,
/// and a handle does not say which CONNECTION. Both arms reached the autocommit
/// lane - a fresh pooled checkout on PostgreSQL, `op_conn` on SQLite - while
/// every ordinary read in the same `db.transaction(fn)` callback went through
/// `exec_query`, which honours `route.in_tx()`. So a `search` over a row the
/// SAME transaction had just written returned nothing: the row existed, and the
/// connection sent to scan for it could not see it. This is the search family's
/// half of the defect `read_raw_column_bytes` documents for unmask, and the two
/// are independent - neither fix is evidence about the other. The route carries
/// both halves (the handle picks the vendor, `in_tx` picks the connection), so
/// they cannot come apart again. Bound by
/// `plugin-db/tests/search_tx_lane.rs`.
///
/// # Errors
///
/// The vendor's own refusals (`vector_extension_missing`, `vector_unsupported`,
/// a descriptor-driven builder error), the statement's database error, and
/// `transaction_scope_expired` / `transaction_connection_busy` when the route
/// claims a transaction whose session is not reachable.
#[allow(clippy::too_many_arguments)]
pub async fn routed_vector_search(
    route: &crate::tx_route::TxRoute,
    binding: &DbBinding,
    collection: &str,
    column: &str,
    query: &[f32],
    k: usize,
    metric: VectorMetric,
    filter: &serde_json::Value,
    schema: &serde_json::Value,
) -> Result<Vec<serde_json::Value>, DbError> {
    match route.backend() {
        // The SQLite arm has to ATTACH the app's database file before it
        // can scan it. That prelude sat at the engine call site; it belongs
        // to the arm that needs it, and nothing else has to know.
        BackendHandle::Sqlite(sq) => {
            sq.attach_app_file(binding.app_id()).await?;
            let (_lane_claim, lane) = sqlite_lane(route, sq)?;
            sq.vector_search_on(
                &lane, binding, collection, column, query, k, metric, filter, schema,
            )
            .await
        }
        BackendHandle::Postgres(pg) => {
            let bq = pg
                .plan_vector_search(binding, collection, column, query, k, metric, filter, schema)
                .await?;
            run_planned_postgres_read(route, pg, &bq).await
        }
    }
}

/// Vendor selection for spatial search. Same rationale as
/// [`routed_vector_search`] directly above, including the ATTACH prelude on the
/// SQLite arm and the lane the scan runs on.
///
/// # Errors
///
/// As [`routed_vector_search`], with `postgis_extension_missing` /
/// `invalid_geo_arg` in place of the vector refusals.
#[allow(clippy::too_many_arguments)]
pub async fn routed_spatial_near(
    route: &crate::tx_route::TxRoute,
    binding: &DbBinding,
    collection: &str,
    column: &str,
    point: GeoPoint,
    radius_m: f64,
    filter: &serde_json::Value,
    limit: Option<usize>,
    schema: &serde_json::Value,
) -> Result<Vec<serde_json::Value>, DbError> {
    match route.backend() {
        BackendHandle::Sqlite(sq) => {
            sq.attach_app_file(binding.app_id()).await?;
            let (_lane_claim, lane) = sqlite_lane(route, sq)?;
            sq.spatial_near_on(
                &lane, binding, collection, column, point, radius_m, filter, limit, schema,
            )
            .await
        }
        BackendHandle::Postgres(pg) => {
            let bq = pg
                .plan_spatial_near(
                    binding, collection, column, point, radius_m, filter, limit, schema,
                )
                .await?;
            run_planned_postgres_read(route, pg, &bq).await
        }
    }
}

/// Issue a planned PostgreSQL read on this dispatch's lane.
///
/// The transaction arm is the parked client, raw: it is already inside the
/// creator's `BEGIN`, whose `SET LOCAL ROLE` and DB-1 timeouts
/// `tx_session_setup_sql` installed when the transaction opened. The autocommit
/// arm goes through `query_roled_json`, which mints that same session state for
/// its own single-statement transaction. Both therefore run under the app's
/// role; what differs is which connection, which is the whole question.
async fn run_planned_postgres_read(
    route: &crate::tx_route::TxRoute,
    pg: &PostgresBackend,
    bq: &crate::query::BuiltQuery,
) -> Result<Vec<serde_json::Value>, DbError> {
    let params: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    if route.in_tx() {
        let lane = crate::exec::take_tx_lane(route)?;
        let TxConnection::Postgres(client) = lane.client() else {
            return Err(lane_vendor_mismatch());
        };
        let rows = client
            .query_text_params(&bq.sql, &params)
            .await
            .map_err(|e| crate::backend::pg_error::classify(&e))?;
        return Ok(zeroship_data_postgres::pg_row_json::rows_to_json_value(
            &rows,
        ));
    }
    // SCHEMA: the roled autocommit lane qualifies the table and derives the
    // per-app role, both from the physical schema.
    pg.query_roled_json(route.schema(), &bq.sql, &params).await
}

/// The unmask fetch's lane vendor disagreed with the bound backend's.
///
/// Unreachable in production - a lane is opened by the same backend the route
/// carries - and typed rather than `unreachable!()` because the two come from
/// different per-thread slots and a panic in the data plane is worse than a
/// refused read.
fn lane_vendor_mismatch() -> DbError {
    DbError::internal(
        "db: the transaction lane's connection does not match the bound backend".to_string(),
    )
}

/// Read the RAW sibling of a masked column as BYTES, **on this dispatch's
/// lane**.
///
/// The encrypted-storage half of unmask: the field's own column holds the
/// mask, the ciphertext lives in the raw sibling. Bytes, not text - the
/// sibling of an encrypted column is BYTEA on PostgreSQL and a BLOB on
/// SQLite, and rendering either through a text path is what made every
/// PostgreSQL unmask of an encrypted column fail (see the regression
/// `unmask_encrypted_column_on_pg_reads_bytea_raw_sibling`).
///
/// **Why the SQL is here and not in `crud::unmask`.** It was written twice
/// in the engine, once per vendor, behind an `as_postgres()` / `as_sqlite()`
/// downcast - the shape #119 exists to remove. Only the lowering differs (`$1`
/// and a roled scalar read against `?1`, quoted identifiers and a typed cell);
/// key resolution, AEAD and the wrap step above this are vendor-neutral and
/// stayed put.
///
/// **Why this takes a `TxRoute` and not a `&BackendHandle` + `app_id`.** It
/// took the latter until 2026-09-03, and a handle does not say which
/// CONNECTION. Every arm below reached the autocommit lane - a fresh pooled
/// checkout on PostgreSQL, `op_conn` on SQLite - while the SELECT whose rows it
/// was unmasking had gone through `exec_query`, which honours `route.in_tx()`.
/// So a `find({ unmask })` over a row the SAME transaction had inserted
/// returned `unmask_not_found`: the row existed, and the connection sent to
/// fetch its ciphertext could not see it. The route carries both halves - the
/// handle picks the dialect, `in_tx` picks the connection - so the two cannot
/// come apart again. Bound by
/// `plugin-db/tests/unmask_tx_lane.rs`.
///
/// # Errors
///
/// The statement's own database error; `transaction_scope_expired` /
/// `transaction_connection_busy` when the route claims a transaction whose
/// session is not reachable; a decode failure when the raw sibling is not
/// byte-typed.
pub async fn read_raw_column_bytes(
    route: &crate::tx_route::TxRoute,
    collection: &str,
    raw_column: &str,
    row_pk: &str,
) -> Result<ScalarRead<Vec<u8>>, DbError> {
    // TWO IDENTITIES, and the arms want different ones. Postgres qualifies the
    // table with the SCHEMA and narrows the session to the role derived from
    // it; SQLite qualifies with the ATTACH ALIAS, which is TENANT-keyed -
    // `attach_app_file(app_id)` is what created it.
    let schema = route.schema().as_str();
    let attach_alias = route.app_id();
    match route.backend() {
        BackendHandle::Postgres(pg) => {
            let sql =
                format!("SELECT \"{raw_column}\" FROM \"{schema}\".\"{collection}\" WHERE id = $1");
            if route.in_tx() {
                let lane = crate::exec::take_tx_lane(route)?;
                let TxConnection::Postgres(client) = lane.client() else {
                    return Err(lane_vendor_mismatch());
                };
                let rows = client
                    .query_text_params(&sql, &[row_pk])
                    .await
                    .map_err(|e| crate::backend::pg_error::classify(&e))?;
                return zeroship_data_postgres::pg_autocommit::scalar_bytes(&rows);
            }
            pg.read_roled_scalar_bytes(route.schema(), &sql, &[row_pk]).await
        }
        BackendHandle::Sqlite(sq) => {
            let q_app = sq.quote_ident(attach_alias);
            let q_coll = sq.quote_ident(collection);
            let q_col = sq.quote_ident(raw_column);
            let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
            let (_lane_claim, lane) = sqlite_lane(route, sq)?;
            let typed = lane.query_typed_internal(&sql, &[row_pk]).await?;
            if typed.rows.is_empty() {
                return Ok(ScalarRead::NoRow);
            }
            match &typed.rows[0][0] {
                sqlite::session::TypedCell::Blob(b) => Ok(ScalarRead::Value(b.clone())),
                sqlite::session::TypedCell::Null => Ok(ScalarRead::Null),
                other => Err(DbError::internal(format!(
                    "unmask: expected BLOB for encrypted column, got {other:?}"
                ))),
            }
        }
    }
}

/// Read the RAW sibling of a masked column as TEXT, on this dispatch's lane.
///
/// The plaintext-storage half: the column carries `.mask({...})` WITHOUT
/// `.encrypted(...)`, so the sibling holds the value in its own declared
/// type. Distinct from [`read_raw_column_bytes`] because the encrypted path
/// must not go near a text rendering; routed for the reason spelled out there.
///
/// # Errors
///
/// As [`read_raw_column_bytes`], with the decode failure reported when the raw
/// sibling is not text-typed.
pub async fn read_raw_column_text(
    route: &crate::tx_route::TxRoute,
    collection: &str,
    raw_column: &str,
    row_pk: &str,
) -> Result<ScalarRead<String>, DbError> {
    // The same two identities as [`read_raw_column_bytes`], split for the same
    // reason: PG qualifies with the schema, SQLite with the tenant-keyed ATTACH
    // alias.
    let schema = route.schema().as_str();
    let attach_alias = route.app_id();
    match route.backend() {
        BackendHandle::Postgres(pg) => {
            let sql =
                format!("SELECT \"{raw_column}\" FROM \"{schema}\".\"{collection}\" WHERE id = $1");
            if route.in_tx() {
                let lane = crate::exec::take_tx_lane(route)?;
                let TxConnection::Postgres(client) = lane.client() else {
                    return Err(lane_vendor_mismatch());
                };
                let rows = client
                    .query_text_params(&sql, &[row_pk])
                    .await
                    .map_err(|e| crate::backend::pg_error::classify(&e))?;
                return zeroship_data_postgres::pg_autocommit::scalar_text(&rows);
            }
            pg.read_roled_scalar_text(route.schema(), &sql, &[row_pk]).await
        }
        BackendHandle::Sqlite(sq) => {
            let q_app = sq.quote_ident(attach_alias);
            let q_coll = sq.quote_ident(collection);
            let q_col = sq.quote_ident(raw_column);
            let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
            let (_lane_claim, lane) = sqlite_lane(route, sq)?;
            let rows = lane.query_internal(&sql, &[row_pk]).await?;
            if rows.is_empty() {
                return Ok(ScalarRead::NoRow);
            }
            match rows[0].first().and_then(|c| c.clone()) {
                Some(value) => Ok(ScalarRead::Value(value)),
                None => Ok(ScalarRead::Null),
            }
        }
    }
}

/// The SQLite session handle this dispatch's raw-column read must use, with the
/// slot claim that has to outlive the statement.
///
/// The guard comes back to the caller rather than being dropped here, and that
/// is load-bearing: `TxClientSlotGuard` is what keeps a second op on this
/// thread from issuing on the same transaction connection while this read is
/// awaiting, and restores the session to the SAME app's slot on drop (SEC-1).
/// `crate::exec::exec_sqlite_json` holds it across its await for the same
/// reason. The handle is cloned out because a borrow of the guard cannot be
/// returned alongside it.
fn sqlite_lane(
    route: &crate::tx_route::TxRoute,
    backend: &sqlite::SqliteBackend,
) -> Result<
    (
        Option<crate::tx_lanes::TxClientSlotGuard>,
        sqlite::session::SqliteSessionHandle,
    ),
    DbError,
> {
    if !route.in_tx() {
        return Ok((None, backend.autocommit_client()));
    }
    let guard = crate::exec::take_tx_lane(route)?;
    let handle = match guard.client() {
        TxConnection::Sqlite(client) => client.clone(),
        TxConnection::Postgres(_) => return Err(lane_vendor_mismatch()),
    };
    Ok((Some(guard), handle))
}

impl BackendHandle {
    /// Append one row to the app's `__zeroship_audit_unmask` table.
    ///
    /// **Through the role fence on both vendors.** The PostgreSQL arm goes via
    /// `execute_roled`, not a bare pool checkout: `runtime_dependents_sql`
    /// grants the runtime role `WITH INHERIT FALSE`, so an unroled INSERT is
    /// refused outright - and since an unmask whose audit row cannot be written
    /// must not return plaintext, that would fail every unmask rather than leak
    /// one. The funnel also carries the DB-1 statement and lock timeouts.
    ///
    /// Empty strings stand in for absent values rather than SQL NULL. The table
    /// is operator-read-only, so `WHERE actor_id = ''` is the filter, and the
    /// simplicity is worth more here than NULL fidelity.
    ///
    /// **Two identities, one per arm.** PostgreSQL qualifies the table with the
    /// SCHEMA and runs the INSERT under the role derived from it; SQLite reaches
    /// the same table through the ATTACH ALIAS, which is tenant-keyed because
    /// `attach_app_file(app_id)` is what created it. One `&str` served both
    /// while the two values were the same string.
    pub async fn append_unmask_audit(
        &self,
        schema: &zeroship_schema::SchemaName,
        attach_alias: &str,
        row: &UnmaskAuditRow<'_>,
    ) -> Result<(), DbError> {
        match self {
            Self::Postgres(pg) => {
                let schema_name = schema.as_str();
                let sql = format!(
                    r#"INSERT INTO "{schema_name}"."{AUDIT_UNMASK_TABLE}"
                       (actor_id, actor_role, claimed_actor, collection, row_pk, "column",
                        classification, reason, outcome)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
                );
                pg.execute_roled(
                    schema,
                    &sql,
                    &[
                        row.actor_id,
                        row.actor_role,
                        row.claimed_actor,
                        row.collection,
                        row.row_pk,
                        row.column,
                        row.classification,
                        row.reason,
                        row.outcome,
                    ],
                )
                .await?;
                Ok(())
            }
            Self::Sqlite(sq) => {
                let q_app = sq.quote_ident(attach_alias);
                let sql = format!(
                    r#"INSERT INTO {q_app}."{AUDIT_UNMASK_TABLE}"
                       (actor_id, actor_role, claimed_actor, collection, row_pk, "column",
                        classification, reason, outcome)
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#
                );
                sq.pool_exec(
                    &sql,
                    &[
                        row.actor_id,
                        row.actor_role,
                        row.claimed_actor,
                        row.collection,
                        row.row_pk,
                        row.column,
                        row.classification,
                        row.reason,
                        row.outcome,
                    ],
                )
                .await?;
                Ok(())
            }
        }
    }

    /// Open a dedicated transaction session and return it as a lane.
    ///
    /// **What SC-1 asks for, in each backend's terms.** The protocol needs a
    /// session that is open, narrowed to the app's authority, and inside a
    /// transaction block. How that is reached differs enough that it cannot be
    /// written once: PostgreSQL checks a lease out of the pool, sends the
    /// rendered `BEGIN`, then narrows with `SET LOCAL ROLE` - in that order,
    /// because `SET LOCAL` needs a transaction to be local to. SQLite must bind
    /// the app's file into the session BEFORE the connection is opened, because
    /// the connection reads its path from the session's attachment list.
    ///
    /// **Every statement the driver later issues runs narrowed, and `BEGIN` is
    /// why that is checkable.** The pooled checkout arrives carrying the shared
    /// login role. Exactly ONE statement runs before
    /// [`postgres::apply_per_app_role`] narrows it, and that statement is
    /// `BEGIN`, which touches no object and needs no privilege. From there the
    /// session is the app role's until it settles.
    ///
    /// If narrowing fails, this returns `Err` and the lease drops un-installed.
    /// It goes back to the pool with a transaction open, which
    /// `Pool::return_client` handles: it rolls back any session it cannot prove
    /// `Idle` before publishing it.
    ///
    /// Takes BOTH identities. `app_id` is the SC-1 admission key and the SQLite
    /// ATTACH alias; `schema` is what the PostgreSQL session narrows its role
    /// to. One `&str` served both while they were the same string, and the day
    /// they diverge the PG arm would have narrowed to a role nobody created.
    pub async fn open_tx_session(
        &self,
        app_id: &str,
        schema: &zeroship_schema::SchemaName,
        begin: BeginIntent,
    ) -> Result<crate::tx_lanes::TxConnection, OpenSessionError> {
        match self {
            Self::Postgres(pg) => {
                let client = pg.acquire_dedicated_client(app_id).await?;
                let begin_sql = postgres::render_begin(begin);
                pg.client_exec(&client, &begin_sql, &[]).await?;
                postgres::apply_per_app_role(&client, schema).await?;
                Ok(crate::tx_lanes::TxConnection::Postgres(client))
            }
            Self::Sqlite(sq) => {
                sq.attach_app_file(app_id).await?;
                let client = sq.acquire_dedicated_client(app_id).await?;
                // **SQLite spells every intent `BEGIN`, and that is a
                // documented divergence rather than a dropped request.** It has
                // one isolation level - serialisable, enforced by the
                // single-writer actor - so there is no weaker level to ask for
                // and no stronger one to grant. See
                // `docs/reference/sqlite-divergences.md`.
                sq.client_exec(&client, "BEGIN", &[]).await?;
                Ok(crate::tx_lanes::TxConnection::Sqlite(client))
            }
        }
    }

    /// Make this backend ready to serve `app_id`, whatever that takes.
    ///
    /// **PostgreSQL needs nothing; SQLite must bind the app's file into the
    /// session first.** A connection reads its path from the session's
    /// attachment list, so an app that has never been attached gets a
    /// connection that cannot see its own tables. Cheap to repeat:
    /// `attach_app_file` returns on a cache hit before issuing any SQL.
    ///
    /// Callers that already hold a concrete `SqliteBackend` - the SQLite
    /// executor paths in `exec.rs` - call it directly and do not need this.
    /// This exists for the ones holding a handle, which otherwise downcast to
    /// SQLite purely to ask for a prerequisite PostgreSQL does not have.
    ///
    /// # Errors
    ///
    /// The SQLite attach failure. The PostgreSQL arm cannot fail.
    pub async fn prepare_for_app(&self, app_id: &str) -> Result<(), DbError> {
        match self {
            Self::Postgres(_) => Ok(()),
            Self::Sqlite(sq) => sq.attach_app_file(app_id).await,
        }
    }

    /// Persist an app's mask policy wherever this backend keeps one.
    ///
    /// **PostgreSQL stores nothing, and that is not a gap.** Its policy is held
    /// in the thread context and re-installed by `installSchema` on every boot,
    /// so there is no restart to survive. SQLite may run without that
    /// re-install, so it writes a sidecar beside the app files.
    ///
    /// Selecting between those by VENDOR is what this tier is for. The engine
    /// used to write `if backend.as_postgres().is_some() { .. } else if let
    /// Some(sq) = backend.as_sqlite() { persist_sqlite(sq, ..) }`, which put
    /// both concrete backend names, and SQLite's whole storage strategy, into
    /// engine code.
    ///
    /// # Errors
    ///
    /// The SQLite store's I/O failures. The PostgreSQL arm cannot fail.
    pub async fn persist_mask_policy(
        &self,
        app_id: &str,
        policy_json: &serde_json::Value,
    ) -> Result<(), DbError> {
        match self {
            Self::Postgres(_) => Ok(()),
            Self::Sqlite(sq) => sqlite::mask_policy_store::persist(sq, app_id, policy_json).await,
        }
    }

    /// Read back what [`Self::persist_mask_policy`] wrote, if anything.
    ///
    /// `None` on PostgreSQL always: nothing was stored, so there is nothing to
    /// recover, and the caller falls back to the cache the same way it would
    /// for a SQLite app with no sidecar entry.
    ///
    /// # Errors
    ///
    /// The SQLite store's read and parse failures.
    pub async fn load_mask_policy(
        &self,
        app_id: &str,
    ) -> Result<Option<serde_json::Value>, DbError> {
        match self {
            Self::Postgres(_) => Ok(None),
            Self::Sqlite(sq) => sqlite::mask_policy_store::load(sq, app_id).await,
        }
    }

    /// What the LIVE database says this app's tables are, columns included.
    ///
    /// Both arms recover the protection sentinels the migration engine wrote -
    /// `PostgreSQL` from `pg_description`, `SQLite` from `sqlite_master.sql` - so
    /// `ColumnInfo::mask` and `ColumnInfo::encryption` come back populated on
    /// either backend. That is the only part [`crate::crud::protection_floor`]
    /// reads, and it is why this is on the handle rather than downcast at the
    /// call site: selecting an impl by vendor is what this tier is for.
    ///
    /// **This is not a second schema authority.** `crate::descriptor` remains
    /// the sole source of a collection's SHAPE. What the catalog supplies is a
    /// FLOOR on its protections, which the descriptor may raise and may not
    /// lower.
    ///
    /// The `SQLite` arm attaches the app's file first. Its catalog query is
    /// `SELECT … FROM "<app>".sqlite_master`, which fails with a "no such table"
    /// error naming `<app>.sqlite_master` until the file is attached under that
    /// alias - and a
    /// caller reaching this before any statement has run for the app is the
    /// normal case, not an edge one, because this fence runs BEFORE the write it
    /// guards. [`Self::prepare_for_app`] is the existing no-op-on-PostgreSQL
    /// seam for exactly that prerequisite. An app with no file yet attaches an
    /// empty one and introspects to an empty schema, which imposes no floor -
    /// correct, since nothing has been protected yet.
    ///
    /// # Errors
    ///
    /// The `SQLite` attach failure, and either vendor's catalog-read failures.
    pub async fn introspect_schema(
        &self,
        app_id: &str,
    ) -> Result<zeroship_schema::diff::LiveSchema, DbError> {
        use crate::backend::SchemaIntrospect;
        match self {
            Self::Postgres(pg) => pg.introspect_schema(app_id).await,
            Self::Sqlite(sq) => {
                sq.attach_app_file(app_id).await?;
                sq.introspect_schema(app_id).await
            }
        }
    }

    /// Borrow the inner [`PostgresBackend`] as a `&PostgresBackend`
    /// reference — the async-friendly companion to `BackendHandle::with_postgres`.
    ///
    /// **Why both shapes**: the sync
    /// closure (`BackendHandle::with_postgres`) composes cleanly when the
    /// caller's body is sync, but it cannot `.await` across the
    /// closure boundary without lifetime gymnastics (the closure's
    /// inner future would have to outlive the closure scope). The
    /// async paths in `migrations.rs` and every `v8_classes::migration*` call
    /// site instead `.await` on
    /// the returned `&PostgresBackend` directly:
    ///
    /// ```ignore
    /// let backend = ensure_backend().await?;
    /// let pg = backend
    ///     .as_postgres()
    ///     .ok_or_else(unsupported_backend_op_error)?;
    /// pg.acquire_dedicated_client(app_id).await
    /// ```
    ///
    /// Returns `None` on the SQLite arm. The `Option`-shaped signature
    /// is the stable consumer contract so the existing
    /// `ok_or_else(...)?` consumer sites map the non-PG case to a
    /// typed `backend_unsupported` error rather than a panic.
    pub fn as_postgres(&self) -> Option<&PostgresBackend> {
        match self {
            Self::Postgres(b) => Some(b),
            Self::Sqlite(_) => None,
        }
    }

    /// `(idle, active, total)` connections, on a backend that pools.
    ///
    /// `None` on SQLite, which has no pool to count - the same shape the old
    /// `ThreadDbContext::pool()` produced there, since that slot was only ever
    /// filled on the Postgres arm.
    ///
    /// It is a method HERE rather than a `as_postgres()?.pool()` chain at the
    /// call site because the caller is `transaction::probe`, an engine module:
    /// reaching through to `compio_postgres::Pool` to read three counters would
    /// put a vendor type in the engine to answer a question the handle can
    /// answer itself.
    pub fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        match self {
            Self::Postgres(b) => {
                let pool = b.pool();
                Some((pool.idle_count(), pool.active_count(), pool.total_count()))
            }
            Self::Sqlite(_) => None,
        }
    }


    /// Borrow the inner [`SqliteBackend`] as a `&SqliteBackend`
    /// reference — async-friendly companion to `BackendHandle::with_sqlite`.
    /// Returns `Some(&SqliteBackend)` on the SQLite arm; `None` on
    /// the PG arm.
    ///
    /// Used by runtime dispatchers that branch between the PostgreSQL and
    /// SQLite implementations without exposing the concrete enum arm.
    pub fn as_sqlite(&self) -> Option<&SqliteBackend> {
        match self {
            Self::Postgres(_) => None,
            Self::Sqlite(b) => Some(b),
        }
    }

    /// Borrow a [`ChangeStream`] adapter over the SQLite arm —
    /// returns the [`crate::backend::sqlite::cdc::SqliteChangeStream`]
    /// wrapper that fills in `preupdate_hook`/`commit_hook`
    /// integration.
    ///
    /// Starts as a stub. The returned adapter's
    /// `provision`/`deprovision`/`spawn_consumer` are `Ok(())`/unit
    /// returns; `pause_broker` / `engage_schema_pending` return no-op
    /// guards until the real session-hook installation is wired.
    ///
    /// Returns `Some` on the SQLite arm; `None` on the PG arm.
    ///
    /// The adapter holds an `Rc<SqliteBackend>` (Rc-cloned from the
    /// arm's inner value); see
    /// [`crate::backend::sqlite::cdc::SqliteChangeStream`] for the
    /// lifetime / ownership rationale.
    pub fn as_change_stream_sqlite(
        &self,
    ) -> Option<crate::backend::sqlite::cdc::SqliteChangeStream> {
        match self {
            Self::Postgres(_) => None,
            Self::Sqlite(b) => Some(crate::backend::sqlite::cdc::SqliteChangeStream::new(
                b.clone(),
            )),
        }
    }

    // -----------------------------------------------------------------
    // Encryption key sourcing
    // -----------------------------------------------------------------

    /// Borrow the isolate's column-encryption key store.
    ///
    /// **This answers a question that is not about the vendor**, which is why
    /// it is one accessor rather than the `as_encrypted_column_pg` /
    /// `as_encrypted_column_sqlite` pair it replaced on 2026-09-02. Both arms
    /// hold the same [`crate::encryption::KeyStore`] type, both built from the
    /// same `crate::context::isolate_key_source()`, and the AEAD itself is
    /// `crate::encryption::aead` on either backend. The old pair made every
    /// caller open a two-arm match to reach code identical on both sides - 4
    /// of the 10 production vendor downcasts in the engine tier existed for
    /// exactly that and nothing else.
    ///
    /// Returning the store rather than performing the crypto is deliberate:
    /// key SOURCING is the only part a backend ever contributed, and now that
    /// both source identically the seam is a borrow, not a dispatch.
    pub fn key_store(&self) -> &crate::encryption::KeyStore {
        match self {
            Self::Postgres(b) => b.key_store(),
            Self::Sqlite(b) => b.key_store(),
        }
    }

    // The `as_backup_pg` / `as_backup_sqlite` accessors were removed: both were
    // `#[cfg(feature = "test-helpers")]`, no crate in the workspace enables that
    // feature, and neither had a caller anywhere - including this crate's own
    // tests, which reach the impls through the `Backup` trait instead. Whether
    // the capability itself ships is still open; the impls stay, and reviving
    // an accessor is a line of code if a consumer ever appears.
}

#[cfg(test)]
mod routed_read_tests {
    //! The SQLite half of the routed raw-column read.
    //!
    //! The PostgreSQL half is bound live by
    //! `zeroship-plugin-db/tests/unmask_tx_lane.rs`, which needs a server.
    //! SQLite needs none, and it is the tier `pnpm dev` runs on - so the arm
    //! that would otherwise ship unbound is this one. It is a REAL divergence
    //! there and not a formality: SC-2 Decision 1 gave the session actor a
    //! shared `op_conn` plus a transaction connection per app, so an unmask
    //! sent to `op_conn` inside a transaction cannot see that transaction's
    //! writes, exactly as on PostgreSQL.

    use std::path::PathBuf;
    use std::rc::Rc;

    use super::*;
    use crate::tx_route::CapturedRoute;

    /// A raw-sibling read inside a transaction must see that transaction's own
    /// write; the same read outside it must not.
    ///
    /// The two arms differ in ONE token - `in_tx` on the route - so a failure
    /// cannot be a missing table, a missing ATTACH or an unwritten row.
    #[test]
    fn a_routed_raw_read_follows_the_transaction_lane_on_sqlite() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let app = "sqlite_routed_raw_read";
            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    crate::encryption::LocalKeySource::env_var(),
                )
                .expect("open sqlite backend"),
            );
            backend
                .attach_app_file(app)
                .await
                .expect("attach the app file");
            backend
                .pool_exec(
                    &format!(
                        r#"CREATE TABLE "{app}"."people" (
                               id TEXT PRIMARY KEY,
                               ssn TEXT,
                               "__zs_raw__ssn" TEXT
                           )"#
                    ),
                    &[],
                )
                .await
                .expect("CREATE TABLE people");

            let handle = BackendHandle::Sqlite(Rc::clone(&backend));

            // Park a real transaction connection, the way `exec_begin` does.
            let client = backend
                .acquire_dedicated_client(app)
                .await
                .expect("acquire tx client");
            backend
                .client_exec(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            // Write the row ON that connection, so it exists only there.
            backend
                .client_exec(
                    &client,
                    &format!(
                        r#"INSERT INTO "{app}"."people" (id, ssn, "__zs_raw__ssn")
                           VALUES ('p1', '***', '123-45-6789')"#
                    ),
                    &[],
                )
                .await
                .expect("INSERT on the transaction connection");
            crate::tx_lanes::with_mut(|l| {
                let previous = l.install_tx_client(app, TxConnection::Sqlite(client));
                assert!(previous.is_none(), "the tx slot must start empty");
            });

            // CONTROL: a pool-lane read cannot see the uncommitted row.
            let outside = read_raw_column_text(
                &CapturedRoute::pool_for_tests(app, crate::query::SqlDialect::Sqlite)
                    .bind(handle.clone()),
                "people",
                "__zs_raw__ssn",
                "p1",
            )
            .await
            .expect("the pooled read itself must succeed");
            assert!(
                matches!(outside, ScalarRead::NoRow),
                "the row must be invisible on the autocommit lane, or the arm \
                 below rules on nothing: {outside:?}",
            );

            // SUBJECT: the same read, routed onto the transaction.
            let inside = read_raw_column_text(
                &CapturedRoute::tx_for_tests(app, crate::query::SqlDialect::Sqlite)
                    .bind(handle.clone()),
                "people",
                "__zs_raw__ssn",
                "p1",
            )
            .await
            .expect("a routed read inside the transaction must reach the row");
            assert!(
                matches!(&inside, ScalarRead::Value(v) if v == "123-45-6789"),
                "the transaction lane must return its own uncommitted value: {inside:?}",
            );

            // The guard must have handed the session back, or the next op in
            // this transaction would find an empty slot.
            let parked = crate::tx_lanes::with_mut(|l| l.take_tx_client_for(app));
            match parked {
                Some(TxConnection::Sqlite(client)) => {
                    let _ = client.exec("ROLLBACK", &[]).await;
                }
                Some(TxConnection::Postgres(_)) => {
                    panic!("the slot must hold the SQLite session this test parked")
                }
                None => panic!(
                    "the routed read must hand the tx session back; an empty slot here \
                     means the next op in the same transaction would refuse"
                ),
            }
        });
    }
}
