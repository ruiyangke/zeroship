# Native test orchestration

This independent tooling workspace keeps orchestration dependencies out of
shipped services. Assertions remain in their owning crates.

## Shards

`src/shards.rs` divides the workspace's packages into test shards, the same
shards CI runs one job each for. A shard is a set of packages, not a feature
area: `cargo xtask test workflow`, for one, runs the workflow crates, while the
CLI's workflow tests run in `runtime` and Control's workflow process suites in
`billing`. `cargo xtask shards list` prints the names as the JSON array CI's
test matrix reads, and `cargo xtask test <shard>` runs one:

- it raises its own soft locked-memory limit to the hard limit, and a shard
  whose tests start service fleets refuses to start below a floor
  (`src/memlock.rs` says why, and where the floor comes from),
- runs the shard's preparation in order: the JavaScript host chain, the service
  executables Control's workflow suites start, the CDC relay, the data posture
  checks, and the shared PostgreSQL servers it holds for the whole run so each
  first boot is paid once,
- runs its untimed tests with a plain `cargo test`: the ORM's trybuild
  contract, whose first run in a cold checkout compiles a whole dependency
  graph and so measures the cache rather than the code,
- runs every other test of the shard's packages under nextest's `ci` profile,
- runs their doctests, which nextest does not run,
- and runs the shard's declared repeats.

Each run leaves its reports in `target/nextest/ci/` under the workspace root:
`junit-<shard>-<run>.xml`, `list-<shard>-<run>.json` (everything the run's build
lists) and `untimed-<shard>.txt`. `cargo xtask shards verify <directory>` reads
every shard's reports back, as CI's `verify` job does, and fails unless each run
executed exactly what it lists, no test ran in two shards, and every test target
`cargo metadata` declares is in exactly one shard.

`--filter` narrows a diagnostic run to a nextest filter expression and skips the
untimed tests and the doctests; `--build-only` builds every artifact the shard
runs and runs nothing, which is how CI's `cargo-cache` job fills the dependency
cache. `tests/repository/shards.rs` fails when a workspace package with tests is
in no shard or in two, when a test target needs a feature its shard's run does
not enable, when a feature of a shard package is enabled by no run and explained
by no entry, when the test job could skip a shard, or when another CI job runs a
workspace package's tests.

```console
cargo xtask shards list
cargo xtask test data
cargo xtask test data --filter 'test(tests::postgres::transactions::)'
cargo xtask test billing --build-only
cargo xtask shards verify target/nextest/ci
```

The development shell supplies `cargo-nextest`, the `xtask` command and the rest
of the Rust toolchain. Make Docker available, since it stays a host service.
Build the workspace SDKs with `pnpm install --frozen-lockfile` and `pnpm build`
before compiling the V8 runtime.

The `data` shard runs the SQL, model macro, ORM, V8 adapter, CDC wire and relay
tests. Each database case takes a database of its own on the bare PostgreSQL
server every test process of the worktree shares, cloned from the server's
template and dropped with the case's guard. The server supplies logical WAL,
pgvector and PostGIS, with a dynamically assigned host port. SQLite tests use
explicit temporary files. Startup failure fails the test; no external database
URL is needed. `pg_dump` and `pg_restore` clients matching the fixture server
major version must be on PATH; the development shell supplies them, so this
applies to a shell built another way. The shard rejects a version mismatch
before building the suite. The fixture server's base image is
`zeroship_shared_server::images::PGVECTOR_16`, and the shell's client attribute
in `flake.nix` is bumped with it.

A case drops its database after success or panic; the shared server's
in-container watchdog removes the server once no test process has held its
lease for the idle grace, however those processes ended.

## Fixture images

`zeroship_shared_server::images` lists every container image a fixture starts
or builds on, and `zeroship_testkit::images::FETCHING` names the testkit recipes
whose build installs packages from a distribution's mirrors. `cargo xtask
images list` prints both, the recipes by their content-hashed reference;
`cargo xtask images pull` pulls the base images the Docker daemon lacks and
builds the recipes it lacks; `cargo xtask images check` fails naming any
reference it lacks. CI hashes the list into its image cache key, builds and
caches once in its plan job, restores the images into every shard and the
`sdk` job, and stops the daemon from pulling.

## Repository checks

`cargo xtask test data-architecture` runs `tests/architecture/mod.rs`, which owns
dependency, SQL placement, adapter, driver, worker privilege and database
fixture checks. Rust source parsing distinguishes production code from test
modules and documentation; Cargo metadata supplies normal dependency closures.
Each scan has a corpus floor and rejection controls. `cargo xtask test
repository` runs `tests/repository/` and the harness's own unit tests.
`cargo xtask test examples` runs the example apps' own Vitest and Playwright
suites, which start a live platform and run by hand rather than in CI.

Nextest owns reporting, timeouts and process isolation. Retries are disabled so
a failing first attempt remains a failure.

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
