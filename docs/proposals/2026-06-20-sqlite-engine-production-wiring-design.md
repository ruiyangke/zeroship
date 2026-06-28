# SQLite engine dev-tier wiring (epic P6) — design

<!-- Renamed in round 2 (L1): the original title said "production wiring",
     which misleads — SQLite is DEV-TIER-ONLY (§1). The "production apply
     path" for SQLite *is* the dev-tier apply path; there is no prod SQLite.
     The title now names what this is: wiring the engine into the dev tier. -->

**Status:** proposed (2026-06-20), revised post-critic (2026-06-20), uncommitted
(per `feedback_proposal_workflow`).
Worktree `appbase-migrate`, branch `feat/db-migration-engine`.

> **Round-2 note (post-critic, 72/100).** The critic accepted P6a (engine
> genericization, §3/§7) as sound and split it out to build separately. This
> revision re-grounds and *designs* the holes the original left in **P6b**
> (wire `registerModel`→engine + retire `run_sqlite_pipeline`): the two-connection
> coordination (C2/C3), the dev-only structural guard (C1), the existing-file
> baseline (H3), and the concrete dev `app_id`/path resolution (M1/M2). See the
> **Revision log (2026-06-20, post-critic)** at the end. §7's phase plan and the
> new **§7b (P6b two-connection coordination)** carry the resolved design.

**Scope:** Phase **P6** of the SQLite-parity epic
(`docs/proposals/2026-06-19-sqlite-engine-parity-design.md` §3): wire the
already-built, tested, security-hardened `zeroship-migrate` `SqliteBackend` into a
**production apply path** and retire plugin-db's `run_sqlite_pipeline` (the runtime
auto-migrate). This is the phase that closes the gap left by P1–P3b: the SQLite
backend is complete behind the `MigrationBackend` trait, but **nothing drives it** —
`MigrationEngine` is hard-typed to `compio_postgres::Client`, `plan_declarative`
**fails closed** on SQLite rebuilds, and the only live SQLite schema path is still
plugin-db's `run_sqlite_pipeline`.

> This is a READ-ONLY investigation + design. No code changed, no DB touched.

---

## 0. TL;DR

1. **The pivotal scoping answer: SQLite is the DEV TIER ONLY.** It is the
   self-contained peer of `pnpm dev` (the documented sibling of `env.kv`→redb,
   `env.storage`→LocalFs, auth→dev-provider). It is selected **purely by DB URL
   string** (`sqlite:` / `file:` / a bare path); prod workers run Postgres. The dev
   runtime is **single-process by construction** (one `zeroship serve` child owns
   the file). So P6's stakes are **dev-tier wiring**, not multi-tenant production
   integration — the confinement/cross-process story is far lower-stakes (single
   operator, single local file). The hard security work (authorizer, journal
   immutability, ATTACH isolation) is **already built** in P1–P3b and is what makes
   even the dev path safe.

2. **Recommended integration: a `registerModel`-SQLite arm that drives the engine
   from a descriptor diff at runtime boot — NOT a control-plane deploy step.** The
   dev tier has **no control plane** (`apply_bundle_migrations` is never reached in
   dev — grep-confirmed). The natural, minimal seam is: keep `registerModel` as the
   dev SQLite migrator, but replace `run_sqlite_pipeline`'s bespoke
   bootstrap/plan/validate/apply with a call into the **engine**, generating SQLite
   DDL **at apply time** from the declared descriptor diffed against live
   introspection (the P4 path) — no build-side `generate` required.

3. **The one real engine change: make the declarative apply path backend-generic.**
   The executor (`apply_locked`) is **already** generic over `MigrationBackend`. The
   blocker is the thin layer above it — `MigrationEngine` / `plan_declarative` /
   `apply_declarative` are `compio_postgres::Client`-typed and `plan_declarative`
   fails closed on rebuilds. P6 lifts that one layer to the backend seam (or adds a
   parallel SQLite declarative entry point that reuses the generic executor).

4. **What's deleted:** `run_sqlite_pipeline`, `apply_sqlite`, the SQLite use of
   `bootstrap`/`plan`/`validate`, `SqliteLockGuard` (as a migration lock),
   `DeclarativeError::SqliteRebuildRequired` (the fail-closed stub), and the
   `(_, Some(sqlite)) => run_sqlite_pipeline(...)` arm — replaced by an engine call.
   Per the no-back-compat stance, deleted in the same change.

5. **Top decisions needing user input:** (a) the **dev approval posture** for
   destructive SQLite migrations (auto-approve in dev vs. refuse like prod deploy);
   (b) whether P6 ships the **engine-generate-at-boot** model or pulls in the
   deferred **build-side `generate`** (P6b) so dev mirrors prod's "apply pre-built
   migration files"; (c) whether the dev UX changes (auto-migrate-on-boot stays, vs.
   schema-authority §9's "no more auto-migrate-on-boot" — which targets PROD).

---

## 1. The pivotal scoping question — dev-tier-only, definitively

**SQLite is the dev tier only. It is not a production or multi-tenant backend.**
Evidence (all verified in the worktree):

- **`docs/reference/auth-dev-tier.md`** lists the four primitives' tiers
  explicitly: `env.db` → **PostgreSQL (prod) / SQLite embedded (dev)**. SQLite is
  the named dev-tier peer of redb / LocalFs / the in-process dev-auth provider —
  "a *real* implementation of the DB contract … not a fake Postgres", used so
  "`pnpm dev` runs an app with zero platform infra."

- **Backend selection is purely URL-string-driven**, at runtime, in
  `crates/plugin-db/src/lib.rs::backend_for_url` (≈630–699) →
  `init_pool_async` (≈723–775): `postgres://`/`postgresql://` → `Postgres`;
  `sqlite:`/`sqlite://`/`file:`/`:memory:`/bare-path → `Sqlite { path }`. There is
  **no deploy-time or tenant-level toggle** — whichever URL the process is handed
  decides the backend. The prod worker is handed a Postgres DSN
  (`crates/worker/src/main.rs:73` `--db`/`DATABASE_URL`); the dev `zeroship serve`
  child is handed the dev DB URL by the vite-plugin
  (`sdks/vite-plugin/src/dev-server.ts:95–119` `resolveDatabaseUrl`, default DB
  URL when neither shell nor `.env` sets one).

- **The dev runtime is single-process by construction.** The SQLite migration lock
  is `in-process only` (the parity design §2.3 + the existing `lock.rs`), and
  `crates/zeroship-migrate/src/backend_sqlite/mod.rs:190–198` makes the engine's
  SQLite project-lock methods **honest no-ops** because "the single migration
  actor's single connection serializes structurally — one writer, one flume queue".
  Cross-process serialization is the explicitly-deferred **P5b**; the parity design
  states "Until P5b lands, the engine MUST refuse to run on SQLite when a
  cross-process apply could occur (e.g. multi-worker prod). The Confined dev-tier
  path is single-process by construction." <!-- Round 2 (C1): that "MUST refuse"
  is no longer just an assertion — §1.1 designs the structural guard that ENFORCES
  it (init_pool_async + worker-startup both refuse SQLite outside ZEROSHIP_DEV), so
  the no-op project-lock is unreachable in any multi-process context. -->
  **§1.1 turns this "MUST refuse" from an assertion into an enforced guard.**

- **The schema-authority design** (`2026-06-18-…drizzle-model-design.md` §9/§10/R4)
  treats SQLite as a **dev-tier follow-up** non-goal: "SQLite-backend parity in the
  relocated engine — flagged as a dev-tier follow-up; the engine's PG arm is the
  target." `registerModel` "was the dev migrator" (R4).

**Consequence for P6's surface and stakes:** P6 is a **dev-tier wiring** task. The
production multi-tenant apply path is already done (it is **Postgres**, via
`control/src/deploy_migrate.rs::apply_bundle_migrations` at deploy). P6 does NOT
need cross-process locking (P5b), a control-plane deploy step, or a multi-tenant
role model — SQLite has none of those and never will at the dev tier. The hard
security properties (authorizer line-2, journal immutability, ATTACH isolation) are
already built (P2/P3) and are what make even a single-operator local file safe
against a prompt-injected AI-authored migration. **The "production apply path" the
task names is, for SQLite, the dev-tier apply path** — there is no other.

### 1.1 C1 — dev-only is NOT enforced today; the structural guard

<!-- Added in round 2 (C1 CRITICAL): the original asserted "single-process by
     construction" but designed no guard. A `sqlite:`/`file:` DSN handed to a
     prod multi-replica worker silently selects SQLite, whose engine
     project-lock is a no-op (backend_sqlite/mod.rs:192) — so two replicas could
     apply concurrently with NO cross-process serialization. Designed below. -->

**The gap (grounded).** Backend selection is purely DSN-string-driven and has
**no environment gate**:

- `crates/plugin-db/src/lib.rs:640 backend_for_url` maps any `sqlite:`/`file:`/
  `:memory:`/bare-path to `BackendUrl::Sqlite { path }` regardless of who is
  running. `init_pool_async` (`lib.rs:723`) then opens a `SqliteBackend` with no
  check on the runtime tier.
- `crates/worker/src/cache.rs:124` pushes `DbPlugin::new(url, …)` with whatever
  `DB_URL` the worker holds — unvalidated.
- `crates/worker/src/main.rs:73` takes `--db`/`DATABASE_URL` as a free string.

The engine's SQLite project-lock is an **honest no-op**
(`crates/zeroship-migrate/src/backend_sqlite/mod.rs:192`
`acquire_project_lock → Ok(())`), justified by "single migration actor, single
connection serializes structurally". That justification holds **only** in the
single-process dev runtime. On an N-replica prod worker pool fed a `sqlite:` DSN
(operator misconfig, or a `file:`-pointing env leak), each replica opens its own
`SqliteBackend` on its own (possibly shared-volume) file with a no-op lock —
**concurrent cross-process apply with zero serialization.** That is the C1
hazard, and it is a data-corruption class, not a UX wart.

**The structural guard (designed).** Two complementary fail-closed checks, both
keyed on the **same dev signal** that already gates the dev-auth provider —
`ZEROSHIP_DEV=1` (read today only in `crates/runtime/src/core/dev_auth.rs:54`,
`ENV_DEV`). The worker **never** sets `ZEROSHIP_DEV` (confirmed: the only
worker-injected db-related env var is `ZEROSHIP_DEPLOY_ID`, `cache.rs:286`), so
gating SQLite on `ZEROSHIP_DEV` is fail-closed against the prod worker **by
construction** — no allowlist to maintain.

1. **Primary guard — refuse `Sqlite` in `init_pool_async` unless `ZEROSHIP_DEV=1`.**
   In `crates/plugin-db/src/lib.rs::init_pool_async` (`lib.rs:746`, the
   `BackendUrl::Sqlite { path }` arm at `lib.rs:765`), before
   `SqliteBackend::open`, check `std::env::var("ZEROSHIP_DEV").as_deref() ==
   Ok("1")`. If unset, return a typed config error
   (`DbError::config_hinted("sqlite_requires_dev", …, "SQLite is the dev tier
   only; a prod worker must use a postgres:// DSN")`). This is the single
   structural choke point — **every** backend open funnels through
   `init_pool_async`, so no SQLite backend can be constructed in a non-dev
   process. The same `ZEROSHIP_DEV` read is reused by the apply path (H2, §1.2),
   so the signal is read once at backend construction and the decision is
   stable for the isolate's life.

2. **Defense-in-depth — refuse `Sqlite` at worker startup.** In
   `crates/worker/src/main.rs` (the DSN-parse near `:73`/the boot validation
   block around `:234` where `--dev-insecure` is already evaluated), classify
   `--db` with the same `backend_for_url` grammar and **hard-abort the worker
   process** if it resolves to `Sqlite` (the worker is multi-replica by design;
   SQLite there is always a misconfig). This is a faster, louder failure than the
   per-isolate guard and cannot be bypassed by a per-app env override, because
   the worker process simply refuses to start. It is independent of `ZEROSHIP_DEV`
   precisely because the worker must reject SQLite *even if* someone exported
   `ZEROSHIP_DEV=1` into a prod worker — the worker's identity, not an env flag,
   is the authority here.

