# zero-migrate: the driver seam and the per-vendor backend split

**Status.** SHIPPED, with two of this plan's own locked decisions reversed in the
landing. The seam is `crates/zeroship-migrate-backend/src/driver.rs`; the three
backends are `crates/zeroship-migrate-{postgres,mysql,sqlite}/src/backend/mod.rs`;
the napi addon is `crates/zeroship-migrate-node`, a full workspace member that
ships (root `pnpm build` compiles it first). The npm side is `packages/zero-migrate*`.
Reversed: the crate spine is nine migrate crates, not four, and they are branded
`zeroship-migrate-*`, not `zero-migrate-*`. Every flaw this plan was written to fix
is fixed.

## What it is

**One driver seam.** `SqlSession` is the single injected runtime dependency: four
async methods and nothing else.

```rust
pub trait SqlSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError>;              // DDL, txn control, session setup
    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError>;
    async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError>;
    async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError>;
}
```

The seam carries no transaction object and no lock abstraction. `BEGIN`/`COMMIT`/
`ROLLBACK`, advisory locks and confinement `SET`s are SQL strings each backend
issues through `batch`/`exec`: they are engine logic, not driver methods. The
trait is `!Send` by design, because the napi block-on worker and the JS host are
single-threaded.

`Bind` is `Null | Bool | Int(i64) | Decimal(String) | Text(String) |
Inferred(Option<String>)`. There is no `Bytes` variant; binary DML values cross as
base64 inside `Bind::Text` and are decoded in the rendered statement.
`Bind::Inferred` sends a value with no declared parameter type, per value, so one
statement can mix a typed key with an inferred instant.

**Three backends, each owning its own dialect.** `PostgresBackend<'a, D: SqlSession>`
and `MysqlBackend<'a, D: SqlSession>` ride the seam, because for a network database
the host owns the connection, pool and auth. `SqliteBackend` does not: SQLite is
embedded, so it runs in process on a dedicated hardened `rusqlite` connection
(`bundled` + `load_extension`). `SqlSession` is therefore an implementation detail
of the two network backends, not a bound on the shared backend contract.

**No dialect SQL in shared code.** Lock acquisition, journal statements, session
setup and placeholder style are backend methods, rendered before SQL crosses the
seam. Postgres takes `pg_advisory_lock` and writes `$N`; MySQL takes `GET_LOCK`
with a bounded retry count and writes `?`.

**Crate layout.** `zeroship-migrate-ir` is the leaf wire contract: `MigrationIr`,
the closed `Op` enum, the closed `Expr` AST, `IrScalar`, the typed-id and
precondition vocabulary, the structural policy-free validator, the canonical
checksum, the fail-closed IR load gate, and the schemars emit of the envelope
schema. Pure data, no I/O, no C dependency, no dependency on the engine, so CI
TS-codegen and any host that only validates or checksums an envelope can consume
it alone. `zeroship-migrate-policy` owns the policy algebra and seal.
`zeroship-migrate-backend` owns the seam and the dialect-neutral backend contract.
The three vendor crates implement that contract and depend on the engine not at
all; the Postgres one also owns the `pg_query`/libpg_query parse-time deny-list,
cross-schema confinement, statement classification and advisories. `zeroship-migrate`
is the engine facade the embedder depends on. `zeroship-migrate-node` is a
terminal `cdylib`, so the graph is acyclic by construction. **The clause that used to
follow - "the root workspace keeps it out of `default-members` so a plain `cargo build`
never pulls the napi toolchain" - was false and is deleted (2026-09-04).** The root
declares no `default-members` key at all; the addon is a default member, and a plain
`cargo build` and `cargo test` both pull it. It links under both as of 2026-09-04, via
`napi/dyn-symbols` on its dev-dependency; the shipped `.node` is unaffected and
`tests/napi_symbol_shape_gate.sh` holds its symbol shape.

**npm layout.** `@zeroship/migrate` (at `packages/zero-migrate/`) is the authoring
DSL a migration file imports: `op.*`/`table()`/`t.*`, the closed `Expr` AST, and the
pure-JS recorder. Zero native code, zero runtime dependencies. `zero-migrate-cli`
is the host runtime: it loads the addon, ships the `pg` and `mysql2` driver
adapters, exposes apply/plan/status/history/validate, and provides the one
`zero-migrate` command-line tool. `zero-migrate-{postgres,mysql,sqlite}` are
vendor-attribute packages; installing one adds that vendor's table-option
namespace to the DSL, typed from the backend's own declarations.

