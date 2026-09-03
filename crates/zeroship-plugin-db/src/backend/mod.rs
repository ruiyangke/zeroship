//! Backend abstraction — "the data store boundary".
//!
//! ## What this is
//!
//! A single trait, `Backend`, that captures everything the
//! orchestrator and audit-row layer ask of "the database" — connection
//! lifecycle, advisory locks, schema introspection, and audit-table
//! reads/writes. Today the only impl is [`PostgresBackend`]; the goal
//! is to name the seams BEFORE a second backend lands so we don't
//! accidentally bake `compio_postgres::Pool` / `Client` into every
//! consumer file.
//!
//! ## What this is NOT
//!
//! - **Not a query-IR layer.** [`crate::query`] still emits Postgres
//!   DDL/DML directly via `quote_ident`, `ON CONFLICT`, `RETURNING`,
//!   etc. The architecture critic explicitly scoped that out — see
//!   `query.rs` (4099 LOC) and the review's R2 recommendation: "wait
//!   until a real sqlite or planetscale prototype is in motion." This
//!   trait fixes the *non-builder* surface (orchestrator + audit).
//! - **Not a leakage-free abstraction.** [`crate::replication`] and
//!   [`crate::wal_consumer`] talk raw `pg_replication_slots` and the
//!   streaming WAL protocol; those stay Postgres-only behind their own
//!   files. The `auth/*` SECURITY DEFINER bootstrap is likewise PG-only.
//! - **Not async-trait-Boxed.** `compio-postgres` is single-threaded
//!   io_uring; we use `async fn` directly in trait position
//!   (`async_fn_in_trait` is stable) so the orchestrator's hot paths
//!   don't allocate a `Box<dyn Future>` per call.
//!
//! ## Trait shape
//!
//! Associated types `Client` / `LiveSchema` keep the consumer files
//! free of `compio_postgres::Client` / `crate::diff::LiveSchema`
//! direct references — they go through `B::Client` / `B::LiveSchema`
//! instead. The PG impl ties them to the concrete types in
//! [`postgres::PostgresBackend`].
//!
//! ## Capability traits
//!
//! `Backend` is a **pure composition marker** — every operation lives
//! on one of the focused capability traits:
//!
//! - [`SqlExecutor`] — connection lifecycle + run-a-statement.
//! - [`LockManager`] — session-scoped advisory locks.
//! - [`SchemaIntrospect`] — live-schema snapshot + row-count estimate.
//!   Owns the `LiveSchema` associated type that used to live on
//!   `Backend`.
//!
//! A PG-only extension trait, [`PgSqlExecutor`], exposes `pool_handle()`
//! so free-function helpers can reach `&compio_postgres::Pool` without
//! naming the concrete backend.
//!
//! **No capability on this surface emits DDL, and that is the point.**
//! `IndexBuilder` (`CREATE INDEX CONCURRENTLY` with retry recovery),
//! `AuditWriter` (the `__zeroship_migrations` provenance log that
//! existed only to record that DDL), and the `ensure_vector_index` /
//! `ensure_spatial_index` halves of [`VectorIndex`] / [`SpatialIndex`]
//! are DELETED, names included. Schema belongs to `zeroship-migrate`:
//! it authors the pgvector `USING ivfflat` and PostGIS `USING gist`
//! indexes from the declared `t.vector()` / `t.geoPoint()` fields
//! (`zeroship-migrate-core/src/render/declarative.rs`, `vector_index_snapshot`
//! at `:2888` and `geo_index_snapshot` at `:2928`, emitted by
//! `zeroship-migrate-postgres/src/ddl.rs::create_index`). A backend that
//! can alter schema is a backend that can disagree with the descriptor
//! describing it.

use std::rc::Rc;

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::{BeginIntent, DbError, OpenSessionError};

/// Out-of-band cancellation for a pinned transaction session.
///
/// Lives here rather than under `transaction/`, where it sat until 2026-09-02,
/// because both its types are vendor: `PostgresCanceller` holds a
/// `compio_postgres` pool and cancel token, and the SQLite arm holds a handle to
/// the session actor. SC-1 asks a session for its canceller and later asks that
/// canceller to fire; neither call needs the protocol to know which backend
/// answered.
pub(crate) mod cancel;

// SQLite module — crate-private by default; under `test-helpers` it
// becomes `pub` so the integration target (`tests/sqlite_integration.rs`)
// can name `backend::sqlite::SqliteBackend` and the session-handle
// accessor.
// The PG-side test target reaches its backend through
// `backend::PostgresBackend` (re-exported below); the SQLite arm has
// session-actor internals worth pinning at the integration level, so
// the full sub-module is visible under the same gate.
// The SQLite vendor tier moved to `zeroship-data-sqlite` on 2026-09-02. The
// re-export keeps `backend::sqlite::...` resolving for the integration target,
// which names session-actor internals directly.
pub use zeroship_data_sqlite as sqlite;
pub use zeroship_data_sqlite::SqliteBackend;

// Same orphan-rule case as the PostgreSQL arm: `Backend` is this crate's own
// `pub(crate)` marker, so its impl on a foreign type can only be written here.
#[cfg(any(test, feature = "test-helpers"))]
impl Backend for SqliteBackend {}

