# Round 7 - SC-3's normative IR skeleton, effect-node semantics, the non-query
# capability ledger, and the unmask authorization input shape

Author: opus reviewer, round 7. Repo read-only; nothing under `docs/proposals/`
was touched.

---

## 0. Verification log (everything below is checked by me, at HEAD `af00e11ea`)

The brief supplied several citations from other reviewers. I re-derived every
one of them and two are off by enough to matter.

| Claim | Status | What I found |
| --- | --- | --- |
| parent:333-336 promises stated signatures for CDC / keys / audit / operator lifecycle | **VERIFIED** | `docs/proposals/2026-08-26-runtime-db-binding-design.md:333-336`: "**Non-query capabilities**: CDC lifecycle (spawn, retained ownership, pause, schema-pending, shutdown), key provision, audit insertion, operator lifecycle. Each neutral, each with stated ownership and signatures in SC-3's ledger." |
| sc3:104-134 is the ledger, and it structurally cannot carry them | **VERIFIED** | `2026-08-26-sc3-dbplan-ir-and-ledger.md:106-113`: the row shape is `source_symbol \| source_range \| destination \| status`, `source_range` is a `query.rs` line range, and :117-119 binds the source column to "the set of `pub`/`pub(crate)` functions **the crate** actually exposes" - the crate being `zeroship-schema`. None of the four capabilities lives in `zeroship-schema`; all four live in `zeroship-data-v8`. A ledger whose source column is pinned to one crate's exports cannot hold a row for a symbol in another crate without failing its own gate arm. |
| sc3:82-95 pins the expression sub-grammar | **VERIFIED** | Comparison operators, logical composition, field paths incl. nested access, literals with logical type, and `IS NULL` distinct from a null-valued comparison. |
| `exec.rs:455-466` = `is_app_suppressed` | **CITATION DRIFT** | At 455-466 the file holds *doc-comment prose about* the gate (`exec.rs:459` names `wal_consumer::is_app_suppressed(app_id)`). The **code** is `exec.rs:501`. The parent quotes the comment text verbatim at :520-524, so the quote is right and the line number points at the comment, not the call. Anyone grepping :455-466 for a call site finds none. |
| `wal_consumer.rs:589` = `emit_for_tuple` | **VERIFIED** | `fn emit_for_tuple(` begins at exactly `crates/zeroship-data-v8/src/wal_consumer.rs`. |
| the string `epoch` appears ZERO times in `wal_consumer.rs` | **VERIFIED BY ME** | `grep -c epoch` -> `0`; `grep -ic epoch` -> `0`; file is 1440 lines. Both directions checked, so this is not a case-folding artefact. |
| `ISOLATE_CTX` doc calls it "the per-isolate DB context" but it is a `thread_local!` | **VERIFIED** | `context.rs:888-893`. |
| Fork B: `check_unmask_authorization` is sync | **VERIFIED** | `crud/unmask.rs:305-323`, `fn` not `async fn`, reads `crate::context::with(|c| c.mask_policy_for(app_id))` at :314. |

Additional facts I established for this report, all first-hand:

- `BuiltQuery { pub sql: String, pub params: Vec<String> }` (`query.rs:77-80`).
  Today's "plan" **is** SQL text plus untyped text parameters. SC-3's "the plan
  never carries a fragment of SQL text" is therefore not a guardrail on an
  existing shape, it is a total replacement of the return type of all 48
  runtime builders.
- `validate_field_name` (`query.rs:791-814`) allows **only** ASCII alphanumeric
  and `_`. There is no nested access in the tree at all today. SC-3:91's "field
  paths (including nested access)" is a *new* capability, not a port.
- `value_to_param_inner` (`query.rs:5764-5773`) maps `Value::Null` to
  `String::new()` with the comment "should not be used as param (use IS NULL)".
  `$in` / `$nin` (`query.rs:5400-5435`) call `value_to_param(v)` on **every**
  array element with **no null filter**, so `{f: {$in: [null]}}` renders
  `"f" IN ($1)` with `$1 = ""`. It matches the empty string, not NULL. No test
  in `query.rs` covers a null inside a membership list (grepped).
- `$search` (`query.rs:5468-5477`) emits `to_tsvector('english', col) @@
  plainto_tsquery('english', $n)` with **no dialect arm**, inside a function
  that takes `dialect: SqlDialect` and branches on it three lines above for
  `$ilike`. On SQLite that is PostgreSQL SQL.
- `current_sql_dialect()` (`crud/mod.rs:172-177`) is a frontend dialect match
  on `BackendHandle::Sqlite(_)` with `_ => Postgres` as the catch-all.
- `emit_local` (`wal_consumer.rs:144-164`) always sets `old_tuple: None`.
  `emit_for_tuple` (`:636`) sets it from the WAL old tuple when replica identity
  is FULL. The two producers emit **different event shapes**.
- `SUPPRESSED_APPS` is a `LazyLock<Mutex<HashMap<String, usize>>>` static
  (`wal_consumer.rs:72-73`) - process-wide - while `is_app_suppressed`'s own doc
  at `:98-99` says "on this thread". Same doc/reality mismatch family as
  `ISOLATE_CTX`.
- `ChangeStream::pause_broker` and `::engage_schema_pending`
  (`backend/mod.rs:978`, `:988`) have **zero non-test callers**
  (grepped across `crates/`; every hit outside `backend/mod.rs` is in
  `crates/zeroship-data-v8/tests/sqlite_integration.rs`). The trait carries
  `#[allow(dead_code)]` at `:945`.
- **`crates/zeroship-data-v8/src/backend/api.rs` does not exist.**
  `find crates/zeroship-data-v8 -name api.rs` returns nothing. The directory
  is `lock_guard.rs`, `mod.rs`, `postgres.rs`, `sqlite/`. Three acceptance
  statements are bound to that path (sc3:161, parent:180, parent:1210).

---

# PART 1 - the normative `DbPlan` type skeleton

Destination: a new leaf module `crates/zeroship-data-v8/src/plan/`, no
`v8`, no `compio_postgres`, no `zeroship_schema` dependency. Every type below
is `#[non_exhaustive]` omitted deliberately: pre-launch, we break shapes rather
than reserve for them.

## 1.1 Identifiers - the type that makes SQL text unrepresentable

```rust
// plan/ident.rs

/// A validated SQL identifier. The ONLY way to obtain one is
/// [`Ident::parse`], which runs the reserved-name fences that SC-3
/// requires be moved into `shared/identifier.rs` (today `query.rs:648-652`
/// for tables and `query.rs:740-748` for columns).
///
/// NOT `Deserialize`, NOT `From<String>`, NOT `Ident(pub String)`.
/// The field is private and the module exposes no other constructor.
/// A `#[derive(Deserialize)]` here would reconstruct the newtype from
/// wire bytes without ever running `parse`, which is exactly how a
/// validated-newtype guarantee is usually lost.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ident(String);

impl Ident {
    pub fn parse(raw: &str) -> Result<Self, IdentError>;
    /// Borrow the validated text. There is deliberately no `into_string`:
    /// nothing outside the renderer needs to own it.
    pub fn as_str(&self) -> &str;
}

/// Where an identifier is being used. The fences differ: a table name is
/// fenced by the `__zeroship` prefix list, a column name by the
/// `__zeroship_` / `__zs_` / `sqlite_` prefixes plus the `_masked`
/// sibling suffix. SC-3:49-60 is explicit that these are a PAIR and that
/// moving one without the other is the dangerous half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentRole { Namespace, Collection, Column, Alias, Constraint, Index }

impl Ident {
    pub fn parse_as(raw: &str, role: IdentRole) -> Result<Self, IdentError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentError {
    Empty,
    NulByte,
    TooLong { bytes: usize, max: usize },
    NotAsciiIdentifier { found: char },
    Reserved { fence: &'static str, hint: &'static str },
    SystemField { name: String },
}
```

`Ident::parse_as` is the single fence. Two gate arms, one per role, each with
its own vectors - SC-3:59-60's requirement, held at the type level rather than
by convention.

## 1.2 Field paths - and why key segments are values

```rust
// plan/path.rs

/// A path to a value inside a row. The head is always a real column;
/// every later segment descends into a document value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    head: Ident,
    tail: Vec<PathSeg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg {
    /// Object member. The key is an ARBITRARY unicode string, because JSON
    /// object keys are. It is therefore NOT an `Ident` and MUST reach the
    /// database as a bound parameter, never quoted into the statement.
    Key(String),
    /// Array element, zero-based.
    Index(u32),
}

impl FieldPath {
    pub fn column(head: Ident) -> Self;
    pub fn descend(self, seg: PathSeg) -> Result<Self, PathError>;
    pub fn head(&self) -> &Ident;
    pub fn tail(&self) -> &[PathSeg];
    /// True when this path is a bare column - the only shape usable as an
    /// ON CONFLICT target, an index key, or a `RETURNING` source.
    pub fn is_bare_column(&self) -> bool { self.tail.is_empty() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError { TooDeep { depth: usize, max: usize } }
```

This is the single most consequential shape decision in Part 1 and it is
argued against in section 5. The short version: today `validate_field_name`
(`query.rs:807-814`) rejects any character outside `[A-Za-z0-9_]`, so `"a.b"`
is refused outright. Adding nested access has exactly two implementations -
either the key segments are validated as identifiers (which silently forbids
most legal JSON keys and would make `{"user name": ...}` unreachable), or they
are bound as parameters. Binding them is the only one that keeps SC-3's
constraint 1 ("values are parameters") true for a construct whose "identifier"
half is user data.

## 1.3 Typed literals - and the null rule

```rust
// plan/value.rs

/// The logical type of a plan value. Backend-neutral: a backend maps this
/// onto its own type OID / affinity. There is no `Unknown` and no `Raw`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalType {
    Bool, Int32, Int64, Float64, Decimal,
    Text, Bytes, Uuid, Json,
    Date, Time, TimestampTz, Interval,
    Vector { dims: u32 },
    Geography, Geometry,
}

/// A plan value. Every variant carries its logical type, INCLUDING null.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A typed null. `Null(LogicalType::Text)` is a text null; it is NOT
    /// the empty string, and it is NOT the absence of a value.
    Null(LogicalType),
    Bool(bool),
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Decimal(DecimalRepr),
    Text(String),
    Bytes(Vec<u8>),
    Uuid([u8; 16]),
    Json(serde_json::Value),
    Date(CivilDate),
    Time(CivilTime),
    TimestampTz(UnixMicros),
    Interval(IntervalRepr),
    Vector(Vec<f32>),
    Geo(GeoRepr),
}

impl Literal {
    pub fn logical_type(&self) -> LogicalType;
    pub fn is_null(&self) -> bool;
}
```

Three things this fixes that are live defects, not hypotheticals:

1. `Vec<String>` params (`query.rs:79`) become `Vec<Literal>`. Today a bool
   reaches the server as the text `"true"` (`query.rs:5768`) and a JSON object
   as its `to_string()` (`:5771`), so the server does the typing by inference
   from context. A typed literal channel is what lets a backend choose binary
   format, and it is the precondition for the binary-bind hack
   (`__zsbin__<col>` sibling keys, `query.rs:86-97`) to be deleted rather than
   ported.
2. `Literal::Null(T)` is representable **inside a membership set**, which
   today's `value_to_param(Null) -> ""` (`query.rs:5769`) is not.
3. The decode fix the parent cites - a value silently becoming null changing an
   operator - becomes unrepresentable, because a null literal cannot appear in a
   `Compare` node at all (see 1.4).

## 1.4 The expression sub-grammar - pinned, per SC-3:82-95

```rust
// plan/expr.rs

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// Empty vec means "no constraint" and renders as TRUE. Stated so the
    /// renderer does not have to invent a convention.
    And(Vec<Predicate>),
    /// Empty vec renders as FALSE. Deliberately NOT the same as And's.
    Or(Vec<Predicate>),
    Not(Box<Predicate>),

