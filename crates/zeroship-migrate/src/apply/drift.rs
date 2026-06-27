//! Drift detection (design §5 "Drift", scenarios 35 + 36) — **read-only**.
//!
//! Drift is any divergence between what the journal says happened and either
//! (a) the migration set the operator now ships, or (b) the live database
//! schema. This module **surfaces** drift; it NEVER emits DDL and NEVER mutates
//! anything (design §5: *"surface, don't auto-fix"*). The whole module runs as
//! the admin/read connection over `information_schema`/`pg_catalog`, never as
//! the privileged `migrator` role, and binds every identifier so an injected
//! schema/table name cannot break the introspection queries.
//!
//! Two independent axes:
//!
//! - **B1 — checksum / tamper / orphan drift** ([`check_checksum_drift`]):
//!   compares the journal's recorded checksum for each NET-applied version
//!   against the checksum of the same version in the supplied set. A mismatch
//!   means the migration SQL was edited after it applied, or the journal row was
//!   tampered (design §1.5 / scenario 36). A net-applied version with NO matching
//!   migration in the supplied set is an **orphan** ([`OrphanJournal`]) — the
//!   bundle is missing a migration the database already has. This is the exact
//!   comparison the executor's apply flow does as its abort-on-drift pre-check
//!   (design §2.3 step 3); [`apply`](crate::apply) calls this function and aborts
//!   if it returns any [`ChecksumDrift`], so the report and the gate share one
//!   implementation.
//!
//! - **B2 — structural introspection** ([`snapshot_schema`] + [`diff_snapshots`]):
//!   introspect the LIVE project schema into a deterministic [`SchemaSnapshot`]
//!   and `diff` it against an **expected** snapshot the CALLER supplies. The
//!   expected snapshot is owned by the control-plane / authoring layer (it holds
//!   the declared/union schema, design §0); this module does NOT rebuild a schema
//!   model by replaying DDL — that is the authoring layer's job. `diff_snapshots`
//!   is a pure function returning a [`StructuralDrift`] report; it never returns
//!   DDL.

use std::collections::BTreeMap;

use compio_postgres::Client;

use crate::conn::ExecutorConfig;
use crate::model::ir::IdentityCol;
use crate::apply::journal::{self, AppliedEntry, JournalError, Phase};
use crate::model::migration::Migration;

// ---------------------------------------------------------------------------
// B1 — checksum / tamper / orphan drift
// ---------------------------------------------------------------------------

/// A net-applied version whose journal checksum no longer matches the supplied
/// set's checksum for that version — tamper / edited-after-applied (scenario 36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumDrift {
    /// The drifting migration's version (`mig_…`).
    pub version: String,
    /// The checksum recorded in the journal (the latest `completed` event).
    pub recorded: String,
    /// The checksum of the migration now in the supplied set.
    pub expected: String,
}

/// A net-applied version with NO corresponding migration in the supplied set —
/// the journal knows of a migration the shipped bundle does not (a dropped slice,
/// a downgrade). Surfaced, not silently ignored (executor M1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanJournal {
    /// The orphaned version recorded as net-applied in the journal.
    pub version: String,
    /// The journal's recorded checksum for it.
    pub recorded: String,
}

/// Error from a drift query.
#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    /// A database error.
    #[error("drift db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal read failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// A **dialect-neutral** backend error (non-Postgres
    /// [`MigrationBackend`](crate::backend::MigrationBackend) impls). See
    /// [`crate::executor::ApplyError::Backend`]. The Postgres impl never
    /// constructs this arm.
    #[error("drift backend error: {0}")]
    Backend(String),
}

/// The result of [`check_checksum_drift`]: the per-version checksum mismatches
/// plus the journal versions absent from the supplied set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChecksumDriftReport {
    /// Versions whose journal checksum disagrees with the supplied set.
    pub checksum_drift: Vec<ChecksumDrift>,
    /// Net-applied versions with no migration in the supplied set.
    pub orphan_journal: Vec<OrphanJournal>,
}

impl ChecksumDriftReport {
    /// True if neither tamper nor orphan drift was found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.checksum_drift.is_empty() && self.orphan_journal.is_empty()
    }
}

