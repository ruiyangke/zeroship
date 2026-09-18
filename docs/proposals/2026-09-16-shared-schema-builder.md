# One schema builder: merge the `@zeroship/db` and `@zeroship/migrate` `t.*` surfaces into a shared leaf package

**Status:** PROPOSED — 2026-09-16 · **Origin:** `docs/reviews/2026-09-16-orm-api-review.md` BUG-1 (ORM API review)

Design review the same day settled the vocabulary table, the semantic forks
(temporal, protection, identity, numbers, validation, arrays, structured
types), the refusal matrix, the portability doctrine, and the agent-author
principles. One open question remains, collected at the end.

---

## Context

Creators author schema twice removed from themselves: migrations written against
`@zeroship/migrate`'s fluent `t.*` lexicon, and a generated
`generated/zeroship/env.db.ts` that re-expresses the same schema in
`@zeroship/db`'s *different* fluent `t.*` dialect so the `Db<typeof schema>`
type-inference chain can read it. The two dialects share concepts but not
spellings — and some collide on the same name outright:

| Concept | `@zeroship/migrate` | `@zeroship/db` |
| --- | --- | --- |
| Unbounded text | `t.text()` | `t.string()` |
| Bounded `VARCHAR(N)` | `t.string(opts)` (length defaults when omitted) | — (does not exist) |
| 32-bit integer | `t.int()` (`t.integer` deliberately removed) | `t.integer()` |
| Approximate float | `t.real()` (float4) / `t.double()` (float8) | `t.number()` (float8; no float4 exists) |
| Sub-int4 / half-precision | `t.smallInt()` / `t.real()` | — (the runtime lexicon never carried them) |
| Calendar date | `t.date()` | `t.calendarDate()`; the db reference doc states `t.date()` is not in the surface |
| Nullability | nullable by default, `.notNull()` | optional by default, `.required()` |
| Foreign key | `.references(table, column, opts)` on the column | `t.ref(table, { column, relation })` factory, or `.references(table, opts)` on a scalar builder |
| Text array | `t.textArray()` (native `text[]`) | `t.array(t.string())` (JSON storage; native only via the Rust `#[orm(array_storage = "native")]` attribute — a third spelling) |

The internal token vocabularies collide too: db's `FieldDef.type: "date"` means
*timestamptz*; migrate's `ColType: "date"` means *SQL DATE*.

Beneath the two public surfaces sits a third copy of the lexicon:
`packages/zero-migrate/src/db-types.ts` is a vendored, re-authored subset of
db's `TypeBuilder`/`FieldDef`, inlined — per its own header — so the standalone
migrate package carries no db dependency. The repo has already been burned by
this exact pattern: `docs/reviews/2026-08-28-migrate-dsl-fork-divergence.md`
records the outage caused by a forked DSL implementation recording into a
different singleton. The vendored type-builder subset is the same shape of
risk at the type-definition layer: a `FieldDef` facet added to db and not to
the vendored copy (or vice versa) fails, when it fails, at the fold — after
the creator has committed migrations.

Finally, the fold maintains an active translation between the two dialects on
every build. `generated/zeroship/env.db.ts` says so in its own header:

> The schema below is a reconstruction of `@zeroship/db` `t.*()` builder
> calls — NOT a hand-rolled interface — so it flows through the SAME
> `InferSchema`/`Row`/`Collections`/`Db`/`Id<>`/`MaskedValue<>` inference
> chain a declared schema would.

Every translation layer is a fidelity surface; the same header's caveat that
"fixed-precision numeric facets are preserved" exists because drift is a live
concern.

## Decision

**One schema builder, one vocabulary, one new leaf package.** A shared,
immutable `TypeBuilder` — carrying db's phantom brand generics and `FieldDef`
as its canonical internal representation, widened to migrate's full DDL
lexicon — lives in a new zero-dependency package, **`@zeroship/schema`**.
`@zeroship/db` re-exports it; `@zeroship/migrate` builds its column surface on
it and deletes the vendored subset. The fold emits a single dialect.

This is a merge, not a victory for either side:

- **From db:** the phantom brands (`T, R, M, E, D, F`) and the `FieldDef`
  record shape. The brands are the type-level program that
  `InferSchema`/`Row`/`RowInput`/`Filter`/`MaskedValue` read; they do not
  move. `FieldDef` is the representation the runtime descriptor already
  speaks, and the only one of the two that can represent structured types
  (`object`/`union`/`literal`/`array`), which the inference chain recurses
  into.