// The PostgreSQL vendor tier moved to `zeroship-data-postgres` on 2026-09-02.
// Re-exported at the addresses the crate already spells, so this is a move
// rather than a rename sweep across every consumer.
//
// The split is NOT `pub use zeroship_data_postgres::*`: the gated half must
// carry the same `cfg` as the impls in THIS crate. `SchemaIntrospect` proved
// why - its trait is `cfg(feature)` in data-core while plugin-db impls it under
// `cfg(any(test, feature))`, and the mismatch was invisible to every `cargo
// check` configuration and appeared only in the lib-TEST target.
pub use zeroship_data_postgres::{PostgresBackend, pg_error, pg_row_json, postgres};
// `pg_autocommit` and `pg_session_sql` lost their last UNGATED consumer in this
// crate when the PostgreSQL tier left: what still names them is `exec.rs`'s test
// module, `auth/bootstrap.rs` (itself gated) and `tests/integration.rs`. The
// gate keeps a default build warning-free without hiding them from the callers
// that exist.
#[cfg(any(test, feature = "test-helpers"))]
pub use zeroship_data_postgres::{pg_autocommit, pg_session_sql};
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) use zeroship_data_postgres::{PgLockManager, PgSqlExecutor, lock_guard, pg_introspect};
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) use zeroship_data_postgres::lock_guard::LockGuard;


// The vocabulary these capability traits speak in moved to
// `zeroship-data-core` on 2026-09-02: every one is a value type or a constant
// that names no driver, no runtime and no V8, which is what rank 0 means. The
// traits themselves follow in a later batch; moving the nouns first means they
// arrive with nothing left to drag behind them.
//
// Re-exported wholesale rather than repointed at call sites, the same mechanism
// `budgets` used: `crate::backend::LockScope` and friends resolve unchanged, so
// this is a move rather than a rename sweep across the crate.
pub use zeroship_data_core::capability::{ScalarRead, UnmaskAuditRow};
// Same story as `pg_autocommit` above: `lock_guard.rs` was the ungated consumer
// of both, and it travelled to the PostgreSQL crate with `LockGuard`.
#[cfg(any(test, feature = "test-helpers"))]
pub use zeroship_data_core::capability::{LockScope, SNAPSHOT_RESTORE_LOCK_TAG};
// The backup/snapshot four carry `#[cfg(feature = "test-helpers")]`, and THAT
// ATTRIBUTE CHANGED MEANING WHEN THEY CROSSED THE CRATE BOUNDARY: it used to
// name this crate's feature, and now names `zeroship-data-core`'s. The two are
// wired (`test-helpers = ["zeroship-data-core/test-helpers"]`) so they turn on
// together - but only when something turns them on. A dependent building
// plugin-db with default features gets data-core WITHOUT the feature, so an
// unconditional re-export here names four items that do not exist.
//
// `cargo check -p zeroship-plugin-db --features test-helpers --all-targets`
// CANNOT SEE THAT. It passed clean while zeroship-worker and zeroship-cli both
// failed to build. Only a dependent build reveals it; check one.
#[cfg(feature = "test-helpers")]
pub use zeroship_data_core::capability::{BusyPolicy, PitrTarget, SnapshotHandle, SnapshotOpts};

// The eight capability traits followed their vocabulary down on 2026-09-02.
// Each is a contract and nothing more: associated types and signatures over the
// items above. What stayed here is every `impl Trait for T` - nineteen of them
// across this crate, including two on `BackendHandle` - because the orphan rule
// puts an impl in the crate owning the type, not the crate owning the trait.
//
// THE TWO GATED TRAITS NEED TWO DIFFERENT GATES, and the asymmetry is the whole
// lesson of this move. In `zeroship-data-core` both are `#[cfg(feature =
// "test-helpers")]`, because across a crate boundary a `cfg(test)` arm means
// THAT crate's test build and never fires for a consumer - data-core's own
// manifest says so.
//
// Here the gate has to match THIS crate's impls instead:
//
//   * `Backup`'s two impls carry `#[cfg(feature = "test-helpers")]`, so the
//     re-export does too.
//   * `SchemaIntrospect`'s two impls carry `#[cfg(any(test, feature = ...))]`,
//     so it is in scope during `cargo test --lib`, which does NOT set this
//     crate's feature. Gating the re-export on the feature alone left the impls
//     present and the trait absent - 7 errors in the lib-TEST target while all
//     three check configurations were clean. data-core still supplies the item
//     there: the `[dev-dependencies]` entry turns ITS feature on for test
//     targets.
pub use zeroship_data_core::storage::{
    ChangeStream, DialectBuilder, LockManager, SpatialIndex, SqlExecutor, VectorIndex,
};
#[cfg(feature = "test-helpers")]
pub use zeroship_data_core::storage::Backup;
#[cfg(any(test, feature = "test-helpers"))]
pub use zeroship_data_core::storage::SchemaIntrospect;






// `VectorMetric` is a schema-shape descriptor
// (consumed by the DDL builder in `zeroship_schema::query` to pick the
// pgvector opclass). It was relocated into the leaf crate
// `zeroship-schema` and is re-exported here so existing
// `crate::backend::VectorMetric` references resolve unchanged.
pub use zeroship_schema::descriptors::VectorMetric;


// `GeoPoint` is a schema-shape descriptor
// (consumed by `zeroship_schema::query::build_spatial_near` and the
// `geoPoint` DDL emitter). It was relocated into the leaf crate
// `zeroship-schema` and is re-exported here so existing
// `crate::backend::GeoPoint` references (the `SpatialIndex` trait input,
// the SQLite haversine impl) resolve unchanged.
pub use zeroship_schema::descriptors::GeoPoint;