/// Compare the journal's NET-applied checksums against the supplied migration
/// set (design §2.3 step 3 / §5).
///
/// For each net-applied version (the latest event is `completed`, per
/// [`journal::applied`]):
///
/// - the supplied set has a migration with that version whose checksum differs
///   ⇒ [`ChecksumDrift`] (the migration SQL was mutated after apply, or the
///   journal row was tampered — scenario 36);
/// - the supplied set has NO migration with that version ⇒ [`OrphanJournal`].
///
/// The recorded checksum used is the one [`journal::applied`] returns, which is
/// the **latest `completed` event's** checksum for the version — correct across
/// rollback↔re-apply cycles (a re-applied migration's checksum is its newest
/// incarnation, not a stale earlier one).
///
/// This is the canonical comparison; [`apply`](crate::apply) calls it as its
/// abort-on-drift pre-check (it aborts if [`checksum_drift`](ChecksumDriftReport::checksum_drift)
/// is non-empty), so the report and the apply gate cannot diverge.
///
/// **Read-only.** No mutation, no DDL.
///
/// # Errors
/// [`DriftError::Journal`] if the journal read fails.
pub async fn check_checksum_drift(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<ChecksumDriftReport, DriftError> {
    let applied = journal::applied(conn, cfg).await?;
    Ok(compare_applied_to_set(&applied, migrations))
}

/// The **dialect-agnostic** core of [`check_checksum_drift`]: compare a set of
/// net-applied journal entries (already read by the dialect-coupled `applied`)
/// against the supplied migration set, producing the [`ChecksumDriftReport`].
///
/// Extracted so EVERY [`MigrationBackend`](crate::backend::MigrationBackend) impl
/// shares ONE comparison — the Postgres path and the SQLite path both call this
/// with their own `applied()` read, so the repeatable-exemption / kind-mismatch /
/// tamper / orphan rules can never diverge across dialects (design §2.7: the
/// comparison is dialect-agnostic; only the journal read underneath differs).
///
/// Pure: no I/O. See [`check_checksum_drift`] for the per-rule rationale.
#[must_use]
pub fn compare_applied_to_set(
    applied: &[AppliedEntry],
    migrations: &[Migration],
) -> ChecksumDriftReport {
    let by_version: BTreeMap<&str, &Migration> =
        migrations.iter().map(|m| (m.version.as_str(), m)).collect();

    let mut report = ChecksumDriftReport::default();
    for entry in applied {
        // Only NET-applied (completed) versions can drift / be orphaned; a lone
        // `started` inflight marker is a crash-recovery key, not a settled state.
        if entry.phase != Phase::Completed {
            continue;
        }
        match by_version.get(entry.version.as_str()) {
            Some(m) => {
                // v3 Plan E (re-critic) — DRIFT EXEMPTION anchored on the JOURNALED
                // kind, NEVER on the attacker-suppliable `m.flags.repeatable`.
                //
                // A repeatable migration's checksum changes by DESIGN (a changed
                // `CREATE OR REPLACE …` re-runs each deploy), so a checksum mismatch
                // on a GENUINE repeatable is the re-run signal, not tamper. But the
                // ONLY trustworthy evidence that a version IS a repeatable is what the
                // journal recorded when it last applied (`kind='repeatable'`) — the
                // supplied flag is forgeable. So the exemption requires BOTH the
                // journaled kind AND the supplied flag to agree on "repeatable":
                //
                //  - journaled `repeatable` AND supplied `repeatable=true` ⇒ EXEMPT
                //    (the repeatable phase handles its re-apply);
                //  - journaled once-only (apply/baseline/squash) but supplied
                //    `repeatable=true` ⇒ KIND MISMATCH = TAMPER (the flip-flag attack:
                //    turning an applied once-only into a repeatable to slip a mutated
                //    `up` past the once-only abort) ⇒ ChecksumDrift / abort;
                //  - journaled `repeatable` but supplied `repeatable=false` ⇒ reverse
                //    re-classification (also a kind mismatch) ⇒ ChecksumDrift / abort;
                //  - journaled once-only AND supplied once-only ⇒ the ordinary
                //    once-only tamper guard (changed checksum still aborts).
                let journaled_repeatable =
                    entry.kind.is_some_and(crate::apply::journal::JournaledKind::is_repeatable);
                let supplied_repeatable = m.flags.repeatable;
                if journaled_repeatable && supplied_repeatable {
                    // Legit repeatable re-run signal — exempt from the tamper abort.
                    continue;
                }
                if journaled_repeatable != supplied_repeatable {
                    // Kind mismatch: the supplied repeatability disagrees with the
                    // journaled identity-class. This is tamper (the flip-flag bypass
                    // or its reverse) — abort with ChecksumDrift regardless of whether
                    // the checksums happen to match, because the RE-CLASSIFICATION
                    // itself is the attack. Reuse ChecksumDrift so `apply` aborts on
                    // the shared gate; recorded vs expected carry the two checksums.
                    report.checksum_drift.push(ChecksumDrift {
                        version: entry.version.clone(),
                        recorded: entry.checksum.clone(),
                        expected: m.checksum.as_str().to_string(),
                    });
                    continue;
                }
                // Both once-only: the ordinary tamper guard.
                if entry.checksum != m.checksum.as_str() {
                    report.checksum_drift.push(ChecksumDrift {
                        version: entry.version.clone(),
                        recorded: entry.checksum.clone(),
                        expected: m.checksum.as_str().to_string(),
                    });
                }
            }
            None => report.orphan_journal.push(OrphanJournal {
                version: entry.version.clone(),
                recorded: entry.checksum.clone(),
            }),
        }
    }
    report
}

// ---------------------------------------------------------------------------
// B2 — structural introspection + pure diff
// ---------------------------------------------------------------------------

/// One column of a table, as introspected from `information_schema.columns`.
///
/// `default` is **DDL-emission metadata, not a drift-comparable attribute**: it
/// carries the column `DEFAULT` clause the declarative author wants emitted at
/// CREATE / ADD COLUMN time (#4). It is deliberately EXCLUDED from `PartialEq` /
/// `Eq` / `Hash` (see the manual impls below) because Postgres normalises a
/// stored default (`'{}'` → `'{}'::jsonb`, `NOW()` → `now()`, …) so a byte
/// compare of the authored default against the introspected one would
/// phantom-drift, AND plugin-db itself never re-diffs column defaults (a default
/// is set once at create time). Tracking it in equality would make the differ
/// emit a phantom op and break the lossless round-trip oracle.
///
/// Introspection (`snapshot_schema`) leaves it `None`; only `desired_snapshot`
/// populates it (for emission). All drift comparison is on `data_type` +
/// `nullable` only (see `diff_attrs`).
#[derive(Clone, Default)]
pub struct ColumnSnapshot {
    /// Column name.
    pub name: String,
    /// The SQL data type (`information_schema.columns.data_type`), e.g. `text`,
    /// `integer`, `timestamp with time zone`.
    pub data_type: String,
    /// `true` if the column is nullable.
    pub nullable: bool,
    /// The `DEFAULT` clause expression to emit at CREATE / ADD COLUMN (#4), e.g.
    /// `'active'` or `'{}'::jsonb`. Emission-only; NOT drift-compared (see the
    /// type-level note). `None` ⇒ no default emitted; always `None` from
    /// introspection.
    pub default: Option<String>,
    /// Dialect-rendered type spelling to use in DDL instead of deriving from
    /// `data_type`. This is emission-only for named type references: a Postgres
    /// enum/domain column needs a schema-qualified type name in the emitted DDL,
    /// while structural drift still compares the introspectable `data_type`.
    pub ddl_type_override: Option<String>,
    /// Column-level CHECK clauses to append at the use-site, e.g. the SQLite
    /// enum/domain inline forms. Each entry includes the `CHECK (...)` wrapper and
    /// is rendered only by the DDL emitter. Emission-only: live introspection tracks
    /// table constraints, not this authoring metadata.
    pub inline_checks: Vec<String>,
    /// A generated/computed column expression rendered for the target dialect,
    /// plus whether it is STORED or VIRTUAL. Emission-only, like `default`: live
    /// introspection does not carry this expression into the structural snapshot,
    /// so it is excluded from drift equality.
    pub generated: Option<GeneratedColumnSnapshot>,
    /// A SQL identity column facet. Emission-only: drift tracks the physical
    /// column and primary-key constraint, not the sequence metadata.
    pub identity: Option<IdentityCol>,
    /// **P4 HALF A** — the inline encryption sentinel to append after this
    /// column's type in CREATE / ADD COLUMN DDL, e.g.
    /// `/* zsenc:randomised:default:string */`. Emitted for a `t.encrypted(...)`
    /// column (its physical type is `BYTEA`); it is the schema-shape contract
    /// plugin-db reads at runtime to drive the AEAD encrypt/decrypt pass.
    ///
    /// Emission-only, exactly like `default`: it is NOT a drift-comparable
    /// attribute (introspection's `snapshot_schema` leaves it `None`; only
    /// `desired_snapshot` populates it), so it is EXCLUDED from `PartialEq` /
    /// `Eq` / `Hash`. The sentinel is built by the shared
    /// [`zeroship_schema::query`] kernel — never re-spelled here.
    pub encryption_sentinel: Option<String>,
    /// **P4 HALF A** — the body of a `COMMENT ON COLUMN` sentinel to attach to
    /// THIS column in CREATE / ADD COLUMN DDL. Two sentinel families ride here:
    ///   - `__zsmask:kind=…,classification=…` on a hidden `<col>_masked` sibling
    ///     (drives the runtime mask read-pass), and
    ///   - `zsenc:<mode>:<keyId>:<wraps>` on an encrypted column itself — the
    ///     PG-recoverable form of the `encryption_sentinel`, since PG discards
    ///     the inline `/* zsenc */` comment at parse time, so plugin-db recovers
    ///     the encryption metadata from `pg_description` at runtime.
    ///
    /// Built by the shared codecs ([`zeroship_schema::mask_codec`]) — never
    /// re-spelled here. Emission-only — EXCLUDED from `PartialEq` / `Eq` /
    /// `Hash` (introspection never reads COMMENTs into the snapshot; the
    /// encrypted/masked COLUMN itself round-trips as a plain column).
    pub comment_sentinel: Option<String>,
}

/// Emission metadata for a generated/computed column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedColumnSnapshot {
    /// The dialect-rendered closed expression body.
    pub expr: String,
    /// `true` ⇒ STORED; `false` ⇒ VIRTUAL.
    pub stored: bool,
}

