//! Schema-shape snapshot value types.

use std::collections::BTreeMap;

use crate::model::ir::{
    ColType, IdentityCol, IndexSortOrder, IndexStorageParams, PartitionBounds, PartitionSpec,
    SafeI64, SafeU64, SequenceOwnedBy, TableRuntimeOptions,
};

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
/// Introspection (`snapshot_schema`) leaves it `None` except for recovered
/// PostgreSQL `nextval('<sequence>'::regclass)` defaults, which are compared by
/// parsed sequence identity. All other drift comparison is on `data_type` +
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
    /// type-level note). `None` ⇒ no default emitted; live introspection only
    /// populates recovered PostgreSQL `nextval('<sequence>'::regclass)`
    /// defaults.
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
    /// `Some(false)` means this logical text column is case-insensitive. It is a
    /// drift-comparable catalog attribute on engines where the intent is
    /// recoverable (Postgres `citext`, SQLite `COLLATE NOCASE`). `None` is the
    /// byte-identical default case-sensitive text behavior.
    pub case_sensitive: Option<bool>,
    /// **P4 HALF A** — the inline encryption sentinel to append after this
    /// column's type in CREATE / ADD COLUMN DDL, e.g.
    /// `/* zero-migrate:enc:randomised:default:string */`. Emitted for a `t.encrypted(...)`
    /// column (its physical type is `BYTEA`); it is the schema-shape contract
    /// plugin-db reads at runtime to drive the AEAD encrypt/decrypt pass.
    ///
    /// Emission-only, exactly like `default`: it is NOT a drift-comparable
    /// attribute (introspection's `snapshot_schema` leaves it `None`; only
    /// `desired_snapshot` populates it), so it is EXCLUDED from `PartialEq` /
    /// `Eq` / `Hash`. The sentinel is built by the shared
    /// [`crate::schema::query`] kernel — never re-spelled here.
    pub encryption_sentinel: Option<String>,
    /// **P4 HALF A** — the body of a `COMMENT ON COLUMN` sentinel to attach to
    /// THIS column in CREATE / ADD COLUMN DDL. Two sentinel families ride here:
    ///   - `zero-migrate:mask:kind=…,classification=…` on a hidden `<col>_masked` sibling
    ///     (drives the runtime mask read-pass), and
    ///   - `zero-migrate:enc:<mode>:<keyId>:<wraps>` on an encrypted column itself — the
    ///     PG-recoverable form of the `encryption_sentinel`, since PG discards
    ///     the inline `/* zero-migrate:enc */` comment at parse time, so plugin-db recovers
    ///     the encryption metadata from `pg_description` at runtime.
    ///
    /// Built by the shared codecs ([`crate::schema::mask_codec`]) — never
    /// re-spelled here. EXCLUDED from `PartialEq` / `Eq` / `Hash`: desired
    /// snapshots use it to emit runtime metadata, and PostgreSQL introspection
    /// classifies matching catalog comments back into this field instead of the
    /// user-facing `comment` facet.
    pub comment_sentinel: Option<String>,
    /// User-authored catalog comment on this column. Unlike `comment_sentinel`,
    /// this is drift-comparable metadata folded from `Op::Comment` and recovered
    /// from PostgreSQL `pg_description`.
    pub comment: Option<String>,
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
            .field("case_sensitive", &self.case_sensitive)
            .field("encryption_sentinel", &self.encryption_sentinel)
            .field("comment_sentinel", &self.comment_sentinel)
            .field("comment", &self.comment)
            .finish()
    }
}

impl PartialEq for ColumnSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.data_type == other.data_type
            && self.nullable == other.nullable
            && self.case_sensitive == other.case_sensitive
            && self.comment == other.comment
    }
}
impl Eq for ColumnSnapshot {}
impl std::hash::Hash for ColumnSnapshot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.data_type.hash(state);
        self.nullable.hash(state);
        self.case_sensitive.hash(state);
        self.comment.hash(state);
    }
}

