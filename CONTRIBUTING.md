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
  (`gateway`, `runtime`, `control`, `worker`, `auth`/`authn`/`authz`,
  `plugin-{db,kv,storage}`, `metering`, `stream`, `bundle`, `core`, `mailer`, `cli`,
  and the migration crates `migrated`/`zeroship-migrate-adapter`/`zeroship-schema`).
- `libs/` - standalone, zeroship-independent driver libraries: `compio-postgres`,
  `compio-redis`, `compio-s3`. Publishable on their own.
- `sdks/` - the `@zeroship/*` npm packages (a pnpm workspace): `db`, `kv`, `storage`,
  `auth`, `rpc`, `ui`, `vite-plugin`, `control`, `payments`, `react`, `migrate`, and
  the framework-internal `bootstrap`.
- `db/` - the platform's own database schema, authored as `@zeroship/migrate`
  migrations in `db/migrations-ts/` (the sole platform migration source - no SQL/Flyway).
- `deploy/` - everything about running/shipping: `Dockerfile`, `compose/`, `ops/`,
  `verdaccio/`, `policies/` (Cedar), `scripts/`.
- `examples/` - creator-app demos. `tests/` - end-to-end shell suites.
- `docs/` - architecture, reference contracts, decisions (ADRs), proposals, runbooks.
- `third_party/zero-migrate` - the vendored migration engine (a git submodule).

## Development

Prerequisites: a stable Rust toolchain, Node.js >= 20, and pnpm 9. The live database
tests expect PostgreSQL 16 (the dev stack exposes it on `127.0.0.1:5440`). Some e2e
suites need Docker.

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

Rust gates (these mirror CI - run them before pushing):

```
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo check --workspace
cargo test --workspace
```

Per-crate iteration is faster; run the full per-crate suite (not just `--lib`) for the
crate you touched, e.g. `cargo test -p zeroship-gateway`. Driver tests that need a live
database run single-threaded, e.g. `cargo test -p compio-postgres -- --test-threads=1`.

JavaScript:

```
pnpm build
pnpm check            # typecheck every sdks/* package
pnpm test             # vitest across sdks/*
```

Web Platform Tests (only when you touch the runtime's web surface) are fetched on
demand and are not tracked in git:

```
./crates/runtime/tests/setup-wpt.sh
```

DB-gated and end-to-end suites (bring up the dev Postgres via
`docker compose -f deploy/compose/docker-compose.yml up -d postgres`, or point at your
own server):

```
tests/run_billing_suite.sh      # provisions the DB + runs the billing money-path gate
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

Do not invent a new type unless there is a real need; keep it lowercase and
single-word.

### Scope

A single lowercase token (may contain `-`) naming the area touched. Reuse an existing
scope before inventing one - grep `git log` for the current vocabulary. Common scopes:

- Services & kernel: `gateway`, `runtime`, `control`, `worker`, `auth`, `authz`,
  `plugin-db`, `plugin-kv`, `plugin-storage`, `metering`, `stream`, `bundle`, `core`,
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
  defaults in CREATE TABLE DDL"), not "update code". Roughly 50-72 characters; never
  exceed about 80.
- Lowercase first word after the colon; no trailing period.

### Breaking changes

Mark with `!` before the colon (`refactor(core)!: ...`). Do not use a `BREAKING CHANGE:`
footer. If the impact needs explaining, put it in the body and say what to use instead.
(Pre-launch, "breaking" is a code-evolution signal, not a user-compat promise.)

### Body

Usually omitted; a good subject carries most changes. Add a body when the rationale,
trade-off, or migration impact is not obvious. Separate it with one blank line, and
write prose paragraphs (one idea each, blank line between), not a bullet dump.

```
refactor(core)!: move wrapper_revocation out of core into authz

core linked compio-postgres solely for OAuth token-family revocation, which made the
wire-types leaf pull a database driver. authz already owns the wrapper-token surface
and links compio-postgres, so it is the natural home; core is now a true leaf.
```

### Checklist

- [ ] `type(scope): ...` with a known type and an existing scope
- [ ] Imperative, lowercase after colon, no trailing period
- [ ] Describes a real outcome; about 72 characters or fewer
- [ ] `!` added if and only if it breaks a public contract
- [ ] Body only when the why is not obvious
- [ ] A regression test accompanies every bug fix