impl std::fmt::Debug for ColumnSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("ColumnSnapshot");
        s.field("name", &self.name)
            .field("data_type", &self.data_type)
            .field("nullable", &self.nullable)
            .field("default", &self.default);
        if self.ddl_type_override.is_some() {
            s.field("ddl_type_override", &self.ddl_type_override);
        }
        if !self.inline_checks.is_empty() {
            s.field("inline_checks", &self.inline_checks);
        }
        s.field("generated", &self.generated)
            .field("identity", &self.identity)
            .field("encryption_sentinel", &self.encryption_sentinel)
            .field("comment_sentinel", &self.comment_sentinel)
            .finish()
    }
}

// `default` + type overrides + inline checks + the two sentinels are intentionally
// excluded from equality + hashing — they are DDL-emission metadata, not drift
// attributes (see the type doc). Comparing `default` would phantom-drift against
// Postgres' normalised stored default; the sentinels are never introspected into
// the snapshot at all (`snapshot_schema` leaves them `None`), so comparing them
// would make every freshly-created encrypted/masked table phantom-drift against
// itself and break the round-trip oracle. Generated/identity metadata follows the
// same policy: the structural column + PK shape round-trips, while the
// expression/sequence details remain emission metadata unless a future live-catalog
// facet recovers them byte-exactly.
impl PartialEq for ColumnSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.data_type == other.data_type
            && self.nullable == other.nullable
    }
}
impl Eq for ColumnSnapshot {}
impl std::hash::Hash for ColumnSnapshot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.data_type.hash(state);
        self.nullable.hash(state);
    }
}

/// One index of a table, as introspected from `pg_catalog`.
///
/// `opclass` is **emission-only** (like `ColumnSnapshot::default` /
/// `encryption_sentinel`): it is NOT recovered by `snapshot_schema` and NOT a
/// drift attribute, so it is EXCLUDED from `PartialEq` / `Eq` / `Hash`. It rides
/// on a desired snapshot so `render_create_index` can spell the per-column
/// operator class (`vector_cosine_ops`, …) an `ivfflat` ANN index needs; live
/// introspection cannot recover it cheaply, so comparing it would make every
/// freshly-built vector index phantom-drift against itself.
#[derive(Debug, Clone)]
pub struct IndexSnapshot {
    /// Index name.
    pub name: String,
    /// `true` if it enforces uniqueness.
    pub unique: bool,
    /// The KEY columns the index covers, in index order (the leading
    /// `indnkeyatts` attributes — INCLUDE columns and expression keys are
    /// excluded). Introspected from `pg_index.indkey` joined to `pg_attribute`
    /// (see [`snapshot_schema`]). This is the column set a `CREATE INDEX … (…)`
    /// must emit verbatim; recovering it from the index NAME is unsound, so the
    /// snapshot carries it explicitly.
    pub columns: Vec<String>,
    /// The index ACCESS METHOD (`pg_am.amname`): `btree` (the default), `gin`
    /// (FTS over a tsvector), `gist` (spatial / geography), `ivfflat` / `hnsw`
    /// (pgvector ANN), etc. Recovered from `pg_am` so a method FLIP
    /// (`btree` → `ivfflat`, the vector-ANN drift) is surfaced by the attribute
    /// diff instead of being invisible to name-only diffing (#index-method-drift).
    /// A desired snapshot the author builds stamps the kind it intends to emit;
    /// `render_create_index` turns a non-`btree` method into `USING <method>`.
    pub access_method: String,
    /// The index EXPRESSION / PREDICATE text, when the index is over an
    /// expression (`pg_index.indexprs`, e.g. an FTS `to_tsvector(...)` index) or
    /// is partial (`pg_index.indpred`). Recovered via `pg_get_expr` so an
    /// expression / partial index round-trips (it has no plain `columns`, so
    /// without this it would phantom-drop) and an out-of-band rewrite of the
    /// expression is surfaced by the attribute diff. `None` for a plain
    /// column-list index.
    pub expression: Option<String>,
    /// **Emission-only** per-column operator class for an `ivfflat`/`hnsw` ANN
    /// index (`vector_cosine_ops`, `vector_l2_ops`, `vector_ip_ops`). `None` for
    /// every plain / GIN / GiST index. NOT a drift attribute — see the type doc;
    /// `snapshot_schema` always leaves it `None`. The author stamps it on a
    /// desired vector index so `render_create_index` can emit
    /// `USING ivfflat ("col" <opclass>)`.
    pub opclass: Option<String>,
}

