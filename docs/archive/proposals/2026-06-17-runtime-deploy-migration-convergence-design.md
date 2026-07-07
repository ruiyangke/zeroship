# Runtime ↔ Deploy Migration Convergence — Design

**Status:** proposal (design artifact for a critic loop — not yet implemented).
**Worktree / branch:** `appbase-migrate` / `feat/db-migration-engine`.
**Direction (decided, do not reopen):** REPLACE / CONVERGE. Deploy-time migration becomes the
single source of schema truth for a creator app; `registerModel` at runtime becomes **verify-only**.
**Depends on:** the approved migration-engine design (`docs/proposals/2026-06-16-db-migration-engine-design.md`)
and the implemented `zeroship-migrate` crate on this branch.

---

## 1. Goal

Today a creator app's Postgres schema is mutated in **two** places by **two** engines that do not
know about each other:

- **At runtime**, `db.registerModel(...)` (the four-phase pipeline in
  `crates/plugin-db/src/register_model/{mod,bootstrap,plan,validate,apply}.rs`) introspects the live
  schema, diffs it against the app's declared `export default { schema }`, classifies each change
  (Additive / Compatible / Destructive), **refuses destructive**, and **applies the rest as DDL** —
  on every cold start, under the privileged platform/login role, journaling into
  `"<app_id>".__zeroship_migrations`, serialised by `pg_advisory_lock(hashtext("<app_id>:register_model"))`.

- **At deploy**, nothing. `POST /api/apps/{id}/deploy` (`crates/control/src/api.rs:394-620`) ingests the
  `.zship`, stores blobs, commits the manifest + routes — and never touches the database schema. The
  `zeroship-migrate` crate (the security-first versioned engine) exists but is **depended on by nothing**
  (`crates/control/Cargo.toml` has no `zeroship-migrate`).

This is the wrong shape:

1. **App code applies DDL.** The runtime worker, executing on the request path's cold start, runs
   `CREATE`/`ALTER` under a privileged role. A schema change is a privileged, reviewable, gated
   operation; doing it as a side-effect of the first request is a consistency *and* security liability.
2. **Two diff engines, two type maps.** `register_model` and `zeroship-migrate`'s declarative differ
   (`crates/zeroship-migrate/src/declarative.rs`) consume the *same* descriptor JSON and **duplicate**
   the DSL→PG type map (`declarative.rs:208-229` mirrors `crates/plugin-db/src/query.rs:2043` `def_to_pg_type`
   + `query.rs:1827` `def_to_column_type_for_dialect`; `declarative.rs:242-253` mirrors `query.rs:856`
   `build_system_field_columns`). Divergence today is a latent fidelity bug; after convergence it becomes a
   **verify-vs-apply mismatch** (the engine applies one shape, the runtime verifies another) — so the
   duplication must be resolved as part of this work.
3. **The engine can't gate what it never sees.** Destructive changes, renames, backfills, expand-contract —
   the entire point of `zeroship-migrate` — are unreachable, because `register_model` flatly *refuses*
   destructive and there is no deploy-time path to the engine's `submit_migration` adapter.

**Goal:** make `zeroship-migrate` THE schema authority for a creator app. All DDL flows through the
engine **at deploy**. At runtime, `registerModel` stops applying DDL and instead **verifies** that the
live schema already matches the declared schema, **failing closed** if it does not. One journal, one
lock, one type map.

---

## 2. Ground truth (from two completed explorations; verified against this branch)

### 2.1 plugin-db is APP-scoped
- One Postgres schema **per app**, named literally `"<app_id>"` (the UUID): `query.rs:541-542`
  `build_create_schema` → `CREATE SCHEMA IF NOT EXISTS "<app_id>"`; every name is fully schema-qualified
  (`query.rs:620` `"{schema}"."{table}"`), so there is **no `search_path` reliance**.
- Per-app role `app_<app_id>_role`, `NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE NOINHERIT`
  (`crates/plugin-db/src/auth/bootstrap.rs` `create_role_if_missing`, the `APP_ROLE_TEMPLATE` arm);
  granted CREATE-on-own-schema + DML + `ALTER DEFAULT PRIVILEGES`; applied via `SET LOCAL ROLE` for
  CRUD/transactions (`crates/plugin-db/src/transaction::apply_per_app_role`). Provisioned today **at
  runtime** inside `register_model` bootstrap (`register_model/bootstrap.rs:209` →
  `crate::auth::bootstrap::ensure_per_app_role`).
- One shared DB; isolation by **schema + role**, NO RLS.
- App identity is **server-injected** from the dispatch path → `APP_ID` env → stamped
  (`crates/worker/src/cache.rs:273`, `crates/runtime/src/core/plugin.rs:225`), and JS overrides are refused.
- `register_model` DDL runs under the privileged **platform/login** role; the per-app role is CRUD-only.
- Plan-stage live introspection exists as `SchemaIntrospect::introspect_schema(app_id)`
  (`crates/plugin-db/src/backend/postgres.rs:282`, trait at `backend/mod.rs:608`) producing
  `crate::diff::LiveSchema`, fed to `crate::diff::compute_diff` (`register_model/plan.rs:47,71`). These two
  are exactly what verify-only reuses.
- Destructive refusal lives in `register_model/validate.rs`: `strict` (default) → `validation_refused`
  envelope short-circuits; `lenient` → drop destructive from apply set; `off` → apply.

### 2.2 zeroship-migrate is PROJECT-scoped (but trivially specialisable to one-app-per-project)
- `ExecutorConfig { project_id, project_schema, meta_schema, statement_timeout, lock_timeout,
  migrator_role, trust (pub(crate)), platform_schemas, platform_exts, operator_cap }`
  (`crates/zeroship-migrate/src/db.rs:26-104`). `ExecutorConfig::new(project_id, project_schema)` defaults
  `meta_schema = "<project_schema>_migrations"`, `trust = Confined`, `migrator_role = None`.
