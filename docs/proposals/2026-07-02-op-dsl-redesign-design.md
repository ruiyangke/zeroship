# Next-generation migration DSL: `op.*` / `pg.*` clean-slate authoring rewrite over IR v2

- **Status:** DRAFT (uncommitted design; commits with the implementing PR-train per `feedback_proposal_workflow`)
- **Date:** 2026-07-02
- **Decision:** REWRITE the authoring surface + IR wire shape (one clean pre-launch break, `ir_version: 2`); KEEP the engine spine (validate → lower → render → apply, capability gates, expand/contract machinery, multi-dialect backends) unchanged in kind.
- **Resolves:** `OPS_DSL_CRITIQUE.md` (all 10 ranked findings + the verdict-level coverage demand)
- **Supersedes as normative:** `docs/proposals/2026-06-23-js-op-dsl-migration-design-normative.md`, `docs/proposals/2026-06-25-op-dsl-fluent-redesign.md`, `docs/proposals/2026-06-26-sql-features-structured-dsl-design.md` (all become historical once this lands)
- **Acceptance corpus:** this repo's Liquibase platform changelog (`db/changelog`) — the `control`, `auth`, and per-app schemas. **Sandbox `V0011`–`V0024` are explicitly OUT of scope** (see the corrected §8 note): per AGENTS.md the sandbox is extracted to the sibling `zeroship-sandbox` repo, owns its own `sandbox_*` roles/migrations, and is deferred — it adopts this DSL independently on its own timeline. Scoping to per `docs/proposals/2026-06-25-vendor-pg-primitives.md`'s rebaseline end state.
<!-- Revised 2026-07-02: addressing critic MINOR #6 — sandbox is a sibling repo per AGENTS.md; removing it from this repo's port corpus resolves the scope-boundary ambiguity. -->

> **Revision note (2026-07-02).** This draft was revised against a feasibility review that verified claims against `crates/zeroship-migrate/src/render/{step.rs,sql_preview.rs,lower.rs}` and `src/model/{ir.rs,validate.rs}`. The review's load-bearing finding is confirmed by the code: the SQLite 12-step rebuild is a **live-state-dependent** lowering (`LiveSchema.tables → TableSnapshot`, `sql_preview.rs:412`), rendered offline only as a `-- [runtime-resolved]` label. The prior draft's "validity == renderability, render must always succeed from IR alone" mechanism was therefore false for the entire SQLite ALTER-add-constraint / rename / online-type-change / enum-rewrite class. §2.4, §5.2, §8, §9, §10, §12 are rewritten around a **two-mode render model** (offline-deterministic vs live-resolved) that matches the engine. Every criticism is tracked to a section below.

> **Revision note (2026-07-02 final consistency pass).** Closed the remaining critical/major consistency defects: PG-only `domain`/`sequence` moved to `pg.*`; checksum treatment now distinguishes pure apply hints from phase-generating online op-groups; `drift` includes a journal-state check for open contracts; MySQL drift uses a dialect-specific catalog introspector rather than the Postgres `pg_dump` comparator.

---

## 1. Motivation

### 1.1 The critique's verdict

`OPS_DSL_CRITIQUE.md` scored the current DSL 5/5/4 and delivered a verdict we accept in full:
the **architecture is right** — frozen dialect-neutral checksummed IR, structured authoring,
capability-gated creator/operator split, strict portable core — but the **delivered surface is
not clean, not honest, and not powerful enough** to be the sole authoring surface for the
platform schema. Ten findings, all verified against the repo:

1. The public contract is internally inconsistent (docs say four exports and "`table()` is the
   sole entry"; `sdks/migrate/src/index.ts:21-37` exports nine value symbols plus ~50 types;
   `ops.ts:24` repeats the stale claim).
2. The "no raw SQL" stance is false (three raw islands in `ir.rs:18`; core exports
   `view.createRaw`; `pg.raw`/`pg.sql` at `pg.ts:143-160`).
3. `pg.sql` records tagged-template binds (`pg.ts:374-384`) that `validate.rs:748` rejects
   unconditionally and `vendor.rs:583-585` has a dedicated error for — the advertised safe-raw
   API is unusable for its purpose.
4. The core/vendor boundary leaks: the portable `TableHandle` exposes RLS/policies
   (`types.ts:845-856`) and the core expression builder exposes `currentSetting`/`currentUser`
   (`types.ts:387-389`).
5. TypeScript accepts shapes the lowerer refuses: create-table PKs, table-level checks,
   composite/non-id FKs, SQLite table-level unique/FK (`lower.rs:2739-2807`, `:3413`);
   `alter({type, nullable})` silently drops `nullable` when both are given.
