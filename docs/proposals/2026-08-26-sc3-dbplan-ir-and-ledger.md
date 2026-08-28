# SC-3: the `DbPlan` IR and its source ledger

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`. **Blocked on L12**,
the replication-slot ceiling: the index records that finalising SC-3 and
starting the IR implementation both wait on it, because the subscription
transport this document's live-query decisions sit on is exactly what L12 puts
in question. Until now this document said so nowhere.

**Read the set from:**
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md`. Live defects cited
here by number (L12, L16) live in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`.

**Gates:** step 10 of that document (porting the plan families), the randomized
encryption atomicity that needs plan-level variants, and the retirement of
`zeroship-schema`, which cannot complete while any entry below is unported.

---

## Why this is a document and not a discovery

An IR discovered incrementally is shaped by whichever call site is ported first.
The parent proposal's v2 claimed the grammar "is specified in full" while
containing no grammar; that claim was withdrawn.

This document now carries three things, and the distinction between them is
worth keeping: the **measured scope** (what must be ported, counted), the
**normative core** (the identifier, expression, projection and literal shapes
every family shares - fixed here so the first port cannot fix them by
accident), and the **mechanism** that drives the ledger to zero.

What it still does **not** contain is the per-family node spelling - the exact
read/write/search/unmask plan structs and their lowering signatures. Those are
deliberately left to each family's port, because they are separable in a way the
shared core is not: a family that gets its own node shape wrong costs that
family a revision, whereas a shared expression node fixed wrongly by whoever
ports first costs every family after it. That is the whole argument for the
split, and it is why "not a finished grammar" is no longer the right description
while "not a finished IR" still is.

## The scope, measured

`crates/zeroship-schema/src/query.rs` is **11,821 lines** (it was 11,645 when
this was first measured; the difference is regression tests added by this
session's own fixes - a reminder that a line count is a reading, not a fact),
with inline test
modules from `:572` and `:5504`. It exposes **86** `pub` / `pub(crate)`
functions. They do not all have the same destination, and that is the finding
that makes this tractable:

| Half | Range | Entry points | Destination |
| --- | --- | --- | --- |
| DDL / schema rendering | `:1019-2905` | **27** | **Not `DbPlan`.** These render `CREATE TABLE`, indexes, FK add/drop, and the encryption / mask sentinel comments. The runtime executes no DDL, so they belong to the migration and schema side or die with the crate |
| Runtime query builders | `:2907-6011` | **48** | **`DbPlan` nodes.** This is the port |
| Preamble | `< :1019` | **11** | Classify individually - identifier validation, quoting and shared helpers, some of which belong in `shared/identifier.rs` |

So "specify an IR for 6,000 lines" is really **48 runtime entry points to port,
27 DDL entry points to dispose of, and 11 to classify**. That is a tractable
ledger, and it is why this was worth measuring before designing.

**How the count is taken is part of the contract**, because the first attempt
got it wrong in an instructive way. Counting `^pub fn` anchored at column zero
returns 84 and is **blind to indented methods** - it missed
`SqlDialect::binary_bind_placeholder` and `::wrap_binary_bind_param`
(`query.rs:180`, `:190`), both runtime-side. The gate counts `pub` items at any
indentation, and its own arm must be proved against a fixture containing an
indented method, or it will reproduce exactly this blind spot on the next
addition. A ledger whose instrument cannot see part of its source is worse than
no ledger, because it reports completeness it never measured.

Two of the 11 are security-critical and must not be lost in the shuffle, and
they are **a pair, not a single guardian**: `validate_collection`'s
reserved-`__zeroship` prefix check (`query.rs:648-652`) fences *table* names,
and a second reservation list 90 lines away (`query.rs:740-748`,
`ReservedName::Prefix("__zeroship_")`, `"__zs_"`, `"sqlite_"`, plus the
`_masked` sibling suffix) fences *column* names. An earlier draft called the
first the "sole guardian", which would have moved one fence and left the other
behind - the more dangerous half of a bulk move, because the survivor makes the
namespace look defended.

Both move to `shared/identifier.rs`, together, each with its own gate arm and
its own vectors.

## The IR, at the altitude that matters

`DbPlan` is an identifier-validated, backend-neutral operation tree. What it
must carry is fixed by what the 48 builders do; the exact node spelling is
implementation. The families are:

- **read**: projection (caller-narrowable, always unioned with the required
  platform fields, never `SELECT *`), filter, ordering, pagination, aggregate
  and distinct;
- **relation**: a join carrying kind, target collection, on-condition, its own
  nested projection and its own filter, producing a nested result. Lowerable
  either as a SQL join or as the batched second read used today - the plan says
  *what*, the backend chooses *how*;
- **write**: insert, update, upsert with an explicit conflict target, delete,
  each carrying its returning shape;
- **search**: vector and spatial - which today read declared schema **inside**
  the concrete backends (`backend/postgres.rs:494-527`, `:701-724`, `:826-855`,
  and the SQLite equivalents), which is why they cannot be ported as a wrapper.
  *This read "vector, full-text, spatial - the three" until 2026-08-27. Full-text
  search was DELETED (L11, decided by the operator and implemented on
  `feat/db-delete-fts`: `backend/sqlite/fts.rs` and
  `zeroship-schema/src/fts_sqlite.rs` both removed). The IR must not carry a
  family for a feature that no longer exists - and this document went on
  describing one for a day after the deletion landed, because nothing re-reads a
  list once it is written;*
- **unmask reads**, which carry an authorization decision rather than producing
  it;
- **effects**: the publication a committed mutation owes the broker. A bare rows
  result cannot express it, and committed-change publication already differs by
  backend today (`exec.rs:442-499`).

**One part of the grammar must be pinned here, not discovered.** The families
above are separable and can each be shaped by their own port. The **expression
sub-grammar** underneath them cannot: filters, projections, ordering keys and
search predicates all compose the same comparison, logical, path and literal
nodes, so whichever family ports first will otherwise fix that shape for every
family after it - which is this document's own stated failure mode, applied one
level down.

So the expression nodes are agreed before the first family lands: comparison
operators, logical composition, field paths (including nested access), literal
values with their logical type, and the null-handling rule that makes
`IS NULL` distinct from a null-valued comparison. That last one is not
academic - the parent proposal's decode fix exists because a value silently
becoming null changed an operator.

**And the builder got it wrong until this session - which is the strongest
argument for pinning it, precisely because a fix is not a guarantee.**
`value_to_param_inner` maps `Value::Null` to `String::new()` - an empty string -
and its own comment concedes the case "should not be used as param (use IS
NULL)". The `$in` arm pushed **every** array element through that conversion
with no null check, so `{ f: { $in: [null] } }` compiled to `f IN ($1)` with
`$1 = ''`: on a text column that silently matched rows whose value is the empty
string and missed every row that is actually NULL. Wrong results, no error, no
diagnostic.

That is **fixed** - the membership arms now partition nulls out and emit
`IS NULL` / `IS NOT NULL`, with tests
(`fix(schema): treat a null $in/$nin member as IS NULL, not an empty string`).
The sibling defect, `$in: []` emitting the syntactically invalid `IN ()`, is
fixed too.

**Keeping the story in past tense matters, and an earlier draft of this section
asserted the bug as live after it had been repaired.** The argument is not
"the code is broken"; it is that the *shape* permitted it. The knowledge that
null is special was present all along - in a comment, one function away from the
call site that ignored it. A comment is not a type, and a fix is not a type
either: nothing stops the next author from adding a third membership operator
that reaches for `value_to_param` again. The IR's null rule must therefore be a
**node distinction** (`IsNull` is not `Compare`), so the broken form cannot be
expressed rather than merely being currently absent.

Deliberately no line citations in this paragraph. The ones it carried drifted
within a single session - `value_to_param_inner` moved when the fix landed -
because this document cites a file the same work keeps editing.

## The normative core

These types are contract. The families above fix *what* a plan carries; these
fix the shapes that every family shares, and they are stated here because a
family that ports first would otherwise fix them for all the others.

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
a validated-newtype guarantee is lost. The role matters because the fences
differ - a **table** name is fenced by the `__zeroship` prefix list
(`query.rs:648-652`), a **column** name by `__zeroship_` / `__zs_` / `sqlite_`
plus the `_masked` sibling suffix (`query.rs:740-748`). They are a pair; moving
one without the other is the dangerous half.

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

- **`IsNull` is a separate node** and `Compare` forbids null operands, so the
  `$in`/`$nin` bug - a null silently bound as the empty string - becomes
  unrepresentable rather than merely discouraged.
- **`And([])` renders `TRUE` and `Or([])` renders `FALSE`**, stated so the
  renderer does not invent a convention.
- **`LiteralSet` is NON-EMPTY by construction** and enforces a homogeneous
  element type and the cardinality cap (`MAX_MEMBERSHIP_LIST_LEN`,
  `query.rs:604`), so both bounds travel with the type instead of being
  re-checked at each call site.

  **The lower bound is the part an earlier draft omitted, and without it this
  section's own claim was half wrong.** `LiteralSet` had only an upper bound, so
  an empty set was constructible and `Membership` over it still rendered
  `IN ()` - the exact PostgreSQL syntax error this project fixed hours ago.
  `And([])`/`Or([])` does not cover that case: it is a different node. Empty
  membership is therefore not representable as a `Membership` at all - it
  simplifies to `Const(false)` for `In` and `Const(true)` for `NotIn` at
  construction.

  Worth naming the pattern rather than just the fix: I claimed a grammar made a
  class of defect unrepresentable, and it made **one of the two**
  unrepresentable while the other stayed expressible. "The types prevent it" is
  exactly the kind of claim that stops people checking, so it has to be true
  node by node.

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

**No `Star` variant** makes "never `SELECT *`" a property of the type. The other
three parts each answer a question this document previously left open, and an
earlier draft of this section dropped all three:

- **`MaskedSibling` makes the mask substitution a NODE**, not a string rewrite
  applied late. This is what closes the projection leak described above by
  construction: narrowing selects `ProjectedField`s, and a masked column's field
  *is* the sibling variant, so there is no representable path that emits a bare
  `"ssn"` for a masked column. Its own note - "the parent column is NOT read" -
  is also what keeps key resolution off the default read path.
- **`Exposure` answers whether the unioned platform fields are visible to the
  creator**, which the union rule above states but does not resolve. `Platform`
  fields are added by the planner and **stripped before user code** unless the
  declared schema names them too. Without this the rule is ambiguous in a way
  that shows up as either a leaked `deleted_at` or a missing one.
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
decision, which is why it sits beside the normative core rather than inside the
relation section where it was first written.

The driver has prepared-statement caching
(`Client::new_with_statement_cache`, `statement_cache_execution_threshold`;
`libs/compio-postgres/src/{bind.rs:163,prepare.rs:312-356}`) - **and it is
switched off.** `statement_cache_capacity` defaults to **0**
(`libs/compio-postgres/src/config.rs:833`) and `plugin-db` never calls
`prepare_cached`, so every operation today sends an unnamed statement that
PostgreSQL parses and plans from scratch, every time.

An earlier draft of this paragraph said the driver "already caches prepared
statements", which read as *this is working and the IR improves it*. The
machinery exists; the cache does not run. That is a materially different
starting point - and a much larger prize, since turning it on is a
per-operation saving that needs no IR at all.

**This overlaps L16 in the defect register, and the overlap is accepted rather
than resolved.** L16 measures the same switched-off cache as one of the
constant factors on every warm operation. The copy here is kept because it
carries four citations L16 does not - `Client::new_with_statement_cache`,
`statement_cache_execution_threshold`, `bind.rs:163`, `prepare.rs:312-356` -
and because the self-correction above corrects *this document's* earlier draft
rather than the register's. A self-correction stays with the draft it corrects.

The IR makes that cache far more valuable: values are always parameters, so two
calls differing only in their arguments produce **identical SQL text** and reuse
one prepared plan.

That benefit is contingent on a property nothing currently states: **rendering a
given plan shape must be byte-stable.** If a lowering iterates a `HashMap` to
emit a projection list or an `AND` chain, column order varies between runs, the
SQL text varies with it, and every call is a cache miss - a silent performance
regression that no correctness test would notice. Renderers therefore emit from
ordered collections.

**The arm that checks it is not the obvious one.** Rendering the same plan twice
and asserting the SQL matches does not test canonicalization. The arm is **two
semantically identical plans built by opposite insertion permutations**,
asserted against ONE canonical SQL/parameter fixture, plus a mutation that
deletes the canonical sort and proves the arm turns red. The acceptance shape
carries it in that form; the superseded form, and why it fails, is below.

### Superseded specification of the determinism arm

The round-7 reviewer recorded this on 2026-08-27 as its required cannot-fail
finding. The correction has been applied, above and in the acceptance shape.
The finding is kept rather than deleted, because deleting it erases the
correction instead of recording it - and because this document shipped the arm
it opens by rejecting for as long as the finding sat unapplied in the front
matter:

> **A determinism arm here is probabilistic, not discriminating.**
>
> An arm that renders the same plan twice and asserts the SQL matches does not
> test canonicalization. Rendering one in-memory unordered map twice can
> preserve *that instance's* iteration order, so a **non-canonical
> implementation passes**. Making the maps freshly randomized does not fix it -
> it turns the arm into a coin flip, which is worse than a weak test because it
> fails at random and gets re-run until green.
>
> **Replace it with:** two semantically identical plans built by **opposite
> insertion permutations**, asserted against ONE canonical SQL/parameter
> fixture - plus a mutation that deletes the canonical sort and proves the arm
> turns red.
>
> This matters more here than elsewhere because plan canonicalization is what a
> prepared-statement cache key rests on (see L16 in the defect register). A plan
> that renders two different SQL strings for the same logical operation silently
> halves the cache hit rate, and no test would say so.

## Three boundary decisions, made here so they are not made by default

Each of these is a one-line change away from being decided accidentally by
whoever ports first.

### 1. Projection narrowing and joins are first-class, and the IR carries both

An earlier draft of this section argued for staying join-free. **That is
withdrawn, and two of its arguments were wrong.** It claimed a join planner on a
"multi-tenant path" was a security simplification: it is not, because every
table in a join lives in the *same app's schema* and identifiers are fenced to
that namespace, so a join cannot cross a tenant boundary. What remains is a
**cost** concern, which belongs to limits and metering. It also claimed
join-free keeps both backends' lowerings the same shape - weak, since PostgreSQL
and SQLite both do standard joins perfectly well.

Today the builder emits **zero** SQL joins (all 58 `join` occurrences in
`query.rs` are Rust's `Vec::join` on strings) and relations are served by a
batched second read: collect parents, deduplicate foreign keys, one
`WHERE id IN (..)` per relation, stitch in memory
(`docs/reference/db.md:532-590`). That stays valid as a *lowering strategy*. It
stops being the only one.

**Projection first, because it is smaller and independently valuable.** The
relation load "always fetches every column of the target"
(`db.md:592-605`), which is wasteful and, more to the point, drags masked and
encrypted columns nobody asked for through the mask pass. `Projection` is
already a node; what is added is caller-supplied narrowing, under one rule that
must be enforced in the constructor rather than remembered:

> A narrowed projection is always **unioned with the required platform fields**.
> Narrowing can request fewer *declared* fields; it can never drop a column the
> system needs to function.

The required set is `SYSTEM_FIELD_NAMES` - `id`, `created_at`, `updated_at`,
`created_by`, `updated_by`, `version`, `deleted_at` (`query.rs:721-729`).

**`id` is load-bearing in four independent ways**, which is worth enumerating
because no single one of them would be found by testing the others:

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

(`query.rs:2917`, `:3000-3010`), and the mask pass depends on exactly that -
"the SELECT clause already aliased `<col>_masked AS <col>`, so `row[col]` holds
the masked string and no `<col>_masked` key" (`crud/mask_pass.rs:382-383`).

So the obvious implementation of narrowing - filter the final column list down
to what the caller asked for - **emits a bare `"ssn"` and returns plaintext.**
Not fewer columns than intended: the *wrong* value, silently, on precisely the
columns that are marked as needing protection. Projection narrowing must
therefore run **before** the mask substitution and feed it, never after it, and
the arm below has to assert the rendered SQL of a narrowed projection over a
masked column rather than the count of columns returned.

**Then joins.** The node carries kind (inner / left), the target collection, the
on-condition, and its own nested projection - and produces the **nested** result
shape the current API already returns (`todo.userId -> { .. }`), so the creator
surface does not change under anyone. Aliasing becomes mandatory rather than
optional, because column names collide across joined collections;
`IdentRole::Alias` exists for exactly this. Relation-level filters fall out once
the node exists, closing the second documented v1 limitation.

Nested joins need a bound, and it is **a cap on the TOTAL number of relation
nodes in a plan, not on nesting depth**. An earlier draft reached for the
savepoint cap as the analogy; that was the wrong shape. A savepoint cap guards
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
that looseness. Entity Framework had to add an explicit split-query mode after
users hit **cartesian explosion**: one `JOIN` per to-many include multiplies
parent rows by the product of the children, so two collection includes over a
modest page can return an enormous result for a small amount of data. Prisma
made the same choice a per-query knob with a global default rather than an
engine decision.

What makes this tractable here is a fact worth stating explicitly: **every
relation today is to-one.** The `with` key must be a `t.ref(...)` field declared
on the *parent* (`docs/reference/db.md:577`), i.e. a foreign key, and no reverse
or to-many relation exists in the surface. For to-one relations a join is safe
and strictly better than the batched read - one round trip, no row
multiplication.

So the rule is:

- **to-one: join.** One round trip, no duplication, and the batched read remains
  a legal lowering for a backend that prefers it.
- **to-many (when it arrives): never a naive join.** Either split queries - the
  batched read this system already implements, which is exactly EF's answer - or
  a lateral subquery aggregating children into JSON, which is Drizzle's answer
  and gives nesting in one round trip with no parent duplication. PostgreSQL has
  `json_agg`; SQLite has `json_group_array`, and whether their behaviour matches
  closely enough for parity is a question to settle before relying on it, not
  after.

The strategy therefore belongs in the plan as an explicit property with a
cardinality-derived default, not as a backend's private choice. A creator
hitting a pathological plan should be able to see which strategy was used.

**And "the plan says what, the backend chooses how" is FALSE for some plans -
a claim this document made and has to withdraw.** Three shapes make the two
lowerings observably different, not merely differently costly:

- **Pagination with an inner-kind relation.** `isDone` is computed from the
  *parent* rows before any relation load - `rows.length <= numItems` on the raw
  parent result - and the code deliberately notes that the cursor "is computed
  BEFORE relation loading" so the captured `orderBy` value is the raw column
  rather than an overwritten joined object
  (`sdks/db/src/query.ts:345-356`). A fused inner join drops childless parents
  **before** `LIMIT`, so it returns a different page, a different `isDone`, and
  a different cursor. Same plan, two answers.
- **Ordering by a child column.** The cursor is
  `{ orderBy, lastValue, lastId }` (`query.ts:36-38`) - **one** value and
  **one** id, with no slot for a child's. A fused join can order by a child
  column; the batched strategy cannot even express the resume point. The cursor
  format, not the SQL, is the limit.
- **A filtered left join**, where the child predicate belongs in `ON` under the
  fused lowering and in the child's own `WHERE` under the batched one. Placed
  wrongly it silently converts the left join to an inner one, dropping parents
  rather than nulling children.

So the strategy is free to vary **only** for plans that neither paginate across
a relation nor order by a child column. Beyond that boundary the plan must pin
the strategy, and a plan requesting a shape its pinned strategy cannot serve is
**refused with a typed error** rather than served differently. Cursor
resumption across a strategy change is not supported and must be stated: a
cursor minted under one lowering is not portable to the other.

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
  column, because a parent's change event will not carry it in any future
  event either. A relation filter recorded as an ordinary conjunct therefore
  suppresses delivery permanently.
- **A change to a child row produces no parent event at all**, so a live query
  whose result depends on joined data is never invalidated when that data
  changes. Whether the lowering fuses the join or splits it, the subscription is
  registered against the parent collection.

Neither is a lowering bug that a better renderer fixes; the read-set model has
no vocabulary for "this result depends on rows in another collection". Until it
does, **a plan carrying a relation is refused on the live-query path with a
typed error**, rather than accepted and silently starved. Refusing is the honest
option and matches the refuse-rather-than-emulate rule, which is boundary
decision 2 below rather than anything above this point: a subscription that
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

The alternative is real - jOOQ emulates missing dialect features - and it is
rejected here. Refusal keeps the two tiers honestly different rather than
plausibly similar, which matters because the dev tier is SQLite and the
production tier is not. The consequence must be owned rather than discovered:
**the set of nodes SQLite refuses is itself a contract**, and it belongs in
`docs/reference/sqlite-divergences.md` beside the divergences already recorded
there.

The stronger reason is that the failure mode of the other choice is silent. A
provider that quietly evaluates an untranslatable predicate outside the database
returns correct-looking rows having filtered them **after** they left the
tenant's boundary - a correctness and performance problem in an ordinary ORM, a
security failure here.

### 3. `DbPlan` is in-process only and MUST NOT derive `Serialize`

The moment a plan crosses a process boundary it is a wire format, and a wire
format needs versioning, compatibility rules and a migration story - the exact
burden this document exists to avoid. Deriving `Serialize` for a debug dump or a
test fixture is a one-line change that silently creates that obligation. If a
plan must be inspected, render it through a `Debug`/display path that is
explicitly not a format anyone may parse.

### Two constraints that are contract, not spelling

These close the section; neither is a property of decision 3, under whose bold
lead-in they used to render.

1. **Values are parameters, identifiers are validated and quoted.** The plan
   never carries a fragment of SQL text.
2. **A backend that cannot serve a node returns a typed error from its own
   module.** No capability is discovered by a dialect match in the frontend.

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
  literal nor accepts a non-zero count as the property.

  An earlier draft of this bullet said the arm "reports its ruled-on count and
  asserts it is non-zero", which the acceptance shape below then rejected in
  its own words: a non-zero floor is degenerate, one row clears it, and an arm
  a single row satisfies cannot distinguish an exhaustive ledger from an
  abandoned one. The two sites now state the same property.

  An earlier draft said the count "must equal 86 today", which is precisely the
  hard-coded census this repository forbids - and which this same session
  watched fail: `PLUGIN_DB_MIN_PASSED=118` in
  `tests/run_plugin_db_live_suite.sh` now exceeds the maximum the tree can
  produce (108 measured), so that gate cannot go green on any code. A number
  maintained by hand beside the thing it measures goes stale exactly this way.
  The arm compares two sets it derives at run time - the ledger's source column
  and the crate's actual exports - and a mismatch in either direction fails. 86
  is what that comparison yields today, not what it is told to expect;
- **`zeroship-schema` is retired when `unported` reaches zero AND every plan
  family has landed** - two conditions, because the first does not imply the
  second. Dropping the dependency early would leave the port invisible.

  The relation family is exactly the case that breaks a ledger-only rule: the
  string `relation` occurs **once** in `query.rs`, in a comment, and the batched
  loader lives in TypeScript (`sdks/db/src/collection/relations.ts`). So the
  family has **no source row to port**, `unported` reaches zero without it, and
  a retirement gated on the ledger alone would fire with a whole family
  unbuilt - reporting completion of a port that never happened.

  This is the ledger's own blind spot restated: it enumerates what
  `zeroship-schema` exports, so anything that was never in `zeroship-schema` is
  invisible to it however exhaustive it is over what it does cover. The
  families are therefore a **separate checklist** with its own arm, not a
  derived consequence of the row count.

This is deliberately the same discipline the repository already applies to
gates: a count that comes from the thing being measured, not from a table
someone maintains by hand.

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
  difference in either direction. It reports its ruled-on count as well, but
  the count is diagnostic output, not the assertion.

  An earlier draft of this arm, and the ledger bullet it mirrors, asked only
  that the count be reported and non-zero. "Reports a non-zero count" is a
  degenerate floor - one row clears it - and an arm a single row satisfies
  cannot distinguish an exhaustive ledger from an abandoned one. Both sites now
  state set equality.
- No `DbPlan` node ships without a ledger row naming its source.
- Every plan family has parity fixtures producing equivalent resolved results on
  both backends, after documented dialect lowerings.
- No plan carries SQL text; a mutation attempting it fails a source gate.

  **This shares the cannot-fail shape flagged at the end of this list.** The
  gate is a negative check over plan types that do not exist yet, so it matches
  nothing and reports success today, and would keep doing so if the port were
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
- **A joined row's mask policy is resolved per source collection.** This arm
  now lives in **SC-6's acceptance shape**, which owns per-source-collection
  mask policy - a division this document states in its own words under "Live
  queries over relations must be refused": the real work "belongs to SC-6's
  contract rather than to this document's grammar". It is named here because
  SC-3's relation node is what makes the case reachable, and the two must not
  drift apart. Its discriminating form, which SC-6 carries: a parent and child
  whose policies **differ on the same classification** - the case a single
  per-result lookup gets wrong while every same-policy fixture passes.
- **A relation plan submitted to the live-query path is refused with a typed
  error**, asserted by subscribing and observing the refusal - not by observing
  that no event arrives, which is exactly what the broken behaviour also looks
  like.
- **Two semantically identical plans built by OPPOSITE insertion permutations
  render to one canonical SQL/parameter fixture**, so the driver's
  prepared-statement cache can hit - with a mutation that deletes the canonical
  sort and proves this arm turns red. A rendering that iterates an unordered map
  passes every correctness test and misses the cache on every call.

  Asserting instead that the same plan renders twice identically is **not** this
  arm and does not test canonicalization: rendering one in-memory unordered map
  twice can preserve that instance's iteration order, so a non-canonical
  implementation passes. That was this list's arm until the round-7 finding was
  applied; see "Plan shape must render deterministically".
- **A plan that paginates across a relation, or orders by a child column, PINS
  its strategy** - and a plan asking for a shape its pinned strategy cannot
  serve is refused with a typed error. Asserted on the three divergent shapes
  named above, since a parity arm run only on plans where the strategies agree
  is a tautology.
- **Both lowerings of a relation load produce equivalent resolved results**,
  **on plans within the agreed boundary**:
  the batched two-read strategy and the join strategy must agree on rows,
  nesting, and null handling for a missing or null foreign key
  (`db.md:576-590` already fixes that behaviour and it must survive the change
  of strategy).
- **`DbPlan` and its node types do not implement `Serialize`**, asserted by a
  source gate rather than by review - the derive is one line and its cost is a
  versioned wire format.

  **This shares the cannot-fail shape too**, for the same reason: `DbPlan` and
  its node types do not exist yet, so a gate looking for a `Serialize` derive on
  them scans an empty set and passes. It must first assert that the types it
  rules on exist.
- **Every node a backend refuses is listed** in
  `docs/reference/sqlite-divergences.md`, and the arm compares the documented
  set against the set the backend actually rejects. A refusal that is real but
  undocumented is the same surprise as an emulation.
- An unsupported capability surfaces as a typed error from the concrete backend,
  with no dialect match above the neutral backend boundary.

  **That arm must first assert the boundary file exists.** `backend/api.rs` does
  not exist today - `crates/zeroship-plugin-db/src/backend/` holds `mod.rs`,
  `postgres.rs`, `sqlite/` and `lock_guard.rs` - and neither does the
  `DbBackend` trait the parent lists among the neutral types to introduce. A
  negative grep scoped to a path that was never created **matches nothing and
  reports success**, so this arm passes today, passes if the refactor is
  abandoned, and passes if the file is deleted later. It is the cannot-fail
  shape.

  **It is not the only one, and the number is deliberately not stated.** Within
  this list the same shape covers the no-SQL-text arm and the `Serialize` arm,
  both flagged above, and the determinism arm carried it until the round-7
  finding was applied. An earlier draft said the shape "appeared three times
  across these documents", which was a hand-maintained census of a property
  nothing measures - the instrument this document rejects under "The ledger",
  for the ledger's own count. The class itself is tracked in
  `docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`, which
  is where a census of it belongs if one is ever built.
- `zeroship-schema` is removed from `zeroship-plugin-db`'s manifest **only**
  when `unported` is zero, and the reserved-prefix check has landed in
  `shared/identifier.rs` with its own arm.

---

## Measured against the prior art (2026-08-28)

Compared against Prisma, Arel, sqlglot, Calcite, Spark, Diesel, Ecto, SeaQuery
and Convex. Figures below were measured by the reviewer against released
software, not recalled.

### The finding that is a REGRESSION, not a hardening request

**Depth is the denial-of-service vector. Size is not. This crate bounds the
wrong one.**

Binary-searched against sqlglot 30.17.0 on CPython 3.13 at the default
recursion limit: 43 nested function calls raise `RecursionError`; 49 nested
parens; 54 nested `CASE`; 108 chained `NOT`. A **100,000-element `IN` list of
589 KB parses fine in 2.33s**. So roughly a hundred bytes kills it and half a
megabyte does not.

`MAX_MEMBERSHIP_LIST_LEN` bounds the list. **Nothing bounds depth.**

Three things make it serious:

1. **Rust has no recursion limit**, so a deep tree is a stack overflow and an
   abort - and the worker runs many apps per thread under LRU isolate eviction,
   so that is the process and every co-tenanted app, not one failed request.
2. **It cannot be fixed in the traversal.** `Predicate` derives `Ord` and
   `Hash`, which recurse structurally, and the canonical path's `flat.sort()`
   invokes them. So `canonical()`, `write_predicate()`, `mentions_aggregate()`
   **and** the derived comparisons all recurse. Only a bound at CONSTRUCTION
   covers all four.
3. **The code this IR replaces already carries the bound**:
   `MAX_FILTER_NESTING_DEPTH = 16` (`zeroship-schema/src/query.rs:604`, enforced
   at `:5666`). The IR dropped it. That is the project's own recurring failure -
   the bound stayed at the old call site instead of moving with the operation.

**Precedent that this is easy to get wrong:** sqlglot was offered both bounds in
one PR; `max_depth` was **rejected** and only `max_nodes` shipped, defaulting to
disabled. A node-count cap cannot see a 43-deep tree.

**Not live** - the IR has no callers yet.

### Where this design beats the prior art, specifically

- **No null literal.** Diesel's `eq(None)` silently matches nothing
  (diesel#1306). Nullness as a node makes that unrepresentable.
- **A projection slot that cannot hold a string.** That slot was
  Sequelize CVE-2023-22578, CVSS 10.0.
- **No `Deserialize`** - and the justification is stronger than the
  wire-format one this document gives. sqlglot's `Expression.load()` calls
  `__import__` on a dotted name taken **straight from the JSON payload**
  (verified empirically). Deserialising an AST is an arbitrary-import surface.
- **Unconditional identifier quoting.** sqlglot's `RESERVED_KEYWORDS` is
  **empty for Postgres**, and `identify=False` is the default, so its identifier
  soundness rests entirely on the parser having set `quoted=True` upstream.
- **Constructor-enforced invariants.** sqlglot's `arg_types` is enforced only
  under `if UNITTEST:`; `exp.EQ(this=col)` with a required argument missing
  renders the malformed `'a ='` with no exception, and `Select` has 32 keys and
  **zero** required.

**The sentence worth keeping:** sqlglot can afford its raw nodes *because its
producer is a parser, not an API. An IR whose producer is creator code cannot.*

**And the strongest external validation of the no-raw-SQL thesis:** Prisma's
tagged-template "safe raw" API is bypassable in one line of JavaScript
(`stringsArray.raw = [query]`), documented by Prisma, unfixed for years.

### The three decisions most likely to be regretted

1. **No expression node, and specifically an expression-free `ORDER BY`.** This
   is the exact restriction that created `Arel.sql`, and it already blocks the
   shipped vector and spatial search.
2. **Byte-identical SQL as the determinism contract.** Postgres identifies
   queries by a structural jumble, Calcite by `RelDigest`, sqlglot and Ecto by a
   separate cheap fingerprint generator. **Nobody targets text.** The cost here
   is enum-declaration-order coupling becoming wire-visible. The better shape is
   two generators: a cheap canonical fingerprint for identity, and an executable
   renderer free to emit what suits the engine.
3. **Roles validated but not carried**, plus no qualified column reference - the
   latter means the relation family cannot land without breaking the shared
   grammar this crate exists to protect.

### An option declined rather than unconsidered

**Convex** - the closest positional competitor, also untrusted app code, also no
SQL surface - escapes into the **runtime**: its `.filter()` runs arbitrary
TypeScript over a document range, explicitly not using the index. That is a
fourth escape-hatch shape and the one most naturally available here, since this
platform also runs JS. It is precisely what decision 2 refuses, because it
filters after rows have left the tenant boundary. Recorded because a reviewer
will otherwise propose it as though it were unexamined.
