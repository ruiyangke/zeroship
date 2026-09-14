# The standalone Node/Bun napi shell for the migration engine

**Status.** SHIPPED, except the shadow dry-run.

The addon is `crates/zeroship-migrate-node/` (a napi-rs `cdylib` plus a checked-in
`zeroship-migrate-node.linux-x64-gnu.node`). The driver seam it drives the engine
through is `crates/zeroship-migrate-backend/src/driver.rs`. The JS host runtime, the
CLI and the `pg`/`mysql2` driver adapters are `packages/zero-migrate-cli/`; the
authoring DSL is `packages/zero-migrate/`, published as `@zeroship/migrate`. The root
`pnpm build` compiles the addon as its first filter, so a clean checkout gets it with
no separate step (see the Development section of `AGENTS.md`).

The one part of this design that did not ship is the host-side shadow dry-run.
`ShadowDryRun` is declared and has **zero implementors anywhere in the workspace**, so
every `dry_run` refuses with `DryRunError::ShadowUnsupported`; that fact is held in
place by `crates/zeroship-migrate/tests/dialect_matrix/shadow_dry_run_has_no_implementor.rs`.
See Open 1.

## What it is

The migration engine ships as a Node/Bun-loadable N-API addon: one npm install, no
Rust toolchain, no V8 embedded in Rust, no io_uring. The Rust core stays V8-free and
runs its own executor on a worker thread; the JS host supplies the other half, namely
migration authoring (the TypeScript DSL) and the database drivers.

**The seam.** `zeroship_migrate::driver::SqlSession` is the one injected runtime
dependency the network backends (`PostgresBackend`, `MysqlBackend`) are generic over.
It has exactly four verbs:

```rust
async fn batch(&self, sql: &str) -> Result<(), DbError>;                       // DDL, txn control, session setup
async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError>;       // parameterized DML -> rows affected
async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError>;
async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError>;
```

Every type in those signatures is driver-neutral. `Bind` is
`Null | Bool | Int(i64) | Decimal(String) | Text(String) | Inferred(Option<String>)`.
`Value` (the decoded cell) is `Null | Text | Int(i64) | Bool | Decimal(String) |
TextArray(Vec<Option<String>>)`, text-biased to match SQL the apply path already
emits: timestamps are `to_char`-cast to text, counts are `bigint`, most arrays are
`array_agg`. `Row` carries `len()` and `try_get` (no panicking `get`), resolving
through a closed `FromValue` trait with eleven impls: `String`, `Option<String>`,
`bool`, `i64`, `Option<i64>`, `i32`, `Option<i32>`, `i8`, `char`, `Vec<String>`,
`Option<Vec<String>>`. `DbError` is opaque: a message plus an optional SQLSTATE, which
is how every seam consumer treats it (wrap as `#[source]`).

`Bind::Inferred` sends a parameter with **no declared type** so the server infers one
from context. That is the `text` to `timestamptz` coercion path the DML executor
depends on; `Inferred(None)` is a SQL NULL. A driver that declares parameter types
maps it to its unspecified-type text spelling; node-pg and mysql2 never declare types,
so it is indistinguishable from `Bind::Text` there.

SQLite does **not** ride this seam. It is an in-process `rusqlite` actor
(`zeroship_migrate_sqlite::backend`) reached over channels, with no session object and
no host callback.

`driver/conformance.rs` is the seam's conformance suite, the first consumer beyond the
engine itself: a driver author runs `conformance::run` against a live empty session and
gets one pass/fail verdict over session pinning, transaction visibility, bind-inference
semantics, and error/SQLSTATE mapping. PostgreSQL and MySQL each supply a `SeamFixture`
with their own spellings and both run it live.

**The bridge.** `crates/zeroship-migrate-node/src/session.rs` defines
`NapiHostSession<D: VerbDispatch>`, which implements `SqlSession` by marshaling each
verb to a host dispatcher and awaiting the reply on a `futures::channel::oneshot`. Two
dispatchers exist: `TsfnDispatch` (the napi transport) and a mock that answers inline,
so a full apply is provable without a Node host.

