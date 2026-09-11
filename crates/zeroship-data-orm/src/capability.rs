//! The vocabulary the storage-capability traits speak in.
//!
//! Rank 0: every item here is a value type or a constant. None names a database
//! driver, a runtime, or V8, and none carries behaviour that does - which is
//! what makes them movable at all. The traits that consume them
//! (`DatabaseFixture`, `LockManager`, `ChangeStream`, ...) follow in a later batch;
//! moving the vocabulary first means those traits arrive with nothing left to
//! drag behind them.
//!
//! Moved from `zeroship-data-v8`'s `backend/mod.rs` on 2026-09-02. Two things
//! were fixed in transit rather than carried:
//!
//! * `LockScope`'s 32-line rustdoc had been silently re-attached to
//!   `SNAPSHOT_RESTORE_LOCK_TAG`, because the constant was inserted between the
//!   doc block and the enum it documents. `LockScope` carried no doc at all.
//!   Both now own their own text.
//! * `LockScope::app_id` and `::name` were `pub(crate)`. A crate boundary makes
//!   that invisible to their only caller (`zeroship-data-v8`'s lock policy),
//!   so both are `pub` here. This is the Phase 0.5 audit-1 promotion the split
//!   requires, applied at the moment it becomes load-bearing rather than in a
//!   sweep - the compiler is the oracle for it in this direction.
//!
//! # NO `cfg(feature)` IN THIS FILE
//!
//! Four types here - `SnapshotOpts`, `BusyPolicy`, `SnapshotHandle` and a
//! `PitrTarget` since deleted - carried `#[cfg(test)]`
//! until 2026-09-04, because their only consumer,
//! [`crate::storage::Backup`], carried it too.
//! Both gates are gone. Every one of the four appears in a `Backup` method
//! SIGNATURE, so they are not vocabulary the contract happens to use, they are
//! part of it: a build that cannot name `SnapshotOpts` cannot state the
//! contract, and gating them made a whole capability's shape depend on a
//! DEV-dependency feature that `--all-targets` and `--all-features` silently
//! turn on. See `storage.rs`'s module header for the three breakages that came
//! out of exactly that.
//!
//! They cost nothing to ship: a `String`, a `[u8; 32]`, two `u64`s and two
//! fieldless enums, no dependency of any kind, and no code at all until
//! something constructs one. The gate that DOES still apply here is the one on
//! the vendor impls, which is where the `pg_dump` shell-out and the `sha2` it
//! hashes with live.
//!
//! Ungating cost three DEAD DOC LINKS their invisibility, and that is the
//! second-order effect to expect from any ungating here. The three bare
//! `Backup::*` links below resolved to nothing - `Backup` lives in
//! [`crate::storage`] and is not
//! imported into this module - but a DEFAULT `cargo doc` never rendered items
//! that were `cfg`-gated out of it, so `tests/run_doc_gate.sh` never saw them.
//! They are qualified paths now. The same thing happened to `Catalog`'s
//! two `crate::catalog::` links in `storage.rs` the day before, for the same
//! reason. Measured over `cargo doc -p zeroship-data-core --no-deps`: 20
//! unresolved links under `--all-features` before, 16 after; 16 either way on
//! default features.

/// Typed classification of every advisory-lock acquisition the
/// plugin-db crate performs.
///
/// **§7 / §10.5 distinction** (see
/// `docs/archive/db-system-design.md`):
///
/// - [`LockScope::GlobalApp`] — **cross-process visibility**. The lock
///   must be observable by every worker process pointed at the same
///   logical database. Postgres maps this to
///   `pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)` —
///   visible cluster-wide. A future SQLite backend would map it to
///   either `BEGIN EXCLUSIVE` (when the lock duration aligns with a
///   transaction) or a sentinel-row in a `__zs_locks` table (for
///   session-scoped duration). Snapshot and restore use this category so two
///   worker processes cannot replace one app's database concurrently.
///
/// - [`LockScope::LocalApp`] — **single-process visibility**. The lock
///   coordinates work inside one worker's Rust runtime — backed by an
///   in-process Rust HashMap registry, NOT by SQL. No SQL is issued;
///   the backend impl maps this to whatever in-memory coordination
///   primitive that backend already runs. The variant exists today
///   so call sites can classify their intent explicitly; there is no
///   production `LocalApp` caller yet — every site is `GlobalApp`.
///
/// **§7.2 / §10.5 key-naming convention**: implementations derive the
/// underlying `(key1, key2)` string-key pair from the variant fields
/// as `(format!("{app_id}:{name}"), name)`. The PG impl then hashes
/// each key through `hashtext()` (§7.2) before passing to
/// `pg_advisory_lock`. SQLite-future impls would use the strings
/// directly as keys into a per-process HashMap (`GlobalApp` and
/// `LocalApp` both, since SQLite is in-process by definition;
/// §8.5). The variant classifies *visibility*, not key shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockScope {
    /// Cluster-wide / cross-process advisory lock. Visible to every
    /// worker pointed at the same database. PG maps to
    /// `pg_advisory_lock`; SQLite-future would map to either
    /// `BEGIN EXCLUSIVE` or a sentinel-row primitive.
    GlobalApp {
        /// App identifier — first half of the key namespace.
        app_id: String,
        /// Scope name tag, e.g. `"snapshot_restore"`,
        /// `"mig:add_archived_flag"`. Both halves of the underlying
        /// `(key1, key2)` advisory-lock pair derive from this field
        /// (see [`Self::to_keys`]).
        name: String,
    },
    /// Single-process / in-Rust advisory lock. Coordinates work inside
    /// one worker's Rust runtime — backed by an in-memory HashMap
    /// registry, NOT by SQL. No production caller yet; the variant
    /// exists so future call sites can classify their intent.
    #[allow(
        dead_code,
        reason = "LocalApp remains part of the lock model for test-helper coverage even though the release build only constructs GlobalApp."
    )]
    LocalApp {
        /// App identifier — first half of the key namespace.
        app_id: String,
        /// Scope name tag.
        name: String,
    },
}

