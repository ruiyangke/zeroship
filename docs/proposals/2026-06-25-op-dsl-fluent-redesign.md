# op.* migration DSL — fluent-only redesign (design spec)

**Status:** approved design (2026-06-25), pre-implementation. Pre-launch, no back-compat —
the flat surface is **removed**, not deprecated. This spec supersedes the relevant items in
`2026-06-25-op-dsl-api-consistency.md` (the consistency critique that motivated it).

**Goal:** one elegant, consistent, fully-named **fluent** authoring surface for `@zeroship/migrate`,
replacing the dual flat-ops + facade surface. The recorded IR / `.ir.json` wire shape is **byte-identical**
to today except the single C1 FK-actions change; the engine apply path is unchanged. This is a
surface + correctness pass, not an executor change.

---

## 1. Principles (locked)

1. **Fluent-only.** `table(name, { schema? })` is the **sole** public entry. The flat op functions
   (`createTable`/`addColumn`/…) are **removed from the public API** — their op-construction logic
   stays as internal helpers the handle delegates to (so the IR is unchanged).
2. **Fully-named terminals, no exceptions.** Every recording method takes **exactly one named object**.
   The only positional argument anywhere is the *name* passed to `table(name)` and to a selector
   (`.column(name)`, `.index(name)`, …) — i.e. **identity is positional, payload + options are a named object**.
3. **Eager.** Every terminal records its op immediately onto the ambient per-migration recorder
   (vitest-style). No deferred/terminal-less builder state on the recording path.
4. **Selector sub-handles** for named sub-objects (columns, constraints, indexes); **direct methods**
   for the table itself and its data (create/drop/rename, insert/update/del/backfill). See §3.
5. **Names are plain strings** (anti-rot — migrations are immutable history; never bound to the live
   `@zeroship/db` schema). Column existence is validated at **apply** time, never `tsc` time.
6. **No raw SQL.** Every transform/predicate is a closed-AST `(c) => Expr`. Types via the `t.*` chain.
7. **Var-assignable.** The handle (and `t.*` type values) are reusable values; both chained and
   variable-assigned authoring styles are first-class (§4).

Public exports of `@zeroship/migrate`: `table`, `t`, `fromDb`, `lintDeterminism`, and the types.
**Removed exports:** every flat op function; the `t.*` `{notNull,default}` options-object overload
(`ColumnOpts`/`applyOpts`); the `t.*` aliases `string`/`int`; the `dropConstraint(spec|string)` union.

---

## 2. Migration module shape (unchanged)

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "split_name_column", // optional; defaults to the filename label
  up() { /* table(...)… */ },
  down() { /* optional structured inverse; never raw SQL */ },
};
```

`up()`/`down()` are parameterless, synchronous, return `void`. The build/dev evaluator installs a
fresh recorder before each, drains the recorded op list, renders the checksummed `.ir.json`.

---

## 3. The full fluent surface

```ts
table(name: string, opts?: { schema?: string }): TableHandle
```

`{ schema }` is the **default schema** for every op on this handle (overridable per op). Profile rules
unchanged: general/Trusted honors it; Confined pins it to the project schema and refuses cross-schema
at validate time (PR10).

### 3.1 Table itself — direct named methods

```ts
.create({
  columns: Record<string, ColumnDef>,        // { id: t.id(), email: t.text().notNull(), … }
  primaryKey?: string[],                      // composite PK (else via t.id()/.primaryKey())
  uniques?:     Array<{ name: string; columns: string[] }>,
  checks?:      Array<{ name: string; expr: ExprFn }>,
  foreignKeys?: Array<{ name: string; columns: string[]; references: { table: string; columns: string[] }; onDelete?: RefAction; onUpdate?: RefAction }>,
  indexes?:     Array<{ name: string; columns: string[]; unique?: boolean; using?: IndexMethod; where?: ExprFn }>,
  ifNotExists?: boolean,
  schema?: string,                            // overrides the handle default
}): TableHandle

