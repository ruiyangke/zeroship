# SC-3: the `DbPlan` IR and its source ledger

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`.

**Read the set from:**
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md`. Live defects cited
here by number (L11, L12, L16) live in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`.

**Transport dependency, narrowly.** The shared normative core and the read,
write, search and unmask families have no transport dependency and are buildable
now. Two surfaces do depend on L12's CDC service: the **effects** family, whose
node shape follows the transport a committed mutation publishes through, and the
relation family's **live-query** lowering, because `read_set.rs` is
relation-unaware (see "Live queries over relations must be refused", below).

**Gates:** step 10 of the parent document (porting the plan families), the
randomized encryption atomicity that needs plan-level variants, and the
retirement of `zeroship-schema`, which cannot complete while any ledger entry is
unported.

---

## Why this is a document and not a discovery

An IR discovered incrementally is shaped by whichever call site is ported first.

This document carries three things, and the distinction between them is
load-bearing: the **measured scope** (what must be ported, counted), the
**normative core** (the identifier, expression, projection and literal shapes
every family shares - fixed here so the first port cannot fix them by accident),
and the **mechanism** that drives the ledger to zero.

What it deliberately does **not** contain is the per-family node spelling - the
exact read/write/search/unmask plan structs and their lowering signatures. Those
are left to each family's port, because they are separable in a way the shared
core is not: a family that gets its own node shape wrong costs that family a
revision, whereas a shared expression node fixed wrongly by whoever ports first
costs every family after it.

## The scope, measured

`crates/zeroship-schema/src/query.rs` carries three regions before its inline
test modules (which begin at `:572`, `:5504`, `:5509` and `:6034`). They do not
all have the same destination, and that is the finding that makes this
tractable:

| Region | Entry points | Destination |
| --- | --- | --- |
| DDL / schema rendering | **27** | **Not `DbPlan`.** These render `CREATE TABLE`, indexes, FK add/drop, and the encryption / mask sentinel comments. The runtime executes no DDL, so they belong to the migration and schema side or die with the crate |
| Runtime query builders | **48** | **`DbPlan` nodes.** This is the port |
| Preamble | **11** | Classify individually - identifier validation, quoting and shared helpers, some of which belong in `shared/identifier.rs` |

So "specify an IR for 6,000 lines" is really **48 runtime entry points to port,
27 DDL entry points to dispose of, and 11 to classify**. Line ranges are
deliberately omitted: this document cites a file that the same work keeps
editing, and ranges here drifted within a single session. The shape is the
durable part; the ledger (below) derives the membership at run time.

**How the count is taken is part of the contract.** Counting `^pub fn` anchored
at column zero is **blind to indented methods** - it misses
`SqlDialect::binary_bind_placeholder` and `::wrap_binary_bind_param`
(`query.rs:179`, `:189`), both runtime-side. The gate counts `pub` /
`pub(crate)` items at any indentation, and its own arm must be proved against a
fixture containing an indented method, or it will reproduce exactly this blind
spot on the next addition. A ledger whose instrument cannot see part of its
source is worse than no ledger, because it reports completeness it never
measured.

Two of the preamble entries are security-critical and are **a pair, not a single
guardian**: `validate_collection`'s reserved-prefix check (`pg_`,
`__zero_migrate`, and `__zeroship`) fences *table* names, and a separate
reservation list (`RESERVED_NAMES` - `ReservedName::Prefix("__zeroship_")`,
`"__zs_"`, `"sqlite_"`, `"_"`, plus the `_masked` sibling suffix and the
classification names) fences *column* names. Moving one without the other is
the more dangerous half of a bulk move, because the survivor makes the
namespace look defended. Both move to `shared/identifier.rs`, together, each
with its own gate arm and its own vectors.

## The IR, at the altitude that matters

`DbPlan` is an identifier-validated, backend-neutral operation tree. What it
must carry is fixed by what the 48 builders do; the exact node spelling is
implementation. The families are:

- **read**: projection (caller-narrowable, always unioned with the required
  platform fields, never `SELECT *`), filter, ordering, pagination, aggregate
  and distinct;
- **relation**: a join carrying kind, target collection, on-condition, its own
  nested projection and its own filter, producing a nested result. Lowerable
  either as a SQL join or as the batched second read used today, within the
  boundary fixed below;
- **write**: insert, update, upsert with an explicit conflict target, delete,
  each carrying its returning shape;
- **search**: vector and spatial - **and only those two.** Full-text search was
  deleted (L11); the IR must not carry a family for a feature that no longer
  exists. Both surviving kinds read declared schema **inside** the concrete
  backends (`backend/postgres.rs:494-527`, `:701-724`, `:826-855`, and the
  SQLite equivalents), which is why they cannot be ported as a wrapper;
- **unmask reads**, which carry an authorization decision rather than producing
  it;
- **effects**: the publication a committed mutation owes the broker. A bare rows
  result cannot express it, and committed-change publication already differs by
  backend today (`exec.rs:442-499`).

**One part of the grammar is pinned here, not discovered.** The families above
are separable and can each be shaped by their own port. The **expression
sub-grammar** underneath them cannot: filters, projections, ordering keys and
search predicates all compose the same comparison, logical, path and literal
nodes, so whichever family ports first would otherwise fix that shape for every
family after it - this document's own failure mode, applied one level down.

## The normative core

These types are contract. The families above fix *what* a plan carries; these
fix the shapes that every family shares.

### Identifiers - the type that makes SQL text unrepresentable

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

The role matters because the fences differ - a **table** name is fenced by the
`validate_collection` prefix list, a **column** name by `RESERVED_NAMES`. They
are the pair named above, and both must reach `shared/identifier.rs` together.

Identifiers are quoted unconditionally at render time. Quoting conditional on a
reserved-word list is not sound: the equivalent list is empty for PostgreSQL in
at least one widely used SQL library, so soundness would rest entirely on
whoever set the "needs quoting" flag upstream.

### The expression sub-grammar

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

Three of these encode defects this project actually hit, which is why they are
types rather than conventions:

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
  element type and the cardinality cap (`MAX_MEMBERSHIP_LIST_LEN`,
  `query.rs:606`), so all three bounds travel with the type instead of being
  re-checked at each call site. The lower bound is not covered by `And([])` /
  `Or([])` - that is a different node - and without it a `Membership` over an
  empty set renders the syntactically invalid `IN ()`. Empty membership is
  therefore not representable as a `Membership` at all: it simplifies to
  `Const(false)` for `In` and `Const(true)` for `NotIn` at construction.

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
sort invokes them. So `canonical()`, the renderer, the aggregate walk **and**
the derived comparisons all recurse; rewriting the hand-written traversals
iteratively covers three of four and leaves the derive. Only a construction-time
bound covers all of them. `MAX_PREDICATE_DEPTH = 16` mirrors
`MAX_FILTER_NESTING_DEPTH = 16` (`query.rs:604`, enforced at `:5666`), the bound
the translator being replaced already applies - the recurring failure this
project makes is leaving a bound at the old call site instead of moving it with
the operation. The depth check itself is **iterative**: a recursive depth check
overflows on exactly the input it exists to reject.

A node-count cap is not a substitute and cannot see a 43-deep tree.

### Projection

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
    /// `<col>_masked AS <col>`. The parent column is NOT read.
    MaskedSibling { parent: Ident, sibling: Ident },
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

The other three parts each answer a question the family list leaves open:

- **`MaskedSibling` makes the mask substitution a NODE**, not a string rewrite
  applied late. This is what closes the projection leak described below by
  construction: narrowing selects `ProjectedField`s, and a masked column's field
  *is* the sibling variant, so there is no representable path that emits a bare
  `"ssn"` for a masked column. Its own note - "the parent column is NOT read" -
  is also what keeps key resolution off the default read path.
- **`Exposure` answers whether the unioned platform fields are visible to the
  creator.** `Platform` fields are added by the planner and **stripped before
  user code** unless the declared schema names them too. Without this the union
  rule is ambiguous in a way that shows up as either a leaked `deleted_at` or a
  missing one.
- **`alias` is a first-class slot**, which is what the relation family needs:
  mandatory aliasing for cross-collection column collisions has somewhere to
  live rather than being a rendering trick.

### What the types cannot enforce

Some invariants are not expressible in the shapes above and therefore need
property tests, named rather than assumed: an aggregate operand is legal only in
a `HAVING` position; a plan's parameter count must equal the placeholders its
lowering emits; and a backend's refusal must be a typed error from its own
module rather than an empty result.

## Plan shape must render deterministically, or the statement cache misses

This is a property of rendering **any** plan, not of any one family or boundary
decision.

The driver has prepared-statement caching
(`Client::new_with_statement_cache`, `statement_cache_execution_threshold`;
`libs/compio-postgres/src/{bind.rs:163,prepare.rs:312-356}`) - **and it is
switched off.** `statement_cache_capacity` defaults to **0**
(`libs/compio-postgres/src/config.rs:833`) and `plugin-db` never calls
`prepare_cached`, so every operation today sends an unnamed statement that
PostgreSQL parses and plans from scratch, every time. Turning it on is a
per-operation saving that needs no IR at all. (L16 in the defect register
measures the same switched-off cache as a constant factor on every warm
operation; the overlap is deliberate, and the four citations above are the ones
a reader needs in order to act.)

The IR makes that cache far more valuable: values are always parameters, so two
calls differing only in their arguments produce **identical SQL text** and reuse
one prepared plan.

That benefit is contingent on a property nothing else states: **rendering a
given plan shape must be byte-stable.** If a lowering iterates a `HashMap` to
emit a projection list or an `AND` chain, column order varies between runs, the
SQL text varies with it, and every call is a cache miss - a silent performance
regression that no correctness test would notice. Renderers therefore emit from
ordered collections.

**The arm that checks it is not the obvious one.** Rendering the same plan twice
and asserting the SQL matches does **not** test canonicalization: rendering one
in-memory unordered map twice can preserve *that instance's* iteration order, so
a non-canonical implementation passes. Freshly randomizing the maps is worse - it
turns the arm into a coin flip that fails at random and gets re-run until green.
The arm is **two semantically identical plans built by opposite insertion
permutations**, asserted against ONE canonical SQL/parameter fixture, plus a
mutation that deletes the canonical sort and proves the arm turns red.

## Three boundary decisions, made here so they are not made by default

Each of these is a one-line change away from being decided accidentally by
whoever ports first.

### 1. Projection narrowing and joins are first-class, and the IR carries both

A join planner is not a security concern on this path: every table in a join
lives in the *same app's schema* and identifiers are fenced to that namespace,
so a join cannot cross a tenant boundary. What remains is a **cost** concern,
which belongs to limits and metering.

Today the builder emits **zero** SQL joins (every `join` occurrence in
`query.rs` is Rust's `Vec::join` on strings) and relations are served by a
batched second read: collect parents, deduplicate foreign keys, one
`WHERE id IN (..)` per relation, stitch in memory (`docs/reference/db.md:580-633`).
That stays valid as a *lowering strategy*. It stops being the only one.

**Projection first, because it is smaller and independently valuable.** The
relation load "always fetches every column of the target" (`db.md:640-644`),
which is wasteful and, more to the point, drags masked and encrypted columns
nobody asked for through the mask pass. `Projection` is already a node; what is
added is caller-supplied narrowing, under one rule enforced in the constructor
rather than remembered:

> A narrowed projection is always **unioned with the required platform fields**.
> Narrowing can request fewer *declared* fields; it can never drop a column the
> system needs to function.

The required set is `SYSTEM_FIELD_NAMES` - `id`, `created_at`, `updated_at`,
`created_by`, `updated_by`, `version`, `deleted_at` (`query.rs:723-731`).

**`id` is load-bearing in four independent ways**, worth enumerating because no
single one of them would be found by testing the others:

1. the **relation stitch** matches children to parents by it, so dropping it
   breaks joins rather than returning less;
2. the **unmask handle** plucks `row_pk` from it, and a row without one gets an
   empty string that `unmask()` then rejects;
3. **encrypted columns** bind it into the AEAD tag via `canonical_aad`, so
   without it the ciphertext is undecryptable;
4. **change events** correlate on it - `emit_for_rows` reads `row["id"]` with an
   `_id` fallback (`crates/zeroship-plugin-db/src/exec.rs:507-511`), so a
   projection that dropped the PK emits an event with no key and a subscriber
   that cannot match it to anything.

A narrowing implementation could plausibly be written, reviewed and shipped
against any one of those without the other three ever being exercised.

**And a union is not sufficient, because masked columns are SUBSTITUTED rather
than added.** The read path does not select a masked column and its sibling
together; it replaces the column with the sibling, aliased back to the original
name:

```sql
SELECT "id", "ssn_masked" AS "ssn", "email_masked" AS "email", "name"
```

(`query.rs:2949-2971`, and the unmask variant at `:3002-3020`), and the mask
pass depends on exactly that - "the SELECT clause already aliased
`<col>_masked AS <col>`, so `row[col]` holds the masked string and no
`<col>_masked` key" (`crud/mask_pass.rs:381-384`).

So the obvious implementation of narrowing - filter the final column list down
to what the caller asked for - **emits a bare `"ssn"` and returns plaintext.**
Not fewer columns than intended: the *wrong* value, silently, on precisely the
columns marked as needing protection. Projection narrowing must therefore run
**before** the mask substitution and feed it, never after it, and the acceptance
arm has to assert the rendered SQL of a narrowed projection over a masked column
rather than the count of columns returned.

**Then joins.** The node carries kind (inner / left), the target collection, the
on-condition, and its own nested projection - and produces the **nested** result
shape the current API already returns (`todo.userId -> { .. }`), so the creator
surface does not change under anyone. Aliasing becomes mandatory rather than
optional, because column names collide across joined collections;
`IdentRole::Alias` exists for exactly this. Relation-level filters fall out once
the node exists, closing the second documented v1 limitation (`db.md:645-648`).

Nested joins need a bound, and it is **a cap on the TOTAL number of relation
nodes in a plan, not on nesting depth**. A savepoint-style depth cap guards
*runtime recursion* - a chain that deepens as code executes - whereas a plan is
a **value**, fully known before anything runs, and its relations **branch**
rather than chain. Five sibling relations at depth one cost the planner what
five joins cost; a depth-only cap would wave them through.

Eight is the number, and it is not arbitrary: PostgreSQL's own
`join_collapse_limit` and `from_collapse_limit` are both **8** on the tier we
target, with `geqo_threshold` at **12** (measured on the project's PG 16
container). Past that boundary the planner stops exhaustively reordering joins
and eventually switches to a genetic optimiser, so a plan carrying more than
eight relations has left the region where its cost is predictable. Borrowing the
engine's own threshold means the limit tracks the reason it exists.

#### The strategy is chosen by relation CARDINALITY, not by backend preference

"The backend chooses how" is too loose, and the industry has already paid for
that looseness: one `JOIN` per to-many include multiplies parent rows by the
product of the children, so two collection includes over a modest page return an
enormous result for a small amount of data. Entity Framework had to retrofit an
explicit split-query mode; Prisma made it a per-query knob with a global default
rather than an engine decision.

What makes this tractable here is that **every relation today is to-one.** The
`with` key must be a `t.ref(...)` field declared on the *parent*
(`docs/reference/db.md:624`), i.e. a foreign key, and no reverse or to-many
relation exists in the surface. For to-one relations a join is safe and strictly
better than the batched read - one round trip, no row multiplication.

So the rule is:

- **to-one: join.** One round trip, no duplication, and the batched read remains
  a legal lowering for a backend that prefers it.
- **to-many (when it arrives): never a naive join.** Either split queries - the
  batched read this system already implements - or a lateral subquery
  aggregating children into JSON, which gives nesting in one round trip with no
  parent duplication. PostgreSQL has `json_agg`; SQLite has `json_group_array`,
  and whether their behaviour matches closely enough for parity is a question to
  settle before relying on it, not after.

The strategy therefore belongs in the plan as an explicit property with a
cardinality-derived default, not as a backend's private choice. A creator
hitting a pathological plan should be able to see which strategy was used.

**"The plan says what, the backend chooses how" is FALSE for three plan
shapes**, where the two lowerings are observably different rather than merely
differently costly:

- **Pagination with an inner-kind relation.** `isDone` is computed from the
  *parent* rows before any relation load - `rows.length <= numItems` on the raw
  parent result - and the cursor is deliberately computed "BEFORE relation
  loading" so the captured `orderBy` value is the raw column rather than an
  overwritten joined object (`sdks/db/src/query.ts:370-384`). A fused inner join
  drops childless parents **before** `LIMIT`, so it returns a different page, a
  different `isDone`, and a different cursor. Same plan, two answers.
- **Ordering by a child column.** The cursor is
  `{ orderBy, lastValues, lastId }` (`sdks/db/src/query.ts:34-48`) - the
  *parent's* ordering values and the parent's id, with no slot for a child's. A
  fused join can order by a child column; the batched strategy cannot even
  express the resume point. The cursor format, not the SQL, is the limit.
- **A filtered left join**, where the child predicate belongs in `ON` under the
  fused lowering and in the child's own `WHERE` under the batched one. Placed
  wrongly it silently converts the left join to an inner one, dropping parents
  rather than nulling children.

So the strategy is free to vary **only** for plans that neither paginate across
a relation nor order by a child column. Beyond that boundary the plan must pin
the strategy, and a plan requesting a shape its pinned strategy cannot serve is
**refused with a typed error** rather than served differently. Cursor resumption
across a strategy change is not supported: a cursor minted under one lowering is
not portable to the other.

#### Live queries over relations must be refused until read-set tracking understands them

A relation plan cannot be handed to the live-query path as it stands, and the
failure is silent rather than loud - the subscription simply never fires.

`read_set.rs` contains the string `relation` **once**, in a comment, and is
otherwise entirely relation-unaware. Two consequences, both verified:

- **A conjunct on a child column never matches.** When a column is absent from
  the change event's tuple, `Conjunct::matches_text` returns non-match, on the
  documented reasoning that "the subscriber won't see this event but it'll see
  the next one that DOES carry the column"
  (`crates/zeroship-plugin-db/src/read_set.rs:118-127`). That is correct for a
  TOASTed value, which does arrive later. It is **never** true for a child
  column, because a parent's change event will not carry it in any future event
  either. A relation filter recorded as an ordinary conjunct therefore
  suppresses delivery permanently.
- **A change to a child row produces no parent event at all**, so a live query
  whose result depends on joined data is never invalidated when that data
  changes. Whether the lowering fuses the join or splits it, the subscription is
  registered against the parent collection.

Neither is a lowering bug that a better renderer fixes; the read-set model has
no vocabulary for "this result depends on rows in another collection". Until it
does, **a plan carrying a relation is refused on the live-query path with a
typed error**, rather than accepted and silently starved. A subscription that
never fires is indistinguishable, from the creator's side, from a table that
never changes.

**The real work is not the SQL.** It is that the mask and unmask passes become
**per-source-collection**. Today a result row comes from one collection, so
policy lookup is trivial and implicit. A joined row carries columns from several
collections with different mask policies and different classifications, and a
single per-result policy lookup would silently apply the parent's policy to the
child's columns. That is the failure mode to design against, and it belongs to
SC-6's contract rather than to this document's grammar.

### 2. A backend that cannot serve a node REFUSES; it does not emulate

The alternative is real - some SQL builders emulate missing dialect features -
and it is rejected here. Refusal keeps the two tiers honestly different rather
than plausibly similar, which matters because the dev tier is SQLite and the
production tier is not. The consequence must be owned rather than discovered:
**the set of nodes SQLite refuses is itself a contract**, and it belongs in
`docs/reference/sqlite-divergences.md` beside the divergences already recorded
there.

The stronger reason is that the failure mode of the other choice is silent. A
provider that quietly evaluates an untranslatable predicate outside the database
returns correct-looking rows having filtered them **after** they left the
tenant's boundary - a correctness and performance problem in an ordinary ORM, a
security failure here.

**The same refusal covers the escape hatch this platform will be tempted by.**
Convex - the closest positional competitor, also untrusted app code, also no SQL
surface - escapes into the *runtime*: its `.filter()` runs arbitrary TypeScript
over a document range, explicitly not using the index. That shape is the one
most naturally available here, since this platform also runs JS, and it is
exactly what this decision refuses. Recorded because a reviewer will otherwise
propose it as though it were unexamined.

### 3. `DbPlan` is in-process only and MUST NOT derive `Serialize` or `Deserialize`

The moment a plan crosses a process boundary it is a wire format, and a wire
format needs versioning, compatibility rules and a migration story - the exact
burden this document exists to avoid. Deriving `Serialize` for a debug dump or a
test fixture is a one-line change that silently creates that obligation. If a
plan must be inspected, render it through a `Debug`/display path that is
explicitly not a format anyone may parse.

`Deserialize` is worse than symmetric. Deserialising an AST is an
arbitrary-construction surface: sqlglot's `Expression.load()` calls `__import__`
on a dotted name taken **straight from the JSON payload** (verified
empirically). An IR whose producer is creator code cannot afford a
reconstruct-from-bytes path that bypasses every constructor invariant above.

### Two constraints that are contract, not spelling

1. **Values are parameters, identifiers are validated and quoted.** The plan
   never carries a fragment of SQL text. A "safe raw" escape hatch is not an
   acceptable substitute: Prisma's tagged-template version is bypassable in one
   line of JavaScript (`stringsArray.raw = [query]`), documented and unfixed.
2. **A backend that cannot serve a node returns a typed error from its own
   module.** No capability is discovered by a dialect match in the frontend.

## Costs and open risks, accepted deliberately

These are the decisions most likely to be second-guessed later. They are
recorded so that a future reader knows they were taken with the cost in view.

1. **No general expression node, and specifically an expression-free
   `ORDER BY`.** This is the restriction that produced `Arel.sql` elsewhere, and
   here it collides with the *shipped* vector and spatial search, which order by
   a computed scalar. The search family's port must give that scalar an
   ordering-visible slot (`ProjectionSource::SearchScalar` is where it lives) or
   the restriction has to be revisited before the family lands.
2. **Byte-identical SQL as the determinism contract.** PostgreSQL identifies
   queries by a structural jumble, Calcite by `RelDigest`, sqlglot and Ecto by a
   separate cheap fingerprint generator; none of them targets text. The cost
   accepted here is that enum declaration order becomes coupled to the cache key.
   The alternative shape - two generators, a cheap canonical fingerprint for
   identity and an executable renderer free to emit what suits the engine - is
   the one to reach for if that coupling starts to bite.
3. **Roles are validated but not carried, and there is no qualified column
   reference.** The second half is a hard dependency, not a preference: the
   relation family cannot land without one, because a joined plan must be able to
   name `parent.col` and `child.col` distinctly. Adding it after a family has
   shipped means changing the shared grammar - the exact cost this document
   exists to avoid - so `FieldPath` must gain its qualifier before the relation
   family, not with it.

## The ledger

A checked file, not prose. One row per source entry point:

```text
source_symbol | source_range | destination | status
```

`destination` is one of: a named `DbPlan` node, `shared/*`, the migration side,
or `deleted-with-<feature>`. `status` is `ported` or `unported`.

The mechanism is the point:

- a **gate arm** asserts the ledger's source column equals the set of
  `pub`/`pub(crate)` functions the crate actually exposes, so a new entry point
  cannot appear without a row;
- **that set equality is the assertion, and the count is not.** The arm reports
  its ruled-on count as diagnostic output. It neither pins that count to a
  literal nor accepts a non-zero count as the property. A non-zero floor is
  degenerate - one row clears it, and an arm a single row satisfies cannot
  distinguish an exhaustive ledger from an abandoned one. A literal expected
  count is the hand-maintained census this repository forbids: the arm compares
  two sets it derives at run time, and a mismatch in either direction fails;
- **`zeroship-schema` is retired when `unported` reaches zero AND every plan
  family has landed** - two conditions, because the first does not imply the
  second. Dropping the dependency early would leave the port invisible.

  The relation family is exactly the case that breaks a ledger-only rule: the
  string `relation` occurs **once** in `query.rs`, in a comment, and the batched
  loader lives in TypeScript (`sdks/db/src/collection/relations.ts`). So the
  family has **no source row to port**, `unported` reaches zero without it, and
  a retirement gated on the ledger alone would fire with a whole family unbuilt
  - reporting completion of a port that never happened.

  This is the ledger's own blind spot restated: it enumerates what
  `zeroship-schema` exports, so anything that was never in `zeroship-schema` is
  invisible to it however exhaustive it is over what it does cover. The families
  are therefore a **separate checklist** with its own arm, not a derived
  consequence of the row count.

This is the same discipline the repository already applies to gates: a count
that comes from the thing being measured, not from a table someone maintains by
hand.

## Sequencing

The 48 runtime entry points do **not** land as one merge. They land per family -
read, write, search, unmask - each with the parity fixtures for both backends,
and each reducing `unported` by a countable amount. A family is done when its
rows are `ported` and the contract suite passes against both factories.

The 27 DDL entry points are a **separate disposition question** and must not be
smuggled into the port: the runtime executes no DDL, so none of them becomes a
plan node. They move to the migration side or are deleted with the feature they
serve, and each gets a row either way.

## Acceptance shape

- The ledger exists and **the gate asserts set equality** between its source
  column and the crate's actual `pub`/`pub(crate)` exports, failing on a
  difference in either direction. It reports its ruled-on count as well, but the
  count is diagnostic output, not the assertion.
- No `DbPlan` node ships without a ledger row naming its source.
- Every plan family has parity fixtures producing equivalent resolved results on
  both backends, after documented dialect lowerings.
- No plan carries SQL text; a mutation attempting it fails a source gate.

  **This has the cannot-fail shape flagged at the end of this list.** The gate is
  a negative check over plan types that do not all exist yet, so it matches
  nothing and reports success, and would keep doing so if the port were
  abandoned. It must first assert that the plan types it scans exist.
- **A narrowed projection still contains every required platform field**, on
  every family that projects, asserted against a projection that asks for one
  declared column. This is the arm that catches narrowing implemented as a
  filter over the caller's list.
- **A narrowed projection over a MASKED column still renders
  `"<col>_masked" AS "<col>"`**, asserted on the rendered SQL, not on the shape
  of the returned row. A row-shape assertion passes while the value is
  plaintext, because the column arrives under the name the caller asked for
  either way - which is the whole failure.
- **A predicate deeper than `MAX_PREDICATE_DEPTH` is refused at construction**,
  with the check itself iterative so it does not overflow on the input it exists
  to reject, and with the derived `Ord`/`Hash` recursion covered by the same
  bound.
- **A joined row's mask policy is resolved per source collection.** This arm
  lives in **SC-6's acceptance shape**, which owns per-source-collection mask
  policy. It is named here because SC-3's relation node is what makes the case
  reachable, and the two must not drift apart. Its discriminating form, which
  SC-6 carries: a parent and child whose policies **differ on the same
  classification** - the case a single per-result lookup gets wrong while every
  same-policy fixture passes.
- **A relation plan submitted to the live-query path is refused with a typed
  error**, asserted by subscribing and observing the refusal - not by observing
  that no event arrives, which is exactly what the broken behaviour also looks
  like.
- **Two semantically identical plans built by OPPOSITE insertion permutations
  render to one canonical SQL/parameter fixture**, so the driver's
  prepared-statement cache can hit - with a mutation that deletes the canonical
  sort and proves this arm turns red. A rendering that iterates an unordered map
  passes every correctness test and misses the cache on every call. Asserting
  instead that the same plan renders twice identically is **not** this arm and
  does not test canonicalization.
- **A plan that paginates across a relation, or orders by a child column, PINS
  its strategy** - and a plan asking for a shape its pinned strategy cannot
  serve is refused with a typed error. Asserted on the three divergent shapes
  named above, since a parity arm run only on plans where the strategies agree
  is a tautology.
- **Both lowerings of a relation load produce equivalent resolved results**,
  **on plans within the agreed boundary**: the batched two-read strategy and the
  join strategy must agree on rows, nesting, and null handling for a missing or
  null foreign key (`db.md:622-633` already fixes that behaviour and it must
  survive the change of strategy).
- **`DbPlan` and its node types do not implement `Serialize` or `Deserialize`**,
  asserted by a source gate rather than by review - the derive is one line and
  its cost is a versioned wire format plus a reconstruct-from-bytes path around
  every constructor invariant.

  **This has the cannot-fail shape too**, for the same reason: a gate looking for
  a derive on types that do not exist yet scans an empty set and passes. It must
  first assert that the types it rules on exist.
- **Every node a backend refuses is listed** in
  `docs/reference/sqlite-divergences.md`, and the arm compares the documented set
  against the set the backend actually rejects. A refusal that is real but
  undocumented is the same surprise as an emulation.
- An unsupported capability surfaces as a typed error from the concrete backend,
  with no dialect match above the neutral backend boundary.

  **That arm must first assert the boundary file exists.** `backend/api.rs` does
  not exist today - `crates/zeroship-plugin-db/src/backend/` holds `mod.rs`,
  `postgres.rs`, `sqlite/` and `lock_guard.rs` - and neither does the
  `DbBackend` trait the parent lists among the neutral types to introduce. A
  negative grep scoped to a path that was never created **matches nothing and
  reports success**, so this arm passes today, passes if the refactor is
  abandoned, and passes if the file is deleted later.

  The cannot-fail shape recurs across this list - it also covers the
  no-SQL-text arm and the `Serialize` arm above - and the class is tracked in
  `docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`, which is
  where a census of it belongs if one is ever built.
- `zeroship-schema` is removed from `zeroship-plugin-db`'s manifest **only** when
  `unported` is zero, and the reserved-prefix pair has landed in
  `shared/identifier.rs` with its own arm.