**Persisted sentinels are a knob with a native default.** `SentinelPrefix`
defaults to `zero-migrate:enc:` and `zero-migrate:mask:`, and the default parser
rejects any other prefix fail-closed. A host that co-writes a schema with a
foreign writer injects that writer's prefix and gets a build/parse round trip
against it. `__zero_migrate` is the reserved SQL identifier prefix (table-rebuild
scratch tables, MySQL preview session variables).

## Why it is this way

A shared executor that issues one dialect's SQL cannot host a second dialect, and
advertising the second one anyway produces a typed lie that errors on the first
statement. That is why lock, journal and placeholder rendering are per-backend
methods rather than executor branches, and why a dialect must not appear in a
public driver config before a backend exists behind it.

SQLite stays in process. `node:sqlite` was rejected for its experimental extension
API and the Node dependency it forces onto the test loop; `better-sqlite3` was
rejected as a second native addon buying nothing over `rusqlite`. In-process keeps
the Node-free test loop fast, keeps statically registered `vec0` and FTS5 with
`load_extension` locked down, and keeps the SQLite C authorizer as defence in
depth. The engine already links C through `pg_query`, so the bundled SQLite is
incremental cost.

`Bind::Inferred` is load-bearing, not a formatting preference. PostgreSQL refuses
to assign a parameter *declared* as text into a `timestamptz` column and offers no
automatic cast, but accepts the identical bytes when the client declares nothing,
because it then infers the type from the target column and parses with that type's
input function. The engine renders DML from the IR, where an instant is a string
and the column type is unknown to it, so "declare nothing" is the only correct
choice for those values. A driver that declares types must map this to its
unspecified-type text spelling (`Type::UNKNOWN` for rust-postgres).

A standalone default must carry this project's own brand: no stranger's `pg_dump`
should carry a foreign one. The knob exists only because two independent writers
can share a schema, and two writers means two sentinel spellings.

Cut a crate only where a consumer exists. Splitting for a consumer that is not in
the repo is how the old schema crate became dead weight, most of it a data-plane
query language riding along for a caller that lived elsewhere.

## Open

1. **Brand.** Crates are `zeroship-migrate-*`, the DSL package is
   `@zeroship/migrate`, the CLI and vendor packages are `zero-migrate-*`, and the
   sentinel and reserved-identifier defaults are `zero-migrate`. This plan locked
   a single long-form brand everywhere and the tree did not take it. Decide
   whether the mixed naming is the end state or whether one of the two families
   converges on the other.
2. **MySQL has no live oracle.** `MysqlBackend` is real and the `mysql2` adapter
   ships, but the tests assert *generated MySQL SQL* only. No `MYSQL_TEST_URL`, no
   MySQL service in any gate under `tests/` and none in the compose topology, so
   nothing has ever executed a MySQL apply. Decide whether MySQL is supported (it
   needs a live integration gate) or advertised as untested.

## History

The deliberation was two independent architecture reviews dated 2026-07-12 that
scored the extraction 16/100 and 18/100. Those review files are **not in the tree**
(nothing matching them exists under `docs/reviews/`); the nearest surviving
material is `docs/reviews/2026-08-27-migrate-crate-survey.md`. Protective notes:

- **Do not add a crate-wide `allow(dead_code, unused_imports)`.** It was tried, and
  it hid roughly 12.5k lines that no build could compile while also making genuine
  dead code undetectable. Its justifying comment named a feature that was never
  declared, so the allowance was permanent rather than temporary.
- **Do not gate code or tests behind a cargo feature that is not declared in
  `Cargo.toml`.** It was tried, and 42 of 92 test files opened with
  `#![cfg(feature = "native-pg")]` against an undeclared feature, so they never
  compiled and about 38k lines of live-database coverage was dark while the build
  stayed green.
- **Do not advertise a dialect in a public driver config before its backend
  exists.** It was tried: `{kind:"mysql"}` was accepted and routed straight into
  the Postgres executor, so every MySQL apply issued `pg_advisory_lock` and died on
  the first statement.
- **Do not ship an embedding seam as trait declarations with no implementations.**
  It was tried, and the three host traits had zero `impl` blocks and no consumer
  outside dead feature-gated code, so the customization the extraction existed to
  provide did not exist.
- **Do not give the seam's `Row` a panicking `get()`.** Accessors are `try_get`;
  a driver returning an unexpected shape must be an error, not a panic in the
  engine.
- **Do not decompose the backend contract into `DbSession`/`LockProvider`/
  `JournalStore`/`CatalogReader`/`TransactionManager`.** That was proposed and
  rejected: it forces every driver author to implement or reject five traits to
  express one dialect. Dialect logic belongs in concrete per-vendor types.
