# SC-3: the `DbPlan` IR and its source ledger

**Status.** PARTIAL, and specifically **BUILT-UNWIRED.** The IR is
`crates/zeroship-data-query-builder` - 6,282 lines, zero dependencies, carrying
the shared normative core plus the read, write and search families and a
PostgreSQL renderer
(`crates/zeroship-data-query-builder/src/render/postgres.rs`).
**No shipped binary links it.**
`zeroship-plugin-db` declares it under `[dev-dependencies]` only, and
`grep -rn data_query_builder crates/zeroship-plugin-db/src/` returns zero hits.
The runtime still executes SQL built by string concatenation in
`crates/zeroship-schema/src/query.rs`. The **source ledger does not exist**; (DELETED; runtime compilation now lives in `crates/zeroship-data-query-builder/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
`grep -rn source_symbol tests/ crates/` is empty.

Live defects cited by number (L11, L12, L16, L29, L31) live in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`.

---

## What it is

`DbPlan` is an identifier-validated, backend-neutral operation tree. Values are
always parameters; identifiers are always role-validated and quoted; a fragment
of SQL text is not a representable value. It replaces the string-concatenating
builders in `zeroship-schema::query`, one family at a time, against a checked
ledger that counts what is left.

### The scope

Measured at HEAD `a3706db6f`: `crates/zeroship-schema/src/query.rs` is 14,309 (DELETED; runtime compilation now lives in `crates/zeroship-data-query-builder/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
lines and exposes **87** `pub` / `pub(crate)` function items at any indentation.
They do not share a destination, and that is what makes the port tractable:

| Region | Destination |
| --- | --- |
| DDL / schema rendering (`build_create_table_with_fks`, `build_add_column`, `build_create_indexes`, the encryption and mask sentinel comments, the type mappers) | **Not `DbPlan`.** The runtime executes no DDL, so these belong to the migration and schema side or die with the crate |
| Runtime query builders (`build_find_*`, `build_insert*`, `build_update*`, `build_delete*`, `build_upsert*`, `build_aggregate*`, `build_distinct*`, `build_vector_search`, `build_spatial_near`, `build_where*`) | **`DbPlan` nodes.** This is the port |
| Identifier validation, quoting and dialect helpers (`validate_collection`, `validate_field_name`, `quote_ident`, `SqlDialect::binary_bind_placeholder`) | Classify individually; most belong in the IR's `ident` module |

Roughly 49 / 27 / 11 at this measurement. **Do not pin those numbers.** The
ledger derives membership at run time; a literal here is the hand-maintained
census this repository forbids.

**How the count is taken is part of the contract.** Counting `^pub fn` anchored
at column zero is blind to indented methods - it misses
`SqlDialect::binary_bind_placeholder` and `::wrap_binary_bind_param`, both
runtime-side. The gate counts `pub` / `pub(crate)` items at any indentation, and
its own arm must be proved against a fixture containing an indented method. A
ledger whose instrument cannot see part of its source is worse than no ledger,
because it reports completeness it never measured.

Two identifier fences are security-critical and are **a pair, not a single
guardian**: `validate_collection`'s reserved-prefix check (`pg_`,
`__zero_migrate`, `__zeroship`) fences *table* names, and `RESERVED_NAMES`
fences *column* names (`_`, `__zs_`, `__zeroship_`, `sqlite_`, the `_masked`
suffix, the six classification names). Moving one without the other is the more
dangerous half of a bulk move, because the survivor makes the namespace look
defended. Both move together, each with its own gate arm and its own vectors.
The IR's `crates/zeroship-data-query-builder/src/ident.rs` already states both,
as `PLATFORM_RESERVED_COLLECTION_PREFIXES` plus `BACKEND_CATALOG_RESERVATIONS`
on the table side and `COLUMN_RESERVATIONS` on the column side; when the port
lands, one of the two copies is deleted rather than both maintained.

### The families

- **read**: projection (caller-narrowable, always unioned with the required
  platform fields, never `SELECT *`), filter, ordering, pagination, aggregate
  and distinct;
- **relation**: a join carrying kind, target collection, on-condition, its own
  nested projection and its own filter, producing a nested result. Lowerable
  either as a SQL join or as the batched second read used today, within the
  boundary fixed below;
- **write**: insert, update, upsert with an explicit conflict target, delete,
  each carrying its returning shape. `RowLimit` is mandatory on `Update` and
  `Delete` with no "all rows" value;
- **search**: vector and spatial, **and only those two.** Full-text search was
  deleted (L11); the IR must not carry a family for a feature that no longer
  exists. Both surviving kinds read declared schema **inside** the concrete
  backends (`crates/zeroship-data-postgres/src/postgres.rs`,
  `crates/zeroship-data-sqlite/src/{vector.rs,spatial.rs}`), which is why they
  cannot be ported as a wrapper;
- **unmask reads**, which carry an authorization decision rather than producing
  it;
- **effects**: the publication a committed mutation owes the broker. A bare rows
  result cannot express it, and committed-change publication already differs by
  backend today (`emit_for_rows`, `crates/zeroship-data-engine/src/exec.rs`).

Per-family node spelling is each family's own to fix. The **expression
sub-grammar** underneath them is not: filters, projections, ordering keys and
search predicates all compose the same comparison, logical, path and literal
nodes, so whichever family ports first would otherwise fix that shape for every
family after it.

### The normative core

#### Identifiers

```rust
/// The ONLY constructor is `parse_as`. The field is private; there is no
/// `From<String>`, no `Deserialize`, and no `into_string`.
pub struct Ident(String);

pub enum IdentRole { Namespace, Collection, Column, Alias, Constraint, Index }

impl Ident {
    pub fn parse_as(raw: &str, role: IdentRole) -> Result<Self, IdentError>;
    pub fn as_str(&self) -> &str;
}
```

`Deserialize` is excluded deliberately: deriving it would reconstruct the
newtype from wire bytes without ever running `parse_as`, which is the usual way
a validated-newtype guarantee is lost.

The role matters because the fences differ - a table name by the
`validate_collection` prefix list, a column name by `RESERVED_NAMES`, an alias by
a third list that deliberately permits a single leading `_` (the platform's own
`_distance` on vector search) which a column does not.

Identifiers are quoted unconditionally at render time. Quoting conditional on a
reserved-word list is not sound: the equivalent list is empty for PostgreSQL in
at least one widely used SQL library, so soundness would rest entirely on
whoever set the "needs quoting" flag upstream.

#### The expression sub-grammar

```rust
pub enum Predicate {
    And(Vec<Predicate>),   // empty renders TRUE
    Or(Vec<Predicate>),    // empty renders FALSE - deliberately not And's
    Not(Box<Predicate>),

    /// NEITHER side may be a null literal. Nullness is expressed only by IsNull.
    Compare { lhs: Operand, op: CompareOp, rhs: Operand },

    /// A null member means IS NULL - `In` lowers to `(lhs IN (..) OR lhs IS
    /// NULL)` and `NotIn` to `(lhs NOT IN (..) AND lhs IS NOT NULL)`. NOT
    /// "whatever SQL does natively": `lhs IN (NULL)` matches NOTHING on
    /// PostgreSQL (measured), so lowering a null member literally would revert
    /// the behaviour this project shipped and tested.
    /// `LiteralSet` is non-empty by construction; empty membership is
    /// `Const(false)` for `In` and `Const(true)` for `NotIn`.
    Membership { lhs: Operand, op: MembershipOp, set: LiteralSet },

    Pattern { lhs: Operand, op: PatternOp, pattern: Literal, escape: Option<char> },

    /// The ONLY way to test nullness. A distinct node, not an operator.
    IsNull { operand: Operand, negated: bool },

    Range { lhs: Operand, low: Operand, high: Operand, inclusive: RangeBounds },

    /// Produced by simplification; never authored.
    Const(bool),
}

pub enum Operand { Path(FieldPath), Lit(Literal), Aggregate(AggregateRef) }
```

Three of these are types rather than conventions because each encodes a defect
this project hit:

- **`IsNull` is a separate node** and `Compare` forbids null operands. The
  builder this IR replaces converted `Value::Null` to an empty string and pushed
  every `$in` array element through that conversion, so `{ f: { $in: [null] } }`
  compiled to `f IN ($1)` with `$1 = ''`: on a text column that silently matched
  rows whose value is the empty string and missed every row that is actually
  NULL. Wrong results, no error, no diagnostic. The knowledge that null is
  special was present all along, in a comment one function away from the call
  site that ignored it. A comment is not a type, and neither is a fix - nothing
  stops the next author adding a third membership operator that reaches for the
  same conversion. The node distinction makes the broken form unrepresentable
  rather than merely currently absent.
- **`And([])` renders `TRUE` and `Or([])` renders `FALSE`**, stated so the
  renderer does not invent a convention.
- **`LiteralSet` is NON-EMPTY by construction**, and enforces a homogeneous
  element type and the cardinality cap (`MAX_MEMBERSHIP_LIST_LEN`), so all three
  bounds travel with the type instead of being re-checked at each call site. The
  lower bound is not covered by `And([])` / `Or([])` - that is a different node -
  and without it a `Membership` over an empty set renders the syntactically
  invalid `IN ()`. Empty membership is therefore not representable as a
  `Membership` at all.

#### Depth is bounded at construction, and it has to be

**Depth is the denial-of-service vector; size is not.** Measured against sqlglot
30.17.0 on CPython 3.13 at the default recursion limit: 43 nested function calls
raise `RecursionError`, 49 nested parens, 54 nested `CASE`, 108 chained `NOT` -
while a **100,000-element `IN` list of 589 KB parses fine in 2.33s**. Roughly a
hundred bytes kills it and half a megabyte does not. A width cap alone
(`MAX_MEMBERSHIP_LIST_LEN`) bounds the wrong dimension.

Rust has no recursion limit, so a deep tree is a stack overflow and a process
abort - and the worker runs many apps per thread under LRU isolate eviction, so
that is the process and every co-tenanted app, not one failed request.

**The bound must be at CONSTRUCTION, not in the traversals.** `Predicate`
derives `Ord` and `Hash`, which recurse structurally, and the canonicalizing
sort invokes them. So `canonical()`, the renderer, the aggregate walk **and** the
derived comparisons all recurse; rewriting the hand-written traversals
iteratively covers three of four and leaves the derive. Only a construction-time
bound covers all of them. `MAX_PREDICATE_DEPTH = 16` mirrors
`MAX_FILTER_NESTING_DEPTH = 16` in `zeroship-schema::query`, the bound the
translator being replaced already applies - the recurring failure this project
makes is leaving a bound at the old call site instead of moving it with the
operation. The depth check itself is **iterative**: a recursive depth check
overflows on exactly the input it exists to reject.

A node-count cap is not a substitute and cannot see a 43-deep tree.

#### Projection

```rust
pub struct Projection { fields: Vec<ProjectedField> }   // non-empty, no Star

pub struct ProjectedField {
    pub source:   ProjectionSource,
    pub alias:    Ident,
    pub exposure: Exposure,
}

pub enum ProjectionSource {
    Column(Ident),
    Path(FieldPath),
    Aggregate(AggregateRef),
    SearchScalar(SearchScalarKind),
}

/// Why the field is in the list. `Platform` fields are added by the planner
/// and stripped before the row reaches user code unless the declared schema
/// also names them.
pub enum Exposure { Declared, Platform, Internal }
```

**No `Star` variant** makes "never `SELECT *`" a property of the type. A
projection slot that can hold a caller-supplied string is how a widely deployed
ORM shipped a CVSS 10.0 injection; the slot here cannot hold one.

**`Exposure` answers whether the unioned platform fields are visible to the
creator.** Without it the union rule is ambiguous in a way that shows up as
either a leaked `deleted_at` or a missing one.

**`alias` is a first-class slot**, which is what the relation family needs:
mandatory aliasing for cross-collection column collisions has somewhere to live
rather than being a rendering trick.

**Masked columns need no projection variant, because of the storage flip.** The
field's own column (`ssn`) holds the **masked** string and the sibling
`__zs_raw__ssn` holds the real value (`RAW_COLUMN_PREFIX`, `raw_column_name`,
`raw_column_for_field` in `zeroship-schema::query`). A builder that knows nothing
about masking selects and filters the natural name, which is the mask, and leaks
nothing. Plaintext has exactly one reader: the explicit unmask API, where the
authorization check and the audit row already live. The IR carries that
guarantee in `COLUMN_RESERVATIONS`, whose `Prefix("__zs_")` makes
`__zs_raw__ssn` unconstructable as an `IdentRole::Column`, so the raw column is
not nameable in a filter, a projection or a sort. The unmask family, when it is
spelled, is the one path allowed to reach it, and it must reach it through its
own node rather than by relaxing that fence.

#### What the types cannot enforce

Named rather than assumed, because these need property tests: an aggregate
operand is legal only in a `HAVING` position; a plan's parameter count must equal
the placeholders its lowering emits; a backend's refusal must be a typed error
from its own module rather than an empty result; and **the IR cannot decide that
a column is masked or encrypted**, because that requires the declared schema,
which lives outside this leaf crate. The node makes the correct form expressible;
the schema-aware layer above is what must choose it.

### Plan shape must render deterministically, or the statement cache misses

The driver has prepared-statement caching (`Client::new_with_statement_cache`,
`statement_cache_execution_threshold`; `libs/compio-postgres/src/bind.rs`,
`libs/compio-postgres/src/prepare.rs`) **and it is switched off.**
`statement_cache_capacity` defaults to **0** (`libs/compio-postgres/src/config.rs`)
and `plugin-db` never calls `prepare_cached`, so every operation today sends an
unnamed statement that PostgreSQL parses and plans from scratch, every time.

The IR makes that cache worth turning on: values are always parameters, so two
calls differing only in their arguments produce **identical SQL text** and reuse
one prepared plan.

That benefit is contingent on a property nothing else states: **rendering a given
plan shape must be byte-stable.** If a lowering iterates a `HashMap` to emit a
projection list or an `AND` chain, column order varies between runs, the SQL text
varies with it, and every call is a cache miss - a silent performance regression
that no correctness test would notice. Renderers therefore emit from ordered
collections.

**The arm that checks it is not the obvious one.** Rendering the same plan twice
and asserting the SQL matches does **not** test canonicalization: rendering one
in-memory unordered map twice can preserve *that instance's* iteration order, so
a non-canonical implementation passes. Freshly randomizing the maps is worse - it
turns the arm into a coin flip that fails at random and gets re-run until green.
The arm is **two semantically identical plans built by opposite insertion
permutations**, asserted against ONE canonical SQL/parameter fixture, plus a
mutation that deletes the canonical sort and proves the arm turns red.

### Boundary decision 1: projection narrowing and joins are first-class

A join planner is not a security concern on this path: every table in a join
lives in the *same app's schema* and identifiers are fenced to that namespace,
so a join cannot cross a tenant boundary. What remains is a **cost** concern,
which belongs to limits and metering.

Today the builder emits **zero** SQL joins and relations are served by a batched
second read: collect parents, deduplicate foreign keys, one `WHERE id IN (..)`
per relation, stitch in memory (`sdks/db/src/collection/relations.ts`,
documented in `docs/reference/db.md`). That stays valid as a *lowering strategy*.
It stops being the only one.

**Projection first, because it is smaller and independently valuable.** The
relation load always fetches every column of the target, which is wasteful and
drags masked and encrypted columns nobody asked for through the mask pass.
Caller-supplied narrowing is added under one rule enforced in the constructor
rather than remembered:

> A narrowed projection is always **unioned with the required platform fields**.
> Narrowing can request fewer *declared* fields; it can never drop a column the
> system needs to function.

The required set is `SYSTEM_FIELD_NAMES` - `id`, `created_at`, `updated_at`,
`created_by`, `updated_by`, `version`, `deleted_at`.

**`id` is load-bearing in four independent ways**, worth enumerating because no
single one of them would be found by testing the others:

1. the **relation stitch** matches children to parents by it, so dropping it
   breaks joins rather than returning less;
2. the **unmask handle** plucks `row_pk` from it, and a row without one gets an
   empty string that `unmask()` then rejects;
3. **encrypted columns** bind it into the AEAD tag via `canonical_aad`, so
   without it the ciphertext is undecryptable;
4. **change events** correlate on it - `emit_for_rows` reads `row["id"]` with an
   `_id` fallback (`crates/zeroship-data-engine/src/exec.rs`), so a projection that
   dropped the PK emits an event with no key and a subscriber that cannot match
   it to anything.

A narrowing implementation could plausibly be written, reviewed and shipped
against any one of those without the other three ever being exercised.

**Then joins.** The node carries kind (inner / left), the target collection, the
on-condition, and its own nested projection - and produces the **nested** result
shape the current API already returns (`todo.userId -> { .. }`), so the creator
surface does not change under anyone. Aliasing becomes mandatory rather than
optional, because column names collide across joined collections;
`IdentRole::Alias` exists for exactly this. Relation-level filters fall out once
the node exists, closing the second documented v1 limitation in
`docs/reference/db.md`.

Nested joins need a bound, and it is **a cap on the TOTAL number of relation
nodes in a plan, not on nesting depth**. A savepoint-style depth cap guards
*runtime recursion* - a chain that deepens as code executes - whereas a plan is a
**value**, fully known before anything runs, and its relations **branch** rather
than chain. Five sibling relations at depth one cost the planner what five joins
cost; a depth-only cap would wave them through.

Eight is the number, and it is not arbitrary: PostgreSQL's own
`join_collapse_limit` and `from_collapse_limit` are both **8** on the tier we
target, with `geqo_threshold` at **12** (measured on the project's PG 16
container; the compose topology still pins `postgres:16`). Past that boundary the
planner stops exhaustively reordering joins and eventually switches to a genetic
optimiser, so a plan carrying more than eight relations has left the region where
its cost is predictable. Borrowing the engine's own threshold means the limit
tracks the reason it exists.

#### Strategy is chosen by relation CARDINALITY, not by backend preference

"The backend chooses how" is too loose, and the industry has already paid for
that looseness: one `JOIN` per to-many include multiplies parent rows by the
product of the children, so two collection includes over a modest page return an
enormous result for a small amount of data. Entity Framework had to retrofit an
explicit split-query mode; Prisma made it a per-query knob with a global default
rather than an engine decision.

Every relation today is **to-one**: the `with` key must be a `t.ref(...)` field
declared on the *parent* (`docs/reference/db.md`), i.e. a foreign key, and no
reverse or to-many relation exists in the surface. So:

- **to-one: join.** One round trip, no duplication, and the batched read remains
  a legal lowering for a backend that prefers it.
- **to-many (when it arrives): never a naive join.** Either split queries - the
  batched read this system already implements - or a lateral subquery aggregating
  children into JSON, which gives nesting in one round trip with no parent
  duplication. PostgreSQL has `json_agg`; SQLite has `json_group_array`, and
  whether their behaviour matches closely enough for parity is a question to
  settle before relying on it, not after.

The strategy therefore belongs in the plan as an explicit property with a
cardinality-derived default, not as a backend's private choice. A creator hitting
a pathological plan should be able to see which strategy was used.

**"The plan says what, the backend chooses how" is FALSE for three plan shapes**,
where the two lowerings are observably different rather than merely differently
costly:

- **Pagination with an inner-kind relation.** `isDone` is computed from the
  *parent* rows before any relation load - `rows.length <= numItems` on the raw
  parent result - and the cursor is deliberately computed before relation loading
  so the captured `orderBy` value is the raw column rather than an overwritten
  joined object (`sdks/db/src/query.ts`). A fused inner join drops childless
  parents **before** `LIMIT`, so it returns a different page, a different
  `isDone`, and a different cursor. Same plan, two answers.
- **Ordering by a child column.** The cursor is `{ orderBy, lastValues, lastId }`
  (`sdks/db/src/query.ts`) - the *parent's* ordering values and the parent's id,
  with no slot for a child's. A fused join can order by a child column; the
  batched strategy cannot even express the resume point. The cursor format, not
  the SQL, is the limit.
- **A filtered left join**, where the child predicate belongs in `ON` under the
  fused lowering and in the child's own `WHERE` under the batched one. Placed
  wrongly it silently converts the left join to an inner one, dropping parents
  rather than nulling children.

So the strategy is free to vary **only** for plans that neither paginate across a
relation nor order by a child column. Beyond that boundary the plan pins the
strategy, and a plan requesting a shape its pinned strategy cannot serve is
**refused with a typed error** rather than served differently. Cursor resumption
across a strategy change is not supported: a cursor minted under one lowering is
not portable to the other.

#### Live queries over relations are refused until read-set tracking understands them

A relation plan cannot be handed to the live-query path as it stands, and the
failure is silent rather than loud - the subscription simply never fires.

`crates/zeroship-data-core/src/read_set.rs` contains the string `relation`
**once**, in a comment, and is otherwise entirely relation-unaware. Two
consequences:

- **A conjunct on a child column never matches.** When a column is absent from
  the change event's tuple, `Conjunct::matches_text` returns non-match, on the
  documented reasoning that the subscriber "won't see this event but it'll see
  the next one that DOES carry the column". That is correct for a TOASTed value,
  which does arrive later. It is **never** true for a child column, because a
  parent's change event will not carry it in any future event either. A relation
  filter recorded as an ordinary conjunct therefore suppresses delivery
  permanently.
- **A change to a child row produces no parent event at all**, so a live query
  whose result depends on joined data is never invalidated when that data
  changes. Whether the lowering fuses the join or splits it, the subscription is
  registered against the parent collection.

Neither is a lowering bug that a better renderer fixes; the read-set model has no
vocabulary for "this result depends on rows in another collection". Until it
does, **a plan carrying a relation is refused on the live-query path with a typed
error**, rather than accepted and silently starved. A subscription that never
fires is indistinguishable, from the creator's side, from a table that never
changes.

**The real work is not the SQL.** It is that the mask and unmask passes become
**per-source-collection**. Today a result row comes from one collection, so policy
lookup is trivial and implicit. A joined row carries columns from several
collections with different mask policies and different classifications, and a
single per-result policy lookup would silently apply the parent's policy to the
child's columns. That belongs to SC-6's contract rather than to this document's
grammar.

### Boundary decision 2: a backend that cannot serve a node REFUSES; it does not emulate

Refusal keeps the two tiers honestly different rather than plausibly similar,
which matters because the dev tier is SQLite and the production tier is not. The
consequence is owned rather than discovered: **the set of nodes SQLite refuses is
itself a contract**, and it belongs in `docs/reference/sqlite-divergences.md`
beside the divergences already recorded there.

The failure mode of the other choice is silent. A provider that quietly evaluates
an untranslatable predicate outside the database returns correct-looking rows
having filtered them **after** they left the tenant's boundary - a correctness and
performance problem in an ordinary ORM, a security failure here. The same refusal
covers the escape hatch this platform will be tempted by: Convex, the closest
positional competitor, also untrusted app code and also no SQL surface, escapes
into the *runtime* - its `.filter()` runs arbitrary TypeScript over a document
range, explicitly not using the index. That shape is the one most naturally
available here, since this platform also runs JS, and it is exactly what this
decision refuses.

### Boundary decision 3: `DbPlan` is in-process only and MUST NOT derive `Serialize` or `Deserialize`

The moment a plan crosses a process boundary it is a wire format, and a wire
format needs versioning, compatibility rules and a migration story - the exact
burden this document exists to avoid. Deriving `Serialize` for a debug dump or a
test fixture is a one-line change that silently creates that obligation. If a
plan must be inspected, render it through a `Debug`/display path that is
explicitly not a format anyone may parse.

`Deserialize` is worse than symmetric. Deserialising an AST is an
arbitrary-construction surface: sqlglot's `Expression.load()` calls `__import__`
on a dotted name taken **straight from the JSON payload** (verified empirically).
An IR whose producer is creator code cannot afford a reconstruct-from-bytes path
that bypasses every constructor invariant above.

This is structural rather than reviewed: the IR crate declares **no dependencies
at all**, so `serde` is not in scope and the derive does not compile.

### The ledger

A checked file, not prose. One row per source entry point:

```text
source_symbol | source_range | destination | status
```

`destination` is one of: a named `DbPlan` node, the IR's `ident` module, the
migration side, or `deleted-with-<feature>`. `status` is `ported` or `unported`.

- A **gate arm** asserts the ledger's source column equals the set of
  `pub`/`pub(crate)` functions the crate actually exposes, so a new entry point
  cannot appear without a row.
- **That set equality is the assertion, and the count is not.** The arm reports
  its ruled-on count as diagnostic output. It neither pins that count to a
  literal nor accepts a non-zero count as the property. A non-zero floor is
  degenerate - one row clears it, and an arm a single row satisfies cannot
  distinguish an exhaustive ledger from an abandoned one. A literal expected
  count is a hand-maintained census; the arm compares two sets it derives at run
  time, and a mismatch in either direction fails.
- **`zeroship-schema` is retired when `unported` reaches zero AND every plan
  family has landed** - two conditions, because the first does not imply the
  second.

  The ledger enumerates what `zeroship-schema` exports, so anything that was
  never in `zeroship-schema` is invisible to it however exhaustive it is over
  what it does cover. The relation family is exactly that case: the string
  `relation` occurs **once** in `query.rs`, in a comment, and the batched loader
  lives in TypeScript (`sdks/db/src/collection/relations.ts`). It has **no source
  row to port**, so `unported` reaches zero without it and a ledger-only
  retirement would fire with a whole family unbuilt. The families are therefore a
  **separate checklist** with its own arm, not a derived consequence of the row
  count.

### Acceptance shape

- The ledger exists and **the gate asserts set equality** between its source
  column and the crate's actual `pub`/`pub(crate)` exports, failing on a
  difference in either direction. It reports its ruled-on count as well, but the
  count is diagnostic output, not the assertion.
- No `DbPlan` node ships without a ledger row naming its source.
- Every plan family has parity fixtures producing equivalent resolved results on
  both backends, after documented dialect lowerings.
- No plan carries SQL text; a mutation attempting it fails a source gate. **The
  arm must first assert that the plan types it scans exist**, or it is a negative
  check over types that may not have been written, matching nothing and reporting
  success.
- **A narrowed projection still contains every required platform field**, on
  every family that projects, asserted against a projection that asks for one
  declared column. This is the arm that catches narrowing implemented as a filter
  over the caller's list.
- **A narrowed projection over a MASKED column renders the masked column and
  never the raw sibling**, asserted on the rendered SQL, not on the shape of the
  returned row. A row-shape assertion passes while the value is plaintext,
  because the column arrives under the name the caller asked for either way.
- **A predicate deeper than `MAX_PREDICATE_DEPTH` is refused at construction**,
  with the check itself iterative so it does not overflow on the input it exists
  to reject, and with the derived `Ord`/`Hash` recursion covered by the same
  bound.
- **A joined row's mask policy is resolved per source collection.** This arm
  lives in **SC-6's acceptance shape**, which owns per-source-collection mask
  policy. It is named here because SC-3's relation node is what makes the case
  reachable. Its discriminating form: a parent and child whose policies **differ
  on the same classification** - the case a single per-result lookup gets wrong
  while every same-policy fixture passes.
- **A relation plan submitted to the live-query path is refused with a typed
  error**, asserted by subscribing and observing the refusal - not by observing
  that no event arrives, which is exactly what the broken behaviour also looks
  like.
- **Two semantically identical plans built by OPPOSITE insertion permutations
  render to one canonical SQL/parameter fixture** - with a mutation that deletes
  the canonical sort and proves this arm turns red. Asserting instead that the
  same plan renders twice identically is **not** this arm and does not test
  canonicalization.
- **A plan that paginates across a relation, or orders by a child column, PINS
  its strategy** - and a plan asking for a shape its pinned strategy cannot serve
  is refused with a typed error. Asserted on the three divergent shapes named
  above, since a parity arm run only on plans where the strategies agree is a
  tautology.
- **Both lowerings of a relation load produce equivalent resolved results**, on
  plans within the agreed boundary: the batched two-read strategy and the join
  strategy must agree on rows, nesting, and null handling for a missing or null
  foreign key (`docs/reference/db.md` already fixes that behaviour and it must
  survive the change of strategy).
- **`DbPlan` and its node types do not implement `Serialize` or `Deserialize`**,
  asserted by a source gate rather than by review. **This arm must also first
  assert the types it rules on exist**, for the same reason as the no-SQL-text
  arm.
- **Every node a backend refuses is listed** in
  `docs/reference/sqlite-divergences.md`, and the arm compares the documented set
  against the set the backend actually rejects. A refusal that is real but
  undocumented is the same surprise as an emulation.
- An unsupported capability surfaces as a typed error from the concrete backend,
  with no dialect match above the neutral backend boundary. **That arm must first
  assert the boundary file exists.** `backend/api.rs` does not exist -
  `crates/zeroship-plugin-db/src/backend/` holds `mod.rs` and `cancel.rs` only -
  and neither does the `DbBackend` trait the parent document lists among the
  neutral types to introduce (`grep -rn "trait DbBackend" crates/` is empty). A
  negative grep scoped to a path that was never created matches nothing and
  reports success.
- `zeroship-schema` is removed from `zeroship-plugin-db`'s manifest **only** when
  `unported` is zero and the identifier-fence pair has landed in the IR with its
  own arm.

---

## Why it is this way

- **The shared expression grammar is fixed here, not discovered.** An IR
  discovered incrementally is shaped by whichever call site is ported first. A
  family that gets its own node shape wrong costs that family a revision; a
  shared expression node fixed wrongly by whoever ports first costs every family
  after it. Per-family spelling is therefore deliberately left open; the shared
  core is not.
- **The IR crate is a leaf with zero dependencies, and that is load-bearing.**
  `zeroship-schema` is the obvious dependency and is refused: its manifest
  declares `compio-postgres`, `zeroship-core` and `tracing`, so depending on it
  would drag a live PostgreSQL driver into a crate whose whole claim is that it
  builds and tests without a database, a runtime or an isolate. The identifier
  fences are re-stated in the IR's own
  `crates/zeroship-data-query-builder/src/ident.rs`, not wrapped, and the
  duplication is temporary by contract: when the port lands, one of the two
  copies is deleted.
- **A ledger's instrument must see all of its source.** A count taken by a
  pattern blind to part of the file reports completeness it never measured.
- **The identifier fences move as a pair.** A survivor makes the namespace look
  defended.
- **Values are parameters, identifiers are validated and quoted.** The plan never
  carries a fragment of SQL text. A "safe raw" escape hatch is not an acceptable
  substitute: Prisma's tagged-template version is bypassable in one line of
  JavaScript (`stringsArray.raw = [query]`), documented and unfixed.
- **A backend that cannot serve a node returns a typed error from its own
  module.** No capability is discovered by a dialect match in the frontend.
- **The no-serde rule is structural, not reviewed.** Keep the IR crate's
  dependency list empty; adding any dependency that pulls `serde` re-opens the
  derive.
- **A relation family cannot land without a qualified column reference.** A
  joined plan must name `parent.col` and `child.col` distinctly, so `FieldPath`
  gains its qualifier **before** the relation family, not with it. Adding it
  afterwards means changing the shared grammar - the exact cost this document
  exists to avoid.
- **Byte-identical SQL is the determinism contract, and it has a price.**
  PostgreSQL identifies queries by a structural jumble, Calcite by `RelDigest`,
  sqlglot and Ecto by a separate cheap fingerprint generator; none targets text.
  The cost accepted here is that enum declaration order becomes coupled to the
  cache key. If that coupling starts to bite, the alternative is two generators -
  a cheap canonical fingerprint for identity and an executable renderer free to
  emit what suits the engine.
- **There is no general expression node, and specifically no expression-valued
  `ORDER BY`.** This is the restriction that produced `Arel.sql` elsewhere, and
  it collides with the shipped vector and spatial search, which order by a
  computed scalar. `ProjectionSource::SearchScalar` is the ordering-visible slot
  that keeps the restriction affordable. `write::Arithmetic` is the other narrow
  exception, confined to the SET/VALUES position because
  `"version" = "version" + 1` is emitted on every dispatched update; it is a
  closed shape, not a general expression node.

---

## Open

1. **Wire the IR into the shipped path, or delete it.** BUILDABLE. Today
   `zeroship-data-query-builder` is a `[dev-dependencies]` entry of
   `zeroship-plugin-db` and no shipped module references it, so the crate's
   6,282 lines are exercised only by its own tests and by
   `crates/zeroship-plugin-db/tests/search_ir_live.rs`. Three register items are
   closed **on paper** by pointing at it and are not closed in the tree: L31
   (`updateMany`'s unbounded second branch, which `RowLimit` makes
   unrepresentable), L16's prepare-once half (a stable plan shape is the
   precondition for a named prepared statement), and L29 (the `sqlite_`
   reservation on the wrong identifier role, which the IR fences on both). The
   first family to wire is `search`, whose IR lowering already exists and whose
   two production call sites are
   `crates/zeroship-data-postgres/src/postgres.rs` and
   `crates/zeroship-data-sqlite/src/{vector.rs,spatial.rs}`. Estimate: 8 hours
   for the search family end to end, on the basis that the lowering is written,
   the live test target exists, and the change is a dependency move plus two call
   sites; not measured for the other families.
2. **`ProjectionSource::MaskedSibling` encodes the pre-flip storage layout and
   must be deleted.** BUILDABLE. The IR ships `MaskedSibling { parent, sibling }`,
   `ProjectedField::masked` and `Ident::masked_sibling_of`, which derive
   `<col>_masked` and render `"ssn_masked" AS "ssn"`. That column no longer
   exists: `mask_sibling_column_for_field` occurs **zero** times in `crates/`, and
   the shipped layout is the inverse - `ssn` holds the mask, `__zs_raw__ssn` holds
   the real value. A plan built through `ProjectedField::masked` today renders SQL
   against a column no migration creates. Delete the variant and its two
   constructors; the `Prefix("__zs_")` column fence already keeps the raw column
   unnameable. Estimate: 4 hours, on the basis that it touches
   `crates/zeroship-data-query-builder/src/{projection.rs,ident.rs}` plus its
   `render/postgres.rs` and their tests, in a crate with no external callers.
3. **There is no SQLite renderer.** BUILDABLE.
   `crates/zeroship-data-query-builder/src/render/` holds `mod.rs` and
   `postgres.rs` only, so the acceptance arm "every plan family has parity
   fixtures producing equivalent resolved results on both backends" cannot pass
   and the "set of nodes SQLite refuses" contract has no producer. Not estimated:
   the refusal set has to be decided before it can be rendered, which is item 4.
4. **Which nodes does SQLite refuse?** NEEDS-DECISION. The refuse-not-emulate
   rule makes the refusal set a contract that belongs in
   `docs/reference/sqlite-divergences.md`, and nothing has enumerated it. The
   decision is per node, not global, and it gates item 3.
5. **The retirement condition for `zeroship-schema` is unmeasurable.** The
   condition is `unported == 0`, and the ledger that would count `unported` does
   not exist: `grep -rn source_symbol tests/ crates/` returns nothing. Until the
   ledger and its set-equality arm are built, "the port is complete" is an
   unfalsifiable claim. BUILDABLE for the arm itself, not estimated: the arm is
   cheap, but the ledger's initial rows are 87 hand-classified destinations.
6. **The effects family and the relation family's live-query lowering depend on
   the CDC relay, which does not exist.** NEEDS-DECISION on sequencing. `ls
   crates/ | grep -i cdc` is empty, and the relay's transport foundation is
   declared unvalidated by its own author. The shared core, read, write, search
   and unmask families have no transport dependency and are buildable now.
7. **`FieldPath` has no qualifier.** BUILDABLE, and it must land before the
   relation family rather than with it. Not estimated: the shape is one field on
   `FieldPath` plus every render site, but the render sites multiply with item 3.

---

## History

Deliberation for the whole set lives in
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md` and the review
snapshots under `docs/reviews/dbbind-2026-08-26/`. Decisions are in
`docs/proposals/2026-08-26-runtime-db-binding-decision-log.md`; the class of
cannot-fail acceptance arm is tracked in
`docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`.

Every constraint that still binds is stated above, in the specification. These
four notes are the ones that only make sense as history - each records a route
already taken and abandoned:

- **Do not re-derive `<col>_masked` sibling names.** That layout shipped and was
  flipped: the WHERE builder takes no schema hint and so could not substitute,
  which made `find({ ssn: { $gt: ... } })` compare against plaintext and let
  repeated probes binary-search a masked value with no authorization check and no
  audit row. The IR's `MaskedSibling` node is the last surviving piece of it (see
  Open 2).
- **Do not re-adopt "render the same plan twice and assert the SQL matches" as
  the determinism arm.** It was specified that way, and it is probabilistic
  rather than discriminating;
  `crates/zeroship-data-query-builder/src/render/mod.rs` records the
  supersession in the code.
- **Do not re-open full-text search as a plan family.** It was deleted (L11)
  because it had no producer anywhere in the tree.
- **Do not reach for `zeroship-schema` when the IR needs a validator.** It was
  the obvious dependency and was refused: it declares `compio-postgres`, which
  would put a live PostgreSQL driver inside a crate whose value is needing none.
  The fences are re-stated in
  `crates/zeroship-data-query-builder/src/ident.rs` instead, and the
  duplication is retired by the port rather than maintained.