The dev signal is `ZEROSHIP_DEV` (the affirmative "this is the self-contained dev
runtime" flag set by the Vite plugin / `zeroship serve`), **not**
`ZEROSHIP_DEV_INSECURE` (which only relaxes the control-plane / worker admin-key
requirement, S1, and is the wrong axis — see H2/§1.2). With guard (1) in place,
the engine's no-op project-lock can never permit concurrent cross-process apply,
because a non-dev process can never hold a `SqliteBackend` at all.

### 1.2 H2 — where the dev signal is read in the *apply* path

<!-- Added in round 2 (H2): the dev approval posture (§4.1) needs ZEROSHIP_DEV,
     but the apply runs in plugin-db's register_model, which today reads only
     ZEROSHIP_DEPLOY_ID. Pin the read site + confirm the worker never sets it. -->

The destructive-approval posture (§4.1) and the C1 guard both need the dev signal
**inside the apply path**, which runs in plugin-db, not the runtime crate. Today:

- `crates/plugin-db/src/register_model/mod.rs:138` reads only `ZEROSHIP_DEPLOY_ID`
  (→ `deploy_id`, audit grouping). It does **not** read `ZEROSHIP_DEV`.
- `ZEROSHIP_DEV` is read **only** in `crates/runtime/src/core/dev_auth.rs:54`
  (`resolve_dev_user_json`, the request path) — a different crate, a different
  call site, never reached from `register_model`.

**Design:** `run_sqlite_via_engine` (§7b) reads `ZEROSHIP_DEV` directly via
`std::env::var("ZEROSHIP_DEV")` at the top of the apply, alongside the existing
`ZEROSHIP_DEPLOY_ID` read, and derives the `Approval` posture from it
(§4.1: `ZEROSHIP_DEV=1` ⇒ `Approval::Approved`; else the path is unreachable
because the C1 guard already refused the backend open). Because this arm is only
ever reached after the C1 guard (§1.1) has confirmed `ZEROSHIP_DEV=1` at backend
construction, the read here is a consistency assertion, not a second trust
decision — but reading it locally keeps the apply self-contained and testable
against a temp file with the env var toggled.

**`ZEROSHIP_DEV` vs `ZEROSHIP_DEV_INSECURE`.** They are orthogonal:
`ZEROSHIP_DEV` asserts *"this is the self-contained single-process dev runtime"*
(the peer of redb/LocalFs/dev-auth); `ZEROSHIP_DEV_INSECURE` asserts *"permit
empty admin/worker keys"* (a control-plane laxity flag, `control/src/main.rs:769`,
`worker/src/main.rs:55`). SQLite gating keys on `ZEROSHIP_DEV` — a prod operator
running `--dev-insecure` to skip key setup must **still** be refused a SQLite
backend, which `ZEROSHIP_DEV` (worker never sets it) correctly enforces and
`ZEROSHIP_DEV_INSECURE` would not.

---

## 2. The current SQLite schema-application flow (full trace)

### 2.1 Who calls `registerModel` for SQLite, and when

