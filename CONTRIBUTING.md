# Contributing to zeroship

Thanks for helping build zeroship. This guide covers the repository layout, how to
build and test, and the commit message conventions. Read `AGENTS.md` first - it is the
landing page, with a per-feature task router and the key invariants.

## Development status - pre-launch, no back-compat

zeroship has never been published: no production users, no creator apps in the wild.
Every API, wire format, and schema is fair game to break. **Rename the symbol, delete
the old name, update every caller in the same change.** No `@deprecated` aliases, no
migration shims, no "legacy mode" fallbacks. See `AGENTS.md` -> *Development status* for
the full stance.

## Repository layout

- `crates/` - the Rust workspace: platform services and the runtime kernel
  (`gateway`, `runtime`, `runtime-macros`, `control`, `worker`, `auth`/`authn`/`authz`,
  the `kv`/`storage`/`workflow` families with their V8 bindings, `data-orm`/`data-v8`,
  `metering`, `stream`, `bundle`, `core`, `mailer`, `cli`, and the migration crates
  `migrate`, `migrate-core`/`migrate-backend`/`migrate-ir`, `migrate-server` and the
  per-dialect backends). Run `ls crates/` rather than trusting this list; a prose
  inventory has nothing that fails when it rots.
- `libs/` - standalone, zeroship-independent driver libraries: `compio-postgres`,
  `compio-redis`, `compio-s3`. Publishable on their own.
- `packages/` - the npm workspace packages: the `@zeroship/*` SDKs and the
  migration authoring, CLI, and driver packages. Run `ls packages/` for the
  current inventory.
- `db/` - the platform's own database schema, authored as `@zeroship/migrate`
  migrations in `db/migrations-ts/` (the sole platform migration source - no SQL/Flyway).
- `deploy/` - everything about running/shipping: `Dockerfile`, `compose/`, `ops/`,
  `verdaccio/`, `policies/` (Cedar), `scripts/`.
- `examples/` - creator-app demos. `tests/` - end-to-end shell suites.
- `docs/` - architecture, reference contracts, decisions (ADRs), proposals, runbooks.

## Development

Prerequisites: Docker, which stays a host service. Live database and end-to-end
suites use the services and ports declared by
`deploy/compose/docker-compose.yml`. Everything else comes from the `flake.nix`
development shell: the Rust toolchain (`rustc`, `cargo`, `clippy`, `rustfmt`,
`cargo-nextest`), Node.js and pnpm at the versions declared by `package.json`,
the `xtask` command, and the browsers the Playwright suites launch. Enter it with
`nix develop`. `cargo xtask test playwright-browsers` reads `flake.nix` directly
(`xtask/README.md` lists what it needs).

**Build the JavaScript packages before the Rust workspace.** The data V8 crate
embeds `crates/zeroship-data-v8/dist/adapter.js`, so `pnpm build` must run before
`cargo build`:

```
pnpm install
pnpm build            # builds the public DB SDK, host adapter, and remaining packages
cargo build --workspace
```

Rust gates - run them before pushing, from the `nix develop` shell:

```
cargo clippy --workspace --all-targets --all-features
cargo check --workspace
cargo xtask shards list                      # the test shards CI runs, one job each
cargo xtask test <shard>                     # one shard's tests and doctests, as CI runs them
cargo nextest run --workspace --profile ci   # every test at once, without shard preparation
cargo test --workspace --doc                 # nextest runs no doctests
```

`cargo-nextest` comes from the development shell, and the `ci` profile is the
one CI runs. A plain `cargo test --workspace` runs the same tests in-process and
still works; nextest is what the shards use because it runs one process per test
and every suite in parallel against the shared servers.

`cargo xtask test <shard>` raises its soft locked-memory limit to the hard limit,
and a shard whose tests start service fleets refuses to start below a floor,
saying how to raise the hard limit: every io_uring a test process, or a service
binary a test starts, opens is charged to one per-user locked-memory budget, and
a service default runs out partway through such a run as `Os { code: 12 }`
panics.

