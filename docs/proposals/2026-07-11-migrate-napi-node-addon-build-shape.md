# zeroship-migrate as a Node/Bun N-API addon: build, crate and platform shape

**Status.** SHIPPED. The addon is `crates/zeroship-migrate-node` (`src/`, `build.rs`,
committed `index.js` + `index.d.ts`, `package.json`, `__test__/`, `tests/`), and the root
`pnpm build` compiles it first in the chain. The driver seam it plugs into is
`crates/zeroship-migrate-backend/src/driver.rs` (`pub trait SqlSession`, line 71). Native
SQLite is `crates/zeroship-migrate-sqlite` (rusqlite, `bundled` + `load_extension`).
`packages/zero-migrate-cli` and `packages/vite-plugin` both consume it as
`zeroship-migrate-node: workspace:*`.

Three things the shipped crate settles differently from the shape first written here, and
this document specifies the shipped ones: the crate is `crate-type = ["cdylib", "rlib"]`
rather than `cdylib` alone; `napi-build` is version 2; and the engine's feature matrix does
not exist at all, because `zeroship-migrate` has no cargo features - `zsv8`, `native-pg` and
`host-pg` appear nowhere in the tree. V8 left the engine entirely rather than being gated off,
and compio is a dev-dependency of the engine crates rather than a gated one.

## What it is

A leaf `cdylib` crate wrapping the migration engine for Node and Bun, in which every
network driver is supplied by the JS host and the executor carries no reactor.

**The crate.** `crates/zeroship-migrate-node` is a full member of the root workspace via
`members = ["crates/*", "libs/*"]`. It depends on `zeroship-migrate` (the composition root)
and names the three shipping backends directly - `zeroship-migrate-postgres`,
`zeroship-migrate-mysql`, `zeroship-migrate-sqlite` - rather than reaching them through a
re-export. `build.rs` calls `napi_build::setup()`, which emits only cdylib link arguments
(allow-undefined for the Node ABI, resolved at `.node` load time) and needs no Node headers,
so it is safe under a plain cargo build. `build.rs` also folds
`ZERO_MIGRATE_SOURCE_DIGEST`, a sha256 over committed bytes only (crate manifests,
`Cargo.lock`, every `crates/*/src` file), which `buildInfo()` reports so a host that loaded a
`.node` by path can tell which sources produced it.

`crate-type` carries `rlib` alongside `cdylib` so the crate's own Rust integration tests can
link the library; a bare `cdylib` cannot be depended on by a test binary. The published
artifact is the `cdylib` `.node`.

**Features.** `default = ["napi"]`; `napi = ["dep:napi", "dep:napi-derive"]`. The feature
gates the N-API entrypoints (`bridge.rs`, `#[napi]` functions, `ThreadsafeFunction`,
`JsDeferred`). `--no-default-features` builds the pure-Rust core, the marshal and session
bridge, and the mock-apply integration test without the Node ABI.

A SECOND `napi` entry sits in `[dev-dependencies]`, adding `dyn-symbols`, so test binaries
resolve the Node ABI through libloading and have no undefined symbols to link. It is
declared on the dev-dependency and never routed through `[features]`, because any route
reachable from `default` would put libloading into the shipped `.node`
(`xtask/tests/repository_architecture.rs`, `tests/napi_symbol_shape_gate.sh`).

`napi` is declared `version = "3", default-features = false, features = ["napi6", "serde-json"]`.
`napi4` supplies the ThreadsafeFunction, `napi5` supplies `Env::create_function_from_closure`
and Deferred, `napi6` supplies the BigInt bindings so the exact-integer domain (verb row
counts, journal `event_seq`) crosses as a JS `bigint`. `serde-json` lets a typed verb envelope
carry the IR ops AST and a lowered `Migration` as a real JS value, so every verb
request/response is a typed `#[napi(object)]` in `wire.rs`. Node and Bun both implement
N-API 6 or better.