impl LockScope {
    /// Derive the `(key1, key2)` string-key pair the underlying
    /// advisory-lock primitive consumes.
    ///
    /// **§7.2 / §10.5 convention**: `key1 = "{app_id}:{name}"`,
    /// `key2 = name`. PG impls layer `hashtext($k)::int4` over the
    /// returned strings; SQLite-future impls would use them directly
    /// as text keys in a per-process HashMap. The mapping is
    /// identical for both [`Self::GlobalApp`] and [`Self::LocalApp`]
    /// — the variant classifies *visibility*, not key shape.
    pub fn to_keys(&self) -> (String, String) {
        match self {
            Self::GlobalApp { app_id, name } | Self::LocalApp { app_id, name } => {
                (format!("{app_id}:{name}"), name.clone())
            }
        }
    }

    /// Borrow the `app_id` field regardless of variant. Convenience
    /// for log-rendering and audit-row metadata that doesn't care
    /// about visibility class.
    pub fn app_id(&self) -> &str {
        match self {
            Self::GlobalApp { app_id, .. } | Self::LocalApp { app_id, .. } => app_id.as_str(),
        }
    }

    /// Borrow the `name` field regardless of variant. Convenience
    /// for log-rendering and audit-row metadata.
    pub fn name(&self) -> &str {
        match self {
            Self::GlobalApp { name, .. } | Self::LocalApp { name, .. } => name.as_str(),
        }
    }
}

/// Name of the per-app lock shared by snapshot and restore.
pub const SNAPSHOT_RESTORE_LOCK_TAG: &str = "snapshot_restore";

/// Options for [`Backup::snapshot`](crate::storage::Backup::snapshot).
///
/// Today carries only [`Self::if_busy`]; reserved so future work can
/// add compression / encryption-at-rest knobs without changing the
/// trait method signature.
#[derive(Debug, Clone)]
pub struct SnapshotOpts {
    pub if_busy: BusyPolicy,
}

/// Policy when a snapshot can't be taken immediately (e.g. SQLite
/// `VACUUM INTO` hitting `SQLITE_BUSY` on a schema-change race).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Surface a typed `Configuration { code: "backup_busy" }` error
    /// to the caller immediately. The caller decides whether to retry.
    Abort,
    /// In-trait retry with a small backoff (3 × 1s).
    Retry,
}

/// Handle returned by [`Backup::snapshot`](crate::storage::Backup::snapshot) -
/// the address + integrity metadata needed to restore.
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    /// Where the snapshot lives. Convention:
    /// `s3://<bucket>/snapshots/<app>/<ts>-<hash>.<ext>` or
    /// `file:///<path>`.
    pub uri: String,
    /// SHA-256 over the snapshot bytes, computed streamingly on the
    /// way out. The restore path re-hashes the download and refuses
    /// on mismatch.
    pub content_hash: [u8; 32],
    /// Wall-clock time of snapshot start, milliseconds since UNIX
    /// epoch.
    pub created_at_ms: u64,
}

/// The result of selecting a single cell: `SELECT <col> FROM <t> WHERE id = ?`.
///
/// Two independent things can go missing, and callers give each its own error,
/// so collapsing them into one `Option` would throw away the distinction. The
/// unmask readers report [`Self::NoRow`] as `unmask_not_found` and
/// [`Self::Null`] as `unmask_value_null`; those codes are engine-tier policy
/// and are minted by the caller, not here.
///
/// Lived in `backend::pg_autocommit` until 2026-09-02. Both vendors produce it
/// now, so it sits in the tier that dispatches between them.
#[derive(Debug)]
pub enum ScalarRead<T> {
    /// The query matched no row.
    NoRow,
    /// The row exists and the selected column is SQL NULL.
    Null,
    /// The row exists and the column holds a value.
    Value(T),
}

/// The nine columns of one `__zeroship_audit_unmask` row.
///
/// A struct rather than nine positional parameters: the six trailing ones are
/// all `&str`, so a transposed pair would compile and land the wrong value in
/// an append-only audit table.
#[derive(Debug)]
pub struct UnmaskAuditRow<'a> {
    /// Trusted actor identity, or empty when the call carried none.
    pub actor_id: &'a str,
    /// Trusted actor role, or empty. Never populated from a claim the DB-3
    /// sanitiser rejected - that goes in `claimed_actor`.
    pub actor_role: &'a str,
    /// The REFUSED claim, serialised whole and untrusted. Kept apart from the
    /// trusted columns so an operator reading the row cannot confuse what a
    /// handler sent with what the runtime established.
    pub claimed_actor: &'a str,
    pub collection: &'a str,
    pub row_pk: &'a str,
    pub column: &'a str,
    pub classification: &'a str,
    pub reason: &'a str,
    pub outcome: &'a str,
}
