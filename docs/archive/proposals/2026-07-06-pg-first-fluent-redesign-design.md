# PG-first fluent surface — the `@zeroship/migrate` re-redesign (2026-07-06)

**Status:** approved direction (operator, 2026-07-06). Supersedes the `/pg` two-tier *placement* decisions of the 2026-07-04 surface redesign (S1/S4/S6a/L1); keeps their *good* parts (honest names, structured `Duration`, `setRls`, selector grammar, named payloads). Governed by the new **P0 — PostgreSQL is first-class** (added to `2026-07-04-dsl-surface-redesign-design.md`).

## 1. The thesis

PostgreSQL is the platform's first-class target. The core authoring surface **is** PostgreSQL-shaped; we do **not** bend it toward a lowest-common-denominator portable core for other vendors. Portability to SQLite/MySQL is an **explicit, opt-in** concern expressed with `dialect({...})`, and any construct with no native realization on a target — and no `dialect()` leg — **fails closed with an error at that target**. There is no `@zeroship/migrate/pg` import and no `c.pg.` casting to *use* a PG feature: PG features live on the core surface.

The engine already has everything the portability half needs: the per-op/per-node/per-option **dialect table**, the validate-time fail-closed gate (`DIALECT_UNSUPPORTED`), and the `dialect({...})` escape. This redesign is mostly a **surface** change (move things off `/pg`/`c.pg` onto core, flip the expression algebra to receiver-first chain methods, rename the builder) plus **one engine addition** (`dialect()` at op/spec granularity — "dialectal ops").

## 2. The fluent surface

### 2.1 `col` — the expression entry (was `c`)
The callback param is a **context-typed column-reference maker**, named **`col`**:
- `col("status")` — a column ref; `col("orders", "id")` — qualified (join ON).
- It is **context-typed**: in an immutable position (index / generated-column expression) it returns an *immutable* chain (volatile ops absent → tsc error); in a trigger `when` it carries `old`/`new`; in `having`/`select` it carries aggregates. Context safety for **chain methods** stays at tsc.

### 2.2 Operators & receiver-ful functions are chain methods
Every operator and every function that has a natural receiver is a **receiver-first chain method** on the expression — one uniform model (this is what S2 already did for `and`/`or`/`not`, generalized):

`.eq .ne .lt .le .gt .ge .and .or .not .isNull .isNotNull .isTrue .isFalse .add .sub .mul .div .mod .concat .in .notIn .like .notLike .between .distinctFrom .cast({ to }) .lower .upper .length .trim .coalesce .nullif .substr .replace .extract .round .floor .ceil .abs` — **plus the ex-`c.pg` operators, now first-class:** `.regex(pattern) .columnSize()` and aggregate methods `.count() .sum() .avg() .min() .max()`.

`.regex` renders `~` on PG, `REGEXP` on MySQL, and **fails closed on SQLite** (no stock regex). `.columnSize` renders `pg_column_size()` on PG and fails closed elsewhere (PG-specific). Both are dialect-table rows — first-class on core, portability by the existing gate.

### 2.3 Value constructors are top-level imports
Receiver-less value producers are **top-level imports** (consistent with `lit`/`decimal`/`byteValue` already being imports), so most defaults/values need **no callback**:

`now() uuidv7() genRandomUuid() currentSetting(name, { missingOk? }) currentUser() interval({ days, … }) nextval(name, { schema? }) lit() decimal() byteValue()`