- **Source of the call:** the JS bootstrap. `export default { schema }` →
  `@zeroship/bootstrap` `installSchema(schema, env.db, …)` plants typed Collection
  wrappers and chains `registerModel(collection, schema, indexes)` per collection
  (`sdks/bootstrap/src/runtime-entry.ts` for prod-shaped runtime;
  `sdks/bootstrap/src/dev-entry.ts` for the dev runtime — "Lazy schema install via
  `installSchema(schema, env.db)` on first request"). The native side is
  `register_model_dispatch` (`crates/plugin-db/src/register_model/mod.rs:68`).

- **When (dev):** at **runtime boot / first request** of the `zeroship serve` dev
  child — NOT a deploy step. The dev tier has **no control plane**: grep confirms
  `apply_bundle_migrations` / `deploy_migrate` / `zeroship-migrate` are **never**
  referenced from `crates/cli`, `crates/runtime`, or `sdks/vite-plugin`. The dev
  child runtime's `registerModel` chain is the **only** SQLite migrator. This is
  "auto-migrate-on-boot".

- **The migration SOURCE today:** the declared **`default.schema`** (the descriptor
  JSON installed via `installSchema`). `run_sqlite_pipeline` diffs that declared
  schema against live introspection and applies the additive ops. There is **no
  versioned migration file, no journal** — it is a stateless declared-vs-live diff
  each boot.

### 2.2 The SQLite pipeline body (`run_sqlite_pipeline`)

`register_model/mod.rs:292–520`:

```
run_sqlite_pipeline(sqlite, app_id, collection, schema, indexes, deploy_id):
  reject_cross_app_fk
  strictness = schema._meta.strictness (default "strict")
  SqliteLockGuard::acquire(GlobalApp{app_id, LOCK_TAG})   ← in-process lock
  bootstrap::build_ctx(...)                                 ← RegisterContext
  plan::compute_plan(...)                                   ← diff declared vs live
  validate::validate(...)                                   ← strict refuses destructive
  apply_sqlite(...)                                         ← iterate ops, SKIP Destructive
```

`apply_sqlite` (347–520): per diff op, **skips `ChangeClass::Destructive`**, writes
an audit row, applies via `backend.exec_batch` (CreateTable/AddColumn/FK) or the
typed index builders (`create_index_with_recovery` / `ensure_vector_index` /
`ensure_fts_index` / `ensure_spatial_index`); `MaskBackfill/MaskRewrite/MaskRemove/
RewriteColumnType` → `backend_unsupported`; `DropColumn/DropIndex` → skipped. So
today's SQLite apply is **best-effort additive only**: no journal, no versioning, no
rollback, no destructive handling, no checksum/drift, no two-phase recovery (the
parity design §1.3 says the same).

### 2.3 Contrast with the PG path

`register_model/mod.rs:181–195` is the cutover dispatch:

- **PG arm** `(Some(_pg), _) => Ok(())` — a **no-op**. The engine
  (`control/src/deploy_migrate.rs::apply_bundle_migrations`) is the PG schema
  authority and runs the versioned migrations **at deploy, before go-live**, under
  the Confined profile + `migrator_<app_id>` role. `registerModel` on PG issues NO
  DDL; the dispatch caller still stamps readiness (`mark_model_registered`) + the
  declared cache (`cache_schema`) so the P4 introspection metadata contract holds.

- **SQLite arm** `(_, Some(sqlite)) => run_sqlite_pipeline(...)` — **unchanged
  runtime auto-migrate**, because (per the comment at 172–180) "the engine has NO
  SQLite apply path yet". P6 is the phase that builds that path.

So the asymmetry P6 removes is: **PG = engine-at-deploy, SQLite = bespoke
runtime-diff**. After P6, SQLite is **engine-at-boot** (the dev analog of
engine-at-deploy).

---

## 3. The engine apply path's genericity — what exactly blocks SQLite

### 3.1 What is already generic (done in P1)

`executor::apply_locked` (`executor.rs:851`) and its lock/session shell
`apply_with_lock_backend` (690) are **already `<B: MigrationBackend>`** generic. The
whole orchestration — partition versioned/repeatable, drift/tamper gate,
squash/expand gates, `order_pending`, FIRST/SECOND pass, repeatable phase — routes
every dialect-coupled leaf through the trait. `SqliteBackend`
(`backend_sqlite/mod.rs:182`) is a **complete `MigrationBackend` impl** (lock no-ops,
journal I/O over `_mig`, drift snapshot, confined transactional apply, `validate_non_txn`
rejecting `transaction:false`, additive rollback, the 12-step `rebuild_one` seam).

**So the versioned executor already runs on SQLite today** — `executor::apply` is
PG-only by its `&Client` signature, but `apply_with_lock_backend` is the generic
core, and `SqliteBackend::apply_one_additive` / the trait methods are exercised
directly by P2/P3 tests. There is no forked executor.

### 3.2 What is PG-typed and blocks the declarative/engine surface

The thin **public** layer above the generic executor is still `compio_postgres::Client`-typed:

- `MigrationEngine::apply` / `apply_with_lock` / `apply_verified` (`engine.rs:308–665`)
  take `conn: &Client`.
- `MigrationEngine::apply_declarative` / `apply_declarative_verified` /
  `apply_declarative_locked` (308–470) take `conn: &Client` and call
  `executor::acquire_project_lock_outer(conn, …)` (the PG `pg_advisory_lock`
  free-fns), plus `run_expand` / `run_backfill` (PG-shaped).
- **`plan_declarative` FAILS CLOSED on SQLite rebuilds** (`engine.rs:242–255`): if
  `diff.rebuilds` is non-empty it returns `DeclarativeError::SqliteRebuildRequired`
  with the comment "no SQLite engine apply path / approval gate is wired yet (P6).
  Refusing to drop them silently." **This is the literal P6 marker.**

The PG-specific surface that blocks genericity is small and **already abstracted by
the trait**:

| PG-typed surface (engine layer) | Trait method that already covers it |
| --- | --- |
| `acquire_project_lock_outer(conn, …)` (`pg_advisory_lock`) | `MigrationBackend::acquire/release_project_lock` (SQLite = no-op) |
| `executor::apply_with_lock(conn, …)` | `apply_with_lock_backend(backend, …)` (already generic) |
| GUC/role confinement (`SET LOCAL ROLE`, `search_path`) | `snapshot/restore_session` + `apply_up_transactional` (SQLite = authorizer mode) |
| `run_backfill(conn, …)` (PG `pg_advisory_xact_lock`, paged UPDATE) | **NOT yet abstracted** — see §3.3 |
| `run_expand` (E1/E2 trigger + backfill + E3) | composed of the above; SQLite triggers exist, backfill is the gap |

### 3.3 What it would take to drive `SqliteBackend` through the engine

Two viable shapes; recommend **(A)** for the dev-tier scope, with **(B)** noted:

**(A) Lift the engine layer to the backend seam (the clean, single-sourced option).**
Make `MigrationEngine::apply*` generic over `B: MigrationBackend` (mirroring what P1
already did to `apply_locked`), replacing the `executor::*_outer(conn,…)` free-fns
with `backend.acquire/release_project_lock` and `executor::apply_with_lock` with
`apply_with_lock_backend`. For the **plain additive + drop-gated declarative path**
(scenarios 1–3, 7, 8 — create table, add column, drop) this is a near-mechanical
lift: every leaf already has a trait method. The `plan_declarative` rebuild
fail-close (`engine.rs:242`) becomes: **carry `diff.rebuilds`** into the SQLite plan
and drive them through `SqliteBackend::rebuild_one` (the built, tested 12-step
rebuild seam) under the destructive/approval gate.

The **only genuinely-missing piece** for full parity is the **online expand-contract
backfill** (`run_backfill` / `run_expand`): `backfill.rs` uses PG
`pg_advisory_xact_lock` + paged `UPDATE … RETURNING`. For the **dev tier this is
out of P6's critical path** — dev is single-process, and online zero-downtime
expand-contract is a prod-traffic concern. Recommendation: P6 wires the **plain +
rebuild** declarative path generically; **defer SQLite online expand-contract**
(declarative renames via dual-write+backfill) to a later sub-phase, and have
`plan_declarative` on SQLite either (i) author renames as a **rebuild** (offline
column rename, valid at dev scale) or (ii) keep `SqliteRebuildRequired`'s sibling
error **only** for the online-backfill case, not for plain rebuilds. (This is an
open decision — §8 Q3.)

**(B) A parallel `apply_declarative_sqlite` entry that reuses the generic executor.**
A thinner, lower-blast-radius alternative: leave `MigrationEngine` PG-typed, add a
small SQLite-specific orchestrator (in `engine.rs` or a new `engine_sqlite.rs`) that
calls the **already-generic** `apply_with_lock_backend` + `SqliteBackend::rebuild_one`
directly, skipping the PG-only `*_outer` lock dance (no-op on SQLite anyway). This
avoids touching the heavily-tested PG `MigrationEngine` methods at all. **Downside:**
a second declarative orchestrator to keep in sync (the design's whole point was "no
forked executor"). **(A) is preferred** unless the PG-method genericization proves
to disturb the PG regression bar; the trait already makes (A) mostly mechanical.

---

## 4. Destructive-approval gate, journal, drift, locking for SQLite/dev

### 4.1 Approval gate — the key dev-UX decision

The PG path passes **`Approval::None`** at deploy
(`deploy_migrate.rs:170`), so a destructive migration is **refused at deploy** and
goes through the out-of-band `submit_migration` / expand-contract surface. The
executor's own defense-in-depth gate (`apply_with_lock` `executor.rs:673`) refuses a
destructive batch without `Approval::Approved` regardless of dialect — and
`SqliteBackend::rebuild_one`'s doc explicitly says "callers MUST treat it as ungated
and gate approval themselves" (a rebuild on a table with data is destructive).

For the **dev tier** the question is posture. Two options:

- **(i) Dev-relaxed auto-approve (recommended for boot-time auto-migrate):** in dev,
  `registerModel`→engine passes `Approval::Approved` so an iterating developer's
  destructive schema edit (drop a column, narrow a type) just applies — matching
  today's behavior is *additive-only-skip*, but the developer expectation in dev is
  "my schema edit takes effect." This is **safe** because: dev is a local file the
  operator owns, the authorizer still confines the SQL, and there is no other tenant
  to harm. **But it is a behavior change** the user should sign off on (today
  destructive ops are silently skipped on SQLite; auto-approve makes them *apply*).

- **(ii) Refuse destructive like prod (`Approval::None`):** keeps the dev/prod gate
  identical and forces destructive changes through an explicit path. More faithful,
  but worse dev ergonomics (a `DROP COLUMN` in `schema.ts` would error on boot
  rather than just work). 

**Recommendation:** **(i) dev-relaxed auto-approve**, gated on the dev-only signal
(`ZEROSHIP_DEV=1`, the same flag that gates `dev_auth::resolve_dev_user_json`), with
the engine's authorizer + journal as the safety net — but flag this to the user as a
product-behavior change (§8 Q1).

### 4.2 Journal, drift, lock mapping

- **Journal:** the SQLite `_mig` attached-file journal is **already built** (P2,
  `backend_sqlite/journal_sql.rs`, shared monotonic `event_seq`, immutability by
  authorizer + triggers). Wiring `registerModel`→engine means dev **gains a real
  versioned journal** it does not have today (an upgrade, not a risk).

- **H3 — auto-baseline on an existing, journal-less dev file.** <!-- Added in
  round 2 (H3): existing run_sqlite_pipeline-populated dev files have tables but
  no _mig journal; first engine boot must not drift-abort. --> A dev developer
  who ran the *old* `run_sqlite_pipeline` has a `zs-default.sqlite` with tables
  but **no `_mig` journal** (the old path was a stateless diff, §2.2). The first
  engine boot against that file must **not** collide or drift-abort. The engine
  already has the right primitive: `baseline` (`zeroship-migrate/src/baseline.rs`,
  `pub use baseline::{baseline, BaselineOutcome}`; journal kind `'baseline'`,
  `journal_sql.rs:83`), a **first-entry** operation that records an existing
  schema as applied **without running its `up`** (`journal.rs:70`,
  "the `up` was recorded NOT run"). **Design (step 2 of §7b.2):** after
  `ensure_journal_sqlite` (idempotent), `run_sqlite_via_engine` checks
  `applied_sqlite()` — if the journal has **zero** entries **and** the app file is
  **non-empty** (`snapshot_schema_sqlite` finds creator tables), it calls
  `baseline` to record the live schema as the baseline entry **before** planning
  the declared diff. The subsequent `plan_declarative(Sqlite)` then diffs the
  declared descriptor against that same live snapshot and applies only the
  *additional* ops — so a warm file with tables-but-no-journal is adopted, not
  re-created or drift-rejected. A **fresh** file (no tables, no journal) skips
  baseline and the first `apply` creates everything. This is specified into the
  P6b phase plan (P6b-5), not left as "considered".

- **Drift:** `SqliteBackend::snapshot_schema` + the dialect-agnostic
  `check_checksum_drift` are built (P5/§2.7). The engine's drift/tamper gate runs
  unchanged through the trait.

- **Locking:** the SQLite project-lock methods are **honest no-ops** (single-actor
  serialization is the lock). For **dev single-process this is correct and
  sufficient** — confirmed: the design defers cross-process to P5b and asserts the
  dev path is single-process by construction. P6 does **not** need P5b.

---

## 5. Where SQLite migrations come from in the new model

The P4 descriptor→DDL routing generates SQLite DDL from a **descriptor diff vs live
introspection**. The question: does P6 require a build-side `generate` (schema.ts →
migration files in the `.zship`), or can the engine generate-and-apply at boot from
the descriptor + live introspection?

**Recommendation: engine-generate-at-apply (the diff path) for P6. Build-side
`generate` is NOT required for P6 and stays deferred (P6b).**

Reasoning:

- The dev tier already feeds `registerModel` the **declared descriptor**
  (`default.schema`), and the engine's declarative author (`plan_declarative` →
  `DeclarativeAuthor::diff`) is **exactly** a "desired descriptor vs live snapshot →
  migrations" function. `SqliteBackend::snapshot_schema` supplies the live side. So
  the engine can author + apply at boot with **no new artifact** — it replaces
  `run_sqlite_pipeline`'s bespoke `plan::compute_plan` with the engine's own
  descriptor diff, then drives the generic executor.

- A build-side `generate` (versioned files in the bundle) is the **prod** model
  (`deploy_migrate` loads files via `load_dir`). Pulling it into dev would mean dev
  also ships migration files — a larger UX shift (schema-authority §9's
  `zeroship-migrate generate` + `migrate` dev loop). That is a **coherent eventual
  end state** but it is a **separate, larger change** than "retire
  `run_sqlite_pipeline`", and it touches the vite-plugin build + CLI. Keep it as the
  optional **P6b**; let the user choose whether P6 includes it (§8 Q2).

The minimal coherent P6: **`registerModel`-SQLite-arm → engine descriptor-diff →
generic executor → `_mig` journal**, generating DDL at boot. Same trust model
`registerModel` already uses (validated descriptor, never raw creator SQL), now with
versioning/journal/drift/destructive-handling the old path lacked.

---

## 6. What gets DELETED and the blast radius

Per AGENTS.md no-back-compat: the old path is deleted in the **same change**.

**Deleted from `crates/plugin-db/src/register_model/`:**

- `run_sqlite_pipeline` (`mod.rs:292–345`).
- `apply_sqlite` (`mod.rs:347–520`) + `refreshes_sqlite_cdc_name_cache` helper.
- The `(_, Some(sqlite)) => run_sqlite_pipeline(...)` dispatch arm (`mod.rs:189–191`),
  replaced by `(_, Some(sqlite)) => run_sqlite_via_engine(...)`.
- The SQLite use of `bootstrap`/`plan`/`validate` for migration (the engine's
  declarative author replaces `plan::compute_plan` + `validate::validate` for
  SQLite). **Caution:** `bootstrap`/`plan`/`validate` are **shared with the PG
  test-path** (`run_pipeline`) and the audit machinery — delete only the SQLite
  *callers*, keep the modules (or assess whether PG still needs them post
  schema-authority cutover; PG's `run_pipeline` is itself only reached by
  `exec_register_model_with_pool`, a `#[cfg(feature = "test-helpers")]` path — **see
  the M3 decision in §7c**: P6b-6 deletes the SQLite *callers*; the modules go in a
  dedicated PG-test-infra follow-up).
- `SqliteLockGuard` **as the migration lock** (the engine's single-actor
  serialization replaces it). Confirm no non-migration caller of `SqliteLockGuard`
  remains before deleting the type.

**Deleted from `crates/zeroship-migrate/src/`:**

- `DeclarativeError::SqliteRebuildRequired` and the fail-closed block in
  `plan_declarative` (`engine.rs:242–255`) — replaced by carrying rebuilds into the
  SQLite apply path.
- The "no SQLite engine apply path / approval gate yet (P6)" caveats in
  `backend_sqlite/mod.rs:136–145` (the `rebuild_one` C1 note) and `engine.rs:233–241`.

**What currently depends on the deleted SQLite path (blast radius):**

- **Tests:** `register_model/mod.rs:598–702`
  (`sqlite_register_model_refreshes_cdc_column_name_cache_after_add_column`) calls
  `run_sqlite_pipeline` + `apply_sqlite` directly — must be rewritten to drive the
  engine path (and the CDC-cache-refresh behavior must be preserved: the new engine
  apply must still `invalidate_cdc_name_cache` after CreateTable/AddColumn, OR that
  invalidation moves to the data-plane backend — §8 Q5). `crates/plugin-db/tests/
  sqlite_integration.rs` likely exercises the auto-migrate path. The
  schema-authority-e2e capstone is **PG-only** (no SQLite e2e), so no e2e regression
  there.
- **The dev tier** (`@zeroship/bootstrap` dev-entry → `installSchema` →
  `registerModel`): the JS surface is **unchanged** — `registerModel` keeps the same
  signature and still resolves a Promise. Only the Rust arm behind it changes. This
  is the important containment: **P6 does not reach into the vite-plugin or
  dev-bootstrap JS** if we keep `registerModel` as the seam. (It *would* if we chose
  the build-side `generate` P6b model — that is the larger UX option, §8 Q2.)
- **The mask/encryption data-plane:** `apply_sqlite` already refuses
  `Mask*`/`RewriteColumnType` (`backend_unsupported`). The engine's rebuild
  (`rebuild_one`) actually *handles* the type rewrite — so P6 turns a previously
  unsupported op into a supported one. The AEAD/mask **transform** stays in plugin-db
  (data-plane); only the DDL ownership moves (parity design §2.6).

**Honest blast-radius assessment:** **contained to engine + plugin-db Rust**, with
test rewrites. It does **not** reach the vite-plugin / dev-bootstrap / worker boot
**iff** we keep `registerModel` as the dev migrator seam (recommended). It **does**
reach them if the user wants the build-side `generate` dev loop (P6b) — that is a
deliberate, separable expansion.

---

## 7b. P6b two-connection coordination — the design (C2/C3, M1/M2, H3)

<!-- Added in round 2 (C2/C3 CRITICAL, the crux). The original left the two
     SqliteBackend connections as "verify no two-open-writer hazard" (R4) and
     "the readiness gate MAY already enforce ordering (verify)" — hand-waving.
     This section traces the boot lifecycle and DESIGNS the single-owner window,
     the cache invalidation, and the ordering barrier concretely. -->

> **Reading order:** this section (§7b) is the conceptual core P6b's phase plan
> (§7, just below) references. It is placed first because §7's P6b-2..6 steps each
> point back here. Skim §7b.2/§7b.4/§7b.5 (the a/b/c of the two-connection
> problem), then read §7 for the buildable decomposition.

### 7b.0 The two connections, grounded

| | Data-plane **A** | Migration **B** (post-P6b) |
| --- | --- | --- |
| Type | `plugin_db::backend::sqlite::SqliteBackend` | `zeroship_migrate::backend_sqlite::SqliteBackend` |
| Opened by | `init_pool_async` (`lib.rs:765`) | `run_sqlite_via_engine` (new, §7b.3) |
| Connection | one `SqliteSession` actor (`mod.rs:140`), control session at the DSN file | one hardened `MigrationActor` (`backend_sqlite/actor.rs:147`) |
| Per-app file | ATTACHes `zs-<app_id>.sqlite` lazily via `ensure_app_schema` (`bootstrap.rs:237`) | opens `app_path` (= `zs-<app_id>.sqlite`) as `main`, journal as `_mig` |
| Caches | CDC name cache `Rc<RefCell<HashSet<(app,col)>>>` (`mod.rs:142`), `app_id_cache` (`mod.rs:144`), SQLite per-conn schema cookie / prepared stmts | none (migration actor is stateless across applies; CDC-free by design) |

**Both touch the same app file** — `zs-<app_id>.sqlite`. The original design left
their coexistence as "verify". It is now designed below.

### 7b.1 M1/M2 — the concrete dev `app_id` and file paths (traced, not assumed)

<!-- Added in round 2 (M1/M2): the original assumed per-app zs-<app_id>.sqlite
     ATTACH; the vite default is a single file sqlite:.zeroship/dev.sqlite. Both
     are true at once — traced below. -->

The dev `app_id` is the **literal string `"default"`**. Traced:
`crates/runtime/src/core/plugin.rs:225-228` resolves `app_id_for_instance` from
`SharedState.env_vars["APP_ID"]` and `.unwrap_or_else(|| "default".to_string())`.
In the self-contained dev runtime nothing injects `APP_ID` (the vite-plugin
dev-server sets `DATABASE_URL` but no `APP_ID`; the worker — which *would* inject
a real `app_<uuid>` — is not in the dev path). So `Db::app_id == "default"`, and
every `registerModel` / CRUD call carries `app_id = "default"`.

The dev DSN is `sqlite:.zeroship/dev.sqlite` (`sdks/vite-plugin/src/dev-db.ts:13`).
Resolving that through `backend_for_url` → `init_pool_async` → `SqliteBackend::open`
(`mod.rs:309 open_blocking`): the path `.zeroship/dev.sqlite` is a **file, not a
dir**, so the control session opens at `.zeroship/dev.sqlite` directly and
`db_dir = .zeroship/`. The per-app data lives in a **different** file ATTACHed
lazily: `db_dir.join("zs-default.sqlite")` = **`.zeroship/zs-default.sqlite`**
(the `ensure_app_schema("default")` path). So the two "contradictory" facts
reconcile cleanly:

- **`.zeroship/dev.sqlite`** = the data-plane **control session** file (audit
  table, schema_version, CDC dispatcher home). NOT the app's tables.
- **`.zeroship/zs-default.sqlite`** = the app's actual collections (the file the
  migration backend must own as `main`).

