# Repository reorganization

**Status:** proposal · **Date:** 2026-08-06 · **Scope:** full restructure (approved)

The zeroship workspace spine is sound — `crates/` (23 Rust crates), `sdks/` (19 npm
packages), `docs/`, `examples/`, `tests/`, `db/migrations-ts/` map cleanly to the
two-system model in `AGENTS.md`. This proposal does **not** touch that spine. It
removes accumulated clutter, fixes drift between docs and code, resolves a few
ambiguously-named units, and consolidates deploy/ops config that is currently
scattered across the repo root.

Pre-launch, no back-compat: renames are outright (no aliases, no shims), and every
producer/consumer/fixture/doc changes in the same phase.

---

## Problems (severity-ordered)

### P1 — Root directory clutter & stale artifacts
Dead files and duplicate build dirs at the repo root:

| Item | Status | Disposition |
| --- | --- | --- |
| `appbase.db-shm`, `appbase.db-wal` | stale SQLite WAL, March, **old `appbase` name** | delete + gitignore |
| `.zeroship/kv.redb` | 1 MB dev redb artifact | delete + gitignore `.zeroship/` |
| `.dist/` | April, `appbase`-era build output | delete |
| `dist/` | stale root build output (gitignored) | delete |
| `test-results/` | April test output (gitignored) | delete |
| `blob-cache/` | cache dir, **untracked AND not gitignored** → accidental-commit hazard | gitignore |
| `DSL_GAPS.md` | migrate scratch notes (Jul 4) | move to `docs/archive/` or delete |
| `ISSUES.md` (48 KB) | dev backlog at repo root | move to `docs/` (see P6) |

### P2 — Deploy / ops config scattered across root
Four compose files + `Dockerfile` + `ops/` live at or near the root:
`docker-compose.yml`, `docker-compose.cluster.yml`, `docker-compose.lago.yml`,
`docker-compose.openmeter.yml`, `Dockerfile`, `ops/{Caddyfile,db-migrate.sh,
postgres-init.sql,openmeter-config.yaml,zeroship*.toml}`, `config/verdaccio/`.

No single place describes "how this is deployed/run."

### P3 — `apps/zeroship-builder` should be extracted (not deleted)
195 tracked files of a full Vite frontend (the **creator console**), plus committed
`dist/app.zship`, `design/REDESIGN*.md` scratch, and ignored `node_modules/`,
`test-results/`, `playwright-report/`.

**It is live, not orphaned:** `crates/zeroship-control/src/main.rs:224` seeds it as a
platform-owned regular app; the standalone builder Vite *service* was retired in the
R5 cutover and "the AI app-builder IS the console." Extraction mirrors the deferred
`zeroship-sandbox` split: the console is a standalone frontend that deploys *as an
app* through the same golden path, so it does not need to live in the platform
monorepo. The control-plane seed references it by app record, not by source path, so
extraction does not break seeding.

### P4 — `@zeroship/migrations` is redundant with zero-migrate's backfill → RETIRE
`@zeroship/migrations` (`sdks/migrations/`) is an online data-backfill orchestrator
(`defineMigration({ collection, batchSize, migrateOne })` + `migrations.run/status/
cancel`, dead-letter, dry-run). It overlaps almost entirely with zero-migrate's own
**batched, cursor-based, resumable, filtered** backfill op — `table.backfill({ set,
where, cursorColumn, batchSize })`, executed per-backend with a journaled
`BackfillProgressEntry` (PG/SQLite/MySQL). The only things `@zeroship/migrations`
adds: arbitrary-JS-per-row transforms, app-runtime (on-demand) invocation, and
failure-budget/dead-letter bookkeeping.

