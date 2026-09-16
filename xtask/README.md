# Native test orchestration

`cargo xtask test data` runs the SQL, model macro, ORM, V8 adapter, CDC wire and
relay tests. Assertions remain in their owning crates. This independent tooling
workspace keeps orchestration dependencies out of shipped services.

The command checks data architecture, builds the real relay executable and the PostgreSQL fixture image,
runs nextest, then runs Cargo doctests. Each database test owns its PostgreSQL
container through an explicit Rust guard. The container supplies logical WAL,
pgvector and PostGIS, with a dynamically assigned host port. SQLite tests use
explicit temporary files. Startup failure fails the test; no external database
URL or test overlay is needed.

Install [cargo-nextest](https://nexte.st/docs/installation/pre-built-binaries/)
and make Docker available. `pg_dump` and `pg_restore` clients matching the
fixture server major version must be on PATH; the development shell supplies
them, so this applies to a shell built another way. The task rejects a version
mismatch before building the suite. The fixture server is declared in
`tests/fixtures/postgres/Dockerfile`, and the shell's client attribute in
`flake.nix` is bumped with it.
Build the workspace SDKs with `pnpm install --frozen-lockfile` and `pnpm build`
before compiling the V8 runtime.

```console
cargo xtask test data
cargo xtask test data-architecture
cargo xtask test data --filter 'test(tests::postgres::transactions::)'
```

The filtered command is for diagnosis. The unfiltered command runs the complete
suite and is used by CI. Tests release their containers after success or panic;
testcontainers' watchdog handles interrupted test processes.

`tests/data_architecture.rs` owns dependency, SQL placement, adapter, driver,
worker privilege and database fixture checks. Rust source parsing distinguishes
production code from test modules and documentation; Cargo metadata supplies
normal dependency closures. Each scan has a corpus floor and rejection controls.
Example acceptance tests belong to their examples and run through Vitest with
TypeScript fixtures and browser assertions.

Nextest owns reporting, timeouts and process isolation. Its `data` profile writes
JUnit results under the Cargo target directory. The database group bounds
concurrent container startup and V8 memory use. Pure SQL and wire tests can run
concurrently. Retries are disabled so a failing first attempt remains a failure.

Platform database orchestration lives in this package. The `zs-testkit` binary
is the suite-database provisioner, spawned as a child process by
`tests/live_suite_db.rs`; service tests include preflight source from
`tests/fixtures/platform_db/`. Its integration tests own PostgreSQL containers
and generated overlays. No helper package or optional test feature is required.