**Therefore the migration backend B opens, in dev:**

```
app_path     = <db_dir>/zs-default.sqlite        // = .zeroship/zs-default.sqlite
journal_path = <db_dir>/zs-default.migrations.sqlite
```

where `<db_dir>` is taken from the **data-plane backend A**'s `db_dir()` accessor
(`mod.rs:220`, already `pub(crate)`) — NOT re-derived from the DSN, so A and B are
guaranteed to agree on the directory. The `app_id` (`"default"`) and the
`zs-<app_id>.sqlite` naming are the *same* mapping A uses in
`build_ensure_app_schema` (`backend/sqlite/dialect.rs:61`); `run_sqlite_via_engine`
constructs B's paths from `(backend_a.db_dir(), app_id)` so the two can never
diverge. This resolves R4's "construct B from the same `app_id`→file mapping A
uses" as a concrete two-line path construction, not an aspiration.

> The migration backend's rustdoc (`backend_sqlite/mod.rs:52`) already names
> `journal_path` as `<app>.migrations.sqlite` — we adopt that verbatim:
> `zs-default.migrations.sqlite` beside the app file.

### 7b.2 (a) Single-owner window — A and B do **not** apply concurrently

<!-- Added in round 2 (C2 part a): which backend owns the file during migration. -->

The decisive design choice: **the migration (B) runs to completion before the
data-plane (A) ever ATTACHes or serves CRUD on `zs-default.sqlite`.** This is
achievable because of *when* `registerModel` runs relative to the data plane
opening the app file.

Boot lifecycle, traced:

1. Module evaluation plants Collection wrappers synchronously
   (`runtime-entry.ts:106` / dev `dev-entry.ts`); the **DDL chain is deferred** —
   stashed on `__zsSchemaReady` (prod) or `schemaReady` (dev), *not* awaited at
   eval time (`runtime-entry.ts:112-114`).
2. The first request enters the dispatcher. **Prod:** `dispatcher.ts:109-111`
   `await __zsSchemaReady` before running any procedure. **Dev:**
   `dev-entry.ts:178-188` `loadNormalized` → `maybeRegisterSchema` →
   `await schemaRegistration` runs **before** `normalizeUserModule`, and
   `dev-entry.ts:269-279` additionally `await schemaReady` before dispatch. So
   **no creator handler — and thus no `env.db` CRUD — runs until the
   `registerModel` chain has resolved.**
3. `registerModel`'s chain is `register_model_dispatch`
   (`register_model/mod.rs:90-114`): the async op runs `exec_register_model`, and
   `mark_model_registered` + `cache_schema` are stamped **inside** the resolved
   `Ok(())` arm, i.e. the JS promise (`installSchema`'s `ready`) resolves only
   *after* the Rust apply returns.

**The key insight A→B ordering exploits:** in the current code, A's per-app file
is first touched by `ensure_app_schema("default")` **inside `build_ctx`**
(`bootstrap.rs:237`), which `run_sqlite_pipeline` calls. After P6b,
`run_sqlite_via_engine` controls that ordering explicitly. We make the migration
the **first** thing that touches `zs-default.sqlite`:

```
run_sqlite_via_engine(app_id="default", …):
   1. open migration backend B on (db_dir/zs-default.sqlite, …migrations.sqlite)
   2. ensure_journal + baseline-if-needed (H3, §4.2)
   3. plan_declarative(Sqlite) + apply via the generic executor   ← B owns the file
   4. close/drop B  (releases B's main+_mig handles)
   5. A.ensure_app_schema("default")  → ATTACH zs-default.sqlite   ← A opens AFTER B is gone
   6. A.invalidate_cdc_name_cache(...) for the changed collections (§7b.4)
```

Because step 5 (A's first ATTACH of the app file) happens **after** step 4 (B
dropped), there is a **single-owner window**: B owns the file during DDL, then A
opens it for CRUD. They never hold concurrent write connections on
`zs-default.sqlite`. This dissolves R4's "two-open-writer hazard" structurally
rather than relying on WAL to make concurrent writers safe.

> **Why not let A also own `_mig`/journal?** B is the *hardened* actor (authorizer
> line-2, journal immutability, ATTACH isolation, §1/P2-P3). The journal and DDL
> MUST run on B. A is the CRUD actor and is deliberately CDC-armed and
> *un*-hardened. Keeping them separate is a security invariant (R4), not an
> accident — so "one connection drives both" (the alternative in §7b.6) is
> rejected for the apply, and we instead sequence them.

**Subtlety — does A's control session (`dev.sqlite`) coexist with B?** Yes, but
harmlessly: A's control session (`.zeroship/dev.sqlite`) and B's files
(`zs-default.sqlite` + `.migrations.sqlite`) are **disjoint files**. A's control
session is open the whole time (it holds audit + schema_version + the CDC
dispatcher), but it does **not** have `zs-default.sqlite` ATTACHed until step 5.
The only shared file — the app file — has the single-owner window above.

> **`ensure_audit_table` / `next_schema_version` (today in `build_ctx`).** These
> write to A's control session, not the app file. In the engine model the journal
> (`_mig` on B) is the migration record of authority; the legacy audit table on A
> becomes vestigial for SQLite DDL (the engine journals on B). §7b.3 keeps A's
> `ensure_app_schema` (it creates the ATTACH + the empty file if absent) but drops
> the SQLite use of `ensure_audit_table`/`next_schema_version` for *migration*
> bookkeeping — those belonged to `run_sqlite_pipeline`'s `build_ctx`, now deleted.

### 7b.3 (a cont.) Who creates the empty app file — ordering of `ensure_app_schema`

A nuance the single-owner window must handle: today `ensure_app_schema` both
**creates** `zs-default.sqlite` (if absent) **and** ATTACHes it to A. B's
`MigrationActor::open` also `CREATE`s `app_path` if absent (rusqlite opens
create-by-default). On a **fresh** dev project neither file exists; on a **warm**
project (H3) `zs-default.sqlite` exists from a prior `run_sqlite_pipeline` run.

Design: **B opens the app file first** (step 1) — B is the creator-of-record for
a fresh file, and the opener-of-existing for a warm file. A's `ensure_app_schema`
(step 5) then only ATTACHes (the file already exists). This is safe because
`ensure_app_schema`'s SQL is `ATTACH '…/zs-<app>.sqlite' AS "<app>"` plus
idempotent namespace setup (`dialect.rs:61`) — it does not assume it created the
file. The `app_id_cache` (`mod.rs:144`) dedup on A is unaffected: it guards a
second ATTACH of the same alias, orthogonal to who created the file.

### 7b.4 (b) Post-apply cache invalidation across the two connections

<!-- Added in round 2 (C2 part b): the exact caches on A to refresh after B's
     batch DDL, the set of collections touched, and the mechanism. -->

After B commits DDL on `zs-default.sqlite` and is dropped, A opens the file fresh
(step 5). Because **A's connection to the app file is opened AFTER B's DDL
commits** (single-owner window), A's *connection-level* SQLite caches —
prepared-statement cache, schema cookie (`PRAGMA schema_version` / `sqlite_master`
re-read) — start clean: a freshly-ATTACHed database has no stale prepared
statements and SQLite re-reads `sqlite_master` on first access. **So the
two-connections-to-one-file schema-cookie staleness the critic flagged (C2) does
not arise for the app file**, precisely because we sequenced A to open after B
closed rather than keeping both open across the DDL.

What does **not** auto-clear is A's **application-level** CDC name cache, which is
a `Rc<RefCell<HashSet<(app_id, collection)>>>` on the backend struct (`mod.rs:142`),
independent of any connection and surviving the ATTACH. Today
`run_sqlite_pipeline` invalidates it inline per-op (`register_model/mod.rs:467-469`,
`invalidate_cdc_name_cache` after CreateTable/AddColumn). After P6b the DDL runs
on **B**, which has no such cache, so the invalidation must be **bridged to A**.

**Which collections changed (the batch problem).** The engine apply is now a
**batch** (`plan_declarative` returns a plan over potentially many collections),
not the one-op-at-a-time loop `apply_sqlite` ran. plugin-db must learn the *set*
of collections whose column shape changed to invalidate the right CDC entries.
Design:

- `run_sqlite_via_engine` already holds the engine `Plan` it passed to
  `apply_declarative`. The plan's ops carry their target collection (the same
  `op.collection` the old `apply_sqlite` read, `register_model/mod.rs:361`). Derive
  `changed: HashSet<String>` = the collections of every op whose `change_kind`
  affects column names — `CreateTable | AddColumn | RewriteColumnType | <rebuild>`
  (a rebuild rewrites the table, so its columns are new to the CDC decoder). This
  mirrors today's `refreshes_sqlite_cdc_name_cache` predicate
  (`register_model/mod.rs:467`), now applied to the engine plan instead of the
  validated-op loop.
- After step 4 (B dropped) and step 5 (A re-ATTACHed), call
  `backend_a.invalidate_cdc_name_cache("default", &col)` for each `col in changed`
  (`mod.rs:227`). The publisher loop then re-reads column names before decoding
  the next CDC event for those `(app, collection)` pairs — exactly the
  data-plane-correctness behavior the old inline path gave, now batch-driven.

`run_sqlite_via_engine` holds **both** handles (the data-plane `backend_a` it was
handed, and the migration `backend_b` it constructs), so it is the natural bridge
— the two `SqliteBackend` types live in different crates and cannot reach each
other, but the plugin-db arm sees both. (This is exactly R3's "the plugin-db arm
holds both handles and bridges", now made concrete with the batch-derived
`changed` set.)

> **`app_id_cache` (`mod.rs:144`).** Does NOT need invalidation — it is a dedup of
> ATTACH calls, and a successful re-ATTACH at step 5 inserts `"default"` into it
> the normal way. No migration-driven staleness.

### 7b.5 (c) C3 — the explicit ordering barrier (B completes AND A re-syncs before first CRUD)

<!-- Added in round 2 (C3 CRITICAL): mark_model_registered is a mark not a
     barrier; SchemaPendingGuard gates the broker (subscribe), not CRUD. Designed
     a real barrier and proved it holds across the two connections. -->

The critic correctly observed that the existing primitives are **not** a barrier:

- `mark_model_registered` (`context.rs:522`) is a thread-local *set insert* — it
  records "this isolate has registered this model", read by `is_model_registered`
  to skip re-DDL and by `runtime_schema_for` to gate introspection. It does not
  *block* anything.