    /// `lhs <op> rhs`. NEITHER SIDE MAY BE A NULL LITERAL - see the
    /// `no_null_in_compare` invariant in 1.9. Nullness is expressed only
    /// by [`Predicate::IsNull`].
    Compare { lhs: Operand, op: CompareOp, rhs: Operand },

    /// `lhs IN (..)` / `lhs NOT IN (..)`. The set MAY contain typed nulls;
    /// the renderer is required to lower them to the SQL semantics the
    /// backend actually has (on PostgreSQL, `NOT IN` with a null member
    /// yields no rows - a lowering, not a bug, and it must be documented
    /// per-backend rather than silently normalised).
    Membership { lhs: Operand, op: MembershipOp, set: LiteralSet },

    /// `lhs LIKE $n` and its case-insensitive / negated forms.
    Pattern { lhs: Operand, op: PatternOp, pattern: Literal, escape: Option<char> },

    /// The ONLY way to test nullness. Distinct node, not an operator.
    IsNull { operand: Operand, negated: bool },

    /// `lhs BETWEEN $a AND $b`, symmetric excluded.
    Range { lhs: Operand, low: Operand, high: Operand, inclusive: RangeBounds },

    /// Constant. Produced by simplification; never authored.
    Const(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Path(FieldPath),
    Lit(Literal),
    /// An aggregate result, legal ONLY in a HAVING position. Not
    /// type-enforced; see the `aggregate_operand_only_in_having`
    /// invariant in 1.9.
    Aggregate(AggregateRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum CompareOp { Eq, Ne, Lt, Lte, Gt, Gte }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum MembershipOp { In, NotIn }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum PatternOp { Like, NotLike, ILike, NotILike }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum RangeBounds { II, IE, EI, EE }

/// A bounded, homogeneously typed set of literals.
#[derive(Debug, Clone, PartialEq)]
pub struct LiteralSet {
    /// Every member's `logical_type()` equals this, including nulls.
    elem_type: LogicalType,
    members: Vec<Literal>,
}

impl LiteralSet {
    /// Enforces the element-type invariant and the cardinality cap at
    /// construction. `MAX_MEMBERSHIP_LIST_LEN` is 100 today
    /// (`query.rs:604`); the cap moves here with it.
    pub fn build(elem_type: LogicalType, members: Vec<Literal>)
        -> Result<Self, ExprError>;
}
```

Note what is **absent** and why:

- No `Raw(String)`, no `Sql(String)`, no `Expr(String)`. SC-3's constraint 1 is
  held by the absence of a variant, not by a lint.
- No `$regex`. `query.rs:5478-5482` already rejects it via the `other =>` arm;
  the test at `query.rs:6920` pins that rejection. It does not become a node.
- No `$search` in `Predicate`. Full-text is a **search family** plan
  (1.7), not a filter operator. Today it is a filter operator that emits
  PostgreSQL SQL with no dialect arm (`query.rs:5468-5477`), which is precisely
  the shape SC-3:72-75 says cannot be ported as a wrapper.

## 1.5 Projection - `SELECT *` unrepresentable

```rust
// plan/project.rs

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    /// Non-empty by construction. There is no `Star` variant, so
    /// "never `SELECT *`" (parent:546-548) is a property of the type.
    fields: Vec<ProjectedField>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedField {
    pub source: ProjectionSource,
    pub alias: Ident,
    pub exposure: Exposure,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionSource {
    Column(Ident),
    Path(FieldPath),
    /// `<col>_masked AS <col>`. The parent column is NOT read, which is
    /// what keeps `KeyStore::resolve` off the default-read path
    /// (`encryption/keys.rs:297-309` documents the closeout gate that
    /// asserts exactly this).
    MaskedSibling { parent: Ident, sibling: Ident },
    Aggregate(AggregateRef),
    /// A search-produced scalar (vector distance, text rank). Only legal
    /// in a search plan's projection.
    SearchScalar(SearchScalarKind),
}

/// Why the field is in the list. `Platform` fields are added by the
/// planner and are stripped before the row reaches user code unless the
/// declared schema also names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure { Declared, Platform, Internal }

impl Projection {
    pub fn build(fields: Vec<ProjectedField>) -> Result<Self, PlanError>;
}
```

## 1.6 Read

```rust
// plan/read.rs

#[derive(Debug, Clone, PartialEq)]
pub struct ReadPlan {
    pub projection: Projection,
    pub filter: Predicate,
    pub grouping: Option<Grouping>,
    pub distinct: Distinct,
    pub order: Vec<OrderKey>,
    pub page: Pagination,
    pub lock: RowLock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grouping { pub keys: Vec<FieldPath>, pub having: Predicate }

#[derive(Debug, Clone, PartialEq)]
pub enum Distinct { None, All, On(Vec<FieldPath>) }

#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey { pub operand: Operand, pub dir: SortDir, pub nulls: NullOrder }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SortDir { Asc, Desc }
/// Explicit, never defaulted. PostgreSQL defaults NULLS LAST for ASC and
/// NULLS FIRST for DESC; SQLite defaults NULLS FIRST for both. Leaving it
/// implicit makes the two backends disagree on identical plans, which is a
/// parity-fixture failure waiting to be discovered by a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum NullOrder { First, Last }

#[derive(Debug, Clone, PartialEq)]
pub enum Pagination {
    None,
    Offset { limit: u32, offset: u64 },
    /// Keyset. The cursor's key list MUST be a prefix-equal match for the
    /// plan's `order` keys; see 1.9.
    Keyset { limit: u32, after: Vec<(FieldPath, Literal)>, direction: SortDir },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLock { None, Share, Update, UpdateSkipLocked, UpdateNoWait }

#[derive(Debug, Clone, PartialEq)]
pub struct AggregateRef { pub func: AggFunc, pub arg: AggArg, pub distinct: bool, pub alias: Ident }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum AggFunc { Count, Sum, Avg, Min, Max }
#[derive(Debug, Clone, PartialEq)] pub enum AggArg { Star, Path(FieldPath) }
```

## 1.7 Search

```rust
// plan/search.rs

#[derive(Debug, Clone, PartialEq)]
pub enum SearchPlan {
    Vector {
        column: Ident,
        query: Vec<f32>,
        metric: VectorMetric,
        k: u32,
        /// Post-filter applied inside the same statement.
        filter: Predicate,
        projection: Projection,
        distance_alias: Option<Ident>,
    },
    FullText {
        columns: Vec<Ident>,
        query: TextQuery,
        /// CLOSED enum, not a string. `to_tsvector('english', ...)` is
        /// hard-coded at `query.rs:5474` with no dialect arm; a closed
        /// enum is what lets the SQLite backend answer `Unsupported`
        /// instead of shipping PostgreSQL SQL to SQLite.
        config: TextSearchConfig,
        rank_alias: Option<Ident>,
        filter: Predicate,
        projection: Projection,
        order_by_rank: bool,
    },
    Spatial {
        column: Ident,
        predicate: SpatialPredicate,
        filter: Predicate,
        projection: Projection,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum VectorMetric { L2, InnerProduct, Cosine }
#[derive(Debug, Clone, PartialEq)] pub enum TextQuery { Plain(String), Phrase(String), WebStyle(String) }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum TextSearchConfig { Simple, English }
#[derive(Debug, Clone, PartialEq)]
pub enum SpatialPredicate {
    WithinDistance { center: GeoRepr, meters: f64 },
    Intersects(GeoRepr),
    Contains(GeoRepr),
    BoundingBox { min: GeoRepr, max: GeoRepr },
}
```

The three search variants are the ones SC-3:72-75 says read declared schema
*inside* the concrete backends (`backend/postgres.rs:494-527`, `:701-724`,
`:826-855`). What makes them portable is that the plan carries the resolved
column and the resolved metric/config, so the backend needs no schema read -
it needs only to answer whether it can serve `SearchPlan::Vector` at all.

## 1.8 Write, unmask, and the plan root

```rust
// plan/write.rs

#[derive(Debug, Clone, PartialEq)]
pub struct WritePlan {
    pub kind: WriteKind,
    pub returning: Returning,
    /// SC-3 lists effects as a fifth family. It is modelled here as a
    /// FIELD of the write, not a peer variant - see 5.2 for the argument
    /// against, since this is a deliberate deviation from SC-3:78-80.
    pub effects: EffectSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WriteKind {
    Insert { columns: Vec<Ident>, rows: Vec<Vec<Literal>> },
    Update { assignments: Vec<Assignment>, filter: Predicate },
    Upsert {
        columns: Vec<Ident>,
        rows: Vec<Vec<Literal>>,
        /// Explicit. There is no bare-`ON CONFLICT` variant.
        conflict: ConflictTarget,
        action: ConflictAction,
    },
    Delete { filter: Predicate },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment { pub target: Ident, pub value: AssignValue }
#[derive(Debug, Clone, PartialEq)]
pub enum AssignValue {
    Lit(Literal),
    /// `col = col + $n` and friends. A closed set, not an expression tree:
    /// a general expression on the SET side is how raw SQL re-enters.
    Delta { op: DeltaOp, amount: Literal },
    /// `col = EXCLUDED.col`, upsert only.
    Excluded(Ident),
    /// Document merge at a path.
    JsonSet { path: Vec<PathSeg>, value: Literal },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum DeltaOp { Add, Sub }

#[derive(Debug, Clone, PartialEq)]
pub enum ConflictTarget { Columns(Vec<Ident>), Constraint(Ident) }
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictAction { Nothing, Update { assignments: Vec<Assignment>, where_: Predicate } }

#[derive(Debug, Clone, PartialEq)]
pub enum Returning {
    /// No RETURNING clause. The result is an affected-row count.
    Count,
    Rows(Projection),
}
```

```rust
// plan/unmask.rs

#[derive(Debug, Clone, PartialEq)]
pub struct UnmaskPlan {
    pub column: Ident,
    pub row: RowSelector,
    pub classification: MaskClassification,
    /// Carried, never produced here. See Part 4.
    pub decision: UnmaskDecision,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowSelector {
    /// The only shape a MaskedValue can produce.
    PrimaryKey(Literal),
    /// Bulk unmask on a read. Bounded by the read's own pagination.
    Filter(Predicate),
}
```

```rust
// plan/mod.rs

/// The complete, backend-neutral operation tree. Contains no `String` of
/// SQL, no dialect tag, and no backend handle.
#[derive(Debug, Clone, PartialEq)]
pub struct DbPlan {
    pub target: PlanTarget,
    pub body: PlanBody,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanTarget { pub namespace: Ident, pub collection: Ident }

#[derive(Debug, Clone, PartialEq)]
pub enum PlanBody {
    Read(ReadPlan),
    Write(WritePlan),
    Search(SearchPlan),
    Unmask(UnmaskPlan),
}
```

The backend SPI SC-3 constraint 2 requires:

```rust
// plan/exec.rs  (the trait each concrete backend implements)

pub trait PlanExecutor {
    /// A backend that cannot serve a node returns
    /// `DbError::Unsupported { capability, backend }` FROM ITS OWN MODULE.
    /// It never inspects a dialect tag, because the plan carries none.
    async fn execute(&self, plan: &DbPlan, ctx: &OpContext) -> Result<DbRows, DbError>;

    /// Declarative capability report, used by the parity fixtures to skip
    /// a case rather than assert a wrong error. This is the only
    /// "what can you do" surface, and it is on the backend.
    fn capabilities(&self) -> BackendCapabilities;
}
```

## 1.9 Semantic invariants NOT expressible in the types

These are the property tests. Each names the concrete defect it guards, so a
future reader can tell whether it is still measuring anything.

| # | Invariant | Why the type system cannot hold it | Concrete defect it guards |
| --- | --- | --- | --- |
| I1 | **No SQL text survives into the statement.** For every plan, `render(plan).sql` contains no substring of any `Literal::Text`/`Json`/`Bytes` payload, and `render(plan).params.len()` equals the plan's literal count. Fuzz literals containing `'`, `"`, `--`, `/*`, `;`, `\0`, `$1`, and a lone surrogate escape. | `Literal::Text(String)` is a legal value; whether the renderer *binds* it is a property of the renderer. | The whole of SC-3 constraint 1. Today `BuiltQuery.sql` is authored by `format!` in 48 builders. |
| I2 | **Every `Ident` in a rendered plan round-trips `Ident::parse_as`.** Walk the plan, re-parse every `Ident` in its role, assert Ok. Plus a mutation arm: deleting the reserved-prefix fence must make this fail. | `Ident`'s constructor is private, but `serde`, `mem::transmute`, and a future `impl From<String>` all bypass it. A test is the only thing that notices. | `query.rs:648-652` (table fence) and `:740-748` (column fence) are 90 lines apart; SC-3:49-57 says moving one and not the other is the dangerous half. |
| I3 | **`Ident` is not `Deserialize`.** A compile-fail test (`trybuild`) asserting `fn f<T: serde::de::DeserializeOwned>(){} f::<Ident>()` does not compile. | Trait non-implementation is not checkable at runtime. | The standard way a validated newtype quietly stops validating. |
| I4 | **`no_null_in_compare`**: no `Predicate::Compare` has a `Literal::Null` on either side. Enforced by the builder, checked by a walker on every plan the parity fixtures produce. | `Operand::Lit(Literal::Null(_))` is constructible. | `query.rs:5368-5382`: today `$eq: null` and `$ne: null` are *silently rewritten* into `IS NULL`/`IS NOT NULL`. That rewrite is the exact conflation SC-3:92-95 forbids; the fix is to make the malformed plan unconstructible, not to keep rewriting it. |
| I5 | **`null_in_membership_is_a_null`**: for `Membership { set: [Null(Text), Text("a")] }`, the bound parameter for the null member is a database NULL, and a row whose column is `''` does not match. Paired control: a row whose column IS `''` matches `Membership { set: [Text("")] }`. | Round-tripping through a bind encoder is behaviour, not shape. | `query.rs:5412` calls `value_to_param(v)` on every element; `value_to_param_inner(Null) -> String::new()` (`:5769`). `{f:{$in:[null]}}` today binds `""`. Untested. |
| I6 | **`aggregate_operand_only_in_having`**: `Operand::Aggregate` appears only under `Grouping::having` or in `Projection` when `grouping.is_some()`. | Positional legality is not a type property without duplicating the whole `Predicate` enum per position. | `query.rs:5120` `$having` over aggregate aliases. |
| I7 | **`keyset_cursor_matches_order`**: `Pagination::Keyset::after`'s `FieldPath` sequence is a prefix of `ReadPlan::order`'s operand sequence, and each cursor literal's `logical_type()` matches the corresponding key's resolved column type. | Cross-field agreement between two `Vec`s. | A cursor that does not match the sort key silently pages wrong; there is no keyset paging today, so this arrives with the feature. |
| I8 | **`projection_non_empty_and_carries_pk`**: every read/returning projection is non-empty and contains the collection's primary key with `Exposure::Platform` at minimum. | Requires the resolved schema, which the plan type does not carry. | `emit_for_rows` (`exec.rs:507-511`) reads `row["id"]` then `row["_id"]`; a projection that dropped the PK produces an effect with `pk: None` and a subscriber that cannot correlate. |
| I9 | **`parameter_ordinals_dense`**: rendered params are numbered `1..=n` with no gap, each referenced at least once, and `n` is within the driver's Bind-message parameter count. | Renderer property. | `MAX_MEMBERSHIP_LIST_LEN = 100` (`query.rs:604`) caps ONE list. Nothing caps the plan total: 400 `$in` clauses of 100 is 40,000 parameters. The bound must be **derived from `compio-postgres`'s Bind encoder**, not asserted from memory. |
| I10 | **`plan_depth_bounded`**: `Predicate` nesting depth and `FieldPath` length are both capped, and the cap is enforced at construction so a hostile filter cannot stack-overflow the renderer. | Recursion depth is not a type property. | `Predicate::Not(Box<..>)` and `And(Vec<..>)` are freely nestable from app JSON. |
| I11 | **`unsupported_comes_from_the_backend`**: for every capability in `BackendCapabilities` that a backend reports false, executing a plan needing it yields `DbError::Unsupported` whose `backend` field names that backend, and a source arm asserts no `match` on a backend-discriminating enum exists in the frontend module tree. | A negative source property. | `current_sql_dialect()` (`crud/mod.rs:172-177`) is a frontend match today, and `$search` (`query.rs:5468`) emits PostgreSQL SQL under `SqlDialect::Sqlite` because the arm was simply never written. **The arm must name a path that exists** - see 6.1. |
| I12 | **`both_backends_agree`**: for every plan in the fixture corpus, the resolved rows from the PostgreSQL and SQLite executors are equal after the documented lowerings, and the set of documented lowerings is itself enumerated in `docs/reference/sqlite-divergences.md`. Paired control: a deliberately divergent plan must FAIL this. | Cross-backend equality. | SC-3:157-158's parity requirement. The control is the part that matters: an equality arm over an empty corpus passes. |

---

# PART 2 - effect node semantics

## 2.1 The hard constraint, verified

In production the mutation-side producer is suppressed. `emit_for_rows`
returns early when `wal_consumer::is_app_suppressed(app_id)` is true
(`crates/zeroship-data-v8/src/exec.rs`), and the doc block above it
at `:459-465` states why: "when the WAL consumer is running for this app, it
owns the publish path for events this isolate writes."

The real producer is
`crates/zeroship-data-v8/src/wal_consumer.rs emit_for_tuple`. I verified
myself that the string `epoch` appears **zero** times in that file, in both
case-sensitive and case-insensitive greps, across all 1440 lines.

That is only half the constraint. The other half, which the parent does not
state, is **structural**:

```rust
fn emit_for_tuple(
    &self,
    relations: &HashMap<u32, RelationEntry>,
    rel_id: u32,
    op: ChangeOp,
    tuple: &TupleData,
    old_tuple: Option<&TupleData>,
)                                       // wal_consumer.rs:589-596
```

It receives a relation cache, a relation id, and two tuples. Its caller
`dispatch(&self, relations, msg)` (`:505-509`) receives one decoded pgoutput
message. The transaction identity **is** on the wire - `PgOutputMessage::Begin`
carries `xid` and `final_lsn`, `Commit` carries `commit_lsn` and `end_lsn`
(visible in the test fixtures at `:1244-1253`) - but `run_controlled` consumes
them purely for `stream.advance_lsn(...)` (`:453-458`, `:495`) and never
threads them into `dispatch`. So the producer has no operation context, no
lease, no epoch, **and no transaction identity either**, and acquiring the last
one is a signature change plus a per-transaction accumulator in the consumer,
not a field addition.

And `ChangeEvent` (`broker.rs:81-119`) has **no** id, no sequence, no lsn, no
xid, no epoch. It is `{app_id, collection, op, pk, changed_columns, new_tuple,
old_tuple}`.

## 2.2 What an effect node CAN promise

```rust
// plan/effect.rs

/// What a mutation plan declares it owes the broker. This is a
/// DECLARATION, not a delivery guarantee: the node states what the
/// operation is entitled to have published, and the delivery contract in
/// 2.4 states which of those promises each producer can actually keep.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectSpec {
    pub publish: PublishMode,
    /// Which columns the subscriber-narrowing predicate may read. A
    /// column absent here is treated as non-matching by the broker
    /// (`read_set::Predicate::matches`, per `broker.rs:106-109`).
    pub visible_columns: VisibleColumns,
    /// Resolution the event must have had applied before delivery
    /// (parent:497-501: a Masked parent is replaced by its masked
    /// sibling; an Encrypted parent is dropped absent authorization).
    pub resolution: EffectResolution,
    /// Schema generation the plan was built against. Stamped where the
    /// producer can stamp it; see 2.5.
    pub epoch: SchemaEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    /// The mutation owes nothing (an internal/platform write).
    None,
    /// The mutation owes one event per affected row.
    PerRow,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VisibleColumns { Declared(Vec<Ident>), None }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectResolution { MaskedSiblingSubstituted, PlaintextDeclaredOnly }
```

**Event identity.** An effect node can promise a *derivable* identity, and only
that: `(app_id, collection, pk, op, commit_token, row_ordinal)`. `pk` is
already produced by both producers (`exec.rs:507-511` from the RETURNING row,
`wal_consumer.rs:622-627` from the relation's PK index). `commit_token` is the
piece that does not exist yet on either path and whose availability differs by
producer - see 2.5. Until `commit_token` exists, **an effect node cannot
promise a unique event id**, and any acceptance arm that asserts one is
asserting a property no producer can supply.

**Ordering.** An effect node can promise **per-row, per-collection** order
within one commit, because a single producer emits a commit's rows
sequentially on one thread (`dispatch` is `&self`, called from one decode
loop). It **cannot** promise a total order across collections in the presence
of two producers, and it cannot promise order across commits at all: the local
producer publishes when the mutation's SQL returns, and the WAL producer
publishes when the decoder reaches the record, which is after that transaction
committed. Cross-producer order is undefined by construction.

**Relation to returned mutation rows.** On the local path the relationship is
exact and one-to-one: `emit_for_rows(&rows, ...)` iterates the mutation's own
`RETURNING` rows (`exec.rs:437-438`, `:506`). On the WAL path there is **no
relationship at all** - the consumer never sees the mutation's returning shape,
only the replicated tuple, whose column set is the relation's full column list
(`wal_consumer.rs:629-633` clones **every** relation column into
`changed_columns`, for INSERT, UPDATE and DELETE alike). So an effect node can
promise "one event per affected row" and **cannot** promise "the event carries
the columns the mutation returned". Two concrete divergences follow, and both
are live today:

1. `changed_columns` means "the columns the SET clause touched" on the local
   path (`broker.rs:93-95`) and "every column in the relation" on the WAL path
   (`wal_consumer.rs:629-633`). A subscriber filtering on `changed_columns`
   gets different answers depending on which producer is running.
2. `old_tuple` is **always `None`** on the local path
   (`wal_consumer.rs:162`), and is populated on the WAL path only when replica
   identity is FULL (`:543-549`, `OldTuple::full`). The "row left the view"
   semantics `broker.rs:111-118` documents therefore exist on one producer and
   not the other.

An effect node must declare which of these it needs, and a backend that cannot
supply it must fail the subscription at open time rather than silently deliver
a degraded event. That is the only honest shape.

**Commit-only publication.** The WAL path is commit-only, but **not because of
anything in `emit_for_tuple`** - because pgoutput logical decoding without
`streaming=on` only decodes committed transactions, which the code relies on
explicitly: `wal_consumer.rs:476-479` says the ordering property "would stop
holding under protocol-v2 `streaming=on`, where in-progress transactions
interleave; we do not enable it, and turning it on means revisiting this
branch." So commit-only on that path is a **configuration invariant of the
replication slot**, and an effect node can promise it only for as long as
`streaming=on` stays off. That belongs in the contract, not in a comment.

On the local path commit-only is code: mutations inside an explicit transaction
push into `pending_emits` (`context.rs:213-231`) and the settle path drains on
COMMIT (`transaction/mod.rs:1056`) or clears on ROLLBACK (`:1070`), on COMMIT
failure (`:1065`), and on rollback failure (`:1078`).

**Deduplication.** An effect node **cannot** promise deduplication, and this is
not a gap that can be closed downstream, because `ChangeEvent` carries no field
any consumer could deduplicate on. The design relies entirely on *mutual
exclusion of producers*, in three separate mechanisms:

- `is_app_suppressed` gates the local path while a consumer runs
  (`exec.rs:501`, `wal_consumer.rs:152`);
- `backend_publishes_committed_changes()` short-circuits the local path on
  SQLite entirely, because SQLite's preupdate/commit-hook publisher would
  otherwise "race the CDC publisher and produce duplicate identical live
  snapshots" (`exec.rs:494-499`);
- the startup window uses an **overlapping** guard - `SuppressGuard::activate`
  before provisioning, dropped after `spawn_consumer` returns, with the
  consumer installing its own guard before reporting ready
  (`cdc_lifecycle.rs:265-273`).

Note what the third one buys: the overlap prevents a **gap** (an unsuppressed
window with no consumer), at the cost of a window where suppression is held
twice - hence the refcount rather than a set (`wal_consumer.rs:70-73`). It does
**not** prevent a duplicate: a mutation whose SQL returns just before
suppression engages and whose WAL record is decoded by the newly started
consumer is published twice, with two byte-different events (different
`changed_columns`, different `old_tuple`), and nothing downstream can tell.

So: **an effect node promises at-least-once delivery with no dedup key.**
Anything stronger requires adding an identity field to `ChangeEvent` that both
producers can populate, which is exactly the same open question as the epoch.

**Savepoint behaviour.** This is the underspecified corner, and it is
**broken today**, which I verified by reading the settle path in full:

- `pending_emits` is `HashMap<String, Vec<ChangeEvent>>` keyed by `app_id`
  (`context.rs:231`). It is a **flat vec with no depth marks**.
- The nested settle arm (`transaction/mod.rs:988-1003`) pops the savepoint
  depth and runs `RELEASE SAVEPOINT` or `ROLLBACK TO SAVEPOINT`. It calls
  neither `clear_pending_emits` nor anything else that touches the queue.
  `clear_pending_emits` appears at `transaction/mod.rs:575`, `:671`, `:1024`,
  `:1037`, `:1065`, `:1070`, `:1078` - every one of those is a **top-level**
  path or a teardown guard. Not one is in the `Some(name)` arm.

Consequence: a nested `env.db.transaction(...)` that mutates and then throws
rolls back its rows via `ROLLBACK TO SAVEPOINT zs_sp_N`, but leaves its
`ChangeEvent`s in the app's queue. The outer COMMIT then calls
`drain_pending_emits_on_commit` (`:1056`) and publishes events for rows that no
longer exist. This is live on the PostgreSQL-without-WAL-consumer path -
`zeroship serve`, `pnpm dev`, and any deployment where the consumer has not
started - and it is masked in production by the very suppression that makes the
epoch problem hard.

The normative rule an effect node must state:

> **Effects are scoped to the savepoint frame that produced them.** The buffer
> is a stack of frames, not a flat vec. `SAVEPOINT` pushes a frame; `RELEASE`
> merges the frame into its parent **preserving order**; `ROLLBACK TO`
> **discards** the frame. Only the top-level COMMIT publishes, and it publishes
> the merged frame in production order. A COMMIT that fails discards
> everything, because the transaction's fate is indeterminate.

Shape:

```rust
pub struct EffectBuffer {
    /// frames[0] is the top-level transaction's frame.
    frames: Vec<Vec<PendingEffect>>,
}

impl EffectBuffer {
    pub fn push_frame(&mut self);
    /// RELEASE SAVEPOINT: append this frame's effects onto the parent.
    pub fn release_frame(&mut self);
    /// ROLLBACK TO SAVEPOINT: drop this frame's effects entirely.
    pub fn discard_frame(&mut self);
    /// COMMIT: take everything.
    pub fn take_all(&mut self) -> Vec<PendingEffect>;
    /// ROLLBACK / indeterminate COMMIT: drop everything.
    pub fn clear(&mut self);
}
```

This is SC-1's "effect buffer" (parent:76, :1045) given an actual contract.
The regression test that must fail before the fix: open a transaction, insert
row A, open a nested transaction, insert row B, throw from the nested body,
commit the outer, assert the subscriber sees exactly one event and it is A's.

## 2.3 What an effect node CANNOT promise

Stated plainly, because these are the arms that will otherwise be written and
then pass vacuously:

1. **A unique event id.** No producer has one; `ChangeEvent` has no field for
   one.
2. **Exactly-once.** The producer-exclusion mechanisms have an overlap window
   by design (`cdc_lifecycle.rs:265-273`).
3. **That the event's `changed_columns` reflects the mutation.** True on the
   local producer, false on the WAL producer.
4. **A before-image.** `None` on the local producer always
   (`wal_consumer.rs:162`); on the WAL producer only under
   `REPLICA IDENTITY FULL`, which `wal_consumer.rs:558` says is not set.
5. **An epoch stamped by the operation.** The production producer has no
   operation. See 2.5.
6. **Cross-collection ordering within a commit.**

## 2.4 The delivery contract as a table

| Promise | Local producer (`exec.rs:481`) | WAL producer (`wal_consumer.rs:589`) | SQLite CDC publisher |
| --- | --- | --- | --- |
| Commit-only | Yes, via `pending_emits` drain (`transaction/mod.rs:1056`) | Yes, but as a property of `streaming=off` (`wal_consumer.rs:476-479`) | Yes, via commit hook (`exec.rs:494-499`) |
| Savepoint-scoped | **No** - flat vec, `ROLLBACK TO` does not clear | N/A (rolled-back rows never reach WAL) | N/A |
| Per-row | Yes | Yes | Yes |
| `changed_columns` = SET clause | Yes | **No** - all relation columns (`:629-633`) | Backend-specific |
| Before-image | **Never** (`:162`) | Only under FULL replica identity | Preupdate hook has it |
| Epoch stamp | Possible (holds the lease) | **Impossible without an in-band carrier** | Possible |
| Dedup key | None | None | None |
| Runs when | Consumer absent AND backend is PG | Consumer present | Always on SQLite |

That table is the artefact. Every row of it is a decision the parent proposal
currently leaves to whoever ports first.

## 2.5 The epoch, restated as a requirement on the carrier

The parent (`:530-538`) says the epoch "has to reach the consumer **in-band in
the WAL stream**" and deliberately leaves the carrier open. Having read the
consumer, I can narrow the option set to three and say what each costs, which
is more useful than leaving it fully open:

- **A pgoutput `Message` (logical decoding message)** emitted by the mutation
  via `pg_logical_emit_message`. Lands in the stream in transaction order,
  before the tuples it describes. Cost: one extra statement per transaction
  that mutates, and `dispatch` currently discards `Message` explicitly
  (`wal_consumer.rs:569-573` lists it among "not surfaced"). This is the only
  option that costs no schema change.
- **A column on every published table.** Zero protocol work, and the consumer
  reads it off the tuple it already has. Cost: a system column on every creator
  table, which the parent's own "no ALTER existing tables for new system
  fields" stance (AGENTS.md) tolerates pre-launch but which permanently widens
  every row and every replicated tuple.
- **A consumer-side lookup keyed by the commit lsn.** Rejected: it reintroduces
  the round trip from a session-less path, which is the reason stamping at
  produce was chosen (parent:509-516).

The requirement, independent of carrier: **whatever the carrier, it must be
readable by a function whose only inputs are `(relations, rel_id, tuple)` or it
requires threading transaction state through `dispatch`.** That is the
signature fact the parent's "left open" leaves implicit, and it eliminates the
"a replication message field" phrasing at parent:534 - a `Begin`-carried field
is *not* readable by `emit_for_tuple` as written.

---

# PART 3 - the non-query capability ledger

## 3.1 Why it is a second file

sc3:117-119 binds the ledger's source column to "the set of `pub`/`pub(crate)`
functions **the crate** actually exposes", where the crate is
`zeroship-schema`. All four capabilities parent:333-336 names live in
`zeroship-data-v8`. Adding a row for `cdc_lifecycle::ensure_ready` to a
ledger whose gate arm derives the expected set from `zeroship-schema`'s exports
makes that arm fail in the "ledger has a row with no source" direction - the
gate is explicitly two-directional (sc3:129-130). So the promise at
parent:335 ("signatures in SC-3's ledger") is not merely unfulfilled, it is
**unfulfillable in that file**.

Second ledger, second gate arm, different derivation.

## 3.2 The file: `db/non_query_capabilities.tsv`

Row shape - four columns, deliberately different from the query ledger's:

```text
capability | neutral_signature | owner | today_impl | status
```

- `capability` - the stable name. This column is the gate's key.
- `neutral_signature` - the backend-neutral Rust signature the SPI exposes.
  **This is the column parent:335 asks for and SC-3 does not have.**
- `owner` - which object owns the resource's lifetime.
- `today_impl` - `file:line` of the concrete implementation, so the gate can
  assert it still exists.
- `status` - `neutral` | `backend-coupled` | `deleted-with-<feature>`.

Gate arm derivation (and the reason it differs): the query ledger derives its
expected set from a crate's exports. This one **cannot**, because a
backend-coupled capability is precisely one whose signature names a driver
type, and enumerating "functions that name a driver type" by grep reproduces
the source-enumeration failure the repo already records. Instead:

> The arm parses each row's `today_impl` `file:line`, asserts the named symbol
> exists there, and asserts that the symbol's signature contains a driver type
> **iff** `status == backend-coupled`. It reports the number of rows it ruled
> on and asserts non-zero. A row whose `today_impl` has drifted fails loudly;
> a capability that was made neutral without updating its row fails in the
> other direction.

That is a derivation from the thing being measured, satisfying sc3:136-138's
discipline without borrowing a derivation that cannot work here.

## 3.3 The populated ledger

Every `today_impl` below I read directly. Signatures are verbatim where quoted.

### CDC lifecycle

| capability | neutral_signature | owner | today_impl | status |
| --- | --- | --- | --- | --- |
| `cdc.acquire` | `fn acquire(app_id: &AppId) -> CdcLease` | Process-wide `LifecycleManager` (`Mutex<..>` static, `cdc_lifecycle.rs:60-61`); the lease is **not `Clone`** and exactly one V8 wrapper owns it (`:69-78`) | `cdc_lifecycle.rs:87` | neutral |
| `cdc.release` | `impl Drop for CdcLease` | Same; generation-guarded so a stale lease cannot release a restarted consumer (`:122-124`) | `cdc_lifecycle.rs:80-84`, `:113` | neutral |
| `cdc.ensure_ready` | `async fn ensure_ready(app_id: &AppId) -> Result<(), DbError>` | Caller awaits; the state machine (`Idle`/`Starting`/`Running`/`Failed`/`Stopping`) is in the manager | `cdc_lifecycle.rs:217` | neutral |
| `cdc.spawn_consumer` | `async fn spawn_consumer(&self, app_id: &AppId, worker_id: &WorkerId) -> Result<Self::ConsumerHandle, DbError>` | The `ChangeStream` impl; `ConsumerHandle` is an **associated type** deliberately not dyn-erased (`backend/mod.rs:935-940` gives the reason: `async fn` + associated type is not object-safe without `Box<dyn Future>` per call) | `backend/mod.rs:965`; PG `change_stream_pg.rs:170`; SQLite `backend/sqlite/cdc.rs:751` | **backend-coupled** - by design, and the ledger records the design rather than pretending otherwise |
| `cdc.deprovision` | `async fn deprovision(&self, app_id: &AppId) -> Result<(), DbError>` | `ChangeStream` impl | `backend/mod.rs:958` | neutral (signature names no driver type) |
| `cdc.pause_broker` | `fn pause_broker(&self, app_id: &AppId) -> BrokerPauseGuard` | RAII; `Drop` resumes and emits one `Resync` per subscription | `backend/mod.rs:978` | **zero production callers** - test-only (`tests/sqlite_integration.rs:1712`, `:2010`). See 6.2 |
| `cdc.engage_schema_pending` | `fn engage_schema_pending(&self, app_id: &AppId) -> SchemaPendingGuard` | RAII; `Drop` disengages + resyncs (`backend/mod.rs:1534-1541`) | `backend/mod.rs:988` | **zero production callers** - test-only (`tests/sqlite_integration.rs:1822`). Parent:543-544 says the same |
| `cdc.request_shutdown` | `fn request_shutdown(&self)` | `WalConsumerHandle` | `change_stream_pg.rs:91` | backend-coupled |
| `cdc.shutdown_app` | `async fn shutdown_app(app_id: &AppId)` | Process-wide manager | `cdc_lifecycle.rs:440` | neutral |
| `cdc.suppress_local_emit` | `fn suppress(app_id: &AppId) -> SuppressGuard` | Refcounted process-wide static (`wal_consumer.rs:72-73`) | `wal_consumer.rs:116` (`SuppressGuard::activate`) | neutral, **but its own doc is wrong**: `is_app_suppressed`'s comment at `:98-99` says "on this thread"; the map is a `LazyLock` static, i.e. process-wide |

### Key provision

| capability | neutral_signature | owner | today_impl | status |
| --- | --- | --- | --- | --- |
| `keys.resolve` | `async fn resolve(&self, app_id: &AppId, key_id: &KeyId) -> Result<AeadKey, DbError>` | `KeyStore`, per-isolate, `RefCell` cache keyed `(app_id, key_id)` (`encryption/keys.rs:258-273`) | `encryption/keys.rs:316` | neutral |
| `keys.source` | `enum KeySource { Local(LocalKeySource), Remote(Box<dyn KeyProvider>) }` | Constructed per backend | `encryption/keys.rs:211-225` | **backend-coupled** - `KeySource::PgAdminTable { pool: Rc<compio_postgres::Pool>, fallback }` (`:219-224`) names the driver pool directly. The neutral form replaces the variant with a `KeyProvider` trait object; the PG impl holds the pool |
| `keys.provider` | `trait KeyProvider { async fn root_key(&self, app_id: &AppId, key_id: &KeyId) -> Result<Option<[u8;32]>, DbError>; }` | Backend | **does not exist** | to-build. This is the one-line change that makes the whole capability neutral |
| `keys.lookups_count` | `fn lookups_count(&self) -> u64` | `KeyStore` | `encryption/keys.rs:307`, `#[cfg(test)]` | neutral, test-only. Backs the "default read does not load a column key" closeout gate (`:297-304`) |
| `keys.rotate_session` | `async fn rotate_session_keys(&self) -> Result<RotationOutcome, DbError>` | Platform-role session | `auth/keys.rs:69` - takes `pool: &Pool` | **backend-coupled** |

### Audit insertion

Every one of these takes `pool: &compio_postgres::Pool` and two return
`compio_postgres::Row`-derived values. The module's imports are
`use compio_postgres::{Client, Pool, Row};` (`audit.rs:43`).

| capability | neutral_signature | owner | today_impl | status |
| --- | --- | --- | --- | --- |
| `audit.ensure_table` | `async fn ensure_audit_table(&self, app_id: &AppId) -> Result<(), DbError>` | Platform session (the caller "MUST have already created the schema", `audit.rs:225-226`) | `audit.rs:227` | **backend-coupled** (`&Pool`) |
| `audit.next_schema_version` | `async fn next_schema_version(&self, app_id: &AppId) -> Result<SchemaVersion, DbError>` | Platform session | `audit.rs:341` | **backend-coupled** |
| `audit.write_row` | `async fn write_audit_row(&self, app_id: &AppId, row: &AuditRow) -> Result<AuditRowId, DbError>` | Platform session | `audit.rs:359` | **backend-coupled** |
| `audit.update_status` | `async fn update_audit_status(&self, app_id: &AppId, id: AuditRowId, status: TerminalStatus, ..) -> Result<(), DbError>` | Platform session | `audit.rs:403` | **backend-coupled** |
| `audit.read_processed` | `fn read_processed(row: &DbRow) -> Result<i64, DbError>` | - | `audit.rs:531` - takes `&compio_postgres::Row` | **backend-coupled**; neutralised by taking `&DbRow` |
| `audit.read_dead_letter_pks` | `fn read_dead_letter_pks(row: &DbRow) -> Result<Value, DbError>` | - | `audit.rs:553` | **backend-coupled** |
| `audit.unmask_decision` | `async fn record_unmask(&self, app_id: &AppId, rec: &UnmaskAuditRecord) -> Result<(), DbError>` | The unmask dispatch, which writes a row on **every** path incl. denial (`crud/unmask.rs:368-375`) | `crud/unmask.rs` dispatch | to-specify - Part 4 extends `UnmaskAuditRecord` |

Note the shape of the finding: **the entire audit capability is backend-coupled
at the signature level**, and `AuditRow` itself is `#[cfg(any(test,
feature = "test-helpers"))]` (`audit.rs:206`), so the production write path
constructs its row inline. Neutralising audit is a bigger change than
neutralising keys.

### Operator lifecycle

| capability | neutral_signature | owner | today_impl | status |
| --- | --- | --- | --- | --- |
| `operator.drop_namespace` | `async fn drop_namespace(&self, provisioning: &PlatformSession, app_id: &AppId, opts: &DropNamespaceOpts) -> Result<DropNamespaceOutcome, DbError>` | Two sessions: `backend` supplies the slot capability, `pool` is "a separate provisioning connection ... it must not be the worker login" (`drop_namespace.rs:95-98`) | `drop_namespace.rs:105` | **backend-coupled** (`&BackendHandle`, `&Pool`) |
| `operator.subscription_gate` | `subscription_count: usize` supplied **by the caller**, aggregated cluster-wide via `/internal/subscriptions/:app_id`; the orchestrator deliberately does NOT read the in-process broker because it "is per-isolate-thread and would undercount in a multi-worker cluster" (`drop_namespace.rs:69-77`) | Control plane | `drop_namespace.rs:64-78` | neutral - and it is the one place in this crate that already got the per-thread-vs-per-cluster distinction right |
| `operator.ensure_per_app_role` | `async fn ensure_per_app_role(&self, app_id: &AppId) -> Result<PerAppRoleOutcome, DbError>` | Platform session | `auth/bootstrap.rs:1548` | **backend-coupled**; parent:556-558 records it has **zero production callers**, which is why the reserved-prefix revoke it contains never runs |
| `operator.drop_per_app_role` | `async fn drop_per_app_role(&self, app_id: &AppId) -> Result<(), DbError>` | Platform session | `auth/bootstrap.rs:1681` | backend-coupled |
| `operator.ensure_admin_schema` | `async fn ensure_admin_schema(&self) -> Result<BootstrapOutcome, DbError>` | Platform session; installs the SECURITY DEFINER getter the key source reads (`encryption/keys.rs:214-216`) | `auth/bootstrap.rs:96` | backend-coupled |
| `operator.role_sql` | - | - | `auth/bootstrap.rs:1424`, `:1437`, `:1446`, `:1453`, `:1480`, `:1502` | **deleted-with-neutral-session** - six functions returning SQL strings. A neutral SPI has no place for `fn set_local_role_sql(app_id) -> String`; role application becomes `PlatformSession::assume_app_role(app_id)` |
| `operator.replication_watchdog` | `async fn watchdog(&self, app_id: &AppId) -> Result<Vec<SlotHealth>, DbError>` | Pool from `ensure_pool()` | `replication_ops.rs:14` -> `replication::watchdog_query(&pool, ..)` at `:32` | backend-coupled |
| `operator.drop_abandoned_slots` | `async fn drop_abandoned(&self, app_id: &AppId, inactive: Duration) -> Result<Vec<SlotName>, DbError>` | Same | `replication_ops.rs:49` -> `replication::drop_abandoned_slots` at `:68` | backend-coupled. Note `replication_ops.rs:1-6` says consumer *provisioning* is deliberately absent from this module so "an app cannot create a logical slot with no task responsible for it" - a real invariant worth carrying into the neutral SPI |

**Count today: 27 rows, 18 `backend-coupled`, 8 `neutral`, 1 `to-build`.**
That is a number produced by counting the table above, not a target. The
retirement condition mirrors SC-3's: the crate's manifest drops
`compio-postgres` only when `backend-coupled` reaches zero, and parent:336's
"any feature lacking one is deleted" applies to the `operator.role_sql` row.

---

# PART 4 - the unmask authorization input shape

## 4.1 What is actually broken today

Round 6 found that "carry an authorization decision" does not specify the
proof, and does not say what stops a frontend caller manufacturing an allow.
Having read the path, the answer is: **nothing does, for any role other than
`auto`.**

The three entries all funnel through
`check_unmask_authorization(app_id, actor: &Option<Value>, classification)`
(`crud/unmask.rs:305-323`). `actor` is app-supplied JSON. It reads
`actor["kind"]` as a bare string (`:313`) and hands it to
`MaskPolicy::allows(kind, classification)` (`:316`).

`sanitize_app_actor` (`:292-303`) strips **only** kinds in
`RESERVED_SYSTEM_ACTOR_KINDS`, which is `&["auto"]` (`:280`). Its own doc says
so, and its tests confirm the shape:
`sanitize_app_actor(Some(json!({"kind":"support_agent"})))` returns the object
unchanged (`:1734-1736`).

So for an app whose declared policy is `{"admin": ["pii"]}` - the exact shape
`MaskPolicy::from_json`'s doc gives as the example (`mask_policy.rs:116-121`) -
**any** handler, reachable from any unauthenticated route, can call
`unmask({ actor: { kind: "admin" } })` and read PII. `MaskPolicy::allows`
(`mask_policy.rs:102-110`) will return true. There is no binding whatsoever
between `actor.kind` and the authenticated end user; `env.auth` is not
consulted anywhere on this path.

DB-3 fixed impersonation of the **platform**. It did not fix impersonation of a
**creator role**, and the doc at `:282-291` describes the fix in terms that
make it sound total ("so app code can never impersonate the system actor" - true
and narrower than it reads).

## 4.2 The mechanism the platform already has

The runtime already carries a forgery-resistant per-request identity, and it is
**synchronous to read**, which is what Fork B needs:

- The gateway resolves the user from the app-session cookie and emits
  `ZeroShip-User` as `base64(JSON).<request_id>.<iat>.<hex-hmac>`, signed with
  the shared worker key (`gateway/src/oidc_rp.rs:1092-1113`). Its doc states
  the threat model exactly: "Signing prevents a caller with direct network
  access to the worker from forging a user identity, even if the worker's
  endpoint bearer-auth were ever bypassed" (`:1097-1100`).
- The worker verifies the MAC **and binds it to the request id** before V8 is
  entered (`worker/src/handler.rs:88-116`); a missing or unparseable
  `x-request-id` is a 401 (`:101-104`).
- The payload is `WorkerUser { id, email, name, avatar, email_verified, scopes }`
  (`gateway/src/oidc_rp.rs:1071-1090`). `scopes` is documented as "A PERMANENT
  kernel-contract field" (`:1081-1082`).
- `runtime/src/auth.rs::current_user` (`:91-109`) resolves it from V8's
  continuation-preserved invocation frame first, falling back to
  `executing_request_id`. **No I/O.** The module comment at `:9-19` explains
  why a `thread_local!` was wrong here and what replaced it - which is the same
  trap this brief flags for `ISOLATE_CTX`, already fixed once in this tree.

That is the value that cannot be manufactured from app JS. `sanitize_app_actor`
should not be sanitising an app-supplied actor at all; the actor should never
have been an app input.

## 4.3 The specified shape

```rust
// plan/unmask_auth.rs

/// The complete authorization input to an unmask. Produced by the
/// operation's `prepare` phase; consumed by the synchronous check.
/// Every field is platform-sourced. There is NO field an app handler
/// can set.
#[derive(Debug, Clone, PartialEq)]
pub struct UnmaskAuthorityInput {
    /// Who the platform says is calling. NOT `serde::Deserialize` -
    /// the only constructor takes a verified `WorkerUser` or is the
    /// in-Rust system constructor.
    pub principal: Principal,
    /// The operator ceiling, read per SC-6 in the prepare batch
    /// (autocommit) or on the separate platform-role session (in-tx),
    /// arriving AS A VALUE. Fork B.
    pub ceiling: CeilingValue,
    /// The deploy-scoped, UNTRUSTED creator half. SC-6:211-215:
    /// the `.zship` manifest carries no signature, so this is data whose
    /// only security property is that it cannot outlive its deploy.
    pub declared: DeclaredPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Principal {
    /// No verified identity for this turn. Denies everything.
    Anonymous,
    /// A platform actor. CONSTRUCTIBLE ONLY IN RUST, by a caller that is
    /// already inside the platform: migration, backfill, drift. The type
    /// has no public constructor outside `plugin-db`, which is a stronger
    /// statement than `sanitize_app_actor`'s string strip, because there
    /// is no string to strip.
    System(SystemActor),
    /// An authenticated end user, projected from the MAC-verified
    /// `ZeroShip-User` payload.
    EndUser(EndUserPrincipal),
}

#[derive(Debug, Clone, PartialEq)]
pub struct EndUserPrincipal {
    /// `WorkerUser::id`.
    pub subject: UserId,
    /// `WorkerUser::email_verified`. A role grant may require it.
    pub email_verified: bool,
    /// `WorkerUser::scopes` - the OAuth scopes granted to THIS app for
    /// THIS user. Gateway-signed, per-app, and already a permanent
    /// kernel-contract field.
    pub scopes: Vec<Scope>,
    /// The roles the PLATFORM resolved for this subject on this app.
    /// See 4.4 - this is the field that does not exist yet.
    pub roles: Vec<RoleName>,
    /// The request this identity was bound to. Carried so the audit row
    /// can name it and so a decision cannot be reused across requests.
    pub request_id: RequestId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemActor { Migration, Backfill, Drift }

/// The ceiling, as a VALUE. `Unreadable` is a distinct variant, not an
/// empty map, because SC-6:184-188 requires "a ceiling that cannot be
/// read is not an empty ceiling" and requires the denial to be
/// distinguishable in the audit row.
#[derive(Debug, Clone, PartialEq)]
pub enum CeilingValue {
    Read { version: CeilingVersion, allow: BTreeMap<RoleName, BTreeSet<MaskClassification>> },
    Absent,
    Unreadable { reason: CeilingReadFailure },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredPolicy(BTreeMap<RoleName, BTreeSet<MaskClassification>>);
```

The check stays synchronous and gains the ceiling as a parameter, per Fork B:

```rust
/// Fork B: no I/O. Every input is a value the operation already resolved.
pub fn check_unmask_authorization(
    input: &UnmaskAuthorityInput,
    classification: MaskClassification,
) -> UnmaskDecision;

#[derive(Debug, Clone, PartialEq)]
pub enum UnmaskDecision {
    Allow(UnmaskGrant),
    Deny(DenyReason),
}

/// The proof. It is not a bool, and it is not app-constructible: the
/// field is private and `plan::unmask_auth` is the only module that can
/// mint one. `UnmaskPlan` can therefore only be built by code that ran
/// the check.
#[derive(Debug, Clone, PartialEq)]
pub struct UnmaskGrant {
    principal: PrincipalDigest,
    classification: MaskClassification,
    /// Which ceiling version authorized. Recorded on the audit row and
    /// compared at the point of use, so a grant minted under a higher
    /// ceiling cannot be replayed after a revocation.
    ceiling_version: CeilingVersion,
    request_id: RequestId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DenyReason {
    /// No verified identity.
    Unauthenticated,
    /// The declared policy does not grant this role this classification.
    NotDeclared { role: RoleName },
    /// The declared policy grants it; the operator ceiling does not.
    /// This is the revocation arm and it MUST be distinguishable.
    CeilingDenied { role: RoleName, version: CeilingVersion },
    /// SC-6:184-188. Distinct from `CeilingDenied` in the audit row.
    CeilingUnreadable { reason: CeilingReadFailure },
    /// The column has no mask declaration (`crud/unmask.rs:387-397`).
    NotMasked,
}
```

The resolution rule, which replaces `MaskPolicy::allows` at
`mask_policy.rs:102-110`:

```
effective(role) = declared(role)  INTERSECT  ceiling(role)

allow(principal, classification) =
    match principal {
        Anonymous       => false
        System(_)       => classification IN effective("auto")
                           where, if the ceiling does NOT list "auto",
                           effective("auto") = ALL   // SC-6:221-233 fallback
        EndUser(u)      => any r in u.roles where classification IN effective(r)
    }

ceiling == Unreadable  =>  false, for every principal, always
ceiling == Absent      =>  false, for every principal, always
```

Two deliberate departures from today, both required by SC-6:

- `Absent` denies. Today a missing policy still permits `kind == "auto"`
  (`crud/unmask.rs:317-321`), which SC-6:184-188 says "is not defensible once
  the ceiling is the operator's revocation mechanism".
- The `auto` fallback survives, but as a property of the **ceiling**, not of
  the app's declared policy. SC-6:241-245 establishes that an operator ceiling
  **can** narrow `auto` by listing it, and that this is the point.

## 4.4 The missing piece, named rather than assumed

`WorkerUser` has **no role field**. I checked: `{id, email, name, avatar,
email_verified, scopes}` (`gateway/src/oidc_rp.rs:1071-1090`). So
`EndUserPrincipal::roles` cannot be populated from the header today, and
specifying the input shape without saying so would be exactly the "carry an
authorization decision" hand-wave round 6 rejected, moved one level down.

Three ways to populate it, with what each costs:

1. **Derive from `scopes`.** `RoleName` becomes a projection of the app's
   granted OAuth scopes. Zero new transport - `scopes` is already gateway-
   signed, already per-app, already permanent contract. Cost: the app's consent
   screen becomes the role-grant UI, which is a real semantic stretch for
   "support agent may see PII".
2. **A platform-owned role table, read in the prepare batch beside the
   ceiling.** Same session, same statement group, same privilege posture
   (`__zeroship_admin`, tenant-unreadable). Cost: one more column group in a
   batch that already exists on the autocommit path, and one more read on the
   separate platform-role session inside a transaction - which SC-6:295-305
   already concedes and prices for the ceiling itself.
3. **Extend `WorkerUser` with a signed `roles` claim.** The gateway would have
   to resolve app-scoped roles, which is control-plane knowledge it does not
   have; it would become a second place that knows about mask policy.

**Recommendation: (2).** It shares the ceiling's linearization point exactly -
the roles read and the ceiling read are the same batch, so a revocation that
lands between them is impossible - and it keeps the role model out of the
gateway. It is the only option where "revoke this user's support role" and
"lower the operator ceiling" have the same latency and the same failure mode.

## 4.5 What stops a frontend caller manufacturing an allow, stated as
## four independent barriers

1. **There is no app input.** `UnmaskFieldArgs` loses its `actor` field
   entirely (`crud/unmask.rs:59-69`). `sanitize_app_actor` and
   `RESERVED_SYSTEM_ACTOR_KINDS` are **deleted**, not extended - they exist
   only because an app-supplied actor exists.
2. **The principal is MAC-bound to the request.** Forging it requires the
   worker key AND the matching `x-request-id`
   (`worker/src/handler.rs:96-109`).
3. **`UnmaskGrant` has private fields and one minting module.** A plan that
   unmasks cannot be constructed without one, so "skip the check" is a
   compile error rather than a missed call.
4. **The grant names its ceiling version.** A grant minted before a revocation
   fails the version comparison at the point of use, which is what makes
   SC-6's "raising the ceiling does not retroactively authorize" (:273-274)
   hold in the *other* direction too.

Barrier 3 is the one that generalises: today the check returns `Result<bool>`
and three call sites are responsible for honouring it (`unmask.rs:405`,
`:1069`, `:1269`, per SC-6:42). A fourth call site that forgets is a silent
bypass. A proof token makes the count irrelevant.

## 4.6 The audit record

`crud/unmask.rs:368-375` says every path writes an audit row, granted or
denied. The record must carry enough to distinguish SC-6's arms:

```rust
pub struct UnmaskAuditRecord {
    pub app_id: AppId,
    pub collection: Ident,
    pub column: Ident,
    pub classification: MaskClassification,
    pub principal: PrincipalDigest,     // subject id or SystemActor; never raw PII
    pub request_id: RequestId,
    pub outcome: UnmaskAuditOutcome,
    pub reason_text: Option<String>,    // the caller's free-text rationale
    pub at: UnixMicros,
}

pub enum UnmaskAuditOutcome {
    Granted { ceiling_version: CeilingVersion },
    Denied(DenyReason),                 // carries CeilingDenied vs CeilingUnreadable
}
```

`DenyReason` being a typed enum in the row is what makes SC-6's second
acceptance arm ("the denial is distinguishable in the audit row from a ceiling
that was read and said no", :271-272) checkable rather than aspirational.

---

# 5. Arguing against my own most consequential choice

## 5.1 The choice: `PathSeg::Key(String)` bound as a parameter

This is the decision with the widest blast radius, because it changes what
"identifier" means in the IR. Every other choice here is local.

**The case against.**

*It splits identifier handling in two, and split guarantees are how fences get
moved one at a time.* SC-3:49-60 records exactly that failure: an earlier draft
called `validate_collection` the "sole guardian" and would have moved one of a
pair. `FieldPath` now has a head that goes through `Ident::parse_as` and a tail
that goes through the parameter binder. A future reader who knows "identifiers
are validated" will reasonably assume the whole path is, and a future
optimisation that inlines a constant key into the statement - which is a
*correct-looking* optimisation, since PostgreSQL's `->` operator takes a text
argument that a planner would like to constant-fold - reintroduces injection at
a site the identifier gate does not watch.

*It costs plan cacheability.* A parameterised key means `a->'x'` and `a->'y'`
render to the same statement text, which is good for the prepared-statement
cache and bad for the planner: PostgreSQL cannot use an expression index on
`(a->'x')` when the key is a parameter. An app that indexes a document field
gets a sequential scan. That is a performance regression the validated-key
design would not have, and performance is a stated top priority here.

*It is a bigger change than the alternative.* Restricting keys to
`[A-Za-z0-9_]` reuses `validate_field_name` verbatim (`query.rs:791-814`) and
ships with no new binder work.

**What would make me switch.** Two things, either sufficient:

1. **A measurement showing the index loss is real and material.** If a
   parameterised `->` demonstrably cannot use an expression index on the
   PostgreSQL version we target, and document-field indexes are on the roadmap,
   then the right answer is a **third** segment kind:
   `PathSeg::LiteralKey(Ident)` for keys that pass identifier validation, which
   render inline and are index-eligible, plus `PathSeg::Key(String)` for
   everything else, which renders as a parameter. That keeps injection
   impossible while recovering the index for the common case. I did not measure
   this and will not assert a figure.
2. **A decision that document fields are not queryable at all in v1.** If
   nested access is deferred, `FieldPath` collapses to `Ident` and the whole
   question disappears. Given that `validate_field_name` refuses dotted names
   today (`query.rs:807-814`) and nothing in the tree does nested access, this
   is a defensible scope cut - and it would be *better* than shipping a
   half-designed one. SC-3:91 asserts nested access as part of the pinned
   grammar, but SC-3 asserts it about a grammar that does not exist yet, so it
   is a design intent rather than a measured requirement.

I would not switch for the split-guarantee argument alone, because the fix for
that is a gate arm (invariant I2 walks the whole path and re-parses every
`Ident` in its role, and a mutation arm proves it fails when a key is inlined),
not a different design.

## 5.2 The lesser deviation, stated so it is not smuggled

SC-3:66-80 lists **five** families and names effects as one. I modelled effects
as a field of `WritePlan`, not a fifth `PlanBody` variant. The argument for my
shape: an effect never exists without a mutation - SC-3's own words are "the
publication a committed mutation **owes**" - so a peer variant would be
constructible in a state (`PlanBody::Effect(..)` with no write) that has no
meaning, and the type system should not admit meaningless states.

The argument against, which I find weaker but real: making it a peer would let
the **backfill/resync** path emit effects with no mutation behind them, which is
what `BrokerPauseGuard::drop` does today (one `Resync` per subscription,
`backend/mod.rs:1497-1500`). If resync is modelled as a plan rather than as a
broker-level operation, my shape blocks it. I think resync is correctly a
broker operation and not a plan, but if a later round decides otherwise, the
field becomes a variant and this paragraph is why.

---

# 6. Acceptance arms that cannot pass, or cannot fail

## 6.1 CANNOT FAIL - three arms bound to a path that does not exist

> sc3:161 - "An unsupported capability surfaces as a typed error from the
> concrete backend, with no dialect match above `backend/api.rs`."
>
> parent:180 - "driver types never appear in `frontend/*`, `backend/api.rs`, or
> a shared module"
>
> parent:1210 - "No driver types above `backend/api.rs`; one file names both"

**`crates/zeroship-data-v8/src/backend/api.rs` does not exist.**
`find crates/zeroship-data-v8 -name api.rs` returns nothing; the directory
holds `lock_guard.rs`, `mod.rs`, `postgres.rs`, `sqlite/`. There is no
`frontend/` directory either.

This is the recorded "bound to the wrong thing" family in its purest form. An
arm phrased as a **negative** over a path - "no dialect match above X", "driver
types never appear in X" - is implemented as a search that is expected to
return nothing. When X does not exist, the search returns nothing. **A clean
tree and a tree with the file missing print identically.** The arm can be
written, run, and reported green on today's code, on a half-ported tree, and on
a tree where the port was abandoned.

It is also not merely a stale-name problem, because the property it is trying
to state is **false today in two directions** and the arm as written would not
catch either:

- `crud/mod.rs:172-177` is a dialect match in the frontend, with `_ => Postgres`
  as the catch-all;
- `query.rs:5453-5466` branches on `SqlDialect` for `$ilike` **inside
  `zeroship-schema`**, which is further "above" any backend module than the
  frontend is.

**The fix is not to rename the path.** The arm must be re-bound to a property
the tree can hold whatever the file layout becomes: *no module reachable from
the plan builder may `match` on a value whose type has one variant per backend*.
That is checkable by enumerating the backend-discriminating types
(`BackendHandle`, `SqlDialect`) from source and asserting no `match`/`matches!`
on them exists outside a named allowlist of backend modules - a derivation from
the thing being measured, and one whose count is reportable and non-zero. And
it needs a **positive control**: a deliberately-inserted frontend dialect match
must make it red, or it is measuring nothing again.

## 6.2 CANNOT FAIL - the schema-pending / broker-pause transition arm

parent:540-544 makes the schema-pending window load-bearing: "A stamped epoch
differing from the subscription's forces a `Resync` before any row is
delivered... The existing schema-pending window (`broker.rs:790-796`, guard at
`backend/mod.rs:1527`) is the transition mechanism and **currently has no
production caller**."

I verified the second half: `engage_schema_pending` and `pause_broker` have
zero non-test callers across `crates/`; every hit outside `backend/mod.rs` is
in `crates/zeroship-data-v8/tests/sqlite_integration.rs` (`:1712`, `:1822`,
`:2010`). The `ChangeStream` trait carries `#[allow(dead_code)]`
(`backend/mod.rs:945`), and the trait's own docs say the guards' `Drop` bodies
"start as a no-op (`tracing::trace!` only) until [they are] wired"
(`:975-977`, `:986-987`).

Any acceptance arm of the form "a schema change pauses delivery and resumes
with a `Resync`" passes today by calling the test helper
(`pause_broker_for_tests`, `engage_schema_pending_for_tests`) and observing the
guard do its thing. It exercises the mechanism and rules on **zero production
transitions**, because no production code path can enter the window. It is the
`tests/ws_subscription_stub_gate.sh` shape the repo already names: an arm that
ran, over an empty set.

The arm has to assert the **caller** exists - that some production path,
reachable from the migration/deploy sequence, constructs the guard - and count
the transitions it observed. Otherwise the wiring can be deleted and the arm
stays green.

## 6.3 CANNOT PASS as written - SC-6's own acceptance contradicts SC-6's body

SC-6:263-270 justifies scoping the revocation arm to a creator actor with:
"The section above establishes that `auto` is **exempt from every policy and
ceiling** by way of `MaskPolicy::allows`."

The section above establishes the opposite. SC-6:241-243: "So an operator
ceiling **can** narrow `auto`, by listing it - which makes the ceiling
meaningful for the platform actor too." And SC-6:226-239 is an explicit
warning, in this document, that the "exempt from everything" phrasing was a
previous **over-correction** and is false: "the rule is a **fallback**, not an
exemption."

I checked the implementation rather than either paragraph:
`MaskPolicy::allows` (`crud/mask_policy.rs:102-110`) returns
`set.contains(classification)` when the map lists the role, and only falls
through to `role == "auto"` when it does not. The test
`auto_explicit_restriction_overrides_fallback` (`:570-581`) pins exactly that.
The body is right; the acceptance section reverted to the corrected-away claim.

The arm itself ("denies the next unmask by a creator actor") is passable. But
its **stated justification** would lead an implementer to build the `auto`
exemption as an unconditional bypass - which SC-6:229-231 says the shipped
reference explicitly forbids ("to restrict the system actor, the policy MUST
list `auto`"). An implementer following the checklist's reasoning and an
implementer following the document's body build two different systems, and only
one of them can pass the arm at :275-278 if that arm is ever extended to the
platform actor. SC-6:247-253 is also a near-verbatim duplicate of :241-245,
which is the textual signature of the revert.

This one is cheap to fix and worth fixing precisely because SC-6 already
documented the failure mode it then fell into two paragraphs later.

## 6.4 A fourth, offered without ranking

sc3:159 - "No plan carries SQL text; a mutation attempting it fails a source
gate." On today's code the plan **is** SQL text: `BuiltQuery { sql: String,
params: Vec<String> }` (`query.rs:77-80`). So this arm cannot pass until the
port is complete, which is correct and intended. What is *not* stated is what
the arm measures **during** the port, when both shapes coexist across 48
builders landing family by family (sc3:142-145). An arm that can only run at
the end is an arm that runs once; the mutation-testing formulation ("a mutation
attempting it fails") needs a target that exists throughout. I flag this as a
sequencing gap rather than a defect - I have not established that it cannot be
satisfied, only that SC-3 does not say how.