// ===========================================================================
// The Backup capability trait
// ===========================================================================
//
// ONE capability trait, `Backup`, defined per
// `docs/archive/p5-encryption-backup-implementation-plan.md` §2 + §9. It does
// not join the `Backend` super-trait composition; it is an admin-surface
// accessor routed via a dedicated accessor (mirror of the `as_change_stream_*`
// shape the [`ChangeStream`] capability adopted).
//
// THERE WERE TWO UNTIL 2026-09-02. `EncryptedColumn` is deleted, and its own
// rustdoc is what condemned it: it said "PG and SQLite share the same AEAD
// impl, so per-backend trait impls are thin delegations" and "Key sourcing no
// longer differs" - the second having become true on 2026-08-27 when the admin
// schema went. Measured before deleting: the two impls were identical line for
// line, every `KeyHandle` in the tree (both backends, all three test stubs)
// bound to `crate::encryption::aead::AeadKey`, and both backends held the same
// `crate::encryption::KeyStore` built from the same `isolate_key_source()`.
//
// So the trait carried NO dialect knowledge, and its only effect was to make
// the engine ask which vendor it was on in order to reach code that does not
// depend on the answer - `as_encrypted_column_pg` / `as_encrypted_column_sqlite`
// were 4 of the 10 production vendor downcasts in the engine tier. Encryption
// is a property of the workspace, not of the database, so the key store is now
// borrowed through one dialect-neutral `BackendHandle::key_store` and the CRUD
// passes take `&KeyStore` directly.
//
// Do not reintroduce a per-backend encryption trait to "leave room" for a KMS
// handle. That is the exact argument the deleted associated type carried, and
// it bought a hypothetical variant at the cost of a real vendor coupling in
// every caller. A KMS arm belongs inside `KeyStore`, which is already the one
// type both backends name.
//
// This section brings:
//   - the `Backup` trait declaration;
//   - the supporting [`EncryptionMode`] / [`BusyPolicy`] /
//     [`SnapshotOpts`] / [`SnapshotHandle`] / [`PitrTarget`] types;
//   - impls on `PostgresBackend` + `SqliteBackend`;
//   - compile-time trait-shape pins in the `tests` module.
//
// The encryption module the AEAD path delegates to is at
// `crate::encryption`.


// `EncryptionMode` is a schema-shape descriptor
// (the `t.encrypted({mode})` facet; the DDL builder emits the `zsenc`
// sentinel from it, and the data-plane AEAD path reconstructs the AAD from
// it). It was relocated into the leaf crate `zeroship-schema` and is
// re-exported here so existing `crate::backend::EncryptionMode` references
// (`encryption::aad`, the CRUD passes) resolve
// unchanged. The mode's semantics (AAD shape / nonce derivation) are
// implemented by the data-plane crypto in plugin-db, which STAYS here.
pub use zeroship_schema::descriptors::EncryptionMode;



/// The data-store boundary. One impl per storage backend; today only
/// Postgres ([`PostgresBackend`]).
///
/// `Backend` is now a **pure composition marker** — every operation
/// lives on a focused sub-trait. The super-trait bound
/// is the carved capability set:
///
/// - [`SqlExecutor`] — the `Client = compio_postgres::OwnedPooledClient`
///   pin was dropped from this super-bound so a `SqliteBackend` whose
///   `SqlExecutor::Client = SqliteSessionHandle` can also satisfy
///   `Backend`. PG-only consumers that *need* the concrete client
///   type continue to bound on
///   [`PgSqlExecutor`] / [`PgLockManager`] (which still pin
///   `Client = compio_postgres::OwnedPooledClient`).
/// - [`LockManager`]
/// - [`SchemaIntrospect`] with `LiveSchema = crate::diff::LiveSchema`
///
/// The audit-table operations that used to live here
/// (`ensure_audit_table`, `next_schema_version`, `write_audit_row`, …)
/// and the `IndexBuilder` capability they existed to record are both
/// DELETED. They were the provenance log for the only DDL the data
/// plane still issued; with the DDL gone the log has nothing to record.
///
/// Lifetime invariants (preserved from the pre-carving shape):
///
/// - Methods that take `&Self::Client` use it borrow-only; the caller
///   owns the client.
/// - [`SqlExecutor::acquire_dedicated_client`] returns an owned `Client`
///   detached from any pool lifetime — the caller is free to park it
///   on the per-isolate context (e.g.
///   `ThreadDbContext::tx_conns`) for the duration
///   of a transaction.
///
/// NOTE FOR DOC LINKS, AND AN OPEN DECISION.
///
/// This trait is `cfg(any(test, feature = "test-helpers"))`, so it does not
/// exist in a DEFAULT build. References to it elsewhere in this crate are
/// currently code spans rather than intra-doc links.
///
/// That choice is CONFIGURATION-DEPENDENT, and an earlier version of this note
/// overstated it as "broken on every doc build". Re-measured 2026-08-28 with
/// `--no-deps` (the figures it replaces, 27 and 12, were taken on 2026-08-20
/// before `mod audit` was deleted):
///
/// ```text
/// cargo doc -p zeroship-plugin-db --document-private-items --no-deps
///   -> 14 unresolved links
/// ... --features test-helpers --document-private-items --no-deps
///   -> 6
/// ```
///
/// That block was INDENTED rather than fenced until 2026-08-28, which made
/// rustdoc read it as a Rust doctest; `cargo test -p zeroship-plugin-db
/// --features test-helpers --doc` failed on it with "expected one of `!` or
/// `::`, found `doc`". Default-feature `--doc` runs stayed green throughout,
/// because this trait is cfg-gated out of them and the doctest was never
/// collected - so the failure was invisible to any run that did not pass the
/// feature.
///
/// Most of this crate is `pub(crate)` by default and `pub` only under
/// `test-helpers` - lib.rs pairs the two behind cfg for `auth`, `crud`,
/// `encryption` and `backend`. So under the feature these links
/// RESOLVE, and the spans are only correct for a default-feature doc build.
///
/// THE QUESTION, AND ITS ANSWER AS OF 2026-08-20. Which configuration are this
/// crate's docs for? This note said "zeroship has no doc gate today, so nothing
/// currently encodes either answer". It does now, and it answers BOTH:
/// `tests/run_doc_gate.sh` builds the workspace twice, default and
/// `--all-features`, and requires zero unresolved links in each. So neither
/// configuration is privileged, and the construct that is correct in both is a
/// code span. The spans stay. A cfg-gated internal gets a span, not a link,
/// and the reason is now enforced rather than remembered.
///
/// The counts above are for `--document-private-items`. On a PUBLIC doc build
/// the same question has much smaller but much sharper stakes, measured
/// 2026-08-07 over the then-26 workspace members from a clean `cargo clean
/// --doc` (30 members and the same shape when re-measured 2026-08-20):
///
/// ```text
/// cargo doc --no-deps --workspace                 -> 1 unresolved
/// cargo doc --no-deps --workspace --all-features  -> 0
/// ```
///
/// The unresolved link that motivated this gate was removed with its dead
/// schema-apply module; both arms of the gate stand at zero.
///
/// It is a conformance marker, not the production abstraction: nothing takes
/// `dyn Backend` (see the note above `BackendHandle`), dispatch goes through
/// that enum, and this trait exists so tests can assert the concrete backends
/// implement the whole sub-trait set.
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) trait Backend:
    SqlExecutor + LockManager + SchemaIntrospect<LiveSchema = crate::diff::LiveSchema> + 'static
{
}