// `opclass` is intentionally excluded from equality + hashing — it is
// DDL-emission metadata, not a drift attribute (see the type doc). The drift
// attributes are `name`, `unique`, `columns`, `access_method`, `expression`.
impl PartialEq for IndexSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.unique == other.unique
            && self.columns == other.columns
            && self.access_method == other.access_method
            && self.expression == other.expression
    }
}
impl Eq for IndexSnapshot {}
impl std::hash::Hash for IndexSnapshot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.unique.hash(state);
        self.columns.hash(state);
        self.access_method.hash(state);
        self.expression.hash(state);
    }
}

impl IndexSnapshot {
    /// A plain B-tree index over `columns` (the default kind every column-list
    /// index built by the author is). `access_method = "btree"`, no expression.
    #[must_use]
    pub fn btree(name: impl Into<String>, unique: bool, columns: Vec<String>) -> Self {
        Self {
            name: name.into(),
            unique,
            columns,
            access_method: "btree".to_string(),
            expression: None,
            opclass: None,
        }
    }
}

/// One constraint of a table, as introspected from
/// `information_schema.table_constraints` (kind) + `pg_get_constraintdef` (body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintSnapshot {
    /// Constraint name.
    pub name: String,
    /// The constraint type as Postgres reports it: `PRIMARY KEY`, `FOREIGN KEY`,
    /// `UNIQUE`, `CHECK`.
    pub kind: String,
    /// The full constraint definition as `pg_get_constraintdef(oid)` renders it,
    /// e.g. `CHECK ((age > 0))`, `FOREIGN KEY (user_id) REFERENCES users(id)`.
    /// This closes the CHECK-body hole: a same-name constraint whose predicate was
    /// rewritten out-of-band has the same `kind` but a different `definition`, so
    /// the attribute compare surfaces it. Empty only for an expected snapshot the
    /// caller chose not to populate (then no `definition` mismatch is reported).
    pub definition: String,
}

/// A live table's structure (deterministic ordering throughout).
///
/// `stored_create_sql` is **introspection-only** (like `ColumnSnapshot::default`
/// / `IndexSnapshot::opclass`): it is EXCLUDED from `PartialEq` / `Eq` / `Hash`,
/// so it never participates in drift comparison. The `SQLite` drift path populates
/// it with the verbatim `sqlite_master.sql` of the table; the Postgres path leaves
/// it `None` (PG recovers CHECK / generated / partial-index references from the
/// structured `constraints` / `indexes` buckets via `pg_get_constraintdef` /
/// `pg_get_expr`, so it needs no raw text). It is read by the `SQLite` DROP-COLUMN
/// rebuild router (H1): a native `ALTER TABLE … DROP COLUMN` ERRORS when the
/// column participates in a CHECK / generated-column expression / partial-index
/// predicate, and those references are NOT in the `SQLite` structured snapshot (the
/// drift PRAGMAs surface only FK / UNIQUE / PK / index-key columns), so the router
/// consults this raw text to fail-closed into the 12-step rebuild.
#[derive(Debug, Clone)]
pub struct TableSnapshot {
    /// Columns, ordered by name.
    pub columns: Vec<ColumnSnapshot>,
    /// Indexes, ordered by name.
    pub indexes: Vec<IndexSnapshot>,
    /// Constraints, ordered by name.
    pub constraints: Vec<ConstraintSnapshot>,
    /// **Introspection-only** verbatim `CREATE TABLE` text (`SQLite`
    /// `sqlite_master.sql`). `None` on the Postgres path and on author-built
    /// desired snapshots. EXCLUDED from equality / hashing — see the type doc.
    pub stored_create_sql: Option<String>,
}

// `stored_create_sql` is intentionally excluded from equality + hashing — it is
// introspection metadata, not a drift attribute (see the type doc). Comparing it
// would make every SQLite table phantom-drift against an author-built desired
// snapshot (which carries `None`). The drift attributes are columns / indexes /
// constraints.
impl PartialEq for TableSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.indexes == other.indexes
            && self.constraints == other.constraints
    }
}
impl Eq for TableSnapshot {}

/// A deterministic snapshot of a view-like top-level object.
///
/// View SQL text is intentionally carried only as diagnostic/introspection
/// metadata today. Definitions are not compared because raw/structured SELECT
/// rendering can differ textually while remaining semantically equivalent; the
/// current structural contract tracks the object's presence and whether it is
/// materialized.
#[derive(Debug, Clone, Default)]
pub struct ViewSnapshot {
    /// Whether this is a materialized view (Postgres only).
    pub materialized: bool,
    /// Optional declared/output columns. Emission metadata for now.
    pub columns: Option<Vec<String>>,
    /// Optional live/declared definition text. Diagnostic metadata for now.
    pub definition: Option<String>,
}

impl PartialEq for ViewSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.materialized == other.materialized
    }
}
impl Eq for ViewSnapshot {}

/// A deterministic snapshot of a project schema's structure.
///
/// The map is keyed by table name and iterates in sorted order (a `BTreeMap`),
/// and every inner `Vec` is name-sorted, so two snapshots of the same schema are
/// byte-for-byte equal regardless of catalog scan order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaSnapshot {
    /// Tables in the schema, keyed + ordered by name.
    pub tables: BTreeMap<String, TableSnapshot>,
    /// Views in the schema, keyed + ordered by name.
    pub views: BTreeMap<String, ViewSnapshot>,
    /// Named enum/domain types in the schema, keyed + ordered by name.
    pub named_types: BTreeMap<String, NamedTypeSnapshot>,
}

/// A schema-level named type. The engine only needs the object class for drift and
/// guard probes; enum labels/domain predicates are modeled by the neutral IR and
/// by column use-site metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTypeSnapshot {
    /// `"enum"` or `"domain"`.
    pub kind: String,
}

