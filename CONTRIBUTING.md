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

Rust gates - run them before pushing:

```
cargo clippy --workspace --all-targets --all-features
cargo check --workspace
cargo test --workspace
```

**Use the same Clippy invocation locally and in CI.** The root `Cargo.toml`
sets lint severity. Preserve those levels; a blanket `-D warnings` would turn
intentionally warning-level groups into errors. A failed Cargo command means
the lint run failed; fix the errors and rerun it.

**There is no `cargo fmt` gate.** CI runs no formatting step and the tree is not
rustfmt-clean, so `cargo fmt --all -- --check` fails. Enforcing it is a one-off
tree-wide reformat plus a CI step, landed when nothing else is in flight.

Per-crate iteration is faster; run the full per-crate suite (not just `--lib`) for the
crate you touched, e.g. `cargo test -p zeroship-gateway`.

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

The native auth suite builds the platform migration host and runs the complete
auth, authn, authz, mailer and gateway packages. Tests own their PostgreSQL,
SMTP and HTTP fixtures through Rust; Docker is required. No external test
database URL or generated backend overlay is needed.

```bash
cargo xtask test auth
```

Worker tests also own their PostgreSQL and Redis containers. Run
`cargo xtask test worker` to build the migration host and test the package.
The billing suite does the same for control, migration, metering, and stream
tests, including their PostgreSQL and Redpanda fixtures:

```bash
cargo xtask test billing
```

The remaining system suites use their configured development backends:

```bash
cargo xtask platform-db sweep   # reclaim the test databases no branch can ask
                                # for. Dry run unless --apply; never FORCE.
```

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