**Use the same Clippy invocation locally and in CI.** The root `Cargo.toml`
sets lint severity. Preserve those levels; a blanket `-D warnings` would turn
intentionally warning-level groups into errors. A failed Cargo command means
the lint run failed; fix the errors and rerun it.

**There is no `cargo fmt` gate.** CI runs no formatting step and the tree is not
rustfmt-clean, so `cargo fmt --all -- --check` fails. Enforcing it is a one-off
tree-wide reformat plus a CI step, landed when nothing else is in flight.

Per-crate iteration is faster; run the full per-crate suite (not just `--lib`) for the
crate you touched, e.g. `cargo nextest run -p zeroship-gateway` (or
`cargo test -p zeroship-gateway`).

Coverage-guided fuzzing for the `compio-postgres` wire decoders lives in
`libs/compio-postgres/fuzz` (a cargo-fuzz crate, excluded from the Cargo
workspace). Run a target from the crate root through the flake's `fuzz` shell,
which carries the nightly toolchain the sanitizer needs:

```
nix develop .#fuzz --command bash -c \
  'cd libs/compio-postgres && cargo fuzz run pgoutput -- -max_total_time=120'
```

The `backend_message` target drives the backend frame decoder and `pgoutput`
the logical-replication payload decoder. A panic or hang is a decoder bug; a
decoding error on arbitrary bytes is not.

Undefined behaviour in the workspace's pure `unsafe` code is checked with Miri
through the flake's `miri` shell, which carries the same pinned nightly as
`fuzz` plus the `miri` component. Run only the unit tests that drive a
package's unsafe blocks, because Miri cannot execute the V8, io_uring, FFI or
syscall paths the rest of the suite needs:

```
nix develop .#miri --command bash -c \
  'cargo miri test -p compio-postgres --lib cloning_'
```

The `miri` CI job runs the same per-package filters: `cloning_` and
`commit_declares` for `compio-postgres`, `aead_key_zeroizes` for
`zeroship-data-orm`, and `zeroize_slice` for `zeroship-runtime`.

### Test tiers

A crate links one test target, `main`, declared by `tests/main.rs`. It sets
`autotests = false`, so a `tests/<name>.rs` file is compiled by nothing until a
module declares it. `tests/main.rs` declares the tier modules and the shared
fixtures:

```rust
mod support;
mod integration;
mod e2e;
```

The tiers are:

- **Unit tests** sit beside the code under `src/**` behind `#[cfg(test)]`. They
  exercise one module's internals and own no server:
  `cargo test -p <crate> --lib`.
- **Integration tests** live under `tests/integration/` and drive the crate's
  public API in-process; they may own testkit fixtures and in-process servers or
  databases. `tests/integration/mod.rs` declares one `mod <suite>;` per suite
  file: `cargo test -p <crate> --test main integration::`.
- **End-to-end tests** live under `tests/e2e/`, spawn the crate's real binaries
  or other processes, and observe them from outside, or drive a browser.
  `tests/e2e/mod.rs` declares one `mod <suite>;` per suite file:
  `cargo test -p <crate> --test main e2e::`. A crate with no such suite declares
  no `e2e` module.

Fixtures shared by both tiers live under `tests/support/` and are reached as
`crate::support::...`; the module is declared once by `tests/main.rs` and
compiled once. Register a new suite in its tier's `mod.rs` in the same change as
its file, or its tests never run and nothing reports it. A suite's submodules
live in a directory named after the suite, so module resolution needs no
`#[path]`. Omit a tier module or `mod support;` when the crate has none.

A suite that must run alone in its own process keeps its own `[[test]]` target,
with a comment on the stanza stating why. That covers a suite that installs a
process-global default, counts process-wide resources, mutates process
environment, or reads an input a checkout does not carry. Its entry file lives
at `tests/<name>.rs`, outside the `main` target.

JavaScript:

```
pnpm build
pnpm check            # typecheck the @zeroship packages
pnpm test             # vitest across the @zeroship packages
```

Examples own their tests, fixtures, test configuration, and test dependencies
under their example directory. Repository test commands and CI invoke those
local entry points. Keep example-specific acceptance logic out of shared test
helpers and platform crate test suites.

