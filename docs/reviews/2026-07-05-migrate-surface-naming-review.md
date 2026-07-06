# `@zeroship/migrate` — surface naming & API review (hypercritical)

**Date:** 2026-07-05 · **Base:** `main` @ `8b405692` · **Reviewers:** primary (Claude) + independent adversarial critic (Fable, Laravel-benchmarked)
**Benchmarks:** Laravel schema builder (elegance/developer-happiness) + the DSL's own principles — **P1** (one grammar, one spelling), **P3** (`/pg` vendor tier — vendor unreachable from portable core), **P10** (names tell the truth).
**Spec of record:** `docs/proposals/2026-07-04-dsl-surface-redesign-design.md` (the critic-hardened target surface; §2 export inventory, §5 wire reshape, §6 phased path).

---

## Headline finding — the surface redesign was *specced, not implemented*

The redesign that merged to `main` (the `slice/*` PR-train) landed the **engine/IR foundation**: the checksummed IR v-lattice, the `defineOp` census + `.d.ts` lint, the builder lattice (DefaultBuilder / CheckBuilder(WithPg) / IndexExprBuilder / GeneratedColumnBuilder / DomainValueBuilder), P12 partition collapse, `IrValue` DML unification, EXTRACT widening. That work is real and good.

But the **surface rewrite (design §6 Phases 2–3) did not land.** Both reviews, run independently against `main`, converge: the shipped authoring surface is still the pre-redesign shape. It scores **~55/100 against its own principles.** Every BLOCKER below is a P1/P3/P10 violation *by the DSL's own definitions*, and the target shape for most is already pinned in the spec. An earlier superseded workflow line (`wf/redesign-*`, now stale behind main's wire reshape) implemented several of these — usable as implementation reference, not mergeable.

The refactor = execute design §6's remaining phases as a sequenced PR-train off `main`.

---

## What is genuinely elegant (keep, unchanged)

- **`t.id({ prefix })`** — Laravel's `$t->id()` + typed-id branding in one call (PK + uuid + default + brand). Best name on the surface.
- **Immutable `ColumnDef` with the var-reuse contract** — `const req = t.text().notNull()` reused across columns without aliasing. A real win over Laravel's mutable Blueprint.
- **`columns: Record<string, ColumnDef>`** in `create()` — declarative as a blueprint, still data.
- **`SELECTOR_NOT_TERMINATED` at drain** — fail-closed on the dangling-selector foot-gun, checked late enough that var-held selectors work.
- **Structured errors** with `code` + `suggested_fix`; friendly closed-set authoring-time checks (`VECTOR_METRICS`).
- **Required constraint/index names everywhere** — correct for a portable, journaled engine.
- **The principled `dialect({...})` escape** and the `/pg` header's honesty that the boundary is a tsc gate, not a security gate.

---

## Findings — prioritized, mapped to the spec

Severity: 🔴 BLOCKER (principle violation, spec pins the fix) · 🟠 MAJOR · 🟡 MINOR · ⚪ NIT.
"Spec" = the design-doc section that already specifies the target. "Ref" = a stale `wf/` branch that implemented it.

### 🔴 BLOCKER