/// One ordered key element of an index snapshot. The expression arm stores the
/// dialect-rendered expression text produced from a closed [`crate::model::expr::Expr`]
/// or recovered from catalog introspection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IndexElementSnapshot {
    /// Plain column key.
    Column {
        /// Column name.
        name: String,
        /// Optional per-column sort order. `None` is canonical ASC/default.
        order: Option<IndexSortOrder>,
        /// **Emission-only** PG-vendor per-column operator class (e.g.
        /// `text_pattern_ops`). Like the index-level ANN `opclass`, live
        /// introspection cannot recover it cheaply, so it is EXCLUDED from
        /// canonical equality / hashing (`index_elements_canonically_eq`) and is
        /// spelled by the PG emitter only. `None` for every non-opclass element.
        opclass: Option<String>,
        /// **Emission-only** PG-vendor per-column collation (e.g. `"C"`).
        /// Excluded from canonical equality / hashing for the same reason as
        /// `opclass`. `None` for every non-collated element.
        collation: Option<String>,
    },
    /// Expression key.
    Expr(String),
}

impl IndexElementSnapshot {
    /// Plain column key.
    #[must_use]
    pub fn column(name: impl Into<String>) -> Self {
        Self::Column {
            name: name.into(),
            order: None,
            opclass: None,
            collation: None,
        }
    }

    /// Plain column key with explicit sort order. ASC canonicalizes to the
    /// default/absent representation; only DESC is preserved.
    #[must_use]
    pub fn column_ordered(name: impl Into<String>, order: IndexSortOrder) -> Self {
        match order {
            IndexSortOrder::Asc => Self::column(name),
            IndexSortOrder::Desc => Self::Column {
                name: name.into(),
                order: Some(IndexSortOrder::Desc),
                opclass: None,
                collation: None,
            },
        }
    }

    /// Expression key.
    #[must_use]
    pub fn expr(expr: impl Into<String>) -> Self {
        Self::Expr(expr.into())
    }
}

fn canonical_index_sql_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            out.push(ch);
            continue;
        }
        let mut ident = String::new();
        let mut raw_inner = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            if c == '"' {
                if matches!(chars.peek(), Some('"')) {
                    chars.next();
                    ident.push('"');
                    raw_inner.push_str("\"\"");
                } else {
                    closed = true;
                    break;
                }
            } else {
                ident.push(c);
                raw_inner.push(c);
            }
        }
        let safe_bare = !ident.is_empty()
            && ident.starts_with(|c: char| c == '_' || c.is_ascii_lowercase())
            && ident.chars().all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit());
        if closed && safe_bare {
            out.push_str(&ident);
        } else {
            out.push('"');
            out.push_str(&raw_inner);
            if closed {
                out.push('"');
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn index_elements_canonically_eq(
    left: &[IndexElementSnapshot],
    right: &[IndexElementSnapshot],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| match (a, b) {
            (
                IndexElementSnapshot::Column {
                    name: a,
                    order: order_a,
                    // opclass/collation are emission-only — never a drift attribute.
                    ..
                },
                IndexElementSnapshot::Column {
                    name: b,
                    order: order_b,
                    ..
                },
            ) => a == b && canonical_index_sort_order(*order_a) == canonical_index_sort_order(*order_b),
            (IndexElementSnapshot::Expr(a), IndexElementSnapshot::Expr(b)) => {
                canonical_index_sql_text(a) == canonical_index_sql_text(b)
            }
            _ => false,
        })
}

pub(crate) fn canonical_index_sort_order(order: Option<IndexSortOrder>) -> Option<IndexSortOrder> {
    match order {
        Some(IndexSortOrder::Desc) => Some(IndexSortOrder::Desc),
        Some(IndexSortOrder::Asc) | None => None,
    }
}

pub(crate) fn index_predicates_canonically_eq(left: Option<&str>, right: Option<&str>) -> bool {
    left.map(canonical_index_sql_text) == right.map(canonical_index_sql_text)
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
    /// plain-column attributes). Expression keys are represented in `elements`.
    pub columns: Vec<String>,
    /// Ordered key elements, including both plain columns and expression keys.
    pub elements: Vec<IndexElementSnapshot>,
    /// The index ACCESS METHOD (`pg_am.amname`): `btree` (the default), `gin`
    /// (FTS over a tsvector), `gist` (spatial / geography), `ivfflat` / `hnsw`
    /// (pgvector ANN), etc.
    pub access_method: String,
    /// Partial-index predicate text, when present.
    pub predicate: Option<String>,
    /// Non-key covering columns (`INCLUDE (...)`).
    pub include: Vec<String>,
    /// Typed storage parameters (`WITH (...)`).
    pub with: Option<IndexStorageParams>,
    /// PostgreSQL `ON ONLY` for partitioned parents.
    pub only: bool,
    /// **Emission-only** per-column operator class for an `ivfflat`/`hnsw` ANN
    /// index (`vector_cosine_ops`, `vector_l2_ops`, `vector_ip_ops`). `None` for
    /// every plain / GIN / GiST index. NOT a drift attribute.
    pub opclass: Option<String>,
    /// **Emission-only** PG 15+ `NULLS NOT DISTINCT` flag on a UNIQUE index.
    /// Like `opclass`, it is spelled by the PG emitter but EXCLUDED from drift
    /// equality / hashing (live introspection recovery is out of scope for this
    /// render-only enrichment). `false` for every ordinary index.
    pub nulls_not_distinct: bool,
    /// User-authored catalog comment on this index.
    pub comment: Option<String>,
}

impl PartialEq for IndexSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.unique == other.unique
            && self.columns == other.columns
            && index_elements_canonically_eq(&self.elements, &other.elements)
            && self.access_method == other.access_method
            && index_predicates_canonically_eq(
                self.predicate.as_deref(),
                other.predicate.as_deref(),
            )
            && self.include == other.include
            && self.with == other.with
            && self.only == other.only
            && self.comment == other.comment
    }
}
impl Eq for IndexSnapshot {}
impl std::hash::Hash for IndexSnapshot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.unique.hash(state);
        self.columns.hash(state);
        for element in &self.elements {
            match element {
                IndexElementSnapshot::Column { name, order, .. } => {
                    0_u8.hash(state);
                    name.hash(state);
                    canonical_index_sort_order(*order).hash(state);
                }
                IndexElementSnapshot::Expr(expr) => {
                    1_u8.hash(state);
                    canonical_index_sql_text(expr).hash(state);
                }
            }
        }
        self.access_method.hash(state);
        self.predicate.as_deref().map(canonical_index_sql_text).hash(state);
        self.include.hash(state);
        self.with.hash(state);
        self.only.hash(state);
        self.comment.hash(state);
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
            elements: columns.iter().cloned().map(IndexElementSnapshot::column).collect(),
            columns,
            access_method: "btree".to_string(),
            predicate: None,
            include: Vec::new(),
            with: None,
            only: false,
            opclass: None,
            nulls_not_distinct: false,
            comment: None,
        }
    }
}

