# The `@zeroship/migrate` JS-DSL, v2 — the redesigned authoring surface

**Status:** final synthesized design (design-only; no code in this document is shipped yet).
**Supersedes:** the shipped surface documented in `docs/reference/migrate-dsl-examples.md` and implemented in `sdks/migrate/src/{ops,types,pg,index}.ts`; treats `docs/proposals/2026-07-02-op-dsl-redesign-design.md` as prior art improved upon, not gospel.
**Back-compat:** none. Pre-launch. Every "deleted" below means deleted in the implementing PR — no `@deprecated` aliases, no shims, no detect-and-warn.

---

## 1. Motivation — the 36/100 critique as forcing function

The shipped surface was scored adversarially against five dimensions and failed all of them:

| Dimension | Score /10 | The one-line indictment |
| --- | --- | --- |
| FLUENT | 4 | Chains dead-end (`forValues()` returns `void`), swap grammatical subjects mid-lifecycle, and demand a lambda + `lit()` to set a string constant. |
| ELEGANT | 3 | Systematically two-or-three spellings per operation: `addCheck`/`check().add`/free `check()`; `addForeignKey`/`foreignKey().add`; `membership`/`c.pg.eqAnyArray`; index chain-modifiers *and* an args-bag with a silent merge; `t.int`/`t.integer`; two module shapes. |
| CLEAN | 3 | The `/pg` boundary is decorative (RLS, policies, triggers, `currentSetting`, `nextval`, `t.domain` all on core); `c("VALUE")` magic string; `del` beside a wire tag literally `"delete"`; positional booleans; a `references.schema` argument whose only legal value echoes another argument; the `deferredUpOps` cross-migration side channel. |
| EXTENSIBLE | 4 | Every closed token set hand-mirrored in **three** places under "lock-step" comments; PG-isms baked into "portable" node names (core `membership()` emits `pgArrayMembership`); growing `extract` beyond `"day"` touches four files. |
| EXPRESSIVE | 4 | No enum `ADD VALUE` at all; `update.set` can't take a scalar; expressions can't table-qualify a column so a view join's ON clause is unwritable; views have no GROUP BY/aggregates (the guide's own `order_totals` matview computes no totals); `interval("7 days")` throws. |

**Overall: 36/100.** And the examples guide — which opens with *"Every example below is faithful to the shipped API"* — contains **fourteen** examples or claims that are invalid against the shipped types (positional `setType`, the object-form `case`, the fake `createRaw` `reason` field, the nonexistent `hash` index method, the `t.real()`/`t.float()` "alias" that is actually float4-vs-float8, …).

The diagnosis that drives everything below: the recorder/IR machinery underneath is disciplined — closed AST, structured errors, fail-closed vendor gating, immutable `ColumnDef`. **The grammar on top is a committee.** Each of the fourteen invalid examples and each low score traces to a missing *law*, not a missing feature. So the redesign is a constitution first (§2), then the concrete surface derived from it (§3), with mechanical enforcement so the laws cannot silently rot.

---

## 2. Principles & Styles — the constitution

Every rule improves at least one of the five dimensions and hurts none; where a rule costs a dimension at the call site, the trade is owned inline. Every law names its enforcement mechanism — a law without one is a wish. Licensed exceptions never live in prose: a machine-readable **surface registry** carries every licensed sugar pair and every licensed positional exception; lints assert *membership in the registry*, never shape.

### The twelve principles

**P1 — One grammar, one spelling** *(ELEGANT, FLUENT)*. Every operation has exactly one spelling, derived from one grammar: `kind(name, identityOpts?) → inert handle → selector(name) → terminal({ payload })`. Selector form is THE grammar for named sub-objects; the verb duplicates (`addCheck`, `addForeignKey`, …) are deleted. **Owned trade:** the most common constraint call gets longer (`check(n).add({ expr })` vs `addCheck(n, e)`) — a real per-call-site FLUENT cost paid for uniformity plus the selector machinery (schema inheritance, `SELECTOR_NOT_TERMINATED`). Inline definitions inside `create({...})` are plain object literals, never free constructors. Exactly one module shape: `export default { name?, up, down? }`. The **sugar clause** is the only licensed duplication: a shorthand may exist iff (a) it desugars to exactly one canonical IR shape by a pinned rule, (b) declaring shorthand + longhand over one subject is a record-time collision error, (c) the shorthand never grows options. Enforced by a **two-tier census** that is **derived, never self-reported** <!-- Amended in round 2: the detection mechanism was a wish. -->: every producer is minted through the one `defineOp(kind, slots)` registration chokepoint, and the recorder accepts only mint-branded ops — so the census inventories *are* the `defineOp` registry, and a walk of the compiled artifact's exported symbols proves no export can reach `record()` unminted (a duplicate producer that "forgets to self-register" cannot exist, only fail at record). Tier 1 — exactly one **producer-function identity** per op kind (inherited members count once, so `/pg`-widened entry twins are census-legal by construction); tier 2 — a generated **payload-slot writer inventory**, exactly one public writer per slot, surface-registry pairs the only exceptions, each with a byte-identical-IR test and a collision test. Create-time-inline vs alter-time-selector is *stated as different ops* (different SQL, different op kinds), never asked of the census.

