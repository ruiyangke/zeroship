# The `SqlSession` driver seam

**Status.** SHIPPED, and wider than this document originally proposed. The seam is
dialect-neutral (`SqlSession`, not a PG-only `PgSession`), it carries no driver's
concrete row/error/bind types, and there is no `native-pg` feature: the engine crates
declare no network driver at all. `compio-postgres` appears in exactly one migrate
manifest, `crates/zeroship-migrate-server/Cargo.toml:20`. The trait, its neutral types
and the conformance suite live in `crates/zeroship-migrate-backend/src/driver.rs` and
`driver/conformance.rs`, re-exported as `zeroship_migrate::driver`
(`crates/zeroship-migrate-core/src/lib.rs:151,234`). The generic backends are
`crates/zeroship-migrate-postgres/src/backend/mod.rs:125` and
`crates/zeroship-migrate-mysql/src/backend/mod.rs:91`. Four producers ship:
`NapiHostSession` (`crates/zeroship-migrate-node/src/session.rs`, driven from JS by
`packages/zero-migrate-cli/src/driver-pg.ts` and `driver-mysql2.ts`), `CompioPgSession`
(`crates/zeroship-migrate-server/src/session.rs`), the vendor test double
`RecordingSession` (`crates/zeroship-migrate-postgres/src/backend/recording.rs`, behind
that crate's `testing` feature), and the live test harness `PgDevSession`
(`crates/zeroship-migrate/tests/support/mod.rs:399`).

## What it is

`SqlSession` is the one injected runtime dependency the network-dialect backends are
generic over. It has four async verbs and no transaction object:

```rust
pub trait SqlSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError>;
    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError>;
    async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError>;
    async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError>;
}
```

`batch` is the simple-query verb: DDL, transaction control, and multi-statement session
setup. `exec` is parameterized DML returning rows affected. `query` and `query_one` are
catalog and journal introspection. `BEGIN`/`COMMIT`/`ROLLBACK`, advisory locks and the
confinement `SET`s are SQL strings each backend issues through `batch`/`exec`; they are
engine logic, not driver methods, so the seam needs no transaction or lock abstraction.

Every type crossing the seam is driver-neutral. `Bind` is the typed param carrier
(`Null`, `Bool`, `Int(i64)`, `Decimal(String)`, `Text(String)`, `Inferred(Option<String>)`).
`Value` is the decoded cell (`Null`, `Text`, `Int(i64)`, `Bool`, `Decimal(String)`,
`TextArray(Vec<Option<String>>)`); int2/int4/int8 all widen to `i64`, and element NULLs
inside an array are preserved as `None`. `Row` carries parallel column-name and value
vectors and exposes `len`/`is_empty`/`try_get` only, indexed by name or position through
`ColIndex`; decode resolves through the closed `FromValue` trait rather than any driver's
`FromSql`. `DbError` is a `message` plus an optional `sqlstate`. `Bind` and `Value` are
`#[non_exhaustive]`, so a driver author matches with a wildcard arm.

`Bind::Inferred` means "send with no declared type and let the server infer one from
context"; `Inferred(None)` is a SQL NULL. It is per-value, so one statement can mix a
declared key with an inferred instant. Binary DML values cross as `Bind::Text` holding
canonical base64 and are decoded by the rendered dialect SQL.

`PostgresBackend<'a, D: SqlSession>` and `MysqlBackend<'a, D: SqlSession>` are constructed
by `new_generic(conn: &'a D)` and are the whole apply path: journal, drift, backfill,
baseline, precondition and executor free functions are generic over `D`, and
`OnlineSchemaChange` is implemented directly on the generic backend. SQLite does not ride
this seam - it is an in-process `rusqlite` actor with no session object - so `SqlSession`
is an implementation detail of the network backends and not a bound on `MigrationBackend`.

`driver::conformance::run` is the seam's own test suite and its first external consumer. A
driver author points it at a live, empty scratch schema and gets one pass/fail verdict over
four invariants: session pinning (a temp object created by one verb is visible to the next),
transaction visibility (`BEGIN` / `exec` / `query` / `ROLLBACK` behaves as
`apply_transactional` assumes), bind-inference semantics (declared vs `Inferred`, separately
and mixed), and error/SQLSTATE mapping. The checks are neutral; the scratch SQL that
provokes them is not, so the caller supplies a `SeamFixture` carrying its own temp keyword,
integer and timestamp types, placeholder form, integer cast and missing-table SQLSTATE.
PostgreSQL and MySQL each supply one and both run the suite live.

The engine issues verbs strictly one at a time over one pinned connection. Both host-shaped
producers enforce that mechanically with an `AtomicBool` compare-exchange on verb entry,
cleared by an RAII guard so error paths clear too, panicking on re-entry.

## Why it is this way

**No driver type may appear in a seam signature.** Concrete driver rows, errors and bind
params have private constructors, so a host driver could neither be called (it cannot
extract cells from opaque bind params, which serialize to a wire format) nor return rows and
errors. The neutral `Bind`/`Value`/`Row`/`DbError` set closes both sides. This is what makes
the napi/Node host the *production* producer rather than a hypothetical second impl.

**The seam is text-biased on purpose.** The apply path already `to_char`-casts every
timestamp, counts as `bigint`, and `array_agg`s most arrays, so `Value` needs no `bytea`,
no `f64`, and no OID-typed return. `Decimal` crosses as its canonical string on both `Bind`
and `Value`: there is no `f64` in the IR identity and there must not be one.

**`Bind::Inferred` is not a formatting preference.** PostgreSQL refuses to assign a
parameter DECLARED as text into a `timestamptz` column - there is no automatic cast - and
accepts the identical bytes when the client declares nothing, because it then infers the
parameter's type from the target column and parses the text with that type's input function.
The engine renders DML from the IR, where an instant is a string and the column type is
unknown to it, so "declare nothing" is the only correct choice for those values. A driver
that declares types maps this to its unspecified-type text-format spelling
(`Type::UNKNOWN` for rust-postgres); node-pg and mysql2 never declare, so it is
indistinguishable from `Bind::Text` there.

**Session pinning is a load-bearing assumption the engine never re-checks.** A driver that
round-robins a pool passes a recorder smoke test and corrupts a real apply, because the
`BEGIN` lands on one backend and the `COMMIT` on another. The conformance suite exists to
make that a checked property of every driver rather than an assumption.

**Shadow-database dry runs stay off the seam.** They need a second connection and a
`CREATE DATABASE` provisioning lifecycle with no host analogue. `ShadowDryRun` is therefore
a capability passed as a parameter to `MigrationEngine::dry_run`, not a method on a backend,
and it currently has no implementor anywhere in the workspace, so every dry run refuses with
`DryRunError::ShadowUnsupported` and no caller can be handed a false-success report.
`crates/zeroship-migrate/tests/dialect_matrix/shadow_dry_run_has_no_implementor.rs` holds
that line, with a file floor over its walk and a positive control on the trait declaration
so a rename cannot make it report a meaningless zero.

**Vendor types stay in vendor crates.** The engine crates name no vendor crate, backend
module or grammar; the `dialect_matrix` gates under `crates/zeroship-migrate/tests/` enforce
this. That separation is why the driver dependency could leave the engine entirely instead of
hiding behind a feature flag.

## Open

1. **A real shadow-dry-run harness.** The trait's own docs describe the untrusted or
   AI-authored DDL preview it exists for, and nothing implements it. The decision needed is
   where shadow provisioning lives given that the seam deliberately carries no connect or
   `CREATE DATABASE` lifecycle, and writing the `impl` is not sufficient on its own: the
   engine refuses every dry run until a caller passes the capability, so the harness must be
   wired into the `dry_run` call path in the same change.
2. **Whether `DbError::sqlstate` gets a reader.** It is carried so a future retry or branch
   has a home without another widening, and no seam consumer reads it today - every consumer
   wraps the error opaquely as `#[source]`. The decision needed is whether SQLSTATE-driven
   retry belongs in the engine's apply path or stays a driver concern.

## History

The deliberation lives in the git history of this file and in its siblings:
`2026-07-10-migrate-runtime-decoupling-design.md` (the V8 gate),
`2026-07-11-migrate-napi-shell-design.md`, `2026-07-11-migrate-napi-host-boundary.md`,
`2026-07-11-migrate-napi-node-addon-build-shape.md`, and
`2026-07-12-zero-migrate-redesign-plan.md`. The Rust-side public surface is compiled as
doctests from `docs/embedding.md`; note that coverage there is partial (three of seven Rust
fences compile), and the `SqlSession` fence is deliberately `ignore`d because compiling a
quoted trait definition would declare a second trait that can silently diverge from the real
one while still passing.

Protective notes, kept because each records something that was tried and broke:

- **Do not put a concrete driver's `Row`, `Error` or `ToSql` in the seam signatures.** It was
  the first design here, and it makes the read path unimplementable by a host driver: a
  napi driver cannot construct or return a row type with private fields, so `query` and
  `query_one` could not be implemented at all.
- **Do not add a panicking `get` to `Row`.** Only `try_get` is exposed; a decode failure is
  always a `Result`. One consumer runs arbitrary user SQL and must be able to reject a
  non-boolean single-column select as an error rather than a panic.
- **Do not collapse `Bind::Inferred` into `Bind::Text`.** The declared-vs-inferred
  distinction is the difference between a statement running and failing on PostgreSQL, not a
  spelling choice.
- **Do not write a second copy of `RecordingSession` in the engine's test tree.** Its canned
  catalog and journal rows are the shared premise of two suites that cannot live in one crate
  (the engine depends on the vendor crates, so the edge back is a cycle Cargo refuses), and
  two copies drift silently until one suite asserts against a row shape the other already
  corrected. There is one recorder per vendor, reached through the vendor crate's `testing`
  feature, which resolver 3 keeps out of the normal build.
  `dialect_matrix/a_test_recorder_never_ships.rs` walks a table of vendors, so a third
  recorder is a row rather than a copy of that file.
- **Do not assert `backend.shadow().is_none()` per vendor.** Three such tests existed, all
  passed, all always would have, and together they still could not see a fourth backend or a
  host adapter quietly acquiring a shadow harness. One workspace-wide scan replaced them.
- **Do not `impl SqlSession for compio_postgres::Client` directly.** Both types are foreign
  to the consuming crate, so the orphan rule refuses it. `CompioPgSession` is the newtype
  carrier, and it is a module of the one crate that drives it rather than a crate of its own -
  the orphan rule is satisfied by the newtype being local, not by it being packaged alone.
- **Do not rely on node-pg's global type parsers in a host driver.** `pg.types.setTypeParser`
  is global and mutable, so a host application that overrode the int8 parser to `Number`
  would silently truncate large bigints below the seam with no error, and an override leaking
  the raw bool wire string turns every `false` into `true` via `Boolean("f")`. The shipped
  driver constructs its client with its own `types` object and consults `pg.types` for none
  of the affected OIDs.