- Declarative differ (`declarative.rs`) consumes the SAME `CollectionDescriptor / FieldDescriptor /
  IndexDescriptor` JSON `registerModel` emits, with the DSL→PG type map **duplicated** from plugin-db
  (`declarative.rs:26-36, 208-229`), guarded only by `tests/declarative_pg.rs`.
- Engine surface (`crates/zeroship-migrate/src/engine.rs`): `plan` / `plan_declarative` (`:223`) /
  `apply` (`:468`) / `apply_with_lock` (`:494`) / `apply_verified` (`:614`) / `apply_declarative` (`:285`) /
  `apply_declarative_verified` (`:390`) / `dry_run` (`:658`) / `dry_run_declarative` (`:675`) /
  `rollback` (`:710`) / `run_expand` (`:763`). Plus drift, backfill, expand-contract, baseline, squash,
  declarative diff, shadow dry-run, the integrity manifest (`DeclarativeDeployPlan::manifest`,
  `engine.rs:906`), the `submit_migration` adapter (`submit.rs:267,409`), and the dbmate-like CLI.
- Journal: `<meta_schema>.schema_migrations` (+ `_inflight`, `_rolled_back`, `_supersedes`,
  `_event_seq`) — `journal.rs:154-260`. Immutable (trigger-enforced), append-only, **off the migrator's
  privilege path** (the meta schema is a separate namespace the migrator cannot write).
- Advisory lock: `pg_advisory_lock(hashtext(project_id))` (per design §2.3;
  `executor::acquire_project_lock_outer`).
- Least-priv role: `migrator_<project>` (`role.rs:154` `migrator_role_name`, `:187` `provision_migrator`):
  `NOLOGIN`, **owns** the project schema, `ALTER DEFAULT PRIVILEGES FOR ROLE migrator`, **no** access to
  the meta schema, no public-schema reach. `search_path`-pinned to the project schema during apply
  (`db.rs:33`).
- Trust profiles `Confined` / `Platform` / `Trusted` (`guard.rs`), gated by an `OperatorCapability` the
  control plane cannot mint (the privileged constructors are `pub(crate)` + token-gated).

### 2.3 control plane
- ntex/compio HTTP, Bearer auth (PAT + OAuth) → principal = `users.id`; app scoping via
  `authz.require(Action::AppsDeploy, Resource::App { id })` (`api.rs:410-415`); ownership via
  `app_members(role='owner')`.
- `deploy` (`api.rs:394-620`): stream → tmp → mmap → `deploy::ingest` → blob + manifest + routes +
  per-app OAuth reconcile. **No migration step.**
- A **single `--db` DSN**. **No `CREATEDB` admin DSN.** **No async-job infra** (only detached crons under
  `crates/control/src/cron/`).
- TS client `@zeroship/control` (`sdks/control/src/index.ts`): `ControlClient` with namespaced groups
  (`apps`, `env`, …) on a shared `#fetch`.

### 2.4 The load-bearing gap nobody flagged: the manifest does not carry the schema
The `.zship` manifest (`crates/bundle/src/manifest.rs`) carries RPC `schemas` (JSONSchema by sha256,
`manifest.rs:61-65`) but **not** the DB collection descriptors. Schema discovery happens **at runtime**
by reading `default.schema` off the loaded entry module (`manifest.rs:120-124, 240-256`; the old
`schema` *path* field is dead). `installSchema(schema, env.db)` calls `registerModel` per collection
inside the V8 isolate (`sdks/bootstrap/src/{runtime-entry,dev-entry}.ts`).

**Consequence:** the control plane has **no declared schema at deploy time**. Convergence is impossible
until the declared schema (the descriptor set) is materialised into the deploy artifact where the control
plane can read it without booting V8. **This is Phase 1 of the plan and a hard prerequisite.**

---

## 3. The convergence model — one engine, app-scoped

`zeroship-migrate` becomes the single schema authority for a creator app, specialised to the
**one-app-per-project** case:

| Engine concept | Bound to (app-scoped) | Rationale |
| --- | --- | --- |
| `project_id` | `app_id` | seeds the advisory lock; matches plugin-db's per-app serialisation key |
| `project_schema` | `"<app_id>"` | the literal schema plugin-db already uses (`query.rs:541`) |
| `meta_schema` | `"<app_id>__migrations"` (see §7) | the journal namespace, off the migrator's path |
| ownership | trivial — the app owns its whole schema | every `owner_app` == the deploying app; the differ's per-table ownership checks all pass vacuously |
| `trust` | `Confined` | creator SQL is untrusted; the deny-list + confined migrator role apply |
| `migrator_role` | `migrator_<app_id>` (see §10) | least-priv DDL role, owns `"<app_id>"` |

**The project-union / per-table-owner model is RETAINED in the code but UNUSED here.** The differ's
`DesiredSchema.ownership`, `live_ownership`, `NotTableOwner`, `DropOfUnownedTable`,
`CrossAppFkTargetMissing` machinery (`declarative.rs:272-507, 985-1011`) is exercised with a single
declaring app, so it collapses to "the app owns everything it declares." The full multi-app-shared-db
umbrella is a **documented FUTURE extension** (§12 Non-goals): when a project hosts multiple apps, the
control plane passes the complete union and a real `live_ownership` map, and the same engine code handles
it. Nothing here forecloses that; we simply do not turn it on.