**Drivers.** PostgreSQL and MySQL are host-provided; SQLite is native and in-process.
`src/session.rs` defines `NapiHostSession<D: VerbDispatch>`, which implements `SqlSession` by
marshaling each verb (`batch`, `execute`, `query`, `queryOne`) to a host verb dispatcher and
awaiting the reply on a `futures::channel::oneshot`. Two dispatchers exist: the napi transport
(`bridge.rs::TsfnDispatch`, `napi` feature) fires a `ThreadsafeFunction` at the JS driver and
resolves the oneshot from a Rust-supplied `done` callback; a mock dispatcher returns canned
rows synchronously and asserts the recorded SQL sequence, proving the bridge drives a real
apply with no Node host present. Only `Send + 'static` owned data crosses the boundary: a
`JsRequest` out, a `Result<Vec<JsRow>, JsError>` back.

`NapiHostSession` carries a one-in-flight `AtomicBool` guard. Each verb
`compare_exchange(false, true)`s on entry and clears on the completion arm, panicking on
re-entry.

**Executor.** `src/runtime.rs` runs the engine on a dedicated `std::thread` under
`futures::executor::block_on` - no io_uring, no tokio, no reactor. `block_on` parks the worker
on a thread-parking waker; a `Sender::send` from the JS thread unparks it and the awaited
receiver resolves. That is sufficient because every wakeup the engine future sees comes from
an out-of-thread send. `run_engine_blocking` is the reusable primitive: give it a future
factory and it runs `block_on` on a fresh worker thread, delivering the result to a completion
callback. The napi entrypoints wrap it with a `JsDeferred` so JS gets a Promise resolved
cross-thread.

That worker thread is also where engine diagnostics are collected. The engine emits `tracing`
events for secondary failures its reply cannot carry (a release that failed, a `RESET ROLE`
that failed); `with_diagnostics` installs a subscriber for the length of the verb, on the
thread running it, when the host has opted in through `set_diagnostics`. The switch value is
passed in from the host rather than read from the process environment.

**Entrypoints.** Sync, DB-free entrypoints (`plan`, `generate` / load-verify) run inline on
the napi call thread with no bridge. Async, host-driven entrypoints (`apply`, `status`,
`history`) go through `run_engine_blocking` and bottom out in the host driver over a
ThreadsafeFunction. There is no `#[napi] async fn`, no `Promise::await`, no `tokio_rt`.
`dry_run` is not surfaced: no shadow harness exists for the host-driven backends, so a shadow
dry-run returns `DryRunError::ShadowUnsupported`.

**Distribution.** `@napi-rs/cli` (`napi build --platform --release`) compiles the cdylib into a
platform `.node` plus a generated `index.d.ts`. `package.json` declares `binaryName`
`zeroship-migrate-node` and five targets: `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`,
`x86_64-pc-windows-msvc`. The generated `index.js` loader resolves the right prebuild and is
committed; `.node` binaries are gitignored. `engines.node` is `>=18`. `npm test` drives the
real `.node` through `__test__/*.mjs`; `tests/napi_export_parity_gate.sh` checks the committed
`index.d.ts` and `index.js` against each other.

## Why it is this way

**The engine must stay a plain rlib.** The core is embedded by the platform's own binaries.
Putting `napi`/`napi-derive` or a `cdylib` crate-type on `zeroship-migrate` would drag N-API
into every embedder's dependency closure and force a lib+cdylib dual crate-type on the core.
A `cdylib` is terminal - no Rust crate can depend on one - so the addon is necessarily a leaf
and `zeroship-migrate-node -> zeroship-migrate` stays the only edge.

**io_uring is the only thing that could make the `.node` Linux-only, and it is absent.**
Nothing in the addon's closure links compio: the engine crates declare it under
`[dev-dependencies]` for their async test harnesses only, and `zeroship-migrate-core` links
neither tokio nor compio. rusqlite with `bundled` compiles SQLite from source on Linux, macOS
and Windows, so keeping SQLite native costs nothing on the cross-platform axis.

**Zero tokio holds in the leaf.** napi's ergonomic async (`async`, `tokio_rt`) is a tokio
runtime on an extra thread. `default-features = false` drops it and the executor is
`futures::executor` instead.