- `SchemaPendingGuard` / `is_schema_pending` (`broker.rs:482`) gates
  **`try_subscribe`** (change-stream subscriptions) only — it makes `subscribe`
  fail loudly during a schema-pending window. It does **not** gate `find` /
  `insert` / `update` CRUD.

So neither, alone, guarantees "B's DDL is done and A has re-synced before the
first `env.db.users.find()` runs."

**The barrier we rely on is the `installSchema` promise chain — and it is a real
barrier, proven across both connections:**

1. `run_sqlite_via_engine` is `async` and runs **entirely** inside
   `exec_register_model` (`register_model/mod.rs:91`), whose `await` the
   `register_model_dispatch` spawned-op holds. The op resolves the JS promise
   (`installSchema`'s `ready`) **only after** `run_sqlite_via_engine` returns
   `Ok(())`. Because steps 1-6 of §7b.2 are all `await`ed *inside*
   `run_sqlite_via_engine` — including step 4 (B dropped), step 5 (A re-ATTACH),
   and step 6 (CDC invalidation) — the JS `ready` promise cannot resolve until
   **B has committed and been dropped AND A has re-ATTACHed AND A's CDC cache is
   invalidated**. The barrier therefore spans *both* connections, not just B.
2. No creator CRUD can run before `ready` resolves: prod awaits `__zsSchemaReady`
   in the dispatcher (`dispatcher.ts:109-111`); dev awaits `schemaRegistration`
   then `schemaReady` before any handler (`dev-entry.ts:188`, `:277-279`). A
   creator `fetch`/`rpc` handler is the *only* place `env.db` CRUD originates, and
   it runs strictly after that await.

This is a **structural happens-before**: `run_sqlite_via_engine` returns →
`ready` resolves → dispatcher's `await` completes → handler runs → first CRUD.
The two-connection re-sync (steps 4-6) is *inside* the first link, so it is
ordered before the last. **The barrier already exists in the promise topology; we
make it cover the re-sync by doing the re-sync inside the awaited apply.** No new
barrier primitive is required — but P6b MUST keep steps 4-6 *inside*
`run_sqlite_via_engine`'s awaited body (not spawned/detached), or the guarantee
breaks. That is the load-bearing constraint, called out for the implementer and
the test (§7 P6b gate).

> **Belt-and-suspenders (optional, recommended).** Wrap the whole
> `run_sqlite_via_engine` body in `engage_schema_pending("default")` …
> `disengage_schema_pending` (`broker.rs:758/768`, reachable via the data-plane
> handle's `engage_schema_pending`, `cdc.rs:780`). This does **not** gate CRUD
> (only `subscribe`), but it correctly refuses change-stream subscriptions during
> the apply window and pushes a `Resync` per active subscription on disengage —
> so a dev app that subscribed in a *previous* hot-reload cycle re-syncs cleanly
> after a schema edit. It is not the CRUD barrier (the promise chain is), but it
> closes the subscribe-during-apply hole for free using an existing primitive.

### 7b.6 Alternatives considered (and rejected) for the two-connection problem

<!-- Added in round 2: the critic asked to evaluate simpler shapes that dissolve
     the problem. Done. -->

- **(Alt-1) Run the migration BEFORE A is opened at all.** This is essentially
  what §7b.2 does (B opens/owns the app file first, A ATTACHes after), but note A's
  *control session* (`dev.sqlite`) is still opened earlier by `init_pool_async`
  (the lazy-init in `exec_register_model`, `mod.rs:128-133`). We cannot fully defer
  A's open because `run_sqlite_via_engine` needs A's `db_dir()` to construct B's
  paths (§7b.1). So the realized shape is "A's control session open early (needed
  for db_dir + audit), B owns the *app file* first, A ATTACHes the app file last."
  The app file — the only contended resource — still has a clean single-owner
  window. **Adopted (this is §7b.2).**

- **(Alt-2) One connection drives both CRUD and the engine.** Reuse A's
  `SqliteSession` connection to run the engine DDL, eliminating B entirely. This
  *would* dissolve the cache-coherence question (one connection ⇒ one schema
  cookie ⇒ no cross-connection staleness). **Rejected:** B is the hardened actor
  (authorizer line-2 deny-list, journal immutability, ATTACH isolation, §1). A is
  CDC-armed and intentionally *un*-hardened for CRUD throughput. Running
  AI-authored / declarative DDL on A would bypass the entire P2/P3 security model
  — the whole reason the migration backend exists. The single-owner *window*
  (Alt-1) gives the coherence benefit (A opens clean after B closes) **without**
  surrendering the hardening. **Rejected in favor of §7b.2.**

- **(Alt-3) Keep both A and B open concurrently and rely on WAL + manual
  schema-cookie bump.** This is the shape the critic feared: two writers on one
  file, A's prepared-statement/schema-cookie cache going stale after B's DDL,
  needing an explicit `A.connection.clear_cache()` + re-prepare. **Rejected:** it
  is the most fragile option (SQLite gives no portable "another connection changed
  the schema, drop your cache" signal short of re-opening), and the no-op project
  lock (C1) makes concurrent writers actively unsafe even single-process if a
  second op interleaves. The single-owner window avoids the entire class.

**Recommendation:** §7b.2's single-owner window (Alt-1 realized) + §7b.4's
batch-bridged CDC invalidation + §7b.5's promise-chain barrier. This is buildable
without new primitives and without a product/architecture escalation — see the
closing assessment.

---

## 7. Concrete phase plan (each step independently testable against a temp SQLite file)

<!-- Revised in round 2: P6b is now decomposed into independently-testable steps
     that carry the C1 guard, the §7b coordination, the H3 baseline, and the
     M1/M2 path resolution. -->

- **P6a — Make the engine declarative apply backend-generic; carry rebuilds.**
  *(Critic-accepted; built separately.)* Lift `MigrationEngine::apply` /
  `apply_with_lock` / `apply_verified` and the plain arm of `apply_declarative` to
  `<B: MigrationBackend>` (option A §3.3), replacing `executor::*_outer(conn,…)` +
  `executor::apply_with_lock(conn,…)` with the trait/`apply_with_lock_backend`.
  Replace `plan_declarative`'s `SqliteRebuildRequired` fail-close with carrying
  `diff.rebuilds` into a SQLite plan that drives `SqliteBackend::rebuild_one` under
  the destructive/approval gate. **H1 consistency:** SQLite renames are handled by
  `rebuild_one` (P6a) — `plan.renames` is empty for the SQLite dialect (renames
  surface as a rebuild), so P6b never sees a standalone rename op; §7b.4's `changed`
  set treats a rebuild as "columns changed". **Gate:** the full **PG** suite stays
  green (regression bar — the PG `MigrationEngine` methods must be
  byte-behavior-identical), AND a temp-file SQLite test applies create-table +
  add-column + a type-rewrite-rebuild through `MigrationEngine`, with a real `_mig`
  journal `completed` row and a clean re-run no-op.

- **P6b-1 — The C1 dev-only guard (independently testable, ships first).** Add the
  `ZEROSHIP_DEV=1` gate in `init_pool_async`'s `Sqlite` arm (`lib.rs:765`, §1.1
  guard 1) and the worker-startup SQLite refusal (`worker/src/main.rs`, §1.1 guard
  2). **Gate:** a unit test that `init_pool_async` returns
  `sqlite_requires_dev` for a `sqlite:` URL with `ZEROSHIP_DEV` unset and succeeds
  with `ZEROSHIP_DEV=1`; a worker-boot test that a `--db sqlite:…` aborts startup.
  This lands **before** the wiring so the no-op project-lock can never be reachable
  in prod even mid-epic.

- **P6b-2 — `run_sqlite_via_engine` skeleton + path resolution (M1/M2).** Replace
  the `(_, Some(sqlite)) => run_sqlite_pipeline(…)` arm
  (`register_model/mod.rs:189`) with `run_sqlite_via_engine`. Construct B's
  `app_path`/`journal_path` from `(backend_a.db_dir(), app_id)` per §7b.1
  (`zs-<app_id>.sqlite` + `zs-<app_id>.migrations.sqlite`); read `ZEROSHIP_DEV`
  (H2) and `ZEROSHIP_DEPLOY_ID` at the top. **Gate:** a temp-file test with
  `app_id="default"` confirms B opens `zs-default.sqlite` beside A's control
  session and that the paths match A's `ensure_app_schema` target.

- **P6b-3 — The single-owner window + ordering (C2a/C3).** Implement the step 1-6
  sequence (§7b.2): B opens app file → journal+baseline → plan+apply → drop B → A
  `ensure_app_schema` → CDC invalidate, **all inside the awaited body**. **Gate:**
  a test that asserts B is dropped before A ATTACHes (no concurrent app-file
  writers), and an ordering test that `installSchema`'s `ready` does not resolve
  until after A's re-ATTACH (instrument the sequence).

- **P6b-4 — Batch CDC invalidation bridge (C2b/R3).** Derive the `changed`
  collection set from the engine plan and call
  `backend_a.invalidate_cdc_name_cache` per collection after the apply (§7b.4).
  **Gate:** rewrite `sqlite_register_model_refreshes_cdc_column_name_cache_after_add_column`
  (`register_model/mod.rs:598-702`) to drive `run_sqlite_via_engine` and assert the
  CDC cache is invalidated for the added-column collection via the engine path —
  the regression test for the bridge.

- **P6b-5 — H3 auto-baseline on empty journal.** Implement the
  baseline-if-needed step (§4.2 / step 2 of §7b.2). **Gate:** a test that points
  `run_sqlite_via_engine` at a `zs-default.sqlite` pre-populated with tables but
  **no** `_mig` journal (the `run_sqlite_pipeline` legacy shape) and confirms the
  first engine boot **baselines** (records the existing schema as applied, no DDL
  re-run, no drift abort) then applies the new declared diff cleanly.

- **P6b-6 — Delete the old path (no-back-compat).** Delete `run_sqlite_pipeline`,
  `apply_sqlite`, `refreshes_sqlite_cdc_name_cache`, the SQLite
  `bootstrap`/`plan`/`validate` callers, `SqliteLockGuard` migration use, and
  `DeclarativeError::SqliteRebuildRequired` (§6, M3 decision §7c). **Gate:** the
  dev-tier e2e — a `zeroship serve` SQLite app whose `default.schema` add-column
  takes effect through the engine, journaled in `zs-default.migrations.sqlite`,
  with no plugin-db auto-migrate code path remaining.

- **P6c (optional, separable) — SQLite online expand-contract + build-side `generate`.**
  If the user wants full parity: SQLite `run_backfill` (single-actor, no
  `pg_advisory_xact_lock`), dual-write triggers, and/or the `zeroship-migrate
  generate` + `migrate` dev loop (schema.ts → bundle migration files), retiring
  auto-migrate-on-boot in dev to mirror prod (schema-authority §9). **This is the
  part that reaches the vite-plugin/CLI** — keep it out of P6 unless explicitly
  requested.

## 7c. M3 — fate of the `bootstrap`/`plan`/`validate` four-phase pipeline

<!-- Added in round 2 (M3): decide whether P6 deletes the now-dead four-phase
     pipeline wholesale or keeps it. -->

**Decision: delete the SQLite callers in P6b-6; keep the modules themselves out of
P6 (delete in a dedicated follow-up).** Rationale, grounded:

- The four-phase `bootstrap → plan → validate → apply` pipeline (`run_pipeline`,
  `register_model/mod.rs:223`) is, for PG, reached **only** by
  `exec_register_model_with_pool`, a `#[cfg(feature = "test-helpers")]` path — the
  PG production arm is `(Some(_pg), _) => Ok(())` (`mod.rs:187`), a no-op. So after
  P6b removes the SQLite caller, the whole pipeline is **dead outside tests**.
- Per the no-back-compat stance, dead code should go. **But** `bootstrap`/`plan`/
  `validate` are still imported by the PG test-helper path and by audit machinery
  that some PG integration tests drive directly. Ripping them out *wholesale* in
  P6b would balloon the blast radius (rewriting PG test fixtures) and entangle a
  dev-tier wiring change with a PG-test-infra change.
- **Therefore:** P6b-6 deletes the **SQLite use** of these modules (the `build_ctx`/
  `compute_plan`/`validate` calls inside the now-deleted `run_sqlite_pipeline`) and
  leaves the modules compiling for the `test-helpers` PG path. A dedicated
  follow-up (tracked, not P6) deletes the modules wholesale once the PG
  test-helpers are themselves retired or repointed at the engine — a clean
  simplification the no-back-compat stance invites, but one that is a *PG-test*
  change, not a *SQLite-wiring* change, and so does not belong in P6.

---

## 8. Risks / open questions for the design-critic and the user

**Flag explicitly to the user (product-behavior / dev-UX changes):**

- **Q1 — Dev destructive-approval posture (behavior change).** Today SQLite
  **silently skips** destructive ops (`apply_sqlite` `continue`s on
  `ChangeClass::Destructive`). Engine wiring makes destructive ops *real*. Auto-approve
  in dev (recommended, §4.1(i)) means a developer's `DROP COLUMN` / type-narrow in
  `schema.ts` now **applies** (data loss in the dev file) rather than being ignored.
  Acceptable for a local dev file? Or refuse like prod (§4.1(ii))?

- **Q2 — Engine-generate-at-boot vs build-side `generate` (dev-UX shape).** P6 minimal
  keeps **auto-migrate-on-boot** (engine authors from the descriptor at first request).
  Schema-authority §9 envisions "**no more auto-migrate-on-boot**" with a
  `zeroship-migrate generate`/`migrate` dev loop. The latter is more faithful to prod
  but is a **bigger change that reaches the vite-plugin + CLI** (P6c). Which end state
  does P6 target?

- **Q3 — SQLite declarative renames: offline rebuild vs online expand-contract.** PG
  renames are online (dual-write + backfill, multi-deploy). At dev scale, a SQLite
  rename can be an **offline rebuild** (simpler, single-deploy). Recommend offline for
  dev; confirm we are **not** porting `run_backfill`/`run_expand` to SQLite in P6.
  (H1 consistency: in P6a `plan.renames` is empty for SQLite — renames already
  surface as a `rebuild_one`, so "offline rebuild" is the *only* shape P6b sees.)

- **Q5 — Dev checksum-drift escape hatch (the one residual product call).** <!--
  Added in round 2: split out of R5 as the single genuine product decision. -->
  Once dev has a real `_mig` journal (a P6b upgrade), a developer who **edits the
  effective shape of an already-applied migration** trips checksum drift. Prod's
  answer is a hard error. Auto-baseline (§4.2) handles the *journal-less* and
  *deleted-`.zeroship/`* cases, so this only bites the in-place-edit case. Options:
  (a) dev keeps prod's hard-error and tells the developer to `rm -rf .zeroship/`
  (simplest, recommended for P6b — the auto-baseline then re-adopts); (b) a dev-only
  auto-rebaseline on drift (smoother, but masks a real "you changed history" signal).
  **This does NOT block P6b** — (a) is the P6b default; (b) is a follow-up UX knob.

**For the design-critic (technical):**

- **R1 — PG regression risk from genericizing `MigrationEngine`.** The PG
  `apply*`/`apply_declarative*` methods are heavily tested + carry the H10
  outer-lock + manifest-verify logic. Genericizing them (option A) must be
  **byte-behavior-identical** for PG. The critic should verify the lifted
  `acquire_project_lock_outer`→trait swap preserves the H10 single-acquire/single-release
  discipline and that `apply_with_lock_backend` is a faithful substitute for
  `executor::apply_with_lock`. (Option B sidesteps this at the cost of a second
  orchestrator.)

- **R2 — `cache_schema` / readiness contract on the engine path.** The P4
  introspection-metadata contract (the `t.id(prefix)` idPrefix, vector/mask hints)
  depends on `cache_schema` + `mark_model_registered` being stamped on `Ok(())`. The
  dispatch caller does this for **both** arms today; confirm `run_sqlite_via_engine`
  returning `Ok(())` still triggers it and that the engine path does not need an extra
  introspection priming step the old `bootstrap` did.

- **R3 — CDC name-cache invalidation moves. → RESOLVED in §7b.4.** The hook lives
  in `run_sqlite_via_engine`, which holds **both** the data-plane handle (A) and
  the migration handle (B) and bridges the invalidation. The original left "which
  collections" as a gap; §7b.4 designs the **batch-derived `changed` set** from the
  engine plan and the post-re-ATTACH `invalidate_cdc_name_cache` loop. The
  regression test is P6b-4.

- **R4 — The two `SqliteBackend` types and file/ATTACH coordination. → RESOLVED in
  §7b.1/§7b.2.** The original left this as "confirm no two-open-writer hazard;
  the readiness gate *may* enforce ordering; verify". §7b.2 designs a **single-owner
  window**: B opens and owns `zs-<app_id>.sqlite` for the DDL, is dropped, then A
  ATTACHes — so there are never concurrent writers on the app file (the WAL
  coexistence question is dissolved, not relied upon). §7b.1 pins the exact paths
  from `(backend_a.db_dir(), app_id)`. The critic's specific worry — that
  `mark_model_registered` is *not* the ordering authority — is correct and addressed
  in §7b.5 (the `installSchema` promise chain is the real barrier; the mark is not).

- **R5 — `_mig` journal lifecycle in dev. → RESOLVED (auto-baseline) in §4.2/§7b.2,
  escape-hatch deferred.** The "first boot against a journal-less file" case (which
  is also **H3**) is designed: auto-`baseline` on an empty journal + non-empty file
  (§4.2). The harder sub-case — a developer **edits an already-applied migration's
  effective shape** mid-stream (checksum drift) — is the one genuine product
  decision left: prod's answer is "drift = hard error"; dev *may* want roll-forward
  or auto-rebaseline. This is **§8 Q5 (new)**, flagged to the user; it does **not**
  block P6b (the common reset path is "delete `.zeroship/` and re-boot", which the
  auto-baseline handles since the journal is gone too).

- **Q4 → RESOLVED as M3 in §7c.** Decision: P6b-6 deletes the SQLite *callers* of
  `bootstrap`/`plan`/`validate`; the modules themselves are deleted in a dedicated
  PG-test-infra follow-up, not P6. See §7c for the grounded rationale.

---

## Revision log (2026-06-20, post-critic)

The critic scored the original 72/100, accepted **P6a** (engine genericization,
§3/§7) as sound and split it to build separately, and judged **P6b** (wire
`registerModel`→engine + retire `run_sqlite_pipeline`) under-designed — especially
the two-connection coordination left as "verify"/"likely". This revision designs
each finding to ground, with file:line citations re-verified against the worktree.

### Findings resolved

- **C1 (CRITICAL) — dev-only not enforced. → §1.1 + P6b-1.** Designed a two-layer
  structural guard keyed on `ZEROSHIP_DEV=1`: (1) `init_pool_async`'s `Sqlite` arm
  (`lib.rs:765`) refuses a SQLite backend unless `ZEROSHIP_DEV=1` — the single
  choke point every backend open funnels through; (2) worker startup
  (`worker/src/main.rs`) hard-aborts on a `sqlite:`/`file:` `--db`. The worker
  **never** sets `ZEROSHIP_DEV` (only `ZEROSHIP_DEPLOY_ID`, `cache.rs:286`), so the
  gate is fail-closed against prod by construction. With (1), the engine's no-op
  project-lock (`backend_sqlite/mod.rs:192`) is **unreachable** in any non-dev
  process, so cross-process concurrent apply cannot occur.

- **C2/C3 (CRITICAL, the crux) — two connections, no invalidation/ordering. → new
  §7b (a/b/c) + P6b-2..4.**
  - **(a) single-owner window (§7b.2):** B (hardened migration actor) opens and owns
    `zs-<app_id>.sqlite` for the DDL, is dropped, *then* A (CRUD actor) ATTACHes —
    the app file never has concurrent writers. A's control session
    (`.zeroship/dev.sqlite`) and B's files are disjoint, so their coexistence is
    harmless.
  - **(b) cache invalidation (§7b.4):** because A opens the app file *after* B
    closes, A's connection-level schema-cookie/prepared-stmt caches start clean (the
    C2 cross-connection staleness is dissolved, not patched). Only A's
    application-level CDC name cache needs a bridge: `run_sqlite_via_engine` derives
    the **batch `changed` collection set** from the engine plan and calls
    `invalidate_cdc_name_cache` per collection after re-ATTACH.
  - **(c) ordering barrier (§7b.5):** traced the `installSchema` promise topology —
    `run_sqlite_via_engine` runs entirely inside the awaited `exec_register_model`,
    so `ready` resolves only after B commits+drops AND A re-ATTACHes AND CDC is
    invalidated; the dispatcher (`dispatcher.ts:109`) / dev-entry
    (`dev-entry.ts:188/277`) await that before any handler — the only origin of
    CRUD. The barrier is real and spans both connections **provided steps 4-6 stay
    inside the awaited body** (the load-bearing implementer constraint). Confirmed
    `mark_model_registered` is a mark not a barrier, and `SchemaPendingGuard` gates
    only `subscribe`, per the critic. Alternatives (one-connection; both-open+WAL)
    evaluated and rejected in §7b.6, with the security rationale (B must stay the
    hardened actor).