.drop({ ifExists?: boolean; cascade?: boolean; schema?: string }): TableHandle
.rename({ to: string; schema?: string }): TableHandle      // NEW — table rename — DEFERRED (see note)
```

> **`.rename` is DEFERRED, not shipped.** A table rename is a NET-NEW operation:
> there is no `renameTable` Op variant in the IR and no executor support for it
> (the flat surface never had a table rename either). This PR's hard constraint —
> *IR byte-identical except C1; apply path unchanged* — forbids the new Op +
> executor wave a table rename needs, so `.rename` is intentionally ABSENT from the
> shipped `TableHandle` (`sdks/migrate/src/types.ts`) and tracked as a follow-up
> PR (new `Op::RenameTable` + executor + render). It is listed here for the target
> shape only.

`create` is the one all-object form (no `build` callback) — honors "no exceptions": table-level
constraints/indexes are fields, not a callback. A name is required on each constraint/index (name-first,
§3.4).

### 3.2 Columns — `.column(name)` selector → named terminals

```ts
.column(name: string): ColumnRef
ColumnRef.add({ type: ColumnDef; ifNotExists?: boolean; schema?: string }): TableHandle
ColumnRef.drop({ ifExists?: boolean; schema?: string }): TableHandle
ColumnRef.rename({ to: string; type: ColumnDef; schema?: string }): TableHandle   // named ⇒ no from/to swap
ColumnRef.alter({ type?: ColumnDef; nullable?: boolean; using?: ExprFn; schema?: string }): TableHandle
```

`.column(name).add({ type })` honors **all** modifiers on `type`, including `.unique()` (C2 — emit a
follow-on unique constraint, mirroring the existing `.primaryKey()` hoist) and `.primaryKey()`.

### 3.3 Constraints — per-kind `add`, name-keyed `drop`

```ts
.foreignKey(name).add({ columns: string[]; references: { table: string; columns: string[] }; onDelete?: RefAction; onUpdate?: RefAction; ifNotExists?: boolean; schema?: string }): TableHandle
.unique(name).add({ columns: string[]; ifNotExists?: boolean; schema?: string }): TableHandle
.check(name).add({ expr: ExprFn; ifNotExists?: boolean; schema?: string }): TableHandle
.constraint(name).drop({ ifExists?: boolean; schema?: string }): TableHandle   // kind-agnostic drop by name
```

`RefAction = "cascade" | "restrict" | "setNull" | "setDefault" | "noAction"`. **C1: `onDelete`/`onUpdate`
are actually rendered** on the stand-alone `.foreignKey(name).add(...)` **Postgres** path (today the
imperative FK silently drops them; the declarative `ref` path already honors them — the imperative path
is brought to parity via an IR change, §6).

> **C1 apply reachability (scope).** The RENDERED+TESTED C1 path is the stand-alone
> `addConstraint(fk)` on **Postgres** (`ir_author_render_parity.rs`). C1 is **PG-only at
> apply** today: the SQLite leg has no IR field for FK actions (its column-`ref` path
> carries no action), and the `createTable({ foreignKeys })` table-level FK lowers on
> PG only (the SQLite CREATE renders from the column descriptor — see §3.1). A later
> wave plumbs C1 through the createTable/SQLite-rebuild FK path and/or a
> `t.ref().onDelete()` column-level surface for full both-backends parity.

### 3.4 Indexes — `.index(name)` selector

```ts
.index(name).add({ columns: string[]; unique?: boolean; using?: IndexMethod; where?: ExprFn; ifNotExists?: boolean; schema?: string }): TableHandle
.index(name).drop({ ifExists?: boolean; concurrently?: boolean; unique?: boolean; schema?: string }): TableHandle
```

> **`.index(name).drop({ unique? })` — apply-gating signal.** `unique?` rides on
> the drop args (beyond the bare identity + `ifExists`/`concurrently`) because the
> IR `Op::DropIndex.unique` field DRIVES the destructive/approval gate at apply: a
> `unique: true` drop silently removes a data-integrity guarantee, so it lowers
> `destructive + requires_approval` and is refused under `Approval::None`. The hint
> is OR-ed with the AUTHORITATIVE live catalog (a hostile/buggy `unique: false` on
> an actually-unique index can NOT defeat the gate), but carrying it lets an author
> name the destructive drop for approval up-front. Absent/`false` ⇒ a plain,
> reversible drop.

**Name-first (deliberate):** indexes and constraints must be named via the selector — explicit names are
deterministically droppable in later migrations. `using: "btree" | "gin" | "gist" | "ivfflat" | "hnsw" | "fts5"`.

### 3.5 Table data — direct named DML

```ts
.insert({ rows: Row | Row[]; onConflict?: { columns: string[]; doUpdate?: Partial<Row> }; schema?: string }): TableHandle  // onConflict = PG-only
.update({ set: Record<string, ExprFn>; where?: ExprFn; batch?: { cursorColumn?: string; batchSize?: number }; schema?: string }): TableHandle  // batch = the IrBatch object (Op::Update.batch), not a bare flag
.del({ where: ExprFn; limit?: number; schema?: string }): TableHandle                  // where mandatory; `del` (delete is a JS reserved word)
.backfill({ set: Record<string, ExprFn>; where?: ExprFn; cursorColumn?: string; batchSize?: number; name?: string; schema?: string }): TableHandle
```

DML carries **no existence guard** (not guardable). `schema` rides on the args (PR12 threads it into
`BackfillSpec`).

---

## 4. Var-assign + reuse (first-class)

Both authoring styles are supported; pick per readability.

```ts
// chained
table("users").column("a").add({ type: t.text() }).column("b").drop({ ifExists: true });