// The impl lives HERE and not beside the sub-trait impls in
// `zeroship-data-postgres`, and that is the orphan rule rather than a
// preference: `Backend` is this crate's own `pub(crate)` marker, so a local
// trait on a foreign type is legal and the reverse is not. Every method it
// composes is impl'd in the vendor crate; this line adds no behaviour.
#[cfg(any(test, feature = "test-helpers"))]
impl Backend for PostgresBackend {}


/// Per-isolate backend handle — the typed enum stashed on
/// [`crate::context::ThreadDbContext`].
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
/// concrete backends into engine code. Both arms already called this same trait
/// method with identical arguments, so the branch was only ever SELECTING an
/// impl - and selecting an impl by vendor is precisely what this tier is for.
///
/// The `backend_unsupported("vector_search")` arm the old shape carried is not
/// reproduced, because it was unreachable: [`BackendHandle`] has exactly two
/// variants and neither is `#[cfg]`-gated, so the `else` of `as_sqlite()` was
/// always `Postgres`. Verified by reading the enum, not by test.
impl VectorIndex for BackendHandle {
    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        binding: &DbBinding,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: VectorMetric,
        filter: &serde_json::Value,
        schema: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        match self {
            // The SQLite arm has to ATTACH the app's database file before it
            // can scan it. That prelude sat at the engine call site; it belongs
            // to the arm that needs it, and nothing else has to know.
            Self::Sqlite(sq) => {
                sq.attach_app_file(binding.app_id()).await?;
                sq.vector_search(binding, collection, column, query, k, metric, filter, schema)
                    .await
            }
            Self::Postgres(pg) => {
                pg.vector_search(binding, collection, column, query, k, metric, filter, schema)
                    .await
            }
        }
    }
}

/// Vendor selection for spatial search. Same rationale as the [`VectorIndex`]
/// impl directly above, including the ATTACH prelude on the SQLite arm.
impl SpatialIndex for BackendHandle {
    #[allow(clippy::too_many_arguments)]
    async fn spatial_near(
        &self,
        binding: &DbBinding,
        collection: &str,
        column: &str,
        point: GeoPoint,
        radius_m: f64,
        filter: &serde_json::Value,
        limit: Option<usize>,
        schema: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        match self {
            Self::Sqlite(sq) => {
                sq.attach_app_file(binding.app_id()).await?;
                sq.spatial_near(binding, collection, column, point, radius_m, filter, limit, schema)
                    .await
            }
            Self::Postgres(pg) => {
                pg.spatial_near(binding, collection, column, point, radius_m, filter, limit, schema)
                    .await
            }
        }
    }
}