**P2 — One grammatical subject per object** *(FLUENT)*. Every named sub-object is addressed parent handle → selector(childName) → lifecycle terminal, and the subject never changes across create/alter/drop. **No terminal returns `void`**; the full return matrix is pinned: entry functions return the object's handle; sub-object `.add`/`.create`/`.attach`/`.detach`/`.drop` return the **parent**; column per-intent terminals return the **same column handle**; `.rename` returns a handle addressing the **post-rename** name; every **entry-level** object's `.drop()` (the nouns entry functions construct — table, view, matview, enum, domain, sequence, role) returns a **create-only continuation** (drop-then-recreate sanctioned; anything else is a tsc error, with `HANDLE_DROPPED@record` for aliases), while sub-object `.drop()` returns the parent per the matrix's first clause — the two clauses partition, they do not overlap. **Stale aliases are poisoned**: the recorder keeps a per-subject generation counter; recording through an out-of-generation alias is `HANDLE_STALE@record` with a `suggested_fix` naming the fresh handle. <!-- Pinned in round 2: two-handles-one-subject. --> Subject identity is `(schema, name, kind)`; constructors stay inert and never read recorder state — a newly constructed handle binds to the subject's **current** generation at its **first record**, so constructing a second fresh handle for a subject mid-migration (§3.5's `pgTable` twin after core ops) is legal by construction; only a handle that already recorded (or was returned by a terminal) under an earlier generation is poisoned. Column alteration is per-intent terminals (`setType`, `setNotNull`/`dropNotNull`, `setDefault`/`dropDefault`, `rename`, `comment`) — no `.alter({...})` bag anywhere (sequence and role bags split the same way). `rename({ to })` never restates the type — the fold supplies it (an acknowledged wire reshape: `Op::RenameColumn.type` deleted). Toggle quadruplets collapse (`enable/force/disable/noForce RowLevelSecurity` → `setRls({ enabled, forced? })`). Free functions exist only for parentless objects.

**P3 — Identity is positional, payload is named, toggles are never bare** *(CLEAN)*. Names (identity) are the only positional arguments; everything else is one options object. No positional booleans or enums — enforced by a `.d.ts` declaration lint over built declarations (all overloads, inherited members). Sole exception: the registry-enumerated single-scalar terminals (today exactly one member: `.comment(text | null)`); membership, not shape, is what the lint asserts. **Schema lives once**, on the entry function; selectors/terminals inherit; per-op `{ schema }` overrides and `references.schema` are deleted (corpus-verified safe; the claiming phase re-verifies mechanically over tables, FKs, **and sequence/enum/domain references**; the first genuine cross-schema need re-opens the IR in place — never a parsed dotted string). **Detached value references to parentless objects are entry-function positions and carry their own identity opts** (`nextval("orders_id_seq", { schema: "zeroship" })`) — a bare reference would render search_path-dependent, in a DSL whose pitch is determinism.

**P4 — Values are values everywhere** *(FLUENT, EXPRESSIVE)*. `DmlValue<C> = Scalar | Expr | ((c: C) => Expr)`, one rule for every value position, instantiated per position with a pinned builder. `Scalar` is pinned: `string | number | boolean | null | Json` (plain objects/arrays over Scalar, `|n| < 2^53`); **bigint is outside Scalar and refused at tsc everywhere** — the path is `decimal("10")`. Bare scalars auto-wrap to `Literal`; `lit()` survives for disambiguation only. The `Expr` brand is `Symbol.for("zeroship.migrate.expr/v1")` (dual-package-safe); the reserved `__zsExpr` marker key backs the de-branded-clone refusal (`OP_INVALID`, never silently notarized data). The `{ fn: "now" }` structured default is **deleted** — under "plain objects are scalars" it is ambiguity by construction; function defaults are expressions. `eq(null)`/`ne(null)` are record-time errors steering to `isNull()`/`isNotNull()`. In-band string sentinels are banned — special values are exported symbols or dedicated builders. The widened scalar-or-expression wire slots become the **`kind`-tagged** `{ "kind": "json", "value": … } | { "kind": "expr", "expr": … }` wrapper with `deny_unknown_fields` (acknowledged reshape-in-place; in serde terms this is the *adjacently-tagged* form — tag `kind`, content `value`/`expr` — consistent with the spine's tagged closed enums; an earlier draft mislabeled it "externally-discriminated").

**P5 — The vendor boundary is load-bearing, enforced twice** *(CLEAN, EXTENSIBLE)*. Core exports only what renders on PG + SQLite + MySQL — at **op, node, AND option granularity**; a construct is core **iff** a three-dialect portable base form has a **landed** engine-owned parity proof. <!-- Amended in round 2: third tier. --> The generated table carries **three dispositions** per row: **portable** (the rule above — realization-required on all three), **transparent-degradable** (P12 — realization-required where native, absence-tolerable elsewhere; admitted per option-combination under P12's own proof + affirmation rule), and **vendor**. Core exports the first two classes; `/pg` owns the third; the S10 walk checks every core export against the table's disposition — a core `partitionBy` passes as transparent-degradable, while any vendor-scoped member or option at any granularity is rejected. Every PG-ism lives under `@zeroship/migrate/pg` (ops/entries/values) and `c.pg.*` (expression nodes). The mechanism is **value-level widening** — `/pg` exports entry twins (`pgTable`, `pgView`, `pgEnumType`, `pgMaterializedView`) returning widened handles that *extend* the core handles and *inherit* the same producer functions; **TS module/interface augmentation is banned** as a boundary mechanism (it leaks). Vendor expression nodes carry the `pg` prefix in surface *and* wire names; portable nodes never do. One source of dialect truth: the generated **per-op/per-node/per-option dialect table**, consumed by both the Rust validator and the S10 CI walk of core exports (including core option fields). **Both gates, always**: the import boundary is honesty and ergonomics; the engine's validate-time `VENDOR_OP_DENIED` (confined creators) / `DIALECT_UNSUPPORTED` (off-scope targets) is the trust boundary. Imperative dialect branching is unrepresentable (`up()` receives no dialect); the sole divergence channel is the declarative, ratchet-counted `dialect({...})` value escape.

**P6 — A complete algebra before any escape hatch** *(EXPRESSIVE)*. The expression and query algebra must cover the platform schema corpus structurally; a shape that forces `raw` is a P0 surface defect. Every "is core" claim is a **candidate** under P5's admission rule — admitted iff the three-dialect parity proof lands in the claiming phase, with the vendor tier (or `dialect()` legs) as the pre-declared fallback; P6 confers candidacy, P5 confers membership, so the two principles cannot collide. Qualified column refs are structural (two-arg form), never parsed. Enum evolution is a core candidate with pinned three-dialect legs; positioned insert is a vendor *option*. Intervals are structured values, never a parser wearing a literal's name. Views get `groupBy`/`having`/`distinct`/`c.agg.*`. Cast targets are exactly the ColType tokens. **Sufficiency gate**: the platform Liquibase changelog corpus must be structurally authorable; any corpus shape that can't be blocks the claiming phase.

**P7 — One source of truth for every closed set** *(EXTENSIBLE)*. One schema (`op-ir.schema.json` + enums source) generates the Rust enums, TS literal types, runtime guard arrays, the dialect table, doc token tables, and the surface registry. **One compiled recorder artifact** — the SDK recorder and the engine-embedded recorder are the same build output (the `migrate_ops.js` twin is deleted); it exports both census inventories. One casing rule: camelCase everywhere; wire tags equal surface tokens. **Docs are generated and executed** under a pinned three-marker snippet taxonomy — `compile-and-record` (must pass tsc + recorder), `expect-error(CODE@rung)` (must fail with exactly that code at that rung), `historical` (marked, excluded; unmarked failures are red) — over the guide *and* this document. tsc-rung expectations are matched under the workspace's **pinned TypeScript version**; a compiler bump regenerates the expected diagnostics in the same change, so a TS upgrade can never masquerade as (or mask) a surface regression.

**P8 — Nothing silent** *(CLEAN)*. `deferredUpOps` is deleted; every op call outside a recorder is `OP_OUTSIDE_RECORDER`, uniformly. Discarding part of an accepted argument is forbidden (`OP_INVALID` + `suggested_fix`, never a shrug). No dual-source precedence merges. **Secrets travel as references**: `secretRef(name)` is a typed `SecretRef`; the checksummed IR carries the *name only*; resolution happens at apply from the engine's injected secret map, fail-closed. Credential slots on vendor ops are typed `SecretRef`, never `string`. The raw side: a validate-time credential lint over every SQL island refuses **credential-assignment positions** (`PASSWORD`/`ENCRYPTED PASSWORD` adjacent to role/user DDL, `IDENTIFIED BY`) — gate the carrier, don't pretend to parse SQL — with the checksummed, ratchet-counted `secretLintWaiver: { reason }` as the audited false-positive path. The K8 recorder discipline survives verbatim: eager record, `SELECTOR_NOT_TERMINATED`, fail-closed existence guards, structured codes with `suggested_fix`.

**P9 — Fail at the earliest layer that can express the refusal** *(CLEAN)*. The ladder is **tsc → record → validate → lower** — the record rung includes the recorder's end-of-script **drain** check (`SELECTOR_NOT_TERMINATED` for unterminated selectors); drain is part of record, never a fifth rung. Every construct's spec states its misuse rung with a test at that rung. Position-scoped builder types move whole error classes to tsc; what the types cannot carry is a **declared, enumerated validate residue**, never an undocumented late failure. Dialect/capability/context refusal happens at validate, never at lower; lower keeps only live-schema fail-closed guards; apply owns secret resolution. **Nothing representable is always-refused** — if no input can make a shape legal, the shape is deleted (the shipped `IrConstraintKind::Pk`, reachable only through the always-refused alter path, is the flagship deletion).

**P10 — Names tell the truth** *(ELEGANT, CLEAN)*. Surface names match wire tags, **modulo the subject the handle already carries** (`sequence(x).setOptions` records `setSequenceOptions` — the noun lives on the selector and is never restated in the method name); lifecycle verbs record exactly the SQL they name **on the natively-realizing dialect** (partition `.create` = `CREATE TABLE … PARTITION OF` on PG; `.attach` = `ATTACH PARTITION` — never conflated; degraded P12 legs are named in the plan output, never smuggled under the verb); lookalike pairs are true aliases or don't exist (aliases are banned, so no pairs exist); `del` → `.delete()` (method position is not a reserved-word context); exactly two reserved-word renames in the whole surface (`enumType`, `createFunction`); no name advertises a capability the implementation lacks; per-dialect caveats are type- or validate-level facts, not footnotes.

**P11 — Raw is a debt instrument** *(CLEAN, EXPRESSIVE)*. A **closed island inventory**: `raw({ sql, reason })`, `rawSelect({ sql, reason, columns? })`, `createFunction({ body })`. Nothing else accepts a SQL string; core reaches none of them. `reason` is required on `raw`/`rawSelect` and travels **inside the checksummed IR**; `createFunction.body` carries no reason field (the island type is its reason — mandatory boilerplate would train authors that reasons are ceremony). **Budgets are ratchets — four counters** <!-- Amended in round 2: was three; P12 adds the fourth. -->: the raw/rawSelect count, the `dialect({...})` leg count, the `secretLintWaiver` count, and the **P12 degraded-leg count** each hold against a committed baseline file; increases require a reviewed waiver artifact in the same change; decreases auto-lower the baseline. The first three are recomputed from the recorded IR of the committed corpus alone; the degraded-leg count is a property of (recorded IR × target-dialect set), so the baseline file **pins the target set** it was computed against — changing the target set is itself a baseline change requiring the same waiver discipline. <!-- Amended in round 3: the fourth counter had no unit of account and blurred the semantic/degraded boundary. --> **The unit of account is pinned: one count per (recorded op × target dialect) pair whose realization the generated dialect table classes as the absence form** — a collapse-affirmed partitioned parent create counts once per collapse target, and each fold-recorded (no-DDL) child create counts once per collapse target, so a parent with 12 children baselines at exactly 13 per collapse target, deterministically. **Semantic realizations never count**: an op the table classes as *realized* on a target — different SQL, same feature, e.g. §3.5's child-drop bounded DELETE — is faithful realization, not degradation (P12's own class boundary), so it appears in the plan output but not in this counter. The counter counts absence, nothing else. `procBody` is counted for visibility only — owned softness, not a dressed-up gate. No binds machinery: raw takes a complete SQL string or it is not raw.

**P12 — Best-effort native realization, transparent degradation** *(EXPRESSIVE, EXTENSIBLE, CLEAN)*. <!-- Rewritten in round 2: the "logical vs identical SQL" contrast was false — the portable core already renders per-dialect. The true axis is absence-tolerance. --> The axis P12 adds is **not** "same results vs same rendered SQL" — the portable core never demanded identical SQL anywhere (core enum `addValue` renders `ALTER TYPE` / CHECK-rewrite / `MODIFY` per dialect, §3.7; portable `inList` renders `= ANY(ARRAY[...])` on PG, §3.4). The true axis is **what the proof licenses when a dialect lacks the feature**:

- **P5/P6 portable core — feature-realization-required.** The same semantic feature must be *realized faithfully* on every claimed dialect, via arbitrarily different SQL, or the construct **fails closed**. Absence is never licensed; the proof is behavioral parity of the realized feature.
- **P12 transparent-degradable — feature-absence-tolerable.** A construct whose presence changes only physical layout or performance may be **entirely absent** on a dialect that cannot realize it faithfully — from one recording, with no author-side branching: native form wherever the dialect supports it, base form where it does not. The proof is **absence-equivalence**: identical logical behavior — query results, **insert-acceptance**, constraint behavior, **and DDL-acceptance** (whether a later migration's op against the construct succeeds or errors is itself observable behavior; §3.5's populated-default mirror guard exists because native PG's sibling-create DDL is data-dependent) — with and without the construct. <!-- Amended in round 4: DDL-acceptance added to the equivalence definition; without it a collapsed createPartition that can never fail passes dev and diverges from prod on data-dependent grounds. -->

The gate is **transparency, proven per option-combination and affirmed, never assumed**. Transparency is not a per-op property — option combinations flip the class: a partial *non-unique* index changes only physical layout, but `unique: true` × `where` changes **which inserts succeed** (collapsing the `where` enforces uniqueness on more rows; skipping the index enforces it on none), and `nullsNotDistinct` likewise. So the one generated dialect table (P5/P7) records transparency **per option-combination predicate**: a row is transparent only for the combinations its predicate names, each backed by a landed, engine-owned **absence-equivalence suite** — the named proof artifact: a pinned scenario corpus of DDL, an insert-acceptance matrix (in-bound, out-of-bound, NULL-key rows), constraint-behavior checks, a pinned query set (aggregates included, row-set comparison), **and a down-path leg — up → pinned inserts → down, row-set-identical across the native and the degraded form after the up *and again after the down*** <!-- Added in round 3: absence-equivalence is a property of the whole up/down path, not the up alone. --> — executed against the native and the degraded form with row-set-identical results on every target. Every unnamed combination is semantic and fails closed. **v1 admits exactly one construct to the class: create-time declarative partitioning (§3.5), under the validate rules stated there.** The index family is explicitly **not** admitted: vendor index options stay `/pg`, fail-closed per §3.3, and `IndexAdd`/`PgIndexAdd` carry **no `whenUnsupported` slot by design** — a `/pg` construct is realization-required by the import boundary's own honesty (a vendor op that degrades would make the `/pg` import a lie); a future degradable-index tier would be a new *core* admission through the combination-predicate mechanism, never a slot bolted onto the vendor type.

Degradation is additionally **opt-in at the call site**: the author affirms `whenUnsupported: "collapse"` — a closed union with exactly one member today; `"skip"` does not exist (for a create-time construct, skipping the clause *is* collapse, and skipping the op is incoherent). With the affirmation **omitted**, any target the table cannot realize natively refuses `DIALECT_UNSUPPORTED@validate` naming the missing affirmation — so a partitioned table must affirm collapse to be authorable in the `pnpm dev` = SQLite flow: an owned, stated cost of P8's no-silent-degradation, paid in one visible option. Everything **semantic** (constraints, RLS, `caseSensitive`, defaults, generated columns, domains-with-checks, partial-unique indexes, partition-child `.drop()` — §3.5) is realized faithfully on every target or **fails closed** (`DIALECT_UNSUPPORTED` / `VENDOR_OP_DENIED`, P5), never quietly dropped. `t.text({ caseSensitive: false })` is the cautionary contrast — it *changes* comparison results, so it is realized via each dialect's collation and **fails** where none exists; it is not a degrade-to-plain-`text` candidate. Which form each target received is **surfaced in the plan output** (native-vs-degraded, per dialect) and the degraded-leg count is the **fourth P11 ratchet counter** — you always know, per dialect, exactly what you got. Adding a dialect is adding realization legs to the generated table (P7), never touching an author script.

### Style & naming laws (condensed lawbook)

- **S1/S2 (args):** positional = identity names only (entry functions, selectors, detached parentless refs); zero-or-one options object per terminal; no boolean/enum positionals (`.d.ts` lint; registry membership for the `.comment` exception).
- **S3 (schema):** stated once on entry; inherited; overrides deleted.
- **S4 (one spelling):** two-tier census with pinned units of account; registry-licensed pairs only.
- **S5 (one subject):** the pinned return matrix, generation counters, `HANDLE_STALE`/`HANDLE_DROPPED`. A **widened selector's parent-returning terminals return the widened parent** (a `/pg` chain never silently narrows back to core).
- **S6 (no magic strings):** sentinels are symbols/builders; dotted strings never parse; `__zsExpr` reserved in every JSON position.
- **S7 (values):** `DmlValue<C>` everywhere; pinned per-position builders; the discriminated wire wrapper.
- **S8 (position-scoped builders):** the lambda parameter type is the primary story of legality; residues are declared validate refusals, enumerated per position.
- **S9 (casing):** camelCase author tokens; wire tags equal surface tokens; cast targets are the ColType tokens.
- **S10 (boundary):** core exports only landed-proof **portable or transparent-degradable (P12)** constructs, at op/node/option(-combination) granularity; value-level widening only; CI walk against the one generated dialect table, which carries the three-class disposition per row.
- **S11 (single source):** everything derives from the one schema + one compiled recorder artifact; snippet taxonomy over all docs.
- **S12 (nothing silent):** no module-global state, no discarded arguments, no plaintext credentials through any channel.
- **S13 (ladder):** tsc → record → validate → lower; per-construct rung tests.
- **S14 (raw):** closed inventory, all vendor-gated, all credential-linted, ratcheted against the committed baseline.
- **S15 (module):** exactly one module shape; no dialect parameter.
- **N1–N8 (names):** bare camelCase noun entries; verb terminals recording the SQL they name; property changes are `setX`/`dropX`; type factories match wire tokens; positive-adjective flags; SCREAMING_SNAKE error codes with `suggested_fix`; handles are inert nouns; constructors never record.

### Canonical shared vocabulary (consistency resolutions applied once, used everywhere)

These names were drifting between domains; they are pinned here and used identically in every section below:

- **Builder lattice** (owned by the expressions domain; the *only* public builder type names):
  `DefaultBuilder` (not callable; `fn`, `case`) / `DefaultBuilderWithPg` · `CheckBuilder` (callable `c(col)`) / `CheckBuilderWithPg` · `RowBuilder` · `ConflictUpdateBuilder` (RowBuilder + `c.excluded(col)`) · `QueryExprBuilder` (callable, + two-arg `c(table, col)`) → `SelectExprBuilder` / `HavingBuilder` (+ `c.agg`) · `TriggerWhenBuilder` (+ `c.old`/`c.new`) · `DomainValueBuilder` (the `(v)` form — `v` *is* the value expression; no column accessor exists) · **`IndexExprBuilder`** (callable, **immutable-only** `c.fn` subset — used by index expression elements, partial-index `where`, and exclusion `where`) · **`GeneratedColumnBuilder`** (callable, immutable-only, no agg/old/new — used by `.generated`). The volatility partition (which `c.fn` members are immutable) is a generated-table fact, stated once.
- **Selector types** follow the `<Kind>Ref` convention: `ColumnRef`, `UniqueRef`, `CheckRef`, `ForeignKeyRef`, `IndexRef`, `PolicyRef`, `TriggerRef`, `ExclusionRef`, **`PartitionRef`**.
- **Value constructors:** `decimal("0.5")` and **`byteValue(new Uint8Array(...))`** (one name each; `bytes()` does not exist as a value constructor — it would collide with the `t.bytes()` type token).
- **Sort direction key is `order`** (`"asc" | "desc"`) on every ordered item — index elements *and* view `orderBy` items (`{ by, order? }`); the wire OrderItem field renames to match; `order: "asc"` is elided in canonical form. The ordering **subject** key is an **owned drift**, registered as two distinct item shapes: index/exclusion elements say `column`/`expr` (physical elements that also carry opclass/collation), view `orderBy` says `by` (a projection reference) — deliberately not unified, and owned here rather than left to be discovered.
- **Cast is `.cast({ to })`** with the generated ColType scalar tokens; the token is `"json"` (there is no `"jsonb"` token and no `t.jsonb()` — `t.json()` renders jsonb on PG).
- **bigint is refused at tsc in every value position** (outside `Scalar`); the suggested path is `decimal()`. No auto-lift arm exists.
- **Every entry-level noun's `.drop()` returns a create-only continuation** (`DroppedTableHandle`, `DroppedViewHandle`, `DroppedEnumHandle`, `DroppedDomainHandle`, `DroppedSequenceHandle`, …) — *entry-level* meaning the objects entry functions construct; **sub-object** `.drop()` returns the parent (P2 matrix) — with `HANDLE_DROPPED@record` as the alias backstop.
- **The comment family** (`.comment(text | null)` terminals on table/column/index/view/enum/domain/sequence handles) is a **core-candidate** under the P6 proof gate (SQLite has no `COMMENT ON`; the engine-side story must land). Pre-declared fallback: ALL comment terminals move to the pg-widened handles. Constraint `.comment` is already known-vendor and starts on the pg-widened selectors. **There is no free `comment()` op** — the terminals are the one spelling (one producer per op kind).
- **Structural discriminants everywhere on the surface** — no `kind:` tags in authored payloads: partition bounds `{ from, to } | { in } | { modulus, remainder } | { default: true }`; `partitionBy: { range, whenUnsupported? } | { list, whenUnsupported? } | { hash, whenUnsupported? }` (the P12 affirmation slot — `whenUnsupported: "collapse"`); index/exclusion elements `{ column, … } | { expr, … }`; grant/revoke targets `{ tables: [...], schema? } | { schemas: [...] }`. All desugar to the internally-tagged IR.
- **Qualification is structural, never parsed** — including function references: trigger `execute: { name: "reject_sector_change", schema: "zeroship" }`, never `"zeroship.reject_sector_change"`.
- **The domain-check builder is `DomainValueBuilder`** and the one spelling is `(v) => v.in([...])`; `c("VALUE")` and `c.value()` do not exist.
- **Faceted-`ColumnDef` refusal is tsc, uniformly**: `t.encrypted({ of })`, `setType.to`, domain `as:`, sequence `as:` all take `ColumnDef<false>` (the phantom brand), with `OP_INVALID@record` as the untyped-JS backstop.
- **"Tier" is disambiguated once** <!-- Added in round 3: four senses were in circulation. -->: the P1 census has two **census tiers** (producer identity / writer inventory); the dialect table has three **dispositions** (portable / transparent-degradable / vendor — never "tiers"); "the `/pg` tier" names the vendor *package* (Phase 3); §6's "capability-scoped tier" is a hypothetical future fourth disposition. Where this document says "tier" unqualified about dialect scope, read "disposition".

### The final export inventory (post-resolution)

```ts
// core — @zeroship/migrate  (landed proofs only: portable + transparent-degradable, P5/P12)
import {
  table, view, enumType,
  t, lit, decimal, byteValue,
  dialect,
  fromDb,
  minValue, maxValue,        // partition-bound sentinels — CORE, one home (§3.5 create-time bounds)
} from "@zeroship/migrate";
// NOT core (deleted or moved): and/or/not/membership/notMembership (chain-only now),
// interval (structured Duration replaces it),
// nextval (→ /pg), comment (free op deleted), check/index free constructors, c.col,
// colTypeFromDbField + lintDeterminism (→ @zeroship/migrate/toolchain — non-authoring, examined below).

// vendor — @zeroship/migrate/pg
import {
  pgTable, pgView, pgMaterializedView, pgEnumType,
  pgT,                       // the vendor-widened lexicon (pgT.domain, pgT.*().identityAlways())
  domain, sequence, nextval,
  schema, extension, role, grant, revoke, dropOwnedBy,
  createFunction, dropFunction,
  raw, rawSelect, secretRef,
} from "@zeroship/migrate/pg";
```

**The three non-recording survivors, examined.** <!-- Added in round 2: they no longer ride through the constitution unreviewed. --> The shipped surface carries three symbols that record no ops; each gets a disposition. **`fromDb(dbField)` stays core as a ColumnDef factory** — the `@zeroship/db` bridge lifting a db schema field onto the identical `ColType` path a hand-written `t.*` column takes (nullability carried over, `.required()` → `.notNull()`; table/column names never bound to the live schema; returns a chainable immutable `ColumnDef`). Census: a tier-2 value factory like `t.*` (no op kind; one writer per slot); dialect story: exactly that of the ColType tokens it emits; misuse rung: non-storage db field shapes (`object`/`union`/`literal`/…) refuse with the structured `UnsupportedColType` error at **record** (P9). **`colTypeFromDbField` leaves the authoring surface** for `@zeroship/migrate/toolchain`: it is `fromDb`'s single-source internal reduction and the fold/codegen hook — exporting it beside `fromDb` on core would be a second public spelling of the same bridge (a tier-2 census violation). **`lintDeterminism` moves to `/toolchain` too**: a warn-only whole-source nondeterminism steer (`Date.now()`/`Math.random()`/`new Date()`/…, over-flagging by design, never a hard reject) consumed by the record path and CI — not an authoring symbol. `/toolchain` exports sit outside the census's units of account (they produce no IR) but inside the `.d.ts` positional lint.

---

## 3. The concrete redesigned surface, domain by domain

All snippets follow the S11 taxonomy: unmarked blocks are `compile-and-record`; refusals are `expect-error(CODE@rung)`; shipped-surface blocks are `historical`.

### 3.1 Tables, the type lexicon, columns, per-intent alteration

**Grammar instance:**

```
table(name, { schema? })          → TableHandle
  .create({ ... })                → TableHandle
  .drop({ ifExists?, cascade? })  → DroppedTableHandle     (create-only continuation)
  .rename({ to, ifExists? })      → TableHandle            (addresses the NEW name; old aliases go HANDLE_STALE)
  .setOptions({ ... })            → TableHandle
  .comment(text | null)           → TableHandle            (registry-enumerated S1 exception; core-candidate family)
  .partition(name)                → PartitionRef            (P12 transparent-degradable — §3.5)
     .create({ from/to | in | modulus/remainder | default: true }) → TableHandle
     .drop({ ifExists? })         → TableHandle             (SEMANTIC — realized per target, §3.5)
  .column(name)                   → ColumnRef
     .add({ type, ifNotExists? }) → TableHandle
     .drop({ ifExists? })         → TableHandle
     .rename({ to })              → ColumnRef              (post-rename; NO restated type — the fold supplies it)
     .setType({ to, using? })     → ColumnRef              (using: (c: RowBuilder) => Expr; to: ColumnDef<false>)
     .setNotNull() / .dropNotNull()          → ColumnRef
     .setDefault({ value }) / .dropDefault() → ColumnRef
     .comment(text | null)        → ColumnRef
```

**`create({...})` — core args (exclusions live only on the pg-widened payload):**

```ts
interface CreateTableArgs {
  columns: Record<string, ColumnDef>;
  /** undefined = policy default · null = explicit no-PK · string[] = explicit/composite */
  primaryKey?: string[] | null;
  options?: TableRuntimeOptions;         // same bag setOptions takes (create/alter pair, stated)
  uniques?:     Array<{ name: string; columns: string[] }>;
  checks?:      Array<{ name: string; expr: (c: CheckBuilder) => Expr }>;
  foreignKeys?: Array<{ name: string; columns: string[];
                        references: { table: string; columns: string[] };
                        onDelete?: RefAction; onUpdate?: RefAction }>;
  indexes?:     IndexDefLiteral[];
  /** P12 transparent-degradable (§3.5): native on PG; affirmed collapse elsewhere. */
  partitionBy?: { range: string[]; whenUnsupported?: "collapse" }
              | { list: string[];  whenUnsupported?: "collapse" }
              | { hash: string[];  whenUnsupported?: "collapse" };
  ifNotExists?: boolean;
  // NO schema field (S3). NO exclusions (vendor — pg-widened create only).
}
```

**The one runtime-options terminal** (the three shipped positional toggles die):

```ts historical
table("posts").softDelete(false);  table("posts").withVersioning(true);  table("posts").strictness("strict");
```
```ts
table("posts").setOptions({ softDelete: false, versioning: true, strictness: "strict" });
```

**The type lexicon — one name per type, factory == wire token (N3/N5).** `t.integer()` and `t.float()` are deleted; `t.int()` is THE integer spelling; `t.real()` = float4 and `t.double()` = float8 (the wire token `"float"` renames to `"double"`; `"bool"` → `"boolean"`; `"bytea"` → `"bytes"`). Payload numbers are named: `t.char({ length: 3 })`, `t.numeric({ precision: 12, scale: 2 })`, `t.vector({ dimensions: 1536, metric: "cosine" })`. Named-type references carry their own identity opts (`t.enum("order_status", { schema: "zeroship" })` — the wire enum arm gains `schema?`). **`t.domain` leaves core**: the reference is `pgT.domain(name, { schema? })` on the `/pg`-widened lexicon `pgT`, which delegates to the same core factory objects (census-legal) and also carries `pgT.bigInt().identityAlways()` (GENERATED ALWAYS — vendor); core keeps only `.autoIncrement()` (the portable three-leg identity); `.identity()` is deleted. Cast targets are exactly the scalar ColType tokens, consumed as `.cast({ to: "bigInt" })`.

**Defaults — every form under the one `DmlValue<DefaultBuilder>` rule:**

```ts
t.bigInt().default(0)
t.text().default("pending")
t.timestamp().default(null)                                  // DEFAULT NULL, recorded explicitly
t.json().default({ max_sockets: 4, egress_ceiling_ratio: 0.85 })   // full Json domain, canonical floats
t.uuid().default((c) => c.fn.genRandomUuid())
t.timestamp().notNull().default((c) => c.fn.now())           // { fn: "now" } spelling DELETED
t.numeric({ precision: 38, scale: 9 }).default(decimal("0.000000001"))
t.bytes().default(byteValue(new Uint8Array([0x00])))
t.bigInt().notNull().default(nextval("orders_id_seq", { schema: "zeroship" }))  // /pg import required
```
```ts expect-error(TS2345@tsc)
t.bigInt().default(10n);          // bigint is outside Scalar — use decimal("10")
```
```ts expect-error(TS2349@tsc)
t.text().default((c) => c("other_col"));   // DefaultBuilder has no call signature — column-in-DEFAULT unrepresentable
```

**`t.encrypted` — one spelling, nothing silent** (the shipped silent facet-drop dies at tsc via the `ColumnDef<Faceted>` phantom brand):

```ts
t.encrypted({ of: t.text() }).notNull()      // facets go on the WRAPPER
```
```ts expect-error(TS2322@tsc)
t.encrypted({ of: t.text().notNull() });     // ColumnDef<true> not assignable to ColumnDef<false>
```

**`.generated({ as: (c: GeneratedColumnBuilder) => Expr, virtual? })`** — the expression moves into the options object; the builder is callable but immutable-only (a volatile function in a generated column is a tsc error).

**Registered sugar pairs** (surface-registry entries; each with pinned desugaring + byte-identical-IR test + collision test): `t.id({ prefix? })` ↔ explicit column + `primaryKey` (`PK_DECLARED_TWICE`); `t.ref("orgs")` ↔ `foreignKeys[]` (`FK_DECLARED_TWICE`); `.unique()` facet ↔ `uniques[]` (`UNIQUE_DECLARED_TWICE`); `.primaryKey()` facet ↔ `primaryKey: [col]` (`PK_DECLARED_TWICE`). Constraint-implying facets are **create-time only** — on `.column().add()` they refuse at record with a steering fix (the shipped behavior invented constraint names silently). Two **value-shape** pairs are registered in the same surface registry (the census's unit of account includes them; nothing shorthand-shaped lives outside it): `IndexElement` bare string ↔ `{ column }` (§3.3) and `InsertArgs.rows` single row ↔ one-element array (§3.7) — each with the same byte-identical-IR test.

**Before → after (verified line-by-line against the shipped surface):**

```ts historical
t.integer();  t.int();                          // two spellings, one wire "int"
t.real();  t.float();                           // "aliases" that are float4 vs float8
t.numeric(12, 2);  t.char(3);                   // which positional is which?
t.timestamp().default({ fn: "now" });           // collides with objects-as-JSON
t.json().default({ ratio: 0.5 });               // THROWS — integers-only JSON defaults
t.encrypted(t.text().notNull());                // silently discards the notNull
t.domain("billing_period");                     // PG-only object on the CORE lexicon
table("users").column("bio").rename({ to: "biography", type: t.text() });  // restate the type to rename
table("users").column("age").setType(t.bigInt());                          // guide §5 — invalid as written
```
```ts
t.int();
t.real();  t.double();
t.numeric({ precision: 12, scale: 2 });  t.char({ length: 3 });
t.timestamp().default((c) => c.fn.now());
t.json().default({ ratio: 0.5 });
t.encrypted({ of: t.text() }).notNull();
pgT.domain("billing_period", { schema: "zeroship" });        // /pg import, tsc-enforced
table("users").column("bio").rename({ to: "biography" });    // the fold supplies the type
table("users").column("age").setType({ to: t.bigInt(), using: (c) => c("age").cast({ to: "bigInt" }) });
```

### 3.2 Constraints — one grammar for PK · unique · check · FK · exclusion

**The shipped mess:** three ways to add a CHECK, two for an FK, a silent PK precedence merge ("explicit wins"), a kind-agnostic `constraint(name)` drop selector (the subject changes grammar between add and drop), `t.ref` recording a *second FK wire home* in the column-type slot, an always-refused `IrConstraintKind::Pk`, and `NOT VALID` — which the engine's own lint *advises* — unauthorable.

**The one grammar:** create-time = plain object literals inside `create({...})` (records the CREATE TABLE op's inline constraints); alter-time = `table(x).<kind>(name).add({ payload })` (records ADD CONSTRAINT); drop = the **same typed selector** `.drop({ ifExists? })`. Deleted: `addCheck`, `addForeignKey`, free `check()`, `constraint(name)`, `references.schema`, `IrConstraintKind::Pk`, `ColType::Ref`'s FK semantics.

```ts
// alter-time — the ONE spelling each
table("orders").check("orders_qty_positive").add({ expr: (c) => c("qty").gt(0) });
table("users").unique("users_email_key").add({ columns: ["email"] });
table("line_items").foreignKey("line_items_order_fkey").add({
  columns: ["order_id", "tenant_id"],
  references: { table: "orders", columns: ["id", "tenant_id"] },   // references.schema DELETED — it could only echo
  onDelete: "restrict", onUpdate: "cascade",
});
table("orders").check("orders_qty_positive").drop({ ifExists: true });   // same subject, whole lifecycle
```

**Primary key is create-time only.** `IrConstraintKind::Pk` — reachable only through the always-refused alter path — is deleted from the wire (P9 rule 4). Re-keying an existing table is a declared engine-capability gap with a standing re-open rule. The `.primaryKey()` facet is licensed sugar; facet + explicit `primaryKey` (including `primaryKey: null`), or the facet on 2+ columns, is `PK_DECLARED_TWICE@record` — the shipped silent "explicit wins" merge is exactly the class P8 keeps dead.

**`t.ref` — the duplicate wire home dies.** The column-type slot loses its FK meaning (renamed to the truthful deferred type `{ typeOf: { table, column } }`, fold-resolved); the FK edge is recorded **only** as the canonical `Fk` constraint node via the pinned desugar (name `<table>_<column>_fkey`, target `["id"]`, no actions; anything more is the longhand's job). Declaring both over one column is `FK_DECLARED_TWICE`.

**Vendor depth (`/pg`)** — `deferrable`/`initiallyDeferred` (no MySQL leg) and `notValid` (PG-only) are vendor **options** on the widened selectors; `ValidateConstraint` is a new PG-only op — the engine's `CONSTRAINT_NOT_VALIDATED` lint finally gives authorable advice:

```ts
import { pgTable } from "@zeroship/migrate/pg";
pgTable("usage_aggregates", { schema: "zeroship" }).foreignKey("usage_aggregates_metric_fkey").add({
  columns: ["metric"], references: { table: "billing_metrics", columns: ["metric"] },
  deferrable: true, initiallyDeferred: true,
});
// online constraint adoption — previously unauthorable
pgTable("line_items").foreignKey("line_items_order_fkey")
  .add({ columns: ["order_id"], references: { table: "orders", columns: ["id"] }, notValid: true })
  .foreignKey("line_items_order_fkey").validate();
```

**Exclusion constraints are vendor, whole-op**, homed on `PgTableHandle` only (core `TableHandle` has no `.exclusion`; core `CreateTableArgs` has no `exclusions` field — the S10 walk checks core option fields). Elements use the **same grammar as index elements** (consistency resolution — no in-band string-vs-lambda `target` field):

```ts
pgTable("reservations").exclusion("reservations_no_overlap").add({
  using: "gist",
  elements: [{ column: "room_id", operator: "=" }, { column: "during", operator: "&&" }],
  where: (c) => c("status").ne("cancelled"),          // (c: IndexExprBuilder) — immutable-only
  deferrable: true,
});
```

**Drop carries kind on the wire.** `Op::DropConstraint` gains a required closed `kind: unique | check | fk | exclusion` — one producer, four typed doors; destructive-plan gating keys off it; apply fails closed on a live-catalog kind mismatch.

**Domain CHECK — the magic string dies:**

```ts historical
domain("account_state").create({ as: t.text(), check: (c) => membership(c("VALUE"), ["active", "past_due", "suspended"]) });
// c("VALUES") — one typo — records a broken colRef silently
```
```ts
domain("account_state", { schema: "zeroship" }).create({
  as: t.text(),                                   // ColumnDef<false> — faceted input is a tsc error
  check: (v) => v.in(["active", "past_due", "suspended"]),   // (v: DomainValueBuilder) IS the value; no column accessor exists
});
```

`RefAction` dialect truth is token-granular: `"setDefault"` is marked `pg+sqlite` in the generated table (InnoDB rejects `SET DEFAULT`) and refuses on a MySQL target at validate.

### 3.3 Indexes — one bag, one grammar

**The shipped surface is the census antipattern made flesh:** chain modifiers (`.using/.include/.with/.only`) AND the same keys in the `.add` bag, merged `{ ...indexDraft, ...args }` with args silently winning; a positional boolean `.only()`; a five-spelling element union under a key named `columns` even when the element is an expression; a method enum missing the `hash` its own doc promises while carrying the SQLite engine name `fts5`; partial `where` "refusing fail-closed" at **lower**; and `drop({ unique: true })` forcing the author to restate a catalog fact.

**The entire `IndexRef`:**

```ts
export type IndexElement =
  | string                                        // registry-licensed shorthand for { column: name } (§3.1 value-shape pairs)
  | { column: string; order?: "asc" | "desc" }
  | { expr: (c: IndexExprBuilder) => Expr; order?: "asc" | "desc" };

export interface IndexAdd { on: IndexElement[]; unique?: boolean; ifNotExists?: boolean; }

export interface IndexRef {
  add(args: IndexAdd): TableHandle;
  drop(args?: { ifExists?: boolean }): TableHandle;       // drop.unique DELETED — fold-derived
  comment(text: string | null): TableHandle;
}
```

`IndexExprBuilder` is immutable-only — a volatile function (`now`, `currentSetting`) in an expression element or partial `where` is a **tsc** refusal, not a PG apply error.

**Vendor depth (`/pg`)** — `pgTable(...).index(name)` returns `PgIndexRef`, a type-level widening over the **same producer function**, whose parent-returning terminals return **`PgTableHandle`** (S5's widened-parent rule):

```ts
export type PgIndexMethod = "hash" | "gin" | "gist" | "spgist" | "brin" | "ivfflat" | "hnsw";
// btree is NOT a token: omitted `using` IS btree (one wire shape for the default). fts5 DELETED.

export interface PgIndexAdd extends IndexAdd {
  on: PgIndexElement[];               // + nulls: "first"|"last", opclass, collation per element
  using?: PgIndexMethod;
  where?: (c: IndexExprBuilder) => Expr;   // partial index — the flagship two-of-three option
  include?: string[];
  with?: PgIndexStorage;              // closed per-method param map, generated; mismatch = record error
  only?: boolean;
  nullsNotDistinct?: boolean;
  concurrently?: boolean;             // the ONE authored concurrency knob (the prior proposal's core `online` hint is dropped)
}
export interface PgIndexRef extends IndexRef {
  add(args: PgIndexAdd): PgTableHandle;
  drop(args?: { ifExists?: boolean; concurrently?: boolean }): PgTableHandle;
}
```

```ts historical
table("docs").index("i").using("gin").add({ columns: ["body"], using: "brin" });  // records brin, silently
table("orders").index("orders_email_uq").drop({ ifExists: true, unique: true });  // restated catalog fact
```
```ts
pgTable("audit_log", { schema: "control" }).index("audit_log_time_brin").add({
  on: ["occurred_at"], using: "brin", with: { pagesPerRange: 64 }, concurrently: true,
});
table("orders").index("orders_email_uq").drop({ ifExists: true });   // same apply-path safety, zero ceremony
```

**Admission ledger highlights:** `unique`, per-element `order`, expression elements (MySQL functional-index proof required), `ifNotExists` = core; `where`, `nulls`, `using`, `include`, `with`, `only`, `nullsNotDistinct`, `concurrently` = `/pg`. <!-- Amended in round 2: `where` gets exactly one home; the index family is out of P12. --> **`where`'s home is decided: `/pg`, fail-closed** — partial indexes are PG+SQLite but not MySQL, and the option is deliberately **not** P12-degradable, because transparency flips per option-combination: `unique: true` × `where` changes **which inserts succeed** (collapsing the `where` enforces uniqueness on more rows; skipping the index enforces it on none), so the whole option stays realization-required rather than carrying a per-combination trap. Consistently, `IndexAdd`/`PgIndexAdd` carry **no `whenUnsupported` slot** (P12): a `/pg` construct is realization-required by the import boundary's own honesty. §6's open tension 1 records only a possible future **re-home** of the two-of-three options into a capability-scoped tier (a generated-table data change) — the present disposition is settled, not open. What this design fixes unconditionally is that the refusal moves from lower to **validate**. Inline `create({ indexes })` records `IrIndex[]` inside the createTable op — a different op kind than standalone `createIndex`, stated so the census never has to "catch" it; both consume the same element types.

### 3.4 The expression algebra, values, and the `dialect()` escape

**The builder lattice** (§2 canonical vocabulary) replaces the shipped one-builder-everywhere model; the lambda parameter type is the primary story of legality. At tsc: aggregates in a CHECK, column refs in a DEFAULT, qualified refs in a CHECK, `OLD`/`NEW` outside a trigger, volatile functions in an index position, and `c.pg.*` without the `/pg` import are all **unrepresentable**. Declared validate residues (enumerated, each tested): qualified-ref table not in the FROM set (`QUALIFIED_REF_UNKNOWN_TABLE`), hand-forged agg/rowRef out of context, vendor nodes under a confined capability.

**Qualified column refs — the join-ON fix:** `c("orders", "customer_id")` — two positional identity names producing a qualified colRef (the wire `colRef` gains optional `table`). The two-arg overload exists only on `QueryExprBuilder` and below. Dotted strings refuse at **record** with a steering fix; the strict ident gate at the quoting seam stays as defense in depth.

**The portable chain** (one node each; free `and/or/not/membership/notMembership` are deleted — chain-only): `.eq .ne .lt .le .gt .ge .and .or .not .isNull .isNotNull .isTrue .isFalse .add .sub .mul .div .mod .concat` · **`.in([...])` / `.notIn([...])`** → the new portable `inList` node (replaces `membership`/`notMembership`/`c.pg.eqAnyArray`/`c.pg.neAllArray` and the `pgArrayMembership` wire node — the renderer may still emit `= ANY(ARRAY[...])` on PG; the *node* is portable and portably named) · `.between(low, high)` · `.like(pattern, { escape? })` / `.notLike` (core-candidate; the SQLite case-sensitivity leg is the make-or-break proof) · `.isDistinctFrom` / `.isNotDistinctFrom` · `.cast({ to })`. Chain `.matches()` and `.columnSize()` are deleted — regex and `pg_column_size` live only under `c.pg.*`.

**`c.fn.*`:** carried over `lower upper trim length abs coalesce nullif concatWs splitPart now genRandomUuid`; new candidates `round floor ceil substr replace dateAdd dateSub extract` — each admitted only with a landed three-dialect proof. **`c.fn.currentSetting`/`c.fn.currentUser` leave the portable namespace** → `c.pg.currentSetting(name, { missingOk? })` (the naked positional boolean dies) / `c.pg.currentUser()`, with dedicated `pgCurrentSetting`/`pgCurrentUser` wire nodes.

**CASE is grammar, on the builder:**

```ts historical
(c) => c.fn.case([[c("n").gt(0), lit("pos")]], lit("nonpos"))          // tuples + positional else
```
```ts
(c) => c.case({ when: [{ when: c("n").gt(0), then: "pos" }], else: "nonpos" })
```

**Durations & EXTRACT:** one structured `Duration` (`{ years?, months?, days?, hours?, minutes?, seconds? }`, integers, canonical key order) feeds portable `c.fn.dateAdd/dateSub` (the `dateShift` node) and the vendor interval value `c.pg.interval({ days: 7 })` (replacing the deleted core `interval` export and its HH:MM:SS-only parser). EXTRACT splits at option granularity: `c.fn.extract({ field: "year" | … | "dow", from })` portable (each field its own admission subject; `dow` pinned to 0=Sunday) vs `c.pg.extract({ field: "quarter" | "week" | … })` — disjoint field sets.

**Aggregates:** `c.agg.count() | count(e, { distinct? }) | sum | min | max | avg` → the portable `agg` node, reachable only from `SelectExprBuilder`/`HavingBuilder`.

**Values:** `DmlValue<C>` with pinned per-position builders — `RowBuilder` for `update.set`/`backfill.set`; **`ConflictUpdateBuilder`** for `onConflict.doUpdate` (so `c.excluded(col)` → the `excludedRef` node — the whole point of the position); `DefaultBuilder` for `setDefault` and insert row values. Scalar numbers: integers `|n| < 2^53` and finite doubles with pinned shortest-round-trip canonical serialization; `NaN`/`Infinity` refuse at record. The shipped `{ decimal: string }` carrier, `{ fn: "now" }` default, and `Date.now`-as-value acceptance are all deleted (record-time steering fixes); `decimal()`/`byteValue()` are the branded constructors; bigint is a tsc error everywhere.

**`dialect({ default?, pg?, sqlite?, mysql? })` — the one Layer-2 escape.** Value positions only (whole PG-only *ops* are vendor tier, never values with legs). At least one leg; all legs the same value kind; no nesting; legs recorded **in full** in the checksummed IR as the `dialect` node with canonical leg order. **No `reason` field** — the divergence is fully structured and visible; the gate is the **ratchet** (the leg count is one of the **four** ratcheted counters, P11). Scope math at validate: legs present ∪ (all dialects if `default` present); off-scope targets refuse with `DIALECT_UNSUPPORTED` naming the missing leg; under the confined profile any scope-narrowing value is `VENDOR_OP_DENIED` — dev parity (`pnpm dev` = SQLite) holds by construction.

**Before → after (selected):**

```ts historical
table("plans").update({ set: { name: (c) => lit("Professional") } });          // scalar SET throws today
.join("inner", "customers", (c) => c("orders.customer_id").eq(c("customers.id")))  // ONE ident; fails at lower
(c) => membership(c("status"), ["active", "past_due"])                          // portable costume, pgArrayMembership wire
interval("7 days")                                                              // throws — HH:MM:SS-only parser
```
```ts
table("plans").update({ set: { name: "Professional" }, where: (c) => c("id").eq("pro") });
.innerJoin("customers", { on: (c) => c("orders", "customer_id").eq(c("customers", "id")) })
(c) => c("status").in(["active", "past_due"])
(c) => c("expires_at").le(c.fn.dateAdd(c("created_at"), { days: 7 }))
```

### 3.5 Partitioning — one subject, four honest verbs (a P12 split)

<!-- Rewritten in round 2: transparency is a proof obligation over the full constraint surface + insert-acceptance, and the MySQL-native claim was factually wrong. --> Partitioning splits along the **P12** line because it is the one v1 construct with a provable path to absence-tolerance: a partitioned table *can* behave logically identically to a plain one — but that is a **proof obligation over the full constraint surface plus insert-acceptance plus DDL-acceptance** (P12's equivalence definition — the mirror guard below exists to satisfy its DDL-acceptance clause), never an assumption. Four validate rules close the residue — each **checked, never asserted** (each an `expect-error` snippet in the generated guide) — plus one apply-rung mirror guard for the single fact that is data-dependent and therefore unknowable at validate: <!-- Amended in round 4: rule 3 (bound well-formedness) and rule 2's two-valued-key clause added; the populated-default mirror guard added. -->

1. **Key coverage** — `PARTITION_KEY_COVERAGE@validate`: **every unique-enforcing entry** on the table must include all partition-key columns (PG's own rule for partitioned tables): the PK, every `uniques` entry, every `unique: true` index — create-time `indexes`, standalone `index(n).add({ unique: true })`, and alter-time `unique(n).add()` alike — and `/pg` exclusion constraints (PG applies the same rule). <!-- Amended in round 3: was PK + uniques only, create-time only — a non-covering unique index passed, applied on the collapsed target, and failed DDL on native PG: the exact lie this rule names. --> It is a **fold-checked invariant over every op targeting a partitioned subject**, not a create-time-only check — a later migration adding a unique constraint or index to a partitioned table re-checks coverage. Refused for the **recording**, not per-target — otherwise one recording would be legal on collapsed SQLite and illegal on native PG, and "one recording, no branching" would be a lie.
2. **Collapse admissibility** — affirming `whenUnsupported: "collapse"` requires a bound set that is **checked total, never asserted total** <!-- Rewritten in round 3: "hash partitioning (total by construction)" was factually false — a partial remainder set is fully expressible and re-opens the insert-acceptance divergence this rule exists to forbid. -->: `PARTITION_BOUNDS_NOT_TOTAL@validate` requires, for range/list, a `{ default: true }` child; for hash, **verified remainder coverage** — the declared `{ modulus, remainder }` congruence classes must cover every residue in `0 .. lcm(moduli) − 1` (decidable arithmetic over the declared set; the factor-chain well-formedness this arithmetic presupposes is rule 3's **checked** obligation, not an assumption — a non-chain set like moduli `{2,3}` passes the lcm coverage yet fails native DDL, and rule 3 refuses it). Hash has **no escape hatch**: PG itself forbids a `DEFAULT` partition on hash, so coverage is the only path. Without totality, an out-of-bound insert **errors on native PG but succeeds on the collapsed target**: an insert-acceptance divergence, exactly what P12's absence-equivalence proof forbids. **The evaluation point is pinned**: totality is a fold invariant checked at the **end of every migration** whose ops change the child set (or the `partitionBy`) of a collapse-affirmed parent — create-time and every later migration alike, in **both directions** (a `down` whose end-of-migration fold state leaves a collapse-affirmed parent non-total refuses with the same code); mid-migration transients (drop-then-recreate the default child in one migration) are legal because DDL applies transactionally. So the monthly-partition workflow — a later migration adding one bounded child — is checked and passes (the default child still totalizes), and a migration that drops the default child without replacing it refuses. Additionally, v1 collapse for **range** requires a **single-column partition key** — `PARTITION_COMPOSITE_KEY_UNSUPPORTED@validate` at the parent create, recording-level: the collapse realizations below pin single-column bound predicates, and composite-key bounds are lexicographic row-value comparisons with `minValue`/`maxValue` sentinel-expansion rules that v1 does not own (a future dialect-table data change; list is single-column by PG's own rule, and hash derives no predicate at all). **Collapse affirmation also requires a two-valued partition key** — `PARTITION_KEY_NULLABLE_UNDER_COLLAPSE@validate`, recording-level, keyed on the recording's own affirmation exactly like the hash-drop refusal <!-- Added in round 4: the collapse realizations are SQL predicates under three-valued logic; a nullable key made them silently diverge from native default-partition routing. -->: every partition-key column must be `notNull` in the fold, and `null` is refused as a list-bound member. Why: native PG routes a NULL-key row to the **default** partition (no range/list bound matches NULL), while the collapse realizations below are SQL predicates evaluated under **three-valued logic** — `NOT(bound)` is UNKNOWN on a NULL key — so a nullable key would make the default child's residual DELETE (and every auto-down that reaches it) silently skip exactly the rows the native `DROP TABLE` deletes, and a NULL list-bound member would poison both that child's own `IN`-delete and every sibling's residual conjunct. The requirement is a **fold-checked invariant** like rule 1, not create-time-only — a later `dropNotNull` on a partition-key column of a collapse-affirmed parent refuses with the same code — and it is nearly free: partition-key columns must appear in the PK whenever one exists (rule 1), and PK columns are already NOT NULL, so the refusal bites only PK-less nullable-key layouts — precisely the ones the realizations cannot honestly serve; a non-affirmed, PG-only recording keeps nullable keys freely (there is no collapse realization to lie). Under the guarantee, NULL-key insert-acceptance is *uniform*, not divergent: the native parent and the collapsed plain table carry the same NOT NULL on the key, so **both reject** NULL-key inserts — the Phase 4 NULL-key leg proves matching rejection on both forms — and every bound predicate and its negation in the tier split below is two-valued on every row that can exist. One stated consequence for hash, owned <!-- Added in round 4: previously an unstated expressiveness cliff. -->: **collapse-affirmed hash child sets are immutable in v1** — child drops refuse (`PARTITION_HASH_DROP_UNDERIVABLE`, tier split below), and any child add either breaks this rule's residue coverage or overlaps an existing congruence class and refuses under rule 3, so the standard modulus-split choreography (drop the modulus-2 child, add two modulus-4 children) is unavailable under collapse; a recording that needs hash repartitioning must not affirm collapse (PG-only) — an expressiveness cliff this design accepts rather than fabricating a portable hash predicate.
3. **Bound-set well-formedness** — `PARTITION_BOUNDS_ILL_FORMED@validate` <!-- Added in round 4: totality had a code, disjointness had none; an ill-formed set folds cleanly on collapse but fails native PG DDL — rule 1's lie verbatim. -->: for **every** partitioned recording (affirmed or not — the check is recording-level and dialect-independent), range sibling bounds must be pairwise non-overlapping; no list value may appear twice, neither across siblings nor within one bound; and the hash `{ modulus, remainder }` set must satisfy PG's own factor-chain rule — every remainder `< ` its modulus, every two moduli comparable by divisibility (the smaller divides the larger), and no two congruence classes overlapping (for `m1 | m2`, overlap iff `r2 ≡ r1 (mod m1)`). Native PG enforces all of this in `CREATE TABLE … PARTITION OF` DDL; a collapse target emits no DDL and would fold-record an ill-formed set cleanly — one recording legal on collapsed SQLite, illegal on native PG, and validate can express the check (it is decidable from the recording alone), so deferring it to apply would violate P9. Evaluated at the same end-of-migration fold point as rule 2's totality, in both directions. Well-formedness also underwrites the mirror guard below: with pairwise-disjoint sibling bounds, a row matching a *new* sibling's bound can natively sit only in the default child, so scanning the collapsed parent for matching rows is exactly native PG's default-partition scan.
4. **Child-drop realizability** — child `.drop()` deletes rows, so it is **semantic**, never degraded to a no-op; its per-target realization — including the `{ default: true }` child's residual predicate and the one recording-level hash refusal — is pinned in the tier split below.

**The populated-default mirror guard (apply rung — the one data-dependent arm).** <!-- Added in round 4: native sibling-create DDL is data-dependent; a collapsed createPartition that can never fail would pass dev-SQLite and fail prod-PG — a DDL-acceptance divergence outside the previous equivalence definition. --> Native PG's sibling-create DDL is **data-dependent**: `CREATE TABLE … PARTITION OF … FOR VALUES …` errors when the default partition already holds rows matching the new bound. A collapsed `createPartition` emits no DDL and, unguarded, could never fail — the monthly sibling would succeed on `pnpm dev` = SQLite yet refuse at apply on prod PG whenever strayed rows sit in the default: a DDL-acceptance divergence, now covered by P12's equivalence definition by name. The collapse realization of a bounded sibling `createPartition` therefore carries a **fail-closed apply-time guard**: inside the migration transaction, scan the collapsed parent for rows matching the new bound and error if any exist — the exact mirror of native PG's default-partition scan (equal by rule 3's disjointness: natively, such rows could live only in the default child). Data-dependent facts are unknowable at validate, so apply is this guard's honest rung per P9 (it joins §3.8's live-schema row); it is a *realization*, not a refusal — the recording stays legal, the plan output names the guard, and the monthly-partition workflow passes on collapse targets exactly where its native leg would (a default child free of matching strays). Both arms are named Phase 4 suite legs: strayed matching rows in the default → the sibling create errors on **both** forms; clean default → succeeds on both.

- **Core (transparent-degradable, P12):** `table(...).create({ partitionBy })` + the partition **children declared through the core selector**. **The native leg is Postgres only in v1** (`PARTITION BY` parent + `PARTITION OF` children); SQLite **and MySQL** collapse to a plain table under the author's affirmed `whenUnsupported: "collapse"` (omitted affirmation ⇒ `DIALECT_UNSUPPORTED@validate` on any non-native target, per P12).
- **`/pg` vendor (non-transparent physical ops):** `ATTACH`/`DETACH` of a *pre-existing* table, sub-partitioning, per-partition tablespaces — genuinely PG-specific operations with no portable meaning; these fail closed off-PG.

**Why MySQL is a collapse target, not a native leg.** An earlier draft claimed native inline MySQL partitioning; that claim does not survive MySQL's actual rules, on five axes, each fatal to *faithful* realization: (a) partitioned InnoDB tables support **no foreign keys in either direction** — a partitioned table with (or referenced by) `foreignKeys` is natively unrealizable; (b) MySQL has **no DEFAULT partition** (RANGE's only escape is `MAXVALUE`), so rule 2's default child cannot exist; (c) `RANGE COLUMNS` excludes **TIMESTAMP** (the documented workaround, `RANGE (UNIX_TIMESTAMP(col))`, is a different construct); (d) `VALUES LESS THAN` bounds are implicitly **contiguous** — `{ from, to }` gaps are inexpressible, and a gap row that native PG routes to the default child (or rejects) would be **silently accepted into the next MySQL partition**: a logical insert-acceptance divergence, i.e. non-transparent by P12's own gate; (e) `{ modulus, remainder }` is PG hash grammar — MySQL is `PARTITION BY HASH … PARTITIONS n` with no per-partition modulus, and a later `ADD PARTITION` fails outright once a `MAXVALUE` partition exists (it needs `REORGANIZE`). A native-MySQL leg for the genuinely expressible subset (contiguous `RANGE COLUMNS` over integer/DATE/DATETIME keys, no FKs anywhere on the table, no default child) is a pre-declared **future dialect-table data change**, gated on its own absence-equivalence proof (§6 open tension 2) — the engine never asserts a native form it cannot faithfully produce. <!-- Added in round 3: the future leg structurally interacts with rule 2. --> Note the pinned interaction with rule 2: because a collapse-affirmed range/list recording **must** carry the `{ default: true }` child (rule 2) and the MySQL-native subset **cannot** (axis b), the future native-MySQL leg can only ever serve recordings that never affirm collapse — e.g. PG+MySQL-only target sets — or hash recordings (whose totality is remainder coverage, not a default child); and its admission proof runs the same suite against the native and the degraded form, per P12's own clause.

The shipped surface's sins are fixed regardless: it changed grammatical subject three times (child-subject `partition(x).of(parent)`, parent-subject `detachPartition`, free-function `dropPartition`) with `forValues()` returning `void`. The redesign uses **one subject, four honest verbs** on the parent handle:

```ts
import { table, minValue } from "@zeroship/migrate";   // core: partitioning is transparent-degradable (P12); minValue's ONE home is core

const events = table("sandbox_events", { schema: "zeroship" });
events.create({
  columns: { id: t.uuid().notNull(), occurred_at: t.timestamp().notNull() },
  primaryKey: ["id", "occurred_at"],                    // rule 1: the PK covers the partition key
  partitionBy: { range: ["occurred_at"], whenUnsupported: "collapse" }, // P12: native on PG; plain table on SQLite+MySQL; { range }|{ list }|{ hash }; p.* deleted
});

events.partition("sandbox_events_2026_05")               // PartitionRef — inert selector
  .create({ from: ["2026-05-01 00:00:00+00"], to: ["2026-06-01 00:00:00+00"] })   // PG: CREATE TABLE … PARTITION OF; collapse targets: no DDL, fold-recorded bound
  .partition("sandbox_events_head").create({ from: [minValue], to: ["2026-01-01"] })
  .partition("sandbox_events_default").create({ default: true });   // rule 2: the default child makes the bound set total — collapse admissible

events.partition("sandbox_events_2026_05").drop({ ifExists: true });     // SEMANTIC: DROP TABLE child on PG; fold-derived bounded DELETE on collapse targets

// attach/detach of a PRE-EXISTING table are the NON-transparent VENDOR half (P12) — /pg only, fail-closed off-PG:
import { pgTable } from "@zeroship/migrate/pg";
const pgEvents = pgTable("sandbox_events", { schema: "zeroship" });   // fresh handle binds to the subject's CURRENT generation at first record (P2) — legal after the core ops above
pgEvents.partition("events_backfill").attach({ from: ["2026-01-01"], to: ["2026-07-01"] });  // ATTACH PARTITION of an EXISTING table
pgEvents.partition("sandbox_events_2026_05").detach({ concurrently: true });
```

**Tier split (P12), stated per verb.** <!-- Rewritten in round 3: the collapsed-child down story was self-contradictory (the mandated default child's auto-down was a refusing op on the very targets that mandated it), and the two collapse-only refusals contradicted rule 1's own recording-level rationale. Resolution: own the residual predicate, key the one surviving refusal to the recording, pin one down semantics. --> `partitionBy` + `.partition(name).create()` are the **transparent-degradable core** half: one recording; native on Postgres; on a collapse target the parent is a plain `CREATE TABLE` and a child `.create()` emits **no DDL** but is **fold-recorded with its bound** and, for a bounded sibling, carries the populated-default **mirror guard** at apply (rules above) — the plan output names the degraded leg (P12) and the guard, and later ops derive the bound. `.partition(name).drop()` is **core but semantic** — on the native leg it is `DROP TABLE` of the child, which deletes that child's **rows**; a no-op degradation would leave those rows visible in the collapsed parent, a query-result divergence, so it is never degraded away. On collapse targets it is **realized** as the fold-derived bounded delete — `DELETE FROM parent WHERE <bound>` (range → `key >= from AND key < to`, honoring `minValue`/`maxValue`, single-column by rule 2's v1 restriction — text-typed bounds in the pinned canonical format order lexicographically, a named suite leg; list → `key IN (…)`) — an owned Phase 4 equivalence obligation, surfaced in the plan but **not counted in the degraded-leg ratchet** (it is faithful *semantic realization*, not absence — the P11 counter counts absence only). **The `{ default: true }` child's collapse drop is realized too, not refused**: its predicate is the **residual predicate** — the conjunction of the negations of every sibling bound in the fold at that point — equal to the default partition's row set on native PG **given rule 2's two-valued-key guarantee** <!-- Amended in round 4: the unqualified "exactly equal" claim was false under SQL three-valued logic — NULL keys route to the native default but evaluate NOT(bound) to UNKNOWN; rule 2's NOT-NULL requirement is what makes the equality provable. -->: with every key column NOT NULL and no NULL list-bound members, each bound predicate and its negation evaluates TRUE/FALSE on every row, never UNKNOWN (without that guarantee the DELETE would skip the NULL-key rows native PG routes to the default — which is why rule 2 refuses nullable keys under collapse), and PG's own invariant — a sibling cannot be created while the default partition holds rows matching its bound, re-enforced on the collapse leg by the mirror guard — makes {rows matching no sibling bound} exactly the default child's row set; same derivation machinery as the range predicate, and a named suite leg (`PARTITION_DROP_DEFAULT_UNSUPPORTED` from the round-2 draft is **deleted** — refusing it would have made rule 2's mandated child un-droppable and un-down-able on the very targets that mandate it). **Exactly one refusal survives, and it is recording-level, not per-target** (rule 1's own rationale, applied uniformly): dropping a **hash** child on a recording whose `partitionBy` **affirmed collapse** is `PARTITION_HASH_DROP_UNDERIVABLE@validate` — the hash routing function is dialect-private, so no portable predicate exists, and the drop would also break rule 2's remainder coverage; the refusal keys on the recording's own affirmation (decidable from the recording alone — a PG-only, non-affirmed recording drops hash children freely as plain `DROP TABLE`). **So no core partition op's legality depends on the target set**: legality is a function of the recording (affirmation included); only *realization* varies per target, and that is surfaced in the plan output — the round-2 draft's two collapse-target-scoped refusals are gone. Dropping a child the fold never saw created **fails closed at lower on collapse realizations only** — there is no bound to derive; the native leg needs no bound and proceeds (`DROP TABLE`). In practice the arm is defense-in-depth against fold/live-catalog drift: a fold-unseen child can only arrive via the vendor `.attach()`, which already refuses at validate on any collapse target. `.attach()`/`.detach()` (of a *pre-existing* table) are the **`/pg` vendor** half — genuinely PG-specific, non-transparent, requiring the `pgTable`-widened handle. Every terminal returns the parent handle (`TableHandle` in core, `PgTableHandle` in `/pg`). The bound is bare structural keys — `{ from, to } | { in } | { modulus, remainder } | { default: true }` — with `minValue`/`maxValue` as **core** exported symbols (S6; §2 inventory — one home, everywhere), no `{ bound }` wrapper, no `p.*` builder. `.create` and `.attach` are **different ops** (different SQL, locking, existence semantics — P10; the prior proposal conflated them and is corrected). **Honest reversibility (the §3.7 down matrix instantiated), one pinned semantics:** child `.create` auto-downs to the semantic child drop above — **the drop op itself, realized per target** (native `DROP TABLE`; collapse = the bounded DELETE derived from the create op's own recorded bound, or the residual predicate for the default child) — never a fold-only no-op, because absence-equivalence is a property of the whole up→down path (leaving the collapsed child's rows behind while native PG drops them is a row-set divergence; the suite's down-path leg tests exactly this). Consequently the flagship example above is fully down-able on `pnpm dev` = SQLite: auto-down drops the default child (residual DELETE), the bounded children (bounded DELETEs), then the parent (plain `DROP TABLE`) — row-set-identical to the native down at both ends. The only non-auto-down-able child is the hash child on a collapse-affirmed recording (the refusal above): its create is `DOWN_UNDERIVABLE@record` steering to an explicit `down` or `autoDown: false`. `attach` auto-downs to the matching `detach` — a name-only inverse (`DETACH PARTITION` needs no bound, and parent + child both sit on the attach op itself) <!-- Added in round 4: attach had no stated down-family. -->; `detach` is irreversible without an explicit down (reattachment needs the bound, which detach doesn't carry); no fabricated auto-down. **Wire, pinned:** `createTable` gains `partitionBy` (the spec only — never children); the selector's `.create()` records the standalone core **`createPartition`** op — **one op kind whether recorded in the parent's own migration or any later one**, so the cross-migration monthly-partition workflow needs no special case (unlike §3.3's inline-index duality there is no inline child form in `CreateTableArgs`, so there is exactly one producer and one op kind — stated so the census never has to ask); new `attachPartition` op (vendor); `dropPartition` gains `parent`. Deleted: free `partition()`/`dropPartition()`, `TableHandle.detachPartition`, `PartitionOfHandle`, `forValues`/`asDefault` (the DSL's only void terminals), `p` + `PartitionBuilder`.

### 3.6 Views — SelectAst v2, matviews split, the honest raw escape

**Core structured views** (cross-dialect, per the engine's own validate comment) gain the algebra the guide pretended existed:

```ts
view("signup_rollup", { schema: "zeroship" }).create({
  as: (q) => q.from("users")
    .leftJoin("teams", { on: (c) => c("users", "team_id").eq(c("teams", "id")) })
    .where((c) => c("deleted_at").isNull())
    .groupBy(["plan"])
    .having((c) => c.agg.count().gt(10))
    .select(["plan",
             { as: "n", expr: (c) => c.agg.count() },
             { as: "distinct_teams", expr: (c) => c.agg.count(c("team_id"), { distinct: true }) }])
    .orderBy([{ by: "plan", order: "asc" }]),
});
```

Decisions: **one `as` form** (callback returning the builder — the shipped tri-union and `as(q) || q` laxity die); projection items are bare strings or `{ as, expr }` literals with **`as` required** on every expr item (derived names are dialect-dependent; `SELECT_ITEM_ALIAS_REQUIRED@record`), which deletes the `columns?: string[]` second writer from structured create; per-intent `innerJoin`/`leftJoin` with `{ on, as? }` (generic `join(kind, …)` — an enum positional — deleted); `orderBy` items use the shared **`order`** key; body table refs inherit the entry schema (`TableRef.schema` deleted from the IR — it could only echo); `.drop()` returns a create-only continuation.

**Matviews are their own vendor noun and op family** — not a `materialized: boolean` option (a different SQL statement family; REFRESH needs a home; the flag forced a restated fact at drop):

```ts
import { pgMaterializedView } from "@zeroship/migrate/pg";
const rollup = pgMaterializedView("order_totals", { schema: "zeroship" });
rollup.create({ withData: false, as: (q) => /* same builder */ q.from("orders").groupBy(["customer_id"])
  .select(["customer_id", { as: "total", expr: (c) => c.agg.sum(c("total")) }]) });
rollup.index("order_totals_customer_uq").add({ on: [{ column: "customer_id" }], unique: true });  // same shared index selector
rollup.refresh({ concurrently: true });     // irreversible, plan-visible
rollup.drop({ ifExists: true });            // no restated materialized flag — the noun knows
```

Ops: `createMaterializedView` / `dropMaterializedView` / `refreshMaterializedView`, all vendor rows; `materialized` is deleted from `createView`/`dropView` and the IR.

**The raw escape — reason required, real, in one op.** `view().createRaw` (core-importable, whose `reason` field the guide claims and the type lacks) is deleted. `rawSelect({ sql, reason, columns? })` is a `/pg` branded value accepted in the **`as`** slot of the pg-widened view/matview handles (`columns` lives here — the engine cannot derive names from raw). The wire is **one** `createView`/`createMaterializedView` op whose `ViewQuery` is the union `{ kind: "structured", select } | { kind: "raw", sql, reason, columns? }` — no `createRawView` op kind. The rung ladder, stated once: `reason` required at **tsc**; empty string `OP_INVALID@record`; `RAW_REASON_REQUIRED@validate` as the hand-forged-IR backstop. Every `rawSelect` passes the `SECRET_IN_RAW` lint and counts in the raw ratchet; `distinctOn` and window functions are deliberately absent from the algebra — the escape covers them and its reason string names the gap.

### 3.7 DML, enums/domains/sequences, roles/grants, and the raw budget

**DML — one value rule, honest names:**

```ts
interface InsertArgs<R extends Row = Row> { rows: R | readonly R[]; }        // Row = Record<string, DmlValue<DefaultBuilder>>
interface UpdateArgs   { set: Record<string, DmlValue<RowBuilder>>; where?: (c: RowBuilder) => Expr; batch?: IrBatch; }
interface DeleteArgs   { where: (c: RowBuilder) => Expr; limit?: number; }   // where still mandatory — no unfiltered delete; limit: core-candidate, pinned legs below
interface BackfillArgs { set: Record<string, DmlValue<RowBuilder>>; where?; cursorColumn?; batchSize?; name?; }
// .delete() replaces del() — the wire tag was literally "delete" all along.
```

**`DeleteArgs.limit` earns its non-MySQL legs or dies.** <!-- Added in round 2: it sat on core with no PG rendering. --> PG has no `DELETE … LIMIT`, and SQLite only behind a non-default compile flag — an unproven `limit` on core would be exactly the class the S10 walk exists to catch. It is a **core-candidate** with its emulations pinned as the Phase 4 parity proof: MySQL native `DELETE … LIMIT n`; PG `DELETE … WHERE ctid IN (SELECT ctid FROM t WHERE … LIMIT n)`; SQLite `DELETE … WHERE rowid IN (SELECT rowid FROM t WHERE … LIMIT n)` (no reliance on `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`). The pinned contract on all three: delete **at most n** of the matching rows, subset unspecified — batching semantics, the only honest reading of limit-without-order. Pre-declared fallback if the proof doesn't land: the option is **deleted**, not vendor-homed.

**`onConflict` is a vendor OPTION — the option-granularity flagship.** The shipped core `onConflict` is documented PG-only and rejected at build on SQLite — the exact "core surface that dev refuses" failure. Core `InsertArgs` loses it; the pg twin widens it, with `doUpdate: Record<string, DmlValue<ConflictUpdateBuilder>>` so `c.excluded(col)` is reachable:

```ts
pgTable("plans", { schema: "zeroship" }).insert({
  rows: [{ id: "free", name: "Free" }, { id: "pro", name: "Pro" }],
  onConflict: { columns: ["id"], doUpdate: { name: (c) => c.excluded("name") } },
});
```

A confined author literally cannot type it (Gate A); a smuggled bag dies at validate (Gate B). It is a declared core-candidate (SQLite/MySQL upsert forms exist) — it moves to core iff the parity proof lands.

**Enums — the EXPRESSIVE blocker closed:**

```ts
enumType("order_status", { schema: "zeroship" }).addValue({ value: "refunded" });                       // core, 3 legs
enumType("order_status", { schema: "zeroship" }).renameValue({ from: "canceled", to: "cancelled" });
pgEnumType("order_status", { schema: "zeroship" }).addValue({ value: "refunded", before: "shipped" });  // positioned = vendor OPTION
```

Legs, honest: PG `ALTER TYPE … ADD VALUE` (natively irreversible — `autoDown: false`, no fabricated down); SQLite = CHECK-rewrite rebuild class per dependent table; MySQL `addValue` = `MODIFY` per dependent column — but MySQL `renameValue` is **rebuild-class like the SQLite leg**: a bare `MODIFY` remaps values by string match, turning renamed members into `''`/errors under strict mode, so the honest leg is the three-step choreography `MODIFY` to the widened union(old ∪ new) → `UPDATE` remap → `MODIFY` to the final member list, a named Phase 4 proof obligation. `t.enum(name, { schema? })` is the one reference spelling (the `EnumHandle` overload is deleted); `.drop()` returns `DroppedEnumHandle`. **`deferredUpOps` is deleted outright** — every op call outside a recorder is `OP_OUTSIDE_RECORDER`, uniformly.

**Domains (vendor):** `domain(name, { schema? }).create({ as: ColumnDef<false>, check?: (v: DomainValueBuilder) => Expr, default?: DmlValue<DefaultBuilder>, notNull? })` — faceted `as` is a tsc error via the brand; the check spelling is `(v) => v.in([...])`; the column reference is `pgT.domain(name, { schema? })` (wire `{ domain: { name, schema? } }` — never search_path-dependent).

**Sequences (vendor):** the `.alter` bag splits per-intent — `sequence(name, { schema? }).create({ as?: IntColumnDef<false>, increment?, start?, minValue?, maxValue?, cache?, cycle?, ownedBy? })` (`as` takes only the **integer** tokens — `t.smallInt`/`t.int`/`t.bigInt` carry a distinct `IntColumnDef` brand — so `t.text()` as a sequence type refuses at **tsc**, its declared P9 rung) · `.setOptions({...})` (one `setSequenceOptions` op) · `.restart({ value? })` (one `restartSequence` op) · `.drop()` → continuation. `nextval("orders_id_seq", { schema: "zeroship" })` is `/pg`-only, a branded Expr (wire node `pgNextval`).

**Roles/schemas/extensions/grants (vendor):** same entry grammar; **role's bag is `setOptions` too** (N2 admits no per-domain exceptions): `role("sandbox_app").create({ login: true, password: secretRef("sandbox_app_password"), inRole: ["app_base"] })` · `.setOptions({ login?, password?, … })` · `.drop()`. `password` is typed `SecretRef` — never `string`; the IR carries `{ secret: "sandbox_app_password" }`, resolved fail-closed at apply. Grant/revoke targets are **bare structural** unions: `grant({ privileges: ["select"], on: { tables: ["orders"], schema: "zeroship" }, to: ["app_rw"] })` / `on: { schemas: ["zeroship"] }` — desugaring to the internally-tagged IR like every other structural union. `createFunction`/`dropFunction` stay free ops (`function` is genuinely reserved); trigger `execute` is structural: `{ name, schema? }`.

**Triggers (vendor):** `pgTable(...).trigger(name).add({ timing, events, forEach, execute | body })` / `.drop()`; column-scoped events are structural — `events: [{ update: { of: ["sector_identifier"] } }]` — with the per-dialect truth (PG ✅, SQLite ✅, MySQL ✖) in the generated table. The shipped guide's own §20 flagship `raw` justification becomes structured code, and the ratchet auto-lowers. Trigger `when` receives `TriggerWhenBuilder` (`c.old`/`c.new`); the body builder's `del` → `.delete`.

**RLS (vendor):** four op kinds collapse to one — `pgTable("apps", { schema: "zeroship" }).setRls({ enabled: true, forced: true }).policy("tenant_isolation").add({ using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })) })`.

**The raw budget:** exactly three islands — `raw({ sql, reason, secretLintWaiver? })` (vendor DDL; op tag renames `pgRaw` → `raw` — the N5 prefix rule governs expression nodes, op vendor-ness lives in the dialect table), `rawSelect` (§3.6), `createFunction.body` (no reason field). Ratchet mechanics: a committed baseline file recomputed from the recorded IR of the committed migration corpus (never source grep); CI diffs against merge-base — any ratcheted counter increase requires a new dated waiver artifact in the same diff; decreases force the lower baseline; `procBodies` is visibility-only, owned softness. The `SECRET_IN_RAW` lint refuses credential-assignment positions only (a `password_hash` column never trips it); the documented DO/EXECUTE residue takes the checksummed, counted `secretLintWaiver: { reason }`.

**Reversibility — the down-derivation matrix.** <!-- Added in round 2: the general law the per-op notes above instantiate was implicit. --> `up` is authored; `down` is either authored in the module shape or auto-derived by a pinned per-family rule, stated per op kind in the generated table:

- **Create/add family** (table/view/domain/sequence/role/enum create; column/constraint/index/policy/trigger add) — auto-down is the matching drop, fold-independent. <!-- Amended in round 3: partition-child create carved out — its matching drop is realization-bearing, and the round-2 "fold-independent" claim contradicted §3.5's own refusals. --> **Partition-child create** is the family's one carved-out member: its auto-down is still the matching drop, but that drop is the §3.5 **semantic** child drop, realized per target — native `DROP TABLE`; collapse = the bounded DELETE whose predicate comes from the create op's **own recorded bound** (no external fold state needed), or the fold-derived **residual predicate** for a `{ default: true }` child (fold-dependent by construction: the sibling set). Exactly one sub-case is not auto-derivable: a **hash** child create on a collapse-affirmed recording (its drop is `PARTITION_HASH_DROP_UNDERIVABLE@validate`, §3.5), which is `DOWN_UNDERIVABLE@record` steering to an explicit `down` or `autoDown: false`. Both collapse predicates are two-valued by §3.5 rule 2's NOT-NULL-key guarantee. **`attachPartition`** (vendor) sits in this family too: its auto-down is the matching `detach` — name-only, no bound needed — while `detach` itself stays in the destructive family below (its inverse needs the bound it doesn't carry). <!-- Added in round 4: attach's down-family was unstated. -->
- **Rename family** — auto-down is the inverse rename.
- **SetX family** (`setType`, `set/dropNotNull`, `set/dropDefault`, `setOptions`, `setRls`, `setSequenceOptions`, `setRoleOptions`, `.comment`) — auto-down **iff the fold carries the prior value** (it does for any subject created inside the DSL corpus); otherwise `DOWN_UNDERIVABLE@record` with a `suggested_fix` steering to an explicit `down` or `autoDown: false`.
- **Destructive/irreversible family** (every `.drop`, DML, `backfill`, `raw`/`rawSelect`, matview `refresh`, partition `detach`, PG enum `addValue`) — never auto-derived: explicit `down` or `autoDown: false`, per op.
- **`dialect()` values** — the down runs under the **same recorded legs**, identical scope math in both directions; a down whose dialect scope widens or narrows the up's is `DIALECT_SCOPE_MISMATCH@validate`.
- **Degraded P12 legs** — down re-realizes the inverse **semantic** op per target (the fold records the per-dialect realization); it is **never** "invert the emitted DDL" — a collapsed child create emitted no DDL, but its down is the child drop's collapse realization (the bounded DELETE above), not a fold-only no-op, because P12's absence-equivalence is a property of the whole up→down path: row sets must match the native leg's after the up **and again after the down** (the suite's down-path leg, P12/Phase 4). <!-- Rewritten in round 3: "inverts what was actually realized" read as a license for a no-op down on collapse, which is a row-set divergence. --> Concretely: the down of a collapsed partition-**parent** create is a plain-table drop; the down of a collapsed **child** create is the fold-derived bounded/residual DELETE; the realized bounded delete of §3.5 falls in the destructive family.

### 3.8 The consolidated failure ladder

| Misuse (representative) | Rung | Refusal |
| --- | --- | --- |
| Deleted symbols (`t.integer`, `t.float`, `addCheck`, `del`, chain modifiers, `.identity`, free combinators) | tsc | symbol does not exist |
| Vendor construct without the `/pg` value import (any op, option, node, `pgT` member) | tsc | not on core types (value-level widening) |
| Column ref in DEFAULT/insert value; agg in CHECK/where/ON; `c.old` outside trigger; volatile fn in an index position; bigint/`Uint8Array` bare; faceted `ColumnDef` in `of`/`to`/`as`; access on any dropped-handle continuation | tsc | builder/brand/continuation types |
| Dotted-string qualification; `eq(null)`; empty CASE; mixed/empty bounds or `dialect()` legs; de-branded Expr; `{ fn: "now" }`-class carriers; sugar+longhand collisions (`PK_/FK_/UNIQUE_DECLARED_TWICE`); constraint facet on alter-add; stale/dropped aliases (`HANDLE_STALE`/`HANDLE_DROPPED`); op outside recorder | record | `OP_INVALID`-family with `suggested_fix` |
| Unterminated selector | record (the end-of-script drain check, P9) | `SELECTOR_NOT_TERMINATED` |
| Vendor op/option/node from a confined creator; off-scope dialect target; agg/rowRef context in hand-forged IR; empty raw reason; credential-assignment SQL | validate | `VENDOR_OP_DENIED` / `DIALECT_UNSUPPORTED` / `AGG_POSITION_INVALID` / `RAW_REASON_REQUIRED` / `SECRET_IN_RAW` |
| Partitioning (§3.5): non-covering unique-enforcing entry; non-total bound set (missing default child, uncovered hash remainder, end-of-migration fold check both directions); ill-formed bound set (range overlap, duplicate list members, non-factor-chain hash moduli); nullable partition key or NULL list-bound member under collapse affirmation; composite range key under collapse; hash-child drop on a collapse-affirmed recording | validate | `PARTITION_KEY_COVERAGE` / `PARTITION_BOUNDS_NOT_TOTAL` / `PARTITION_BOUNDS_ILL_FORMED` / `PARTITION_KEY_NULLABLE_UNDER_COLLAPSE` / `PARTITION_COMPOSITE_KEY_UNSUPPORTED` / `PARTITION_HASH_DROP_UNDERIVABLE` <!-- Added in round 3; extended in round 4 with the well-formedness and two-valued-key codes. --> |
| Auto-down underivable (§3.7): SetX-family op whose prior value the fold doesn't carry; hash-child create on a collapse-affirmed recording | record | `DOWN_UNDERIVABLE` <!-- Added in round 4: pinned in §3.7 but missing from the consolidated table. --> |
| A `down` whose `dialect()` scope widens or narrows its up's (§3.7) | validate | `DIALECT_SCOPE_MISMATCH` <!-- Added in round 4: same. --> |
| Live-schema facts only (rebuild-class preconditions, drop-kind mismatch, existence-guard shape, REFRESH CONCURRENTLY unique-index, the §3.5 populated-default mirror guard on a collapsed sibling create) | lower/apply | fail-closed guards |
| Unresolved `secretRef`; ratchet increase without waiver | apply / CI | fail-closed / census gate |

Nothing representable is always-refused; nothing waits for lower that validate can express; nothing waits for validate that the types can express.

---

## 4. What each of the five dimensions gains

**FLUENT (4 → )** Chains never dead-end (`forValues`'s `void` is gone; the full return matrix is pinned) or change subject (partition, constraint-drop, and enum lifecycles are one selector each); scalars work in every value position (the lambda-tax and `lit()` requirement die — the guide's own §18 becomes legal as written); multi-intent column alteration chains on one handle. Owned, stated costs: `check(n).add({ expr })`, `t.numeric({ precision, scale })`, `.cast({ to })`, `setDefault({ value })` are each longer than the shipped forms — per-call verbosity deliberately traded for uniformity.

**ELEGANT (3 → )** Zero alias pairs, zero unregistered sugar, one options geometry, one schema site, one module shape. The duplicate-spelling classes are deleted wholesale (verb twins, the index chain+bag with its silent merge, the five-way element union, the `as` tri-form, the FK's second wire home, `membership`'s costume) — and held deleted **mechanically** by the two-tier census, so the committee grammar cannot regrow.

**CLEAN (3 → )** The vendor boundary is real for the first time — at op, node, AND option granularity, enforced at tsc (value-level widening; augmentation banned) and at validate (`VENDOR_OP_DENIED` over the one generated table). Nothing silent: `deferredUpOps`, the PK precedence merge, the encrypted facet-drop, the index args-over-chain merge, and `c("VALUE")` all become loud refusals at their earliest rung. `del` → `.delete()`; no positional booleans; no restated catalog facts (`rename.type`, `drop({ unique })`, `drop({ materialized })` all fold-derived); no plaintext credential can reach the artifact through any channel.

**EXTENSIBLE (4 → )** Every closed set, both recorder copies, the dialect truth, the doc tables, and the surface registry derive from one generated schema + one compiled recorder artifact — widening `extract` or adding a ColType is one schema edit, and "lock-step" comments are extinct. A future `/mysql` + `c.my.*` gets the identical widening mechanism with zero core churn; the S10 CI walk guarantees core never grows a **vendor**-scoped member or option again (the transparent-degradable class enters only through P12's table-backed, per-combination admission rule — never by walk exemption).

**EXPRESSIVE (4 → )** Newly authorable: enum `addValue`/`renameValue`; qualified column refs (join ON exists); GROUP BY/HAVING/DISTINCT/aggregates (the `order_totals` matview can finally total); matview REFRESH; `ATTACH PARTITION` of existing tables; `NOT VALID` → `VALIDATE` online constraint adoption (the engine's own lint advice becomes followable); composite/non-id/deferrable FKs; hash/spgist/NULLS ordering/opclass/collation/nullsNotDistinct indexes; structured durations and date arithmetic; full-JSON and exact-numeric and binary defaults; column-scoped trigger events; `secretRef`-bearing role DDL; and the counted `dialect({...})` long-tail escape. Nothing the shipped surface could express is lost — the few confinements (partial-index `where`, positioned enum insert) are moved to their honest vendor homes, not removed.

---

## 5. The must-keep spine, and no `ir_version` bump

**The engine spine is kept in kind, untouched in discipline:** a frozen, dialect-neutral, **checksummed IR**; canonical serialization; internally-tagged closed enums; `deny_unknown_fields`; `|n| < 2^53`; the eager recorder with structured errors; fail-closed existence guards; the fold/lower/apply pipeline with live-schema guards at the only rung that can own them.

**The IR *shape* changes; the version does not.** `ir_version` is pinned to **1 forever**. Pre-launch there are no stored artifacts to version against, so every wire change in this document is a **reshape-in-place**: goldens regenerate **once**, and each reshape's checksum canonicalization (key order, tag-elision rules, float formatting, Duration key order, `kind` never elided on the value wrapper) is pinned in the same change. The version field and its fail-closed check survive purely as code-evolution discipline. Every reshape is **acknowledged in the section that causes it** — this document contains no smuggled wire changes. The consolidated inventory:

1. ColType token renames (`float`→`double`, `bool`→`boolean`, `bytea`→`bytes`; dead `string` arm deleted); `char.len`→`length`; enum/domain arms gain `schema?`; `ColType::Ref` → the deferred `typeOf` (FK semantics leave the type slot).
2. `Op::RenameColumn.type` deleted (fold-supplied).
3. `IrDefault` and every scalar-or-expression slot (insert rows, `update.set`, `doUpdate`) → the `kind`-tagged (adjacently-tagged: tag `kind`, content `value`/`expr`) wrapper; `{ fn: … }` arms deleted; float canonicalization pinned.
4. `IrConstraintKind::Pk` deleted; `Fk`/`Check` gain `not_valid?`; `Op::DropConstraint` gains required closed `kind`; new `Op::ValidateConstraint`.
5. Index: `columns` → `on`; per-element `order`/`nulls`/`opclass`/`collation` on both arms; method enum re-enumerated (`hash`/`spgist` in, `btree`-as-token and `fts5` out); `nullsNotDistinct` added; `dropIndex.unique` deleted.
6. Expression nodes: `colRef` gains `table?`; `case` branches → `{ when, then }`; `extract.expr` → `from` + widened fields + new `pgExtract`; `pgIntervalLiteral` → `pgInterval { duration }`; `pgArrayMembership` → portable `inList`; new `between`/`like`/`distinctFrom`/`dateShift`/`agg`/`rowRef`/`excludedRef`/`dialect`/`pgDomainValue`/`pgCurrentSetting`/`pgCurrentUser`/`pgNextval` nodes; `ScalarFn` sheds `currentSetting`/`currentUser`.
7. Partitions: `createTable` gains `partitionBy` (with the `whenUnsupported` affirmation — the spec only, never children); new **core `createPartition`** op (the one child-create home, same-migration or later — §3.5); new `attachPartition` (vendor); `dropPartition` gains `parent`. <!-- Amended in round 3: the child create had no stated wire home. -->
8. Views: `ViewQuery` union (`structured | raw{ sql, reason, columns? }`); `materialized` deleted; three new matview op kinds; `TableRef.schema` deleted; OrderItem direction key → `order`.
9. RLS quadruplet → one `setRls`; `alterSequence` → `setSequenceOptions` + `restartSequence`; role `alter` → `setRoleOptions`; role `password: string` → the `SecretRef` carrier; `pgRaw` op tag → `raw` (+ optional `secretLintWaiver`); new `addEnumValue`/`renameEnumValue` (+ vendor `before`/`after`).
10. `op-ir.schema.json` gains per-op/per-node/per-option dialect metadata — including the **three-class disposition** (portable / transparent-degradable / vendor) and the P12 **per-option-combination transparency predicates** — (schema growth, not a migration-IR shape change); the single source both gates consume.

Every reshape lands with its producers, consumers, fixtures, regenerated goldens, and reference docs in the same patch. No dual-read paths, ever.

---

## 6. The phased path from the shipped surface to this one

Each phase is a landable slice with its own green gate; within any slice that touches the wire, the whole reshape discipline applies (producers + consumers + goldens + docs, one patch). No phase leaves a shim behind.

**Phase 0 — the generation substrate (enables everything else).**
Extend `op-ir.schema.json` with the per-op/per-node/per-option dialect metadata; build the generator emitting Rust enums, TS literal types, runtime guard arrays, the dialect table, and the doc token tables; collapse the SDK recorder and the engine-embedded `migrate_ops.js` twin into **one compiled recorder artifact** whose producers are minted through the single `defineOp` chokepoint — the tier-1 producer census and tier-2 writer inventory are **derived from that registry plus the exported-symbol walk** (P1), never self-reported; create the surface registry file; stand up the CI gates — census assertions, the `.d.ts` declaration lint, the S10 core-export walk (whose classification of **non-op core exports** — the value factories `t`, `lit`, `decimal`, `byteValue`, `minValue`/`maxValue`, `dialect`, `fromDb` — is **registry-backed**: each carries an explicit surface-registry row naming it a value factory with its dialect disposition, so the walk consults the registry for every export uniformly; never a hardcoded exemption/skip list inside the walk, which would be exactly the rot vector S10 exists to kill <!-- Added in round 4: the deferred non-op-export classification gets its mechanism pinned. -->), the snippet-taxonomy runner, the ratchet script with its committed baseline. Exit gate: generated output diffs clean; the *shipped* surface's known duplications show up as census failures (proving the instrument works before the surgery).

**Phase 1 — the wire reshape-in-place.**
Execute the §5 inventory as one coherent engine+SDK change (a few internally-ordered commits are fine; no intermediate dual-shape state ships): goldens regenerate once, canonicalizations pinned, the Rust validator switches from hand-written dialect match arms to consuming the generated table. Exit gate: full `cargo test -p zeroship-migrate` (all targets, never `--lib`-only), render goldens on PG :5440 + in-process SQLite + the mysql2 driver isolate.

**Phase 2 — the core surface rewrite.**
Rewrite `sdks/migrate/src` to the one grammar: entry/selector/terminal shapes, the pinned return matrix + generation counters (`HANDLE_STALE`/`HANDLE_DROPPED`), the builder lattice, `DmlValue` with per-position instantiation, the type lexicon, per-intent alteration, the DML shapes, enum evolution, structured views. Every deletion in this document lands here, totally. Every construct ships its misuse test at its declared rung. Exit gate: census tier 1 + tier 2 pass with exactly the registered sugar pairs; the `.d.ts` lint passes; every `compile-and-record`/`expect-error` snippet in the regenerated reference asserts its contract.

**Phase 3 — the `/pg` tier.**
Build the value-level widening: `pgTable`/`pgView`/`pgMaterializedView`/`pgEnumType`/`pgT` and the vendor entries (`domain`, `sequence`, `nextval`, `schema`, `extension`, `role`, `grant`/`revoke`, `createFunction`, `raw`, `rawSelect`, `secretRef` — `minValue`/`maxValue` are **core**, built in Phase 2 with the §3.5 bounds), the widened selectors returning widened parents, `c.pg.*` on the `…WithPg` builders, and the validate-side capability gate (`VENDOR_OP_DENIED`) over the same generated table. Exit gate: the S10 walk proves core reaches nothing vendor-scoped; a confined-profile validate suite proves Gate B independently of tsc.

**Phase 4 — the claiming phase (proofs, then admission).**
For every **core-candidate** (enum evolution legs — including the MySQL `renameValue` three-step choreography, `like`, `mod`, `round`/`floor`/`ceil`/`substr`/`replace`, `dateAdd`/`dateSub`, the portable `extract` fields, expression index elements, aggregates + `groupBy`/`having`/`distinct`, `replace`-on-views, `isDistinctFrom`, the comment-terminal family, `onConflict`, `DeleteArgs.limit` with its pinned ctid/rowid emulations): land the live three-dialect parity proof (render goldens + live apply on all three backends) and flip the dialect-table row to core — or execute the pre-declared fallback to the vendor tier / `dialect()` legs. The P12 obligation lands here too: the §3.5 **partitioning absence-equivalence suite** (insert-acceptance matrix incl. out-of-bound rows and the NULL-key **uniform-rejection** leg — under rule 2's two-valued-key guarantee both forms carry NOT NULL on the key, so the leg proves matching *rejection*, not matching routing; constraint behavior; the pinned query set; **DDL-acceptance**: the populated-default mirror-guard legs — strayed matching rows in the default → sibling create errors on both forms; clean default → succeeds on both; the rule-3 well-formedness refusal cases (range overlap, duplicate list members, non-factor-chain hash moduli); child-drop row-set equivalence between native `DROP TABLE` and the bounded `DELETE`; the default child's residual-DELETE equivalence against the native default partition's row set; hash remainder-coverage acceptance cases; the canonical text-bound lexicographic-ordering leg; and the **up → pinned inserts → down** leg — row sets identical across native and collapsed forms after the up and again after the down, the flagship §3.5 example included <!-- Added in round 3: the suite had no down-path leg. Amended in round 4: the NULL-key leg re-pinned as uniform rejection (rule 2's refusal makes it green); mirror-guard + well-formedness legs added. -->) — the dialect-table row cannot say *transparent* before it lands. Proof coverage is **not** core-only: every non-PG-only dialect-table row — vendor claimed legs (e.g. `setDefault` = pg+sqlite, trigger `UPDATE OF` = PG/SQLite) and transparency classifications included — carries the same render-golden + live-apply obligation. Run the **P6 sufficiency gate**: structurally author the platform Liquibase corpus in the new surface (including the `secretRef` role migrations and the mechanical re-verification of the schema-once assumption over tables, FKs, and sequence/enum/domain references). Any unauthorable corpus shape is a P0 blocker of this phase. Exit gate: corpus authored; raw-baseline file committed with the honest initial counts and waivers.

**Phase 5 — docs and the platform rebaseline.**
Regenerate `docs/reference/migrate-dsl-examples.md` from executed snippets (the fourteen-invalid-examples class becomes structurally impossible — an unmarked failing snippet is a red build); regenerate the export/op/node/registry inventories; commit the DSL-authored platform migrations as the living corpus the ratchet counts against; retire the shipped guide and the prior proposal to `docs/archive/`. Exit gate: the full pipeline — `pnpm build`, the migrate test suites on all three dialects, the golden path — green; the adversarial critique re-run against the new surface with every BLOCKER/MAJOR finding traceable to a closing section of this document.

Phases 0–1 are the keystone (the instrument and the wire); 2–3 are the surface; 4 is proofs-not-vibes; 5 makes the docs unable to lie again.

### Open tensions (carried forward deliberately, not hidden)

<!-- Restructured in round 4: this list existed only as an unnumbered run-on closing paragraph, so §2's tier note, §3.3's "open tension 1", and §3.5's MySQL-leg pointer all dangled. Same content, now the numbered subsection those references name. -->

1. **Index-option re-home into a capability-scoped tier.** A possible future **re-home** of the two-of-three options (partial `where`, trigger `UPDATE OF`, deferrable-on-SQLite) from their **decided `/pg` home** (§3.3) into a **capability-scoped tier** — the hypothetical fourth disposition of §2's tier note: supported on targets with the capability, refused (never degraded) elsewhere. The generated table makes the re-home a data change; the gate is the P12 per-option-combination transparency predicates, stated and proved per admitted combination (§3.3's `unique × where` insert-acceptance flip is the standing counterexample). The present disposition is settled, only the re-home is open.
2. **Native-MySQL partitioning leg** for the expressible subset (§3.5 — contiguous `RANGE COLUMNS`, integer/DATE/DATETIME keys, no FKs, no default child), gated on its own absence-equivalence proof (which by definition covers insert- and DDL-acceptance) through the same Phase 4 suite; §3.5 pins the rule-2 interaction — only never-affirmed recordings or hash recordings can ever qualify.
3. Rename-after-raw-history.
4. The exact sugar-admission line beyond the four registered pairs.
5. `dialect()` availability for confined creators (operator-only at first).
6. The interval canonical form's final checksum pinning.
7. `secretRef` dev-tier resolution (a dev-only-by-construction requirement, per the auth-dev-tier precedent).