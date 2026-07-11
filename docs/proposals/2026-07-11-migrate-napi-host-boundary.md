# The JS host boundary for a napi-embeddable `@zeroship/migrate` engine

**Status:** design-only (phase: map the JS HOST boundary)
**Branch:** `design/migrate-napi-shell`
**Date:** 2026-07-11

This document maps *exactly* what a Node/Bun JS host must own once the
`zeroship-migrate` engine ships as a napi (N-API) addon whose Rust core is
V8-free. The blueprint is Temporal's TS SDK worker: a V8-free Rust core that
runs its own async runtime on worker threads and calls back into the JS host
(schema authoring + DB drivers) over N-API threadsafe-functions. No second V8
is embedded in Rust.

It is grounded in the *current* code on this branch: the existing V8 recorder
front-end, the existing `PgSession` seam, and — crucially — the **existing
MySQL `JsDriverBackend`**, which is already a working "JS-driver-supplies-rows
over a serialized boundary" precedent. The napi PgSession is that same pattern
with N-API replacing the in-Rust V8 isolate.

---

## 0. The three things the host must own

Today the `zeroship-migrate` crate carries V8 (via `zeroship-runtime`) for
exactly two jobs, both of which move host-side under the napi shell:

1. **Authoring** — evaluating `schema.js` / migration `.ts` to produce op IR.
   Today: a V8 recorder isolate (`src/frontend/`) evals the module and drains
   `globalThis.__zsOpIR`.
2. **The MySQL driver** — `mysql2` in a V8 isolate over `node:net`
   (`src/apply/backend/mysql/`).
3. **(implied) The Postgres driver** — today native `compio-postgres`; on a
   cross-platform npm addon this is a candidate to move host-side too (§2.4).

Under the napi shell, **the Rust core keeps the engine spine** (differ, planner,
canonical IR, checksum, executor, journal, gate, drift) and **the JS host
supplies the two V8-shaped halves** (authoring + drivers). The core becomes
V8-free and — per the established decoupling arc — compiles `--no-default-features`
with no V8, no compio-mysql, and (for a cross-platform addon) optionally no
compio-postgres.

---

## 1. Authoring: can the Node host produce the same `.ir.json` purely in JS?

### 1.1 How `schema.js` / `.ts` → op IR happens today (the V8 recorder)

Two distinct authoring lowerings exist, both V8-hosted today:

**(a) Migration `.ts` → op IR (the recorder).**
`crates/zeroship-migrate/src/frontend/op_recorder.js` is the runtime-entry glue
evaluated inside a V8 recorder isolate. It:

- `import * as userMod from "./__migration__.js"` (the creator migration, already
  bundled to self-contained ESM), and
  `import { __begin, __drain } from "@zeroship/migrate"`.
- Resolves `up()` / `down()` (named exports or `default.{up,down}`).
- Per phase: `__begin("up")` installs a **fresh ambient recorder**, calls the
  phase function (whose `table(...).column(...).add(...)` calls record ops into
  the ambient buffer via the `defineOp` chokepoint → `push`), then `__drain()`
  returns the `Node[]` op list.
- Emits the envelope on `globalThis.__zsOpIR`:

  ```js
  const envelope = { ir_version: 6, name: resolveName(mod), ops };
  globalThis.__zsOpIR = JSON.stringify({ ok: true, ir: envelope });
  ```

The Rust host (`src/frontend/record.rs` / `recorder_service.rs`) reads that
string back, deserializes it into `MigrationIr`, **stamps `owner_app`** (a
tenant-identifying provenance field that untrusted `up()` must not be able to
set — see the SECURITY note in `op_recorder.js`), and folds the **single
authoritative** `Checksum::of_ir` (`CanonicalOpList::canonical_bytes`). The JS
side deliberately does **not** compute the checksum.