- **H1 — SQLite renames → rebuild. → referenced in §7 P6a + §8 Q3.** P6a makes
  `plan.renames` empty for SQLite (renames surface as `rebuild_one`); §7b.4's
  `changed` set treats a rebuild as "columns changed". P6b never sees a standalone
  rename.

- **H2 — dev-approval signal read site. → §1.2.** Pinned: `ZEROSHIP_DEV` is read
  today only in `runtime/src/core/dev_auth.rs:54`; the apply path
  (`register_model/mod.rs:138`) reads only `ZEROSHIP_DEPLOY_ID`.
  `run_sqlite_via_engine` reads `ZEROSHIP_DEV` directly to derive the approval
  posture. Distinguished `ZEROSHIP_DEV` (dev-runtime identity) from
  `ZEROSHIP_DEV_INSECURE` (admin-key laxity) — SQLite gating keys on the former.

- **H3 — existing journal-less dev files. → §4.2 + P6b-5.** Designed auto-`baseline`
  (existing engine API, `baseline.rs`, journal kind `'baseline'`): on an empty
  journal + non-empty app file, record the live schema as the baseline entry before
  planning the declared diff — no drift-abort, no re-create.

- **M1/M2 — dev app_id + paths. → §7b.1.** Traced: dev `app_id` = literal
  `"default"` (`plugin.rs:228` `.unwrap_or_else(|| "default")`). The dev DSN
  `sqlite:.zeroship/dev.sqlite` is the data-plane **control session**; the app's
  tables live in `.zeroship/zs-default.sqlite` (the `ensure_app_schema` ATTACH
  target). B opens `app_path = <db_dir>/zs-default.sqlite`,
  `journal_path = <db_dir>/zs-default.migrations.sqlite`, with `<db_dir>` taken from
  A's `db_dir()` accessor so A and B cannot diverge. Reconciled the
  "single file" vs "per-app ATTACH" apparent contradiction (both true: control
  session file ≠ app file).

- **M3 — fate of bootstrap/plan/validate. → §7c.** Decision: P6b-6 deletes the
  SQLite *callers*; the modules are deleted in a dedicated PG-test-infra follow-up
  (PG `run_pipeline` is `#[cfg(test-helpers)]`-only), keeping the dev-tier wiring
  change from entangling PG test fixtures.

- **L1 — title. → renamed** to "SQLite engine **dev-tier** wiring".

### Phase plan changes (§7)

P6b decomposed into six independently-testable steps (P6b-1 guard → P6b-2 paths →
P6b-3 single-owner/ordering → P6b-4 CDC bridge → P6b-5 baseline → P6b-6 delete),
each with a concrete gate. Added §7b (coordination design) and §7c (M3 decision).

### Buildability assessment (honest)

