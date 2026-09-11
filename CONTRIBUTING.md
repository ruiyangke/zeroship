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
  `plugin-{db,kv,storage,workflow}`, `workflow-scheduler`, `metering`, `stream`,
  `bundle`, `core`, `mailer`, `cli`, and the migration crates
  `migrated`/`zeroship-migrate-adapter`/`zeroship-schema`). Run `ls crates/` rather
  than trusting this list; a prose inventory has nothing that fails when it rots.
- `libs/` - standalone, zeroship-independent driver libraries: `compio-postgres`,
  `compio-redis`, `compio-s3`. Publishable on their own.
- `sdks/` - the `@zeroship/*` npm packages (a pnpm workspace): `db`, `kv`, `storage`,
  `auth`, `rpc`, `ui`, `vite-plugin`, `control`, `payments`, `react`, `migrate`,
  `workflows`, `server`, `types`, `mcp`, `eslint-config`, `eslint-plugin-workflow`,
  `create-zeroship-app`, `zeroship-stub`, and the framework-internal `bootstrap`.
  Same caveat as `crates/`: `ls sdks/` is the source of truth.
- `db/` - the platform's own database schema, authored as `@zeroship/migrate`
  migrations in `db/migrations-ts/` (the sole platform migration source - no SQL/Flyway).
- `deploy/` - everything about running/shipping: `Dockerfile`, `compose/`, `ops/`,
  `verdaccio/`, `policies/` (Cedar), `scripts/`.
- `examples/` - creator-app demos. `tests/` - end-to-end shell suites.
- `docs/` - architecture, reference contracts, decisions (ADRs), proposals, runbooks.
- `third_party/zero-migrate` - the vendored migration engine (a git submodule).

## Development

Prerequisites: a stable Rust toolchain, plus Node.js and pnpm at the versions
`package.json` declares in `engines` and `packageManager` (currently Node >= 20 and
pnpm 9). The live database tests expect the PostgreSQL that
`deploy/compose/docker-compose.yml` pins, exposed on `127.0.0.1:5440`. Some e2e
suites need Docker.

Those files are the authority; the versions named here are a convenience copy and
can drift from them.

**First, initialize the submodule** - the workspace won't resolve without it:

```
git submodule update --init third_party/zero-migrate
```

**Build the SDKs before the Rust workspace.** The runtime crate `include_str!`s
`sdks/bootstrap/dist/*` and `sdks/db/dist/internal.js`, so `pnpm build` must run before
`cargo build`:

```
pnpm install
pnpm build            # db -> bootstrap -> the rest of sdks/*, in dependency order
cargo build --workspace
```

Rust gates - run them before pushing:

```
./tests/clippy_gate.sh
cargo check --workspace
cargo test --workspace
```

**Lint through `tests/clippy_gate.sh`, which is what CI runs, and NOT through
`cargo clippy --workspace -- -D warnings`.** The workspace grades its own lints
in the root `Cargo.toml`; `-D warnings` promotes the pedantic and nursery groups
it deliberately leaves as warnings, so it reports thousands of errors that are
not gate failures (measured on one crate alone: 1744). The gate also audits
cargo's JSON stream to catch packages that were never linted at all, which a
bare `cargo clippy` cannot do because a deny-level lint in one crate aborts the
run before the crates after it are scheduled.

**There is no `cargo fmt` gate.** CI runs no formatting step, and the tree does
not currently satisfy `cargo fmt --all -- --check` - it reports diffs in 888
files (measured 2026-08-23 under the `nix develop` toolchain, and confirmed with
the newer rustfmt in the same store). This block used to list that command and
say the block mirrored CI; both halves were false, which is worse than saying
nothing: a checklist whose gate cannot pass trains you to read red as normal.
If you want formatting enforced, that is a one-off tree-wide reformat plus a CI
step, and it wants to land when nothing else is in flight.

Per-crate iteration is faster; run the full per-crate suite (not just `--lib`) for the
crate you touched, e.g. `cargo test -p zeroship-gateway`. Driver tests that need a live
database run single-threaded, e.g. `cargo test -p compio-postgres -- --test-threads=1`.

JavaScript:

```
pnpm build
pnpm check            # typecheck every sdks/* package
pnpm test             # vitest across sdks/*
```

Examples own their tests, fixtures, test configuration, and test dependencies
under their example directory. Repository test commands and CI invoke those
local entry points. Keep example-specific acceptance logic out of shared test
helpers and platform crate test suites.

Web Platform Tests (only when you touch the runtime's web surface) are fetched on
demand and are not tracked in git:

```
./crates/runtime/tests/setup-wpt.sh
```

DB-gated and end-to-end suites (bring up the dev Postgres via
`docker compose -f deploy/compose/docker-compose.yml up -d postgres`, or point at your
own server):

```
tests/run_auth_suite.sh         # the auth live-database gate. Uses a SHARED
                                # database named after this tree's migration
                                # set, so two agents on one commit can run it
                                # at the same time; --database <name> for a
                                # private one. TEST_DB in the environment is
                                # refused. docs/runbooks/local-dev.md says why.
tests/run_billing_suite.sh      # provisions the DB + runs every live-database suite
                                # (everything behind the `live-db-tests` feature
                                #  in zeroship-control / zeroship-migrate-server)
tests/run_worker_suite.sh       # the same, for zeroship-worker: seven workflow-
                                # advance tests that join zeroship.apps/plans/
                                # app_deploys and so need a MIGRATED database.
                                # --dsn <url> points it at a server you control.
tests/sweep_test_databases.sh   # reclaim the test databases no branch can ask
                                # for. Dry run unless --apply; never FORCE.
./tests/golden_path.sh          # build a creator app locally and deploy it
./tests/e2e_platform.sh         # multi-service platform smoke
./tests/e2e_docker.sh           # the full stack under Docker Compose
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
clone with `git config core.hooksPath .githooks`; CI runs the same checks over
the PR range, so an unconfigured clone or a `--no-verify` is still caught.
Check a range yourself with `tests/commit_msg_gate.sh --range origin/main..HEAD`.

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
  `plugin-db`, `kv-v8`, `plugin-storage`, `metering`, `stream`, `bundle`, `core`,
  `cli`, `mailer`
- Drivers (`libs/`): `compio-postgres`, `compio-redis`, `compio-s3`
- SDKs: `db`, `kv`, `storage`, `rpc`, `ui`, `vite-plugin`, `bootstrap`, `payments`
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

  That ceiling is measured rather than picked. Over a week of 699 commits the
  subject length ran p50 75, p90 91, p99 116: a 72-char limit would have
  rejected 413 of them, and was in fact ignored. 100 accepts 676 and refuses
  only the multi-clause outliers. A ceiling that is kept beats one that is
  rewritten every few days - if you find yourself over it, the subject is
  carrying two changes or a sentence that belongs in the body.
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

**The whole message is capped at 500 characters,** which with a typical subject is
about six wrapped lines. This is deliberately tighter than what the repo was doing:
the week before it landed ran p50 623 and p90 829, so it refuses the prevailing
style rather than ratifying it. Say what changed and why it is not obvious; a
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