Web Platform Tests (only when you touch the runtime's web surface) come from the
development shell's pinned `wpt` flake input, which the shell links at
`crates/zeroship-runtime/tests/wpt` (see `flake.nix`). Enter `nix develop` before
running that target; the tree is not tracked in git and nothing is fetched into
the checkout.

The workspace's tests are divided into shards (`xtask/src/shards.rs`), and each
shard prepares what its tests need before running them. The `auth` shard builds
the platform migration host and runs the complete auth, authn, authz, mailer and
gateway packages; their tests join the migrated platform server every test
process of the worktree shares and own their SMTP and HTTP fixtures through Rust.
The `billing` shard also builds the service binaries Control's workflow process
suites start, and runs control, migration, metering, and stream tests against
the PostgreSQL servers and the Redpanda broker every test process shares. The
`runtime` shard covers the runtime, worker, CLI, KV and storage packages, whose
worker tests own their PostgreSQL and Redis containers. Docker is required; no
external test database address is needed.

```bash
cargo xtask test auth
cargo xtask test billing
cargo xtask test runtime
```

A shard is a set of packages, not a feature area. `cargo xtask test workflow`
runs the workflow crates (the engine, calendar, client, schema, testkit,
V8 binding, manager, server and runner) and their doctests. The CLI's
workflow tests run in `runtime`, Control's workflow process suites in `billing`,
the workflow SDK packages' suites in `pnpm test`, the schema generators'
`--check` runs in CI's `checks` job, and the example apps' suites in
`cargo xtask test examples`.

The compio-postgres suites start their own PostgreSQL server the same way. Its
shard runs them with every feature the workspace-wide build gives the crate -
`tls`, which builds the TLS connector and its live suite, and the `with-*`
codecs other members enable - then again against PostgreSQL 18:

```bash
cargo nextest run -p compio-postgres
cargo xtask test compio-postgres
```

#### Testkit layers

A testkit depends only on crates below every crate whose tests use it, because a
crate whose own tests link a crate that links it compiles itself twice and its
types stop unifying. The layers, from the bottom:

- `zeroship-testkit-server` depends on no workspace crate, so any crate's tests
  may use it, a `libs/` driver's included.
- `zeroship-testkit` depends on the server layer, the `libs/` drivers and crates
  with no workspace dependency of their own.
- `zeroship-<area>-testkit` depends on the two layers below it and on production
  crates below every crate whose tests use it. It never depends on another area
  testkit.

Fixtures are never a crate. Adapters typed by a crate whose own unit tests need
them are a `macro_rules!` in its area testkit, and every crate that uses them
expands it.

`cargo xtask test repository`'s
`test_support_crates_sit_in_the_layer_their_name_says` reads this rule from
`cargo metadata`: every library-only package that classifies itself
`test-dev-tool` must have a name in one of those layers and normal path edges
below it. A package that classifies itself `test-dev-tool` and ships a binary
is placed instead by the reader's one explicit list of such dev tools, and
every name there must ship a binary target.

### CI

`.github/workflows/ci.yml` runs on GitHub-hosted runners:

- `plan` prints `cargo xtask shards list` for the test matrix, and fills the
  fixture image cache when `cargo xtask images list` changes.
- `test (<shard>)` runs `cargo xtask test <shard>`, one job per shard, so every
  test runs exactly once. `cargo xtask test repository` fails when a workspace
  package with tests is in no shard or in two, when the test job's shape could
  skip a shard or forgive its failure, or when another CI job runs a workspace
  package's tests. `miri` is the one deliberate exception: it interprets a few
  unit tests the shards also run natively.
- `verify` reads every shard's reports and fails unless each run executed
  exactly what its build lists, no test ran in two shards, and every declared
  test target is in one shard.
- `checks` runs the generator checks, the shipped-target build, rustdoc links,
  the data-architecture, repository and Playwright checks, the edge artifact
  check and clippy; `sdk`, `miri` and `fuzz-smoke` run the JavaScript suites,
  Miri and the fuzz smoke runs.
- `cargo-cache` builds every shard on `main` and saves the dependency cache the
  shards restore; `cache-budget` prints every Actions cache entry and their
  total. CI builds dependencies without debuginfo (`.github/cargo-ci.toml`) so
  that cache fits the budget.

No test pulls an image or installs a package at run time. The images every
fixture starts or builds on are listed once, in `zeroship_testkit_server::images`,
and the testkit recipes whose build installs packages in
`zeroship_testkit::images::FETCHING`; CI's plan job pulls and builds them once,
the shards and the `sdk` job restore them from the cache, and the Docker daemon
is stopped from pulling before the tests start, so a fixture image missing from
the list fails by name. Add a new fixture image to that list in the same change
as the fixture.

## Key invariants

Do not violate these without discussion (`AGENTS.md` has the full list):

- **Zero tokio.** Everything is compio/io_uring; the drivers under `libs/` are bespoke.
- **Native primitives are the kernel.** Anything expressible via `fetch` or composition
  belongs in a `@zeroship/*` npm package, not in Rust. The native `env.*` surface is
  small and stable on purpose.
- **Wire formats are explicit contracts.** `Manifest`, `RouteEntry`, `AppRecord`, the
  `.zship` layout, and RPC envelopes change deliberately - every producer, consumer,
  fixture, and reference doc changes in the same patch.
- **Every fix adds a regression test** that would fail before the fix.

## Commit messages

This repo uses Conventional Commits. Keep `git log` a readable, greppable changelog.

A `commit-msg` hook enforces the mechanical rules below. Enable it once per
clone with `git config core.hooksPath .githooks`. The hook is the only
enforcement, so an unconfigured clone or a `--no-verify` goes unchecked.

### Format

```
type(scope): imperative summary of what the change does
```

- One line, lowercase after the colon, no trailing period.
- Optional body after one blank line, for the "why" when it is not obvious.
- Breaking changes add a `!` before the colon: `type(scope)!: ...`.

Examples:

```
feat(rpc): stream server-function results over a single envelope
fix(plugin-db): keep decimal-literal column defaults in CREATE TABLE DDL
refactor(core)!: move wrapper_revocation out of core into authz
test(gateway): cover restart-unique metering producer ids
docs(reference): document the @zeroship/kv atomic counter surface
build(deploy): consolidate compose + ops config under deploy/
```

### Type

Pick exactly one. `fix`, `feat`, and `refactor` cover most changes.

- `fix` - a behavior or bug correction
- `feat` - a new user-visible capability
- `refactor` - internal restructuring with no behavior change
- `test` - adding or reworking tests only
- `docs` - documentation only
- `merge` - integrating a completed body of work
- `chore` - repo housekeeping with no source or behavior impact
- `style` - formatting only (rustfmt/prettier), no code change
- `build` - build system, workspace membership, packaging, deploy config
- `ci` - CI workflows and automation
- `perf` - a change made primarily to improve performance
- `bench` - adding or reworking benchmarks (this repo has a first-class benchmarking surface)
- `revert` - reverting a previous commit

Do not invent a new type beyond this list unless there is a real need; keep it
lowercase and single-word. The type is never a scope name - write `feat(auth): ...`,
never `auth: ...`.

### Scope

A single lowercase token (may contain `-`) naming the area touched. Reuse an existing
scope before inventing one - grep `git log` for the current vocabulary. Common scopes:

- Services & kernel: `gateway`, `runtime`, `control`, `worker`, `auth`, `authz`,
  `plugin-db`, `kv-v8`, `storage-v8`, `metering`, `stream`, `bundle`, `core`,
  `cli`, `mailer`
- Drivers (`libs/`): `compio-postgres`, `compio-redis`, `compio-s3`
- SDKs: `db`, `kv`, `storage`, `rpc`, `ui`, `vite-plugin`, `payments`
- Migrations: `migrate`, `migrated`, `migrate-adapter`, `schema`, `db` (platform schema)
- Domains: `billing`, `websocket`, `node-compat`, `workflows`, `deploy`
- Umbrella: `workspace`, `docs`, `tests`, `ci`, `deps`

Choose the most specific scope that still fits (`fix(postgres): ...` over
`fix(gateway): ...` for a driver-level change). A new crate or package earns a new scope
named after it.

### Subject line

- Imperative, present tense: `add`, `reject`, `remove`, `support`, `preserve`,
  `rename`, as if completing "This commit will ...". Not `added` or `adding`.
- Describe the effect, not the mechanics - a real outcome ("keep decimal-literal column
  defaults in CREATE TABLE DDL"), not "update code". Aim for 50-72 characters; the
  enforced ceiling is 100.

  A ceiling that is kept beats one that is rewritten every few days. If you find
  yourself over it, the subject is carrying two changes or a sentence that
  belongs in the body.
- Lowercase first word after the colon; no trailing period.
- No internal-process markers. Strip orchestration artifacts before committing:
  `phase N`, `stage N`, `part N`, `wave N`, `milestone`, job/task IDs (`J2`, `M0`,
  `P1 C6`, `L8`, `M21`), `A`/`B`/`C`/`D` step letters, and PR or issue numbers. They are
  meaningless to anyone reading the history later. State the outcome, not how the work
  was scheduled: `chore(reorg): libs/ extraction`, not `chore(reorg) phase 5: ...`.

## Names in code: no process markers either

The rule above applies to anything that outlives the work: **test names, function
and type names, module names, feature flags, and doc comments.** A commit subject
is read once in `git log`; a test name is read every time it fails, by someone who
was not in the room when the plan was written.

Name the behaviour, not the schedule:

```
p9z_supervised_consumer_exits_on_slot_invalidated        <- what plan item was this?
consumer_exits_when_its_replication_slot_is_dropped      <- what breaks if it fails
```

Two failure modes, both of which this repo has hit:

- **The decoder ring is a single comment.** A plan token gets defined once, usually
  in a doc comment on one file, and then used across many crates. Every other site
  assumes you already read that one comment. Delete or reword it and the rest of
  the tree becomes undecodable.
- **Short plan tokens collide.** Independent plans reach for the same cheap
  identifiers, so one token ends up meaning two unrelated things in one repo.
  Grepping to decode a name then lands you confidently on the wrong plan.

A plan identifier is fine in a proposal or an ADR, where the plan is the subject
and the document is dated. It is not fine in a symbol, because the symbol outlives
the plan. If a name needs a decoder, rename it; pre-launch there is no
compatibility reason not to.

### Breaking changes

Mark with `!` before the colon (`refactor(core)!: ...`). Do not use a `BREAKING CHANGE:`
footer. If the impact needs explaining, put it in the body and say what to use instead.
(Pre-launch, "breaking" is a code-evolution signal, not a user-compat promise.)

### Body

Usually omitted; a good subject carries most changes. Add a body when the rationale,
trade-off, or migration impact is not obvious. Separate it with one blank line, wrap
it at 80 columns, and write prose paragraphs (one idea each, blank line between),
not a bullet dump.

**The whole message is capped at 200 characters.** With a subject at the limit that
leaves about two wrapped lines of body, so it refuses narration rather than
ratifying it. Say what changed and why it is not obvious; a
measurement log, a transcript, or a narrative of how the work was scheduled belongs
in the PR description or a doc under `docs/`, where it can be edited later. A commit
message cannot be, and `git log` is read far more often than it is written.

```
refactor(core)!: move wrapper_revocation out of core into authz

core linked compio-postgres solely for OAuth token-family revocation, which made the
wire-types leaf pull a database driver. authz already owns the wrapper-token surface
and links compio-postgres, so it is the natural home; core is now a true leaf.
```

### Checklist

- [ ] `type(scope): ...` with a known type and an existing scope
- [ ] Imperative, lowercase after colon, no trailing period
- [ ] Describes a real outcome; 100 characters or fewer (aim for 72)
- [ ] `!` added if and only if it breaks a public contract
- [ ] Body only when the why is not obvious
- [ ] A regression test accompanies every bug fix