/// One same-name object whose ATTRIBUTES diverge across the two snapshots.
///
/// A column / index / constraint present on BOTH sides but with a changed
/// attribute — an out-of-band `ALTER` that name-only diffing would miss (e.g.
/// `ALTER COLUMN … TYPE`, `DROP NOT NULL`, an index losing UNIQUE, a rewritten
/// CHECK predicate). This is the tamper blind spot #1 closes.
///
/// Names only — never DDL. The caller decides what (if anything) to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlteredObject {
    /// The table the object belongs to (e.g. `users`).
    pub table: String,
    /// The object, qualified within the table: a column as `column id`, an index
    /// as `index users_email_idx`, a constraint as `constraint users_age_chk`.
    pub object: String,
    /// The attribute that diverged: `data_type`, `nullable`, `unique`, `columns`,
    /// `access_method`, `expression`, `kind`, or `definition`.
    pub field: String,
    /// The expected snapshot's value for `field`.
    pub expected: String,
    /// The live DB's value for `field`.
    pub actual: String,
}

/// A structural-drift report (the pure [`diff_snapshots`] output).
///
/// Names only — never DDL. The caller (control plane) decides what, if anything,
/// to do; this module's job ends at *surfacing* (design §5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructuralDrift {
    /// Objects the EXPECTED snapshot has that the LIVE DB does not — e.g. a table
    /// that should exist but is missing, or a column declared but absent.
    pub missing_objects: Vec<String>,
    /// Objects the LIVE DB has that the EXPECTED snapshot does not — out-of-band
    /// creation (scenario 35): a table/column/index/constraint created by hand,
    /// outside the migration journal.
    pub unexpected_objects: Vec<String>,
    /// Same-name objects (present on BOTH sides) whose ATTRIBUTES diverge — an
    /// out-of-band `ALTER` (type/nullability/uniqueness/CHECK-body change). The
    /// missing/unexpected name buckets cannot see these because the name still
    /// matches; this bucket is the attribute-aware tamper surface (#1).
    pub altered_objects: Vec<AlteredObject>,
}

impl StructuralDrift {
    /// True if the live schema matches the expected snapshot exactly.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.missing_objects.is_empty()
            && self.unexpected_objects.is_empty()
            && self.altered_objects.is_empty()
    }
}

/// Introspect the LIVE structure of `project_schema` into a [`SchemaSnapshot`]
/// (design §5 structural drift).
///
/// **Read-only.** Hits `information_schema` / `pg_catalog` only; emits no DDL,
/// mutates nothing. Run as the admin/read connection (NOT the `migrator` role).
///
/// **Injection-safe.** `project_schema` is passed as a **bind parameter** to
/// every catalog query — never interpolated into SQL text — so a schema name
/// containing a quote, a semicolon, or any SQL metacharacter selects zero rows
/// rather than altering the query.
///
/// Determinism: the result map is a `BTreeMap` and every column/index/constraint
/// vector is sorted by name, so the snapshot is stable across catalog scan order.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection — this function takes
/// whatever [`Client`] it is handed and never elevates to the `migrator` role.
/// Binding `project_schema` by `$1` prevents cross-schema leakage regardless of
/// the connection, but choosing a least-privileged read connection is the
/// caller's obligation.
///
/// # Errors
/// [`DriftError::Db`] on a catalog query failure.
/// Normalise `pg_catalog.format_type(...)` output for an extension type back to
/// the engine's canonical DDL spelling, so a live `USER-DEFINED` column compares
/// equal to the desired snapshot the author built.
///
/// The engine emits (and the desired snapshot stores) `geography(POINT, 4326)`
/// for a geoPoint and `vector(N)` for a vector. Postgres canonicalises the stored
/// type, so `format_type` reports `geography(Point,4326)` and `vector(N)`. This
/// maps PG's canonical form back to the engine's spelling for the two extension
/// types the engine emits; any other `format_type` output is returned verbatim
/// (the closest faithful spelling we have for an unknown extension type).
fn canonical_extension_type(format_type: &str) -> String {
    let trimmed = format_type.trim();
    let lower = trimmed.to_ascii_lowercase();
    // PostGIS geography point: `geography(Point,4326)` → `geography(POINT, 4326)`
    // (the engine's `field_to_column` / `dsl_to_pg_data_type` spelling). Match on
    // the lowercased form so we are robust to PG capitalisation changes, and
    // re-emit the exact engine spelling rather than echoing PG's.
    if lower == "geography(point,4326)" {
        return "geography(POINT, 4326)".to_string();
    }
    // pgvector: `vector(N)` already matches the engine's spelling byte-for-byte.
    // Return `format_type`'s output verbatim for vector (and any other extension
    // type) — it is the precise live spelling.
    trimmed.to_string()
}

