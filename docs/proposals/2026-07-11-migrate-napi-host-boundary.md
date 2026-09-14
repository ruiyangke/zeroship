# The JS host boundary for the napi migration engine

**Status.** SHIPPED. This document was headed "design-only" and the design is built.
The V8-free Rust core ships as an N-API addon at `crates/zeroship-migrate-node`
(a `cdylib` the root `pnpm build` compiles first, before the SDKs that consume it).
The driver seam is `trait SqlSession` in
`crates/zeroship-migrate-backend/src/driver.rs`. The JS host is two packages:
`packages/zero-migrate` (published as `@zeroship/migrate`, the DSL and the pure-JS
recorder) and `packages/zero-migrate-cli` (the addon loader, the `pg` and `mysql2`
drivers, and the `zero-migrate` binary). `deploy/ops/db-migrate.sh` applies the
platform's own migration corpus through that CLI.

## What it is

The engine core is Rust and knows nothing about JavaScript. Everything that used to
need an embedded V8 moved host-side: authoring runs in the host's own JS engine, and
the network drivers are host modules the core calls back into over N-API. The addon
carries no compio, no io_uring, no V8 and no tokio, so the `.node` is cross-platform.

**Authoring.** The recorder is `packages/zero-migrate/src/internal/recorder.ts`,
reachable through the DSL package's one sanctioned subpath export
`./internal/recorder`. Given an already-imported migration module it runs a single
forward phase (`schema()` for DDL or `data()` for DML) under a fresh ambient
recorder, drains the op list, and returns the envelope
`{ ir_version, name, ops, inverse_ops?, irreversible? }`. A reversible data
migration's `inverse()` is recorded in a second, independent pass; an irreversible
one carries its authored reason instead. The envelope deliberately carries no
`owner_app` and no checksum: the addon stamps `owner_app` from the host-supplied app
id and folds the single authoritative `Checksum::of_ir`
(`crates/zeroship-migrate-ir/src/migration.rs`). `ir_version` is read back from the
addon's `irVersion()` rather than hardcoded in JS; the Rust constant
`CURRENT_IR_VERSION` (`crates/zeroship-migrate-ir/src/ir.rs`) is the authority and is
currently 1.

The schema-descriptor path is the same shape in the other direction.
`crates/zeroship-migrate-node/src/descriptors.rs` mirrors `CollectionDescriptor`
both ways: inbound, a declared schema becomes `createTable` ops (the `genArtifacts`
verb); outbound, the folded schema is handed back as typed collections so a host can
render its own files. The `TypeBuilder` the descriptors are authored with lives in
`packages/zero-migrate/src/db-types.ts`, inside the DSL package itself.

**The driver seam.** `SqlSession` has exactly four in-session verbs: `batch` (DDL,
transaction control, multi-statement session setup; one string, no params, no rows),
`exec` (parameterized DML returning rows affected), `query`, and `query_one`. Every
signature is typed in the driver-neutral `Bind` / `Value` / `Row` / `DbError`, never
in a concrete driver's types. Transaction control, advisory locks and confinement
`SET`s are SQL strings issued through those verbs by each dialect's backend, so the
seam carries no transaction object and no lock abstraction.

**The transport.** The JS host registers one function with the addon,
`hostDriver([request, done]) => void`. napi delivers the pair as a single array
argument. A verb crosses as the typed `#[napi(object)]` DTOs in
`crates/zeroship-migrate-node/src/wire.rs`: `JsRequest { kind, sql, binds }` out,
`JsReply { rows, rowCount }` or `JsError { message, code }` back, with cells as
`JsCell` and rows as parallel column-name and cell vectors in `JsRow`. The engine
future runs on a dedicated `std::thread` under `futures::executor::block_on`
(`crates/zeroship-migrate-node/src/runtime.rs`); the host's `done` callback fires a
`futures` oneshot whose send unparks that thread. No JS Promise is awaited in Rust,
no reactor exists, and the JS thread is never blocked on a join.
`NapiHostSession` (`crates/zeroship-migrate-node/src/session.rs`) implements
`SqlSession` over that transport behind a `VerbDispatch` trait, so an in-process mock
dispatcher can drive a real apply with no Node host, and it carries a one-verb-in-flight
`AtomicBool` that panics on re-entry.

**Drivers.** PostgreSQL and MySQL are host-provided:
`packages/zero-migrate-cli/src/driver-pg.ts` and `driver-mysql2.ts`, each binding one
pinned client to the session for its lifetime, never a pool. SQLite is native and
does not ride the seam at all: a bundled `rusqlite` in-process actor in
`crates/zeroship-migrate-sqlite`, reached through separate path-taking addon verbs
(`applyIrSqlite`, `rollbackSqlite`, `statusIrSqlite`).

**The host API.** `packages/zero-migrate-cli/src/index.ts` exports `apply`,
`rollback`, `resolvePending`, `baseline`, `status`, `statusEnvelopes`, `history`,
plus the DB-free `plan`, `validate` and `previewSql`. `apply` authors the envelope in
pure JS, opens the driver session, and hands the envelope and the `hostDriver`
callback to `applyIr`; for SQLite it sends the complete ordered envelope sequence to
`applyIrSqlite` instead. Network sessions are closed on success and on throw.

**Other seam producers.** The compio path is a peer implementation, not a special
case: `CompioPgSession` in `crates/zeroship-migrate-server/src/session.rs` implements
the same trait for the in-repo Linux server build. Each vendor crate has a
`RecordingSession` that proves its SQL without a database, and
`crates/zeroship-migrate-backend/src/driver/conformance.rs` is a live-session suite a
driver author runs to prove session pinning, transaction visibility, bind inference
and SQLSTATE mapping.

