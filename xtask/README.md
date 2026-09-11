# Native test orchestration

`cargo xtask test data` runs the SQL, model macro, ORM, V8 adapter, CDC wire and
relay tests. Assertions remain in their owning crates. This independent tooling
workspace keeps orchestration dependencies out of shipped services.

The command builds the real relay executable and the PostgreSQL fixture image,
runs nextest, then runs Cargo doctests. Each database test owns its PostgreSQL
container through an explicit Rust guard. The container supplies logical WAL,
pgvector and PostGIS, with a dynamically assigned host port. SQLite tests use
explicit temporary files. Startup failure fails the test; no external database
URL or test overlay is needed.

Install [cargo-nextest](https://nexte.st/docs/installation/pre-built-binaries/),
make Docker available, and put `pg_dump` and `pg_restore` clients matching the
fixture server major version on PATH. The task rejects a version mismatch before
building the suite. The fixture server is declared in `tests/fixtures/postgres/Dockerfile`.
Build the workspace SDKs with `pnpm install --frozen-lockfile` and `pnpm build`
before compiling the V8 runtime.

```console
cargo xtask test data
cargo xtask test data --filter 'test(native_transaction)'
```

The filtered command is for diagnosis. The unfiltered command runs the complete
suite and is used by CI. Tests release their containers after success or panic;
testcontainers' watchdog handles interrupted test processes.

Nextest owns reporting, timeouts and process isolation. Its `data` profile writes
JUnit results under the Cargo target directory. The database group bounds
concurrent container startup and V8 memory use. Pure SQL and wire tests can run
concurrently. Retries are disabled so a failing first attempt remains a failure.