**(b) `schema.js` (the `@zeroship/db` `t.*` DSL) → descriptor IR (the adapter).**
`src/frontend/ir_adapter.js` imports `__schema__.js` + `TypeBuilder` from
`@zeroship/db`, walks the schema map, calls the **pure** `TypeBuilder.toFieldDef()`
+ `normalizeSchema`, translates SDK `FieldDef` names → engine `FieldDescriptor`
names (`refTarget → ref`, …), and stashes serialized `CollectionDescriptor[]` on
`globalThis.__zsSchemaIR`. Consumed by `src/frontend/eval.rs::eval_schema_to_ir`.

Both are pure lowerings: no `installSchema`, no live `env.db`, no network, no fs.

### 1.2 What the TS DSL in `sdks/migrate` already produces

`sdks/migrate/src/ops.ts` (4309 lines) is **the same recorder** — it is the typed
peer of `op_recorder.js`, and per the S0.5 "one compiled recorder artifact"
decision, `sdks/migrate/src/embedded-recorder.ts` is `tsup`-bundled into the
`dist/embedded-recorder.js` that the crate `include_str!`s as the `@zeroship/migrate`
module inside its V8 isolate. In other words: **the SDK build output and the
engine-embedded recorder are already byte-identical.**

The recorder seam that authoring needs is already exported:

- `__begin(phase)` / `__drain()` — the ambient recorder lifecycle
  (`embedded-recorder.ts` re-exports both).
- The full op-producer surface (`table`, `view`, `enumType`, `t`, `dialect`, …)
  and value factories (`lit`, `decimal`, `byteValue`, `now`, …).
- The determinism lint (`lintDeterminism`).
- Structured authoring errors (`OP_OUTSIDE_RECORDER`, `SELECTOR_NOT_TERMINATED`,
  `OP_INVALID`) with `code` + `suggested_fix`, thrown synchronously.

The op-list shape the recorder drains (`Node[]`) is exactly the `ops` array of
the `ir_version: 6` envelope; the wire IR types are generated from the engine's
`op-ir.schema.json` (`sdks/migrate/src/generated/ir.ts`, re-exported as `ir` from
`index.ts`).

### 1.3 Yes — the Node host can produce the SAME `.ir.json` purely in JS

The recorder is **already ordinary ES modules** that run in any modern JS engine.
Nothing in the authoring path needs V8-in-Rust; the V8 isolate was only ever a
*sandbox* for untrusted `up()`, not an authoring requirement. So the host flow is:

```
Node/Bun host:
  1. transpile + bundle the creator migration .ts → self-contained ESM
     (esbuild/tsup — already the JS build pipeline's job; the Rust runtime
      never had a transpile step, see eval.rs "Input contract").
  2. import the bundle + `@zeroship/migrate`.
  3. for each phase: __begin(phase); userMod.up(); const ops = __drain();
  4. build the envelope { ir_version: CURRENT_IR_VERSION, name, ops }.
  5. hand the envelope (as JS object or JSON string) to the Rust core over N-API.

Rust core (V8-free):
  6. deserialize → MigrationIr.
  7. STAMP owner_app from the host-supplied, ownership-checked app_id
     (NOT from the JS envelope — same security invariant as today).
  8. fold Checksum::of_ir  → the single authoritative checksum.
  9. differ / planner / executor as today.
```

The **checksum invariant is preserved for free**: the JS side emits ops, Rust
folds `Checksum::of_ir`. Whether the ops were drained inside a Rust-owned V8 or
inside the host's own JS engine is irrelevant to the fold — it is value-equality
over the canonical op list, invariant under JCS-formatting differences (this is
exactly what the existing `op_round_trip.rs` gate already asserts between the SDK
recorder and the Rust re-canonicalization).

### 1.4 What is missing from the TS side to do this without the Rust V8 recorder

Small and mechanical. The recorder logic is complete; what's missing is a **thin
host-driver entry** that today lives in Rust glue (`op_recorder.js` +
`record.rs`) and must be re-expressed as a host-callable JS function:

1. **A host recorder entry** (new, tiny) — the JS equivalent of `op_recorder.js`'s
   `try { __begin; up(); __drain } catch` wrapper, returning
   `{ ok, ir } | { ok:false, error }`. It belongs in the npm package (e.g.
   `@zeroship/migrate/host` or the addon's JS wrapper), not in the Rust
   `include_str!`. It must re-implement `resolveMigration` / `resolveName`
   (both currently in `op_recorder.js`, not exported from the SDK).
2. **`CURRENT_IR_VERSION` as a single source of truth across the boundary.**
   Today `ir_version: 6` is hard-coded in `op_recorder.js` and
   `CURRENT_IR_VERSION = 6` in `model/ir.rs`. The host entry must not re-hardcode
   it; expose it from the addon (the Rust core is the authority) and have the JS
   entry read it back, or keep the existing generated-types drift gate
   (`ir-types-drift.test.ts`) extended to assert the version constant matches.
3. **The `crypto`/`Date.now`/`Math.random` capture-by-identity guard** (top of
   `ops.ts`) already works in Node/Bun — the guards are `if absent` and become
   pure no-ops where real Web Crypto exists. Nothing to add; just verify the
   determinism-lint's "bare symbol = DB-side eval" identity capture still fires
   in the host engine (it captures `globalThis.crypto.randomUUID` at module load;
   fine under Node/Bun).
4. **Sandboxing is now the host's problem, not ours.** The Rust V8 isolate gave
   untrusted `up()` a resource budget + wall watchdog + no-fs/no-net kernel
   layers (`SandboxPosture`, `ResourceBudget`). A pure-JS host recorder runs
   `up()` in the host's own JS context — so **for the self-host / local-CLI
   posture (trusted, creator's own machine) this is fine and matches today's
   `SandboxPosture::Local`.** For a *hosted multi-tenant* recorder the sandbox
   must be re-established host-side (worker thread + `vm` context + timeout, or
   keep the Rust V8 recorder service for that one posture). **This is the one
   real capability the napi authoring path drops vs. the Rust V8 recorder** and
   should be called out as a scoping decision: the napi addon targets the
   *trusted local* authoring posture; multi-tenant sandbox recording stays a
   separate concern.

For the descriptor path (§1.1b), `TypeBuilder.toFieldDef()` / `normalizeSchema`
already live in `@zeroship/db`; the host imports them directly. The only Rust
piece that must accept host input is `eval_schema_to_ir`'s consumer — it becomes
"deserialize `CollectionDescriptor[]` JSON from the host" rather than "read
`globalThis.__zsSchemaIR` out of our V8".

**Net:** authoring moves host-side with *no new mechanism* — it reuses the
already-shared recorder artifact. The deliverables are a thin JS host-entry
function + an `ir_version` single-source-of-truth + an explicit "sandbox is
host-side / trusted-local scope" decision.

---

## 2. Drivers host-side: the host-callback `PgSession`

### 2.1 The seam today, and the read-widening gap

`src/apply/backend/postgres/session.rs` defines `trait PgSession` with five
in-session verbs: `batch_execute`, `execute`, `execute_text_params`, `query`,
`query_one`. `PostgresBackend<'a, D: PgSession = Client>` is generic over it; the
default impl forwards to `compio_postgres::Client`.

**The blocker** (already documented in that file's own docstring): the trait
signatures name `compio_postgres::{Row, Error, types::ToSql}`. `query*` returns
`Vec<compio_postgres::Row>` and `execute*` returns `compio_postgres::Error` —
**both have private constructors**, so a non-compio host driver *cannot build a
return value*. Only the write path is run-proven generic.

The apply path consumes rows via **typed accessors**:
`row.get("col")` and `row.get::<_, i64>("col")`. Across `src/apply/` there are
~222 such accessor call sites, but the **distinct target types are tiny**:
`String` (×6), `i64` (×2), `bool` (×1), `Option<String>` (×1), `Option<i32>` (×1).
That small closed set is what a driver-neutral row must satisfy.

### 2.2 `SeamRow` / `SeamError` — the read widening (the crux)

Introduce driver-neutral read types so a host driver can *return* rows and errors.
The design is already prefigured by the MySQL backend, which marshals rows as
`Vec<Map<String, Value>>` (JSON objects) — **the exact shape to reuse**:

```rust
// New, driver-neutral, in the postgres/session module (or a shared seam mod).
pub struct SeamRow {
    // column-name → typed cell. A JSON-value carrier is the proven choice
    // (mirrors mysql::RowSet's Vec<Map<String,Value>>), but PG needs a few
    // types JSON can't losslessly carry (bytea, int8, numeric), so the cell
    // is a small typed enum, not raw serde_json::Value:
    cells: Vec<(String, SeamCell)>,
}

pub enum SeamCell {
    Null,
    Bool(bool),
    Int(i64),          // int2/int4/int8 widened to i64
    Float(f64),
    Text(String),      // text/varchar/uuid/timestamptz-as-text/numeric-as-text
    Bytes(Vec<u8>),    // bytea
}

impl SeamRow {
    // The typed accessor the apply path already calls. Replaces the two
    // compio-postgres `.get`/`.get::<T>` forms with a driver-neutral one.
    pub fn get<T: FromSeamCell>(&self, col: &str) -> T { … }
}

pub struct SeamError {   // replaces the concrete compio_postgres::Error return
    pub sqlstate: Option<String>,   // the 5-char PG code (drives conflict/retry logic)
    pub message: String,
    pub kind: SeamErrorKind,         // Db | Connection | Marshal | Timeout
}
```

`FromSeamCell` is implemented for exactly the closed accessor set (`String`,
`i64`, `bool`, `Option<String>`, `Option<i32>`, plus `Vec<u8>` for bytea if any
introspection reads it). The compio impl builds a `SeamRow` from a
`compio_postgres::Row` (it *can* read its own private type); the host impl builds
a `SeamRow` from the marshalled N-API payload. Both satisfy the same `.get`
call sites → **the ~222 accessors change once (`row.get` signature) and never
again.**

> Note: this widening is the ONE Rust-core change that is a prerequisite for
> *any* host driver (napi PG or a future compio-free anything). It is worth
> landing on its own, decoupled from the napi addon, with the compio impl as the
> first `SeamRow` producer so the native path stays green.

### 2.3 The host-callback PgSession over an N-API threadsafe-function

The Rust core runs the engine on its own worker thread (Temporal-style). Each of
the five verbs, when it needs the DB, calls back into the JS host via a
**`ThreadsafeFunction`** and awaits the JS Promise result. napi-rs's
`ThreadsafeFunction` is callable from any Rust thread; with the async/tokio
feature it returns a Rust future that resolves to the JS callback's returned
value (incl. a Promise) [napi-rs docs: "Call Async with Unknown Return Value" /
`callAsyncWithUnknownReturnValue`; `#[napi] async fn` requires the `async`
feature]. That is precisely the primitive needed.

**What crosses the boundary (per verb):** an SQL string + ordered typed params
→ marshalled rows (or affected-count) or a structured error. Concretely:

```
Rust core                         N-API boundary                 JS host
─────────                         ──────────────                 ───────
PgSession::query(sql, binds) ───▶ tsfn.call_async({             const client = new pg.Client(dsn)
                                    kind: "query",
                                    sql,                          async (req) => {
                                    binds: [SeamBind…]  })          const r = await client.query(
                                                                        req.sql, req.binds.map(fromSeamBind))
   await Promise ◀──────────────── returns Promise<{             return {
                                     rows: [{col: cell,…}],           rows: r.rows.map(toSeamRow),
                                     rowCount }>                       rowCount: r.rowCount }
                                                                  }
   marshal → Vec<SeamRow> ◀─────── or throws → { error:{ sqlstate, message } }
```

**Marshalling — params (`BindValue`/`SeamBind`) out:** reuse the existing
`BindValue` enum (`Null | Bool | Int(i64) | Decimal(String) | Text(String)` —
`render/step.rs`). The MySQL backend already has the exact JS mapping
(`bind_to_json`): `Int → number`, `Decimal|Text → string`, `Bool → bool`,
`Null → null`. For napi this becomes a `#[napi(object)]` `SeamBind` struct/enum
so it crosses as a native JS value, not a re-parsed JSON string. **Crucially,
`execute_text_params` must keep text-format binds** (the seam's own docstring
notes PG refuses a concrete-OID binary bind for `text → timestamptz`), so text
binds cross as JS strings and the host driver hands them to `pg` as text params
— `pg` already sends parameters in text format, so this is natural.

**Marshalling — rows (`SeamRow`) in:** the host returns
`Array<{ [col]: cell }>` where each cell is JS `null | boolean | number | string`
(+ `Buffer`/`Uint8Array` for bytea, + string for int8/numeric to avoid f64 loss).
Rust builds `SeamRow` from the N-API object array. This is *the same marshalling
the MySQL `query_json` path already does* (`Vec<Map<String,Value>>`), lifted from
"serde_json over a V8 channel" to "N-API native objects".

**int8 / numeric fidelity:** `node-postgres` returns `bigint`/`numeric` columns
as **strings** by default (no lossy f64). That maps cleanly to `SeamCell::Text`
and the engine's `i64`/`Decimal(String)` domains. The host wrapper should set a
`pg` type parser so int8 → string (or JS `bigint` if the addon marshals bigint),
never JS `number`, to preserve the IR's exact-integer invariant.

**Errors (`SeamError`) in:** the host catches the driver error and returns
`{ error: { sqlstate: err.code, message: err.message } }` (node-postgres puts the
PG SQLSTATE on `err.code`). Rust maps to `SeamError`. This mirrors the MySQL
backend's `remoteError(err)` → `{ code, sqlstate, message }` → `JsDriverError::Remote`
precedent exactly; `sqlstate` is what the engine's conflict/lock-retry logic keys
on.

**Transaction control / advisory locks / confinement SETs are NOT new callbacks.**
Per the seam docstring, `BEGIN`/`COMMIT`/`ROLLBACK`, `pg_advisory_lock`, and
`SET search_path`/role confinement are SQL strings issued through
`batch_execute`/`execute` — they ride the existing five verbs. The host driver
needs no transaction-object abstraction; it just runs the SQL the core sends on
its single pinned connection. **Connection pinning matters:** the whole apply is
one session (advisory lock + txn + SETs are connection-scoped), so the host must
bind ONE `pg` client to the addon session for its lifetime — not a pool. (The
MySQL backend already enforces exactly this: one `conn`, non-reentrant,
one-command-in-flight.)

### 2.4 mysql2 and bun:sqlite / better-sqlite3 — the analogous paths

**MySQL.** Trivial under this model — it *is* the existing `JsDriverBackend`,
minus the Rust-embedded V8. `src/apply/backend/mysql/transport.rs` already speaks
`{ kind, sql, binds }` → `{ ok: rows } | { err: { code, sqlstate, message } }`
over a JS driver loop with `mysql2/promise`. Under napi, the JS driver loop
(`MYSQL_DRIVER_ENTRY`) becomes a host module the addon calls via the same tsfn
verb protocol — `conn.execute(sql, binds)` → rows — and the Rust
`JsDriverConn`/`RowSet` marshalling is replaced by the shared `SeamRow`/`SeamBind`
N-API marshalling. The `RowSet { rows: Vec<Map<String,Value>> }` type and its
`value_to_string` accessors already prove the read side works; they fold into the
`SeamRow` unification. **The MySQL TLS-pin policy, net-policy allowlist, and
timeout/poison logic move host-side** (they were properties of the Rust V8 net
stack; under napi the host owns sockets, so the host driver wrapper enforces
TLS/allowlist, or the addon passes policy down as config the host honors).

**SQLite — a real design fork (the cross-platform note).**

- **Option A — native `rusqlite` (bundled), stays in the Rust core.** rusqlite is
  cross-platform (unlike compio/io_uring). SQLite is in-process and synchronous,
  so it needs no host callback at all — the core opens the file and runs
  statements directly. The existing SQLite backend (`apply/backend/sqlite/`) is
  already in-process (an `actor` + authorizer + rebuild/backfill SQL emitters);
  it keeps working with zero host involvement. **This is the recommended default:
  SQLite = native + bundled, PG/MySQL = host-provided.** It gives the addon a
  batteries-included embedded engine with no host driver dependency, and it's the
  natural cross-platform story.
- **Option B — host-provided (`bun:sqlite` / `better-sqlite3`).** Same
  host-callback `PgSession`-shaped seam: `db.query(sql).all(binds)` → rows,
  `db.run(sql, binds)` → changes. Both are *synchronous* APIs, so the tsfn
  callback resolves immediately (no real Promise await needed, though the seam is
  uniform). Worth offering as an override for hosts that want ONE driver surface
  (all three DBs host-side) or Bun users who prefer `bun:sqlite`. But it is an
  option, not the default.

The seam is dialect-agnostic: `PgSession` is really "a `SqlSession` the backend
is generic over." Rename note aside, the *same* five-verb + `SeamRow`/`SeamError`
contract serves PG, MySQL, and host-SQLite; the dialect differences already live
in the render/backend layer, not the session seam.

### 2.5 Cross-platform packaging consequence

To ship a cross-platform npm addon: **PG and MySQL host-provided (no compio,
no compio-mysql)** + **SQLite native via bundled `rusqlite`** yields a Rust core
that needs zero io_uring and zero V8. That is the `--no-default-features` core
this branch's decoupling arc already built toward, plus the `SeamRow` widening
from §2.2. compio stays *available* behind `native-pg` for the in-repo,
Linux-only, self-hosting server build (which keeps the fast native path); the
npm addon just doesn't enable it.

---

## 3. The npm package shape

### 3.1 Artifacts

```
@zeroship/migrate            (the npm package)
├── dist/                    the TS DSL + host glue (tsup, ESM)
│   ├── index.js             public authoring API: table, t, view, dialect, …
│   │                        (unchanged — this is today's sdks/migrate surface)
│   ├── host-recorder.js     NEW: the host authoring entry (§1.4) —
│   │                        __begin/up()/__drain wrapper → { ok, ir } envelope
│   └── driver-*.js          NEW: host driver wrappers (pg / mysql2 /
│                            bun:sqlite|better-sqlite3) → the SeamSession callback
├── native/
│   └── migrate.<platform>.node   the napi addon: V8-free Rust engine core
│                                 (differ/planner/executor/journal/gate/checksum)
│                                 + optional bundled rusqlite
└── package.json             optionalDependencies: pg / mysql2 (peer-ish),
                             prebuilt .node per platform (napi-rs prebuild flow)
```

The `.node` addon exposes napi entry points (Temporal-style): the core spins its
own async runtime on a worker thread; the JS API is a thin async facade that
registers the host callbacks (recorder + driver) as threadsafe-functions and
`await`s the core's returned Promises.

### 3.2 What the creator calls

```ts
import { apply, plan, generate } from "@zeroship/migrate/host";

// APPLY — the primary verb.
await apply({
  dir: "./migrations",          // migrations/<14digit>_<desc>.ts
  driver: { kind: "postgres", url: process.env.DATABASE_URL },
  // or { kind: "mysql", url }  → host mysql2 wrapper
  // or { kind: "sqlite", file: "./app.db" }  → native rusqlite (default)
  // approve: (step) => boolean  // the destructive-op gate, host-decided
});

// PLAN — dry-run: record + diff + emit the plan, no DB writes.
const plan = await plan({ dir, driver });

// GENERATE — schema.js → diff live DB → new migration .ts (authoring).
await generate({ schema: "./schema.ts", dir, driver });
```

Under the hood `apply`:

1. **Host authoring:** for each `migrations/*.ts`, esbuild-bundle → import →
   `host-recorder.js` runs `__begin/up()/__drain` → `{ ir_version, name, ops }`
   envelope (pure JS, §1.3).
2. **Hand envelopes + a driver callback to the addon.** The addon deserializes
   envelopes → `MigrationIr`, stamps `owner_app`, folds `Checksum::of_ir`, runs
   the differ/planner, and drives the executor. Whenever the executor needs the
   DB it invokes the registered `SeamSession` tsfn (query/execute/batch) against
   the host `pg`/`mysql2`/`sqlite` wrapper; rows/errors marshal back as
   `SeamRow`/`SeamError` (§2.3).
3. **The destructive-op gate + drift + journal** are unchanged engine logic in
   the Rust core; only their DB I/O flows through the host callback.

The creator never sees N-API, threadsafe-functions, or `SeamRow` — just
`apply({ dir, driver })`. The DSL they *author* with (`table`, `t`, `dialect`,
…) is exactly today's `@zeroship/migrate` public surface (`sdks/migrate/src/index.ts`),
unchanged.

---

## 4. Summary of the boundary + the deliverables

| Concern | Owner under the napi shell | New work |
|---|---|---|
| DSL authoring surface (`table`/`t`/`dialect`/…) | JS host (npm) | none — already `sdks/migrate` |
| Recorder (`__begin`/`__drain`, op producers) | JS host (npm) | none — already the shared `embedded-recorder` |
| Migration `.ts` → op IR envelope | JS host | thin `host-recorder.js` entry (§1.4) + `ir_version` single-source |
| `schema.js` → descriptor IR | JS host (`@zeroship/db` `TypeBuilder`) | host adapter re-expressing `ir_adapter.js`; Rust consumer takes host JSON |
| Checksum / provenance / `owner_app` | **Rust core** (authoritative) | none — core keeps `Checksum::of_ir`, stamps `owner_app` |
| Differ / planner / executor / journal / gate / drift | **Rust core** (V8-free) | none structurally |
| `SqlSession` read type (`SeamRow`/`SeamError`) | seam, both impls | **the crux: read-widening** (§2.2), lands with compio impl first |
| PG / MySQL driver | JS host (`pg` / `mysql2`) over tsfn | driver wrappers + `SeamBind`/`SeamRow` N-API marshalling |
| SQLite driver | **Rust core** (bundled `rusqlite`, default) *or* host (`bun:sqlite`/`better-sqlite3`) | native rusqlite path is the cross-platform default |
| Sandbox for untrusted `up()` | JS host (if multi-tenant) | **scoping decision**: napi addon targets trusted-local; hosted sandbox stays separate |
| TLS-pin / net-allowlist / timeouts | JS host (owns sockets) | move MySQL policy logic host-side |

**Two things must be built in the Rust core; everything else reuses existing
mechanism:**

1. **`SeamRow`/`SeamError` read-widening** of the `PgSession` (→ `SqlSession`)
   seam (§2.2) — the one prerequisite for any host driver. Land it independently
   with the compio impl as the first producer.
2. **The napi entry layer**: worker-thread async runtime + threadsafe-function
   host callbacks for (a) the driver `SqlSession` and (b) accepting the
   host-recorded IR envelope; marshalling `SeamBind`/`SeamRow`/`SeamError` as
   `#[napi(object)]` values.

Both are *strongly de-risked by the existing MySQL `JsDriverBackend`*, which
already proves the "JS supplies rows over a serialized `{sql,binds}→rows|err`
boundary, one pinned non-reentrant connection, SQLSTATE-carrying structured
errors" model end-to-end. The napi shell replaces its Rust-embedded V8 + serde
channel with N-API native values — same protocol, no second V8.