pub async fn snapshot_schema(
    conn: &Client,
    project_schema: &str,
) -> Result<SchemaSnapshot, DriftError> {
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    let mut views: BTreeMap<String, ViewSnapshot> = BTreeMap::new();
    let mut named_types: BTreeMap<String, NamedTypeSnapshot> = BTreeMap::new();

    // Base tables in the schema. `table_schema` is BOUND ($1), never interpolated.
    let table_rows = conn
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
             ORDER BY table_name",
            &[&project_schema],
        )
        .await?;
    for r in &table_rows {
        let name: String = r.get("table_name");
        tables.insert(name, TableSnapshot {
            columns: Vec::new(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            // PG recovers CHECK / generated / partial-index references from the
            // structured buckets (pg_get_constraintdef / pg_get_expr); no raw text.
            stored_create_sql: None,
        });
    }

    // Plain and materialized views in the schema. Definitions are carried as
    // diagnostic metadata but excluded from equality for now; materialized-ness is
    // structural because SQLite cannot represent it and PG uses a distinct object
    // class.
    let view_rows = conn
        .query(
            "SELECT c.relname AS view_name, c.relkind, pg_get_viewdef(c.oid, true) AS definition \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('v', 'm') \
             ORDER BY c.relname",
            &[&project_schema],
        )
        .await?;
    for r in &view_rows {
        let name: String = r.get("view_name");
        let relkind: i8 = r.get("relkind");
        let materialized = matches!(u8::try_from(relkind).ok().map(char::from), Some('m'));
        let definition: Option<String> = r.try_get("definition").ok().flatten();
        views.insert(name, ViewSnapshot {
            materialized,
            columns: None,
            definition,
        });
    }

    let type_rows = conn
        .query(
            "SELECT t.typname AS type_name, t.typtype \
             FROM pg_type t \
             JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname = $1 AND t.typtype IN ('e', 'd') \
             ORDER BY t.typname",
            &[&project_schema],
        )
        .await?;
    for r in &type_rows {
        let typtype: i8 = r.get("typtype");
        let kind = match u8::try_from(typtype).ok().map(char::from) {
            Some('e') => "enum",
            Some('d') => "domain",
            _ => continue,
        };
        named_types.insert(
            r.get("type_name"),
            NamedTypeSnapshot {
                kind: kind.to_string(),
            },
        );
    }

    // Columns (one query for the whole schema; bucket by table).
    //
    // `information_schema.columns.data_type` reports `USER-DEFINED` for any
    // extension / composite type (pgvector's `vector(N)`, PostGIS's
    // `geography(POINT, 4326)`), which loses the precise spelling the desired
    // snapshot carries — so those columns would phantom-drift forever. We also
    // pull `pg_catalog.format_type(atttypid, atttypmod)` (the canonical PG
    // spelling, e.g. `vector(384)` / `geography(Point,4326)`) and, for a
    // `USER-DEFINED` column, normalise it back to the engine's DDL spelling
    // (see [`canonical_extension_type`]). T13.
    let col_rows = conn
        .query(
            "SELECT c.table_name, c.column_name, c.data_type, c.is_nullable, \
                    format_type(a.atttypid, a.atttypmod) AS format_type \
             FROM information_schema.columns c \
             JOIN pg_namespace n ON n.nspname = c.table_schema \
             JOIN pg_class rel ON rel.relname = c.table_name AND rel.relnamespace = n.oid \
             JOIN pg_attribute a ON a.attrelid = rel.oid AND a.attname = c.column_name \
             WHERE c.table_schema = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY c.table_name, c.column_name",
            &[&project_schema],
        )
        .await?;
    for r in &col_rows {
        let table: String = r.get("table_name");
        if let Some(t) = tables.get_mut(&table) {
            let nullable: String = r.get("is_nullable");
            let data_type: String = r.get("data_type");
            // For a `USER-DEFINED` (extension) type, recover the precise spelling
            // from `format_type` and canonicalise it to the engine's DDL form so
            // it round-trips against the desired snapshot.
            let data_type = if data_type.eq_ignore_ascii_case("USER-DEFINED") {
                canonical_extension_type(&r.get::<_, String>("format_type"))
            } else {
                data_type
            };
            t.columns.push(ColumnSnapshot {
                name: r.get("column_name"),
                data_type,
                nullable: nullable.eq_ignore_ascii_case("YES"),
                // Introspection never carries a default or a sentinel — those
                // are emission-only metadata (see the `ColumnSnapshot` type
                // doc). The masked-sibling/encrypted COLUMN itself round-trips;
                // its sentinel is not a snapshot attribute.
                ..Default::default()
            });
        }
    }

    // Indexes via pg_catalog (schema BOUND as a text comparison on the namespace
    // name). `indisunique` distinguishes unique indexes. The KEY columns (the
    // leading `indnkeyatts` entries of `indkey`) are recovered IN ORDER by
    // `unnest(indkey) WITH ORDINALITY` joined to `pg_attribute`, so a composite
    // / custom-named index carries its real column list (recovering columns from
    // the index NAME is unsound — 1a). Expression keys (`attnum = 0`) and any
    // INCLUDE columns (ordinal beyond `indnkeyatts`) are excluded.
    //
    // The ACCESS METHOD is recovered from `pg_am.amname` (`am.amname`) so a
    // GIN/GiST/ivfflat/hnsw index round-trips as that kind and a method FLIP is
    // surfaced (#index-method-drift). The EXPRESSION / PREDICATE text is recovered
    // via `pg_get_expr(indexprs, indrelid)` / `pg_get_expr(indpred, indrelid)`:
    // an FTS `to_tsvector(...)` index or a partial index has no plain `indkey`
    // columns, so without this it would phantom-DROP and be re-created on every
    // diff (#fts-declarative). `pg_get_expr` renders the canonical, re-parse-stable
    // spelling, so a desired snapshot the author builds with the SAME spelling
    // re-diffs to zero.
    //
    // INVALID indexes are intentionally absent from the structural snapshot. They
    // are not usable implementations of a declared index, and treating them as
    // present would let guarded two-phase `CREATE INDEX CONCURRENTLY` falsely
    // `SatisfiedNoop` instead of entering recovery and rebuilding.
    let idx_rows = conn
        .query(
            "SELECT c.relname AS table_name, ic.relname AS index_name, x.indisunique, \
                    am.amname AS access_method, \
                    pg_get_expr(x.indexprs, x.indrelid) AS index_expr, \
                    pg_get_expr(x.indpred, x.indrelid) AS index_pred, \
                    ( \
                      SELECT array_agg(att.attname ORDER BY k.ord) \
                      FROM unnest(x.indkey) WITH ORDINALITY AS k(attnum, ord) \
                      JOIN pg_attribute att \
                        ON att.attrelid = x.indrelid AND att.attnum = k.attnum \
                      WHERE k.ord <= x.indnkeyatts AND k.attnum <> 0 \
                    ) AS columns \
             FROM pg_index x \
             JOIN pg_class c ON c.oid = x.indrelid \
             JOIN pg_class ic ON ic.oid = x.indexrelid \
             JOIN pg_am am ON am.oid = ic.relam \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND x.indisvalid = true \
             ORDER BY c.relname, ic.relname",
            &[&project_schema],
        )
        .await?;
    for r in &idx_rows {
        let table: String = r.get("table_name");
        if let Some(t) = tables.get_mut(&table) {
            // `array_agg` over an empty/all-expression key set is SQL NULL → an
            // empty column list (a wholly-expression index has no plain columns).
            let columns: Vec<String> = r.try_get("columns").unwrap_or_default();
            // Fold the (optional) expression + (optional) partial predicate into
            // one comparable string. `pg_get_expr` returns NULL for a plain
            // column index → `None`. When both are present (an expression partial
            // index) they are joined `<expr> WHERE <pred>` so the round-trip /
            // drift compare sees the whole shape.
            let index_expr: Option<String> = r.try_get("index_expr").ok().flatten();
            let index_pred: Option<String> = r.try_get("index_pred").ok().flatten();
            let expression = match (index_expr, index_pred) {
                (Some(e), Some(p)) => Some(format!("{e} WHERE {p}")),
                (Some(e), None) => Some(e),
                (None, Some(p)) => Some(format!("WHERE {p}")),
                (None, None) => None,
            };
            t.indexes.push(IndexSnapshot {
                name: r.get("index_name"),
                unique: r.get("indisunique"),
                columns,
                access_method: r.get("access_method"),
                expression,
                // Emission-only; never recovered from the catalog.
                opclass: None,
            });
        }
    }

    // Constraints via pg_catalog (schema BOUND $1 on the namespace name). We read
    // the constraint BODY from `pg_get_constraintdef(oid)` so a same-name CHECK
    // whose predicate was rewritten out-of-band is surfaced by the attribute diff
    // (#1). `contype` is mapped to the same human label information_schema reports
    // (`PRIMARY KEY` / `FOREIGN KEY` / `UNIQUE` / `CHECK`) so `kind` is unchanged.
    let con_rows = conn
        .query(
            "SELECT c.relname AS table_name, con.conname AS constraint_name, \
                    con.contype AS contype, pg_get_constraintdef(con.oid) AS definition \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = con.connamespace \
             WHERE n.nspname = $1 AND con.contype IN ('p', 'f', 'u', 'c') \
             ORDER BY c.relname, con.conname",
            &[&project_schema],
        )
        .await?;
    for r in &con_rows {
        let table: String = r.get("table_name");
        if let Some(t) = tables.get_mut(&table) {
            let contype: i8 = r.get("contype");
            let kind = match u8::try_from(contype).ok().map(char::from) {
                Some('p') => "PRIMARY KEY",
                Some('f') => "FOREIGN KEY",
                Some('u') => "UNIQUE",
                Some('c') => "CHECK",
                _ => "UNKNOWN",
            };
            t.constraints.push(ConstraintSnapshot {
                name: r.get("constraint_name"),
                kind: kind.to_string(),
                definition: r.get("definition"),
            });
        }
    }

    Ok(SchemaSnapshot {
        tables,
        views,
        named_types,
    })
}

