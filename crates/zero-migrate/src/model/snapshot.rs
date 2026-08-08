//! Schema-shape snapshot value types.

use std::collections::BTreeMap;

use crate::model::ir::{
    ColType, IdentityCol, IndexSortOrder, IndexStorageParams, PartitionBounds, PartitionSpec,
    SafeI64, SafeU64, SequenceOwnedBy, TableRuntimeOptions, ValueFormat,
};

/// One column of a table, as introspected from `information_schema.columns`.
///
/// `default` is **DDL-emission metadata, not a blanket drift-comparable
/// attribute**: it
/// carries the column `DEFAULT` clause the declarative author wants emitted at
/// CREATE / ADD COLUMN time (#4). It is deliberately EXCLUDED from `PartialEq` /
/// `Eq` / `Hash` (see the manual impls below) because Postgres normalises a
/// stored default (`'{}'` → `'{}'::jsonb`, `NOW()` → `now()`, …) so a byte
/// compare of the authored default against the introspected one would
/// phantom-drift, AND plugin-db itself never re-diffs column defaults (a default
/// is set once at create time). Tracking it in equality would make the differ
/// emit a phantom op and break the lossless round-trip oracle.
///
/// ID-bearing defaults are projected into [`Self::id_default`] and compared
/// semantically. Ordinary defaults remain excluded so harmless catalog
/// normalization cannot create phantom drift.
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
    /// `'active'` or `'{}'::jsonb`. The raw SQL is emission/diagnostic metadata
    /// and is NOT drift-compared (see the type-level note). `None` means no
    /// default. Live introspection may retain a catalog-rendered expression;
    /// only its narrow [`Self::id_default`] projection participates in drift.
    pub default: Option<String>,
    /// Dialect-rendered type spelling to use in DDL instead of deriving from
    /// `data_type`. This is emission-only for named type references: a Postgres
    /// enum/domain column needs a schema-qualified type name in the emitted DDL,
    /// while structural drift still compares the introspectable `data_type`.
    pub ddl_type_override: Option<String>,
    /// Column-level CHECK clauses to append at the use-site, e.g. the SQLite
    /// enum/domain inline forms. Each entry includes the `CHECK (...)` wrapper and
    /// is rendered only by the DDL emitter. Emission-only: live introspection tracks
    /// table constraints separately; only recognized ID format CHECKs project
    /// into [`Self::value_format`].
    ///
    /// KNOWN GAP: these bodies NAME THEIR OWN COLUMN, and a rename does not rewrite
    /// them, so a rebuild can emit a clause naming a column the new table does not
    /// have. The stale copy is made at `render::declarative`'s SQLite rename rebuild,
    /// which clones the LIVE table snapshot and rewrites only `ColumnSnapshot::name` -
    /// not in the fold's `Op::RenameColumn` arm, which never sees this field.
    ///
    /// That DDL is REJECTED rather than accepted, and the rejection is designed. The
    /// SQLite actor turns off double-quoted string literals for both DDL and DML
    /// (`SQLITE_DBCONFIG_DQS_DDL` / `_DQS_DML` in the hardened open sequence), so an
    /// unknown quoted identifier is an error naming the column rather than a silent
    /// string literal. Without that setting the clause would degrade to a
    /// constant-false CHECK the database accepts. The rebuild's `CREATE TABLE` leads
    /// its statement spec, so the failure lands before any value copy, inside the
    /// transaction.
    ///
    /// Two more things keep it off every live path: a SQLite CATALOG snapshot carries
    /// this field empty and `stored_create_sql` set, which routes the rebuild through
    /// the arm that replays SQLite's own stored body; and the only fold-sourced
    /// snapshot feeds a historical replay whose artifact is never applied. The first
    /// of those is an accident of what introspection recovers, not a guard.
    ///
    /// Regenerating the clause from the column's facets does NOT work as a general
    /// repair, and this is the load-bearing detail for anyone attempting it: only the
    /// value-format writer still has its input on the snapshot. The uuid, SQLite enum
    /// and domain writers all overwrite `data_type` / `ddl_type_override` and discard
    /// the name of the type that produced the check, so there is nothing left to
    /// regenerate from - only something to infer.
    ///
    /// [`Self::generated`] carries the same hazard through the same emitter and is
    /// worse: a generated expression names OTHER columns by definition, so no
    /// rename-side rewrite is even conceivable for it.
    pub inline_checks: Vec<String>,
    /// A generated/computed column expression rendered for the target dialect,
    /// plus whether it is STORED or VIRTUAL. Emission-only, like `default`: live
    /// introspection does not carry this expression into the structural snapshot,
    /// so it is excluded from drift equality.
    pub generated: Option<GeneratedColumnSnapshot>,
    /// SQL identity / portable auto-increment facet. Desired snapshots use it
    /// for emission and live introspection recovers it from PostgreSQL
    /// `attidentity`, MySQL `AUTO_INCREMENT`, or SQLite's explicit
    /// `AUTOINCREMENT` clause. It is drift-comparable.
    pub identity: Option<IdentityCol>,
    /// Whether this SQLite column is the exact rowid alias shape
    /// (`INTEGER PRIMARY KEY`, excluding `DESC` and `WITHOUT ROWID`).
    ///
    /// SQLite's ordinary rowid allocator and its stronger `AUTOINCREMENT`
    /// contract are physically distinct even though only the latter corresponds
    /// to [`Self::identity`]. Desired and live SQLite snapshots populate this
    /// drift-comparable bit so a normal rowid primary key stays clean while an
    /// out-of-band rowid/AUTOINCREMENT flip is visible.
    pub sqlite_rowid: bool,
    /// A locally enforced TypeID/ULID format CHECK recovered from the catalog.
    ///
    /// This is deliberately separate from [`Self::inline_checks`], which may
    /// contain unrelated emission-only CHECKs. Typed reference columns that
    /// inherit format safety through their foreign key leave this field `None`;
    /// their reference constraint is the drift contract.
    pub value_format: Option<ValueFormat>,
    /// Whether the live catalog carries the engine's own exact UUID spelling
    /// CHECK for this column on MySQL or SQLite. PostgreSQL never sets it: its
    /// native `uuid` type is the contract and the renderer emits no CHECK.
    ///
    /// This exists so a reference whose target has no authored contract can
    /// still be proved from the catalog. It is INTROSPECTION-ONLY metadata and
    /// MUST NOT be compared: author-built desired snapshots always leave it
    /// `false`, so it is excluded from `PartialEq`, `Hash`, and the drift
    /// attribute diff. Comparing it would report permanent phantom drift on
    /// every UUID column, and on SQLite a column-attribute difference is
    /// reconciled by a full table rebuild.
    ///
    /// A typed reference column that omits its own CHECK and inherits format
    /// safety through its foreign key leaves this `false`; that inheritance is
    /// not local catalog evidence.
    pub catalog_uuid_format_check: bool,
    /// Semantic drift key for a default on an ID-bearing column.
    ///
    /// `None` means this is not an ID-default comparison surface. `Some(Absent)`
    /// means it is ID-bearing and deliberately has no database default, which is
    /// distinct from an untracked ordinary column.
    pub id_default: Option<IdDefaultSnapshot>,
    /// MySQL's authoritative distinction between an expression default
    /// (`EXTRA` contains `DEFAULT_GENERATED`) and a scalar literal. MySQL strips
    /// SQL quotes from `COLUMN_DEFAULT`, so the raw text alone cannot distinguish
    /// a literal such as `"uuid()"` from the function call `uuid()`.
    ///
    /// Live MySQL snapshots retain this only for expected-driven ID-default
    /// classification. It is introspection metadata, not an independently
    /// drift-comparable portable facet, and is excluded from equality/hashing.
    pub mysql_default_generated: Option<bool>,
    /// `Some(false)` means this logical text column is case-insensitive. It is a
    /// drift-comparable catalog attribute on engines where the intent is
    /// recoverable (Postgres `citext`, SQLite `COLLATE NOCASE`, and MySQL
    /// `information_schema.COLUMNS.COLLATION_NAME`). `None` is the
    /// byte-identical default case-sensitive text behavior.
    pub case_sensitive: Option<bool>,
    /// Exact non-default catalog collation identity for PostgreSQL and SQLite.
    ///
    /// This is deliberately separate from [`Self::case_sensitive`]: `C` and
    /// `POSIX` are both case-sensitive PostgreSQL collations but are distinct
    /// foreign-key storage contracts. SQLite's default `BINARY` collation is
    /// canonicalized to `None`, while `NOCASE` continues to round-trip through
    /// `case_sensitive = Some(false)`; named alternatives such as `RTRIM` stay
    /// here. MySQL uses [`Self::mysql_text_storage`] because character-set
    /// identity is part of its compatibility contract too.
    pub collation: Option<ColumnCollationSnapshot>,
    /// Exact MySQL character storage recovered from
    /// `information_schema.COLUMNS`. This is introspection-only metadata used
    /// to validate character foreign-key compatibility; author-built desired
    /// snapshots leave it `None`. It is deliberately excluded from structural
    /// drift equality and hashing because the portable schema surface records
    /// collation intent, not a server-default MySQL collation name.
    pub mysql_text_storage: Option<MysqlTextStorageSnapshot>,
    /// The inline encryption sentinel to append after this
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
    /// The body of a `COMMENT ON COLUMN` sentinel to attach to
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
            .field("sqlite_rowid", &self.sqlite_rowid)
            .field("value_format", &self.value_format)
            .field("id_default", &self.id_default)
            .field("case_sensitive", &self.case_sensitive)
            .field("collation", &self.collation);
        if self.catalog_uuid_format_check {
            s.field("catalog_uuid_format_check", &self.catalog_uuid_format_check);
        }
        if self.mysql_text_storage.is_some() {
            s.field("mysql_text_storage", &self.mysql_text_storage);
        }
        if self.mysql_default_generated.is_some() {
            s.field("mysql_default_generated", &self.mysql_default_generated);
        }
        s.field("encryption_sentinel", &self.encryption_sentinel)
            .field("comment_sentinel", &self.comment_sentinel)
            .field("comment", &self.comment)
            .finish()
    }
}

