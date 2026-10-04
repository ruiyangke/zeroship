# Native test orchestration

`cargo xtask test data` runs the SQL, model macro, ORM, V8 adapter, CDC wire and
relay tests. Assertions remain in their owning crates. This independent tooling
workspace keeps orchestration dependencies out of shipped services.

The command checks data architecture, builds the real relay executable and the PostgreSQL fixture image,
runs nextest, then runs Cargo doctests. Each database case takes a database of
its own on the bare PostgreSQL server every test process of the worktree shares,
cloned from the server's template and dropped with the case's guard. The server
supplies logical WAL, pgvector and PostGIS, with a dynamically assigned host port. SQLite tests use
explicit temporary files. Startup failure fails the test; no external database
URL or test overlay is needed.

The development shell supplies `cargo-nextest`, the `xtask` command and the rest
of the Rust toolchain. Make Docker available, since it stays a host service.
`pg_dump` and `pg_restore` clients matching the fixture server major version must
be on PATH; the development shell supplies them, so this applies to a shell built
another way. The task rejects a version mismatch before building the suite. The
fixture server is declared in `crates/zeroship-testkit/src/postgres/Dockerfile`, and the
shell's client attribute in `flake.nix` is bumped with it.
Build the workspace SDKs with `pnpm install --frozen-lockfile` and `pnpm build`
before compiling the V8 runtime.

```console
cargo xtask test data
cargo xtask test data-architecture
cargo xtask test data --filter 'test(tests::postgres::transactions::)'
```

The filtered command is for diagnosis. The unfiltered command runs the complete
suite and is used by CI. A case drops its database after success or panic; the
shared server's in-container watchdog removes the server once no test process
has held its lease for the idle grace, however those processes ended.

`tests/architecture/mod.rs` owns dependency, SQL placement, adapter, driver,
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
`tests/suite_db/mod.rs`; the live-database preflight lives in
`src/platform_db/`. Its integration tests own PostgreSQL containers
and generated overlays. No helper package or optional test feature is required.

## Playwright and the development shell's browsers

`cargo xtask test playwright-browsers` checks that the workspace's Playwright
is the release the development shell's browsers were built for.
`tests/playwright/mod.rs` explains why that has to hold and what each check
reads. It is the one area that needs Nix, and it needs three things:

- Nix installed (https://nixos.org/download). The check enables the
  `nix-command` and `flakes` features on each `nix` call, so a default
  configuration works, and it reads `flake.nix` with `nix eval` rather than
  from the calling shell's environment.
- `pnpm install` completed, since it reads the installed `playwright-core`.
- The shell's browsers in the Nix store. Entering `nix develop` builds them.

```console
cargo xtask test playwright-browsers
```

CI runs it inside the development shell. Without Nix, run the other areas;
this one refuses with a message that says what is missing.