- **From migrate:** implementation discipline — clone-on-modify immutability,
  facets as disjoint fields with "absent means omitted on the wire" — and the
  DDL lexicon: `char`, `double`, `inet`, `uuid`, `enum`, `domain`, native
  array storage, plus the `collation`, `generated`, `identity`,
  `caseSensitive`, and `idPrefix` facets. (`smallInt`/`real` are *not*
  contributed — the runtime lexicon never carried them; the engine keeps the
  tokens for introspection only. See
  [Numbers: the semantic family](#numbers--the-semantic-family).)
- **From neither:** the package home. Rationale in
  [Package topology](#package-topology).

### Why db's representation is canonical and migrate's is derived

The dependency arrow must run `FieldDef → ColType`, not the reverse:

1. `FieldDef` is the language of `schema.runtime.json` and the Rust descriptor
   validation in `crates/zeroship-data-orm`. The runtime authority speaks it.
2. `ColType` is provably derivable: `colTypeFromDbField` in
   `packages/zero-migrate/src/db-lexicon.ts` is exactly that derivation. It is
   lossy today — it throws `UnsupportedColTypeError` for the structured
   `TypeName` members — *because* `FieldDef` is narrower than `ColType`, not
   because the direction is wrong. Widening `FieldDef` to the full lexicon lets
   the bridge become total. It does NOT move into the leaf: it returns migrate's
   own `ColType`, so relocating it would make the two packages reference each
   other. It stays in `@zeroship/migrate` and is completed alongside the token
   rename.
3. Inverting the arrow makes the DDL lexicon canonical, and then the
   structured types have no home — migrate refuses them by design, but the
   type-inference chain needs their structure.

### The merge direction fixes a latent aliasing bug

db's `TypeBuilder` mutates `this._def` and returns `this`
(`required()`, `min()`, `max()`, … in `packages/db/src/types.ts`):

```ts
const base = t.string();
const a = base.required();   // sets base._def.required = true
const b = base.max(10);      // b is ALSO required at runtime; its type says optional
```

Runtime/type divergence on builder reuse. The generated code never aliases —
every field chains off a fresh factory call — so the bug is latent, but
hand-written shared schemas can hit it. migrate's `ColumnDefImpl` already
clones via `with()`; the shared builder adopts that discipline, and the
aliasing bug dies with the merge.

## Vocabulary unification

One spelling per concept. Renames land with every caller updated in the same
change; no aliases, no deprecation shims (pre-launch rule).

| Concept | Winning spelling | Losing spelling | Notes |
| --- | --- | --- | --- |
| Unbounded text | `t.string()` | `t.text()` | TS-honest: creators are TS developers, and the generated dialect already emits `t.string()`. |
| Bounded varchar | `t.string({ length: N })` | `t.string(opts)` default-length form | One factory, optional bound. `length` is **required** in the options bag — the silent default length in today's migrate lexicon dies. |
| 32-bit integer | `t.int()` | `t.integer()` | Matches the `bigInt` family rhythm; migrate already made this call. |
| Integer widths | `t.int()` / `t.bigInt()` only | `t.smallInt()` | The runtime lexicon (int4/int8/float8/numeric) is the arbiter — `smallInt` never crossed the V8 boundary, so keeping it would *widen the runtime*, not unify spelling. The engine keeps the `ColType` token for introspection/drift only. Full rationale in [Numbers: the semantic family](#numbers--the-semantic-family). |
| Approximate real | `t.double()` | `t.number()`, `t.real()` | Precision-honest naming: `t.double()` makes the approximation visible at the call site, while `t.number()` read as "the default number" — which is how money ends up in floats. `t.real()` (float4) dies with `smallInt`. |
| Calendar date | `t.calendarDate()` | `t.date()` | `t.date()` in a TS ecosystem reads as "JS Date", i.e. a timestamp. |
| Nullability | `.required()` | `.notNull()` | Optional-by-default stays the default; the generated dialect already spells it `.required()`. |
| Internal timestamp token | `FieldDef.type: "timestamp"` | `"date"` (meaning timestamptz) | The SQL-DATE token is `"calendarDate"`. Token rename sweeps the fold, the descriptor decoder, and the Rust `TypeName` mapping together. **Retired-spelling posture, decided explicitly:** the tolerant READERS (`render/fold.rs`, `schema/decode.rs`, `gen-types/render-env-db.ts`, and one `codecs.rs` test list) still accept `"date"` while the fail-closed paths (`render/declarative.rs`, `gen-types/manual.ts`, `validate.ts`) reject it. That asymmetry is deliberate rather than accidental - already-written descriptors stay readable - and it is a **removal obligation**: drop the alias arms once no descriptor in the wild spells `"date"`. The three backend type maps carry an explicit arm so the retired spelling can never fall into the silent `TEXT`/`VARCHAR(191)` fallback in the meantime. |
| Array column | `t.array(item, { storage? })` — one factory, storage as a facet | `t.textArray()` | `textArray` bakes a vendor spelling (`text[]`) into the contract name; the storage choice is a physical facet, not a type. Default storage is `"json"`; `{ storage: "native" }` renders `text[]` on PG and is emulated faithfully elsewhere. Full rationale in [Arrays: contract vs storage](#arrays--contract-vs-storage). |
| Foreign key | `.references(table, column, opts)` on any scalar builder — `column` is **required** — plus `t.ref(table, { relation? })` as the narrowed constructor for the platform's dominant pattern | today's `t.ref(table, opts)` with its optional `{ column }` | `t.ref` is NOT sugar: it targets the target's typed-id `id` primary key, always. The `{ column }` option is removed from it, so anything nonstandard — a non-`id` target, non-text storage — is forced into the explicit method form where a reviewer can see it. The two forms never overlap; the target shape forces the choice. (Convex's `v.id("users")` is the same move: the identity reference is the constructor, not an abbreviation.) Both lower to the same facet. |
| Primary key | `.primaryKey()` implies required | db's non-implying form | Aligns with migrate's current behavior. Decided in review: the fold **stops emitting** the redundant `.required()` on `id` columns — generated code should be minimal. |
| Typed identity | `t.typedId(prefix)` | `t.id(prefix?)` (db), `ids.typeId({ prefix })` (migrate) | One factory spelling the platform's own term (typed_id: UUIDv7 + base36 + entity prefix). Lowers to the `idPrefix` facet; assignment metadata derived at descriptor emission. See [Value formats and identity](#value-formats-and-identity--one-ttypedidprefix-spelling). |
| Mask | db's closed-set validation (`MaskKind`, `Classification`) | migrate's `{ kind: string; classification: string }` carrier | The closed sets are the runtime's contract; authoring should not accept what the runtime refuses. |
| Vector dims | db's authoring-time range check against pgvector's dimension ceiling | migrate accepting any positive integer | Same reasoning: fail at authoring, not at index build. |
| Encryption | `.encrypted()` chain method on the plaintext builder | `t.encrypted({ of })` wrapper | Protection is a verb, not a wrapper. Kills the hidden string default, the inner-facet shredding, and the `{ of }` bag; the plaintext type is always explicit. See [Protection facets](#protection-facets--encrypted-and-mask). |

Naming doctrine for the whole table (settled in review, applies to every
future naming decision): **each name has exactly one meaning, each meaning has
exactly one name, and a wrong guess fails in a way that names the right one.**
Factories are nouns (`t.ref`, `t.string`, `t.int` — what the field *is*);
chain methods are verbs (`.references()`, `.required()`, `.mask()` — what it
does or constrains). No word appears in both positions.

Two companion rules, both exercised by the encrypted redesign: **a type
constructor takes its constituent as a receiver or a plain positional
argument** (`t.array(t.string())`, `t.union(a, b)`), never as an `{ of: ... }`
options key — options bags are for facets (`length`, `storage`, `metric`,
`dimensions`); and **declaration order never carries semantics** — chain calls
write disjoint facets, and defaults resolve once at seal time (see
[Protection facets](#protection-facets--encrypted-and-mask)).

## Semantic forks and their resolutions

These are the places where the two dialects are not just spelled differently
but *mean* different things. Each needs a designed answer, not a pick.

### `.default()` — server-side vs client-side

db's `.default()` accepts a scalar **or a factory function evaluated in the
SDK at insert**. migrate's `.default()` accepts scalars, value constructors
(`now()`, `uuidV7()`, `nextval(...)`), and expression ASTs rendered as **SQL
`DEFAULT`**. One name, two evaluation points.

Resolution: **`.default(value)` is always the database default.** Scalars,
constructors, and expressions lower to SQL `DEFAULT`. SDK-evaluated defaults
get their own spelling: **`.clientDefault(fn)`**. The scalar case behaves
identically under either evaluation point, so most creator code is untouched;
only factory-function call sites change spelling.

### Protection facets — `.encrypted()` and `.mask()`

Today encryption is a wrapper factory (`t.encrypted({ of: t.string() })`)
while masking — its sibling protection — is a chain method (`.mask(...)`).
Two postures for one concern family, and the wrapper has three structural
defects:

1. **A hidden default**: `t.encrypted()` with no argument means string
   plaintext.
2. **Facet shredding**: the wrapper builds a fresh def from the inner
   builder's `type`/`precision`/`scale` only, silently dropping `.mask()`,
   `.required()`, `.min()`, and anything else declared on the inner builder
   (`packages/db/src/types.ts`, the `encrypted` factory).
3. **A type/runtime mismatch**: the generic constraint admits `bigint`
   plaintext while the runtime check refuses it with
   `ENCRYPTED_TYPE_UNSUPPORTED`. It compiles, then throws at schema build.

Resolution: **protection is a verb.** `.encrypted()` is a chain method on the
plaintext builder; the wrapper and its `{ of }` bag die:

```ts
t.string().encrypted()
t.string().encrypted().mask({ kind: "last4", classification: "pci" })
t.bigInt().encrypted()
t.bytes().encrypted()
```

Rules:

- **Independent axes.** `.encrypted()` is the storage side (AEAD ciphertext,
  fresh nonce per write, bound to app/collection/column/row); `.mask()` is
  the read side (pre-computed mask in the visible column, raw value in a
  hidden sibling, audited unmask). All four combinations are meaningful:
  plain; mask-only (plaintext sibling, masked reads — no at-rest protection);
  encrypt-only via `.mask({ kind: "none" })` (ciphertext at rest, plaintext
  reads); both (the default when `.encrypted()` stands alone). A mask sibling
  column exists iff a mask is declared, independent of encryption.
- **Seal-time defaults; declaration order carries no semantics.**
  `.encrypted()` sets `encrypted: true` and nothing else. At seal time
  (`toFieldDef()`), one rule runs: `encrypted && mask === undefined → apply
  { kind: "full", classification: "pii" }`. An explicit `.mask()` wins from
  either chain position. General builder law: chain calls write disjoint
  facets, defaults resolve once at seal from the final facet set, and if two
  orders of the same calls produce different descriptors the builder is buggy
  by definition.
- **Encryptable plaintext: `string`, `number`, `bigInt`, `bytes`.** `bigInt`
  is admitted and gains its plaintext codec in the same change (today the
  types admit it and the runtime refuses it). Everything else is refused at
  authoring, each for its own reason — the gate is three tests, not "is there
  a codec": the value must round-trip losslessly; it must still mean something
  when its only operations are write and read-back-after-unmask; and it must
  require no database-level invariant that needs plaintext. Refs fail the
  third test — FK enforcement compares plaintext, and randomized ciphertext
  never matches — which is the existing rule. Vector/geo fail the second: the
  type exists to be searched, and an encrypted value cannot be. `json` /
  `object` / `union` pass the round-trip test but are withheld by granularity
  doctrine: sensitive data belongs in named columns with individual masks and
  per-field audit, not inside an encrypted blob the protection model cannot
  see. Temporal types pass all three and are excluded only for capacity — a
  codec away, not a redesign away. Arrays fall to the bare-item rule in
  [Arrays](#arrays--contract-vs-storage). The set widens when the runtime
  grows a codec — builder and codec land in the same change.
- **migrate's form converges.** `t.encrypted({ of: ColumnDef | ColType })` —
  including the loose-token escape hatch — is replaced by the facet. The IR
  carries it as `IrColumn.encrypted: Option<bool>` beside the plaintext `type`,
  not as an `{ encrypted: { of } }` wrapper.

Companion work (not the merge): a **blind index** facet
(`.encrypted().blindIndex()` — a platform-derived HMAC sibling column so
equality-lookup-by-plaintext stops being hand-rolled by creators;
deterministic encryption stays refused), and the **PII-name lint** (field
names like `ssn`/`card`/`passport`/`dob` without `.encrypted()` warn in dev
with the remedy; mask-only on such names warns that snapshots stay readable).

### Value formats and identity — one `t.typedId(prefix)` spelling

migrate's ID helpers (`ids.typeId`, `ids.ulid`) carry a validated-text facet; db carries
`t.id(prefix?)`, and the fold converts between them and the
`.assigned({ by: "typedId", on: "insert" })` assignment metadata seen in
generated code. Resolution (decided in review): **one factory,
`t.typedId(prefix)`** — spelling the platform's own term for the concept
(typed_id: UUIDv7 + base36 + entity prefix, the stack-wide identity
invariant). It lowers to the `idPrefix` facet, and the descriptor emitter
derives the assignment metadata from it — the same derivation the fold
performs today, relocated, not redesigned. `t.id()` and `ids.typeId()` are
removed with no alias. The prefix stays declarable per field; when omitted,
the current derivation from the collection name is preserved. `ids.ulid()`
retires with the namespace: the `ids` object and `IdFormats` are removed
outright, and the `ulid` value format goes with them. That is a deliberate
reversal of the position this paragraph first took.

### Numbers — the semantic family

Vendor inventories suggest many numeric types; semantically there are three
things a number can mean:

| Concept | For | Storage |
| --- | --- | --- |
| Exact integer | counts, quantities, FKs, minor currency units | int4 / int8 |
| Approximate real | measurements, scores, ratios | float8 |
| Exact decimal | money, rates, anything regulated | `numeric(p,s)` |

Every production number bug is a category error against that table: money in
a float, a counter overflowing its width, an equality match against an
approximate value.

The decisive evidence for the width cut is internal: **the runtime lexicon —
what crosses the V8 boundary — already ships exactly int4 / int8 / float8 /
numeric.** `smallInt` and `real` exist only in migrate's DDL lexicon, so
keeping them in the unified builder would mean *widening the runtime* (new
descriptor tokens, codecs, V8 conversions, parity tests) for types whose
benefit is irrelevant at platform scale and whose risks are live — a
`smallInt` counter overflows at 32767, an entirely plausible agent-authored
mistake. The engine already curated the vendor zoo (no `tinyint`,
`mediumint`, `unsigned`, or display widths); this finishes the cut at the
line the runtime drew. The engine keeps the `smallInt`/`real` `ColType`
tokens for introspection and drift detection on pre-existing schemas,
steering declarations to `t.int()` / `t.double()`; the authoring factories
die.

The unified number family:

```
t.int()              exact integer, bounded growth          — the default integer
t.bigInt()           exact integer, unbounded growth, ids   — "it grows" → this
t.double()           approximate real                       — the only float
t.numeric({p, s})    exact decimal — money, rates           — the money type
```

Rules:

- **Money is `t.numeric({ precision, scale })` or integer minor units
  (`t.bigInt`), never a float.** Companion work (not the merge itself): the
  platform lint pack flags money-looking field names
  (price/amount/total/balance/cost/fee/…) typed as `t.double()`, with the
  remedy in the message.
- **No unsigned anywhere.** MySQL-only, unportable by construction.
- **`t.number()` dies.** The name carried no approximation signal and read as
  "the default number", which is exactly how money ends up in a float.
  `t.double()` forces the beat of thought the choice deserves.
- **Equality on approximate columns: warn in dev, never refuse.** Same spirit
  as the unindexed-query warning — flags-as-0.0/1.0 and bucketed values are
  legitimate; accidental float equality is common enough to warn about.
- **`t.bigInt()` keeps its `number | bigint` JS contract.** Companion work:
  the RPC transport serializes bigint as a tagged string (the cursor codec
  already does), so returning a row containing a bigint cannot hit
  `JSON.stringify`'s refusal — agent-authored code should not have to
  remember the conversion.

### Temporal types — the two-type kernel

The DSL ships exactly two temporal column types, and the absence of the others
is the design:

```
t.timestamp()      an instant on the UTC timeline
t.calendarDate()   a day on the calendar: "YYYY-MM-DD", no time, no timezone
```

**No naive datetime type exists, ever.** The settled practice is that a recorded
moment is `timestamptz`; a bare `timestamp without time zone` stores a calendar
reading with no anchor and reinterprets silently across environments. Diesel's
history is the evidence: it shipped naive `Timestamp` as the portable default,
spent years of user data carrying timezone bugs, and added `Timestamptz` later
as the correction. The DSL starts where that arc ends, and enforces it the only
way that survives pressure — by not offering the type. The doctrine sentence:

> A timestamp is an instant on the UTC timeline. A calendar date is a day. To
> model a wall-clock rule in a place, store a calendar date (or plain fields)
> plus an explicit IANA zone column — the DSL will not pretend a naive datetime
> is a moment in time.

**The JavaScript contract is the millisecond instant, made self-consistent.**
Reads return a Unix-ms `number`; writes accept a `number`, `Date`, or ISO
string. PostgreSQL stores microseconds, so an equality filter built from a JS
value has to compare at millisecond granularity or the natural query breaks:
`find({ createdAt: row.createdAt })` would miss the row it just read. The
adapter therefore compiles JS timestamp equality to the millisecond bucket
(`ts >= v AND ts < v + 1ms`), and `distinct`, aggregates, and cursor seeks use
the same bucket. Rust hosts keep exact microseconds (`UtcInstant`); the portable
contract is the intersection, and the stronger backend normalizes its surplus
at the boundary instead of leaking it — see
[Portability](#portability--a-dsl-type-is-a-semantic-contract).

| | PostgreSQL | SQLite | MySQL |
| --- | --- | --- | --- |
| `t.timestamp()` | `TIMESTAMPTZ`, microseconds | canonical UTC text, milliseconds | `DATETIME(6)`, UTC by convention |
| `t.calendarDate()` | `DATE` | `TEXT` `YYYY-MM-DD` | `DATE` |

Resolution is declared per backend and enforced on the one path every written
instant crosses: SQLite refuses a finer-than-millisecond value with
`timestamp_precision_unsupported` rather than flooring it into a different
instant. `calendarDate` is string-only and deliberately rejects `Date` objects —
a JS `Date` has no calendar day until a timezone is chosen.

**Names never claimed: `.date()`, `.time()`, `.datetime()`.** In a TypeScript
ecosystem `Date` already means an instant, so `t.date()` is a trap in either
direction; `.time()` reads as "when did it happen" to the same audience; and
`t.datetime()` names the naive concept the DSL refuses. Two additions are
reserved and deferred, each for a stated reason: **`t.timeOfDay()`** — a clock
reading without a date is usually a recurring rule, which wants structured
fields, and MySQL's `TIME` is secretly a span, so there is no portable
semantics either — and **`t.interval()`** — genuinely useful, with the
expression-side `interval()` already present for date arithmetic; a column type
plus a JS value contract is its own change.

### Validation refinements — `.min()` / `.max()` / `.enum()` / `.pattern()`

Today these exist only on the db path, so whether a field has a `CHECK`-grade
constraint depends on which authoring surface the creator happened to use.
With one builder that split is no longer accidental — it must be decided:
either the refinements lower to `CHECK` constraints in migrations (making them
database-enforced), or they remain explicitly runtime validation. **This
proposal takes the former as the direction** — the migrate engine's expression
renderer is the natural lowering target, and `docs/reference/db.md` already
documents the strictness policy as descriptor-carried — but the lowering is a
follow-up change with its own design pass; the merge itself only makes the
refinements *available* on both paths, enforced at runtime as today.

### Arrays — contract vs storage

Arrays are where the portability doctrine gets its cleanest application, and
they arrive pre-split: db's `t.array(t.string())` is parameterized but
JSON-stored only; migrate's `t.textArray()` is native `text[]` but
text-only and unparameterized; the Rust model layer spells the same choice a
third way (`#[orm(array_storage = "native")]`).

The load-bearing fact: **the runtime contract is identical across both
storages** — same values, same order/duplicates semantics, same
`push`/`pull`/`addToSet`, same NULL/empty/`"NULL"` distinctions
(`docs/architecture/data-orm.md`, "Array storage"). A creator cannot tell from
results which storage served the query. Storage is therefore a *physical
facet*, not a type — and `t.textArray()` dies because it bakes a vendor
spelling (`text[]`) into the contract name. On SQLite and MySQL that name is
already a lie (the storage is JSON text there); the DSL names contracts, not
vendor types.

Resolution: **one parameterized factory, storage as an options facet.**

```ts
t.array(t.string())                          // storage defaults to "json"
t.array(t.string(), { storage: "native" })   // text[] on PG; faithful emulation elsewhere
```

- **Default storage is `"json"`** — the portable, always-emulatable choice.
  Native is the opt-in performance path, which is what it actually is (binary
  protocol binding, `array_append`/`array_remove`/`array_position`,
  GIN-indexable on PG).
- **Storage stays authored, never auto-picked.** The choice changes the
  physical column type (`text[]` vs `jsonb`), and migrations render physical
  DDL — a heuristic that picks storage would silently change what a redeploy
  renders and make migration output non-reproducible. Authored-with-default
  is the posture for anything that reaches DDL.
- **No factory explosion.** When native storage later supports non-text
  elements (PG has `int8[]`, `uuid[]`, …), the parameterized form absorbs them
  with no new factories; the `t.textArray()` world would need
  `t.bigIntArray()`, `t.uuidArray()`, ….

Element and storage legality, enforced at authoring with remedies:

| Element | `storage: "json"` (default) | `storage: "native"` |
| --- | --- | --- |
| `t.string()` | ✓ | ✓ |
| `t.int()` / `t.bigInt()` / `t.double()` / `t.boolean()` / `t.timestamp()` / `t.calendarDate()` / `t.json()` | ✓ | refuse: native array storage supports string elements only — drop `storage` or use `t.string()` |
| `t.ref(...)` / `.references(...)` | **refuse, both** | **refuse, both** |
| `t.object()` / `t.union()` / `t.literal()` / nested `t.array()` | refuse, both | refuse, both |
| masked or encrypted element | refuse — per-element protection is unsupported (and today it is *silently dropped*; see the bare-item rule below) | refuse — matches the ORM's `invalid_schema` today |

**Items must be bare.** An item builder carrying any facet — `encrypted`,
`mask`, `min`/`max`, `precision`/`scale`, defaults — is refused at authoring
with a remedy. Today's `t.array` reduces the item to its type discriminant
and silently drops the rest: `t.array(t.encrypted())` builds an *unencrypted*
string array, and `t.array(t.numeric({ precision: 10, scale: 2 }))` loses its
scale. A protection declaration evaporating without an error is the worst
failure class this proposal exists to remove; refusal is the only acceptable
behavior until per-element facets are genuinely supported.

The ref row is doctrine, not a portability limitation: **arrays are values,
not relations.** No backend can enforce per-element foreign keys, so
arrays-of-refs are refused on *every* backend — the one refusal that has
nothing to do with storage. The remedy names the correct modeling: a separate
collection with a `t.ref`.

The Rust `#[orm(array_storage = "native")]` attribute maps onto the same
`storage` facet; the descriptor already carries the distinction (the
`textArray` column type), so no wire change is needed — only the authoring
spelling unifies.

### Structured types in migrations — `t.object` / `t.union` / `t.literal`

Decision (closed in review): **the migration path accepts the structured
types.** Today the db→migrate bridge refuses them with
`UnsupportedColTypeError` — an artifact of the two-dialect split, not a
designed exclusion. The runtime descriptor and the SDK already carry their
structure (validation, `InferSchema` recursion, dotted error paths); apps
authored through migrations simply could not reach a shipped capability. With
one builder, the refusal has no reason to exist.

Rendering:

- `t.object({...})` → `JSONB` on PG, JSON text on SQLite/MySQL; the nested
  shape is recorded in the descriptor for validation and typed reads.
- `t.union(...)` → flat nullable columns, one per variant-wide field, plus
  the discriminator `IN (...)` constraint and per-variant `CHECK`
  constraints — the same layout the db SDK documents today.
- `t.literal(v)` → the underlying primitive with a `CHECK (col = <value>)`
  constraint.

The work (not the design): the differ must diff a union's flat-column layout
cleanly — adding a variant is a column-set change plus `CHECK` updates, and
the `CHECK` constraints must round-trip through introspection on both
backends. The `UnsupportedColTypeError` arm of the bridge is retired for
these types; `colTypeFromDbField` gains the lowering.

## Portability — a DSL type is a semantic contract

The DSL names contracts, not vendor types. Three layers:

```
DSL type      the contract       values, legal operations, promised precision
Engine codec  the fulfillment    storage pick, encode/decode, operator emulation
Vendor type   an implementation detail
```

The rule that keeps emulation honest:

> Semantics may never degrade silently. Performance may degrade silently.
> Capability may narrow only where the contract declares it, and the narrowing
> must refuse loudly at use rather than approximate.

Every (type, backend) pair lands in exactly one of four fidelity tiers:

| Tier | Meaning | In the tree |
| --- | --- | --- |
| Native | vendor type matches the contract | `timestamptz`, `jsonb` on PG |
| Faithful emulation | other storage, same contract | JSON-as-`TEXT` plus structural equality on SQLite; `text[]` as JSON text |
| Declared narrowing | contract holds; a named capability or resolution is reduced, declared, and refused at use | `innerProduct` → `VECTOR_UNSUPPORTED_METRIC`; polygon ops → `POLYGON_OPS_PG_ONLY`; finer-than-millisecond timestamps refused on SQLite |
| Refused | the contract cannot be honored — fail closed | `t.timeOfDay()`, naive datetime |

The test for the middle two: **can a user tell from results alone which backend
served the query?** If yes and the difference is semantic, it is not an
emulation but a different type — refuse it. If yes and the difference is speed,
it belongs in `docs/reference/sqlite-divergences.md`. If no, it is faithful.

**The weakest backend defines the portable contract.** Portability is the
intersection: SQLite's millisecond timestamps make milliseconds the portable
resolution, and PostgreSQL's microseconds are surplus the adapter normalizes at
the boundary. The alternative — exposing per-backend precision — means portable
code cannot be written, and an agent-authored app trips the difference in
production where the dev backend never reproduced it. Rust hosts opt into a
narrower, explicitly non-portable tier (`UtcInstant`); that tier is not the
default.

A new type is admitted only with its contract card filled in: the contract, the
JS and Rust value, the legal operations, the per-backend storage plan, the
declared narrowings, and the dual-backend parity gate. A type whose card cannot
be filled is not portable — it gets either a declared narrowing or no seat.

## Designing for agent authors

The primary author of a zeroship schema is a coding agent, so the DSL must be
correct under *statistical* authorship — pattern-matching from training data
saturated with other platforms' conventions — not careful authorship.

| Agent failure mode | Countermeasure |
| --- | --- |
| Plausible-name guessing (`t.date()`, `datetime`) | The wrong names do not exist; the names that do explain themselves |
| Convention smuggling (naive timestamps, money in floats, `varchar(255)`) | Make the smuggled pattern unwritable, not merely discouraged |
| Silent-wrong-answer acceptance (`find({ createdAt: row.createdAt })`) | Self-consistent contracts: the natural query is the correct query |
| Hallucinated surface (`t.time()`, `.nullable()`) | Closed sets validated at author time, with errors naming the valid options |
| Doc-skimming | Examples are the spec: the starter scaffold and generated files are what gets cloned |
| Retry-and-vary on error | Total, deterministic errors carrying the remedy — the error is the documentation the agent reads |

The last row is the one to keep investing in, and the platform already has the
pattern: the deploy gate answers `409 schema_not_applied` with a `"remedy"`
field naming the exact command. **Every refusal should name the fix.**
"`t.date()` is not in the surface; use `t.timestamp()` for an instant or
`t.calendarDate()` for a YYYY-MM-DD day" is worth more than three paragraphs of
prose the agent will never open.

Companion nets, none of them part of this merge: the **lint pack** (money-named
fields typed as floats; PII-named fields without `.encrypted()`; equality
filters on approximate columns), and the **scaffold-as-training-set**
discipline — `examples/starter/` and the generated `env.db.ts` are read as
ground truth by every agent that touches the platform, and deserve the same
review rigor as the API.

## The refusal matrix

A shared builder lets a creator write DDL-only facets on the declared-schema
path, where no migration executes them. The descriptor emitter must refuse,
with a structured error, never silently drop:

| Facet | On the migration path | On the declared-schema path |
| --- | --- | --- |
| `identity` / `autoIncrement` | renders `GENERATED … AS IDENTITY` | refuse: identity is DDL-authored; use an assignment generator |
| `generated(expr)` | renders the generated column | refuse: computed columns are DDL-authored |
| `collation(intent)` | renders per-dialect collation | **accept and ignore** (decided in review): collation is purely physical — the database enforces it on every query, so the runtime has no behavior to drive and nothing is dropped. This also lets a collated field definition be *shared* between a migration and a declared schema. The runtime's own bytewise pin on sortable `id` columns is independent and unaffected. |
| `enum(name)` / `domain(name)` | references the named type object | refuse: standalone type objects are migration-authored |
| `caseSensitive` | recorded facet | accept and ignore, same reasoning as collation |
| `min`/`max`/`enum(values)`/`pattern` | runtime validation today; `CHECK` lowering per the follow-up above | runtime validation (unchanged) |
| `mask`, `encrypted`, `unique`, `index`, `references`, `default`, `primaryKey`, `required` | supported | supported |
| `clientDefault` | supported | **refuse** (decided in review): the descriptor crosses a JSON boundary into the engine, where a live factory cannot survive, so the runtime would silently lose the default. A declared schema carries only a database default via `.default(value)`. |

The matrix is enforced at one point — descriptor emission — with one error
code family (`SCHEMA_FACET_NOT_DECLARABLE` or similar), and the table above is
the test plan: every cell gets a test naming its expected outcome.

The principle that decides each cell (settled in review): **refuse on the
declared-schema path only when the DDL facet would silently substitute for a
runtime fact the descriptor must drive.** `identity`/`generated` imply
write-input exclusion — a runtime behavior that must instead arrive via
assignment metadata — so they are refused. A purely physical,
database-enforced facet (`collation`, `caseSensitive`) drives nothing at
runtime: the database applies it to every query, the runtime merely inherits
it, so it is accepted and ignored rather than refused. "Ignore" here is not
a silent drop — there is no runtime behavior to drop.

## Package topology

New package **`@zeroship/schema`** under `packages/schema/` (name decided in
review; `@zeroship/schema-dsl` was the rejected alternative):

- Contains: the shared immutable `TypeBuilder` with the phantom brands, the
  widened `FieldDef` union, `toFieldDef()`, and the type-level inference chain
  that reads them (`InferSchema` and the key helpers — `t.object()` names it, so
  leaving it in db would make the two packages reference each other).
- Contains nothing else: no `Collection`, no `Query`, no recorder, no native
  types, no runtime access. Its `package.json` declares zero dependencies, and
  its `tsconfig.json` sets `"types": []`, so a reference to `process`, `Buffer`
  or a `node:*` module fails to compile there — the bare-Node constraint the
  migrate toolchain needs, enforced by the compiler. That is narrower than a
  module-graph probe: nothing here can assert what the leaf's importers touch.
- The `FieldDef → ColType` lowering stays in `@zeroship/migrate`, because it
  returns migrate's `ColType`.
- `@zeroship/db` re-exports `t`, `TypeBuilder`, `FieldDef`, and friends, so
  creator-facing imports are unchanged. **It BUNDLES the leaf rather than
  depending on it** (devDependency + tsup `noExternal` + `dts: { resolve: true }`),
  which differs from the "db depends on it" wording this section first carried.
  The reason is mechanical: `@zeroship/db`'s code is embedded in V8 (directly, and
  through the `zeroship:db/adapter` host module), and no bare npm specifier
  resolves there — an externalised leaf produced 75 failing V8 tests across the
  adapter and the package dist. Bundling keeps both artifacts self-contained and
  means `@zeroship/db` no longer requires the leaf to be published.
- `@zeroship/migrate` depends on it, replaces `ColumnDefImpl` with the shared
  class, and **deletes `packages/zero-migrate/src/db-types.ts`**. The IR
  recorder reads the unified representation; the wire format is unchanged.

### Why not `migrate → db` (considered and rejected)

Depending on `@zeroship/db` directly would kill the vendored copy without a
new package, but:

1. **The purity constraint is load-bearing.** The migrate CLI and the N-API
   drain host run in plain Node with no `env`. The db package's module graph
   reaches the native bridge, policy, live queries, subscriptions. It happens
   to be side-effect-free at import time today; nothing enforces that, and
   the migration toolchain would break the day that stops being true.
2. **The stability gradient inverts.** The lexicon is the slowest-moving,
   most-depended-on artifact in the stack — the Rust descriptor decoder, the
   fold, and every generated file pin to it. Coupling its release cadence to
   the runtime SDK's is backwards.
3. **Cycle risk.** The moment db-side tooling wants IR types (schema diff,
   drift diagnostics), `db → migrate` appears and the pair is circular.
4. **Repo precedent.** `crates/zeroship-workflow-schema` exists as a leaf
   crate precisely so a platform service can install the journal without
   depending on the engine. Contracts live below their consumers.

Also considered and rejected: keeping the builder in db behind a dedicated
`@zeroship/db/schema` subpath export with a plain-Node import gate. Defensible,
bounds the import graph, but keeps the publish coupling and requires permanent
discipline to keep the subpath runtime-free.

## What does not change

- `InferSchema`, `Row`, `RowInput`, `Filter`, `MaskedValue`, `Id<>`, and the
  entire type-inference chain. The brands keep their names and positions.
- `schema.runtime.json` — the descriptor wire shape is unchanged by the merge
  itself. It widens only if and when the runtime learns to serve more of the
  lexicon, as its own gated change.
- The IR wire format and the ambient recorder singleton in
  `@zeroship/migrate`. The recorder's *input* representation changes; its
  output and draining contract do not.
- Creator-facing imports: `import { t } from "@zeroship/db"` and
  `import { t } from "@zeroship/migrate"` both keep working — with renamed
  members per the table, landed as clean breaks.

## Relationship to descriptor-direct typing

An alternative end-state was discussed during review: skip builder
reconstruction in `env.db.ts` entirely and have `Db<D>` infer directly from a
const-asserted descriptor type (migrations → descriptor → types, one
direction, no re-expression). **This proposal does not decide that.** The
shared builder is compatible with either outcome: if descriptor-direct typing
later lands, the leaf package remains the single lexicon (now serving the
declared-schema path and tests), and the fold simply stops emitting builder
calls. The merge removes the translation layer's *dialect* risk whether or not
the translation itself is later removed.

## Rollout

Sequenced as separate PRs, each self-contained, no aliases at any step:

1. **Extract the leaf.** Create `@zeroship/schema`: move db's `TypeBuilder` +
   `FieldDef` + brands, converted to clone-on-modify. db re-exports; migrate's
   vendored `db-types.ts` is deleted, and `db-lexicon.ts` follows it in a later
   step rather than this one (it returns migrate's own `ColType`, so moving it
   here would make the two packages reference each other). Landed in this step
   instead: the inference chain moves too (it is what `t.object()` names), and
   `@zeroship/schema` compiles under `"types": []` so the zero-ambient-types
   property is enforced rather than asserted. No CREATOR-FACING spelling
   changed; migrate's re-exported bridge surface did change with the rewire -
   see `docs/reviews/2026-09-16-orm-api-review.md` for the exact deltas.
2. **Token renames.** `FieldDef.type: "date"` → `"timestamp"`; sweep the fold,
   the descriptor decoder, the Rust `TypeName` mapping, and every fixture in
   the same change.
3. **Factory unification.** `t.string({ length })`, `t.int()`,
   `t.calendarDate()`, `.required()`, `.references` canonical form with the
   narrowed `t.ref`, `t.array(item, { storage })` replacing `t.textArray()`,
   `.encrypted()` replacing the `t.encrypted({ of })` wrapper (with seal-time
   default-mask resolution), `t.typedId(prefix)` replacing `t.id()` /
   `ids.typeId()`, and the number family: `t.number()` → `t.double()`, with
   `t.smallInt()` / `t.real()` removed from authoring.
   Every migration corpus file, example, and generated fixture updates in the
   same PR; `db/migrations-ts/` and `examples/` are the sweep surface.
4. **`.default` / `.clientDefault` split.** Factory-function call sites move
   to `.clientDefault`; scalar and expression defaults stay.
5. **`ColumnDefImpl` replacement.** migrate's recorder consumes the shared
   class; the widen-`FieldDef` facets (`char`, `double`, `inet`, `uuid`, `enum`,
   `domain`, `collation`, `generated`, `identity`, `caseSensitive`, `idPrefix`,
   `encrypted`, `vectorMetric`, plus the carriers `charLength`, `enumName`/
   `enumSchema`, `domainName`/`domainSchema`, `refName` and `arrayStorage`) land
   with the refusal matrix enforced at descriptor
   emission. This step also carries the engine half of structured types in
   migrations (`t.object`/`t.union`/`t.literal` rendering and differ support)
   — the one part of the merge that is new engine behavior rather than a
   spelling change.
6. **Fold emits one dialect.** `gen-types` stops translating; generated
   `env.db.ts` reads in the unified spelling. The `gen-types --check` gate
   verifies regeneration is a no-op on a clean tree.

Step 1 is the risky one (it touches the inference chain's foundation); steps
2–4 are mechanical sweeps gated by the type-checker; step 5 is where design
review concentrates (the refusal matrix); step 6 is deletion.

## Verification

- **Type-level:** the existing `InferSchema`/`Row`/`Filter` type tests must
  pass unmodified against the extracted builder; add aliasing-regression type
  tests (`const a = base.required(); const b = base.max(1)` must not leak
  facets between `a` and `b`, at both type and runtime level).
- **No-ambient-types, enforced at compile time:** `packages/schema/tsconfig.json`
  sets `"types": []`, so a reference to `process`, `Buffer` or a `node:*` module
  fails to compile in the leaf. That is the migrate toolchain's bare-Node
  constraint enforced by the compiler rather than by a runtime probe; a bare-Node
  import check is still worth adding when the leaf grows a build step.
- **Recorder singleton:** the existing drain tests from the fork-divergence
  collapse must pass unchanged; step 5 adds a test that a migration authored
  with the shared builder records identical IR to the pre-merge spelling for
  equivalent columns (fixture pairs, compared as parsed IR, not text).
- **Fold fidelity:** the `gen-types --check` CI gate plus a new round-trip
  property: for every example app, fold → emit → parse the emitted module →
  compare against the descriptor as structured data.
- **Temporal contract:** a dual-backend case asserting that a JS timestamp read
  and then used as an equality filter matches its own row (the millisecond
  bucket), and that SQLite refuses a sub-millisecond write with
  `timestamp_precision_unsupported`.
- **Refusal matrix:** one test per cell of the matrix above.
- **Existing suites:** `cargo xtask test data`, `cargo xtask test migrations`,
  the platform migration corpus gate, and the example acceptance suites run
  green at every step.

## Open questions

1. **Validation refinements → `CHECK` constraints** — direction endorsed here,
   but the lowering (expression renderer, SQLite/PG parity, the
   `strictness` descriptor policy's interaction) is a follow-up design, not
   part of this merge.