On the napi path each verb allocates a oneshot, moves the `Sender` into the
threadsafe-function payload, and calls the TSFN in Blocking mode. On the JS thread the
callback marshals the request, builds a per-call `done(err, reply)` JS function
capturing the `Sender`, and invokes the host driver. napi-rs delivers the pair as a
single JS array argument, so **the host-driver contract is
`hostDriver([request, done]) => void`**. The host does its real async work and calls
`done`, whose Rust body fires the `Sender` and wakes the parked worker.

`NapiHostSession` carries a one-in-flight `AtomicBool`: each verb
`compare_exchange(false, true)`s on entry and clears on an RAII guard, panicking on
re-entry. On a real pinned connection a second concurrent verb would deadlock; the
guard turns that into a loud panic instead.

**The executor.** `crates/zeroship-migrate-node/src/runtime.rs` gives
`run_engine_blocking`: a single `futures::executor::block_on` on a fresh `std::thread`
per verb, with no I/O reactor. It works because every I/O leaf is a channel receiver
woken out of thread, and `std::thread::Thread::unpark` is cross-thread-safe. The napi
entrypoints wrap it with a `JsDeferred`, so JS gets a promise resolved cross-thread when
`block_on` completes. The JS thread is **never** joined on the worker.

**The crate.** `zeroship-migrate-node` is a leaf: `crate-type = ["cdylib", "rlib"]`
(the `rlib` exists only so the crate's own integration tests can link it). It depends
on `zeroship-migrate` plus the three vendor backends by name
(`zeroship-migrate-postgres`, `-mysql`, `-sqlite`), because the engine no longer
re-exports vendors. `napi` 3 is declared `default-features = false` with `napi6` and
`serde-json` only. The `napi` cargo feature is on by default and gates the Node ABI
entrypoints. `--no-default-features` builds the crate without them; it was the ONLY
configuration that built until 2026-09-04, and is no longer required. A second `napi`
entry in `[dev-dependencies]` adds `dyn-symbols`, so a bare
`cargo test -p zeroship-migrate-node` links and runs with `napi` ON while the shipped
cdylib keeps resolving the Node ABI from its host
(`tests/napi_symbol_shape_gate.sh`).

**The napi surface** is generated from the Rust exports. The generated CommonJS
loader assigns `nativeBinding` directly to `module.exports`, while the same napi
build emits `index.d.ts`. DB-free verbs run inline, host-driven database verbs take
a driver callback and return a promise, and SQLite verbs use the bundled in-process
rusqlite backend. The package's Node tests load and call the real addon.

**The JS half.** `packages/zero-migrate-cli` loads the addon (`src/addon.ts`), ships
`driver-pg.ts` and `driver-mysql2.ts`, and exposes the facade: `apply`, `rollback`,
`resolvePending`, `plan`, `validate`, `status`, `statusEnvelopes`, `history`,
`baseline`, `previewSql`. Authoring is pure JS through `@zeroship/migrate`'s
`./internal/recorder` export; the addon then stamps `owner_app` and folds the single
authoritative checksum, and `irVersion()` is the one source of the IR version across
the boundary.

`driver-pg.ts` constructs its `pg.Client` with its **own** connection-scoped `types`
object whose `getTypeParser` forces oid 20 (int8), 1700 (numeric) and 1016 (int8[]) to
verbatim strings and decodes the boolean/text-array oids itself, consulting `pg.types`
for none of them. Distribution is napi-rs prebuilds for five targets:
linux x64/arm64 gnu, darwin x64/arm64, win32 x64. SQLite is `rusqlite` bundled, so
there is no system libsqlite dependency.

The platform's embedded path produces the same seam from the other side:
`CompioPgSession` in `crates/zeroship-migrate-server/src/session.rs` is a newtype over
a compio-postgres connection implementing `SqlSession`. One engine, two producers.

## Why it is this way

**Zero tokio, structurally.** napi-rs's ergonomic async (`#[napi] async fn`,
`Promise::await`) is gated behind the `async`/`tokio_rt` features, which bundle a tokio
runtime. Neither may ever be used here. The completion-callback-to-oneshot bridge
exists precisely so the addon never returns a JS promise for Rust to await.