**SQLite confinement is in-process or it is not a guarantee.** The engine's hardening rests on
rusqlite specifics: `Connection::load_extension_disable()`, the `Arc<AtomicU8>` authorizer
mode flip, and the SQLite version floor the journal-immutability proof reads
(`DEFENSIVE`/`TRUSTED_SCHEMA`/`RETURNING`, authorizer `zDb`-on-`DROP_TABLE` semantics). A host
`bun:sqlite` or `better-sqlite3` is a different SQLite build at a different version with no way
to install that authorizer.

**The engine owns its own control flow.** Apply is a multi-step state machine - plan, guard,
advisory lock, BEGIN, per-op apply with confinement SETs, journal append, two-phase recovery
and rollback, cursor-batched resumable backfill, shadow verification - whose steps await
driver I/O. It runs on its own thread so that machine neither interleaves into nor blocks the
Node event loop.

**Core may not name a vendor.** `core_names_no_vendor_crate` forbids the engine from naming a
vendor crate outside its registry, so `zeroship-migrate` re-exports no backend. The addon
names `zeroship-migrate-postgres`, `-mysql` and `-sqlite` itself. Any new host must do the
same rather than asking for a re-export.

**`unsafe` is scoped to this crate alone.** The workspace pins `unsafe_code = "deny"`, correct
for the pure-Rust engine crates. The napi bridge FFIs into the Node ABI
(`ToNapiValue`/`FromNapiValue` raw conversions) are `unsafe fn` by contract, so
`#![allow(unsafe_code)]` sits on this crate and nowhere else.

**The source digest is deliberately narrow, and the gap it leaves is a hole rather than a
handoff.** It covers committed bytes and nothing else - no wall clock, no absolute path, no
git state, no hostname - so rebuilding an unchanged tree yields the same value. It therefore
does not distinguish two artifacts differing only in toolchain, cargo profile, or enabled
features, and no other field of `BuildInfo` does either. It also hashes bytes as checked out,
so a CRLF checkout digests differently from an LF one.

## Open

1. **Nothing produces the declared prebuilds.** `package.json` names five napi targets, but
   `.github/workflows/ci.yml` contains zero occurrences of `napi` and builds the addon only
   through `pnpm build` on the CI host's own platform. Decide who cross-compiles and publishes
   the five per-platform packages, and whether the release is npm optionalDependencies under
   the standard `@napi-rs/cli` layout.
2. **Bun support is claimed and exercised by nothing.** `src/lib.rs` opens "the Node/Bun N-API
   addon" and the transport is Bun-safe by construction, but no test in the tree runs under
   `bun run` or `bun build --compile`. Decide whether to add a per-platform Bun smoke test or
   to stop claiming Bun until one exists. Treat the `bun build --compile` story on Windows as
   UNVERIFIED in either case.
3. **The loader carries a wasm32-wasi branch nothing builds.** The napi-rs generated
   `index.js` falls back to `./zeroship-migrate-node.wasi.cjs` and
   `zeroship-migrate-node-wasm32-wasi`, and `__test__/force_wasi.mjs` exercises that fallback,
   but `package.json` declares no wasi target. Decide whether to ship a wasi artifact or to
   accept the branch as permanently unreachable in this package.
4. ~~**The addon's Cargo.toml describes a workspace arrangement the root does not have.**~~
   **RESOLVED 2026-09-04, by the third option.** The finding was correct: the header claimed
   the root kept the addon out of the default build and test flow via `default-members`, and
   the root `Cargo.toml` has no such key (`members = ["crates/*", "libs/*"]`, `resolver = "3"`,
   an `exclude` list of three unrelated paths). The addon was a default member all along, so a
   workspace-wide `cargo test` did reach a target that could not link - exit 101, 1719
   `undefined reference` lines.

   Of the three options offered - add `default-members`, flip the `napi` default off, or
   correct the comment - NONE was taken as written, because each of the first two removes
   coverage. What shipped is a fourth: the addon stays a default member with `napi` ON, and a
   `napi` entry in `[dev-dependencies]` carrying `dyn-symbols` makes its test binaries link.
   `cargo test --workspace --no-run` now exits 0 with no `--exclude`, and `src/bridge.rs` is
   type-checked by `cargo check --workspace` for the first time. The false comment is deleted
   and recorded as false in the manifest itself.