/// Diff an **expected** snapshot against the **actual** (live) snapshot — a PURE
/// function, no I/O, no DDL (design §5: surface, don't auto-fix).
///
/// The expected snapshot is **supplied by the caller** — the control-plane /
/// authoring layer owns the declared/union schema (design §0) and is the only
/// component that knows the intended shape. This function does NOT rebuild that
/// model by replaying the migration DDL; that is deliberately the authoring
/// layer's responsibility, and this seam keeps the two concerns separate.
///
/// Returns:
/// - `missing_objects` — present in `expected`, absent in `actual` (a declared
///   table/column/index/constraint the DB never got).
/// - `unexpected_objects` — present in `actual`, absent in `expected` (an
///   out-of-band object created outside the journal — scenario 35).
///
/// Object names are qualified for legibility: a table as `"users"`, a column as
/// `"users.email"`, an index as `"users index orders_email_idx"`, a constraint
/// as `"users constraint users_pkey"`. Output vectors are sorted + deterministic.
///
/// Same-name objects present on BOTH sides are compared ATTRIBUTE-BY-ATTRIBUTE
/// (#1): columns by `data_type` + `nullable`, indexes by `unique` + `columns`,
/// constraints by `kind` + `definition`. Any divergence becomes an
/// [`AlteredObject`] — closing the out-of-band-`ALTER` blind spot that pure name
/// diffing left open.
#[must_use]
pub fn diff_snapshots(expected: &SchemaSnapshot, actual: &SchemaSnapshot) -> StructuralDrift {
    let mut missing = Vec::new();
    let mut unexpected = Vec::new();
    let mut altered = Vec::new();

    // Tables present in expected but not actual → missing (whole table + its
    // children fold into the single table name; the table is the unit of
    // missing-ness). Tables in actual but not expected → unexpected.
    for name in expected.tables.keys() {
        if !actual.tables.contains_key(name) {
            missing.push(name.clone());
        }
    }
    for name in actual.tables.keys() {
        if !expected.tables.contains_key(name) {
            unexpected.push(name.clone());
        }
    }
    for name in expected.views.keys() {
        if !actual.views.contains_key(name) {
            missing.push(format!("view {name}"));
        }
    }
    for name in actual.views.keys() {
        if !expected.views.contains_key(name) {
            unexpected.push(format!("view {name}"));
        }
    }
    for (name, exp_ty) in &expected.named_types {
        if !actual.named_types.contains_key(name) {
            missing.push(format!("{} {name}", exp_ty.kind));
        }
    }
    for (name, act_ty) in &actual.named_types {
        if !expected.named_types.contains_key(name) {
            unexpected.push(format!("{} {name}", act_ty.kind));
        }
    }
    for (name, exp_ty) in &expected.named_types {
        let Some(act_ty) = actual.named_types.get(name) else {
            continue;
        };
        if exp_ty.kind != act_ty.kind {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("type {name}"),
                field: "kind".to_string(),
                expected: exp_ty.kind.clone(),
                actual: act_ty.kind.clone(),
            });
        }
    }
    for (name, exp_v) in &expected.views {
        let Some(act_v) = actual.views.get(name) else {
            continue;
        };
        if exp_v.materialized != act_v.materialized {
            altered.push(AlteredObject {
                table: name.clone(),
                object: format!("view {name}"),
                field: "materialized".to_string(),
                expected: exp_v.materialized.to_string(),
                actual: act_v.materialized.to_string(),
            });
        }
    }

    // For tables present on BOTH sides, diff their columns / indexes / constraints
    // by name (added/removed → missing/unexpected) AND, for same-name children,
    // by attribute (→ altered).
    for (name, exp_t) in &expected.tables {
        let Some(act_t) = actual.tables.get(name) else {
            continue;
        };
        diff_named(
            name,
            "",
            &exp_t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            &act_t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );
        diff_named(
            name,
            "index ",
            &exp_t.indexes.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
            &act_t.indexes.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );
        diff_named(
            name,
            "constraint ",
            &exp_t.constraints.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            &act_t.constraints.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            &mut missing,
            &mut unexpected,
        );

        // Attribute diff for same-name objects on both sides.
        diff_attrs(name, exp_t, act_t, &mut altered);
    }

    missing.sort_unstable();
    unexpected.sort_unstable();
    altered.sort_unstable_by(|a, b| {
        (&a.table, &a.object, &a.field).cmp(&(&b.table, &b.object, &b.field))
    });
    StructuralDrift {
        missing_objects: missing,
        unexpected_objects: unexpected,
        altered_objects: altered,
    }
}

