//! Schema-shape snapshot value types.

use std::collections::BTreeMap;

use crate::model::ir::IdentityCol;

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
    /// excluded).
    pub columns: Vec<String>,
    /// The index ACCESS METHOD (`pg_am.amname`): `btree` (the default), `gin`
    /// (FTS over a tsvector), `gist` (spatial / geography), `ivfflat` / `hnsw`
    /// (pgvector ANN), etc.
    pub access_method: String,
    /// The index EXPRESSION / PREDICATE text, when the index is over an
    /// expression or is partial. `None` for a plain column-list index.
    pub expression: Option<String>,
    /// **Emission-only** per-column operator class for an `ivfflat`/`hnsw` ANN
    /// index (`vector_cosine_ops`, `vector_l2_ops`, `vector_ip_ops`). `None` for
    /// every plain / GIN / GiST index. NOT a drift attribute.
    pub opclass: Option<String>,
}

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
    pub definition: String,
}

/// A live table's structure (deterministic ordering throughout).
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
    /// desired snapshots. EXCLUDED from equality / hashing.
    pub stored_create_sql: Option<String>,
}

impl PartialEq for TableSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.indexes == other.indexes
            && self.constraints == other.constraints
    }
}
impl Eq for TableSnapshot {}

/// A deterministic snapshot of a view-like top-level object.
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