**P6b is now buildable without a product/architecture escalation.** The
two-connection coordination (C2/C3) resolves to a **sequencing** design
(single-owner window + a barrier that already exists in the `installSchema` promise
topology) using only primitives that exist today (`db_dir()`, `ensure_app_schema`,
`invalidate_cdc_name_cache`, `baseline`, the dispatcher await). No new runtime
primitive, no new IPC, no cross-process lock (P5b stays deferred and is made
unreachable by the C1 guard). The design's correctness rests on one load-bearing
implementer constraint — **steps 4-6 (B-drop, A-re-ATTACH, CDC-invalidate) must run
inside `run_sqlite_via_engine`'s awaited body**, not detached — which is captured
as the P6b-3 test gate.

The **only** residual product decision is **Q5** — the dev checksum-drift escape
hatch when a developer edits an already-applied migration in place. It does **not**
block P6b: the P6b default is prod's hard-error + the documented `rm -rf .zeroship/`
reset (which auto-baseline then re-adopts); a smoother dev auto-rebaseline is an
optional follow-up UX knob. The pre-existing Q1 (destructive auto-approve in dev)
and Q2 (engine-at-boot vs build-side `generate`) remain user-facing posture choices,
not blockers — P6b ships with the recommended defaults (auto-approve in dev;
engine-at-boot) and either can be flipped without re-architecting the coordination.

---

## Comparative decision (2026-06-20 round 3) — A vs B vs C, re-derived

> Round 3 is the **deciding comparative**. Rounds 1–2 designed *one* option (the
> separate-connection-B single-owner window, here called **Option A**) and a critic
> hardened it. This round puts A head-to-head against **Option B** (engine on the
> data-plane connection A under a Trusted profile) and **Option C** (keep
> `run_sqlite_pipeline`), re-grounds the load-bearing facts, and **re-derives** the
> recommendation from the tradeoffs rather than inheriting it. It also surfaces
> **two facts the round-2 design under-weighted** that materially move the verdict.

### R3.0 Two re-grounded facts that change the analysis

**Fact 1 — "single-process" ≠ "single-isolate". A hand-run `zeroship serve` is
multi-isolate.** Verified:

- `crates/runtime/src/core/serve.rs:143` — `workers == 0` ⇒
  `std::thread::available_parallelism()`. The CLI default is `--workers=0`
  (`crates/cli/src/main.rs:57`). So a bare `zeroship serve myapp.js` spins **N
  isolates** (one per core), each a separate worker thread.
- The SQLite data-plane backend **A** is stored in **per-isolate** context
  (`init_pool_async` → `ctx_mut(|c| c.set_sqlite_backend(…))`, `lib.rs:771`;
  `context::with` is thread-local). `is_model_registered` /
  `mark_model_registered` are **per-isolate** too (`lib.rs:228/233`). So **each
  isolate opens its own `SqliteBackend` A on the same `zs-default.sqlite` and runs
  its own cold-path `registerModel`.**
- The engine's SQLite project-lock is a **no-op** (`backend_sqlite/mod.rs:192`).

**Consequence:** on a hand-run multi-isolate `zeroship serve` against a `sqlite:`
DSN, **N isolates can run the migration concurrently on the same file with no
cross-process/-thread serialization.** This is the *same* corruption class C1
named for the prod worker — but it can occur in a **single dev process**. The
round-2 framing "single-process by construction ⇒ safe" is therefore **too weak**:
the real safety property is **single-isolate**, which only the **Vite** path
guarantees (`sdks/vite-plugin/src/dev-server.ts:466` spawns
`zeroship serve … --workers=1`). A hand-run does not. **Every option must address
this**, and it is the strongest argument *against* Option A (which doubles the
concurrent writers to 2N: N×A + N×B).

**Fact 2 — the round-2 C1 guard, as written, BRICKS a hand-run `zeroship serve`,
and the H3 baseline primitive does NOT exist for SQLite.**

- C1 re-key: `crates/cli/src/main.rs cmd_serve` registers the DB plugin from
  `DATABASE_URL` (`:134`) and **never sets `ZEROSHIP_DEV`** — only the Vite plugin
  does (`dev-server.ts:447 [ENV_DEV]: "1"`). The round-2 "refuse SQLite unless
  `ZEROSHIP_DEV=1` in `init_pool_async`" guard (P6b-1, §1.1 guard 1) would reject a
  documented, supported invocation: `zeroship serve myapp.js` with
  `DATABASE_URL=sqlite:…` (AGENTS.md "Run single-tenant (dev)"). **The guard is
  mis-keyed.** Re-keyed below (R3.4).
- H3 baseline: round-2 §4.2 claims "the engine already has the right primitive:
  `baseline`". **It does not, for SQLite** — `baseline(conn: &Client, …)`
  (`crates/zeroship-migrate/src/baseline.rs:116`) is hard-typed to
  `compio_postgres::Client`. A SQLite baseline must be **built** on the
  `journal_sql` side (it does not fall out of P6a). Round-2 treated a to-be-built
  component as already-built — corrected in R3.4.

### R3.1 Option A — separate hardened connection B + single-owner window

**What it is:** §7b. B (`zeroship_migrate::SqliteBackend`, hardened) opens and owns
`zs-<app_id>.sqlite` for the DDL, is dropped, *then* A
(`plugin_db::…::SqliteBackend`, CDC-armed) ATTACHes for CRUD. The
`installSchema`-promise chain orders B-done + A-re-ATTACH + CDC-bridge before the
first handler runs.

**`ensure_app_schema` as the chokepoint — is it universal? Honestly: no, but it
doesn't have to be.** Grounded:
- The **write** path funnels the app-file ATTACH through `ensure_app_schema`
  (`crud/write_pipeline.rs:743`). The **read** path does **not** call
  `ensure_app_schema` at all (only `crud/write_pipeline.rs` references it in
  `crud/`). Reads assume the ATTACH already exists.
- But A is a **single-writer actor** (`backend/sqlite/session.rs:1` — one
  `rusqlite::Connection`, one `flume::bounded(64)` queue, one thread). The ATTACH
  is connection state; once `ensure_app_schema` has run on A's connection, **every**
  subsequent op on A (read or write) sees the attached file. So the chokepoint that
  matters for Option A is not "every op" but "**A's first touch of the app file**",
  which **is** `ensure_app_schema`, and which the cold `registerModel` path controls.
  The **warm-isolate fast path** (`is_model_registered`, `register_model/mod.rs:78`)
  short-circuits *before* `exec_register_model` — so on a warm isolate
  `run_sqlite_via_engine` never runs and CRUD hits an A that was already correctly
  attached on this isolate's cold pass. **The latch is only ever engaged on the cold
  pass, per isolate.** This is self-consistent.