5. **No host-side shadow harness exists**, so `dry_run` is unavailable over the host-driven
   backends. Decide whether to supply one or to keep `ShadowUnsupported` as the permanent
   answer for this host.
6. **Build identity does not separate toolchain, profile or feature set** (see the last
   paragraph of the previous section). Decide whether `BuildInfo` grows fields for them.

## History

The deliberation - the executor comparison, the rejected sans-io rewrite of the apply loop,
the host-SQLite option and the original feature-matrix framing that the engine's feature
removal made moot - lives in this file's git history.

Do-not notes, each recording something that was tried or measured:

- ~~Do not expect a bare `cargo test -p zeroship-migrate-node` to work.~~ **CORRECTED
  2026-09-04: the bare command works, with `napi` ON.** This bullet was accurate when
  written and stayed accurate until the fix below, so the link failure it describes is
  real history: exit 101, 1719 `undefined reference` lines, the first
  `napi_create_function` from `src/bridge.rs:1222` in `_napi_rs_internal_register_status`.
  What it got wrong was the conclusion that no test binary could ever link. The crate now
  declares `napi` in `[dev-dependencies]` with the `dyn-symbols` feature, which swaps
  napi-sys's extern block for a libloading-populated pointer table - the same code that
  already ships on `x86_64-pc-windows-msvc`. Under resolver v3 that reaches test targets
  and NOT the shipped `--lib` build, so the `.node` is byte-for-byte unchanged.
  `--no-default-features` still builds the napi-free core but is no longer required and no
  longer the recommended path; it type-checks 1486 fewer lines. `tests/napi_symbol_shape_gate.sh`
  holds both halves. The boundary itself is still `npm test`'s question, through the real
  `.node`: no Rust test may CALL an N-API function, because with no host loaded the
  napi-sys stub returns a value that can read as success.
- Do not enable napi's `async` or `tokio_rt` features. They install a tokio runtime on an
  extra thread and break the workspace zero-tokio invariant inside the leaf.
- Do not `join()` the engine worker thread from the JS thread. It deadlocks libuv and Bun.
  Resolve the `JsDeferred` cross-thread instead, fire-and-resolve.
- Do not remove the one-in-flight `AtomicBool` guard in `NapiHostSession`. On a real pinned
  host connection a second concurrent verb blocks on a socket the first has not released; the
  guard converts that deadlock into a loud panic.
- Do not reach for `uv_default_loop` or raw `uv_async` anywhere in the addon. Bun does not run
  on libuv on Linux or macOS, and callbacks registered on the default loop never fire there.
  napi-rs's `ThreadsafeFunction` routes through the N-API primitives and is safe by
  construction; dropping below it gives that up.
- Do not replace `tests/napi_export_parity_gate.sh` with a regeneration step. The `baseline`
  verb shipped in `index.d.ts` and not in `index.js` on 2026-08-28: it compiled, it
  type-checked (TypeScript reads the `.d.ts` and never `index.js`), and it tested green
  because the authoring worktree held a built artifact the commit did not carry. Regenerating
  on build would have fixed the local tree and left the commit just as wrong. The gate rules
  on committed bytes.
- Do not host-provide SQLite. The confinement and journal-immutability proofs are
  in-process-rusqlite shaped and cannot be reproduced over a `bun:sqlite` or
  `better-sqlite3` callback; doing it would require re-proving journal immutability against a
  driver we do not control.
- Do not rely on building from source at install time. The addon has a C dependency (SQLite
  via rusqlite `bundled`) and user machines carry no C toolchain guarantee.
- Do not read configuration from the process environment inside the addon. `ZERO_MIGRATE_LOG`
  was read here and declared nowhere else in the tree, so any process carrying it turned on
  stderr diagnostics inside callers - the Vite plugin among them - that never asked. The host
  passes the value across the boundary instead.
