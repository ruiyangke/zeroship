# zeroship-migrate as a Node/Bun N-API addon — build / crate / platform shape

Date: 2026-07-11
Status: Design-only (no code). Branch `design/migrate-napi-shell`.
Scope: **Phase — build/crate/platform.** Answers the four questions the phase
brief poses: (1) crate structure for the `.node` addon, (2) the feature matrix
(`zsv8` OFF, `native-pg` OFF) and the SQLite/PG driver-provenance decision,
(3) whether the Rust core still needs its own async runtime when all I/O is
host-provided, (4) N-API-on-Bun caveats for `bun build --compile`.

This is the **platform/packaging** half of the napi-shell design. The
**read-path seam widening** (`SeamRow`/`SeamError` so a non-compio host driver
can *return* rows — the known gap) is the sibling design; this doc references it
as a hard dependency but does not re-specify it. The build shape below is what
that seam is *for*.

---

## 0. Recap of the established base (do not re-litigate)

The decoupling arc already landed on this branch's base:

- **`zeroship-core` cut** — id/db_url vendored into `zeroship-migrate`, so the
  engine no longer transitively pulls the platform's inter-service wire crate.
- **V8 behind `zsv8`** — `src/frontend/` (JS schema authoring in a V8 recorder)
  and `src/apply/backend/mysql/` (mysql2 in a V8 isolate over `node:net`) are
  the *only* V8 users. `--no-default-features` compiles a V8-free core
  (guard + IR + render + PG-apply + SQLite-apply + journal).
- **PG driver seam** — `pub trait PgSession` in
  `src/apply/backend/postgres/session.rs` (`batch_execute` / `execute` /
  `execute_text_params` / `query` / `query_one`). `PostgresBackend<'a, D: PgSession = Client>`
  is generic. The compio-postgres impl is default behind `native-pg` (default-on).

**Verified during this phase (facts the design rests on):**

| Fact | Evidence |
| --- | --- |
| `PgSession` **read** verbs (`query`/`query_one`) return `Vec<compio_postgres::Row>` and every verb's error is `compio_postgres::Error` — both have **private constructors**, so a host driver can't build them. | `session.rs` trait signatures name `compio_postgres::{Row, Error, ToSql}`; the module doc itself flags "A future compio-free variant … lands with the second (Node/napi) driver impl." |
| Read consumers use **typed column access**: `row.get::<_, T>("colname")` for `String`/`bool`/`i64`/`Option<String>`. | `backfill.rs` (~15 call sites), `shadow.rs:1196`. This is the surface `SeamRow` must reproduce. |
| The apply loop is driven by **`compio::runtime::Runtime::new().block_on(run_migrate())`**. | `command/runner.rs:2482`. |
| The **SQLite backend needs no compio** — it is a `std::thread::spawn` + `flume` single-writer actor over `rusqlite` (bundled). | `apply/backend/sqlite/actor.rs:227`; `rusqlite = { features=["bundled","load_extension"] }`. |
| `compio` is a **non-optional** workspace dep of `zeroship-migrate` today, even though `compio-postgres` is `native-pg`-gated. | `Cargo.toml`: `compio = { workspace = true }` (not `optional`). This is a loose end the addon build must tighten (§2.4). |
| There is **no napi crate anywhere** in the workspace yet. | `grep -rl napi crates/*/Cargo.toml` → empty. |

---

## 1. Crate structure — a NEW leaf `cdylib` crate wrapping the core

### 1.1 Recommendation

Add a **new crate**, `crates/zeroship-migrate-node`, that is a napi-rs `cdylib`
depending on `zeroship-migrate` with a **host-driver** feature profile. The core
crate is **not modified for packaging** — it only gains the `SeamRow`/`SeamError`
read widening (sibling design) so the host driver impls can exist. The addon
crate owns *all* napi surface: `#[napi]` exports, the threadsafe-function
plumbing that calls back into the host JS drivers, and the `napi_build::setup()`
in `build.rs`.