// var-assigned (DRY; { schema } set once)
const users = table("users", { schema: "app" });
users.column("email").add({ type: t.text().notNull() });
users.unique("uq_email").add({ columns: ["email"] });
users.insert({ rows: [{ id: "u1", email: "a@b.co" }] });
```

Design requirements that make this safe:

- **The handle is a reusable value**: `table()` returns a `TableHandle` carrying only `{ name, schemaDefault }`;
  terminals record onto the ambient recorder and **return the handle**, so it stays valid for unlimited reuse.
- **`t.*` chain is IMMUTABLE**: each modifier (`.notNull()`/`.default()`/`.unique()`/`.primaryKey()`/`.ref()`)
  returns a **fresh** `ColumnDef`, so a hoisted type var is safe to reuse across columns without aliasing
  (today the chain mutates `this` — this changes).
- **Selectors may be held in a var** and terminated later; see §5.

---

## 5. The `SELECTOR_NOT_TERMINATED` guard (footgun → hard error)

A selector (`.column(x)`/`.foreignKey(x)`/`.unique(x)`/`.check(x)`/`.constraint(x)`/`.index(x)`) returns a
sub-builder that records **only** when its terminal is called. A forgotten terminal would otherwise silently
record nothing. The recorder makes this a **loud, structured error**:

- The recorder tracks every selector it hands out and whether it was terminated.
- At **`up()`/`down()` drain** (not eagerly — so a var-held selector terminated on a later line is fine),
  if any handed-out selector was never terminated, throw a structured
  `{ code: "SELECTOR_NOT_TERMINATED", selector: "column", name: "email" }` error.
- A selector terminated twice is also an error (`SELECTOR_ALREADY_TERMINATED`).

So `table("u").column("email")` with no terminal is a hard build error, not a no-op.

---

## 6. IR / wire impact

**Byte-identical to today except C1.** The fluent surface records the *same* op objects the flat ops did;
the golden corpus output is unchanged, only the authoring call sites change.

The **one** wire change — **C1 FK referential actions**: `IrConstraintKind::Fk` gains `on_delete` /
`on_update` optional fields (`ir.rs`), `op-ir.schema.json` regenerated, `Checksum::of_ir` folds them,
the **stand-alone `addConstraint(fk)` Postgres** render brought to parity with the declarative `ref`
path (`declarative.rs` already renders actions), the recorder twin emits them, the of_ir round-trip +
variant-exhaustiveness gate updated, golden re-blessed. `ir_version` bumps if the wire contract requires
it (the field is additive-optional; confirm the `deny_unknown_fields` contract during impl).

> **Apply scope is PG-only today** (see the §3.3 C1-reachability note): the wire fields exist on both
> dialects, but only the PG stand-alone FK add RENDERS them. SQLite FK actions and the
> `createTable`/SQLite-rebuild FK path are a later wave; the spec frames C1 as parity *of the wire
> contract*, with apply parity tracked as follow-up — not as already-shipped both-backends apply.

---

## 7. Removed / changed (no back-compat)

- **Removed:** all flat op exports; `t.*` `{notNull,default}` options overload (`ColumnOpts`/`applyOpts`);
  `t.*` aliases `string`/`int` (canonical: `text`/`integer`); `dropConstraint(spec|string)` union →
  `.constraint(name).drop({ifExists?})`; the `createTable(name, columns, build?, opts?)` positional footgun
  (no flat createTable at all).
- **Changed:** `t.*` chain is immutable; FK actions honored; `.column().add()` honors `.unique()`.
- **`c` / `c.fn` dedup:** `coalesce`/`concatWs` live **only** on `c.fn` (remove the chain duplicates);
  `.cast(...)` vocabulary aligned to the `t.*` names where they overlap.

---

## 8. Recorder twin (lock-step)

`crates/zeroship-migrate/src/frontend/migrate_ops.js` (the engine V8 recorder) mirrors the **same** fluent
surface — the engine evaluates authored migrations through it, so it must expose `table()`/selectors/terminals
identically and emit byte-identical ops. The JS↔Rust `of_ir` round-trip + variant-exhaustiveness gate stay
green.

---

## 9. Implementation plan

1. **Rewrite `sdks/migrate/src/{ops.ts,types.ts,index.ts}`** to the fluent surface: `table()` + `TableHandle`
   + the selector builders + the immutable `t.*` chain + the recorder + the `SELECTOR_NOT_TERMINATED` guard.
   Keep internal op-construction helpers; remove flat exports.
2. **Mirror in the recorder twin** `migrate_ops.js`.
3. **C1 IR change** (FK actions) end-to-end (ir/schema/checksum/render/twin/of_ir/golden).
4. **Rewrite the test suites** (`sdks/migrate/tests/*`, the Rust twin `full_surface`/`op_round_trip`) to the
   fluent surface; assert the recorded IR is byte-identical to the pre-redesign golden (except C1), proving
   the surface change is pure sugar.
5. **Rewrite `docs/reference/migrate-op-dsl.md`** to the fluent-only surface; keep the doc-example CI gate green.
6. Verify: migrate lib + control deploy + `@zeroship/migrate` JS suite + of_ir round-trip + clippy, all green
   on real PG :5440 + SQLite.

Drive via implement → adversarial critic → fix → independent verify; commit-only, never push; commit incrementally.

**Deferred:** PR14 offline `--sql` preview (WIP at `fe732dd1`) resumes on the cleaned fluent surface.