6. Indexes are far too thin (no BRIN/hash/SP-GiST, no INCLUDE, no per-element
   opclass/collation/order/nulls, no storage params; IR has `concurrently` but TS doesn't).
7. The closed expression AST is undersized (no IN/BETWEEN/LIKE/regex/IS DISTINCT FROM,
   no JSON/array ops, no intervals, no OLD/NEW, no window/aggregate) — the single gap that
   forces checks, policies, partial indexes, views, and backfills back to raw.
8. Too much failure happens late — in Rust lowering rather than tsc/record/validate.
9. Fluent inconsistencies: `pgEnum` eager-records AND returns a `.create()` handle;
   `t.int` exists but is documented as removed; positional booleans (`softDelete(true)`).
10. `@zeroship/migrate` vs `@zeroship/migrations` is an unnecessary naming collision.

### 1.2 The platform-rebaseline forcing function

The vendor-pg-primitives track (`docs/proposals/2026-06-25-vendor-pg-primitives.md`) commits us
to retiring the Liquibase changelog: this repo's platform schema — roles, grants, RLS, policies,
functions, triggers, extensions — must be authorable in this DSL, with **catalog-level pg_dump
equivalence** (§8) against the Liquibase-applied baseline as the exit gate. (The sandbox
`V0011`–`V0024` role machinery is **excluded** — sibling `zeroship-sandbox` repo, §header/§8.)

Today that port is impossible without `pg.raw` becoming routine, exactly the smell the critique
names: BRIN/covering indexes on event tables, composite FKs, rendered checks, partitioned usage
tables, per-sequence grants, `ALTER DEFAULT PRIVILEGES`, `SECURITY DEFINER` + `SET search_path`
functions, `NOT VALID → VALIDATE` constraint adoption — all normal platform-schema work, all
currently raw-or-nothing. When ordinary constructs require raw SQL, the structured surface is
too thin, the "no raw SQL" claim is marketing, and the frozen-IR discipline protects strings.

### 1.3 What this design does

Keep the four load-bearing inventions — **frozen checksummed dialect-neutral IR**,
**capability-gated core/vendor trust split**, **multi-dialect apply**, **expand/contract
machinery** — and delete the entire authoring grammar plus the IR v1 wire shape in one
PR-train. No aliases, no shims, no "deferred render" arms surviving into v2. Per the repo's
pre-launch no-back-compat stance (AGENTS.md), half-supported shapes are **deleted, not
documented around**.

---

## 2. The decision: rewrite vs grow

**Decision: clean-slate authoring rewrite (surface + IR wire shape), engine spine retained.**

<!-- Revised 2026-07-02: addressing critic MINOR #2 — the convergence claim rested on Stances A/B/C that were not in this document. Summarized inline so the graft provenance is evaluable. -->
**The three stances, in one line each** (referenced throughout as the graft sources):

- **Stance A — "grow the current surface incrementally."** Keep the named-export grammar, add the missing ops/nodes in place, gate docs↔exports drift in CI. Grafts adopted: the in-IR `PgRaw.reason` field, the node-by-node Tier-P **parity-proof / demotion** discipline, and the one-release recorder byte-parity **tripwire**.
- **Stance B — "clean-slate two-root rewrite" (this design's core).** Delete the grammar and the v1 wire shape; `op.*`/`pg.*` two roots; per-intent alter terminals; static support declarations.
- **Stance C — "declarative desired-state + differ" (Atlas/Prisma model), rejected.** Grafts adopted from its determinism-safe half: the first-class checksummed **`SchemaState` fold**, the standalone **`drift`** gate, and capability presets as **symmetric policy-config files**.

Stances A and B were designed in full and judged. They converge on ~90% of substance: one clean
`ir_version: 2` break with half-supported shapes deleted; static per-op/per-node dialect+tier
support declarations with an exhaustive **mode-aware** render-matrix test (validity ==
*applicability*, two render modes — §5.2, not the earlier "validity == renderability"); a
two-tier derived expression AST; boundary carried by API shape with the capability double-gate
kept as defense-in-depth; binds deleted end-to-end; a single compiled recorder artifact; the
Liquibase corpus + pg_dump-zero-diff + raw budget; the `@zeroship/backfill` rename. The
decision therefore reduces to the authoring surface, where the rewrite wins on structural
grounds:

1. **The clean-slate surface unifies every object kind under one grammar (`kind(name, opts?) →
   handle → selector → terminal`) across two packages.** *(2026-07-02 operator directive: the
   `op.`/`pg.` root-object prefix from the first draft is removed — §4.)* The trust split lives in
   the import path (`@zeroship/migrate` vs `@zeroship/migrate/pg`) plus the engine capability gate
   on the lowered op, not a repeated call-site prefix. The two TS reserved words the prefix used to
   shield — `enum`, `function` — are handled by renaming exactly those two exports (`enumType`,
   `createFunction`); every other kind imports as its bare noun. The docs↔exports drift *class* is
   closed by generating the reference from the actual package exports with a CI diff gate (finding
   #1) — a mechanism that is namespace-independent, so the prefix bought nothing here.
2. **Per-intent alter terminals (`setType`/`setNotNull`/`setDefault`) make the silent-drop bug
   unrepresentable at tsc** — one intent, one terminal, one op. The incremental alternative
   (an options bag recording two ops in pinned order) is honored-but-implicit: a weaker reading
   of "make unrepresentable what can't be rendered" and of Principle 1.
3. **The rewrite is more concrete exactly where migration safety lives:** the non-transactional
   executor step class for `CONCURRENTLY` (journaled in-progress marker, drop-and-rebuild crash
   recovery, still under the advisory lock) and deterministic, re-lower-stable per-phase
   `version_key`s as the interface the per-version approval scoping consumes.
4. **v1's SQLite table-level constraint refusals were partly renderer debt — but the fix is
   scoped precisely, not blanket.** There are two distinct SQLite cases and the prior draft
   conflated them (critic CRITICAL #1):
   - **On `CREATE TABLE`** (`op.table(x).create({ checks, uniques, foreignKeys })`), table-level
     checks/uniques/composite-non-id FKs render **deterministically offline** — the full table
     definition is in the op, no live state is consulted. This *was* renderer debt; the rewrite
     renders it. These are golden-testable.
   - **On `ALTER TABLE … ADD CONSTRAINT`** (`op.table(x).check(n).add(...)`, and column
     rename/type-change/enum→CHECK-rewrite), SQLite has **no `ALTER … ADD CONSTRAINT`**. The
     engine reconciles these via the **12-step table rebuild**, which needs the **live table
     structure** (`LiveSchema.tables → TableSnapshot`, verified at `render/lower.rs:137` and
     `render/sql_preview.rs:412`). These **cannot** be rendered from single-op IR alone; offline
     `plan` prints them as `-- [runtime-resolved]` and they are *lowered*, not *rendered-golden*,
     against a live (or synthetic-fixture) schema.

   The rewrite therefore shrinks the *refusal* matrix (CREATE-path constraints now render) **without
   erasing the live-schema dependency** the engine is built around. This distinction drives the
   two-mode render model in §5.2 and the `🔁 live-rebuild` cells in §8 — it is not a "✅ renders
   offline" claim for the ALTER/rebuild class.
5. **The cost delta is modest and correctly priced.** The Rust engine spine (validate/lower/
   render/executor, capability gates, expand/contract machinery, all three dialect backends) is
   retained either way; the rewrite's extra cost is the TypeScript surface — the cheapest, most
   test-pinned layer to rewrite — spent now rather than carrying the critique-named debts
   (the "clever rather than inevitable" `c("x")` builder, accreted handle-inheritance type
   machinery, undocumented aliases) into the generation that is supposed to resolve the critique.

**Rejected: declarative desired-state + differ (Atlas/Prisma model).** Its
determinism-compatible form (pure differ over `fold(migrations)` vs `record(schema/)`, never
the live DB) is genuinely careful — but it is by its own admission a strict superset: everything
this design builds PLUS a correctness-critical differ whose failure mode is wrong-DDL/data
loss, a fold grown to model the full object model as the differ's left input, a two-artifact
coherence gate, and demotion of the mature, critique-praised recorder path to secondary status.
Wrong risk profile for the layer that guards the platform schema. Its two best
determinism-safe ideas are **grafted** into this design (§10.2, §10.3): the standalone `drift`
gate and the first-class checksummed `SchemaState` fold — which is exactly the left input a
future declarative layer would consume, keeping that product open as a later *additive* layer
on top of IR v2 without buying the differ today.

---

## 3. What survives verbatim (the MUST-KEEP spine)

| # | Invariant | Disposition |
|---|---|---|
| K1 | Frozen, dialect-neutral, checksummed IR wire artifact: canonical serialization, single cross-impl SHA-256 checksum, internally-tagged closed enums, `deny_unknown_fields`, `IrScalar` \|n\|<2^53 enforced at deserialize | Kept in *kind*; the *shape* breaks once to `ir_version: 2` (§5). Checksum-stability property tests and unknown-tag fail-closed deserialize tests regenerate against v2 goldens. |
| K2 | No committed `.ir.json` — IR is build-time/in-memory; platform migrations are `.ts`-only; creator IR is ephemeral in-`.zship`; only test-fixture goldens exist on disk | Kept. `gen-types` consumes the `.ts` migration set re-recorded transiently. <!-- Revised 2026-07-02: addressing critic MINOR #4 — "Flyway/dbmate trusted CLI profile" was a stale/unestablished name; the corpus and platform path are Liquibase→retired-into-this-DSL. --> (The prior draft named a "Flyway/dbmate trusted CLI profile" here; that was stale. The only profiles this design establishes are the capability presets of §10.4 — confined/operator/local — all `.ts`, all folded. There is no separate `.sql`-artifact CLI profile in scope.) The `migrate-op-dsl.md:1035` "loads the committed .ir.json set" wording is corrected in the rewritten reference. |
| K3 | Strict portable creator core vs operator-only privileged layer, capability-gated: `VendorCapabilities` presets, unmintable `pub(crate)` `OperatorCapability`, `SchemaScope::None → confined`, double gate at validate AND lower (`VENDOR_OP_DENIED`) | Kept as defense-in-depth. NEW: the API shape now *also* carries the boundary (§4, §6). The `pg.ts` header stance survives: the subpath import is deliberately NOT the security gate; trust is server-side. Presets additionally serialize as symmetric policy-config files (§10.4). |
| K4 | Multi-dialect apply: PG native (compio-postgres) + in-process SQLite + MySQL via JsDriverBackend; derived (never author-declared) `dialect_scope`; apply faithfully or refuse fail-closed at validate | Kept; strengthened. **Precisely (critic CRITICAL #1):** *dialect-scope* refusal (op not supported on a target dialect) moves entirely to **validate**; the *live-schema fail-closed* guards for the `LiveResolved` rebuild class (§5.2) remain at **lower** because they are apply-time facts (the live table shape), not validity errors. The design no longer claims "all refusal at validate." |
| K5 | pg_dump-equivalence achievable for the full platform schema; no regression of roles/grants/RLS/policies/functions/extensions coverage in `pg.ts` + `render/vendor.rs` | Kept; promoted to the exit gate (§8, §11 P5). |
| K6 | Safe online migrations first-class: `render/expand_contract.rs`, online renameColumn dual-write, PR9a pending-contract interlock, crash-safe cursor backfill, split statement_timeout(60s)/lock_timeout(3s) envelope with checksummed overrides, production-gate pin `crates/control/tests/deploy_migrate_test.rs:643` | Kept; extended with NOT VALID→VALIDATE pairing, online type change, CONCURRENTLY step class, per-phase version_keys (§9). Interlock additionally hoisted to validate time (§9.3). |
| K7 | Names-are-strings: no live-schema **tsc/type** binding (migration files are immutable history; the rot-bug rationale, `migrate-op-dsl.md:133-167`). Structural typing of op shapes, `t.*`, expression node shapes stays; name existence validates at apply time | Kept. **Clarified (critic CRITICAL #3):** K7 is about the *authoring/type* layer — the DSL never binds to a live catalog at author time. It does **not** forbid the *engine* from passing derived context into `validate` or `lower`. See the pinned validate-input taxonomy in §9.3: validate reads `(IR, applied-migration fold facet)`; the **fold is not a live-DB consult** (it is the deterministic left-fold of prior migrations, K2/§10.2). The only *live catalog* consult in the whole system is `drift` (§10.3). |
| K8 | Critique-praised quality properties: eager-record recorder with `SELECTOR_NOT_TERMINATED`/`OP_OUTSIDE_RECORDER`; structured error codes with `suggested_fix`; fail-closed shape-verify-or-fail existence guards; honest `plan` with `-- [runtime-resolved]` labels, no fabricated SQL; determinism lint + bare-native-symbol→fnSynth translation (`Date.now` ⇒ `fn.now()`); mandatory `where` on `del`; shared ColType lexicon with `@zeroship/db` (`fromDb`/db-lexicon.ts); recorder byte parity | All kept. Parity is re-achieved by construction via the single compiled recorder artifact (§10.1). |
| K9 | One migration = one `.ts` module `export default { name?, up, down? }`; auto-derived `down` only for fully reversible op lists; never fabricated inverses for DML/lossy DDL | Kept verbatim. |

---

## 4. The new surface: two packages, named exports, one grammar

<!-- Revised 2026-07-02 (operator directive): the `op.`/`pg.` ROOT-OBJECT prefix is REMOVED.
     A prefix repeated at every call site was noise; the reserved-word cases it existed to
     dodge (`enum`, `function`) are handled by renaming those two exports. The trust split now
     lives entirely in the IMPORT PATH + the engine capability gate, not a call-site prefix. §4.1
     below is the superseding grammar; illustrative examples further down that still read
     `op.x`/`pg.x` mean the corresponding bare/`create*` export and are mechanically renamed when
     P2 lands the surface. -->

**Exactly two import paths. Direct named exports — no root object, no call-site prefix.** The
trust split is visible in the import *line*, and enforced by the engine's capability gate on the
lowered IR op (a non-operator importing from `/pg` is refused at apply regardless of surface).

```ts
// portable creator core — renders on PG + SQLite + MySQL
import { table, view, index, enumType, foreignKey, check, unique, comment } from "@zeroship/migrate";
// operator-only vendor layer (PostgreSQL depth)
import { schema, role, domain, sequence, policy, enableRls, grant, createFunction, createTrigger, extension, raw } from "@zeroship/migrate/pg";
```

- **`@zeroship/migrate` value exports (portable core):** `table`, `view`, `index`, `enumType`,
  `foreignKey`, `check`, `unique`, `comment`, plus the column-type lexicon `t` (the `fromDb`
  db-lexicon bridge) and `lintDeterminism`. Nothing else. There is no `op` object.
- **`@zeroship/migrate/pg` value exports (operator vendor):** `schema`, `role`, `domain`,
  `sequence`, `policy`, `enableRls`, `disableRls`, `grant`, `revoke`, `grantRole`, `revokeRole`,
  `defaultPrivileges`, `createFunction`, `createTrigger`, `extension`, `raw`, `rawSelect`. There
  is no `pg` object.
- **Two reserved-word renames** (the only cost of dropping the namespace): `enum` → **`enumType`**
  (core), `function` → **`createFunction`** (vendor). Every other kind imports as its bare noun.
- The reference's export table, op inventory, and expression-node inventory are **generated
  from** the package exports and the schemars `op-ir.schema.json`, with a CI diff gate —
  docs↔exports drift becomes a build failure, permanently (finding #1). Generation is identical
  whether exports are a namespace or named functions; the namespace was never load-bearing for it.

### 4.1 The grammar (uniform for every object kind, both packages)

```
kind(name, opts?) → inert handle → selector(name) → terminal({ options })
```

- **Handles are inert values** carrying only identity (`{ name, schema? }`). No constructor
  ever records — the `pgEnum` double-create trap is structurally gone.
- **Every terminal takes exactly one options object** (identity positional, payload named),
  records eagerly onto the ambient recorder, and returns the handle for chaining.
  Drain-time `SELECTOR_NOT_TERMINATED` / `SELECTOR_ALREADY_TERMINATED` stay.
- **No positional booleans anywhere**, pinned by a lint rule + a type-level test
  ("no public method has a boolean-typed positional parameter").
- Core kinds: `table`, `view`, `enumType` (the `enum`-reserved-word case; imports as a bare
  name because it is renamed, not because a namespace shields it).
- `comment` is a uniform `.comment(text)` terminal on every handle (and
  `.column(n).comment(...)`, `.index(n).comment(...)`), and also a standalone `comment(...)`
  export for object-level comments.
- `t.int` and every undocumented alias: deleted. The lexicon is exactly the documented set,
  because the doc is generated from source.
- `softDelete(enabled?)` / `withVersioning(enabled?)` are deleted; the single terminal is
  `table(x).setOptions({ softDelete?, versioning?, strictness? })`.

**Vendor extends the same grammar.** Operator migrations `import { … } from "@zeroship/migrate/pg"`
and get the identical `kind(name, opts?) → handle → selector → terminal` shape. Vendor-only kinds:
`schema`, `role`, `extension`, `createFunction`, `domain`, `sequence`; direct statement ops
`grant`, `revoke`, `grantRole`, `revokeRole`, `defaultPrivileges`; and the single raw escape
`raw({ sql, reason })`. When a table needs vendor depth (RLS, policies), the operator migration
imports those vendor terminals, which widen the core `table` handle's accepted selectors —
one handle type, capability-gated at lower, not a parallel `pg.table` builder.

**Core root means three-dialect portable.** No PG-only object kind is reachable from `op.*`.
Domains and standalone sequences are PostgreSQL objects for this design's dialect set, so they
live under `pg.*` and refuse under SQLite/MySQL through the normal vendor support matrix. This
keeps the SQLite dev tier (`env.db` -> SQLite) from accepting authoring forms the engine will
reject on the creator's own `pnpm dev`.

### 4.2 Creator core: table + columns + constraints + a rich-but-portable index

```ts
// migrations/0042_events.ts
import { op, t } from "@zeroship/migrate";

export default {
  up() {
    op.table("events").create({
      columns: {
        id: t.id({ prefix: "evt" }),
        org_id: t.ref("orgs").notNull(),   // single-column FK sugar — desugars to one IrForeignKey (see note)
        kind: t.text().notNull(),
        payload: t.json(),
        occurred_at: t.timestamp().notNull().default(t.now()),
      },
      // ALL of these now RENDER on PG + SQLite + MySQL — v1's accepted-then-refused arms are gone:
      uniques: [{ name: "events_org_kind_uq", columns: ["org_id", "kind"] }],
      checks:  [{ name: "events_kind_nonempty", expr: (c) => c("kind").ne("") }],
      foreignKeys: [{
        name: "events_org_fk",
        columns: ["org_id"],                              // composite allowed
        references: { table: "orgs", columns: ["id"] },   // non-id allowed
        onDelete: "cascade",
      }],
      // `primaryKey` is NOT a field on the core type — unrepresentable in core (platform owns t.id()).
    });

    op.table("events").index("events_org_occurred_idx").add({
      on: [
        { column: "org_id" },
        { column: "occurred_at", order: "desc", nulls: "last" },  // per-element sort/nulls: portable
      ],
      where: (c) => c("kind").in(["signup", "purchase"]),          // Tier-P `in`; partial idx: PG+SQLite; MySQL refused at VALIDATE
      online: true,  // PURE APPLY HINT for index build (§9.4). PG builds CONCURRENTLY
                     // (outside the txn envelope, non-atomic, retryable); SQLite/MySQL plain.
                     // Same op identity + end state; excluded from checksum/fold/shape-compare/
                     // dialect_scope. This carve-out does NOT apply to online rename/type-change.
    });
  },
};
```

<!-- Revised 2026-07-02: addressing critic MAJOR #6 — t.ref() and foreignKeys:[] were two ways to declare an FK with no stated canonical path (Principle-1 violation). Pinned as one lowering, not two paths. -->
**One FK, one lowering — `t.ref` is sugar over `foreignKeys` (MAJOR #6).** There are not two FK
*mechanisms*; there is one `IrForeignKey` model and one lowering path. The two surfaces are a
**single-column shorthand and its longhand**, with a pinned rule:

- `t.ref("orgs")` on a column is sugar that **desugars to exactly one `IrForeignKey`** with
  `columns: ["<this col>"]`, `references: { table: "orgs", columns: ["id"] }`, and default actions.
  It is the canonical path for the common case: a single-column FK to another table's `id`.
- `foreignKeys: [...]` at the table level is the path **whenever you need anything `t.ref` cannot
  express**: composite columns, non-`id` referenced columns, `onDelete`/`onUpdate`, `match`,
  `deferrable`/`initiallyDeferred`, or `notValid` (the vendor options).
- **They cannot collide**: it is a `record`-time `OP_INVALID` (`FK_DECLARED_TWICE`) to declare both a
  `t.ref` **and** a table-level `foreignKeys` entry over the same column set. So there is exactly one
  obvious path per case — `t.ref` for the simple single-column-to-id FK, `foreignKeys` for
  everything else — and both fold to the identical IR node, so lowering, `down`-derivation, and the
  catalog comparator see one shape. `t.ref(...).notNull()` composes; advanced FK options are
  **not** added to `t.ref` (that would recreate the two-paths problem) — reach for `foreignKeys`.

### 4.3 Column alteration: per-intent terminals (the silent-drop bug is unconstructible)

```ts
const c = op.table("orders").column("total");
c.setType({ type: t.numeric(14, 2), using: (x) => x("total").cast("numeric") });
c.setNotNull();             // separate op — recording both intents is two explicit lines
c.dropNotNull();
c.setDefault({ value: 0 });
c.dropDefault();
c.rename({ to: "amount" });                       // offline rename (dev/dialect-safe cases)
c.rename({ to: "amount", online: true });         // phase-generating online rename (§9.1/§9.4)
// v1's .alter({ type?, nullable? }) bag is DELETED. One intent, one terminal, one op.
```

### 4.4 Type evolution (core enum; PG-only types in `pg`)

```ts
op.enum("order_status").create({ values: ["pending", "paid", "canceled"] });   // ✅✅✅ reversible (drop type)
op.enum("order_status").addValue({ value: "refunded" });                       // append: ✅✅✅* — IRREVERSIBLE (no auto-down)
op.enum("order_status").addValue({ value: "refunded", after: "paid" });        // positioned: PG+SQLite only; MySQL ⛔ (see below)
op.enum("order_status").renameValue({ from: "canceled", to: "cancelled" });    // PG+SQLite; MySQL ⛔
op.enum("order_status").rename({ to: "order_state" });                          // ✅✅✅ reversible

// PER-DIALECT REALITY (critic MAJOR #7 — the prior "✅✅✅ ENUM alter" over-claimed MySQL):
//   PG:     ALTER TYPE … ADD VALUE (append cheap; `after`/`before` positioned insert supported).
//           `plan` marks the PG caveat: ADD VALUE cannot run in a txn block with other ops.
//   SQLite: enums are CHECK constraints → every addValue/renameValue is a 12-step CHECK REWRITE
//           = RenderMode::LiveResolved (🔁), needs the live table shape, no offline golden.
//   MySQL:  an enum is a per-column type. addValue/renameValue = MODIFY COLUMN rewriting EVERY
//           table that uses the type, with NULL/default semantics that differ from PG's ADD VALUE.
//           APPEND-at-end is admitted (proven MODIFY-rewrite envelope). POSITIONED insert (`after`/
//           `before`) and renameValue are REFUSED on MySQL at validate (ENUM_POSITION_UNSUPPORTED)
//           — three-dialect agreement genuinely does not hold there, so we do not fake it.
//
// REVERSIBILITY (critic MAJOR #5, K9): enum `create`/`rename`/`renameValue` are auto-reversible;
// `addValue` is IRREVERSIBLE on PG (PG cannot DROP an enum value) → NO fabricated auto-down. A
// migration whose `up` contains addValue must either omit `down` (irreversible, refuse to
// auto-derive) or supply an explicit `down`; see the §5.3 reversibility taxonomy.
```

Domains and standalone sequences are **not** core. They are PostgreSQL-only object kinds and are
reachable only from `pg.domain` / `pg.sequence` (§4.5, §8). That is the portability boundary:
portable enum evolution stays in `op.enum`; PG type objects stay in `pg.*`.

### 4.5 Operator vendor: domains, sequences, partitioning, rich indexes, RLS/policy, function/trigger, grants

```ts
// db/migrations/0300_audit_log.ts — Trusted/Platform profile only
import { pg } from "@zeroship/migrate/pg";
import { t } from "@zeroship/migrate";

export default {
  up() {
    pg.domain("email_t").create({ base: t.text(), check: (c) => c.value().like("%@%") });
    pg.domain("email_t").setDefault({ value: "" });
    pg.domain("email_t").dropDefault();
    pg.domain("email_t").setNotNull();
    pg.domain("email_t").constraint("email_t_shape").add({ check: (c) => c.value().like("%@%") });
    pg.domain("email_t").constraint("email_t_shape").drop({ ifExists: true });
    pg.domain("email_t").rename({ to: "email_address_t" });

    pg.sequence("audit_log_id_seq", { schema: "control" }).create({ as: t.bigInt(), start: 1 });
    pg.sequence("audit_log_id_seq", { schema: "control" }).alter({ cache: 100 });

    const audit = pg.table("audit_log", { schema: "control" });

    // ---- partitioned create with composite PK (vendor-only fields) ----
    audit.create({
      columns: { /* … t.* … */ },
      primaryKey: { name: "audit_log_pk", columns: ["occurred_at", "id"] },   // composite PK: vendor-only
      partitionBy: { method: "range", columns: ["occurred_at"] },              // range | list | hash
    });

    // ---- partitions ----
    audit.partition("audit_log_2026_07").create({
      bound: pg.bound.range({ from: ["2026-07-01"], to: ["2026-08-01"] }),     // MINVALUE/MAXVALUE: pg.bound.min/max
    });
    audit.partition("audit_log_backfill").attach({
      bound: pg.bound.range({ from: ["2026-01-01"], to: ["2026-07-01"] }),
    });
    audit.partition("audit_log_2026_06").detach({ concurrently: true });

    // ---- vendor index depth ----
    audit.index("audit_log_time_brin").add({
      using: "brin",                                  // btree|hash|gin|gist|spgist|brin|ivfflat|hnsw
      on: [{ column: "occurred_at" }],
      with: { pagesPerRange: 64 },                    // closed per-method storage-param map
      concurrently: true,                             // plan-visible non-transactional phase (§9.4)
    });
    audit.index("audit_log_app_covering_idx").add({
      on: [{ column: "app_id", opclass: "uuid_ops" },
           { expr: (c) => c.fn.lower(c("kind")), collation: "C", order: "asc", nulls: "last" }],
      include: ["kind", "occurred_at"],               // INCLUDE
      unique: true,
      nullsNotDistinct: true,                         // PG 15 UNIQUE NULLS NOT DISTINCT
      where: (c) => c("kind").isDistinctFrom(null),
    });

    // ---- RLS + policy (one options-object terminal replaces four methods) ----
    audit.setRls({ enabled: true, forced: true });
    audit.policy("audit_tenant_isolation").create({
      for: "select",
      to: ["app_runtime"],
      using: (c) => c("app_id").eq(
        c.fn.currentSetting("zeroship.tenant_app", { missingOk: true }).castTo("uuid"),  // Tier-PG nodes
      ),
    });
    audit.policy("audit_tenant_isolation").alter({ to: ["app_runtime", "readonly"] });   // ALTER POLICY
    audit.policy("audit_tenant_isolation").rename({ to: "audit_tenant_iso" });

    // ---- function depth ----
    pg.function("touch_updated_at", { schema: "control" }).create({
      returns: "trigger",
      language: "plpgsql",
      security: "definer",                            // definer | invoker
      set: { search_path: ["control", "pg_temp"] },   // SET config
      strict: true, leakproof: false, parallel: "safe", cost: 10,   // + rows?, volatility
      body: `BEGIN NEW.updated_at := now(); RETURN NEW; END`,       // raw island #2 (§7)
    });

    // ---- trigger depth ----
    audit.trigger("audit_touch").create({
      timing: "before",                               // before | after | insteadOf
      events: [{ update: { of: ["name", "plan"] } }, "insert"],     // UPDATE OF columns
      forEach: "row",
      when: (c) => c.new("plan").isDistinctFrom(c.old("plan")),     // OLD/NEW row refs, trigger-context-only
      execute: { function: "control.touch_updated_at" },
      constraint: { deferrable: true, initiallyDeferred: false },   // constraint triggers
      referencing: { old: "old_rows", new: "new_rows" },            // transition tables
    });
    audit.trigger("audit_touch").setEnabled({ mode: "replica" });   // always|replica|origin|disabled

    // ---- granular grants + role membership + default privileges ----
    pg.grant({ privileges: ["usage"], on: { kind: "sequence", name: "audit_log_id_seq", schema: "control" }, to: ["app_runtime"] });
    pg.grant({ privileges: ["execute"], on: { kind: "function", name: "touch_updated_at", schema: "control", argTypes: [] }, to: ["app_runtime"] });
    pg.grant({ privileges: ["select"], on: { kind: "table", name: "audit_log", schema: "control", columns: ["id", "kind"] }, to: ["readonly"] });
    pg.grantRole({ role: "control_admin", to: ["ops_oncall"], withAdminOption: true });
    pg.defaultPrivileges({ forRole: "control_owner", inSchema: "control" })
      .grant({ on: "tables", privileges: ["select"], to: ["readonly"] });     // ALTER DEFAULT PRIVILEGES
  },
};
```

### 4.6 Views and matviews

```ts
// structured (SelectAst v2: distinct/distinctOn, group/having, aggregates+window,
// set-ops, non-recursive CTEs, subqueries in FROM)
pg.view("daily_rollup").create({
  materialized: true,
  withData: false,                                    // WITH NO DATA
  as: (q) => q.from("usage_events")
              .groupBy(["app_id", (c) => c.fn.dateTrunc("day", c("at"))])
              .select({ app_id: (c) => c("app_id"),
                        day: (c) => c.fn.dateTrunc("day", c("at")),
                        total: (c) => c.agg.sum(c("amount")) }),
});
pg.view("daily_rollup").index("daily_rollup_uq").add({ on: [{ column: "app_id" }, { column: "day" }], unique: true });
pg.view("daily_rollup").refresh({ concurrently: true });   // irreversible op, plan-visible

// beyond the SelectAst line (§7): raw island #3, RawViewBody-gated, parse-guarded single SELECT
pg.view("legacy_report").create({ as: pg.rawSelect("SELECT … LATERAL …") });
```

---

## 5. IR v2 — the one clean break

<!-- Revised 2026-07-02 (operator directive): DO NOT BUMP the ir_version integer. "v2"/"IR v2"
     here is a GENERATION LABEL for the design, NOT the wire integer. The engine is already at
     CURRENT_IR_VERSION = 6 and STAYS 6 through the whole rewrite — the wire shape changes IN
     PLACE. Rationale: pre-launch + committed .ir.json ELIMINATED (build-time/in-memory only,
     never persisted) ⇒ there are NO stored artifacts to version against, so a bump buys nothing.
     The ir_version FIELD + fail-closed version check are KEPT (code-evolution discipline: reject
     a malformed/future artifact loudly via closed-enum deny_unknown_fields) — just never bumped.
     Read every "ir_version: 2" / "v1→v2 break" / "bump" below as "the wire shape changes in place
     at the current integer (6), one destructive shape break, no version increment." -->
**Honest framing of "one break" (MINOR #1).** Precisely: there is **one destructive wire break**
(the v1 authoring shapes are deleted, goldens regenerated once, at P0). After that, the shape is
**additively extended** across P3/P4 as vendor waves land (new `Op`/`Expr` variants, SelectAst v2).
The `ir_version` integer stays **fixed (6, unbumped)** throughout because closed-enum
`deny_unknown_fields` deserialize means each addition is a superset a prior reader fails **loudly**
on, not a silent-compat surface — additive growth is code-evolution discipline (K1), not a
user-compat shim. So "frozen IR" (K1) is true at the **end** of the train; during it, the shape
grows additively under a fixed version. Acceptable pre-launch; the doc says so rather than implying
the shape is inert from P0.

Wire shape changes in place at `ir_version: 6` (no bump); goldens regenerated once; the v1
authoring shapes are deleted (pre-launch, no alias). Hostile
`.ir.json` still fails to parse via closed internally-tagged enums + `deny_unknown_fields` +
`IrScalar` domain enforcement. Checksum = SHA-256 over the canonical serde byte stream, as
today. `dialect_scope`, expression tier, and capability sets are **derived, never serialized
as author input**.

### 5.1 Shape

```rust
MigrationIr {
  ir_version: 2,
  name: Option<String>,
  ops: Vec<Op>,
  flags_override: Option<LockSafetyOverride>,   // checksummed, as today
  version_keys: Vec<VersionKey>,                // deterministic per-phase ids (§9.2)
}
```

One closed `Op` enum, internally tagged, camelCase, partitioned into modules for
maintainability (`op::table`, `op::column`, `op::index`, `op::constraint`, `op::type_evo`,
`op::dml`, `op::online`, `op::vendor::{acl, policy, function, trigger, partition, view, domain,
sequence}`).

**Deleted shapes (unrepresentable, not deprecated):**

- `CreateTable.primaryKey` on the core-reachable type (exists only on the vendor create args).
- `PgRaw.binds` and `IrLowerError::PgRawBindsUnsupported` (§7.2).
- Core-reachable `ViewQuery::Raw` construction (only the vendor recorder emits it; validate
  requires `RawViewBody`).
- The single-column-id-only FK special case (subsumed by the full FK model).
- The `alter({type?, nullable?})` bag (per-intent ops: `SetColumnType { using? }`,
  `SetColumnNotNull`, `DropColumnNotNull`, `SetColumnDefault`, `DropColumnDefault`).
- `IrLowerError::ExprRenderDeferred` and every accepted-then-refused `UnsupportedOp` arm.

**New payloads (the coverage waves, §8):** full `IndexElement`
(`{ target: Column|Expr, opclass?, collation?, order?, nulls? }`) + `include` + per-method
storage maps + `concurrently` + `nullsNotDistinct`; full
`IrForeignKey { columns, references{table,columns}, match?, onDelete?, onUpdate?, deferrable?,
initiallyDeferred?, notValid? }` + `Op::ValidateConstraint`; partition ops (create/attach/
detach{concurrently} + bound model with min/max sentinels); enum evolution ops; PG-scoped
domain/sequence evolution ops under the vendor module;
`GrantTarget` grows `Sequence{name} | Function{name, argTypes} | Type{name} |
TableColumns{table, columns}` (today's schema-wide form renamed `AllSequencesInSchema` etc.);
`Op::AlterDefaultPrivileges`; `Op::GrantRoleMembership { adminOption }`; function options
(security, set-config, strict, leakproof, parallel, cost, rows); trigger depth (update-of,
constraint, transition tables, setEnabled); `Op::AlterPolicy`/`RenamePolicy`;
`Op::RefreshMaterializedView { concurrently }`; SelectAst v2 (§4.6); the multi-phase
`OnlineRename`/`OnlineTypeChange` op-groups. `PgRaw` gains a **required `reason: String`**
that travels inside the checksummed artifact itself (graft from Stance A).

### 5.2 Validity equals *applicability* — the core mechanism (corrected)

<!-- Revised 2026-07-02: addressing critic CRITICAL #1 & #2 — the prior "validity == renderability, lower/render total over IR alone" claim was false for the SQLite live-rebuild class (verified: render/sql_preview.rs:412, render/lower.rs:137). Replaced with a two-mode model that matches the engine. -->

The prior draft claimed "validity == renderability" and "lower/render become total functions
over **validated IR**." That is false for SQLite ALTER-add-constraint / rename / online-type-
change / enum-CHECK-rewrite ops, which the engine reconciles via the live-state-dependent 12-step
rebuild (§2.4). The corrected invariant is **validity == *applicability***: a validated op will
*apply* faithfully on every dialect in its declared scope — but "apply" has **two render modes**,
and the golden-matrix test asserts the correct one per op.

Every `Op` variant and every `Expr` node carries a **static support declaration**:

```rust
impl Op {
  pub const fn support(&self) -> Support;
}
struct Support {
  dialects: DialectSet,        // dialects this op can be APPLIED on
  tier: Tier,                  // Core | Vendor(&'static [VendorCapability])
  contexts: CtxSet,            // expression-position gate (trigger WHEN, SelectAst, …)
  render: DialectMap<RenderMode>,  // per (op, dialect): how it becomes SQL
}
enum RenderMode {
  /// Deterministic from the IR alone. Golden-testable offline. `plan` prints real SQL.
  Offline,
  /// Needs the live table structure (SQLite 12-step rebuild; online rename/type-change
  /// cutover; enum→CHECK rewrite). `plan` prints `-- [runtime-resolved]`; lowering requires
  /// a LiveSchema and is proven against a *synthetic-fixture* LiveSchema in CI, not a golden.
  LiveResolved,
}
enum Tier { Core, Vendor(&'static [VendorCapability]) }
```

- A core **object root** must have a three-dialect portable, creator-safe base form. Individual
  terminals/options still carry their own `support.dialects` (for example, partial indexes and
  positioned enum edits are explicitly matrixed), but a construct with **no** portable base form
  cannot be an `op.*` root with two permanent validate-refusal cells. It must be a `Vendor` op on
  `pg.*`. This is the mechanical rule that keeps `pg.domain`/`pg.sequence` out of the portable root.
- **validate** computes derived `dialect_scope` = ∩ of every op's + every expression node's + every
  **value carrier's** `dialects` set. A `Dialectal<T>` value carrier (§6.4) contributes
  `{ legs present } ∪ (all dialects if a `default` leg is present)`; a bare value contributes all
  dialects. validate refuses out-of-scope targets with structured
  `DIALECT_UNSUPPORTED { op, dialect, suggested_fix }` — at validate, **never lower**. (validate's
  input taxonomy — including the pending-contract facet — is pinned in §9.3; it does **not** read
  the live DB.)
- **lower is a total function over `(validated IR, LiveSchema)`** — not over IR alone. For
  `RenderMode::Offline` ops the `LiveSchema` is unused (the engine passes an empty one, exactly as
  `sql_preview.rs:274` does today). For `RenderMode::LiveResolved` ops lowering **consults the
  LiveSchema** and fails closed (`SqliteRebuildOnly` / `SqliteRenameNeedsLiveTable`) if the needed
  table shape is absent — never emitting a wrong rebuild. The only *unexpected* lower-time errors
  left are engine-bug invariants.
- **Exhaustive matrix test (mode-aware):** a fixture constructor per op variant + per expr node (a
  Rust `match` over all variants with **no wildcard arm**, so adding a variant without a fixture
  fails compile). For each `(op, dialect)` cell CI asserts, per the declared `RenderMode`:
  - `Offline` → renders to a byte-stable **golden** with an empty LiveSchema (must succeed).
  - `LiveResolved` → (a) offline `plan` emits the `-- [runtime-resolved]` label (the honest-plan
    contract, K8), **and** (b) given a **synthetic `LiveSchema` fixture** carrying the ambient
    table shape, `lower_plan` produces the expected rebuild statement stream (golden over
    *IR + fixture*, must succeed). A missing fixture-schema for a `LiveResolved` op fails compile
    the same way.
  - Out-of-scope dialects → validate must **refuse** with the structured code (must fail-closed).

  This is why the "render every instance on every dialect, must succeed" phrasing is replaced: an
  `Offline` op is golden-rendered; a `LiveResolved` op is golden-*lowered over a fixture schema*.
  Both are exhaustive and both are mechanical, but they are **different assertions** — the design no
  longer claims a single offline golden for the rebuild class.
- **Failure-placement ladder:** tsc (discriminated unions, per-intent terminals, tier-split
  builders) → record (structured `OP_INVALID`, envelope lints) → validate (dialect matrix,
  capability, expression context, schema scope, pending-contract §9.3) → lower (only the
  live-schema fail-closed refusals for the `LiveResolved` class, which are *not* validity errors —
  they are "this rebuild needs the live table, which is present at apply time").

---

### 5.3 Reversibility taxonomy for the new op surface (K9 resolved)

<!-- Revised 2026-07-02: addressing critic MAJOR #5 + Missing Concept #1 — K9 ("auto-derived down only for fully reversible op lists") was unresolved for the ~2× new op surface; enum addValue was even marked portable-Core while being fundamentally irreversible in PG. -->

K9 stands: auto-`down` is derived **only** for fully reversible op lists; lossy/irreversible ops
never get a fabricated inverse. Every `Op` variant carries a **static `reversibility()`** class
(exhaustive Rust match, no wildcard — a new op without a class fails compile), and `down`-derivation
consults it. If **any** op in a migration's `up` is `Irreversible`, auto-`down` is **refused** with
`DOWN_NOT_DERIVABLE { op, reason }`; the author must supply an explicit `down` or accept a
forward-only migration.

| Reversibility class | Meaning | New ops in this class |
|---|---|---|
| **Auto-reversible** | inverse is mechanical and lossless | enum `create`/`rename`/`renameValue`; domain `create`/`rename`/`setDefault`↔`dropDefault`/constraint `add`↔`drop`; `setNotNull`↔`dropNotNull`; index `add`↔drop; partition `attach`↔`detach`; `grant`↔`revoke`; `grantRole`↔`revokeRole`; `alterDefaultPrivileges.grant`↔`revoke`; RLS `enable`↔`disable`; policy `create`↔drop; FK/check `add({notValid})` ↔ drop; `validateConstraint` (down = re-mark NOT VALID, lossless) |
| **Conditionally reversible** | reversible only with captured prior state | `setType` (down needs the prior type + a `using` back-cast — derivable only when the cast is known-lossless, e.g. `int→bigint`; lossy narrowings are `Irreversible`); `setDefault` over an existing default (down needs the old default); `rename` online (down = reverse-phase, engine-owned) |
| **Irreversible** (no auto-`down`, must be explicit or forward-only) | inverse loses information or the dialect forbids it | **enum `addValue`** (PG cannot DROP an enum value); `refresh` matview; `dropColumn`/`dropTable`/`del` and all DML (unchanged from K9); partition `detach({concurrently})` data reattachment; `setType` lossy narrowing; `pg.raw` (opaque) |

The taxonomy is a **column in the §8 coverage checklist** — every new op states its class, tested by
a `down`-derivation unit per op. This closes K9 for the whole new surface rather than leaving it
implicit.

## 6. Cross-dialect values: two layers (portable intent nodes + a per-dialect escape)

<!-- Revised 2026-07-02 (operator decision + design-critic pass): the TWO-LAYER model governs
     dialect-divergent VALUE positions (expressions, column types, defaults, index methods, storage
     options) — NOT whole ops. Op-level dialect availability stays the separate op-capability Tier
     (§5.2 `Tier{Core,Vendor}`, unchanged): a whole PG-only op like `createPolicy`/`enableRls`/
     `grant`/`createFunction`/`domain` is not a "value with legs" and cannot be wrapped by `dialect`.
     The single-leg collapse below applies ONLY to PG-only VALUE nodes. The one combinator is
     `dialect({...})` (the earlier `.on()` fluent form was dropped — it and the map are isomorphic;
     the combinator is position-agnostic + unambiguous). -->

**Two axes, kept distinct.** (1) **Op-capability tier** (§5.2 `Tier{Core, Vendor}`): whether a whole
op is portable-core or PG-vendor, refusing on unsupported dialects — unchanged by this section.
(2) **Value portability layer** (this section): how a *value inside* an op (a type, default,
expression, index method) handles dialect divergence. The two are orthogonal; the rest of §6 is
about axis (2).

**The value model in one line:** a value is either **engine-proven-portable** (Layer 1) or
**author-asserted per-dialect** (Layer 2); `raw` is the last resort; author-time dialect
branching is forbidden.

- **Layer 1 — portable *intent* nodes (engine-owned mapping + parity proof).** The author
  declares intent; the engine renders it correctly per dialect *and owns the equivalence proof*.
  `col.json()` → jsonb/json/text; `col.uuid()` → uuid/char(36)/text; `fn.now()`;
  `membership(x, [...])` → `IN`; `dayOfMonth(e).eq(1)` → EXTRACT/DAYOFMONTH/strftime. This is the
  **preferred** path: the guarantee is engine-proven (not author-asserted) and the surface is
  clean (the author never sees dialects). The cost — the three-dialect parity proof — is paid
  **once, by us**, and amortized across every user. Growing this node set is the primary way we
  shrink divergence. (This is the closed AST of §6.1, and the same intent principle extends to
  types/defaults/index-methods.)
- **Layer 2 — the per-dialect escape `dialect({ default?, pg?, sqlite?, mysql? })`, author-asserted.**
  For the long tail no intent node covers. A portable base (the `default` leg) plus selective
  per-dialect overrides (§6.4). Because the author supplies the divergent legs, the engine can no
  longer *prove* equivalence — so Layer 2 carries `raw`-like discipline: **`reason`-marked,
  leaf-granularity, no nesting, and its own budget** — a **separate `dialect`-budget** (distinct
  from the `raw` budget), gated on **creator** migrations (§7.1), because a `dialect` override is a
  "not-yet-a-proven-intent-node" debt that should trend to **zero** as Layer 1 grows, whereas `raw`
  has a sanctioned-forever floor. A migration's cross-dialect debt is measurable as the
  `dialect`-override count; its raw-escape debt is the `raw` count — two distinct signals, not one.
- **A PG-only *value* node = the degenerate single-leg case** — a `dialect({ pg: … })` with no other
  leg and no `default` refuses on SQLite/MySQL via the normal validity==applicability mechanism
  (§5.2, §6.4). This is only about *value* positions; it does **not** subsume whole PG-only ops
  (those stay op-capability `Vendor` tier per the note above). §6.2's PG-only expression nodes are
  this value-level case.
- **Forbidden: author-time dialect branching** (`up(m, { dialect }) { if (dialect==='pg') … }`).
  It destroys the single-source multi-dialect IR — the recorded artifact becomes dialect-specific
  and loses checksum-once + render-all-three-from-one-source. Ruled out at the surface (the
  migration function is not handed a dialect).

One expression grammar underlies Layer 1 for the *expression* positions — a closed internally-
tagged enum, never parsed from text, serde-rejecting unknown node tags — reused by checks,
generated columns, defaults, policies, partial indexes, exclusion targets, views, trigger
`when`, and DML `set`/`where`. **Layer membership is derived from the node/override set used,
never author-declared.**

### 6.1 Layer 1: portable intent nodes (the closed expression AST)

Admission rule (graft from Stance A): each new portable intent node ships with a **live
three-dialect evaluation parity proof** (PG :5440 / in-process SQLite / MySQL JsDriverBackend)
before it is admitted, following the splitPart-envelope discipline. A node that fails or lacks
its proof is **not admitted as Layer 1** — but it no longer blocks a release, because the author
retains two honest fallbacks: keep the portable base and supply the divergent leg via
**`dialect({ default, pg?, … })`** (Layer 2, §6.4), or use a vendor node where one exists. So the
portable claim stays engine-proven without schedule hostage-taking, and "not yet a proven
intent node" degrades to a *marked, budgeted* per-dialect override rather than silent raw.

- v1 set carried over: 13 binops, 5 unaryops, case, coalesce/nullif/lower/upper/trim/length/
  abs, concat, portable casts, fnSynth concatWs/splitPart/now/genRandomUuid with pinned
  envelopes.
- New: `.in([...])` / `.notIn`, `.between(a, b)`, `.like(pattern, { escape? })` (escape
  semantics pinned), `.isDistinctFrom(x)` (SQLite `IS`, MySQL `<=>` — proven), boolean
  literals, `c.fn.round/floor/ceil`, and interval/date arithmetic as pinned synths:
  `c.fn.dateAdd(e, { days?, hours?, months?, … })` → PG `+ interval`, SQLite
  `datetime(e, '+N day')`, MySQL `DATE_ADD`.
- Trigger row refs `c.old("col")` / `c.new("col")` are **Tier-P but context-gated** (SQLite
  triggers have OLD/NEW natively): valid only in trigger `when`/body positions, enforced by the
  `CtxSet` in the support declaration.

### 6.2 The PG-only node set (the degenerate single-leg Layer-2 case)

Regex (`~`, `~*`, `!~`), `ilike`; JSON/JSONB operators (`->`, `->>`, `#>`, `#>>`, `@>`, `?`,
jsonb path as data, not text); array nodes (array literal, `= any(...)`, `@>`, `&&`);
`castTo(typeName)` arbitrary cast with an identifier-grammar-validated type name — never free
text concatenated into SQL; `currentSetting`/`currentUser`/`sessionUser`; interval literals;
aggregate + window nodes (`c.agg.count/sum/min/max/…`, `.over({ partitionBy, orderBy,
frame? })`) — admitted **only inside SelectAst contexts**, enforced by the same context gate.

### 6.3 The boundary in the type system, both languages

Core expression positions take `(c: ExprBuilder) => Expr`; vendor positions take
`(c: PgExprBuilder) => Expr` where `PgExprBuilder extends ExprBuilder`. Rust-side,
`ScalarFn::CurrentSetting`/`CurrentUser` move out of the shared enum into the vendor extension
set. A core-authored op containing a PG-only node is doubly impossible: the core builder cannot
construct it (tsc), and validate refuses hand-forged IR (`VENDOR_EXPR_DENIED` /
`EXPR_NOT_PORTABLE`, with the exact node and both resolutions in `suggested_fix`).

### 6.4 Layer 2: the `dialect(...)` per-dialect escape — one combinator, every value position

The escape is a **single combinator**, `dialect({ default?, pg?, sqlite?, mysql? })`, usable
wherever a dialect-divergent value is expected — column types, defaults, index methods, storage
options, *and* expressions. (The earlier fluent `.on()` form was dropped: it and the combinator are
isomorphic, and the combinator is position-agnostic — no per-builder chaining — and unambiguous
about which value it modifies.)

```ts
col.type(dialect({ default: "text", pg: "jsonb", mysql: "json" }))   // portable base + overrides
.default(fn.now())                                                   // Layer-1 intent node — no dialect() needed
.default(dialect({ pg: fn.genRandomUuid(), mysql: myUuid }))         // override only where it diverges
.index("logs_ts").using(dialect({ default: "btree", pg: "brin" }))
check("first_of_month", dayOfMonth(period).eq(1))                    // Layer-1 intent node
check("kind_ok", dialect({ default: membership(kind, ALLOWED),       // Layer-1 base (renders IN everywhere)…
                           pg: pgExpr`kind = ANY(ARRAY[...])` }))     // …with a pg-native override if wanted
```

**Shape.** The IR envelope is uniform per wrappable type `T`:
`Dialectal<T> = { default?: T; pg?: T; sqlite?: T; mysql?: T }` — at least one leg present. The
`default` leg is the portable base ("enhance a declaration with a selective override"); omit it to
express "PG-only" (the §6.2 value case) or "PG + SQLite only," etc. A dialect with **no matching
leg and no `default`** makes the value **unavailable on that dialect** → the enclosing op
**validate-refuses that dialect** (§5.2). Concretely, `Dialectal<T>` contributes to the op's derived
`dialect_scope` (§5.2): its dialect set = `{ legs present } ∪ (all dialects if default present)`,
and the op's `dialect_scope` is the intersection over all its values' sets — so the refusal is
*wired*, not just asserted. `Dialectal` never appears inside `RenderMode::LiveResolved` reasoning
(it is a compile-time value selection, orthogonal to live-schema rebuild).

**Guardrails (mandatory — without them the portable claim is hollow):**
- **Layer 1 is the default.** `dialect(...)` is the escape *when* an intent node can't (yet) be
  proven — never a shortcut past finding one. Review + the budget enforce this.
- **`reason`-marked + its OWN budget.** Each `dialect(...)` override carries a `reason` (checksummed,
  like `raw.reason` — §7.2). It counts against a **separate `dialect`-budget**, distinct from the
  `raw` budget (§7.1), and enforced on **creator** migrations (§7.1) — because a `dialect` override
  is *"an intent node we haven't built yet"* debt that must trend to **zero** as Layer 1 grows,
  whereas `raw` has a sanctioned-forever floor. Merging the two would hide the signal. A migration
  therefore reports **two** numbers: `dialect`-override count (cross-dialect debt) and `raw` count
  (raw-escape debt).
- **Leaf granularity, never op-level.** `dialect(...)` wraps a *value* (type/default/expr/method);
  it never wraps a whole `table().create()` per dialect — the structural skeleton (columns, PK,
  index shape, constraint skeleton) stays single-source-portable.
- **No nesting.** A leg is a concrete value — which **may** be a Layer-1 (dialect-polymorphic)
  intent node (e.g. `default: json()`) but **not** another `Dialectal` (flattened at record). A
  Layer-1 node in a leg is still one concrete IR value; it is not nesting.

**IR + determinism.** The checksum hashes the **IR structure**, which never contains per-dialect SQL
*text* — only nodes and `Dialectal` legs. So a `Dialectal` value hashes all its legs (in canonical
key order), and a Layer-1 intent node hashes as *the node* (dialect-neutral), never its per-dialect
expansion. One rule, applied structurally, to both. Consequently a change to a **Layer-1 mapping**
(how `json()` renders on MySQL) does *not* change the checksum — it is an **engine** change, caught
by the per-dialect render goldens (`sql_preview_{pg,sqlite,mysql}` shift), not by the IR checksum.
The two guards are complementary: checksum pins IR determinism (node-level); goldens pin rendering
(expansion-level). `Dialectal` composes with those goldens with **no new golden machinery** — each
dialect's golden simply shows its selected leg.

**Reversibility.** Op reversibility stays the **static per-`Op`-variant class of §5.3** — a
`Dialectal`-wrapped *value* does not change the class (a value leg carries no inverse-information-loss;
that lives in the op — `dropColumn`, lossy `setType`). The one interaction is with the
**Conditionally-reversible** class (§5.3), whose auto-`down` is derivable only when a value-dependent
condition holds (e.g. `setType` — reversible only if the cast is lossless). Because a migration has a
**single, multi-dialect `down`** (not a per-dialect one), down-derivation checks that condition across
**all** the `Dialectal` value's legs and derives auto-`down` **only if every leg passes**; if any leg
fails (e.g. a `Dialectal` type whose `pg` leg is a lossless widen but whose `mysql` leg is a lossy
narrow), auto-`down` is **refused** with `DOWN_NOT_DERIVABLE { op, dialect, reason }` and the author
supplies an explicit (itself multi-dialect) `down`. This is the existing Conditionally-reversible
check run per-leg with an all-must-pass rule — not a new "meet of legs" reversibility class, and it
never makes a non-conditional op's class payload-dependent.

**Coverage note.** The platform schema is PostgreSQL-only, so for the platform-migration payoff
`dialect(...)` is rarely needed — the `pg` leg (or a vendor node) suffices, and the `dialect`-budget
is ~0 there. Its real value is **creator** portable schemas that hit one stubborn divergence — which
is exactly the population the `dialect`-budget gates. It is also what lets the Layer-1 intent-node
set land incrementally: an un-proven node degrades to a budgeted `dialect(...)` override, not a
release block.

---

## 7. The honest structured-vs-raw boundary

**The "no raw SQL" claim is scoped to the core entry, where it is now actually true**:
`@zeroship/migrate` contains no `Raw` type, no sql template, no raw view body. Property A holds
for `op.*`, unconditionally.

The vendor layer documents **exactly three raw islands**, each capability-gated (the existing
`RawSql` / `Function` / `RawViewBody` flags), each parse-guarded via the trusted-toolchain SQL
parser, none creator-reachable:

1. **`pg.raw({ sql, reason })`** — whole-statement escape. `reason` is a required field that
   travels inside the checksummed IR (graft): the justification is part of the artifact, not
   only repo metadata.
2. **`pg.function(...).create({ body })`** — raw plpgsql/sql body under a fully structured
   signature/options shell. Common trigger logic (NEW assignment, guarded checks) is
   expressible structurally via OLD/NEW expression nodes in `when`; raw bodies are for
   genuinely procedural logic. A structured plpgsql AST is explicitly out of scope.
3. **`pg.rawSelect("…")`** — view/matview bodies past the SelectAst line (§4.6). The line:
   no recursive CTEs, no LATERAL, no correlated-subquery-in-expression, no DML-RETURNING —
   corpus-driven, not SQL-completeness-driven, written into the spec rather than negotiated
   per view.

**Sanctioned raw-forever remainder** (named up front so the budget is honest): event triggers,
custom aggregates/operators/opclasses, collations, `ALTER SYSTEM`,
publications/subscriptions. These stay `pg.raw` deliberately; the reference says so.

### 7.1 The raw budget

A CI test counts `pgRaw`/`rawSelect` ops across the ported platform migration set and **fails
above 5** (target 0). Each surviving raw op is listed in an in-repo `raw-budget.toml` with its
reason; the gate fails on unlisted additions. Belt-and-braces with the in-IR `reason`: the
budget file is the ledger, the IR field is the provenance.

**A separate `dialect`-budget (§6.4), enforced where the code lives.** `Dialectal(...)` overrides
are counted **independently** of raw. They are kept apart on purpose (H3): a `dialect` override is a
*"Layer-1 intent node not yet built"* debt that should trend to **zero** as the intent-node set
grows, so its budget ratchets **downward** over releases, whereas `raw` has a sanctioned-forever
floor — merging them would let a stable raw floor mask a growing portability gap.

The enforcement point differs by *whose* migrations they are, because the platform holds **no
creator codebases** (build-local→deploy; AGENTS.md) so this repo's CI cannot see creator migrations
(design-critic NEW-2):
- **Platform's own migrations** (`db/migrations-ts/`) — a repo CI gate against a `dialect-budget.toml`,
  exactly like the raw gate. The platform is PostgreSQL-only, so this count is ~0 by construction.
- **Creator migrations** — computed at the creator's **local build** by the vite-plugin/CLI (the
  same pass that emits the IR). It is surfaced to the author as a **lint/count** and is available as
  an **opt-in hard gate** in the creator's own project config/CI; the platform additionally records
  the per-migration `dialect`-override count at **deploy** as **telemetry** (a soft signal shown to
  the creator, e.g. "this migration is non-portable in N places"), never a deploy block. So the
  guardrail runs on the population that actually triggers it, without the platform pretending to CI
  code it does not hold.

Each override's `reason` is in the checksummed IR (§7.2 treatment, applied to `Dialectal.reason`),
so the ledger and the provenance agree for both escape kinds, in both locations.

### 7.2 `pg.sql` binds: resolved by unrepresentability

The v1 shape advertised binds that `validate.rs:748` rejected unconditionally — the worst kind
of API lie. Resolution: **`binds` is deleted from the TS types AND the IR**, the `pg.sql`
tagged template is deleted with it (any `${}` interpolation would be a tsc error anyway), and
`PgRawBindsUnsupported` dies. Rationale: migrations are static, operator-trusted DDL-shaped
statements; structured `insert`/`update`/`del` cover parameterized DML; raw never needs binds.
No third state remains.

---

## 8. Coverage matrix (construct → structured?)

Scoping instrument and acceptance corpus: **this repo's Liquibase changelog** (`db/changelog` —
`control`/`auth`/per-app; sandbox `V0011`–`V0024` are **excluded**, owned by the sibling
`zeroship-sandbox` repo per AGENTS.md, §header note).

<!-- Revised 2026-07-02: addressing critic MAJOR #1 — the "normalizer" was the load-bearing, unspecified component of the primary exit gate. Specified as a catalog-level comparator with an explicit semantic/cosmetic boundary. Grounded in Atlas/migra, which compare parsed catalog objects, not text. -->
**Exit gate: catalog-level zero-diff, not text-diff.** Apply the ported `.ts` set and the Liquibase
baseline to two fresh DBs; `pg_dump --schema-only` **both**; **parse each dump into a normalized
catalog model** (the same object model `drift` uses, §10.3) and assert **structural equality**.
This follows Atlas/migra, which compare *parsed schema objects*, not dump text — the only defensible
way to get to zero without a normalizer so aggressive it hides real divergence. The boundary is
**enumerated in the spec, not left to a hand-wave**:

- **Cosmetic — normalized away (safe):** whitespace/newlines; statement *order* (re-sorted by the
  dependency topo-order of §8.1); identifier quoting when semantically identical; `public.`-schema
  qualification; system-assigned constraint/index names **iff** the constraint's semantics (columns,
  predicate, type) are identical (name-independence is opt-in per object class, off by default for
  named platform constraints); default-expression *spelling* only when the parsed default AST is
  equal (`0` vs `0::integer` vs `(0)`); trailing-semicolon and `ONLY` decoration.
- **Semantic — NEVER normalized (a real diff, always fails the gate):** column set, column type,
  nullability, default *value* (parsed AST inequality), constraint predicate/columns/action, index
  method/columns/predicate/uniqueness, ownership/grant/role membership, RLS enabled/forced, policy
  USING/WITH CHECK/roles/command, function signature/security/volatility/SET, trigger timing/events,
  partition strategy/bounds, sequence parameters.

The **raw `diff -u` of the two dumps is retained as the audit artifact** on every run (even at zero
structural diff), so a reviewer can see exactly what the comparator normalized. A normalization rule
is only added with a test showing the two inputs are semantically identical.

This `pg_dump` comparator is the **Postgres corpus gate**. It is not reused for MySQL; MySQL drift
uses a dialect-specific catalog introspector (§10.3, §12.1) that normalizes `information_schema` /
`SHOW CREATE` output into the shared `SchemaState` object model for MySQL-supported constructs.

**Debugging a non-zero diff (per-changeset bisection, Missing Concept #2).** With 100+ ported
changesets a single non-zero cell is otherwise a needle in a haystack. The gate runs **incrementally**:
it replays the Liquibase baseline and the `.ts` port **changeset-by-changeset in dependency order**,
snapshotting the catalog model after each, and reports the **first changeset index** whose post-state
diverges, with the specific object + field. Attribution is per-changeset, not per-corpus.

Legend: **Core** = creator-reachable structured (`op.*`) whose object roots have a portable base
form; per-terminal/option support is shown by the dialect cells. **Vendor** = structured on
`pg.*`, capability-gated; **Raw** = sanctioned raw island. Per-dialect apply cell:
**✅** = renders offline (deterministic, golden-tested, `RenderMode::Offline`);
**🔁** = applies via the **live rebuild** (`RenderMode::LiveResolved`, §5.2 — offline `plan` shows
`-- [runtime-resolved]`, lowered against the live/fixture table shape, no offline golden);
**⛔** = structured **validate-time** refusal (never lowered).

| Construct | Authoring | PG | SQLite | MySQL |
|---|---|---|---|---|
| Tables/columns, `t.*` lexicon, generated/identity cols | Core | ✅ | ✅ | ✅ |
| Composite + non-id FKs, MATCH, ON DELETE/UPDATE | Core (MATCH: Vendor) | ✅ | ✅ create / 🔁 add | ✅ |
| FK DEFERRABLE / INITIALLY DEFERRED | Vendor | ✅ | ⛔ | ⛔ |
| Rendered CHECK constraints (create + add) | Core | ✅ | ✅ create / 🔁 add | ✅ |
| NOT VALID → VALIDATE CONSTRAINT two-step | Vendor | ✅ | ⛔ | ⛔ |
| Composite uniques; UNIQUE NULLS NOT DISTINCT | Core; Vendor | ✅ | ✅ create / 🔁 add; ⛔ | ✅; ⛔ |
| Exclusion constraints | Vendor (as today) | ✅ | ⛔ | ⛔ |
| Index: per-element order/nulls, expression, partial, unique | Core | ✅ | ✅ | partial ⛔ |
| Index: brin/hash/spgist/gin/gist/ivfflat/hnsw, opclass, collation, INCLUDE, storage params | Vendor | ✅ | ⛔ | ⛔ |
| Index `concurrently` / `online` build | Core hint / Vendor explicit | ✅ (non-txn step) | ✅ (plain) | ✅ (plain) |
| Partitioning: partitionBy range/list/hash, create/attach/detach(+concurrently), bound helpers | Vendor | ✅ | ⛔ | ⛔ |
| Enum create; append value (no position) | Core | ✅ | ✅ create / 🔁 addValue | ✅ create / ✅ append (MODIFY-rewrite) |
| Enum addValue **with `after`/`before`**; renameValue | Core (positioned: PG+SQLite); rename Core | ✅ | 🔁 (CHECK rewrite) | ⛔ positioned; renameValue ⛔ |
| Domain create + full evolution (default/not-null/constraints/rename) | Vendor (`pg.domain`) | ✅ | ⛔ | ⛔ |
| Sequences create/alter/drop | Vendor (`pg.sequence`) | ✅ | ⛔ | ⛔ |
| Grants: single table/sequence/function/type/column targets | Vendor | ✅ | ⛔ | ⛔ |
| Role membership (+ ADMIN OPTION), ALTER DEFAULT PRIVILEGES | Vendor | ✅ | ⛔ | ⛔ |
| Roles, schemas, extensions | Vendor (as today) | ✅ | ⛔ | ⛔ |
| RLS enable/force, policy create/alter/rename/drop | Vendor | ✅ | ⛔ | ⛔ |
| Functions: structured signature + security definer/invoker, SET config, strict/leakproof/parallel/cost/rows/volatility | Vendor (body = raw island) | ✅ | ⛔ | ⛔ |
| Triggers: UPDATE OF, constraint, transition tables, setEnabled, WHEN with OLD/NEW | Core WHEN portable; depth Vendor | ✅ | WHEN ✅; depth ⛔ | WHEN ✅; depth ⛔ |
| Views: SelectAst v2 (distinct/group/having/agg/window/set-ops/non-recursive CTE/subquery-FROM) | Core basic; v2 depth Vendor | ✅ | basic ✅ | basic ✅ |
| Matviews: create WITH NO DATA, indexes, REFRESH [CONCURRENTLY] | Vendor | ✅ | ⛔ | ⛔ |
| Expressions — Layer 1 portable value nodes (in/between/like/isDistinctFrom/dateAdd/…) | Core | ✅ | ✅ | ✅ |
| Expressions — PG-only value nodes (single-leg §6.4: regex/ilike/json/array/castTo/currentSetting/agg+window) | Vendor | ✅ | ⛔ | ⛔ |
| DML insert/update/del (mandatory where), backfills | Core | ✅ | ✅ | ✅ |
| View bodies beyond SelectAst line | **Raw** (rawSelect) | ✅ | ⛔ | ⛔ |
| Function/trigger bodies | **Raw** (parse-guarded) | ✅ | ⛔ | ⛔ |
| Event triggers, custom aggregates/operators/opclasses, collations, ALTER SYSTEM, pub/sub | **Raw-forever** (pg.raw) | ✅ | ⛔ | ⛔ |

Per-construct exit checklist (success criterion): every row has (a) an authoring form; (b) a golden
PG render (PG is `Offline` for every non-CONCURRENTLY op); (c) for each **✅** cell a byte-stable
offline golden; for each **🔁** cell a `-- [runtime-resolved]` offline `plan` line **plus** a
golden lowering over a synthetic `LiveSchema` fixture (§5.2); (d) a structured **validate-time**
refusal on every **⛔** dialect.

**Does the creator core grow?** Modestly. The `op.*` object roots keep portable base forms; richer
terminals/options are admitted only with explicit dialect cells in the matrix, so narrowing is
visible at validate instead of hidden in lower. All-three end-state additions include per-element
index order/nulls, rendered checks/composite-FKs/uniques (offline on CREATE, live-rebuild `🔁` on
SQLite ALTER — §5.2), the Tier-P expression additions, enum `create` + **append**-value, and trigger
OLD/NEW WHEN. Narrower core options are named rather than implied: partial predicates over Tier-P
are PG+SQLite with MySQL refusal, and positioned enum value edits / `renameValue` are PG+SQLite with
MySQL refusal (§4.4/§8). Pure index-build hints like `online` are a separate same-end-state
apply-strategy carve-out (§9.4); phase-generating online rename/type-change are not. BRIN/INCLUDE/
opclass/hash/spgist/storage-params/partitioning/matviews are vendor-only — a creator multi-tenant
event table gets them, if ever, via a future platform-managed apply policy, not authored PG-only ops
that would break the SQLite dev tier.

**Fold boundary (corrected, critic CRITICAL #4):** the `env.db.ts`/`schema.runtime.json` fold
consumes `SchemaState::row_shape_projection()` (§10.2) — a **projection** of the full-catalog fold
that keeps only row-shape-affecting core facts. Vendor ops (policies, grants, partitions, matviews)
never alter row shapes and so are absent from the projection and invisible to generated app types —
but they **are** in the full `SchemaState` that `drift` guards. Stated rule with a projection-
stability test. (The prior "fold consumes only the core op set" wording was wrong: it would have
made `drift` structurally blind to the entire operator surface.)

### 8.1 Object dependency ordering (apply correctness + pg_dump equivalence)

<!-- Revised 2026-07-02: addressing critic Missing Concept #5 — object dependency ordering was entirely absent, yet it is load-bearing for both apply correctness and the catalog-equivalence gate. -->

Both the executor (apply order) and the §8 comparator (statement re-sort) need a **deterministic
dependency topological order** over objects, because the platform schema has real cross-object
dependencies pg_dump itself resolves: **extensions → schemas → roles → types/domains/enums →
sequences → tables → table constraints → functions → triggers → views/matviews → policies →
grants/default-privileges**. Rules:

- The IR carries **explicit dependency edges** where a construct references another (a trigger's
  `execute.function`, an FK's `references.table`, a policy's `using` calling a function, a default
  citing a sequence). `validate` builds the DAG and **rejects cycles** with a structured
  `DEPENDENCY_CYCLE` code (with the cycle path in `suggested_fix`).
- Within a single migration the executor applies ops in **topo order**, not authoring order, so an
  author need not hand-sequence function-before-trigger. Ties break by a stable key (object class
  rank, then name) so the order — and thus the executor stream and the comparator's re-sort — is
  reproducible.
- Extension-owned objects (types/functions a `CREATE EXTENSION` installs) are treated as
  **pre-existing** once the extension op is folded, so they are neither re-created nor flagged as
  drift.
- The comparator (§8) re-sorts **both** dumps into this same topo order before structural compare,
  so statement-order churn is a *cosmetic* normalization with a principled basis rather than a
  fuzzy text sort.

---

## 9. Migration safety: expand/contract as first-class constructs

### 9.1 The constructs

```ts
// NOT VALID → VALIDATE adoption: two committed migrations; plan renders both phases
pg.table("orders").check("orders_total_nonneg").add({ expr: (c) => c("total").gte(0), notValid: true });
// …later migration:
pg.table("orders").check("orders_total_nonneg").validate();      // VALIDATE CONSTRAINT (share-lock only)
pg.table("orders").foreignKey("orders_user_fk").add({ …, notValid: true });   // same two-step

// Online column rename: ONE authored op; the ENGINE owns the phases and plan prints them
op.table("users").column("name").rename({ to: "full_name", online: true });
//   plan:  -- phase 1/3 [expand]   ADD COLUMN full_name…; dual-write trigger…
//          -- phase 2/3 [backfill] -- [runtime-resolved] windowed by pk
//          -- phase 3/3 [contract] pending — owed to a later approved deploy (interlock)

// Online type change: shadow column + dual-write + backfill + swap, same phasing
op.table("users").column("score").setType({ type: t.bigInt(), online: true, batchSize: 5000 });

// Concurrent index build: non-transactional phase (§9.4)
pg.table("events").index("i").add({ …, concurrently: true });
```

The module shape stays `export default { name?, up, down? }`. The crash-safe resumable cursor
backfill executor, the PR9a pending-contract interlock, and the split
statement_timeout(60s)/lock_timeout(3s) envelope with checksummed per-migration overrides all
carry over unchanged.

### 9.2 Per-phase version keys → approval scoping interface

Each phase derives a **deterministic, re-lower-stable `version_key`** carried in IR v2. The
control plane's per-version approval scoping (the remaining production gate for online-rename
go-live) approves `{migration_checksum, phase}` pairs; `plan` emits
`requires_approval: [{ phase, reason }]`. This design specifies the keys and the interface;
the approval UI/flow is the parallel control-plane track. The production-gate regression pin
(`crates/control/tests/deploy_migrate_test.rs:643`) stays until that track lands.
For phase-generating online op-groups, the `online: true` choice is part of the canonical IR and
therefore part of `migration_checksum`; otherwise an approval for an offline rename/type-change
could be replayed against an expand/backfill/contract plan with a different phase structure.

<!-- Revised 2026-07-02: addressing critic MAJOR #3 — the headline "expand/contract as first-class constructs" cannot fully go-live within this design; being explicit about the cross-track dependency instead of presenting SC11 as fully shippable. -->
**Cross-track dependency (stated, not buried — MAJOR #3).** This is an **explicit scope boundary**:
what ships in *this* design is the **authoring + engine half** — the constructs, `version_key`
derivation, `requires_approval` plan output, and the validate + apply interlock, all exercised
end-to-end **in the migrate test harness on live PG**. What does **not** ship here is production
**go-live** of online-rename, which stays blocked by the `deploy_migrate_test.rs:643` gate pending
the control-plane approval-flow track (unowned by this proposal). Consequently online-rename is, by
design, **"authorable and engine-complete here; production-enabled there."** SC11 is worded to that
boundary (harness-level, not production go-live), and the two tracks share the `version_key`
interface this design freezes — so the control-plane track has a stable contract to build against.
We do **not** claim the feature is fully live at the end of this PR-train.

### 9.3 Interlock hoisted to validate — and the validate-input taxonomy (graft from Stance C)

<!-- Revised 2026-07-02: addressing critic CRITICAL #3 — the prior draft said validate is pure and "the live DB is consulted only by drift," yet also had validate check pending-contract state (a cross-migration fact). Pinning what validate reads resolves the contradiction: the fold, not the live DB. -->

**What validate reads (pinned taxonomy).** validate is a pure function of exactly two inputs:

1. **the migration IR under validation** (the single `.ts` module's recorded ops), and
2. **the `SchemaState` fold of the already-applied migration set** (§10.2) — specifically its
   `pending_contracts` facet: the set of `{table, phase}` markers left open by prior online-rename/
   type-change *expand* phases whose *contract* has not yet landed.

Neither input is the live database. The fold is the deterministic left-fold of prior committed
migrations — a build-time/in-memory artifact (K2), the same object `gen-types` and `drift` consume.
So validate stays pure *with respect to its inputs*, and §10.3's "the live DB is consulted here and
only here [drift]" holds: **the pending-contract check is a fold read, not a catalog read.** This
is the resolution to the apparent K7 conflict — K7 forbids live-catalog *type binding*, not passing
the deterministic fold into validate.

**The interlock.** Given those inputs, validate refuses — with structured
`TABLE_HAS_PENDING_CONTRACT` — any op touching a table with an open contract marker in the fold,
**earlier** than the apply-time check (which stays as the backstop for the case where the fold and
the live journal disagree — caught by `drift`'s journal-state check, not by the catalog comparator;
§10.3). This matches the failure-placement ladder: the earliest layer that has the information,
catches it. (The apply-time check *does* run against the live journal; that is the executor's
concern, not validate's.)

### 9.4 Non-transactional executor step class

`CREATE INDEX CONCURRENTLY` / `DETACH PARTITION CONCURRENTLY` cannot run inside a transaction.
The executor gains a **non-transactional step class**: hoisted out of the per-step txn,
journaled with an in-progress marker, recovered by drop-and-rebuild on crash (standard
CONCURRENTLY recovery). Live-PG crash tests are part of the wave that ships it; everything
before that wave stays transactional as today.

<!-- Revised 2026-07-02: addressing critic Missing Concept #4 — advisory-lock hold duration under a long CONCURRENTLY build was unaddressed; the prior "still under the advisory lock" would block all project deploys for the build's duration. -->
**Advisory-lock yielding under CONCURRENTLY (Missing #4).** A `CREATE INDEX CONCURRENTLY` can run
for many minutes; holding the **project advisory lock** for its whole duration would block every
other deploy for that project. The executor therefore **yields the advisory lock during the
concurrent build**: it (1) takes the lock, writes the `index-building` in-progress journal marker,
and releases the lock; (2) runs the build **without** holding it; (3) re-takes the lock to flip the
marker to `done`/journal the result. Mutual exclusion during the build is provided by the
**object-name journal marker** (a second deploy targeting the same index name refuses with
`OBJECT_BUILD_IN_PROGRESS`), not by the global lock — so unrelated deploys proceed. Crash during the
build leaves the marker + a possibly-`INVALID` index; recovery drops and rebuilds (standard PG
CONCURRENTLY recovery). This is the gh-ost/pg-online-schema-change philosophy: never hold a
schema-wide lock across a long build.

<!-- Revised 2026-07-02: addressing critic MAJOR #2 and final CRITICAL #2 — online:true in the CORE tier put dialect-divergent atomicity in the tier that is supposed to guarantee identical behavior, and the draft then over-corrected by checksum-excluding flags that generate expand/contract phases. Split the taxonomy. -->
**Two classes of "online/concurrently" flags.** The critique is right that PG-CONCURRENTLY-vs-plain
is a *per-dialect atomicity divergence*, and burying it inside a "core" op contradicts the tier's
identical-behavior thesis. The corrected rule is not "all online flags are hints"; it is:

- **Pure apply hints are checksum-excluded.** `index(...).add({ online: true })`,
  `pg.table(...).index(...).add({ concurrently: true })`, and comparable non-transactional
  statement-strategy choices produce the **same end-state catalog** as their plain form. The hint
  changes only how the executor reaches that state: PG builds CONCURRENTLY (outside the txn
  envelope, non-atomic, retryable), while SQLite/MySQL use their plain in-envelope build where
  applicable. These pure hints live on executor directives, are **not part of op IR identity or
  checksum**, are excluded from `dialect_scope` intersection and fold/shape-compare, and are marked
  `RenderMode`-orthogonal in the support declaration.
- **Phase-generating online op-groups are checksum-included.** `rename({ online: true })` and
  `setType({ online: true })` do **not** have the same IR identity as their offline forms. They
  lower to expand/backfill/contract phases, create `pending_contracts`, derive per-phase
  `version_key`s, and change the reversibility class (§5.3). Therefore the `online: true` choice
  is part of canonical IR identity, checksum, fold state, approval keying (`{migration_checksum,
  phase}`), and drift's journal-state check. Stripping it from identity would make an offline op
  and an online op share an approval/checksum while requiring different phase contracts.

The strict core portability claim is scoped accordingly: pure index-build hints are a bounded,
documented apply-strategy carve-out over the same end state; phase-generating online rename/type
change are first-class semantic op-groups with their own checksummed identity.

---

## 10. Determinism, the fold, drift, and policy configs (grafts)

### 10.1 One recorder artifact (parity by construction)

The TS package's recorder compiles to one dependency-free ESM artifact
(`sdks/migrate/dist/recorder.mjs`) that the engine `include_str!`s — the exact pattern the
runtime crate already uses for `sdks/bootstrap/dist/runtime-entry.js`. The hand-mirrored
`frontend/migrate_ops.js` twin is deleted. Byte parity becomes build provenance.
**Transition plan (graft from Stance A):** the byte-parity golden test survives one release as
a tripwire across the include_str! embedding boundary (guarding the stale-dist failure mode),
then flips to asserting artifact identity.

Canonical serialization, single cross-impl checksum, structural-allowlist validation,
closed-enum deserialize rejection, determinism lint + bare-native-symbol translation, honest
`plan` labels — all carry into v2 with regenerated goldens and property tests.

### 10.2 First-class `SchemaState` fold — full catalog, with a core projection (graft from Stance C)

<!-- Revised 2026-07-02: addressing critic CRITICAL #4 — a core-only fold cannot back drift for the platform schema (mostly vendor objects). The fold is now full-catalog; gen-types consumes a *projection* of it. -->

The prior draft scoped the fold to core ops only, which is incoherent with `drift` comparing it to
the live catalog: the platform schema is *overwhelmingly* vendor objects (roles, grants, RLS,
policies, functions, partitions), so a core-only fold would flag every vendor object as spurious
drift. Corrected model — **one full-catalog fold, two consumers via projection**:

- **`SchemaState` (full).** A first-class, canonically-serialized, checksummed derived structure —
  **never a committed file** (transient/in-memory, test goldens only, K2). It folds **every** op,
  core **and** vendor: tables/columns/constraints/indexes **plus** roles, grants, policies, RLS
  flags, functions, triggers, partitions, sequences, extensions, and the `pending_contracts` facet
  (§9.3). This is the object model a full catalog can be compared against, and exactly the left
  input a future declarative desired-state layer would consume — keeping Stance C's product open as
  a later additive layer without buying the differ today.
- **`SchemaState::row_shape_projection()` — the fold boundary.** `gen-types`
  (`env.db.ts`/`schema.runtime.json`) consumes a **deterministic projection** that keeps only
  row-shape-affecting core facts (tables, columns, types, nullability, generated columns) and drops
  everything that never alters app-visible row shapes (policies, grants, partitions bounds,
  matviews, RLS, function/trigger bodies). This is the "vendor ops are invisible to generated app
  types" rule from §8 — but stated correctly as a **projection of the full fold**, not a
  scoped-down fold. A test pins that the projection is stable and vendor-blind.

So the coverage-critique concern is resolved: the fold *sees* the whole operator surface (so `drift`
guards it), while gen-types *sees* only what changes app types.

### 10.3 `drift` command/gate (graft from Stance C)

A standalone `zeroship-migrate drift` runs two checks against live state:

1. **Catalog drift.** It introspects the **live catalog** and compares it against the **full
   `SchemaState`** (§10.2), across both the core and vendor object surfaces (roles, grants,
   policies, functions, partitions included — the whole point, since that surface is what the
   platform schema is *made of*). Comparison is **catalog-level, not textual**: each dialect has an
   introspection adapter that normalizes live objects into the shared `SchemaState` object model,
   then applies the same semantic-vs-cosmetic boundary as the §8 Postgres corpus gate.
   - **Postgres adapter:** `pg_dump --schema-only` parsed into the normalized catalog model, with
     the §8 cosmetic rules (`public.`, `ONLY`, cast spelling, topo-order sorting).
   - **MySQL adapter:** `information_schema` plus `SHOW CREATE TABLE`/`SHOW CREATE VIEW` where
     MySQL exposes semantics only textually (enum token order, generated columns, some default/check
     spellings), normalized for the MySQL-supported subset: tables, columns, nullability/defaults,
     generated columns, indexes, checks, uniques, FKs, basic views, and enum column definitions.
     Its cosmetic rules are MySQL-specific (backtick quoting, display width noise, engine/
     auto-increment counters when not authored, collation display defaults); it deliberately does
     **not** reuse `pg_dump`-shaped rules like `public.`, `ONLY`, or `::integer`.
2. **Journal-state drift.** It reads the migration journal's open phase rows and compares them to
   `SchemaState.pending_contracts`. A mismatch fails with
   `JOURNAL_DRIFT_PENDING_CONTRACT { table, fold_phase, journal_phase }`. This is the backstop for
   expand/contract interlocks: during expand the live catalog may correctly show both columns and
   a dual-write trigger, so a catalog comparator cannot tell whether contract is still pending.

Drift is a **structured error, never auto-reconciled**; the live DB is consulted **here and only
here** in the whole system. Objects the fold cannot model (sanctioned raw-forever remainder, §7) are
enumerated in an explicit `drift-ignore.toml` allowlist so they are *knowingly* excluded rather than
silently missed.

### 10.4 Capability presets as symmetric policy-config files (graft from Stance C)

The confined/operator/local `VendorCapabilities` presets serialize as symmetric policy-config
files from day one. Gates already key on capability flags, never profile names, so this costs
little and pre-positions the 2026-06-30 migrate-engine-server direction
(effective = operator_ceiling ⊓ creator_draft, monotonic-tighten) **without dissolving
`OperatorCapability` prematurely** — that dissolution is the engine/server track's move, not
this design's. `SchemaScope::None → confined` stays the least-privilege default; the double
gate stays; the subpath import remains explicitly not the security gate.

<!-- Revised 2026-07-02: addressing critic MINOR #3 — introducing a file-driven capability surface for a confinement system without a trust model is a privilege-escalation path; specifying the trust model. -->
**Trust model for the policy-config files (MINOR #3).** A file-driven capability surface for the
system whose *whole purpose* is creator confinement needs a stated trust model, or the file becomes
an escalation path. The rules:

- **The file is a *ceiling expression*, never a source of authority.** Effective capability =
  `compiled_in_OperatorCapability_ceiling ⊓ file` (monotonic-tighten only, per the 2026-06-30
  engine-server direction). A policy file can **only subtract** capability; it can never grant a
  capability the compiled-in `pub(crate)` ceiling does not already hold. So a tampered or forged
  file **cannot escalate** past the code-level ceiling — the worst it can do is over-restrict.
- **Provenance is server-side and integrity-checked.** In the managed platform the effective policy
  is **injected by the server** from a trusted deploy path, not read from creator-writable
  locations; the file is **checksummed** and its checksum travels with the deploy record. Creator
  `.zship` bundles cannot carry a policy file that widens their confinement.
- **`SchemaScope::None → confined` remains the default when no policy is resolved** — absence
  fails *closed* to least privilege, never open.
- The symmetric-config framing (operator drafts like the server) is a *convenience for authoring
  and review*, explicitly **not** a claim that the file is the security boundary — the boundary is
  the compiled ceiling ⊓ server-injected policy, exactly as the double gate is today.

### 10.5 Package disambiguation + backfill convergence

`@zeroship/migrations` → **`@zeroship/backfill`** in the same PR-train: rename outright, all
callers updated, no alias (pre-launch stance). <!-- Revised 2026-07-02: addressing critic MINOR #5 — the substantive "one closed expression language" convergence is punted to a follow-up; not claiming it lands here. -->
**What lands here vs. later (MINOR #5).** The **rename** lands in this train and is complete. The
**convergence** — `migrateOne`'s transform fragments adopting the Tier-P expression nodes so online
backfills and migration backfills share one closed AST — is an **explicit follow-up, out of scope
for this design**. Until it lands, "one closed expression language" is the *destination*, not a
claim about this PR-train's end state: this train ships the rename and the enlarged Tier-P AST; the
backfill package keeps its current transform surface until the follow-up wires it to Tier-P.

---

### 10.6 The docs-generation pipeline, concretely (Missing Concept #6)

<!-- Revised 2026-07-02: addressing critic Missing Concept #6 — the doc asserted a generated-docs CI gate (finding #1's fix depends on it) but never specified the generator. -->

Finding #1's fix (docs↔exports drift becomes a build failure) depends on a generator that was
asserted but undefined. Concrete spec:

1. **Source of truth = two machine artifacts.** (a) `sdks/migrate/dist/op-ir.schema.json` — the
   schemars-emitted JSON Schema of the `Op`/`Expr` enums, produced from the Rust types (already the
   IR contract). (b) A **`--emit-surface` mode of the compiled recorder artifact** (§10.1): loading
   `recorder.mjs` and enumerating the reachable `op.*`/`pg.*`/`t.*` methods + their TS parameter
   shapes via the exported type metadata. No hand-maintained list.
2. **Generator = one small deterministic binary** (`xtask gen-migrate-docs`) that joins the two:
   for each `Op` variant in the schema it finds the emitting DSL method(s), and emits the reference
   tables — the export inventory, the op inventory, the expression-node inventory, and the §8
   coverage rows' authoring column — into `docs/reference/migrate-op-dsl.md` between generated-region
   markers. Support declarations (§5.2), `RenderMode`, tier, and reversibility class (§5.3) are read
   from a `const fn`-backed `Op::describe()` table also serialized into the schema, so the matrix's
   ✅/🔁/⛔ and the reversibility column are **generated, not transcribed**.
3. **CI gate.** `xtask gen-migrate-docs --check` re-runs the generator and fails on any diff against
   the committed doc — the same pattern as `cargo test`'s golden checks. A new export, op, node, or
   dialect-support change that is not reflected in the doc **fails the build**; a doc edit inside a
   generated region likewise fails (edit the source, not the output). This is what makes
   "docs↔exports drift is a build failure, permanently" (§4.1) an actual mechanism rather than an
   aspiration.

## 11. How this honors the repo's API principles and the must-keep invariants

### 11.1 The ten principles (`docs/reference/api-design-guidelines.md`)

| # | Principle | How v2 honors it |
|---|---|---|
| 1 | One obvious path | Two roots total, one grammar for every object kind; top-level `table/view/pgEnum/pgDomain/sequence/comment` deleted; PG-only domains/sequences live only under `pg.*`; FKs are one model with a pinned sugar/longhand split (`t.ref` single-col-to-id, `foreignKeys` for composite/vendor options, collision is `FK_DECLARED_TWICE`, §4.2); generated docs CI-diffed against exports (§10.6) so a second path cannot silently appear. |
| 2 | Match the zeroship entry contract | Migration modules keep `export default { name?, up, down? }`; schema changes stay in migrations, never the app entry (K9). |
| 3 | Keep auth/trust explicit | The privileged surface is an explicit import (`@zeroship/migrate/pg`) — no hidden mode flags; real trust is the server-side capability gate, stated in the docs exactly as the `pg.ts` header does today. |
| 4 | Zero setup for creator code | `import { op, t }` and write; no client construction, no registration; recorder is ambient with structured misuse errors (`OP_OUTSIDE_RECORDER`). |
| 5 | Names read like English | `op.table("users").index("i").add({...})`, `op.enum("s").addValue({...})`, `pg.domain("email_t").create({...})`, `pg.sequence("s").create({...})`, `pg.grantRole({...})`; `pgEnum`→`op.enum` because enum authoring is portable, while `pgDomain`/`sequence` become `pg.domain`/`pg.sequence` because those objects are PG-only; `@zeroship/backfill` disambiguates the package pair. |
| 6 | Stable parameter order | Identity positional, payload as one options object, everywhere; no signature swaps between kinds. |
| 7 | Predictable return rails | Handles are pure values; terminals record and return the handle; no hidden execution; `down` derivation rules unchanged. |
| 8 | Structured input over string DSLs | The enlarged closed expression AST + SelectAst v2 remove the routine need for strings; the remaining string inputs are the three enumerated raw islands, capability-gated and budgeted — the boundary is honest instead of aspirational. |
| 9 | Stable machine-readable error codes | The full structured-code + `suggested_fix` discipline extends to every new refusal (`DIALECT_UNSUPPORTED`, `EXPR_NOT_PORTABLE`, `VENDOR_EXPR_DENIED`, `VENDOR_OP_DENIED`, pending-contract at validate); failure moves to the earliest catching layer; no silent narrowing anywhere (per-intent alter terminals). |
| 10 | Chains read left-to-right | `root.kind(name).selector(name).terminal({...})` narrows left-to-right; each step is inert until the terminal; anti-pattern compliance (no positional booleans) pinned by lint + test. |

### 11.2 The must-keep invariants

Covered in §3 (K1–K9); the load-bearing deltas: K1's shape breaks exactly once with the
discipline intact; K2's stale doc wording is corrected as part of the rewrite; K3 gains the
shape-carried boundary, the policy-config serialization, and its **trust model** (§10.4); K4/K6 are
strengthened (dialect-scope refusal at validate, interlock hoisted to validate) **while honestly
retaining the live-schema fail-closed guards at lower** for the `LiveResolved` class (§5.2) and the
MySQL non-transactional reality (§12.1); K7's meaning is clarified (author-layer, not engine
context); K8's dual-recorder parity is re-achieved by construction; K9 is closed by the
reversibility taxonomy (§5.3). Nothing in the spine is weakened.

**Boundary proof (success criterion):** (1) a compile-fixture "confined migration" importing
only `@zeroship/migrate` where `// @ts-expect-error` probes demonstrate every vendor op/expr
node is unnameable; (2) a recorder-level test enumerating every op tag `op.*` can emit and
asserting `Tier::Core` (zero VendorCapability mappings) via an exhaustive Rust match;
(3) the validate+lower capability double gate as defense-in-depth against forged IR.

---

## 12. Multi-dialect handling

Mechanics unchanged, guarantee strengthened:

- **PG native** (compio-postgres), **SQLite** in-process with the 12-step rebuild lowering
  (now also carrying composite FKs/uniques/checks and enum-value evolution), **MySQL** via the
  JsDriverBackend — all consumers of v2 IR.
- One artifact applies faithfully on every dialect in its derived `dialect_scope`, or refuses
  **fail-closed at validate** with a structured code — never at lower (the matrix test is the
  proof, §5.2).
- Vendor declarations are PG-scoped by construction (the support matrix stamps it).
- Existence-guard probes extend to the new object kinds (partitions, policies, matviews) with
  the same shape-verify-or-fail defaults.
- `docs/reference/sqlite-divergences.md` gains the new constructs' rows **per wave as each
  lands** (graft: additive landability), so the divergence doc never lags the surface.

### 12.1 MySQL DDL is non-transactional — partial-failure recovery (MAJOR #4, Missing #3)

<!-- Revised 2026-07-02: addressing critic MAJOR #4 + Missing Concept #3 — the crash-safety narrative (two-phase recovery, txn-per-step) silently assumed PG/SQLite; MySQL auto-commits every DDL and cannot roll back. -->

The two-phase-recovery / txn-per-step / split-timeout-envelope story in §9 is **PG- and
SQLite-shaped**. MySQL **auto-commits every DDL statement** and cannot roll back a DDL: a multi-op
MySQL migration that fails on statement *k* leaves statements *1..k-1* **committed** and *k..n*
unapplied — there is no clean rollback, exactly as Flyway documents for MySQL/MariaDB. The design
must state this rather than paper over it. The MySQL path is therefore **fail-marked + repair +
drift**, aligned with how Flyway and gh-ost handle the same reality:

1. **No false transactional promise.** The executor **does not** wrap a MySQL migration in a txn it
   cannot honor. Each DDL step journals **individually** with a per-statement `applied` marker (not
   a per-migration one), so recovery knows the exact last-committed statement.
2. **Fail-marked, manual repair.** On failure the migration is journaled `failed` at statement *k*
   (Flyway's model); the deploy stops; the operator gets the partial-state report (statements
   1..k-1 committed) and a resume/repair path (`zeroship-migrate repair --from k`). We do **not**
   auto-"roll back" by synthesizing inverse DDL — that is the fabricated-inverse trap K9 forbids.
3. **Expand/contract as the real safety net.** The primary mitigation is the same one Flyway
   recommends: **backward-compatible, additive-first migrations** so a partial failure is not a
   disaster — the prior app version still runs against the partially-migrated schema. The §9
   expand/contract constructs are exactly this discipline; on MySQL they are the *load-bearing*
   safety mechanism, not an optimization.
4. **`drift` is the MySQL backstop.** Because there is no atomic apply, `drift` (§10.3) against the
   MySQL catalog is how a partially-applied or repaired schema is reconciled to the fold. This is
   **not** the Postgres `pg_dump` comparator: it is the MySQL catalog adapter described in §10.3,
   scoped to MySQL-supported constructs and backed by `information_schema`/`SHOW CREATE`
   normalization.
5. **Granularity guidance, enforced.** Authoring guidance is one schema-changing DDL per migration
   on the MySQL profile where practical; a lint (`MYSQL_MULTI_DDL_MIGRATION`) warns when a
   MySQL-targeted migration bundles multiple non-idempotent DDLs, since each added statement widens
   the partial-failure blast radius. This is guidance + lint, not a hard refusal (some changes are
   irreducibly multi-statement).

This is an **inherited** property of MySQL, not something this redesign introduces — but the prior
draft's "strengthened crash-safety guarantee" over-claimed by ignoring it. The honest statement:
**PG/SQLite get transactional or two-phase-recoverable apply; MySQL gets per-statement journaling +
fail-mark + repair + drift + an expand/contract-first authoring discipline.**

---

## 13. Phased implementation plan

Dual-reviewed PR-train per repo discipline; relative sizes only (no fabricated hour/LOC
figures, per the measure-or-say-unknown rule). Each phase lands with goldens, matrix-test
fixtures, a regression test per critique finding it resolves, and the full verification
discipline: `cargo test -p zeroship-migrate` (all targets, live PG :5440 for DB legs) + the
`sdks/migrate` pnpm suite. **Every wave is independently landable and independently valuable**
(graft): a partial landing still improves the surface.

- **P0 — IR v2 skeleton (medium; highest churn, lowest uncertainty).** New Op/Expr enums with
  the support-declaration machinery; exhaustive matrix-test harness; checksum/canonical-
  serialization property tests; one-shot golden regeneration; delete v1 shapes +
  `ExprRenderDeferred` + all accepted-then-refused arms + `PgRaw.binds`; `PgRaw.reason`
  required; capability presets serialized as policy-config files.
- **P1 — the two-layer value system (large; highest technical risk) — §6.** Two workstreams:
  (a) **Layer-1 intent nodes** — node-by-node admission with live three-dialect parity proofs
  (the closed AST makes partial delivery safe); (b) **Layer-2 `dialect({...})`** — the
  uniform per-dialect escape (`Dialectal<T>` envelope + `default` leg, own budget/`reason`-marked,
  refuse-uncovered-dialect) across types/defaults/exprs/index-methods. An un-proven node degrades
  to a budgeted `dialect(...)` override, not a release block. Also lands the **exact-platform-table keystone**
  (policy-driven system-field injection + explicit/composite PK + ownership registration — the
  ~46% marker unblock, `P1_KEYSTONE_PLAN.md`), which is independent of (a)/(b).
- **P2 — core surface rewrite (medium).** The `op` root; per-intent alter terminals; rendered
  table-level constraints on all dialects (SQLite rebuild threading); index-element model
  (portable slice); enum evolution; single compiled recorder artifact replacing
  `migrate_ops.js` (+ the one-release parity tripwire); `SchemaState` fold as a first-class
  checksummed structure; the `drift` command.
  — *Cut line:* P0–P2 deliver a complete, better creator core independently. But the
  critique's verdict says vendor depth IS the point: P3–P5 are the body of the work, not the
  stretch goal.
- **P3 — vendor depth wave A (large).** The `pg` root; full index model (methods, opclass,
  collation, INCLUDE, storage params); full FK/constraint model incl. NOT VALID→VALIDATE;
  granular grants + role membership + ALTER DEFAULT PRIVILEGES; function/trigger/policy depth;
  renderers + goldens + sqlite-divergences rows.
- **P4 — vendor depth wave B (large).** Partitioning (+ bound helpers); domain evolution;
  SelectAst v2 + matviews (+ REFRESH CONCURRENTLY); the non-transactional executor step class
  with journaled recovery, advisory-lock yielding (§9.4), + live-PG crash tests; the MySQL
  per-statement journaling + fail-mark/repair path + MySQL drift adapter (§10.3) +
  `MYSQL_MULTI_DDL_MIGRATION` lint (§12.1).
- **P5 — corpus port + gates (large, long-tail).** Port this repo's Liquibase changelog
  (`control`/`auth`/per-app; **sandbox `V0011`–`V0024` excluded** — sibling repo); the
  **catalog-level pg_dump-equivalence comparator** (§8, parse-to-object-model, semantic/cosmetic
  boundary, per-changeset bisection) + zero-diff gate; the object-dependency topo-sort (§8.1);
  `raw-budget.toml` + CI raw gate; the `xtask gen-migrate-docs` generator + CI export-diff gate
  (§10.6); `@zeroship/backfill` rename with all callers updated.
- **P6 — expand/contract polish (small-medium).** Online setType phasing; NOT VALID/VALIDATE
  pairing ergonomics; per-phase `version_key` emission + `requires_approval` plan output;
  validate-time pending-contract refusal; plan phase rendering with honest labels.

---

## 14. Risks

Ranked, with mitigations:

1. **Tier-P parity-proof burden (P1) — the real cost center.** LIKE escape rules, BETWEEN null
   semantics, `isDistinctFrom` mappings, dateAdd envelopes are where subtle divergence hides.
   *Mitigation:* node-by-node admission; the demotion mechanism makes retreat cheap and honest;
   validate refuses whatever isn't admitted — never blocks shipping. *Residual:* the standing
   tax on every future portable node is the price of the portable claim being true; the
   predictable failure mode under schedule pressure is admitting nodes on eyeballed
   equivalence — the envelope discipline exists precisely to forbid that.
2. **pg_dump-equivalence long tail (P5).** pg_dump text is cosmetically unstable
   (constraint-def formatting, default-expression spelling). *Mitigation:* the **catalog-level
   comparator specified in §8** — parse both dumps to a normalized object model (Atlas/migra
   approach), with an **enumerated semantic-vs-cosmetic boundary** (never normalizing type/
   nullability/default-value/predicate/columns), the raw `diff -u` retained as audit artifact, the
   §8.1 dependency topo-sort as the principled basis for order-normalization, and **per-changeset
   bisection** for attribution; the ≤5 enumerated raw budget as the honest escape while coverage
   catches up. *Residual:* the comparator's normalization rules are themselves a surface that must
   be conservative-by-default (a rule ships only with a proof the two inputs are semantically
   identical), or it re-introduces the "mask real divergence" risk the critique named — that
   discipline is the mitigation's own load-bearing part.
3. **One-shot IR break coordination (P0).** Goldens, `op-ir.schema.json`, generated TS wire
   types, engine recorder, gen-types, control-plane deploy tests all churn at once.
   *Mitigation:* P0 is the FIRST slice so everything after builds on v2; closed-enum
   deserialize fails loudly on any missed consumer; full per-crate suite discipline. *Residual
   cost:* a large dual-review budget burned on regeneration diffs.
4. **CONCURRENTLY outside the txn envelope (P4).** New executor semantics. *Mitigation:*
   journaled-step drop-and-rebuild recovery + live-PG crash tests; ships in P4, everything
   earlier stays transactional.
5. **SelectAst scope creep.** Someone will eventually need LATERAL. *Mitigation:* the line
   (no recursive CTE / LATERAL / correlated subquery-in-expression) is written into the spec;
   beyond it is `rawSelect` + budget — honest and counted, but not zero. A maximalist reading
   of "make pg.raw rare" will still score this a gap; it is a named line, not an oversight.
6. **Permanent maintenance surface growth.** Roughly 2× op variants and expression nodes,
   across three renderers plus goldens, owned forever. The support-matrix machinery converts
   this into mechanical, test-pinned breadth rather than cleverness — but it is still breadth.
   (The alternative that would have externalized it — introspect-and-diff — was rejected for
   abandoning the frozen-IR spine.)
7. **pg/core handle duplication drift.** `pg.table` structurally supersets `op.table`; kept in
   sync via shared generic base types in one module. The export-inventory CI gate covers the
   public surface, not internal duplication — review discipline covers the rest.
8. **Single-recorder build coupling.** `include_str!` of `recorder.mjs` extends the
   bootstrap→runtime pattern to a second crate; a stale dist file produces parity-tripwire
   failures rather than compile errors. *Mitigation:* the one-release tripwire → artifact-
   identity assertion transition (§10.1); root `pnpm build` ordering already documented.
9. **MySQL leg test cost.** JsDriverBackend runs are slow. *Mitigation:* matrix tests tiered —
   render-goldens always; live-apply legs nightly/gated.
10. **Two-tier expression mental model.** Authors must internalize portable-vs-pg.
    *Mitigation:* the type-split builders make accidental crossing impossible; error quality is
    load-bearing (`EXPR_NOT_PORTABLE` names the exact node and both resolutions) or the
    boundary will feel arbitrary.

---

## 15. Success criteria (exit gates, adopted verbatim from the brief)

1. Platform-schema port: this repo's Liquibase changelog (sandbox excluded, §8) authorable with
   ≤5 `pg.raw` ops (target 0, each enumerated with a reason), **catalog-level** pg_dump diff of
   ZERO vs the Liquibase baseline (parse-to-object-model comparator, §8; raw `diff -u` retained as
   audit artifact; per-changeset bisection on any non-zero).
2. Zero *unexpected* late-lowering failures: the mode-aware exhaustive matrix test (§5.2 —
   `Offline` ops golden-render, `LiveResolved` ops golden-lower over a fixture `LiveSchema` + emit
   the `-- [runtime-resolved]` plan line); `ExprRenderDeferred` and all accepted-then-refused arms
   no longer exist. (The remaining lower-time refusals are the live-schema fail-closed guards for
   the `LiveResolved` class, which are apply-time facts, not validity errors.)
3. One package per trust level: generated docs == actual exports, CI-gated;
   `@zeroship/migrate` (portable core) and `@zeroship/migrate/pg` (operator vendor) are the only
   import paths; named exports, no root-object prefix; no stale sole-entry claims anywhere.
4. Boundary by shape: the confined-migration compile fixture + Tier::Core recorder enumeration.
5. Binds: gone from TS types and IR — no third state.
6. Coverage checklist green: every construct in §8's matrix has an authoring form; a golden PG
   render; per **✅** cell a byte-stable offline golden and per **🔁** cell the runtime-resolved
   plan line + fixture-schema lowering golden; a structured validate-time refusal on every **⛔**
   dialect; and a stated reversibility class (§5.3) with a `down`-derivation test.
7. Fail-closed alter: the per-intent terminals make the both-fields shape a tsc error,
   regression-pinned.
8. No positional booleans on the public surface, verified by lint + test.
9. `@zeroship/backfill` rename shipped in the same change, all callers updated, no alias.
10. Determinism preserved: recorder parity (tripwire → identity), checksum-stability property
    tests, unknown-tag fail-closed deserialize, no committed `.ir.json`, gen-types doc wording
    reconciled.
11. Expand/contract golden path (**harness-scoped, per §9.2 MAJOR #3**): one e2e authors online
    rename + backfill + NOT VALID→VALIDATE, `plan` renders phases with honest labels, and apply runs
    under the validate + apply interlock on live PG **in the migrate test harness**. Production
    go-live remains gated by the control-plane approval track (`deploy_migrate_test.rs:643` pin
    stays); this criterion does **not** assert production go-live, only that the engine half is
    complete and the `version_key` interface is frozen for the parallel track.
12. Full suite discipline: per-crate cargo (all targets, live PG :5440) + sdks/migrate pnpm
    suite green; a regression test per fixed critique finding.