/// Drift-comparable default semantics for an ID-bearing column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IdDefaultSnapshot {
    /// The column is ID-bearing and intentionally has no SQL default.
    Absent,
    /// The engine's exact database-side UUIDv4 generator.
    UuidV4,
    /// The engine's exact database-side UUIDv7 generator.
    UuidV7,
    /// A PostgreSQL `nextval` generator, stored in canonical rendered form so
    /// schema and sequence identity remain part of the key.
    Nextval(String),
    /// A typed scalar literal, stored as a canonical semantic spelling (JSON
    /// strings, bare booleans/numbers, and exact decimal text). Catalog-specific
    /// SQL quoting, tagged int64 transport, and type casts are not part of the
    /// comparison key.
    Literal(String),
    /// A scalar literal on PostgreSQL's native UUID surface. PostgreSQL
    /// canonicalizes accepted UUID text to lowercase hyphenated spelling, so
    /// this distinct semantic arm normalizes that representation without
    /// weakening literal comparison on portable UUID/TypeID/ULID text columns.
    UuidLiteral(String),
    /// Another catalog expression on an ID-bearing column. The string is a
    /// dialect-normalized expression fingerprint, not emission SQL.
    Expression(String),
}

/// Normalize catalog SQL before narrow ID-literal parsing by ignoring casing,
/// whitespace, outer grouping, and MySQL character-set introducers. Structured
/// expression defaults use the semantics-preserving fingerprint in the render
/// layer instead.
#[must_use]
pub(crate) fn canonical_id_default_expression(expression: &str) -> String {
    fn balanced_outer_parens(value: &str) -> bool {
        let bytes = value.as_bytes();
        if bytes.first() != Some(&b'(') || bytes.last() != Some(&b')') {
            return false;
        }
        let mut depth = 0_i32;
        let mut cursor = 0_usize;
        let mut quote = None;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if let Some(delimiter) = quote {
                if byte == delimiter {
                    if bytes.get(cursor + 1) == Some(&delimiter) {
                        cursor += 2;
                        continue;
                    }
                    quote = None;
                }
                cursor += 1;
                continue;
            }
            match byte {
                b'\'' | b'"' | b'`' => quote = Some(byte),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 && cursor + 1 != bytes.len() {
                        return false;
                    }
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        depth == 0 && quote.is_none()
    }

    let mut value = expression.trim();
    while balanced_outer_parens(value) {
        value = value[1..value.len() - 1].trim();
    }

    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if byte == b'_' {
            let start = cursor;
            cursor += 1;
            while bytes
                .get(cursor)
                .is_some_and(|candidate| candidate.is_ascii_alphanumeric() || *candidate == b'_')
            {
                cursor += 1;
            }
            if cursor > start + 1 && bytes.get(cursor) == Some(&b'\'') {
                // MySQL deparsing may prefix string literals with `_latin1`,
                // `_ascii`, `_utf8mb4`, or another connection charset. The
                // literal bytes—not that catalog annotation—define the default.
                continue;
            }
            out.push('_');
            cursor = start + 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            let delimiter = byte;
            out.push(char::from(byte));
            cursor += 1;
            while cursor < bytes.len() {
                let current = bytes[cursor];
                out.push(char::from(current));
                cursor += 1;
                if current == delimiter {
                    if bytes.get(cursor) == Some(&delimiter) {
                        out.push(char::from(delimiter));
                        cursor += 1;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(char::from(byte.to_ascii_lowercase()));
        cursor += 1;
    }
    out
}

/// Exact PostgreSQL/SQLite catalog identity for a non-default column collation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnCollationSnapshot {
    /// PostgreSQL collation schema; SQLite collations are unqualified.
    pub schema: Option<String>,
    /// Catalog collation name, preserving PostgreSQL identifier case.
    pub name: String,
}

impl ColumnCollationSnapshot {
    /// Stable diagnostic spelling without losing the structured comparison key.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.schema.as_ref().map_or_else(
            || self.name.clone(),
            |schema| format!("{schema}.{}", self.name),
        )
    }
}

/// Exact MySQL character-set and collation metadata for one catalog column.
///
/// MySQL requires both sides of a character foreign key to use compatible
/// character storage. A portable `caseSensitive` Boolean is insufficient to
/// distinguish, for example, `ascii_bin` from `utf8mb4_bin`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MysqlTextStorageSnapshot {
    /// `information_schema.COLUMNS.CHARACTER_SET_NAME`.
    pub character_set: String,
    /// `information_schema.COLUMNS.COLLATION_NAME`.
    pub collation: String,
}