**Why app-scoped, not project-scoped now:** plugin-db is app-scoped end to end (schema, role, identity
injection). Introducing a project layer in the same PR would require a project registry, a project↔app
membership table, and a union-assembly step in the deploy path — none of which exist. App-scoped
convergence is shippable today and is a strict subset of the umbrella model the engine already supports.

---

## 4. `registerModel` → verify-only (+ runtime failure behavior)

### 4.1 What changes
`db.registerModel(collection, schema, indexes)` (dispatch at `register_model/mod.rs:68`) **stops applying
DDL**. The four-phase pipeline (`bootstrap → plan → validate → apply`) is replaced by a **verify** pass:

1. **Introspect** the live schema for `"<app_id>"` — reuse `SchemaIntrospect::introspect_schema(app_id)`
   (`backend/postgres.rs:282`), unchanged.
2. **Diff** the live schema against the declared `schema`/`indexes` — reuse `crate::diff::compute_diff`
   (`register_model/plan.rs:71`), unchanged.
3. **Verdict:**
   - **empty diff ⇒ ok.** Resolve the promise; cache the schema for the CRUD encryption pass exactly as
     today (`register_model/mod.rs:99` `c.cache_schema`). This is the happy path: the deploy-time engine
     already converged the schema, so the runtime sees zero drift.
   - **non-empty diff ⇒ FAIL CLOSED.** Reject the promise with a typed, coded error
     (`DbError::SchemaUnverified { code: "schema_unverified", missing: <diff summary> }`). The runtime
     **never applies DDL**.

This deletes `register_model/{bootstrap (the DDL/schema-create/role parts), validate, apply}` from the
runtime hot path. `plan.rs`'s introspect+diff is the only stage retained, lifted into a `verify` module.
The advisory lock, the `__zeroship_migrations` write, the `CREATE SCHEMA`, the `CREATE TABLE`, the
`CREATE INDEX CONCURRENTLY`, and the `validation_refused` strictness branch all leave the runtime.

### 4.2 Runtime behavior on a non-empty diff
The diff being non-empty means the deploy-time migration **was not applied** (or was applied to a
different shape than the code now declares). This is a deploy/operational fault, not an app-logic bug.

- **The `registerModel` promise rejects** with `schema_unverified`. Because schema install is driven by
  `installSchema` in `runtime-entry.ts` and is **decoupled from module evaluation** (the entry catches and
  does not block the event loop — `runtime-entry.ts:18-21,45`), the app module still loads. But the
  Collection wrappers will operate against a schema the verify pass flagged.
- **Decision: hard-fail the dispatch, not the boot.** The worker treats a `schema_unverified` rejection
  as a **dispatch-fatal** condition for that app: requests to the app return a gateway-visible
  **`503 schema_unverified`** (the app is deployed but its schema is not converged), with the diff summary
  in structured logs/metrics. Rationale:
  - **Fail-closed is the security/consistency win.** Serving requests against an unverified schema risks
    `db.users.find(...)` hitting a table whose columns differ from what the app's encryption/masking pass
    expects (P5 column-key/mask sentinels keyed off the cached schema) — a correctness and a
    data-exposure hazard. Refusing is safer than degrading silently.
  - **Not a `500` (app bug) and not a `402/blocked` (billing).** `503` with a distinct reason code tells
    the gateway + the creator dashboard "your last deploy's migration did not land; re-deploy or contact
    support," which is the true operator signal.
  - **No self-heal.** The runtime must **never** fall back to applying DDL — that would re-introduce
    exactly the privileged-runtime-DDL we are removing. This is the central behavior change and the
    headline risk (§13).
- The error surface is uniform: the worker maps `schema_unverified` to the `503` at the dispatch boundary
  (mirroring how `validation_refused` is mapped today at `register_model/mod.rs` via `to_op_error`); the
  gateway forwards the status + reason header.

### 4.3 Per-app role: moves to deploy
Today `register_model` bootstrap provisions `app_<app_id>_role` at runtime
(`register_model/bootstrap.rs:209`). With verify-only, **the per-app CRUD role provisioning moves to the
deploy-time migration flow** (§5), alongside the migrator role (§10). Rationale: role provisioning is DDL
(`CREATE ROLE` / `GRANT`), it is the same trust class as schema DDL, and the runtime should hold **zero**
privileged DDL capability after convergence. The runtime keeps only `SET LOCAL ROLE app_<app_id>_role`
for CRUD (`transaction::apply_per_app_role`) — which requires the role to already exist, exactly as the
verify model guarantees (deploy ran first).

> Open critic question: should verify *also* assert the per-app role exists (a cheap `pg_roles` probe) so
> a missing-role deploy surfaces as `schema_unverified` rather than a later `SET ROLE` failure? Recommended
> **yes** — fold a role-existence check into the verify verdict so the entire "deploy ran correctly"
> invariant is one fail-closed gate.

---

## 5. The deploy-time server flow (the "server")

### 5.1 Where it runs
A new migration step in the control plane, invoked inline by `deploy` (`api.rs:394-620`) **after** a
successful `deploy::ingest` (manifest validated, blobs stored) and **before** the manifest/route commit
that makes the app's new code resolvable to the gateway's 5s route pull. This ordering is the crux: the
schema must converge **before** the new code can serve a single request.

```
deploy():
  authz.require(AppsDeploy, App{id})            # api.rs:410 — owner from the authed path, never the body
  ingest(.zship)                                # blobs + manifest (now carries descriptors, §Phase 1)
  ── NEW ── migrate(app_id, descriptors)        # §5.2; engine runs here, under Confined ExecutorConfig
  set_deploy_with_manifest(...)                 # api.rs:605 — ONLY on migrate success; this go-lives the code
  reconcile oauth, return 200
```

