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
//! which is this crate's own `pub(crate)` trait and is pinned here by the
//! orphan rule.
//!
//! # Why it may name both vendors
//!
//! The enum has a `Postgres(Rc<PostgresBackend>)` arm and a `Sqlite(..)` arm,
//! which is a vendor-embedding violation everywhere except the tier that
//! dispatches. `data-engine` sits ABOVE both vendor crates and is exactly that
//! tier. The two trait impls below are the point: vendor selection for vector
//! and spatial search lives HERE so the engine's call sites never downcast.
//!
//! See #164 for why decision 4's "adding a database must require ZERO changes
//! to data-engine" overstates the rule this enum has to satisfy - a closed sum
//! necessarily gains an arm per backend; what it must not gain is SQL.

use std::rc::Rc;

use serde_json::Value;

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;
use zeroship_schema::descriptors::{GeoPoint, VectorMetric};

use crate::backend::{
    ChangeStream, DialectBuilder, LockManager, PostgresBackend, ScalarRead,
    SpatialIndex, SqlExecutor, SqliteBackend, UnmaskAuditRow, VectorIndex, postgres, sqlite,
};
use zeroship_data_core::error::{BeginIntent, OpenSessionError};

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
    ) -> Result<crate::tx_lanes::TxConnection, OpenSessionError> {
        match self {
            Self::Postgres(pg) => {
                let client = pg.acquire_dedicated_client(app_id).await?;
                let begin_sql = postgres::render_begin(begin);
                pg.client_exec(&client, &begin_sql, &[]).await?;
                postgres::apply_per_app_role(&client, app_id).await?;
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
    pub(crate) fn pool_counts(&self) -> Option<(usize, usize, usize)> {
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