| # | Finding | File | Fix / target shape | Spec |
|---|---------|------|--------------------|------|
| B1 | **Selector grammar is bimodal — six direct-verb holdouts.** `createTrigger`/`dropTrigger`, `createPolicy`/`dropPolicy`, `detachPartition(name)`, `validateConstraint(name)` use verb+name-in-bag while columns/indexes/FKs use `.name().add()`. Triggers/policies are named sub-objects — P1 says the selector form is THE grammar. | `types.ts:1221,1231,1234,1240` | `.trigger(n).create()`, `.policy(n).create()`, `.partition(n).detach()`, `.constraint(n).validate()` | §3.2, §3.5 |
| B2 | **Two spellings for every policy op and every comment.** Free `createPolicy/dropPolicy` (`pg.ts:308,329`) duplicate the handle methods; free `comment(target,text)` (`ops.ts:3384`) duplicates the `.comment()` terminal on 7 handles. | `pg.ts`, `ops.ts` | Delete the free forms; terminals/handles are the one spelling. | §2 ("no free `comment()`"), §3.7 |
| B3 | **`interval` — a PG node exported from portable core, twice-spelled, a string-parser.** `interval("HH:MM:SS")` emits `pgInterval`, duplicates `c.pg.interval`, and parses a string instead of a structured value. | `ops.ts:1592` | Delete core `interval`; structured `Duration { hours, minutes, seconds }`; `c.pg.interval` only. | §2, §5.6, §6 (open tension 6) |
| B4 | **`.matches()` / `.columnSize()` — vendor nodes camouflaged on the portable chain.** Emit `pgRegexMatch`/`pgColumnSize` — same nodes as `c.pg.regex`/`c.pg.pgColumnSize`. Two spellings (P1) + PG-only reachable from core (P3) + names hide PG (P10). Design §3.3 explicitly deletes them. | `types.ts:471-474`, `ops.ts:1518` | Delete from `ExprChain`; `c.pg.regex`/`c.pg.pgColumnSize` only. | §3.4 · Ref: `wf/redesign-boundary-split`, `-expr-portability` |
| B5 | **Raw SQL reachable from the portable core.** `CreateViewArgs.as` accepts `{ raw: string }` though the package header declares "no raw escape." | `types.ts:872` | Raw view bodies move to `/pg` (`rawSelect`/`raw` with `reason`); core view is structured-only. | §3.6, §3.7 |
| B6 | **`/pg` speaks three creation grammars at once.** Handle-grammar (`domain(n).create()`), noun-records-CREATE (`schema({...})`, `role({...})`, `extension({...})` — P10 lies; `role({name})` reads like a reference but records `CREATE ROLE`), and full verb pairs (`createPolicy`, `createFunction`). Create/drop asymmetry (`schema()` vs `dropSchema()`). | `pg.ts:190-368` | One grammar: `schema(n).create()/.drop()`, `role(n).create()`, `policy(n).on(t).create()`, `fn(n).create()`. | §3.7 |
| B7 | **`.rename()` returns a handle bound to the dead name.** `table("a").rename({to:"b"}).column("x").add()` records against `"a"`. Fluency is a trap on exactly one method. | `ops.ts:3418` | Post-rename handle rebinds to the new name (P2 return matrix). | §3.1 (P2 matrix) |
| B8 | **`backfill` vs `update({ batch })` — two spellings of one op.** The `batch` doc admits it lowers to the same windowed executor. | `types.ts:742-771` | One entry: `backfill` is the honest name. | §3.7 (raw budget / DML) |

### 🟠 MAJOR