**A reactor-less `block_on` is sufficient only while the engine future is strictly
sequential.** No `join!`, no `select!`, no timers, no intra-engine spawn: one verb
issued, awaited, then the next. The one-in-flight guard is what keeps that property
mechanically checked rather than assumed. If concurrency is ever introduced, the escape
hatch is a single-threaded `LocalPool` (still reactor-free), not a reactor.

**No compio in the addon.** io_uring is Linux-only; eliminating compio is what makes
the `.node` cross-platform. Postgres and MySQL are host-provided, SQLite is native
rusqlite.

**Both sides of the seam must be neutral, not just the return side.** A concrete
driver's row, error and bind types have private constructors and serialize binds to a
wire format, so a host driver typed against them could neither be *called* (it cannot
extract cells from opaque bind params) nor *return* rows. That is why `Bind` exists
alongside `Value`/`Row`/`DbError`.

**Drivers must not fork behaviour.** Cross-driver interchangeability is the whole
point, so where a choice exists both adapters make the same one. An element-NULL inside
a `text[]` errors in `FromValue for Vec<String>` on every driver, exactly as
`FromSql` does, so the caller's `.unwrap_or_default()` yields `[]` identically; a
`Option<Vec<String>>` read yields `None` identically. A future decision to fail loudly
there must be applied to every adapter at once.

**SQLite stays native.** The journal-immutability and confinement guarantees are
in-process rusqlite-authorizer-shaped. A host `bun:sqlite` or `better-sqlite3` cannot
install that authorizer, so host-provided SQLite would trade the engine's core security
property for a smaller binary.

**Provenance is stamped in Rust, never in JS.** Untrusted `up()` must not be able to
set `owner_app`, and the checksum is folded once, by the core, over the canonical op
list. That is what makes the host recorder and the embedded recorder interchangeable.

**Exact integers survive the boundary.** int8 and numeric cross as strings. Journal
`event_seq`/`version` comparisons depend on it.

**The addon drops the in-Rust V8 sandbox, and that is a posture decision.** Pure-JS
authoring loses the resource budget, wall watchdog and no-fs/no-net confinement the
recorder isolate gave untrusted `up()`. That is acceptable for the trusted
local/self-host posture the addon targets. A multi-tenant hosted recorder would have to
re-establish sandboxing host-side (worker plus `vm` plus timeout) or keep an in-Rust
recorder for that posture alone.

## Open

1. **Shadow dry-run.** The engine's headline pre-apply check does not exist on any
   path: nothing implements `ShadowDryRun`, so `dry_run` and `dry_run_declarative`
   always return `ShadowUnsupported`. Building it needs a decision on connection
   topology, because the natural host shape splits `CREATEDB`/`CREATEROLE` off the
   migrator connection onto a separate admin DSN rather than fusing them. That split
   is a least-privilege improvement but it is a behaviour change: a caller with no
   admin DSN gets `ShadowUnsupported` where a single privileged connection would have
   succeeded, and seed-source and shadow-target must be proven to share one cluster or
   the verdict is silently wrong. Decide whether the addon takes the split, and whether
   a shadow whose seed omits triggers, functions, sequences, column DEFAULTs and row
   data is worth shipping at all.
2. **Bun.** The addon is described as a Node/Bun addon and the code is written for it
   (the fire-and-resolve topology exists so the JS thread is never parked), but there is
   **no Bun gate in `tests/`** and no Bun run proves TSFN Blocking-mode ordering or
   `done` delivery match Node. Decide whether Bun is supported (then add a full-apply
   parity run under `bun run` asserting journal identity with Node) or unsupported
   (then say so in the package metadata). UNVERIFIED either way today.