```js
id:         t.uuid().primaryKey().default(uuidv7()),   // no (c) => …
created_at: t.timestamp().notNull().default(now()),
using:      (col) => col("app_id").eq(currentSetting("shop.tenant").cast({ to: "uuid" })),
```
**Cost, owned:** a volatile constructor (`now()`) placed inside an *immutable* index/generated expression is caught at **validate**, not tsc (a top-level import can't know its position). Everything else keeps its tsc catch. Accepted per P0's ergonomics-first stance.

### 2.4 PG features are first-class on core (no `/pg`, no `c.pg`)
Reachable directly from `@zeroship/migrate`, fail-closed off-PG, `dialect()` to port:
- **DDL:** `schema()`, `role()`, `extension()`, `grant`/`revoke`, `createFunction`, RLS (`setRls`), policies, triggers, sequences, domains — all core imports/handles (no `pgTable`, no `/pg` subpath).
- **Inline vendor options on core `create()`/`insert()`:** `exclusions`, the full `IndexMethod` set (`btree|hash|gin|gist|spgist|brin|ivfflat|hnsw|fts5`), `include`/`with`/`only`/`nullsNotDistinct`, `onConflict`. No `pgTable`-widened variant, no `PgCreateTableArgs`.
- **Expression nodes:** regex, columnSize, currentSetting, currentUser, PG `extract` fields — chain methods / imports on core (§2.2–2.3).

`@zeroship/migrate/pg` as an *ergonomic/type* boundary is **retired**. A distinct home survives only for genuinely-isolated concerns (`raw`/`rawSelect` stay P11 counted debt instruments by their `reason`/ratchet — that is P11's gate, not a `/pg` import).

## 3. `dialect()` — one context-aware generic, at expression AND op/spec granularity

`dialect({...})` is a **single generic** whose leg type + result type follow the position; each leg is **validated under its own dialect**; an **omitted leg = the thing is absent on that target**.

```ts
declare function dialect<T>(legs: { default?: T; pg?: T; sqlite?: T; mysql?: T }): Dialectal<T>;
```

- **Expression position** (`T = Expr`): `col("ts").gt(dialect({ pg: now().sub(interval({days:7})), sqlite: … }))`.
- **Op/spec position** (`T = IndexSpec | ColumnDef | …`): wrap the whole spec where the *legs actually differ*:
  ```js
  indexes: [
    { name: "sku_key", on: ["sku"], unique: true },            // portable
    dialect({ pg: { name: "embed_hnsw", on: ["embedding"], using: "hnsw", with: { m: 16 } } }),
    // mysql/sqlite omitted → simply no vector index there
  ]
  ```
- **Three ways context-aware:** leg **type** (per position, tsc-checked — a string in an Expr slot or an Expr in an index slot is a tsc error), leg **validation** (each leg walked under its own dialect — a `pg` leg with `hnsw` is legal; a `sqlite` leg with `hnsw` fails), leg **presence** (omitted = absent on that target).

**Granularity rule — wrap where the legs differ:** a value differs → wrap the expression; a whole index/constraint differs or is PG-only → wrap the spec (omit legs = absent); the whole feature only exists on PG → don't wrap, just use it (fail-closed off-PG). For `hnsw`: just use it (PG-first default), or op-level `dialect()` when you actually run multi-target.

**The one engine addition:** `dialect()` must be accepted at **op/spec positions**, not only inside expressions. The IR gains a **dialectal-op** form (the op-level sibling of today's `dialect` expression node): the op carries its per-dialect legs, the renderer selects the target's leg (absent leg ⇒ op skipped), per-leg validation as above.

## 4. What this reverses / keeps (vs the landed refactor)

| Landed slice | Kept | Reversed (moved to core / chain / import) |
|---|---|---|
| S1 (vendor-leak → `c.pg`) | honest names (`regex`, not `.matches`) | `c.pg.regex/columnSize/currentSetting` → `.regex()`/`.columnSize()` chain + `currentSetting()` import |
| S4 (`/pg` grammar) | handle grammar (`schema(n).create()`) | `/pg` import → core import |
| S6a (interval) | structured `Duration` | `c.pg.interval` → top-level `interval()` |
| L1 (`c.pg.columnSize` rename) | — | moot (columnSize is a chain method on core) |
| S2/S3b/S5/L2/L3/L4/L5 | all (not vendor-boundary) | — |
| L6 (M13 → `/pg`) | — | **dropped** (vendor options stay first-class on core) |

## 5. Implementation jobs (queued)

Sequenced; each verified DB-free while `:5440` is shared, live-PG confirm when free.

- **J1 — `dialect()` op/spec generalization (engine + surface).** Generic `dialect<T>()`; accept at op/spec positions; the **dialectal-op** IR + per-target render (absent leg ⇒ skip) + per-leg validation. Foundational.
- **J2 — expression algebra → chain methods, de-`c.pg`.** `.regex`/`.columnSize` chain methods on core (drop `c.pg`); receiver-ful scalar fns → chain methods; wire nodes unchanged (dialect table already PG-marks them). Add the **MySQL `REGEXP`** leg for regex.
- **J3 — value constructors → top-level imports.** `now/uuidv7/genRandomUuid/currentSetting/currentUser/interval/nextval` as core exports returning `Expr`; drop the `c.fn.*` / `c.pg.*` namespaces. Validate-time immutability catch for volatile ones in immutable positions.
- **J4 — rename `c` → `col`.** Builder handle + type + all callbacks; context-typing preserved.
- **J5 — de-`/pg` the DDL grammar.** `schema/role/extension/grant/revoke/createFunction/domain/sequence/pgTable→table` to core imports/handles; fail-closed off-PG. Retire the `/pg` subpath (keep only `raw`/`rawSelect`/`secretRef` if isolation is still wanted, else fold to core with P11 gate).
- **J6 — vendor options first-class on core `create()`/`insert()`.** Full `IndexMethod` set, `exclusions`, `include/with/only/nullsNotDistinct`, `onConflict` on core; fail-closed off-PG. (Undo any L6-style `/pg` widening.)
- **J7 — corpus + fixtures + docs rewrite.** All `db/migrations-ts` + fixtures + `docs/reference/migrate-op-dsl.md` + examples flip to the new surface; goldens regen once.
- **J8 — vendor rendering legs (incremental).** Where a feature exists elsewhere, add the leg (regex→MySQL `REGEXP`, interval→per-dialect, extract→per-dialect); everything else fails closed + `dialect()`.

Exit: `pnpm build` + 3-dialect cargo green; the `db/migrations-ts` corpus applies on live PG; the reference docs regenerate from executed snippets.