```
crates/
├── zeroship-migrate/          # core engine — UNCHANGED shape.
│   │                          #   + SeamRow/SeamError (sibling design) so a
│   │                          #     host driver can construct read results.
│   └── (lib crate-type = default rlib)
└── zeroship-migrate-node/     # NEW — the .node addon. cdylib.
    ├── Cargo.toml             #   [lib] crate-type = ["cdylib"]
    │                          #   napi = "3", napi-derive = "3"
    │                          #   [build-dependencies] napi-build = "1"
    │                          #   zeroship-migrate = { path = "../zeroship-migrate",
    │                          #       default-features = false,
    │                          #       features = ["host-pg"] }   # see §2
    ├── build.rs               #   napi_build::setup();
    ├── package.json           #   @napi-rs/cli — emits index.node + index.d.ts
    └── src/lib.rs             #   #[napi] exports + host-driver TSFN adapters
```

Sources for the napi-rs crate skeleton
([napi-rs README](https://github.com/napi-rs/napi-rs/blob/main/README.md),
[napi-build README](https://github.com/napi-rs/napi-rs/blob/main/crates/build/README.md)):

```toml
# crates/zeroship-migrate-node/Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
napi        = { version = "3", default-features = false, features = ["napi9"] }
napi-derive = "3"

[build-dependencies]
napi-build = "1"
```

```rust
// build.rs
fn main() { napi_build::setup(); }
```

`@napi-rs/cli` (`napi build --platform --release`) compiles the cdylib and
copies the platform `.so`/`.dylib`/`.dll` into a `.node` file plus a generated
`index.d.ts`. That is the npm-shippable artifact; the host JS
(`sdks/migrate/`, the existing TS DSL) `require()`s it.

### 1.2 Why a new crate, not a feature/target on the core

- **Keeps the graph acyclic and the core napi-free.** The core must still build
  `--no-default-features` as a **pure Rust library** for the platform's own use
  (control/worker embed it). Adding `napi`/`napi-derive` deps or a `cdylib`
  crate-type onto `zeroship-migrate` itself would (a) drag N-API into every
  embedder's dependency closure and (b) force the awkward "lib + cdylib" dual
  crate-type on the core. A leaf crate isolates the addon concern.
- **`cdylib` is terminal.** A `cdylib` cannot be depended on by another Rust
  crate; it is an end-artifact. So the addon crate is necessarily a **leaf** —
  nothing in the workspace depends on it, which is exactly the acyclic property
  we want. `zeroship-migrate-node → zeroship-migrate` is the only edge.
- **The core stays multi-consumer.** Same core rlib serves three consumers with
  three feature profiles: the platform (`native-pg` + `zsv8`), the lean embedder
  (`--no-default-features`), and the addon (`host-pg`, no v8, no compio-pg). One
  engine, three shells — mirrors the existing `zsv8`/`native-pg` split rather
  than inventing a new axis.

### 1.3 Workspace membership caveat

`crates/*` is already a workspace glob member, so the new crate joins
automatically. Two things to watch:

- **`resolver = "3"`** is set at the root — good; it keeps the addon's
  host-driver feature profile from unifying back onto the platform binaries
  (feature unification across a `cdylib` leaf won't leak into `control`/`worker`
  because nothing depends on the leaf).
- **A `cdylib` in the default workspace** means `cargo build` at the root now
  builds the addon too. If the napi toolchain (`@napi-rs/cli`, the `napi9` ABI
  headers) is not desired in the platform's `cargo build`, add the leaf to the
  root `exclude` list (like `refs`/`ntex-bench` already are) and build it only
  via `@napi-rs/cli` in the addon's own package. Recommend **exclude** — the
  addon is an independently-released npm artifact, not part of the platform
  server build.

---

## 2. Feature matrix — what the addon actually needs, and the SQLite decision

### 2.1 The addon's feature profile

The addon build turns **`zsv8` OFF** (the Node host evals the schema JS itself,
using the existing `sdks/migrate/` TS DSL — no second V8 embedded in Rust) and
**`native-pg` OFF** (the host provides PG via `pg`/`postgres.js` as a
`PgSession` callback). What remains enabled in the core is the **V8-free,
compio-PG-free** substrate: guard + IR + render + journal + **SQLite apply** +
the **generic** `PostgresBackend<D: HostPgSession>` driven by a host callback.

This requires a **new `host-pg` feature** on the core (sibling to `native-pg`)
that:
- gates the `SeamRow`/`SeamError` neutral read types **on** (they must exist
  whenever a non-compio driver can be plugged — so `host-pg` OR a future
  variant enables them; simplest: they are **always compiled**, unconditional,
  and `native-pg`'s compio impl *converts into* them);
- gates the compio-postgres `impl PgSession` **off** (that impl is `native-pg`);
- does **not** pull `compio-postgres`, `v8`, `zeroship-runtime`, or the sandbox
  deps.

> Design note carried from the sibling seam design: the cleanest shape is that
> `SeamRow`/`SeamError` are **unconditional** in the core (no feature gate — a
> plain data row + a boxed error), `native-pg` provides `From<compio Row/Error>`,
> and `host-pg` provides the napi/host construction path. The trait's read verbs
> return `SeamRow`/`SeamError` for **all** impls. That is the widening the addon
> depends on; the feature matrix here assumes it.

### 2.2 Does the addon need compio at all? — the crux of the matrix

Walk the residual I/O once `native-pg` and `zsv8` are off:

| Subsystem | Who does the I/O in the addon | Needs compio? |
| --- | --- | --- |
| PG apply | **Host** JS driver (`pg`), via TSFN callback | **No** — the generic `PostgresBackend<HostPgSession>` issues SQL strings; the *bytes* move in host JS. |
| MySQL apply | **Host** JS driver (`mysql2`), via TSFN — replaces the in-Rust V8+`node:net` MySQL isolate, which is `zsv8`-only and now OFF | **No** |
| Schema authoring / introspection | **Host** — Node evals `schema.js` and runs `pg`-based introspection; replaces the in-Rust V8 recorder (`zsv8`-only, OFF) | **No** |
| SQLite apply | **In-Rust** `rusqlite` (bundled), `std::thread` + `flume` actor — OR host-provided (see §2.3) | **No** (rusqlite path uses no compio) |
| The apply *loop* itself | `run_migrate` today wrapped in `compio Runtime::block_on` | **This is the only compio user left** — see §3. |

**Conclusion:** with `native-pg` OFF and `zsv8` OFF, **no data-plane I/O in the
addon uses compio**. The *only* residual compio dependence is that the core
today happens to `block_on` its async apply loop on a `compio::runtime::Runtime`
(`runner.rs:2482`) and declares `compio` as a **non-optional** dep. That is an
executor-provenance question, not an I/O question — resolved in §3. The
important packaging consequence:

> **io_uring is Linux-only.** If the addon linked compio for its executor, the
> `.node` would be Linux-only (or fall to compio's non-uring backends on other
> OSes, which is not a path this engine exercises or tests). A **host-only-driver
> addon that also drops the compio executor is cross-platform** (Linux/macOS/
> Windows), which is the whole point of shipping an npm addon. So the matrix
> below is chosen to **eliminate compio from the addon entirely.**

### 2.3 The SQLite provenance decision (native rusqlite vs host-provided)

This is the one genuine fork. Two options:

**Option A — SQLite stays NATIVE in the addon (rusqlite, bundled).**
The addon keeps the existing `sqlite/actor.rs` path: `rusqlite` with `bundled`
(pins the SQLite C source, compiled in) + `load_extension` (to *disable* it).
- **Cross-platform:** `rusqlite` + `bundled` compiles SQLite from source on
  Linux/macOS/Windows — it is the *reason* it's cross-platform. No io_uring, no
  compio, no host driver. ✔
- **Fidelity:** the SQLite apply path is **byte-for-byte the platform's** —
  same hardened, CDC-free, authorizer-mode-flip confinement the platform ships.
  Zero divergence risk between "addon SQLite" and "platform SQLite."
- **Cost:** the `.node` now embeds the SQLite C library (a C build-dep in the
  addon; needs a C toolchain in the addon's CI matrix, per-platform prebuilds).
  This is already true of the core today, so no *new* toolchain burden.
- **Consequence:** the addon has **one** native C dep (SQLite via rusqlite) and
  **zero** compio. Cross-platform, self-contained SQLite, host-provided PG/MySQL.

**Option B — SQLite ALSO host-provided (`bun:sqlite` / `better-sqlite3`).**
A third `HostSqliteSession` callback; the addon carries no `rusqlite`.
- **Pro:** the addon becomes a **pure-Rust, C-dep-free** cdylib — trivial
  prebuilds, no per-platform C toolchain, smallest binary. Under Bun, `bun:sqlite`
  is built-in and fast.
- **Con — fidelity risk:** the platform's SQLite confinement leans on
  *in-process* `rusqlite` specifics — `Connection::load_extension_disable()`, the
  `Arc<AtomicU8>` authorizer mode-flip, the SQLite 3.51.x version floor
  (`DEFENSIVE`/`TRUSTED_SCHEMA`/`RETURNING`, `authorizer zDb`-on-DROP_TABLE
  semantics for the journal-immutability proof). A host `bun:sqlite`/
  `better-sqlite3` provides a *different* SQLite build at a *different* version
  with **no way to install the authorizer** the immutability proof relies on.
  Host-SQLite would mean **re-proving journal immutability against a driver we
  don't control** — a security-relevant divergence, not just a perf one.
- **Con — two SQLite dialects to keep in parity** across addon-vs-platform.

**Recommendation: Option A — keep SQLite native (rusqlite bundled) in the
addon; host-provide only PG and MySQL.** Rationale:

1. **The confinement/immutability guarantees are the product.** They are
   in-process `rusqlite`-authorizer-shaped and cannot be reproduced over a host
   `bun:sqlite`/`better-sqlite3` callback. Host-SQLite trades the engine's core
   security property for a smaller binary — the wrong trade for this engine
   (`feedback_best_not_simplest`).
2. **rusqlite `bundled` is already cross-platform** — it is not the thing that
   makes the addon Linux-only. compio (io_uring) is. So keeping SQLite native
   costs *nothing* on the cross-platform axis; it only adds a C-toolchain step
   to the addon's prebuild CI, which we need anyway.
3. **PG and MySQL are exactly where host-provisioning pays off** — they are
   network drivers with mature, ubiquitous JS impls (`pg`, `mysql2`), and
   host-providing them is what lets us drop compio *and* the in-Rust V8 MySQL
   isolate in one move.

So the addon's driver map is: **PG = host (`pg`), MySQL = host (`mysql2`),
SQLite = native (`rusqlite` bundled).** Host-SQLite (Option B) stays a
**documented future option** for an explicitly "no-native-deps" build target if
one is ever needed, gated behind its own re-proof of journal immutability — not
the default.

### 2.4 Core Cargo tidy the addon forces

Two changes on the **core** crate (small, in-scope for when the sibling seam
lands — noted here so the build shape is complete):

1. **Make `compio` optional**, gated by `native-pg` (and by a future
   `native-executor` if we keep a compio `block_on` for the platform — see §3).
   Today `compio` is unconditional; a `host-pg` + host-executor addon must be
   able to build with **no compio in its closure** to stay cross-platform. The
   `compio Runtime::block_on` at `runner.rs:2482` moves behind that gate, and the
   addon supplies its own driver (§3).
2. **`SeamRow`/`SeamError` unconditional; `native-pg` converts into them.**
   (Sibling design; restated as a Cargo consequence: the read types must not be
   `native-pg`-gated, or `host-pg` can't return rows.)

### 2.5 Feature matrix summary

| Consumer | `zsv8` | `native-pg` | `host-pg` | compio in closure? | SQLite | V8 in closure? | Cross-platform? |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Platform (control/worker) | ON | ON | off | **yes** (native-pg + executor) | native rusqlite | yes | Linux (io_uring) |
| Lean embedder | off | on/off | — | native-pg-dependent | native rusqlite | no | native-pg-dependent |
| **`.node` addon (this design)** | **OFF** | **OFF** | **ON** | **NO** | **native rusqlite (Opt A)** | **NO** | **YES (Linux/macOS/Win)** |

---

## 3. The async/executor question — does the Rust core still need its own runtime?

### 3.1 The question

With all data-plane I/O host-provided (PG/MySQL over TSFN callbacks, SQLite in a
`flume` actor thread that blocks synchronously on `rusqlite`), does the Rust core
still need to spin its **own async runtime** (compio, or napi-rs's bundled tokio)
on a worker thread, or can the whole engine be driven purely by the N-API
threadsafe-function callbacks?

### 3.2 The Temporal model (the stated blueprint)

Temporal's TypeScript SDK worker is exactly this shape: a **Rust core (`sdk-core`)
that owns its own async runtime and state machines**, exposed to Node via a
napi-rs bridge. The Rust core does **not** borrow the JS event loop to run its
logic — it runs its workflow/activity **state machines** on its own tokio
runtime on Rust-owned worker threads, and uses N-API only at the **boundary**:
threadsafe functions to hand work to/receive completions from JS, and `#[napi]`
async methods that return Promises. The JS event loop and the Rust runtime are
**peers connected by a channel**, not one driving the other. This keeps the Rust
core's internal concurrency (timers, retries, the apply/rollback/backfill state
machine) independent of, and not blocking, the Node event loop.

### 3.3 What zeroship-migrate's "state machine" is

The engine is not a trivial request/response wrapper. Its apply flow is a
**multi-step state machine with its own control flow**: plan → guard → advisory
lock → BEGIN → per-op apply with confinement `SET`s → journal append →
two-phase recovery / rollback → backfill loop (cursor-batched, resumable) →
shadow-DB verification. `run_migrate` orchestrates this as an `async` function
today, `block_on`'d on a compio runtime. The steps **`.await` on driver I/O** —
which, in the addon, means awaiting a **host** PG/MySQL round-trip delivered
over a TSFN.

### 3.4 The two viable executor shapes for the addon

**Shape 1 — engine keeps its own runtime (Temporal model).**
The addon spins a **single-threaded async runtime on one Rust worker thread**
and `block_on`s `run_migrate` there. When the engine `.await`s a host DB call, it
`await`s a Rust `oneshot` future; the TSFN posts the SQL to JS, the JS driver
runs it on the Node event loop, and its `.then()` resolves the `oneshot` back on
the Rust side. The Node event loop stays free; the engine's internal state
machine runs on its own thread.
- The runtime here does **not** need to be compio — it does **no** io_uring I/O.
  It only needs to `block_on` a future whose leaves are `oneshot` channels. A
  minimal single-threaded executor (`futures::executor::LocalPool` /
  `block_on`, which is **already a workspace dep** — `futures = { workspace }`)
  is sufficient and **carries no compio, no tokio, no io_uring**. ✔ cross-platform.
- Alternatively napi-rs's own `tokio_rt` feature ("napi-rs provides a tokio
  runtime in an additional thread", per the napi-rs async docs) can host the
  `.await`s. But that pulls **tokio** into the addon — which violates the
  workspace's zero-tokio invariant even in a leaf. **Reject tokio;** use
  `futures::executor` (already present, invariant-clean).

**Shape 2 — no engine runtime; drive the state machine step-by-step from JS.**
Refactor `run_migrate` from one `async fn` into a **re-entrant step function**
the host calls in a loop: each call advances the machine until it needs a DB
round-trip, returns a "please run this SQL" instruction to JS, JS runs it and
calls back in with the result. This is a *sans-io* / driver-loop rewrite.
- **Pro:** literally zero Rust async runtime — the machine is a synchronous
  state transition; JS owns all scheduling.
- **Con:** it is a **large rewrite** of a working, security-audited async apply
  flow into a manual state machine (every `.await` becomes an explicit
  suspend/resume point + serialized continuation). High risk, re-audits the
  guard/journal/rollback ordering, and buys nothing the `oneshot`+`futures`
  approach doesn't already buy (the Node loop is free in both).

### 3.5 Recommendation

**Shape 1, with `futures::executor` (not compio, not tokio), on a single
Rust-owned worker thread — the Temporal model.**

- The engine **keeps its own runtime** because it *has its own state machine*
  with control flow (lock → apply → journal → rollback/backfill) that should not
  be interleaved into, or block, the Node event loop. This is precisely why
  Temporal's core keeps its runtime.
- The runtime is a **minimal single-threaded `futures` block_on**, whose I/O
  leaves are `oneshot` channels fed by TSFN completions — **no compio, no tokio,
  no io_uring** → cross-platform, zero-tokio-invariant-clean.
- **compio's role shrinks to `native-pg` only.** In the platform build, the
  compio runtime keeps driving the native compio-postgres driver (unchanged). In
  the addon build, the executor is the `futures` block_on and there is no compio.
  So `compio` becomes an **optional, `native-pg`-scoped** dep (§2.4-1), and the
  addon's closure is compio-free.

Concretely, the addon's `#[napi]` surface is roughly: an
`async fn apply(plan, drivers) -> Result<Report>` that, internally, hands the
work to the engine thread and awaits its completion (napi-rs turns the returned
future into a JS Promise, per the async-fn docs). The `drivers` argument carries
the host `PgSession`/`MysqlSession` **TSFN callbacks**; SQLite is served in-Rust.

---

## 4. N-API on Bun — `bun build --compile` caveats

The target is "Node **or** Bun embeddable." Bun implements Node-API, so a
napi-rs `.node` built for Node loads in Bun **without recompilation**
([Bun Node.js compatibility](https://bun.com/docs/runtime/nodejs-compat);
[napi-rs](https://napi-rs.github.io/napi-rs/)). But there are real caveats for
the **standalone `bun build --compile`** path and for the TSFN/event-loop
integration this addon leans on:

1. **Event-loop handle — use `napi_get_uv_event_loop`, never `uv_default_loop`.**
   Bun does not run on libuv on Linux/macOS. N-API addons that grab
   `uv_default_loop()` for `uv_async` **don't fire their callbacks** under Bun;
   the fix is to obtain the loop via `napi_get_uv_event_loop`. **napi-rs's
   `ThreadsafeFunction` already routes through the N-API TSFN primitives** (not
   raw `uv_default_loop`), so the recommended TSFN-based driver (Shape 1, §3) is
   the Bun-safe path *by construction*. Do **not** drop to raw
   `uv_async`/`uv_default_loop` anywhere in the addon.
   ([Bun native-addon compatibility notes, 2026](https://dev.to/alexcloudstar/bun-compatibility-in-2026-what-actually-works-what-does-not-and-when-to-switch-23eb).)

2. **`bun build --compile` embeds `.node` addons via `Bun__resolveEmbeddedNodeFile`
   / the `process.dlopen` path — but with sharp edges.**
   Bun's compiled-executable path *does* have a dedicated route for embedded
   `.node` files (the `process.dlopen` extraction path), distinct from the
   `bun:ffi` `type:"file"` path (which regressed in the Rust rewrite —
   [oven-sh/bun#30717](https://github.com/oven-sh/bun/issues/30717)). Mechanism:
   the addon is copied into a temp dir at startup and `dlopen`'d from there.
   Known **broken** case: **Windows** compiled executables have crashed loading
   `.node` addons (segfault at addon load; works under `bun run`, crashes under
   `bun build --compile`) — [oven-sh/bun#23771](https://github.com/oven-sh/bun/issues/23771).
   And **ABI mismatch**: mixing a Bun-loaded prebuild with `node`'s ABI
   surfaces as a native-module ABI error ([qmd#319](https://github.com/tobi/qmd/issues/319)).

3. **ABI / prebuild strategy.** Ship **per-platform prebuilt `.node`** binaries
   (the `@napi-rs/cli` npm layout: `zeroship-migrate-node-{platform}-{arch}`
   optionalDependencies + a loader that picks the right one). This is the
   ubiquitous napi-rs distribution shape and is what both Node and Bun's
   `dlopen`-based loader expect. Do **not** rely on building-from-source at
   install time (no C toolchain guarantee on user machines — and recall the addon
   *does* have one C dep, SQLite via rusqlite, §2.3).

4. **Recommended support posture (given the above):**
   - **Node (`.node` via `require`)** — primary, fully supported target.
   - **Bun (`bun run`, `.node` via `require`/`dlopen`)** — supported; use TSFN
     (already Bun-safe), ship napi prebuilds.
   - **`bun build --compile` standalone** — **supported on Linux/macOS, flagged
     as fragile on Windows** (cite #23771); gate it behind an explicit "compiled
     Bun binary" CI smoke test per-platform before claiming it. Provide the
     "extract-and-dlopen at runtime" fallback only if a compiled-embed bug bites.
   - **Deno / edge / Workers** — **out of scope** for the native addon (no
     stable N-API-in-compiled-binary story); those would need a WASM build, which
     is a separate, later design (WASM has no threads/io_uring and would force
     host-SQLite too — explicitly deferred).

---

## 5. Consolidated recommendation

1. **New leaf crate `crates/zeroship-migrate-node`** — a napi-rs `cdylib`
   (`napi`/`napi-derive` 3, `napi-build` 1, `build.rs` = `napi_build::setup()`),
   depending on `zeroship-migrate` with `default-features = false, features = ["host-pg"]`.
   Add it to the root workspace **`exclude`** so the platform's `cargo build`
   stays untouched; it ships as an independent npm artifact via `@napi-rs/cli`.
   The core crate is otherwise unmodified except for the sibling **`SeamRow`/`SeamError`**
   read widening (hard dependency) and making **`compio` optional/`native-pg`-scoped**.

2. **Feature profile: `zsv8` OFF, `native-pg` OFF, `host-pg` ON.** This leaves
   the addon closure **free of V8, compio-postgres, and — after §3 — compio
   entirely**, so it is **cross-platform** (io_uring is the only Linux-locked
   piece and it's gone). **PG = host (`pg`), MySQL = host (`mysql2`),
   SQLite = native `rusqlite` (bundled)** — keep SQLite native to preserve the
   in-process authorizer/immutability guarantees; host-SQLite is a deferred,
   re-proof-gated option only.

3. **Executor: the Temporal model — the engine keeps its own runtime**, but a
   **minimal single-threaded `futures::executor` `block_on`** (already a
   workspace dep; **not** compio, **not** tokio) on one Rust worker thread,
   whose I/O leaves are `oneshot`s resolved by TSFN completions. The Node/Bun
   event loop stays free; the zero-tokio invariant holds; compio is confined to
   the `native-pg` platform build.

4. **Bun: `.node` loads unmodified; TSFN is Bun-safe** (napi-rs routes through
   N-API TSFN, not `uv_default_loop`). `bun build --compile` embeds `.node`
   via the `process.dlopen` path but is **fragile on Windows** (#23771) — support
   Linux/macOS compiled binaries, smoke-test Windows before claiming it, ship
   per-platform napi prebuilds, and keep Deno/edge as a future WASM track.

---

## 6. Open items handed to sibling / later phases

- **`SeamRow`/`SeamError` read widening** — the hard prerequisite (sibling
  design). This build shape assumes read verbs return neutral types.
- **The `#[napi]` API surface** (the `apply`/`plan`/`introspect` exports, the
  `drivers` TSFN struct shape, error mapping Rust→JS) — a follow-on API-design
  phase.
- **Host schema-authoring bridge** — replacing the in-Rust V8 recorder with
  Node evaluating `sdks/migrate/` + `pg` introspection; the `zsv8`-OFF authoring
  path. Sketched here (§2.2) but not specified.
- **WASM build** for Deno/edge — explicitly deferred (forces host-SQLite, no
  threads).

---

### Sources

- napi-rs crate skeleton / `cdylib` / `napi_build::setup()` —
  [napi-rs README](https://github.com/napi-rs/napi-rs/blob/main/README.md),
  [napi-build README](https://github.com/napi-rs/napi-rs/blob/main/crates/build/README.md),
  [napi-rs site](https://napi-rs.github.io/napi-rs/)
- async-fn / `tokio_rt` bundled-runtime / ThreadsafeFunction error strategies —
  [napi-rs async-fn docs](https://napi.rs/docs/concepts/async-fn),
  [napi-rs ThreadsafeFunction docs](https://napi.rs/docs/concepts/threadsafe-function)
- Bun N-API compatibility & `napi_get_uv_event_loop` caveat —
  [Bun Node.js compat](https://bun.com/docs/runtime/nodejs-compat),
  [Bun compatibility 2026](https://dev.to/alexcloudstar/bun-compatibility-in-2026-what-actually-works-what-does-not-and-when-to-switch-23eb)
- `bun build --compile` + embedded `.node` / dlopen path & Windows breakage —
  [oven-sh/bun#23771](https://github.com/oven-sh/bun/issues/23771),
  [oven-sh/bun#30717](https://github.com/oven-sh/bun/issues/30717),
  [qmd#319 ABI mismatch](https://github.com/tobi/qmd/issues/319)
- Temporal core-owns-its-runtime model — Temporal TypeScript SDK worker
  (Rust `sdk-core` + napi bridge) architecture.