impl PartialEq for ColumnSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.data_type == other.data_type
            && self.nullable == other.nullable
            && self.identity == other.identity
            && self.sqlite_rowid == other.sqlite_rowid
            && self.value_format == other.value_format
            && self.id_default == other.id_default
            && self.case_sensitive == other.case_sensitive
            && self.collation == other.collation
            && self.comment == other.comment
    }
}
impl Eq for ColumnSnapshot {}
impl std::hash::Hash for ColumnSnapshot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.data_type.hash(state);
        self.nullable.hash(state);
        self.identity.map(|identity| identity.always).hash(state);
        self.sqlite_rowid.hash(state);
        match &self.value_format {
            None => 0_u8.hash(state),
            Some(ValueFormat::TypeId { prefix }) => {
                1_u8.hash(state);
                prefix.hash(state);
            }
            Some(ValueFormat::Ulid) => 2_u8.hash(state),
        }
        self.id_default.hash(state);
        self.case_sensitive.hash(state);
        self.collation.hash(state);
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
            && ident
                .chars()
                .all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit());
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
            ) => {
                a == b
                    && canonical_index_sort_order(*order_a) == canonical_index_sort_order(*order_b)
            }
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
    /// **Provenance-only** local columns read by this index's RENDERED-SQL sites -
    /// `predicate` and the `Expr` variant of `elements` - recorded structurally by
    /// the producer rather than read back out of that text.
    ///
    /// PostgreSQL drops an index when ANY column it references is dropped, and it
    /// references columns at four sites: the key columns (`indkey` up to
    /// `indnkeyatts`), the `INCLUDE` payload (`indkey` past it), an expression key
    /// (`indexprs`), and a partial predicate (`indpred`). The first two are exact
    /// names in `columns` / `elements` / `include`, so a cascade compares them
    /// directly and this field deliberately does NOT repeat them. The last two are
    /// rendered SQL TEXT, and a name-shaped token inside rendered SQL is not a
    /// reference - measured on PostgreSQL 18.4,
    /// `CREATE INDEX i ON t (note) WHERE (note <> 'a')` and
    /// `CREATE INDEX i ON t ((note || 'a'))` both SURVIVE `DROP COLUMN a`. So the
    /// producer walks the closed `Expr` instead, descending only the leg the target
    /// dialect selects, exactly as `ConstraintSnapshot::cascade_columns` does for a
    /// CHECK.
    ///
    /// `None` on an index that HAS no such site (every plain column-list index) and
    /// on any producer that cannot record it - live introspection, which would have
    /// to re-parse the deparsed SQL to recover the set. Both mean the same thing to a
    /// consumer: cascade on the exact names alone.
    ///
    /// EXCLUDED from equality / hashing for the same reason as `opclass` and
    /// `nulls_not_distinct`: it is provenance rather than identity, and comparing a
    /// field one side never populates would report drift on every index that has one.
    pub expr_cascade_columns: Option<Vec<String>>,
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
        self.predicate
            .as_deref()
            .map(canonical_index_sql_text)
            .hash(state);
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
            elements: columns
                .iter()
                .cloned()
                .map(IndexElementSnapshot::column)
                .collect(),
            columns,
            access_method: "btree".to_string(),
            predicate: None,
            include: Vec::new(),
            with: None,
            only: false,
            opclass: None,
            nulls_not_distinct: false,
            comment: None,
            expr_cascade_columns: None,
        }
    }
}