/// Compare the attributes of same-name children (columns/indexes/constraints
/// present on BOTH sides of one table), pushing an [`AlteredObject`] per diverging
/// field. Added/removed children are NOT this function's concern (they go to the
/// missing/unexpected buckets via [`diff_named`]); only matched names are compared.
fn diff_attrs(
    table: &str,
    exp_t: &TableSnapshot,
    act_t: &TableSnapshot,
    altered: &mut Vec<AlteredObject>,
) {
    let mut push = |object: &str, field: &str, expected: &str, actual: &str| {
        if expected != actual {
            altered.push(AlteredObject {
                table: table.to_string(),
                object: object.to_string(),
                field: field.to_string(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    };

    // Columns: data_type + nullable.
    let act_cols: BTreeMap<&str, &ColumnSnapshot> =
        act_t.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    for ec in &exp_t.columns {
        if let Some(ac) = act_cols.get(ec.name.as_str()) {
            let obj = format!("column {}", ec.name);
            push(&obj, "data_type", &ec.data_type, &ac.data_type);
            push(
                &obj,
                "nullable",
                &ec.nullable.to_string(),
                &ac.nullable.to_string(),
            );
        }
    }

    // Indexes: unique + columns. A same-name index whose covered columns changed
    // out-of-band (REINDEX over a different column set, or a name reused for a
    // different shape) is surfaced by the `columns` compare — the name-only diff
    // cannot see it (1a).
    let act_idx: BTreeMap<&str, &IndexSnapshot> =
        act_t.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
    for ei in &exp_t.indexes {
        if let Some(ai) = act_idx.get(ei.name.as_str()) {
            let obj = format!("index {}", ei.name);
            push(&obj, "unique", &ei.unique.to_string(), &ai.unique.to_string());
            push(&obj, "columns", &ei.columns.join(","), &ai.columns.join(","));
            // Access-method drift (#index-method-drift): a same-name index whose
            // `pg_am` kind changed out-of-band — e.g. someone dropped the ANN
            // ivfflat and re-created a plain btree under the same name, or vice
            // versa. Name + columns can match while the method silently differs,
            // so this compare is the only thing that catches a btree→ivfflat
            // flip. An expected snapshot built without a method (`""`) opts out of
            // the compare (it never asserts a method it didn't intend to model).
            if !ei.access_method.is_empty() {
                push(&obj, "access_method", &ei.access_method, &ai.access_method);
            }
            // Expression / partial-predicate drift: an FTS `to_tsvector(...)`
            // index or a partial index whose expression was rewritten
            // out-of-band. `None` on the expected side opts out.
            if let Some(exp_expr) = &ei.expression {
                push(
                    &obj,
                    "expression",
                    exp_expr,
                    ai.expression.as_deref().unwrap_or(""),
                );
            }
        }
    }

    // Constraints: kind + definition (definition closes the CHECK-body hole).
    let act_con: BTreeMap<&str, &ConstraintSnapshot> =
        act_t.constraints.iter().map(|c| (c.name.as_str(), c)).collect();
    for ec in &exp_t.constraints {
        if let Some(ac) = act_con.get(ec.name.as_str()) {
            let obj = format!("constraint {}", ec.name);
            push(&obj, "kind", &ec.kind, &ac.kind);
            push(&obj, "definition", &ec.definition, &ac.definition);
        }
    }
}

/// Diff two name lists belonging to one table, pushing qualified names into the
/// missing / unexpected accumulators. `kind_prefix` is `""` for columns (so the
/// name reads `table.col`), `"index "` / `"constraint "` otherwise.
fn diff_named(
    table: &str,
    kind_prefix: &str,
    expected: &[String],
    actual: &[String],
    missing: &mut Vec<String>,
    unexpected: &mut Vec<String>,
) {
    use std::collections::BTreeSet;
    let exp: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
    let act: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
    let label = |child: &str| {
        if kind_prefix.is_empty() {
            format!("{table}.{child}")
        } else {
            format!("{table} {kind_prefix}{child}")
        }
    };
    for child in expected {
        if !act.contains(child.as_str()) {
            missing.push(label(child));
        }
    }
    for child in actual {
        if !exp.contains(child.as_str()) {
            unexpected.push(label(child));
        }
    }
}

// ---------------------------------------------------------------------------
// DriftReport — the aggregate surface (B1 + B2)
// ---------------------------------------------------------------------------

/// The full drift surface for a project: checksum/tamper drift, orphan journal
/// entries, and (when a structural diff is run) missing / unexpected objects.
///
/// Assembled by the caller from [`check_checksum_drift`] and
/// [`diff_snapshots`]; it carries reports only, never DDL or a remediation plan
/// (design §5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftReport {
    /// Net-applied versions whose journal checksum disagrees with the set.
    pub checksum_drift: Vec<ChecksumDrift>,
    /// Expected objects absent from the live DB (from a structural diff).
    pub missing_objects: Vec<String>,
    /// Live objects absent from the expected snapshot (out-of-band creation).
    pub unexpected_objects: Vec<String>,
    /// Same-name objects whose attributes diverge (out-of-band `ALTER` — #1).
    pub altered_objects: Vec<AlteredObject>,
    /// Net-applied versions with no migration in the supplied set.
    pub orphan_journal: Vec<OrphanJournal>,
}

impl DriftReport {
    /// Assemble from a checksum-drift report and a structural diff.
    #[must_use]
    pub fn new(checksum: ChecksumDriftReport, structural: StructuralDrift) -> Self {
        Self {
            checksum_drift: checksum.checksum_drift,
            missing_objects: structural.missing_objects,
            unexpected_objects: structural.unexpected_objects,
            altered_objects: structural.altered_objects,
            orphan_journal: checksum.orphan_journal,
        }
    }

    /// True if no drift of any kind was found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.checksum_drift.is_empty()
            && self.missing_objects.is_empty()
            && self.unexpected_objects.is_empty()
            && self.altered_objects.is_empty()
            && self.orphan_journal.is_empty()
    }
}
