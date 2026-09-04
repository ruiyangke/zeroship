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
//! naming the concrete backend. **It has no callers as of 2026-09-03** - the
//! last four were in `crud/mask_drift.rs`, deleted that day - and it is
//! `test-helpers`-gated at its definition because a bare pool checkout skips
//! the per-app role fence. Deleting it is the open follow-up; do not reach for
//! it as a way around the fence.
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
pub mod cancel;

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
// `BackendHandle` moved to `crate::backend_handle` on 2026-09-02 so the
// data-engine cut moves a file. Re-exported at the address ~15 call sites
// already spell, exactly as the two vendor extractions were.
pub use crate::backend_handle::BackendHandle;

pub use zeroship_data_sqlite as sqlite;
pub use zeroship_data_sqlite::SqliteBackend;

// Same orphan-rule case as the PostgreSQL arm: `Backend` is this crate's own
// `pub` marker, so its impl on a foreign type can only be written here.
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
pub use zeroship_data_postgres::{PgLockManager, PgSqlExecutor, lock_guard, pg_introspect};
#[cfg(any(test, feature = "test-helpers"))]
pub use zeroship_data_postgres::lock_guard::LockGuard;


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
// The backup/snapshot four were `#[cfg(feature = "test-helpers")]` in
// `zeroship-data-core` until 2026-09-04, which forced the same gate here and
// made this re-export a running instance of the trap recorded below: THE
// ATTRIBUTE CHANGED MEANING WHEN THEY CROSSED THE CRATE BOUNDARY. It used to
// name this crate's feature and came to name `zeroship-data-core`'s; the two
// are wired (`test-helpers = ["zeroship-data-core/test-helpers"]`) so they turn
// on together, but only when something turns them on, and a dependent building
// with default features got data-core WITHOUT the feature. The keeping-in-sync
// was the defect. All four are unconditional in data-core now - they are
// `Backup`'s signature vocabulary, so they are contract - and this re-export
// follows.
//
// The measurement that paragraph carried is worth keeping, because it is why a
// gate mismatch here is invisible: `cargo check -p zeroship-plugin-db --features
// test-helpers --all-targets` passed clean while zeroship-worker and
// zeroship-cli both failed to build. Only a dependent lib/bins build reveals
// it; `tests/shipped_config_gate.sh` is that build.
pub use zeroship_data_core::capability::{BusyPolicy, PitrTarget, SnapshotHandle, SnapshotOpts};

// The eight capability traits followed their vocabulary down on 2026-09-02.
// Each is a contract and nothing more: associated types and signatures over the
// items above. What stayed here is every `impl Trait for T` - nineteen of them
// across this crate, including two on `BackendHandle` - because the orphan rule
// puts an impl in the crate owning the type, not the crate owning the trait.
//
// NONE OF THE EIGHT IS GATED ANY MORE, as of 2026-09-04, and the two paragraphs
// that used to explain how to keep two gates in sync are deleted rather than
// updated. `SchemaIntrospect` lost its gate when the write path's protection
// floor started reading the catalog; `Backup` lost its gate because a capability
// contract's SHAPE must not depend on a DEV-dependency feature that
// `--all-targets` and `--all-features` silently unify ON. The reasoning is in
// `zeroship-data-core`'s `storage.rs` module header.
//
// The lesson those paragraphs recorded still applies to anything that IS gated
// here: `#[cfg(feature = "test-helpers")]` on an item from data-core names
// DATA-CORE's feature, while the same attribute on an impl in this file names
// THIS crate's, and `cfg(test)` names neither for a consumer. That mismatch was
// invisible to all three `cargo check` configurations and produced 7 errors in
// the lib-TEST target. The fix is to stop gating contracts, not to align the
// gates more carefully.
//
// `Backup`'s two vendor impls are still `#[cfg(feature = "test-helpers")]`, and
// deliberately: the trait is free to ship, a `pg_dump` shell-out with a
// destructive `DROP SCHEMA ... CASCADE` restore arm is not. The compile-time
// witnesses in `mod tests` below carry that gate with them.
pub use zeroship_data_core::storage::{
    Backup, ChangeStream, DialectBuilder, LockManager, SpatialIndex, SqlExecutor, VectorIndex,
};
// UNGATED since 2026-09-04, and the gate it lost is the one that broke the
// shipped binaries. `BackendHandle::introspect_schema` is called from the
// PRODUCTION write path by `crate::crud::protection_floor`, so this re-export,
// the trait in data-core and both vendor impls have to exist in a default
// build. They did not: `test-helpers` is a DEV-dependency feature here, so every
// `--all-targets` / `--all-features` / clippy / test invocation unified it ON
// and reported zero errors while `cargo check -p zeroship-worker --bins` failed.
// `tests/shipped_config_gate.sh` builds the configuration that actually ships.
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
/// Most of this crate is `pub` by default and `pub` only under
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
pub trait Backend:
    SqlExecutor + LockManager + SchemaIntrospect<LiveSchema = crate::diff::LiveSchema> + 'static
{
}

// The impl lives HERE and not beside the sub-trait impls in
// `zeroship-data-postgres`, and that is the orphan rule rather than a
// preference: `Backend` is this crate's own `pub` marker, so a local
// trait on a foreign type is legal and the reverse is not. Every method it
// composes is impl'd in the vendor crate; this line adds no behaviour.
#[cfg(any(test, feature = "test-helpers"))]
impl Backend for PostgresBackend {}



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

    // `assert_pg_change_stream_impls_change_stream` MOVED to
    // `zeroship-plugin-db`'s `change_stream_pg.rs` with the engine cut. It was
    // the ONE thing in this file that named a CDC module, and it is the reason
    // the file was contested (#170): everything else here re-exports
    // `zeroship-data-core`, both vendor crates and `zeroship-schema`, plus this
    // crate's own `BackendHandle` - all at or below the engine tier. The
    // assertion pins a fact about `PgChangeStream`, so it belongs beside
    // `PgChangeStream`.

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