/// One constraint of a table, as introspected from
/// `information_schema.table_constraints` (kind) + byte-comparable
/// `pg_get_constraintdef` bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintSnapshot {
    /// Constraint name.
    pub name: String,
    /// The constraint type as Postgres reports it: `PRIMARY KEY`, `FOREIGN KEY`,
    /// `UNIQUE`, `CHECK`, `EXCLUDE`.
    pub kind: String,
    /// The full constraint definition as `pg_get_constraintdef(oid)` renders it,
    /// e.g. `CHECK ((age > 0))`, `FOREIGN KEY (user_id) REFERENCES users(id)`.
    ///
    /// Empty for `EXCLUDE`: PG canonicalizes exclusion definitions differently from
    /// the closed IR renderer, and drift tracks those constraints by name + kind.
    pub definition: String,
    /// User-authored catalog comment on this constraint.
    pub comment: Option<String>,
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
    /// Runtime-visible collection options. These are intentionally excluded from
    /// structural drift equality because live catalog introspection cannot recover
    /// them; the offline fold/gen-types path is their authority.
    pub runtime_options: TableRuntimeOptions,
    /// Partitioning strategy for a partitioned table parent.
    pub partition_by: Option<PartitionSpec>,
    /// User-authored catalog comment on this table.
    pub comment: Option<String>,
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
            && self.partition_by == other.partition_by
            && self.comment == other.comment
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
    /// User-authored catalog comment on this view.
    pub comment: Option<String>,
}

impl PartialEq for ViewSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.materialized == other.materialized
            && self.comment == other.comment
    }
}
impl Eq for ViewSnapshot {}

/// Sequence integer data types Postgres can report for a sequence. The portable
/// IR can author `integer`/`bigint`; `smallint` is catalog-visible so the snapshot
/// keeps it distinct and drift-comparable instead of collapsing it into `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SequenceDataTypeSnapshot {
    /// PostgreSQL `smallint`.
    SmallInt,
    /// PostgreSQL `integer`.
    Int,
    /// PostgreSQL `bigint`.
    #[default]
    BigInt,
    /// A future/unsupported catalog type. Kept closed so it can never be mistaken
    /// for an authored portable type.
    Unsupported,
}