### 5.2 The automatic (declarative) migration path
From the manifest's descriptor set the control plane:

1. Builds `DesiredSchema` via `declarative::desired_snapshot(project_schema = "<app_id>", descriptors)`
   (`declarative.rs:358`). With one app, `owner_app` is the deploying app on every descriptor;
   `live_ownership` is "every live table → this app".
2. Introspects live via the engine's `drift::snapshot_schema` (the engine's own introspection, NOT
   plugin-db's — see §7 on which side owns introspection at deploy).
3. `engine.plan_declarative(desired, live, live_ownership, author, hints, &guard_cfg)`
   (`engine.rs:223`) → `DeclarativeDeployPlan` (plain + renames).
4. **Shadow dry-run** (recommended, §11): `engine.dry_run_declarative(admin_conn, &plan, &desired,
   exec_cfg, shadow_cfg, applied_by)` (`engine.rs:675`) against a throwaway clone — catches bad SQL,
   constraint violations, and (via the desired-snapshot re-introspection) proves the plan reaches the
   declared shape before touching the real DB.
5. **Stamp + apply, verified:** compute `plan.manifest()` (`engine.rs:906`) over the SAME generated plan
   instance, then `engine.apply_declarative_verified(&plan, &expected, approval, conn, exec_cfg,
   applied_by)` (`engine.rs:390`). The verified path re-checks the integrity manifest before the lock or
   any DDL (defends against in-flight tamper), then runs the gated, advisory-locked, least-priv apply.
   - **The plan must be generated once and held in-process** (`DeclarativeDeployPlan` is not `serde`;
     `engine.rs:906` rustdoc). Since stamp + apply happen in the same `deploy()` request, this is free —
     hold the plan in a local and stamp it immediately before applying.
6. **Auto-applies only the un-gated set.** A purely additive declarative deploy (`CREATE TABLE`,
   `ADD COLUMN`, `CREATE INDEX`, `DROP NOT NULL`) flows through with `Approval::NotApproved`. The instant
   the plan contains a **gated** op (any `DROP`, a type change, `SET NOT NULL`, a `UNIQUE`-index drop,
   or a rename's contract — `declarative.rs:952-979`), `apply_declarative` returns
   `EngineError::ApprovalRequired` and the deploy is **blocked** with a structured "migration requires
   review" error pointing the creator at the explicit endpoint (§5.4).

### 5.3 Sync vs async — recommendation
- **Recommendation: SYNCHRONOUS for the additive auto-path.** Additive DDL on a pre-launch creator app is
  fast and bounded by `statement_timeout`/`lock_timeout` (`db.rs:38-42`). Running it inline keeps the
  deploy↔go-live ordering trivially correct (the `200` means "code + schema both live") and needs no new
  infra. `CREATE INDEX CONCURRENTLY` is the one long pole; the engine already runs it two-phase outside
  the txn (design §2.3), and for the deploy path we keep it inline behind `statement_timeout` — a long
  index build extends the deploy, which is acceptable for additive deploys.
- **Async job for the explicit/gated path.** Hand-authored destructive migrations, backfills, and
  expand-contract sequences can be long-running and require human approval. These do **not** belong inline
  in a deploy request. They run as an **async migration job** (§5.4) with a status table + poll endpoint —
  the **minimal** job infra (no general queue; control has no async-job infra today, §2.3).

### 5.4 The explicit migration endpoint (`submit_migration`-backed)
For the operations `register_model` always refused — renames, destructive drops, backfills,
expand-contract, and any hand-authored SQL — a new authenticated endpoint backs the engine's
`submit_migration` adapter (`submit.rs:409`):

```
POST   /api/apps/{id}/migrations          # submit a migration (declarative-regen OR hand-authored SQL)
GET    /api/apps/{id}/migrations/{job}    # poll status
GET    /api/apps/{id}/migrations          # list journal (net-applied history)
```

- **Auth:** `authz.require(AppsDeploy, Resource::App { id })` — `owner_app` is the **authenticated path
  app**, NEVER the request body (the body's `owner_app` is untrusted; the adapter already enforces
  ownership against a caller-supplied map, NOT the submitter's claim — `submit.rs:217-229`).
- **Body:** `{ up, down?, name, depends_on?, repeatable?, timeout_ms? }` → `submit::Submission`
  (`submit.rs:132`). Deliberately **no** `destructive`/`requires_approval` field — destructiveness is a
  server judgement by the guard (`submit.rs:127-130`).
- **Pipeline (server-side, async job):** `submit_migration` runs guard → lint → **live-seeded shadow
  dry-run** → gate → apply → journal. A guard denial / dry-run failure / approval-required are
  `SubmissionOutcome` *verdicts* (not errors), surfaced as job statuses; an infra fault is `SubmitError`.
- **Approval gate:** a destructive/gated submission returns `ApprovalRequired` with the linter advisories
  and a passing dry-run; the operator (creator, via the dashboard) re-submits with `approval = Approved`.
  The approval decision is a server-side action on the authenticated path, recorded in the journal's actor
  field.
- **Async status table** (new, control schema):
  `migration_jobs(job_id pk, app_id, kind, status, outcome_json, error, submitted_by, created_at,
  updated_at)`, where `status ∈ {queued, running, approval_required, applied, denied, dry_run_failed,
  failed}`. A control cron (the existing detached-cron mechanism under `crates/control/src/cron/`) drains
  `queued` jobs; the poll endpoint reads the row. This is the **minimal** async surface — one table, one
  drainer, no general queue.

### 5.5 Ordering & failure
- **Migrations apply BEFORE new code serves.** The migrate step sits before `set_deploy_with_manifest`
  (`api.rs:605`), which is what publishes the route the gateway pulls. A deploy whose migrate step fails
  **never commits the manifest** → the gateway keeps serving the *previous* bundle against the *previous*
  (still-matching) schema. No half-deployed state is observable.
- **A failed migration blocks the deploy.** `migrate()` returning `Err` (denial, approval-required, dry-run
  failure, executor error) maps to a `4xx/5xx` deploy response with the structured reason; the artifact is
  ingested (blobs stored, idempotent) but not go-lived.
- **Rollback semantics.** The engine applies transactional batches atomically with the journal and
  non-transactional ops two-phase + idempotent recovery (design §2.4). A mid-apply failure leaves the
  journal in `started`-only for the failed step; the next deploy's recovery path reconciles. The deploy
  response reports the failure; the creator fixes the schema and re-deploys (roll-forward is the default;
  true rollback is the separately-gated `engine.rollback`).
- **Expand-contract is multi-deploy by construction.** A rename's expand lands at deploy N (with the real
  backfill), and its contract is returned as `pending_contract` (`engine.rs:443-446`) — applied at deploy
  N+1 *after* the app's code has switched to the new column. The control plane stores `pending_contract`
  keyed to the app and surfaces it as a follow-up migration job; it is NEVER auto-applied in the same
  deploy (the executor's expand/contract gate would refuse it anyway).

---

## 6. The client (`@zeroship/control`)

`ControlClient` (`sdks/control/src/index.ts:149`) gains a `migrations` group beside `apps`/`env`:

```ts
client.migrations.submit(appId, {
  up, down?, name, dependsOn?, repeatable?, timeoutMs?, approval?,
}): Promise<MigrationJob>            // POST /api/apps/{id}/migrations
client.migrations.status(appId, jobId): Promise<MigrationJob>   // GET .../migrations/{job}
client.migrations.list(appId): Promise<MigrationJournalEntry[]> // GET .../migrations
```

`MigrationJob = { jobId, status, outcome?, advisories?, dryRunOk?, error? }` mirroring `SubmissionOutcome`.
The builder/deploy caller uses `migrations.submit` for hand-authored/destructive migrations; the **automatic**
additive path needs **no** client call — it runs inside `deploy()` server-side.

**Internal Rust client:** the control plane calls the engine in-process (it depends on `zeroship-migrate`
directly — see Phase 2), so no internal HTTP/RPC client is needed. The migrate crate stays a library
dependency, invoked from `deploy()` and the migration-job drainer.

---

## 7. Unifying journal + advisory lock + type-map

These three were independently invented by both engines; after convergence they must be the **same** or
verify-vs-apply will disagree.

### 7.1 One journal
- **Drop** plugin-db's `"<app_id>".__zeroship_migrations` write entirely (it leaves the runtime with the
  `apply` stage in §4).
- The **engine's** `<meta_schema>.schema_migrations` (+ `_inflight`/`_rolled_back`/`_supersedes`,
  `journal.rs:154-260`) becomes the single journal, written **only at deploy** by the engine under the
  admin role (the migrator cannot write it — `role.rs:254` REVOKE).