3. **A hung host driver.** If `done` never fires and its closure stays alive, the
   oneshot resolves neither to a value nor to `Canceled`, and the worker parks
   indefinitely. There is no watchdog or heartbeat in the addon today. If one is added
   it must be a liveness check, not a wall-clock cap: a single legitimate `CREATE
   INDEX` can run for minutes while the driver is perfectly healthy.
4. ~~**Workspace isolation of the addon.**~~ **RESOLVED 2026-09-04: there was no isolation
   to restore, and none is wanted.** The observation was right - the manifest claimed the
   root kept the addon out of the default build via `default-members`, and the root declares
   no such key - so a plain `cargo build` did pull the addon, and a plain `cargo test`
   selected a package that failed at link. Neither remedy offered here was taken: adding the
   key or flipping the `napi` default would each have dropped `src/bridge.rs` out of the
   default workspace check. Instead the addon remains a default member with `napi` ON and
   links its test binaries through `napi/dyn-symbols` on the dev-dependency. The false
   comment is deleted and recorded as false in the manifest.

## History

The deliberation lives in this file's git history and in the two sibling designs of the
same date, `2026-07-11-migrate-napi-host-boundary.md` (the JS-host boundary) and
`2026-07-11-migrate-napi-node-addon-build-shape.md` (the crate and platform shape). Note
that the shipped names differ from every one of those documents: the seam is
`SqlSession` with `Bind`/`Value`/`Row`/`DbError`, not `PgSession` with `Seam*` types;
the `zsv8`/`native-pg`/`host-pg` feature matrix was replaced by the crate split; and
`execute_text_params` was replaced by the `Bind::Inferred` variant on the ordinary
verbs.

- **Do not declare a parameter's type for a value the IR carries as a string.**
  PostgreSQL refuses to assign a parameter declared as `text` into a `timestamptz`
  column and accepts the identical bytes when nothing is declared. `Bind::Inferred` is
  the difference between a statement running and failing, not a formatting preference.
- **Do not let the host `pg` driver inherit its type parsers from `pg.types`.** That map
  is global and mutable; a host app that has overridden the int8 parser to return a JS
  number truncates large bigints below the seam and corrupts journal `event_seq`
  comparisons with no error. A pin that borrows its parser from `pg.types` is not a pin,
  it only moves which oid has to be poisoned.
- **Do not `join()` the engine worker thread from the JS thread.** It deadlocks
  libuv and Bun: the host-driver TSFN callback cannot run while the JS thread is parked
  inside the napi call. Fire and resolve a deferred instead.
- **Do not drop `catch_unwind` from a napi export.** Without it a panic unwinds out of
  the generated `extern "C"` shim and aborts the whole Node process, measured as
  `fatal runtime error: failed to initiate panic, error 5, aborting` and a core dump,
  with no JS stack and nothing for a caller to catch. napi-rs applies its own only on
  that opt-in.
- **Do not use `uv_default_loop`; use `napi_get_uv_event_loop`.** Bun is not on libuv on
  Linux or macOS, and `uv_default_loop`-based async callbacks silently never fire.
  (Carried from the original design as a caveat about Bun; not exercised by any gate
  here, so treat it as guidance rather than a measured property of this tree.)
- **Do not read the MySQL in-V8 driver as evidence the napi bridge works.** It proves
  the protocol shape (request/reply marshaling, one pinned connection, SQLSTATE-carrying
  errors) and nothing about the cross-OS-thread hand-off or the reactor-less wakeup,
  because it runs inside an embedded isolate on the same thread.
- **Do not add an `impl ShadowDryRun` and stop there.** The capability is a parameter to
  `MigrationEngine::dry_run`, so the engine keeps refusing every dry-run until a caller
  passes the harness in. The no-implementor test goes red on the `impl` alone, which is
  deliberate: it forces the author to wire the harness into the call path.
- **Do not read a configuration value out of the process environment inside the addon.**
  The diagnostics switch was `std::env::var` here once; the value it read appeared in
  that one file and nowhere else in the tree, and a process that happened to carry it
  turned on stderr logging inside callers that never asked. The host states it across
  the boundary instead.