impl std::fmt::Display for SequenceDataTypeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SmallInt => "smallint",
            Self::Int => "integer",
            Self::BigInt => "bigint",
            Self::Unsupported => "unsupported",
        })
    }
}

impl SequenceDataTypeSnapshot {
    /// Convert an authored sequence `AS` type into the snapshot's closed catalog
    /// enum. `None` is PostgreSQL's default `bigint`.
    pub(crate) fn from_sequence_col_type(as_type: Option<&ColType>) -> Result<Self, &'static str> {
        match as_type {
            None | Some(ColType::BigInt) => Ok(Self::BigInt),
            Some(ColType::SmallInt) => Ok(Self::SmallInt),
            Some(ColType::Int) => Ok(Self::Int),
            Some(_) => Err("sequence AS type must be smallInt, int, or bigInt"),
        }
    }

    /// Convert the PostgreSQL catalog spelling into the snapshot's closed enum.
    pub(crate) fn from_pg_type_name(name: &str) -> Self {
        match name {
            "smallint" | "int2" => Self::SmallInt,
            "integer" | "int4" => Self::Int,
            "bigint" | "int8" => Self::BigInt,
            _ => Self::Unsupported,
        }
    }

    fn bounds(self) -> (i64, i64) {
        match self {
            Self::SmallInt => (i16::MIN as i64, i16::MAX as i64),
            Self::Int => (i32::MIN as i64, i32::MAX as i64),
            Self::BigInt | Self::Unsupported => (i64::MIN, i64::MAX),
        }
    }
}

fn sequence_default_min_value(as_type: SequenceDataTypeSnapshot, increment: SafeI64) -> i64 {
    if increment.get() < 0 {
        as_type.bounds().0
    } else {
        1
    }
}

fn sequence_default_max_value(as_type: SequenceDataTypeSnapshot, increment: SafeI64) -> i64 {
    if increment.get() < 0 {
        -1
    } else {
        as_type.bounds().1
    }
}

fn normalize_sequence_bound(
    default: i64,
    value: i64,
) -> Result<Option<SafeI64>, String> {
    if value == default {
        Ok(None)
    } else {
        SafeI64::new(value).map(Some)
    }
}

/// Normalize a sequence minimum value against PostgreSQL's default/`NO MINVALUE`
/// semantics.
pub(crate) fn normalize_sequence_min_value(
    as_type: SequenceDataTypeSnapshot,
    increment: SafeI64,
    value: i64,
) -> Result<Option<SafeI64>, String> {
    normalize_sequence_bound(sequence_default_min_value(as_type, increment), value)
}

/// Normalize a sequence maximum value against PostgreSQL's default/`NO MAXVALUE`
/// semantics.
pub(crate) fn normalize_sequence_max_value(
    as_type: SequenceDataTypeSnapshot,
    increment: SafeI64,
    value: i64,
) -> Result<Option<SafeI64>, String> {
    normalize_sequence_bound(sequence_default_max_value(as_type, increment), value)
}

/// PostgreSQL's default start value for an omitted `START WITH`: the minimum for
/// ascending sequences and the maximum for descending sequences, after applying
/// explicit non-default bounds if present.
pub(crate) fn sequence_default_start_value(
    as_type: SequenceDataTypeSnapshot,
    increment: SafeI64,
    min_value: Option<SafeI64>,
    max_value: Option<SafeI64>,
) -> Result<SafeI64, String> {
    if increment.get() < 0 {
        match max_value {
            Some(v) => Ok(v),
            None => SafeI64::new(sequence_default_max_value(as_type, increment)),
        }
    } else {
        match min_value {
            Some(v) => Ok(v),
            None => SafeI64::new(sequence_default_min_value(as_type, increment)),
        }
    }
}

/// A deterministic snapshot of a standalone sequence. Numeric bounds are
/// normalized to the IR semantics: `min_value`/`max_value = None` means the
/// PostgreSQL default (`NO MINVALUE` / `NO MAXVALUE`) for the data type and
/// increment direction, while an explicit value is constrained to [`SafeI64`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceSnapshot {
    /// Sequence integer type.
    pub as_type: SequenceDataTypeSnapshot,
    /// Increment step.
    pub increment: SafeI64,
    /// Explicit minimum value, or the normalized default.
    pub min_value: Option<SafeI64>,
    /// Explicit maximum value, or the normalized default.
    pub max_value: Option<SafeI64>,
    /// Start value.
    pub start: SafeI64,
    /// Cache size.
    pub cache: SafeU64,
    /// Whether the sequence cycles.
    pub cycle: bool,
    /// Optional `OWNED BY table.column` target.
    pub owned_by: Option<SequenceOwnedBy>,
    /// User-authored catalog comment on this sequence.
    pub comment: Option<String>,
}