- `meta_schema` for an app is `"<app_id>__migrations"` (a sibling schema, NOT a table inside `"<app_id>"`),
  so it stays off the migrator's privilege path. (The old in-schema `__zeroship_migrations` table name is
  retired; pre-launch, no back-compat — per AGENTS.md.)
- **Verify-only does NOT read the journal.** Verify compares live introspection vs declared schema
  (§4.1); it does not consult migration history. This keeps the runtime's read surface minimal and means
  the two never race on the journal (the runtime never touches it). The journal is a deploy-time +
  dashboard concern.

### 7.2 One advisory lock
- Both used a per-app lock; converge on the engine's `pg_advisory_lock(hashtext(project_id))` with
  `project_id = app_id`. The runtime's old `hashtext("<app_id>:register_model")` lock
  (`register_model/bootstrap.rs:66-69`) is **removed** (verify takes no lock — it is a read-only diff;
  concurrent verifies are harmless and idempotent).
- The deploy path's `apply_declarative` already holds the project lock for the whole declarative deploy
  (H10, `engine.rs:293-334`), serialising concurrent deploys for the same app — exactly the property the
  runtime lock used to provide, now owned by the one place that mutates schema.

### 7.3 One type map (shared crate)
The duplication (`declarative.rs:208-229` ↔ `query.rs:2043`/`1827`; `declarative.rs:242-253` ↔
`query.rs:856`) must be lifted into a **single shared crate** consumed by BOTH the engine's differ AND
`registerModel`-verify. After convergence, divergence is no longer a latent fidelity bug — it is a
**verify-vs-apply mismatch** that bricks every deploy (the engine materialises column type X; verify
expects type Y; verify fails closed forever).

- **Where it lives — the trust-domain constraint.** The migrate crate must stay **independent of the
  runtime plugin** (it is a different trust domain; `declarative.rs:30-35` calls this out, and
  `zeroship-migrate/Cargo.toml` deliberately does not depend on `plugin-db`). So the shared map **cannot**
  live in `plugin-db`, and `plugin-db` must not depend on `zeroship-migrate` for it either.