- **So Option A does not need a new latch at all** in the single-isolate case — the
  §7b "single-owner window" (B fully before A's first ATTACH) is *itself* the
  serialization, and the promise chain is the barrier. The round-2 design is
  correct *for one isolate*.

**Where Option A breaks (the new Fact-1 hazard):** with N isolates (hand-run
`--workers=0`), Option A opens **N B's + N A's** on one file. The single-owner
window is **per-isolate**; it does **not** serialize *across* isolates. Isolate-1's
B and isolate-2's B can run the 12-step rebuild on `zs-default.sqlite`
concurrently, both holding `BEGIN IMMEDIATE` against the same file, with the engine
project-lock a no-op. SQLite's own file lock will serialize *transactions*, but the
**rebuild's `foreign_keys=OFF` window + multi-statement DDL is not one
transaction**, and two interleaved rebuilds can corrupt. **Option A's correctness
therefore depends on single-isolate**, exactly as much as B and C do — its extra
connection B buys hardening but **adds** a second writer per isolate, making the
multi-isolate hazard *strictly worse* than B.

**Verdict on A:** the cleanest *security* story (DDL on the hardened actor), and the
round-2 single-owner-window + promise-barrier is genuinely sound **for one
isolate**. But it (i) is the **largest** change (two backends, cross-crate path
construction, CDC bridge, baseline-to-build), (ii) **doubles** the per-file writer
count, making the multi-isolate hazard worse, and (iii) still needs the
single-isolate guarantee to be *enforced*, not assumed.

### R3.2 Option B — engine on connection A under a Trusted profile

**What it is:** no second connection. The engine's declarative apply runs on
plugin-db's `SqliteSession` (A) itself, inline where `run_sqlite_pipeline` runs
today, under a "Trusted" migrate profile (authorizer relaxed/with the operator's
own file).

**The wrinkles, assessed head-on:**

- **(i) Cross-crate coupling / trust domain (H4).** The migrate engine's apply path
  is built around its **own** `MigrationActor` (`backend_sqlite/actor.rs`), with the
  two-mode authorizer, `_mig` immutability triggers, ATTACH confinement. To "run the
  engine on A" you must **re-implement** `MigrationBackend` over plugin-db's
  `SqliteSession` — i.e. give plugin-db's connection the journal I/O, drift snapshot,
  `rebuild_one`, `validate_non_txn`, session snapshot/restore. That is **not "no
  second connection, same code"** — it is a **second `MigrationBackend` impl** living
  in plugin-db, driving the engine's generic executor against A. The migrate crate's
  hardened SqliteBackend (P2–P3b, security-verified) would be **bypassed entirely**
  for the live path. So Option B does not *reuse* the built+verified backend; it
  *re-creates* its trait surface on an un-hardened connection. **That is the
  opposite of the epic's "single-source the engine" goal**, and it throws away the
  P2–P3b security verification for the one path that actually runs in dev.
- **(ii) CDC hooks fire on migration DDL/backfill.** A is CDC-armed
  (preupdate/commit hooks, `mod.rs:142`). Running DDL + any backfill on A emits
  **spurious `ChangeEvent`s** for the migration's own writes. In dev this is mostly
  cosmetic (a subscriber sees migration churn), but a rebuild's data copy
  (12-step: copy rows to the new table) would emit a `ChangeEvent` **per row** —
  noisy and semantically wrong (these are not app mutations). Suppressing them means
  threading a "migration in progress, mute CDC" flag through A's hook path — net new
  coupling. **Not fatal in dev, but it is real work and it muddies the CDC
  contract.**
- **(iii) Authorizer / confinement.** On the operator's own dev file the Trusted
  profile *could* drop the authorizer. But the authorizer is precisely the defense
  against a **prompt-injected AI-authored migration** (the migrate engine's whole
  premise per the project memory: "security-first … AI-authored complex migrations
  behind a MigrationAuthor seam"). Dropping it on A means dev loses the one
  protection that distinguishes the engine from `run_sqlite_pipeline`. Keeping it as
  defense-in-depth on A means re-implementing the two-mode authorizer on the
  CDC-armed connection — back to wrinkle (i).
- **(iv) Engine generic apply on the CDC-hooked actor.** The rebuild's
  `foreign_keys=OFF` window + `BEGIN IMMEDIATE` must run on A's single connection.
  A's connection also services CRUD — but the **same promise barrier** (R3.1 / §7b.5)
  keeps CRUD from interleaving (no handler runs until `registerModel` resolves), so
  the *transaction* conflict is avoided **for one isolate**. The journal `_mig`
  would be a second attached file on A (fine). **This part works** — and notably,
  because there is only **one** connection, the C2 schema-cookie/cache-staleness
  question **vanishes** (round-2 §7b.4's whole concern), and the CDC name-cache is
  **naturally consistent** (the cache lives on A; A did the DDL; A invalidates inline
  exactly as `run_sqlite_pipeline` does today — **no cross-connection bridge**).
- **Multi-isolate (Fact 1):** identical exposure to A — N isolates ⇒ N A's each
  running the engine on the file with a no-op lock. **But B opens only N writers,
  not 2N** (no separate migration connection), so the hazard surface is *smaller*
  than Option A's.

**Is B smaller or larger than A?** Mixed:
- **Smaller:** no second connection, **no CDC bridge**, no cross-crate path
  construction, no single-owner-window sequencing, no C2 cache-coherence reasoning,
  fewer writers per file.
- **Larger:** a **new `MigrationBackend` impl on `SqliteSession`** (journal I/O +
  drift + rebuild + authorizer-or-not), which **duplicates** the migrate crate's
  hardened SqliteBackend and **discards its security verification** for the live
  path. Plus CDC-suppression-during-migration.

**Verdict on B:** B is attractive on the coordination axis (it deletes the entire
two-connection problem that consumed round 2) but **loses the epic's central asset
— the hardened, security-verified migration backend** — by re-implementing its
trait surface on the un-hardened CRUD connection. The project memory is explicit
that this engine is **"security-first … least-priv migrator role + parse deny-list
+ immutable journal"**; Option B runs the live dev migrations *outside* that
hardening. That is a poor trade even in dev, because the dev tier is exactly where
AI-authored schema changes (the threat model) are iterated.

### R3.3 Option C — keep `run_sqlite_pipeline`

**What it is:** the engine's SQLite capability (P1–P6a) stays built and tested but
**unused by the dev tier**; `run_sqlite_pipeline` remains the dev migrator.

**What is LOST by not converging (honest):**
- **The known latent bug stays:** `apply_sqlite` **silently skips**
  `ChangeClass::Destructive` (`register_model/mod.rs` apply loop). A developer's
  `DROP COLUMN` / type-narrow in `schema.ts` is **ignored** — the dev DB diverges
  from the declared schema with no error. This is a real correctness wart (it
  surprises the developer), though it is **non-destructive** (it errs toward keeping
  data).
- **No journal / versioning / rollback / drift / 12-step rebuild in dev.** Dev
  cannot reproduce a prod migration locally with the same engine; dev and prod
  schema-application diverge in mechanism (prod = engine-at-deploy, dev = bespoke
  additive diff). The "build your own migration engine and run it everywhere" goal
  is **unmet** for the dev tier.
- **The engine's SQLite arm has no live exercise.** P1–P6a are tested in isolation
  but never driven by a real app boot, so regressions in the SQLite arm can only be
  caught by unit tests, not the dev e2e.

**Is that loss acceptable given SQLite is dev-only and recoverable?** Partly. The
*data-loss* risk is nil (dev file, `rm -rf .zeroship/` resets). The *divergence*
and *silent-skip* costs are real DX/quality costs but not safety costs. **C is the
honest floor: it works today and ships nothing.** Its cost is the unmet
convergence goal and a known-surprising silent-skip — both tolerable, neither
desirable.

**Verdict on C:** lowest risk, lowest reward, leaves a known wart. Acceptable as a
fallback if A and B both prove too costly, but it forfeits the entire reason P1–P6a
were built.

### R3.4 Recommendation (re-derived): **Option A — but with the single-isolate
property ENFORCED, the C1 guard RE-KEYED, and a SQLite baseline BUILT**

Re-deriving from the tradeoffs rather than inheriting round 2:

- **B's coordination win is real but its security loss is disqualifying for *this*
  engine.** The epic exists to run schema changes — including AI-authored ones —
  through a hardened, deny-listed, journalled migrator. Option B runs the live dev
  path *outside* that hardening (re-implemented on the CDC-armed CRUD connection) and
  **discards the P2–P3b security verification** for the only path that executes in
  dev. The project's own first principle is "security-first". B trades that away for
  coordination simplicity. **Rejected.**
- **C forfeits the goal.** It is the fallback, not the recommendation.
- **A keeps the hardened backend on the live path** (its central virtue) and the
  round-2 single-owner-window + promise-barrier is **sound for one isolate**. Its
  two genuine problems are both **fixable and are problems A *shares* with B**:
  1. the **multi-isolate hazard** (Fact 1) — fixed by *enforcing single-isolate for
     SQLite*, which is correct independent of A/B/C and which the engine's no-op
     project-lock already *assumes*;
  2. the **C2 cache-coherence reasoning** — A pays it (cross-connection), B doesn't.
     But A's §7b.4 *resolves* it (A opens after B closes ⇒ clean connection caches +
     a batch CDC bridge), so it is **designed, not open**.

**So the recommendation is A**, on the strength of preserving the hardened backend,
**conditioned on three fixes** that round 2 either mis-specified or under-weighted:

**(1) Enforce single-isolate for SQLite — the real fix for Fact 1 (supersedes the
round-2 C1 guard's *primary* arm).** The hazard is N isolates on one file, not just
the prod worker. The structural fix is: **when the backend resolves to SQLite, the
runtime must run exactly one isolate.** Concretely:

- In `crates/runtime/src/core/serve.rs` (`start_server`, the `num_workers` resolution
  at `:143`), if the configured DB URL resolves to SQLite (classify via
  `plugin-db`'s `backend_for_url` grammar, or a `DATABASE_URL` scheme check), **clamp
  `num_workers` to 1** (and log a one-line notice: "SQLite dev backend → single
  worker (per-file single-writer)"). This makes the single-isolate property — which
  the engine's no-op project-lock *already assumes* — **true by construction** for
  every `zeroship serve`, hand-run or Vite-spawned. The Vite path already passes
  `--workers=1`, so this is a no-op there and a **correctness fix** for the hand-run.
- This is **strictly better than the round-2 "refuse SQLite unless `ZEROSHIP_DEV`"
  primary guard**, which (a) bricks the supported hand-run (Fact 2) and (b) does not
  actually address multi-isolate (it only addressed the *prod worker*). Clamping to
  one isolate addresses the hazard *and* keeps the hand-run working.

**(2) Re-key the prod-refusal guard to the worker, not `ZEROSHIP_DEV`.** Keep the
round-2 **defense-in-depth worker guard** (§1.1 guard 2): `crates/worker/src/main.rs`
hard-aborts if `--db`/`DATABASE_URL` resolves to SQLite — the worker is multi-replica
by identity and SQLite there is always a misconfig. **Drop the `init_pool_async`
`ZEROSHIP_DEV` gate as the primary** (it mis-keys the hand-run). The authority for
"may I use SQLite?" is **process identity** (the worker refuses; `zeroship serve`
permits), not an env flag the CLI doesn't set. Net guard set:
- `zeroship serve` (any DSN): SQLite **permitted**, isolate count **clamped to 1**
  (fix 1). No `ZEROSHIP_DEV` requirement.
- `zeroship-worker`: SQLite **refused at startup** (hard abort) — unchanged from
  round-2 guard 2.
- This removes the C1-vs-hand-run brick (Fact 2) while keeping the prod worker
  fail-closed, and it no longer relies on the CLI setting a flag it doesn't set.
- *Optional* env override `ZEROSHIP_DB_ALLOW_MULTI_ISOLATE_SQLITE=1` to *opt out* of
  the clamp for an operator who knows their app is read-mostly — **not recommended**;
  list as a deliberate escape hatch, default off.

**(3) Build a SQLite baseline on the `journal_sql` side (H3 is unbuilt, not
"already there").** `baseline(conn: &Client)` is PG-only (Fact 2). For Option A's
warm-file adoption (§4.2), add a SQLite baseline that, given B's `MigrationActor`,
writes a single `'baseline'` journal entry recording the live `snapshot_schema`
**without running `up`** — i.e. a `MigrationBackend::baseline_one` (or a free fn in
`backend_sqlite/journal_sql.rs`) mirroring the PG `baseline_locked` semantics over
the `_mig` journal. This is a **new, small, testable unit** (P6b-5 must *build* it,
not just *call* it). Without it, the first engine boot against a warm
`run_sqlite_pipeline` file drift-aborts.

**Why not just take B to dodge fixes (2)/(3)?** Because B still needs fix (1) (the
multi-isolate hazard is B's too), B *also* needs a SQLite baseline (the journal is
the same), and B's "savings" come at the cost of re-implementing + un-verifying the
hardened backend. The fixes A needs are smaller than the security B forfeits.

### R3.5 Honest residual hole (needs a user/architecture decision)

**The one genuine open question fixes (1)–(3) do *not* close:** the
**single-owner window (§7b.2) is correct only if B is fully dropped before A
ATTACHes — but does dropping B (its `rusqlite::Connection`) reliably *release* the
OS file locks before A's ATTACH on a *different* connection in the *same*
process?** SQLite uses POSIX advisory locks, which on some platforms are
released on *any* `close()` of *any* fd to the file in the process — a known POSIX
fcntl-lock footgun. A and B are different connections (different fds) in one
process. The §7b.2 sequencing (B dropped *before* A opens) avoids *concurrent*
holders, so this is **likely** safe, but it has **not been verified on the target
platform** and is a class of bug (the POSIX `close()`-drops-all-locks trap) that
warrants an explicit test: open B, apply, drop B, assert A can ATTACH + write, in a
loop, on Linux. **If** that test flakes, the fallback is **Option B for the apply**
(one connection, no inter-connection lock handoff) — which is *why B must stay a
documented, costed fallback rather than be discarded*. This is the residual risk a
round-3 critic should attack first.

A second, softer hole: **fix (1)'s URL classification in `serve.rs`** duplicates
`plugin-db`'s `backend_for_url` grammar in the runtime crate (a layering smell —
the runtime shouldn't know SQLite DSN shapes). Cleaner: have `plugin-db` expose a
`fn is_sqlite_url(&str) -> bool` and call it from `serve.rs`, or have the DB plugin
report its backend kind back to `start_server` after init and clamp *then* (but the
worker count is fixed before plugin init today — so the pre-init URL check is the
pragmatic choice). Flag for the implementer; not blocking.

### R3.6 Net changes to the phase plan (supersedes §7 where they conflict)

- **P6b-1 (re-keyed):** replace "refuse SQLite unless `ZEROSHIP_DEV=1` in
  `init_pool_async`" with **(a)** clamp `num_workers→1` for a SQLite DSN in
  `serve.rs` (fix 1), and **(b)** keep the worker-startup SQLite hard-abort (fix 2).
  Delete the `ZEROSHIP_DEV`-gated `init_pool_async` refusal from the design. **Gate:**
  a `serve.rs` test that a SQLite DSN yields `num_workers==1` even with `--workers=8`;
  the existing worker-abort test stays.
- **P6b-5 (corrected):** **build** a SQLite `baseline` (journal `'baseline'` entry,
  no `up` run) on the `journal_sql`/`MigrationBackend` side (fix 3), *then* call it.
  Gate unchanged (warm-file adoption test) but now also asserts the baseline entry
  was **written by the new SQLite baseline**, not assumed.
- **New P6b-0 (POSIX-lock handoff proof):** a Linux loop test — B open→apply→drop,
  A ATTACH→write — to validate the single-owner window's cross-connection file-lock
  release (R3.5). If RED, escalate to the Option-B fallback for the apply path.
- **Keep B as the documented fallback** (not deleted from the design), gated on
  P6b-0. Everything else in §7/§7b stands.

### R3.7 What is DELETED (unchanged from §6, reconfirmed) and blast radius

Unchanged: `run_sqlite_pipeline`, `apply_sqlite`, `refreshes_sqlite_cdc_name_cache`,
the SQLite `bootstrap`/`plan`/`validate` callers (modules kept per §7c/M3),
`SqliteLockGuard` migration use, the `(_, Some(sqlite)) => run_sqlite_pipeline`
arm (→ `run_sqlite_via_engine`). `DeclarativeError::SqliteRebuildRequired` is
**already gone** (P6a landed; `engine.rs` no longer defines it — reconfirmed). Blast
radius stays **engine + plugin-db Rust + the `serve.rs` clamp** (fix 1 adds a small
runtime-crate touch — the one *new* file outside plugin-db/migrate), with test
rewrites; it does **not** reach the vite-plugin/dev-bootstrap JS (the `--workers=1`
Vite spawn already satisfies fix 1).

### R3.8 Round-3 verdict, one line

**Recommend Option A** (hardened separate backend B + single-owner window +
promise-barrier) **for its security**, **conditioned on**: (1) clamp SQLite to a
single isolate in `serve.rs` — the real fix for the multi-isolate hazard the
round-2 "single-process" framing missed; (2) re-key the prod refusal to the
**worker identity**, not `ZEROSHIP_DEV`, so a hand-run `zeroship serve` with a
`sqlite:` DSN is **not bricked**; (3) **build** (not assume) a SQLite `baseline`.
**Keep Option B as a costed, documented fallback** gated on the **P6b-0 POSIX
file-lock handoff test** (R3.5) — the single residual hole that could force the
one-connection shape. **Option C** is the do-nothing floor: acceptable only if both
A and B prove infeasible, at the cost of the unmet convergence goal and the
silent-destructive-skip wart.