impl Default for SequenceSnapshot {
    fn default() -> Self {
        Self {
            as_type: SequenceDataTypeSnapshot::BigInt,
            increment: SafeI64::new(1).expect("1 is a safe integer"),
            min_value: None,
            max_value: None,
            start: SafeI64::new(1).expect("1 is a safe integer"),
            cache: SafeU64::new(1).expect("1 is a safe integer"),
            cycle: false,
            owned_by: None,
            comment: None,
        }
    }
}

/// A deterministic snapshot of a privileged Postgres role that a vendor
/// migration intentionally manages. Passwords and role settings are deliberately
/// not modeled here; only closed role attributes and membership declared by the IR
/// participate in structural drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSnapshot {
    /// `LOGIN` / `NOLOGIN`.
    pub login: bool,
    /// `SUPERUSER` / `NOSUPERUSER`.
    pub superuser: bool,
    /// `CREATEDB` / `NOCREATEDB`.
    pub create_db: bool,
    /// `CREATEROLE` / `NOCREATEROLE`.
    pub create_role: bool,
    /// `BYPASSRLS` / `NOBYPASSRLS`.
    pub bypass_rls: bool,
    /// `INHERIT` / `NOINHERIT`.
    pub inherit: bool,
    /// `REPLICATION` / `NOREPLICATION`.
    pub replication: bool,
    /// Roles this role is a member of (`IN ROLE ...`), sorted canonically.
    pub member_of: Vec<String>,
}

impl Default for RoleSnapshot {
    fn default() -> Self {
        Self {
            login: false,
            superuser: false,
            create_db: false,
            create_role: false,
            bypass_rls: false,
            inherit: true,
            replication: false,
            member_of: Vec::new(),
        }
    }
}

/// A deterministic snapshot of a Postgres schema object managed by a vendor
/// migration. `owner = None` means the authored op did not assert
/// `AUTHORIZATION`; diff treats that as presence-only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaObjectSnapshot {
    /// Schema owner / `AUTHORIZATION` role when modeled by the authored op.
    pub owner: Option<String>,
}

/// A deterministic snapshot of a Postgres extension object managed by a vendor
/// migration. `schema = None` means the authored op did not assert placement;
/// diff treats that as presence-only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionSnapshot {
    /// Extension placement schema (`WITH SCHEMA ...`) when modeled.
    pub schema: Option<String>,
}

/// A deterministic snapshot of one child partition relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionSnapshot {
    /// Parent partitioned table.
    pub of: String,
    /// Partition bounds.
    pub bounds: PartitionBounds,
}

/// A deterministic snapshot of a project schema's structure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaSnapshot {
    /// Tables in the schema, keyed + ordered by name.
    pub tables: BTreeMap<String, TableSnapshot>,
    /// Child partitions in the schema, keyed + ordered by child relation name.
    pub partitions: BTreeMap<String, PartitionSnapshot>,
    /// Views in the schema, keyed + ordered by name.
    pub views: BTreeMap<String, ViewSnapshot>,
    /// Named enum/domain types in the schema, keyed + ordered by name.
    pub named_types: BTreeMap<String, NamedTypeSnapshot>,
    /// Standalone sequences in the schema, keyed + ordered by name.
    pub sequences: BTreeMap<String, SequenceSnapshot>,
    /// Privileged Postgres roles intentionally managed by vendor migrations.
    pub roles: BTreeMap<String, RoleSnapshot>,
    /// Postgres schemas intentionally managed by vendor migrations.
    pub schemas: BTreeMap<String, SchemaObjectSnapshot>,
    /// Postgres extensions intentionally managed by vendor migrations.
    pub extensions: BTreeMap<String, ExtensionSnapshot>,
}

/// A schema-level named type. The engine only needs the object class for drift and
/// guard probes; enum labels/domain predicates are modeled by the neutral IR and
/// by column use-site metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTypeSnapshot {
    /// `"enum"` or `"domain"`.
    pub kind: String,
    /// User-authored catalog comment on this enum/domain.
    pub comment: Option<String>,
}