impl BackendHandle {
    /// Read the RAW sibling of a masked column as BYTES.
    ///
    /// The encrypted-storage half of unmask: the field's own column holds the
    /// mask, the ciphertext lives in the raw sibling. Bytes, not text - the
    /// sibling of an encrypted column is BYTEA on PostgreSQL and a BLOB on
    /// SQLite, and rendering either through a text path is what made every
    /// PostgreSQL unmask of an encrypted column fail (see the regression
    /// `unmask_encrypted_column_on_pg_reads_bytea_raw_sibling`).
    ///
    /// **Why the SQL is here and not in `crud::unmask`.** It was written twice
    /// in the engine, once per vendor, behind an `as_postgres()` /
    /// `as_sqlite()` downcast - the shape #119 exists to remove. Only the
    /// lowering differs (`$1` and a roled scalar read against `?1`, quoted
    /// identifiers and a typed cell); key resolution, AEAD and the wrap step
    /// above this are vendor-neutral and stayed put.
    pub(crate) async fn read_raw_column_bytes(
        &self,
        app_id: &str,
        collection: &str,
        raw_column: &str,
        row_pk: &str,
    ) -> Result<ScalarRead<Vec<u8>>, DbError> {
        match self {
            Self::Postgres(pg) => {
                let sql = format!(
                    "SELECT \"{raw_column}\" FROM \"{app_id}\".\"{collection}\" WHERE id = $1"
                );
                pg.read_roled_scalar_bytes(app_id, &sql, &[&row_pk]).await
            }
            Self::Sqlite(sq) => {
                let q_app = sq.quote_ident(app_id);
                let q_coll = sq.quote_ident(collection);
                let q_col = sq.quote_ident(raw_column);
                let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
                let typed = sq
                    .autocommit_client()
                    .query_typed_internal(&sql, &[row_pk])
                    .await?;
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

    /// Read the RAW sibling of a masked column as TEXT.
    ///
    /// The plaintext-storage half: the column carries `.mask({...})` WITHOUT
    /// `.encrypted(...)`, so the sibling holds the value in its own declared
    /// type. Distinct from [`Self::read_raw_column_bytes`] because the
    /// encrypted path must not go near a text rendering.
    pub(crate) async fn read_raw_column_text(
        &self,
        app_id: &str,
        collection: &str,
        raw_column: &str,
        row_pk: &str,
    ) -> Result<ScalarRead<String>, DbError> {
        match self {
            Self::Postgres(pg) => {
                let sql = format!(
                    "SELECT \"{raw_column}\" FROM \"{app_id}\".\"{collection}\" WHERE id = $1"
                );
                pg.read_roled_scalar_text(app_id, &sql, &[&row_pk]).await
            }
            Self::Sqlite(sq) => {
                let q_app = sq.quote_ident(app_id);
                let q_coll = sq.quote_ident(collection);
                let q_col = sq.quote_ident(raw_column);
                let sql = format!("SELECT {q_col} FROM {q_app}.{q_coll} WHERE id = ?1");
                let rows = sq.autocommit_client().query_internal(&sql, &[row_pk]).await?;
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
    pub(crate) async fn append_unmask_audit(
        &self,
        app_id: &str,
        row: &UnmaskAuditRow<'_>,
    ) -> Result<(), DbError> {
        match self {
            Self::Postgres(pg) => {
                let sql = format!(
                    r#"INSERT INTO "{app_id}"."__zeroship_audit_unmask"
                       (actor_id, actor_role, claimed_actor, collection, row_pk, "column",
                        classification, reason, outcome)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
                );
                pg.execute_roled(
                    app_id,
                    &sql,
                    &[
                        &row.actor_id,
                        &row.actor_role,
                        &row.claimed_actor,
                        &row.collection,
                        &row.row_pk,
                        &row.column,
                        &row.classification,
                        &row.reason,
                        &row.outcome,
                    ],
                )
                .await?;
                Ok(())
            }
            Self::Sqlite(sq) => {
                let q_app = sq.quote_ident(app_id);
                let sql = format!(
                    r#"INSERT INTO {q_app}."__zeroship_audit_unmask"
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
    pub(crate) async fn open_tx_session(
        &self,
        app_id: &str,
        begin: BeginIntent,
    ) -> Result<crate::context::TxConnection, OpenSessionError> {
        match self {
            Self::Postgres(pg) => {
                let client = pg.acquire_dedicated_client(app_id).await?;
                let begin_sql = postgres::render_begin(begin);
                pg.client_exec(&client, &begin_sql, &[]).await?;
                postgres::apply_per_app_role(&client, app_id).await?;
                Ok(crate::context::TxConnection::Postgres(client))
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
                Ok(crate::context::TxConnection::Sqlite(client))
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
    pub(crate) async fn prepare_for_app(&self, app_id: &str) -> Result<(), DbError> {
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
    pub(crate) async fn persist_mask_policy(
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
    pub(crate) async fn load_mask_policy(
        &self,
        app_id: &str,
    ) -> Result<Option<serde_json::Value>, DbError> {
        match self {
            Self::Postgres(_) => Ok(None),
            Self::Sqlite(sq) => sqlite::mask_policy_store::load(sq, app_id).await,
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
    pub(crate) fn as_postgres(&self) -> Option<&PostgresBackend> {
        match self {
            Self::Postgres(b) => Some(b),
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

    /// Borrow a [`ChangeStream`] adapter over the PG arm — returns the
    /// thin [`crate::change_stream_pg::PgChangeStream`] wrapper that
    /// re-routes `ensure_publication_and_slot` /
    /// `wal_consumer::run_supervised` through the trait surface.
    ///
    /// Introduced alongside the [`ChangeStream`] trait.
    /// Associated types (`type ConsumerHandle`) block dyn dispatch, so
    /// the consumer migration path mirrors the `as_postgres` /
    /// `as_sqlite` accessor shape rather than a `with_change_stream`
    /// visitor returning `R` (see the trait doc-comment for the dyn
    /// vs. concrete-accessor rationale).
    ///
    /// Returns `Some` on the PG arm; `None` on the SQLite arm.
    ///
    /// The adapter holds an `Rc<PostgresBackend>` (Rc-cloned from the
    /// arm's inner value); see [`crate::change_stream_pg::PgChangeStream`]
    /// for the lifetime / ownership rationale.
    pub fn as_change_stream_pg(&self) -> Option<crate::change_stream_pg::PgChangeStream> {
        match self {
            Self::Postgres(b) => Some(crate::change_stream_pg::PgChangeStream::new(b.clone())),
            Self::Sqlite(_) => None,
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
    pub(crate) fn key_store(&self) -> &crate::encryption::KeyStore {
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
mod tests {
    //! Interface-level (compile-time) tests for the `Backend` trait.
    //!
    //! The trait is `async fn`-in-trait and every method needs a real
    //! Postgres listener via [`PostgresBackend`]; we cannot exercise
    //! method bodies from a `#[test]` without `tests/integration.rs`.
    //! What we *can* do — and what catches the highest-leverage
    //! refactor mistakes — is pin the trait shape at compile time:
    //!
    //! - the canonical impl [`PostgresBackend`] satisfies the bound;
    //! - the associated types stay wired to their concrete
    //!   `compio_postgres` / `crate::diff` counterparts;
    //! - the `'static` bound on the trait flows through.
    //!
    //! Any future change to the trait (new method, swapped
    //! associated-type bound, lifetime tightening) trips one of these
    //! at `cargo build -p zeroship-plugin-db --tests` time, before any
    //! caller fails at a more distant site.

    use super::*;

    /// Compile-time: the canonical impl [`PostgresBackend`] satisfies
    /// the `Backend` trait. Function body type-checks at build time;
    /// it's a deliberate no-op at runtime.
    fn assert_postgres_backend_impls_backend() {
        fn assert_impl<T: Backend>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`SqlExecutor`] capability super-trait. If a future
    /// refactor accidentally pulls a `SqlExecutor` method back onto
    /// the omnibus `Backend` trait — or detaches the impl block from
    /// the `PostgresBackend` type — this stops compiling.
    fn assert_postgres_backend_impls_sql_executor() {
        fn assert_impl<T: SqlExecutor<Client = compio_postgres::OwnedPooledClient>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the carved
    /// [`LockManager`] capability super-trait. The
    /// `: SqlExecutor` super-bound on `LockManager` plus the
    /// `Client = compio_postgres::OwnedPooledClient` constraint here pin the
    /// shape end-to-end — a regression in either direction fails
    /// compilation in this module.
    fn assert_postgres_backend_impls_lock_manager() {
        fn assert_impl<T: LockManager<Client = compio_postgres::OwnedPooledClient>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies
    /// [`SchemaIntrospect`] with the associated type pinned to
    /// [`crate::diff::LiveSchema`].
    fn assert_postgres_backend_impls_schema_introspect() {
        fn assert_impl<T: SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the PG-only
    /// [`PgSqlExecutor`] extension trait. The free-function
    /// helper path hinges on `pool_handle()` being reachable through
    /// this trait without naming `PostgresBackend`.
    fn assert_postgres_backend_impls_pg_sql_executor() {
        fn assert_impl<T: PgSqlExecutor>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: [`PostgresBackend`] satisfies the PG-only
    /// [`PgLockManager`] extension trait. The returned `PooledClient<'p>` keeps
    /// the `'p` lifetime threaded through
    /// [`LockGuard`] without needing a GAT on
    /// [`LockManager`] (Open Q5 resolution).
    fn assert_postgres_backend_impls_pg_lock_manager() {
        fn assert_impl<T: PgLockManager>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: the PG-arm [`ChangeStream`] adapter
    /// [`crate::change_stream_pg::PgChangeStream`] satisfies the
    /// [`ChangeStream`] trait with the agreed
    /// `ConsumerHandle = WalConsumerHandle` shape. A
    /// regression that detaches the impl block from `PgChangeStream`
    /// — or that renames the associated type away from the agreed
    /// shape — trips compilation here rather than at the
    /// `BackendHandle::as_change_stream_pg()` accessor or its
    /// consumers.
    fn assert_pg_change_stream_impls_change_stream() {
        fn assert_impl<
            T: ChangeStream<ConsumerHandle = crate::change_stream_pg::WalConsumerHandle>,
        >() {
        }
        assert_impl::<crate::change_stream_pg::PgChangeStream>();
    }

    /// Compile-time: the [`VectorIndex`] trait's shape is
    /// pinned. Ships no impl — neither [`PostgresBackend`] nor
    /// [`SqliteBackend`] yet satisfies the trait, so this assertion
    /// only checks that the trait *itself* compiles (object-safety,
    /// `async fn` placement, signature shape). A later change will
    /// instantiate this against the concrete backends.
    #[allow(dead_code)]
    fn _assert_vector_index<T: VectorIndex>() {}

    /// Compile-time: the [`SpatialIndex`] trait's shape is
    /// pinned. Ships no impl — neither backend yet satisfies the
    /// trait. A later change will instantiate this against the concrete
    /// backends.
    #[allow(dead_code)]
    fn _assert_spatial_index<T: SpatialIndex>() {}


    /// Compile-time: the [`Backup`] trait's shape is pinned. Both
    /// backends implement it; the per-backend instantiations are
    /// below.
    #[cfg(feature = "test-helpers")]
    #[allow(dead_code)]
    fn _assert_backup<T: Backup>() {}

    /// Compile-time: both backends expose the encryption key store, and
    /// `BackendHandle` reaches it without naming either.
    ///
    /// This replaces three `EncryptedColumn` trait-shape pins deleted with the
    /// trait on 2026-09-02. What is worth pinning changed with it: the old pins
    /// asserted that each vendor satisfied a per-vendor crypto trait, which is
    /// the coupling we removed. What must not regress is the opposite - that
    /// the key store stays reachable through ONE dialect-neutral accessor, so
    /// no caller has to reopen a two-arm match to encrypt a column.
    #[allow(dead_code)]
    fn _assert_key_store_is_dialect_neutral() {
        fn assert_store<T: Fn(&BackendHandle) -> &crate::encryption::KeyStore>(_: T) {}
        assert_store(BackendHandle::key_store);
    }

    /// Compile-time: `PostgresBackend` satisfies [`Backup`].
    /// The `pg_dump`/`pg_restore` shell-out body is backfilled; the
    /// PITR placeholder still targets a `__zeroship_admin.pitr_targets`
    /// table that no longer has an installer. Mirrors the
    /// `_assert_postgres_backend_impls_encrypted_column` shape above.
    #[cfg(feature = "test-helpers")]
    #[allow(dead_code)]
    fn _assert_postgres_backend_impls_backup() {
        fn assert_impl<T: Backup>() {}
        assert_impl::<PostgresBackend>();
    }

    /// Compile-time: `SqliteBackend` satisfies [`Backup`].
    /// The `VACUUM INTO` body is backfilled separately.
    #[cfg(feature = "test-helpers")]
    #[allow(dead_code)]
    fn _assert_sqlite_backend_impls_backup() {
        fn assert_impl<T: Backup>() {}
        assert_impl::<SqliteBackend>();
    }

    /// Compile-time: the associated types stay anchored to the concrete
    /// `compio_postgres::Client` / `crate::diff::LiveSchema`. A
    /// regression here would silently change every `B::Client` /
    /// `B::LiveSchema` consumer's expectations. `LiveSchema` flows
    /// through [`SchemaIntrospect`] now (moved off
    /// `Backend`); `Backend` re-anchors it via the
    /// `SchemaIntrospect<LiveSchema = LiveSchema>` super-bound so the
    /// `Backend<LiveSchema = …>` shorthand below still resolves.
    fn assert_associated_types_pinned() {
        fn pinned_client<T: Backend<Client = compio_postgres::OwnedPooledClient>>() {}
        fn pinned_live_schema<T: Backend<LiveSchema = crate::diff::LiveSchema>>() {}
        pinned_client::<PostgresBackend>();
        pinned_live_schema::<PostgresBackend>();
    }

    /// Compile-time: `Backend: 'static`. The per-isolate context parks
    /// the impl behind a `BackendHandle::Postgres(Rc<PostgresBackend>)`
    /// in a `thread_local!`; dropping the `'static` bound would break
    /// that path.
    fn assert_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<PostgresBackend>();
    }

    /// Compile-time: [`BackendHandle`] is `Clone + 'static`. The
    /// per-isolate context's accessor (`ThreadDbContext::backend`)
    /// returns a cloned handle by value so consumers can hold it
    /// across awaits without keeping the `RefCell` borrow open; the
    /// `Clone` bound is therefore load-bearing. The `'static` bound
    /// flows through because the enum's only data is `Rc<…>` of
    /// `'static` impls.
    ///
    /// This test
    /// replaced the deleted `assert_backend_handle_alias` that pinned
    /// `BackendHandle == Rc<PostgresBackend>`. The alias is gone;
    /// what stays is the shape contract the consumers depend on.
    fn assert_backend_handle_clone_static() {
        fn assert_bounds<T: Clone + 'static>() {}
        assert_bounds::<BackendHandle>();
    }

    /// Construct a `BackendHandle::Postgres(…)` arm via the public API
    /// surface. Pins the variant name so a future rename trips
    /// compilation here rather than at every consumer site, and
    /// proves [`BackendHandle::as_postgres`] dispatches through the PG
    /// arm without panic.
    ///
    /// It also exercised a `with_postgres` closure accessor until
    /// 2026-09-02. That accessor and its SQLite twin were DELETED: they had
    /// no caller anywhere except this pin, and a pin is not a use. The
    /// `as_*` reference forms are what the crate actually calls - six
    /// production sites in `crud/unmask.rs` alone - so the shape this test
    /// protects is unchanged.
    ///
    /// Skipped under `--cfg miri` (the only sandbox where the PG
    /// `Rc<…>` construction below would be problematic): the test
    /// never connects, but the constructor path still exists.
    #[test]
    fn backend_handle_postgres_arm_round_trip() {
        // We deliberately can't call `PostgresBackend::new` here
        // without a real `compio_postgres::Pool` (which only
        // `Pool::connect` produces — covered by tests/integration.rs).
        // What we *can* pin at unit-test time is the compile-time
        // shape: that `BackendHandle::Postgres` is constructible from
        // `Rc<PostgresBackend>` and that the two accessors return the
        // expected reference / closure-applied value.
        //
        // The runtime exercise of these accessors against a live
        // PostgresBackend lives in tests/integration.rs (which spins
        // up Postgres). This test pins the *type* shape.
        fn _shape_check(handle: BackendHandle) -> bool {
            // The accessor returns `Option<…>` (the PG
            // arm yields `Some(…)`; the SQLite arm yields `None`).
            let _: Option<&PostgresBackend> = handle.as_postgres();
            true
        }
        let _ = _shape_check as fn(BackendHandle) -> bool;
    }

    /// Compile-time: [`LockScope`] satisfies the trait
    /// bounds the typed [`LockManager`] API depends on. The variant
    /// is `Clone + 'static` so call sites can stash it across awaits
    /// (e.g. the `release_scope` re-construction in `migrations.rs`'s
    /// cancelled-refusal path) without re-borrowing. Not `Send`/`Sync`
    /// — same Open Q4 reasoning as the rest of the backend traits:
    /// the compio runtime is single-threaded per worker.
    fn assert_lock_scope_clone_send_static() {
        fn assert_bounds<T: Clone + 'static>() {}
        assert_bounds::<LockScope>();
    }

    /// Compile-time + runtime: construct both
    /// [`LockScope`] variants and dispatch through
    /// [`PostgresBackend::try_acquire`] to verify the typed
    /// keyed-mapping wires through. We can't actually issue SQL
    /// without a live Pool (covered by tests/integration.rs), but we
    /// CAN exercise the key-derivation logic ([`LockScope::to_keys`])
    /// and confirm both variants produce the canonical
    /// `(format!("{app_id}:{name}"), name)` shape.
    ///
    /// **Why both variants here**: the production sites are all
    /// `GlobalApp`; `LocalApp` exists today purely as a classification
    /// hook for future call sites (see [`LockScope`] rustdoc). Pinning
    /// the shape here ensures a future contributor adding a `LocalApp`
    /// production caller doesn't accidentally drift the key
    /// derivation between variants.
    #[test]
    fn lock_scope_keys_global_app_canonical_shape() {
        let scope = LockScope::GlobalApp {
            app_id: "app_42".to_string(),
            name: "snapshot_restore".to_string(),
        };
        let (k1, k2) = scope.to_keys();
        assert_eq!(k1, "app_42:snapshot_restore");
        assert_eq!(k2, "snapshot_restore");
        assert_eq!(scope.app_id(), "app_42");
        assert_eq!(scope.name(), "snapshot_restore");
    }

    #[test]
    fn lock_scope_keys_local_app_canonical_shape() {
        // `LocalApp` produces the SAME (key1, key2) shape as
        // `GlobalApp` — the variant classifies *visibility* (which
        // backend primitive handles dispatch) not *key layout*. A
        // future SQLite backend would HashMap on the derived strings
        // for both variants; the PG backend currently treats `LocalApp`
        // the same as `GlobalApp` (only `GlobalApp` callers exist
        // today).
        let scope = LockScope::LocalApp {
            app_id: "app_99".to_string(),
            name: "mig:add_archived_flag".to_string(),
        };
        let (k1, k2) = scope.to_keys();
        assert_eq!(k1, "app_99:mig:add_archived_flag");
        assert_eq!(k2, "mig:add_archived_flag");
        assert_eq!(scope.app_id(), "app_99");
        assert_eq!(scope.name(), "mig:add_archived_flag");
    }

    /// Compile-time: the typed [`LockManager::try_acquire`]
    /// API dispatches the canonical `LockScope` shape through
    /// [`PostgresBackend`] without the caller naming the underlying
    /// `(key1, key2)` string-key primitive. We can't issue SQL from a
    /// unit test, so this is a *type-shape* check: the function body
    /// type-checks against the trait method signature.
    #[allow(dead_code)]
    async fn assert_lock_scope_dispatches_through_try_acquire(
        backend: &PostgresBackend,
        client: &compio_postgres::OwnedPooledClient,
    ) -> Result<bool, DbError> {
        // GlobalApp arm — exercises acquire / try_acquire / release.
        let global = LockScope::GlobalApp {
            app_id: "app_t".into(),
            name: "snapshot_restore".into(),
        };
        let _ = backend.try_acquire(client, &global).await?;
        // `acquire` is on the policy extension trait, not the contract - the
        // import here is itself the shape check that a caller can still reach
        // the bounded surface off a plain `LockManager`.
        {
            use crate::lock_policy::BoundedLockAcquire;
            backend.acquire(client, &global).await?;
        }
        backend.release(client, &global).await?;

        // LocalApp arm — same dispatch surface (variant classifies
        // visibility, not key layout).
        let local = LockScope::LocalApp {
            app_id: "app_t".into(),
            name: "mig:add_archived_flag".into(),
        };
        backend.try_acquire(client, &local).await
    }

    #[test]
    fn compile_time_assertions_link() {
        // Keep the asserter functions live so the dead-code lint
        // doesn't fire. The type-check still runs even if these
        // weren't called, but the explicit cast documents intent.
        let _ = assert_postgres_backend_impls_backend as fn();
        let _ = assert_postgres_backend_impls_sql_executor as fn();
        let _ = assert_postgres_backend_impls_lock_manager as fn();
        let _ = assert_postgres_backend_impls_schema_introspect as fn();
        let _ = assert_postgres_backend_impls_pg_sql_executor as fn();
        let _ = assert_postgres_backend_impls_pg_lock_manager as fn();
        let _ = assert_pg_change_stream_impls_change_stream as fn();
        let _ = assert_associated_types_pinned as fn();
        let _ = assert_backend_is_static as fn();
        let _ = assert_backend_handle_clone_static as fn();
        let _ = assert_lock_scope_clone_send_static as fn();
        // `assert_lock_scope_dispatches_through_try_acquire` is not
        // a `fn()` — it has lifetime parameters and returns a Future.
        // The fn-item cast above already exercises its signature; we
        // don't need to re-cast it here.
    }
}