- **Proposal: a new tiny leaf crate `zeroship-db-types`** (no I/O, no DB driver, no V8 — just the
  DSL→PG type table, the system-field column set, and the deterministic index/FK/PK naming helpers).
  - `zeroship-migrate` depends on it (replacing `declarative.rs:208-253`).
  - `plugin-db` depends on it (replacing `query.rs:856/1827/2043` and the verify path's expectations).
  - It depends on neither, so no trust-domain edge is created (plugin-db → leaf and migrate → leaf are
    both safe; the dangerous edge would be migrate → plugin-db, which this avoids).
- The crate exposes the canonical map in BOTH spellings the two consumers need: the DDL spelling
  (`TEXT`, `DOUBLE PRECISION`, `TIMESTAMPTZ` — what the engine emits in `CREATE`/`ALTER`) and the
  `information_schema.data_type` spelling (`text`, `double precision`, `timestamp with time zone` — what
  introspection reports and verify compares against). These are two views of one source of truth, so they
  cannot drift.
- The existing `tests/declarative_pg.rs` round-trip becomes the **cross-consumer** contract test (§14):
  desired-snapshot → CREATE via engine → introspect → must equal both the engine's desired snapshot AND
  the verify path's expectation.

### 7.4 Who introspects at deploy
The deploy path uses the **engine's** `drift::snapshot_schema` for the plan, and verify uses
**plugin-db's** `introspect_schema`. These are two introspection implementations producing two snapshot
shapes (`drift::SchemaSnapshot` vs `diff::LiveSchema`). They do **not** need to be the same type, but they
must agree on the *type spellings* — which §7.3 guarantees by sharing the type map. The contract test
(§14) pins this: a table the engine creates must introspect identically under both readers (modulo shape
representation). This is the safest split that does not force one crate to depend on the other.

---

## 8. Operational

### 8.1 CREATEDB admin DSN — recommend wiring it
The engine's shadow dry-run (`engine.rs:658,675` → `crate::shadow`) needs `CREATE DATABASE` / `DROP
DATABASE` privileges to clone a throwaway DB. Control today has a single `--db` DSN and **no** admin DSN
(§2.3).

**Recommendation: add an optional `--admin-db` DSN** (a role with `CREATEDB`, distinct from the app DSN).
- Deploy-time migrations are **untrusted creator SQL**, so the shadow dry-run is high-value: it proves the
  plan applies cleanly on a faithful clone before touching the real DB. Skipping it on the deploy path
  would weaken the very safety story this convergence exists to deliver.
- If `--admin-db` is **absent** (e.g. a minimal dev deployment), the deploy path degrades to
  `apply_declarative_verified` **without** the shadow pre-flight (the additive auto-path is low-risk), and
  the **explicit/gated** path (which mandates the live-seeded shadow inside `submit_migration`,
  `submit.rs`) is **disabled** with a clear operator error — destructive migrations require the admin DSN.
  This keeps dev bootable while making the safe default explicit.

### 8.2 Role: dedicated `migrator_<app_id>`, not the CRUD role
Use the engine's dedicated least-priv `migrator_<app_id>` (`role.rs:154,187`) for DDL — NOT the
CRUD-scoped `app_<app_id>_role`.
- **Why not reuse `app_<app_id>_role`:** it is CRUD-scoped (DML + `ALTER DEFAULT PRIVILEGES`) and is the
  role app code runs under via `SET LOCAL ROLE`. Granting it the DDL/owns-schema posture the migrator
  needs would *raise* the privilege of the role app code uses — the opposite of least-privilege.
- **`migrator_<app_id>`** is `NOLOGIN`, owns `"<app_id>"`, can DDL within it, has `ALTER DEFAULT
  PRIVILEGES FOR ROLE migrator`, and has **no** access to the meta schema (so a creator migration cannot
  forge its own history — `role.rs:254`). The deploy admin connection `SET ROLE`s into it per migration.
- **search_path vs qualified names — reconcile.** plugin-db emits fully **qualified** names and relies on
  no `search_path` (§2.1). The engine **pins** `search_path` to the project schema during apply (`db.rs:33`)
  AND the differ also emits qualified names (`declarative.rs:905` `qualified`). Both-qualified-and-pinned
  is consistent and safe: the qualified names are authoritative; the pinned `search_path` is belt-and-
  suspenders for any unqualified reference in hand-authored SQL (the guard's confinement also bounds
  cross-schema references for Confined trust). No conflict — they reinforce each other.
- **Provisioning** of both `migrator_<app_id>` (engine, `role::provision_migrator`) and the CRUD
  `app_<app_id>_role` (lifted out of runtime bootstrap, §4.3) happens in the deploy migrate step, before
  apply. Both are idempotent.

---

## 9. Migration source for an app

An app's migrations come from **two** sources, both deploy-time:

1. **Auto-generated (declarative)** — the default. The differ generates versioned migrations from the
   declared descriptor set (the manifest's schema) diffed against live, every deploy. This is the
   `register_model`-replacement path and covers the common case (additive evolution).
2. **Hand-authored (explicit)** — shipped/submitted for the operations the differ cannot or must not do
   automatically: destructive drops, renames (which need a `RenameHint` — `declarative.rs:161`), backfills,
   data migrations, expand-contract. Submitted via `client.migrations.submit` (§6) → `submit_migration`.

**Where hand-authored files live:** for v1, hand-authored migrations are **submitted via the endpoint**
(not shipped in the `.zship`). Rationale: a migration is a *gated, reviewable* artifact with an approval
step and a server-derived destructiveness verdict; smuggling it inside the deploy bundle would conflate
"ship code" with "approve a destructive schema change." Keeping them on a separate authenticated endpoint
makes the approval gate first-class.

> Open critic question / FUTURE: a dbmate/Flyway-style `migrations/` directory shipped in the `.zship`
> (loaded via the engine's existing `loader.rs`) is attractive for reproducible, version-controlled
> migrations. It is deferred (§12) because it needs a build-pipeline decision (the Vite plugin would have
> to bundle the directory) and a deploy-time "apply pending file migrations" step ordered against the
> declarative diff. The engine's `loader` already supports it; only the pipeline wiring is missing.

**Deploy-time pipeline (combined):**
```
1. Provision roles (migrator_<app>, app_<app>_role) — idempotent
2. Ensure journal (meta schema + schema_migrations*) — idempotent
3. desired_snapshot(descriptors) → live introspect → plan_declarative
4. [if --admin-db] dry_run_declarative on a shadow clone
5. stamp manifest → apply_declarative_verified  (additive auto; gated ⇒ block + point at endpoint)
6. store any pending_contract as a follow-up job
7. commit manifest + routes  (go-live)
```

---

## 10. Phased implementation plan

Each phase is independently shippable and keeps the platform bootable. Order is chosen so the prerequisite
(schema in the artifact) lands first, the engine is wired but inert, then the runtime cutover happens last.

**Phase 1 — Materialise the declared schema into the deploy artifact (prerequisite).**
Crates/SDKs: `bundle`, `sdks/vite-plugin`, `sdks/bootstrap`.
- Emit the per-collection descriptor set (the `CollectionDescriptor` JSON — name, fields, indexes) into
  `manifest.json` at build time (the Vite plugin already discovers `default.schema`; serialise its
  descriptors instead of relying on runtime `installSchema`).
- Add a typed `schema_descriptors` field to `Manifest` (`bundle/src/manifest.rs`) + validation.
- **Shippable:** the manifest carries the schema; nothing consumes it yet. Runtime unchanged.

**Phase 2 — Extract the shared type-map crate.**
Crates: new `zeroship-db-types`; `plugin-db`, `zeroship-migrate` depend on it.
- Lift the DSL→PG map, system-field columns, and naming helpers out of `query.rs:856/1827/2043` and
  `declarative.rs:208-253` into the leaf crate; both crates re-import.
- Add the cross-consumer round-trip contract test (§14).
- **Shippable:** pure refactor, no behavior change; both engines now share one map.

**Phase 3 — Wire the engine into control (deploy migrate step), behind a flag, additive-only.**
Crates: `control` (add `zeroship-migrate` dep), `zeroship-migrate` (app-scoped `ExecutorConfig` helper),
SDK `@zeroship/control` (`migrations` group), bundle (read descriptors).
- Add `--admin-db` (§8.1), the migrate step in `deploy()` (§5) gated by a `ZEROSHIP_DEPLOY_MIGRATE` flag,
  role provisioning at deploy (§4.3, §8.2), the one journal/lock/meta-schema (§7), the
  `migration_jobs` table + drainer + `/migrations` endpoints (§5.4), and the client group (§6).
- In this phase `register_model` **still applies DDL** at runtime (verify-only not yet on); the deploy
  migrate step is additive + idempotent, so running both is safe (the runtime apply sees zero diff after
  the deploy converged).
- **Shippable:** deploy-time migration works; runtime is belt-and-suspenders. Flag off = today's behavior.

**Phase 4 — Flip `registerModel` to verify-only.**
Crates: `plugin-db` (gut `register_model` to a `verify` module reusing introspect+diff), `worker`/`runtime`
(map `schema_unverified` → `503`), control (flag on by default).
- Delete the runtime `apply`/`bootstrap`(DDL parts)/`validate`/advisory-lock/`__zeroship_migrations`-write;
  keep introspect+diff as verify (§4). Implement the fail-closed dispatch behavior (§4.2).
- **Shippable:** the convergence is live. Deploy is the sole DDL authority; runtime verifies.

**Phase 5 — Explicit destructive/rename/backfill path GA + cleanup.**
Crates: `control`, `zeroship-migrate`, `@zeroship/control`, builder.
- Productionise the gated `/migrations` flow (approval UX in the dashboard, `pending_contract` follow-ups,
  expand-contract surfacing). Delete dead runtime code paths (the `validation_refused` strictness branch,
  the per-app role provisioning in runtime bootstrap, the runtime advisory lock).
- **Shippable:** full convergence including the operations `register_model` could never do.

---

## 11. Shadow dry-run on the deploy path

- The **additive auto-path** SHOULD run `dry_run_declarative` when `--admin-db` is present (§8.1): cheap,
  catches a bad generated plan before the real apply, and validates the post-apply shape against the
  desired snapshot.
- The **explicit/gated path** MUST run the live-seeded shadow inside `submit_migration` (`submit.rs` —
  it is built into the adapter), so a destructive/hand-authored migration is proven on a faithful,
  data-seeded clone before the real DB is touched. Without `--admin-db`, this path is disabled (§8.1).

---

## 12. Non-goals

- **The full project-umbrella multi-app-shared-DB model.** Multiple apps sharing one project DB with a
  union schema and per-table ownership is a FUTURE extension. The engine already supports it
  (`DesiredSchema.ownership`, `live_ownership`, the cross-app FK + ownership errors); this design uses the
  one-app-per-project specialisation and does not wire the union assembly, project registry, or
  membership table.
- **dbmate/Flyway-style `migrations/` directory shipped in the `.zship`.** Deferred (§9); the engine's
  loader supports it, only the build-pipeline wiring is missing.
- **SQLite/dev-tier convergence.** This design targets the Postgres platform path. The dev-tier
  (`env.db` → SQLite) keeps its in-process `register_model` for now; aligning dev to the verify model is a
  follow-up (the dev path has no separate deploy step). The SQLite arm of `register_model` (`mod.rs:253`)
  is untouched here.
- **Removing the engine's `Platform`/`Trusted` trust profiles.** Out of scope; the creator path is
  `Confined` only.
- **General async job queue for control.** We add exactly one `migration_jobs` table + one cron drainer
  (§5.4); a generic queue is not in scope.

---

## 13. Risks

- **R1 — The runtime can no longer self-heal schema (headline behavior change).** Today a cold start with a
  stale schema *fixes itself* by applying additive DDL. After convergence it **fails closed** (`503
  schema_unverified`). If a deploy's migrate step is skipped, fails silently, or races a manifest commit,
  the app serves `503` until re-deployed. *Mitigation:* migrate runs **before** the manifest go-live commit
  (§5.5), so a failed migrate never publishes the code; the verify diff summary is logged + metered + shown
  in the dashboard; the explicit endpoint lets an operator converge without a code change.
- **R2 — The deploy-migrate-failure path is now load-bearing.** A wedged migration blocks the deploy. A
  long `CREATE INDEX CONCURRENTLY` extends deploy latency; a denied/gated op blocks an additive deploy if a
  destructive change slipped into the declarative diff. *Mitigation:* `statement_timeout`/`lock_timeout`
  bound runaway DDL; the additive auto-path never gates (gated ops are routed to the explicit endpoint);
  shadow dry-run pre-flights the plan; roll-forward is the default recovery.
- **R3 — Type-map drift becomes a hard deploy-bricking failure.** If the engine emits one column type and
  verify expects another, **every** request `503`s after deploy. This is strictly worse than today's latent
  fidelity bug. *Mitigation:* the single shared `zeroship-db-types` crate (§7.3) makes drift impossible by
  construction; the cross-consumer round-trip test (§14) is a CI gate; both DDL and information_schema
  spellings derive from one source.
- **R4 — Manifest must carry the schema (new artifact dependency).** If Phase 1's descriptor emission is
  incomplete (a collection the Vite plugin fails to discover), the deploy diff sees a missing table and the
  engine creates/drops incorrectly, or verify fails. *Mitigation:* Phase 1 ships and bakes before any
  consumer; the descriptor set is validated at manifest-validation time (`bundle/src/manifest.rs`); the
  shadow dry-run catches a wrong plan.
- **R5 — Privileged DDL moves from runtime to control.** Control now holds an admin/`CREATEDB` DSN and runs
  untrusted creator SQL through the engine. *Mitigation:* this is exactly what the engine's Confined
  profile + least-priv `migrator` role + parse-time deny-list + shadow dry-run are designed for; the admin
  DSN is used ONLY for `CREATE/DROP DATABASE` of the shadow and `SET ROLE migrator`, never to run creator
  SQL directly.
- **R6 — Two introspection implementations (engine vs plugin-db).** They could disagree on edge cases not
  covered by the shared type map (e.g. constraint definition spelling). *Mitigation:* the round-trip
  contract test (§14) pins agreement; verify compares against the declared schema, not the engine's
  snapshot, so the comparison is "live vs declared" on both sides using the shared type vocabulary.

---

## 14. Test strategy (faithful, real-PG)

All against a real Postgres (the project's `:5440` harness; no shims — per `feedback_faithful_e2e_tests`).

1. **deploy → migrate → runtime-verify-passes (the happy path).** Deploy a `.zship` whose manifest carries
   a fresh descriptor set → control's migrate step provisions roles + journal + applies additive DDL via
   `apply_declarative_verified` → boot the worker → `registerModel`-verify introspects + diffs → **empty
   diff ⇒ ok**, requests served. Assert: schema exists, journal has the migration rows, verify resolves,
   `200` on a CRUD round-trip.
2. **schema-behind → verify-fails-closed.** Deploy code declaring a NEW column but skip/disable the migrate
   step → boot worker → verify sees a non-empty diff → `registerModel` rejects `schema_unverified` → the
   dispatch returns **`503`** with the reason code, and **no DDL was applied** by the runtime (assert the
   column is still absent). This is the central regression-proof for "the runtime never self-heals."
3. **destructive via the explicit endpoint.** `client.migrations.submit` a `DROP COLUMN` → job goes
   `approval_required` with advisories + `dry_run_ok` → re-submit with approval → applied; journal records
   it; a non-owner / body-spoofed `owner_app` is rejected (`OwnershipDenied`); guard-denied SQL (RCE/cross-
   tenant) returns `Denied` and the real DB is untouched.
4. **rename (expand-contract) across two deploys.** Deploy N with a `RenameHint` → expand + real backfill
   land, `pending_contract` returned and stored; deploy N+1 applies the contract (gated, approved) →
   `DROP COLUMN <from>`; assert every row's value survived in `<to>` (zero data loss).
5. **shared-type-map round-trip (the cross-consumer contract).** For every supported DSL type: engine
   `desired_snapshot` → `CREATE` via `apply_declarative` → introspect under BOTH the engine's
   `drift::snapshot_schema` AND plugin-db's `introspect_schema` → both must equal the declared shape using
   the shared `zeroship-db-types` spellings. A drift in either consumer fails this test (the §13-R3 guard).
6. **failed migrate blocks go-live.** Inject a migrate failure (e.g. a guard denial in the generated plan) →
   assert `deploy()` returns the structured error, the manifest/route commit did NOT happen
   (`set_deploy_with_manifest` not called), and the gateway still serves the previous bundle.
7. **concurrency.** Two simultaneous deploys for the same app serialise on the single project advisory lock
   (`hashtext(app_id)`); the second waits then no-ops the already-applied set. A deploy concurrent with a
   runtime verify does not race (verify takes no lock, reads only).
8. **role least-privilege.** Assert `migrator_<app_id>` cannot write the meta schema journal, cannot reach
   `public`, and that `app_<app_id>_role` is unchanged (still CRUD-only) — i.e. convergence did not raise
   the privilege of the role app code runs under.