/// One constraint of a table, as introspected from
/// `information_schema.table_constraints` (kind) + byte-comparable
/// `pg_get_constraintdef` bodies.
#[derive(Debug, Clone)]
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
    ///
    /// Populated for `CHECK`, and exempted from comparison BY THE DIFFER rather than
    /// by this type. `apply::drift::constraint_definition_is_comparable` skips a CHECK
    /// body because PostgreSQL deparses one from the parsed tree rather than echoing
    /// what was written - it re-quotes only the identifiers that need it, injects
    /// inferred casts, and expands `IN` - so the offline renderer cannot reproduce the
    /// spelling and a byte compare reports drift on every CHECK that exists. The text
    /// is still recorded because the existence guard's fail-closed refusal prints it,
    /// which is the difference between naming the installed predicate and saying
    /// `<present>`.
    ///
    /// That printer reads a LIVE snapshot, never a folded one. All four production
    /// call sites of `render::existence_probe::decide` pass freshly introspected
    /// input, so the value a user is shown is always the catalog's. This matters
    /// because a folded CHECK body GOES STALE: `Op::RenameColumn` rewrites the
    /// UNIQUE, PRIMARY KEY and FOREIGN KEY definitions, whose leading group is a
    /// column list a string literal cannot appear in, and deliberately leaves a CHECK
    /// body alone rather than substituting a name inside an arbitrary expression.
    /// A folded stale body has no reader: the differ exempts it, the guard prints the
    /// live one, and the three per-dialect emitters that write a `definition` into
    /// `CREATE TABLE` DDL cannot receive a folded CHECK - the PostgreSQL and MySQL
    /// ones are only ever handed a freshly built desired snapshot, and the fold
    /// refuses to put a CHECK constraint in a SQLite snapshot at all.
    ///
    /// Do NOT read that as "stale fold text is harmless" in general. It is a fact
    /// about THIS field, established by tracing its consumers. `ColumnSnapshot::
    /// inline_checks` is fold-produced CHECK text on the same rename path, is not
    /// dialect-gated, and IS emitted into rebuild DDL.
    ///
    /// `ConstraintSnapshot`'s own `PartialEq` below DOES compare this field, for every
    /// kind including `CHECK`, and that divergence is deliberate. The exemption above
    /// is a PostgreSQL re-deparse artifact, not a statement that a CHECK body is not
    /// part of a constraint's identity: SQLite stores the `CREATE TABLE` text verbatim
    /// and reads the same body back byte-identical, so a comparison on that leg needs
    /// no exemption at all. Generalising the differ's rule into this `PartialEq` would
    /// export a PG-shaped concession to every consumer. A consumer that wants the
    /// differ's semantics applies them itself - `tests/fold_roundtrip_sqlite.rs` does,
    /// and says why at its `canonicalize`.
    pub definition: String,
    /// User-authored catalog comment on this constraint.
    pub comment: Option<String>,
    /// **Provenance-only** local columns whose drop cascades this constraint away,
    /// recorded structurally by the producer rather than parsed back out of
    /// `definition`.
    ///
    /// On the live side this is PostgreSQL's `conkey` expanded to names, which is
    /// exactly the catalog's own cascade predicate: `ALTER TABLE ... DROP COLUMN`
    /// removes every constraint whose `conkey` contains the dropped attribute.
    /// On the offline side it is collected from the closed AST the renderer emitted,
    /// by a route that differs per kind because PostgreSQL's own cascade does. For a
    /// `CHECK` it walks the expression, descending only into the leg the target
    /// dialect selects. For an `EXCLUDE` it takes the PLAIN COLUMN elements and walks
    /// no expression at all: an exclusion that reaches a column through an expression
    /// element or its `WHERE` predicate holds a normal dependency rather than an auto
    /// one, so PostgreSQL REFUSES the drop instead of cascading, and `conkey` records
    /// attnum `0` for that element accordingly. Collecting those columns would fold
    /// away a constraint the database kept.
    ///
    /// `None` means the producer did not record it and the consumer must fall back
    /// to parsing the leading parenthesized group of `definition`. `Some(vec![])`
    /// means the constraint references no column at all (`CHECK (true)`, a
    /// whole-row predicate) and therefore never cascades.
    ///
    /// EXCLUDED from equality: this is provenance, not identity. The fold-derived
    /// and `conkey`-derived lists legitimately differ (the fold records it only
    /// where it drives a cascade decision), and comparing them would report drift
    /// on constraints that are structurally identical.
    pub cascade_columns: Option<Vec<String>>,
}

impl PartialEq for ConstraintSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.kind == other.kind
            && self.definition == other.definition
            && self.comment == other.comment
    }
}
impl Eq for ConstraintSnapshot {}

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
        self.materialized == other.materialized && self.comment == other.comment
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

fn normalize_sequence_bound(default: i64, value: i64) -> Result<Option<SafeI64>, String> {
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