| # | Finding | File | Fix |
|---|---------|------|-----|
| M9 | **RLS quadruplet** `enable/force/disable/noForceRowLevelSecurity` — 4 zero-arg methods; `noForce…` is an unparseable double negative (P10). | `types.ts:1236-1239` | One `setRls({ enabled, forced? })` (§5.9). |
| M10 | **Three spellings + three morphologies for table options** — `create({softDelete,…})` inline + `setOptions({...})` + per-option `softDelete()`/`withVersioning()`/`strictness("strict")` (positional enum, violates P3). Method `withVersioning` ≠ wire key `versioning` (P10). `softDelete({enabled:false})` = enable-named method that disables. | `types.ts:1195-1198` | One spelling: `setOptions`. |
| M11 | **Free `and`/`or`/`not` duplicate chain `.and`/`.or`/`.not`.** | `ops.ts:1576` | Chain-only (§2). |
| M12 | **`t.ref()` vs `.ref()` facet — dup spelling, and the facet silently destroys the receiver type** (`t.text().ref("users")` discards `text` — the P8 cardinal sin). | `types.ts:189,239` | Keep `t.ref()`; delete the facet. |
| M13 | **Vendor options riddle the portable `create()` payload** — `exclusions`, `indexes[].include/with/only/nullsNotDistinct`, `InsertArgs.onConflict` (self-labeled PG-ONLY), and the "portable" `IndexMethod` union is a grab-bag (only `btree` is truly portable). | `types.ts:1022-1036,729,675` | Vendor options to `/pg`-widened create; portable `IndexMethod` = `btree` only. |
| M14 | **`c.fn.currentSetting`/`currentUser` — self-labeled "PG vendor" in portable `c.fn`.** | `types.ts:512-515` | Move to `c.pg.*` (§2 "ScalarFn sheds currentSetting/currentUser"). Ref: `wf/redesign-expr-portability`. |
| M15 | **`t.date()` — portable name for a PG-domain-only token** ("validates only as a PostgreSQL domain base type"). Every reader assumes Laravel's `$t->date()`. | `types.ts:232` | Land 3-dialect DATE, or move to `/pg` domain position with an honest name. |
| M16 | **Three vocabularies for type concepts** — cast targets `"integer"/"real"/"blob"` (SQLite affinity) vs lexicon `t.int`/`t.real`/`t.bytes`; binary has 3 names (`t.bytes`/`"blob"`/`byteValue`); exact-precision has `t.numeric`/`decimal`/`{decimal}`. | `types.ts:475` | One lexicon in every type-token position; `t.numeric` → `t.decimal` (Laravel + IR spelling). |
| M17 | **`decimal()` vs `byteValue()` — inconsistent ctor morphology** (bare noun vs noun+`Value`; "byteValue" singular for a plural payload). | `ops.ts:196,217` | Spec pins `decimal()` + `byteValue()` as the one-name-each resolution (§2) — accept the asymmetry deliberately or align; **decision: keep spec names, document why.** |
| M18 | **`c("x")` vs `c.col("x")` — the builder handle has two spellings.** | `types.ts:623` | Callable `c()` only; delete `.col` (§2). |
| M19 | **Value grammar inconsistent across positions** — `update.where`/`delete.where` accept `ExprFn` only while view/join/policy/generated accept `ExprFn|ExprChain|Expr`, so `and()/or()` output can't be passed to `update.where`. | `types.ts:744,755` | `DmlValue`/expr-arg uniform in every value position (P4). |
| M20 | **Constraint lifecycle switches subjects** — add via `unique(n).add`/`check(n).add`/`foreignKey(n).add` but drop via kind-agnostic `constraint(n).drop`; `unique(n).drop()` doesn't exist. | `types.ts:1114` | Same selector across a named object's life. Ref: `wf/redesign-fk-not-valid`. |
| M21 | **`IndexRef.drop({ unique })` — honor-system safety flag.** The untrusted author re-declares uniqueness, and it drives destructive/approval gating. | `types.ts:1142` | Drop the flag; the engine knows from journal/fold (§5.5 `dropIndex.unique` deleted). |
| M22 | **Partitions: create/drop/detach, no `attach`** — detach is one-way; PG `ATTACH PARTITION` unspellable. | `types.ts:700` | Add `.partition(n).attach()` (vendor) (§5.7 `attachPartition`). |
| M23 | **`in`/`notIn` accept `readonly string[]` only** — name promises SQL `IN`, type delivers strings; no numbers/exprs. | `types.ts:482` | Widen to scalar/expr arrays. |

### 🟡 MINOR / ⚪ NIT