**Decision: retire `@zeroship/migrations`; zero-migrate `.backfill()` is the single
canonical backfill.** Two backfill systems is exactly the duplication this reorg
should remove; nearly every real backfill is a declarative column-from-columns
transform that zero-migrate's `set` expressions cover, and the arbitrary-JS minority
can write online from app code. (Supersedes the earlier "rename to `@zeroship/
backfill`" plan — deletion beats rename: the name collision disappears entirely.)

**Coupling caveat (verified):** the retirement is a *surgical extraction*, not a bulk
delete. `crates/zeroship-plugin-db/src/audit.rs` + the `__zeroship_migrations` table are
**shared with the schema/DDL path** — `backend/postgres.rs:1074` and
`register_model/validate.rs:86` write `Phase::Ddl` audit rows; `ensure_audit_table_
exists`/`next_schema_version` back schema versioning. The advisory-lock lifecycle in
`migrations.rs` (`exec_begin`/`release_active_lock`, wired through `backend/mod.rs` +
`owned_lock_guard`) may also be shared. **`audit.rs` + the table + the shared lock
machinery STAY.** Only the backfill-specific surface is removed (see Phase 4).

### P5 — Docs drift: `AGENTS.md` points at a crate that no longer exists
`AGENTS.md` (task router + crate index) references **`crates/zeroship-migrate/`**,
which was split into `crates/zeroship-migrate-server` (creator migration service) +
`crates/zeroship-migrate-adapter` + `crates/zeroship-schema` (leaf schema authority)
+ the `third_party/zero-migrate` submodule. The landing page must match the tree.

### P6 — Thin / duplicated docs buckets
`docs/reports/` (1 file), `docs/research/` (1 file), `docs/design/` (2 files) are
near-empty; `docs/AGENTS.md` (23 lines) is a second, divergent copy of the root
`AGENTS.md`.

### P7 — Cargo workspace cruft (trivial)
`Cargo.toml` `members = ["crates/*", "crates/authz", "crates/compio-s3"]` — the last
two are already matched by `crates/*`. Redundant leftover.

### P8 — Crate-boundary findings (from the 2026-08-06 boundary audit)
A full audit of all 24 crate dirs (the supplied dep graph was corrected: the four
"phantom deps" `zeroship-gate`/`mock-stripe`/`bench-server`/`platform-migrate` are all
`[[bin]]` targets inside existing crates; `core → bundle` is correct; the `auth ↔
authz` cycle is a *dead* dev-dep). Three concrete issues are **in scope**:

- **P8a — `core` is not actually a leaf.** `crates/zeroship-core/src/wrapper_revocation.rs`
  runs async Postgres OAuth token-family revocation queries
  (`use compio_postgres::{Client,Error}`), pulling `compio-postgres` into the
  "wire-types leaf." This is the one genuine layering violation. → move the file into
  `zeroship-auth`, drop `compio-postgres` from `core`.
- **P8b — dead dev-dep + phantom cycle.** `crates/zeroship-authz/Cargo.toml` lists
  `zeroship-auth` in `[dev-dependencies]`, but no `.rs` under `authz` references it.
  → delete it; removes the only cycle in the graph and speeds authz test builds.
- **P8c — bench bins in the prod runtime build.** `zeroship-bench-server` +
  `echo-server` are ungated `[[bin]]`s in the 144k-loc `runtime` crate, compiled by
  every `cargo build -p zeroship-runtime`. → gate behind
  `required-features = ["bench-bins"]` (or move to `crates/zeroship-runtime/examples/`).

Two further findings are **out of scope for this branch** (see Non-goals / Appendix A):
`authn` fold (declined — kept as-is) and the `control` → `control-billing` split
(design-only, Appendix A).

---

## Non-goals (deliberately NOT restructured here)

- **`authn` stays a crate.** The audit flagged it as thin (518 loc) and foldable into
  `core`, but a shared bearer/PAT crate with a clean dep footprint (`authz` + `core`),
  consumed by `control` + `migrated`, is not egregious — and folding into `core` would
  drag `authz` into the wire-types leaf, while folding into `auth` would force
  `migrated` to pull the whole 45k-loc OIDC OP. **Decision: keep as-is, document the
  triad clearly** in `AGENTS.md`.
- **`control` is NOT split in this branch.** The audit's strongest structural finding
  is that ~30k loc of Stripe/pricing/proration/refund billing lives inside `control`
  and should become `control-billing`. That is a ~30k-loc refactor, not a
  file-reorg — bundling it here multiplies branch risk for no scheduling gain.
  **Decision: design it now (Appendix A), execute as a dedicated follow-up.**
- **The migration crate split stays.** `schema` (no-v8/no-runtime leaf), `migrated`
  (managed-policy service), and `migrate-adapter` (genuine `SqlSession` newtype bridge
  — not a re-export) are each justified. We fix the *docs* (P5), not the crates.
- **`refs/` (vendored upstream reference repos) stays** — gitignored, local-only.
- **`third_party/zero-migrate` submodule stays** — it is the vendored engine.

---

## Target top-level layout

Seven source dirs, each with one job; plus two vendored trees and root config.

```
zeroship/
├── crates/                     # 21 platform Rust crates (compio-* moved out)
├── libs/                       # NEW — standalone, zeroship-independent, publishable
│   ├── compio-postgres/        # moved from crates/  (bespoke io_uring drivers,
│   ├── compio-redis/           #   zero zeroship-* deps → first-class "infra libs",
│   └── compio-s3/              #   clean spin-out seam)
├── sdks/                       # 18 npm packages;  migrations/ RETIRED (see P4)
├── examples/  tests/  db/  docs/
├── deploy/                     # NEW — all deploy/run/ops config in one place
│   ├── Dockerfile              # moved from root
│   ├── compose/{docker-compose,cluster,lago,openmeter}.yml   # were root
│   ├── ops/                    # moved from root ops/ (Caddyfile, db-migrate.sh, …)
│   ├── verdaccio/              # moved from config/verdaccio/
│   ├── policies/               # moved from root policies/ (Cedar creator/platform)
│   └── scripts/                # moved from root scripts/ (publish-sdks, dragonfly)
├── refs/                       # gitignored — vendored upstream reference clones
├── third_party/zero-migrate    # submodule — vendored engine
└── (root) Cargo.toml  Cargo.lock  package.json  pnpm-*.yaml  flake.*
          AGENTS.md  README.md  CLAUDE.md  .gitignore  .github/
```

Folded away from the old root: `config/` → `deploy/verdaccio/`; `policies/` →
`deploy/policies/`; `scripts/` → `deploy/scripts/`; `ISSUES.md` → `docs/`;
`DSL_GAPS.md` deleted; `apps/zeroship-builder` extracted to the sibling console repo
(`apps/` removed).

### docs/ internal tidy (unchanged top-level)
```
docs/  proposals/ reference/ architecture/ decisions/ runbooks/ reviews/ archive/
       research/                       # design/ + reports/ folded in
       ISSUES.md                       # moved from root
```

### Workspace wiring after the libs/ move
- `Cargo.toml`: `members = ["crates/*", "libs/*"]` (+ keep `exclude`).
- `[workspace.dependencies]`: re-point the three `compio-*` path entries
  `crates/… → libs/…`. Per-crate `Cargo.toml`s are untouched (they use
  `{ workspace = true }`), so this is a one-file path edit.

---

## Phased migration plan

Each phase is independently committable and independently verifiable. Order matters:
cheap/safe hygiene first, code-touching renames last.

### Phase 0 — Branch & baseline
`chore/repo-reorg` off `main`. Record green baselines: `cargo build --release`,
`pnpm build`, `pnpm -r test` (or the subset used in CI), `tests/golden_path.sh`.

### Phase 1 — Root hygiene (P1, P7) — zero code impact
- Delete: `appbase.db-*`, `.dist/`, `dist/`, `test-results/`, `.zeroship/kv.redb`.
- `.gitignore` += `blob-cache/`, `*.db-shm`, `*.db-wal`, `.zeroship/`.
- De-dup `Cargo.toml` members → `members = ["crates/*"]` (+ keep `exclude`).
- Move `DSL_GAPS.md` → `docs/archive/` (or delete).
- Verify: `cargo metadata` resolves; `cargo build` unaffected.

### Phase 2 — Deploy/ops consolidation (P2) — path rewrites
- Create `deploy/{compose,ops,verdaccio,policies,scripts}`; `git mv` in: the compose
  files, `Dockerfile`, `ops/*`, `config/verdaccio/*`, `policies/*`, `scripts/*`.
- Rewrite relative paths inside compose files (build contexts, `env_file`, volume
  mounts, `postgres-init.sql`/`Caddyfile` references) and update every caller:
  `tests/e2e_docker.sh`, `docs/runbooks/docker-compose.md`, `ops/db-migrate.sh`
  invocations, any `-f docker-compose.*.yml` in scripts/CI, `scripts/publish-sdks.sh`
  callers, and any code that loads Cedar `policies/` by path (grep `policies/creator`,
  `policies/platform`).
- Verify: `docker compose -f deploy/compose/docker-compose.yml config` parses;
  `tests/e2e_docker.sh` still wires up; policy loader still resolves.

### Phase 3 — Docs consolidation (P5, P6)
- Fold `docs/design/` + `docs/reports/` into `docs/proposals/`/`docs/archive/` as
  appropriate; fold `docs/research/` (1 file) into `docs/reference/` or keep if it
  will grow.
- Reconcile `docs/AGENTS.md` vs root `AGENTS.md` → single source (delete the copy or
  make it a pointer).
- Refresh `AGENTS.md`: migrate-crate index (P5) + new `deploy/` locations + any moved
  paths from Phases 1–2.
- Move `ISSUES.md` → `docs/ISSUES.md` (or retire if superseded by the tracker).

### Phase 4 — Retire `@zeroship/migrations` (P4) — surgical feature removal
Treat as a **carefully-scoped, independently-reviewed step** (it touches plugin-db
native + the `env.db` V8 surface + shared audit/lock infra), not a mechanical move.

**Remove (backfill-specific):**
- `sdks/migrations/` (whole package) + `examples/db-migrations-playground/`.
- `crates/zeroship-plugin-db/src/v8_classes/migration.rs` + `v8_classes/migrations.rs` (the
  `env.db` backfill API) and their registration in `v8_classes/mod.rs`.
- `crates/zeroship-plugin-db/src/migration_sweeper.rs` (dead-letter sweeper) + its lib.rs wiring.
- The `migrateOne` batched-transform loop in `crates/zeroship-plugin-db/src/migrations.rs` —
  the row-fetch/apply/dead-letter parts. Keep the advisory-lock lifecycle the schema
  path shares (`exec_begin`/`release_active_lock`).
- Docs: the "Migrations (`@zeroship/migrations`)" section of `docs/reference/db.md`;
  the `@zeroship/migrations` line in `docs/runbooks/private-registry.md`; the comment
  in `sdks/bootstrap/src/internal.d.ts`; the `examples/README.md` row.

**Explicitly KEEP:** `crates/zeroship-plugin-db/src/audit.rs` + the `__zeroship_migrations`
table + `next_schema_version` (shared schema-DDL provenance), and any lock machinery
in `backend/mod.rs`/`owned_lock_guard.rs` the schema apply path uses.

**Verify:** `cargo build -p zeroship-plugin-db` + full per-crate test; the schema/DDL
audit path (`register_model`) still writes `Phase::Ddl` rows; `pnpm build` clean with
the package gone; grep for dangling `@zeroship/migrations` / `migrateOne` /
`migration_sweeper` refs → zero.

### Phase 5 — Crate hygiene + `libs/` extraction (P7, P8a–c) — Rust code changes
- **`libs/` move:** `git mv crates/compio-{postgres,redis,s3} libs/`; set
  `Cargo.toml` `members = ["crates/*", "libs/*"]`; re-point the three `compio-*` paths
  in `[workspace.dependencies]` (`crates/… → libs/…`). Per-crate manifests untouched
  (they use `{ workspace = true }`). Verify `cargo metadata` resolves + full build.
- **P7:** the members line above supersedes the old
  `["crates/*", "crates/authz", "crates/compio-s3"]` (compio-s3 now lives in `libs/`).
- **P8b:** remove the dead `zeroship-auth` dev-dep from `crates/zeroship-authz/Cargo.toml`.
- **P8c:** gate `zeroship-bench-server` + `echo-server` bins behind
  `required-features = ["bench-bins"]` in `crates/zeroship-runtime/Cargo.toml`; add the
  feature. Confirm `cargo build -p zeroship-runtime` no longer builds them; benches
  still build with `--features bench-bins`.
- **P8a (the real fix):** `git mv crates/zeroship-core/src/wrapper_revocation.rs
  crates/zeroship-auth/src/`; re-point its consumers (grep `wrapper_revocation` /
  `RevocationCache` / `family_revoked_at` across `auth`, `authn`, `control`,
  `gateway`); remove `compio-postgres` from `crates/zeroship-core/Cargo.toml`; confirm `core`
  no longer links a DB driver.
- Verify: `cargo build --release`; per-crate tests for `core`, `auth`, `authz`,
  `authn`, `control`, `gateway`, `runtime`.

### Phase 6 — Extract `apps/zeroship-builder` (P3) — destination repo READY
- Push its history to the sibling console repo (git-filter-repo or subtree export) —
  **the transfer itself is done against the destination, not committed on this
  branch.**
- In this repo: `git rm -r apps/zeroship-builder`; drop `apps/*` from
  `pnpm-workspace.yaml` (apps/ is now empty → remove the dir); verify nothing else
  references it (control-plane seed is by app record, not path — unaffected).
- Verify: `pnpm install` clean; `pnpm build` green without the app.

### Phase 7 — Full-suite verification & sign-off
`cargo build --release` + per-crate tests for any crate touched, `pnpm build`,
`pnpm -r test`, `tests/golden_path.sh`, `tests/e2e_docker.sh` (compose paths).
Critic pass on the final diff before the branch is offered for merge.

---

## Risks & mitigations

- **Compose path rewrites (Phase 2) are the highest-risk step.** Build contexts and
  volume mounts are relative; a missed path silently breaks `docker compose up`.
  Mitigation: `docker compose config` (resolves & prints the merged, path-expanded
  spec) on every compose file before/after; keep the diff mechanical. *Variant:* if
  the team prefers the root-level `docker-compose.yml`/`Dockerfile` convention, keep
  those two at root and only move the *alternative* stacks (`cluster/lago/openmeter`)
  + `ops/` into `deploy/` — smaller blast radius, most of the win.
- **Builder extraction (Phase 5) deletes 195 tracked files.** Irreversible without
  the destination repo in place. Mitigation: gate Phase 5 on the console repo
  existing and an explicit go; Phases 1–4 land independently first.
- **SDK rename lockfile churn (Phase 4).** Mitigation: single `pnpm install`, commit
  the lockfile in the same phase; grep for the old specifier repo-wide afterward.
- **AGENTS.md is the AI-agent landing page.** Stale paths mislead every future agent.
  Mitigation: Phase 3 refresh is mandatory, and Phase 6 greps AGENTS.md paths against
  the tree.

---

## Execution notes
- Commit-only; **do not push**. One phase per commit, each verified before the next.
- Phases 1–5 are safe to land as a train. Phase 6 (builder extract) executes once the
  sibling console repo has received the history.
- No `@deprecated` aliases, no compat shims (pre-launch invariant).

---

## Appendix A — `control-billing` crate split (design only; execute as a follow-up)

**Not part of the reorg branch.** Recorded here so the follow-up has a starting point.

**Motivation.** `crates/control` (73k loc) carries two operable concerns with different
blast radius and release cadence: (1) the app control-plane — app CRUD, deploy, env,
route registry, token/PAT handlers; and (2) the Stripe/Connect **billing engine** —
pricing, proration, refund, invoice-item reconciliation, spend engine, and the billing
crons. A fault or redeploy of the billing engine should not risk app-deploy
availability, and vice versa.

**Proposed split.** New crate `zeroship-control-billing`:
- Moves: top-level `stripe_*.rs`, `pricing.rs`, `proration.rs`, `refund.rs`, the
  `metering/` ingest+aggregation module, and `cron/{event_forwarder,spend_recompute,
  billing_reconcile}.rs`.
- Depends on: `core`, `metering`, `stream`, `mock-stripe` (bin), the compio-postgres
  driver — *not* on `control`.
- `control` depends on `control-billing` and mounts its handlers/cron registrations.

**Risks / why it is not in the reorg.** ~30k loc with shared `AppState`, shared DB pool
wiring, and handlers co-mounted on one ntex `App`. The state/handler seam must be
factored (a `BillingState` sub-struct + a `mount(cfg)` entry) before the move compiles.
That is a design-and-refactor task with its own critic pass — deliberately decoupled
from mechanical file moves so a billing regression never rides in on a reorg diff.

**Sequencing.** Land the reorg (Phases 1–7) first; branch `refactor/control-billing`
off the new `main`; factor `BillingState` + `mount()`; then `git mv` behind a green
per-crate + billing e2e suite (`tests/e2e_stripe_*.sh`, `tests/run_billing_suite.sh`).
</content>
</invoke>