**No hosted recorder exists, and none is needed.** The managed service
(`crates/zeroship-migrate-server`) ingests already-recorded `.ir.json` envelopes over
HTTP and applies them over `CompioPgSession`. Its library surface never evaluates
creator JavaScript; V8 appears only in its `[dev-dependencies]`, as an authoring
proof. Recording is therefore a local, trusted-posture activity, and the resource
budget and watchdog the old in-Rust V8 recorder provided are not load-bearing for any
shipped path.

## Why it is this way

Checksum and provenance are Rust-owned. JS emits ops; Rust stamps `owner_app` and
folds `Checksum::of_ir`. Moving either into JS would let authoring code influence a
tenant-identifying field or the integrity value derived from it.

The seam types must be constructible by a stranger. A concrete driver's row and error
types have private constructors, so a host driver could neither be handed binds it
can read nor return rows the engine can read. That is the whole reason `Bind`,
`Value`, `Row` and `DbError` exist, and why no engine signature may name a vendor's
type.

`Bind::Inferred` is correctness, not formatting. PostgreSQL refuses a parameter
DECLARED as text into a `timestamptz` column and accepts the identical bytes when the
client declares nothing, inferring the type from the column. The engine renders DML
from IR and does not know the target column's type, so "declare nothing" is
information that has to ride per VALUE, letting one statement mix a typed key with an
inferred instant.

Exact integers must never become JS floats. `JsCell` carries `intStr` beside `int`
and the string wins when present, because `int8` and `numeric` cross as strings.

One pinned connection, one verb in flight. The advisory lock, the transaction and the
confinement `SET`s are all connection-scoped, so the whole apply is one session. A
second concurrent verb would block on a socket the first has not released; the
in-flight guard turns that deadlock into a loud panic.

The addon must stay cross-platform. That is what forbids compio, io_uring, an
embedded V8, and tokio: `napi` is declared `default-features = false` with only
`napi6` and `serde-json`, which drops the tokio-backed async helpers.

The host owns the sockets, so the host owns transport policy. TLS pinning and the
network allowlist are enforced in the JS drivers, not in Rust.

## Open

1. Host-provided SQLite. Only the native bundled `rusqlite` path exists; there is no
   `bun:sqlite` or `better-sqlite3` driver in the tree. Decision needed: offer a host
   SQLite override for hosts that want one driver surface for all three dialects, or
   declare native-only final and stop treating it as a fork.

## History

The deliberation lives in the sibling proposals
`docs/proposals/2026-07-10-migrate-pg-driver-seam-design.md`,
`docs/proposals/2026-07-11-migrate-napi-shell-design.md`,
`docs/proposals/2026-07-11-migrate-napi-node-addon-build-shape.md` and
`docs/proposals/2026-07-12-zero-migrate-redesign-plan.md`, and in the git history of
the crates and packages named above.

Do-not notes, each recording something that broke:

- Do not let the DSL be imported under a second package name or split into a second
  module instance. The recorder is an ambient singleton; a file that imports a
  different copy records into a different singleton and drains an empty op list, so
  the apply silently does nothing. `tsup` code splitting hoisting `ops.ts` into one
  shared chunk is what keeps `index.js` and `internal/recorder.js` on the same
  instance, and the platform corpus was once unappliable for exactly this reason.
- Do not trust `pg.types.setTypeParser` defaults. It is global and mutable, so a host
  app that overrode the `int8` parser to `Number` would truncate large integers below
  the seam with no error. The driver constructs its client with its own `types`
  object that forces oids 20 and 1700 to the verbatim string and decodes 16, 1003,
  1009 and 1016 itself, consulting `pg.types` for none of them. Oid 16 is pinned for a
  second failure mode: a leaked raw wire string makes `Boolean("f")` return `true`,
  turning every `false` into `true` silently.
- Do not decide a network allowlist from a URL's WHATWG authority. `pg` re-parses the
  connection string and honours a `host` query parameter that overrides the
  authority, so the parser that checked was not the parser that dialled.
  `hostsDesignatedBy` (`packages/zero-migrate-cli/src/net-allowlist.ts`) enumerates
  every host a URL could designate and all must pass.
- Do not add a napi export by editing generated artifacts. The `baseline` verb once
  landed in the declaration without a matching named assignment in `index.js`.
  The generated loader now exports `nativeBinding` wholesale, so its public runtime
  surface comes from the binding rather than a parallel function list. Run the napi
  build and the Node-hosted addon tests after changing the Rust exports.
- ~~Do not run a bare `cargo test -p zeroship-migrate-node`.~~ **CORRECTED 2026-09-04:
  run it; it works with `napi` ON.** The link failure this described was real (exit 101,
  1719 `undefined reference` lines) and is fixed by a `napi` entry in `[dev-dependencies]`
  carrying `dyn-symbols`, which resolves the Node ABI through libloading for TEST BINARIES
  ONLY. The shipped cdylib is unchanged - it still leaves 52 `napi_*` symbols undefined for
  the host to supply, which `tests/napi_symbol_shape_gate.sh` asserts. `npm test` remains
  the only thing that exercises the boundary itself.
- Do not expect `hostDriver` to be called with two arguments. napi delivers
  `(request, done)` as a single array; the host destructures it.
- Do not have the addon read its own configuration from the process environment. The
  diagnostics switch was an ambient `std::env::var`, which turned stderr logging on
  inside callers that never asked for it and named a variable nothing declared. The
  host states the value across the boundary instead.
- Do not read the descriptor round trip as a correctness claim. It pins that every
  facet the fold recovered survives the wire, not that the values are right or that
  re-importing reproduces the schema. `VARCHAR(n)` width crossed the wire intact and
  died one layer down until the producer learned to read `max_length`.