| # | Finding | Fix |
|---|---------|-----|
| m24 | `DelArgs` retains abbreviated `Del` after the `del`→`delete` rename (`types.ts:754`). | → `DeleteArgs`. |
| m25 | `minValue`/`maxValue` sentinels collide with sequence numeric options — same name, unrelated meaning, one namespace (`ops.ts:2120`, `types.ts:313`). | Disambiguate the sequence option or the sentinel. |
| m26 | Free `comment()` returns `void` — the only void entry (moot if B2 deletes it). | Deleted by B2. |
| m27 | `fromDb(field)` — "Db" = the `@zeroship/db` SDK; reads like live-DB introspection (`ops.ts:1354`). | Spec keeps `fromDb` core (§2); accept — it *is* the db-SDK bridge. **No change; documented.** |
| m28 | `IfNotExistsGuard`/`IfExistsGuard` exported aliases are literally `boolean`, used in zero signatures (`types.ts:97,106`). | Delete. |
| m29 | `ViewHandle` has no `rename` while `TableHandle` does — lifecycle asymmetry. | Add or document. |
| m30 | `TriggerBodyBuilder.select(expr)` — "select" as a statement is opaque (`types.ts:808`). | Rename to the effect it performs. |
| m31 | `t.encrypted()` accepts `{of}|ColumnDef|ColType` — three shapes of one arg (`types.ts:262`). | One shape (`{ of }`, `ColumnDef<false>` per §2). |
| m32 | `byteValue("base64")` string input alongside `Uint8Array` — encoding footgun (canonical re-encode check mitigates). | Consider `Uint8Array`-only. |
| m33 | All `/pg` entries return `Node = Record<string,unknown>` — untyped public return (`pg.ts:26`). | Type the `/pg` returns. |
| m34 | `TableStrictness = "strict"|"lenient"|"off"` — "lenient" vs "off" indistinguishable by name (`types.ts:963`). | Name-carry the distinction. |
| n35 | `cCase`/`cFn`/`cAgg`/`cPg` internal twins not underscore-prefixed like siblings (`ops.ts:1849`). | Prefix `_`. |
| n36 | `t.numeric()` silently defaults `(38,9)` behind a nullary call (`ops.ts:1206`). | Require precision or document. |
| n37 | `lintDeterminism(source)` — a source-text lint on the authoring entry point (`ops.ts:3781`). | Move to `@zeroship/migrate/toolchain` (§2). |

---

## Refactor sequencing (PR-train off `main`, commit-only)

Same-file (`types.ts`/`ops.ts`/`pg.ts`) → **sequenced on one worktree**, not parallel-isolated. Each slice: 3-dialect verify (PG :5440 + in-process SQLite + mysql2 isolate), regression test per behavior change, goldens regenerate once where the wire moves.

1. **S1 — vendor-leak removal (no/low wire):** delete chain `.matches`/`.columnSize` (B4); move `c.fn.currentSetting`/`currentUser` → `c.pg` (M14). Ref `wf/redesign-boundary-split` + `-expr-portability`.
2. **S2 — duplicate-spelling deletions (zero wire):** free `and`/`or`/`not` (M11), `c.col` (M18), free `comment()` (B2-comment), `.ref()` facet (M12), `IfNotExists/IfExistsGuard` (m28).
3. **S3 — selector-grammar conversion (wire):** createTrigger/createPolicy → `.trigger(n)`/`.policy(n)` (B1); detachPartition → `.partition(n).detach()` + add `.attach()` (B1/M22); validateConstraint → `.constraint(n).validate()` (B1); RLS quadruplet → `setRls` (M9); free createPolicy/dropPolicy delete (B2). Ref `wf/redesign-fk-not-valid`.
4. **S4 — /pg grammar unification (wire+surface):** `schema`/`role`/`extension` flat fns → handle grammar (B6); raw view → `/pg` (B5); `nextval` → `/pg` (§2).
5. **S5 — table options + type lexicon (wire):** one `setOptions` (M10); `t.numeric`→`t.decimal` + cast-token unification (M16); `t.date`/`t.encrypted` shapes (M15/m31).
6. **S6 — wire reshapes:** `interval` → structured `Duration` (B3); `in`/`notIn` widen (M23); `IndexRef.drop` flag removal (M21); post-rename handle rebind (B7); backfill/update.batch unify (B8); constraint lifecycle unify (M20); value-position uniformity (M19).
7. **S7 — toolchain + polish:** `lintDeterminism`→`/toolchain` (n37); DelArgs rename (m24); `/pg` typed returns (m33); NITs.

Phase-5 exit gate (spec §6): re-run this adversarial critique against the new surface — every BLOCKER/MAJOR traceable to a closing change.
