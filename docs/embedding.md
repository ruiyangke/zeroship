# zero-migrate — embedding & customization

This document is for **hosts** that embed the engine — either a Rust host that
depends on the `zero-migrate` crate directly, or a Node host that drives the
`zero-migrate-node` addon (typically via the `zero-migrate-engine` npm package).

The engine is deliberately small and stateless. `MigrationEngine` holds no
configuration of its own; everything a host customizes flows through three
seams:

1. **Per-engine / per-run configuration** — `ExecutorConfig` (project identity,
   schema confinement, timeouts, the migrator role, extension-resolution
   schemas), plus `GuardConfig` (the guard's trust posture) and the
   `SentinelPrefix` codec knob.
2. **Per-apply identity** — the arguments the host threads into each `apply`
   call: the plan, the approval verdict, the backend, the `ExecutorConfig`
   (carrying project id/schema and migrator role), and the audit `applied_by`
   label. On the load gate, the deploying app id and the ownership registry.
3. **The injected runtime dependency** — a `driver::SqlSession` implementation
   for the network dialects (Postgres, MySQL). SQLite needs no injection (it is
   in-process rusqlite).

---

## Rust host: constructing an engine + a backend

`MigrationEngine` is a unit struct constructed with `MigrationEngine::new()`; it
serves every project because all per-run state is passed per call.

```rust
use zero_migrate::{
    MigrationEngine, ExecutorConfig, GuardConfig, Approval,
};

// 1. Configure the run. `ExecutorConfig::new(project_id, project_schema)` seeds
//    conservative defaults: the meta (journal) schema derives as
//    "<project_schema>_migrations", statement_timeout = 60s, lock_timeout = 3s,
//    no SET ROLE, and ["public"] as the extension-resolution schema.
let cfg = ExecutorConfig::new("prj_...", "app_1234")
    .with_migrator_role("migrator_app_1234");   // least-privilege apply role

// 2. Lint + preview a migration set (read-only, no DB). The guard runs over
//    every migration's `up`; a denial lands in `plan.denied`, a destructive op
//    flags `plan.requires_approval`.
let engine = MigrationEngine::new();
let guard_cfg = GuardConfig::confined(cfg.project_schema.clone());
let plan = engine.plan(&migrations, &guard_cfg);

// 3. Apply. The gate refuses a denied plan and refuses a destructive plan
//    without approval, then delegates to the executor — which independently
//    re-runs the guard and the migrator role (defense in depth).
let outcome = engine
    .apply(&plan, Approval::Approved, &backend, &cfg, "deploy")
    .await?;
```

`apply` is generic over `B: MigrationBackend`. Choose the backend per dialect:

- **Postgres / MySQL** — construct `PostgresBackend<S>` / `MysqlBackend<S>` over
  an injected `S: SqlSession` (see [`driver-authors.md`](./driver-authors.md)).
  These modules compile behind the `pg_seam` cfg (from the `host-pg` feature).
- **SQLite** — construct `SqliteBackend`, an in-process rusqlite actor. No
  session injection; always available.

### Seam #1 — `ExecutorConfig` (per-run config)

`ExecutorConfig` carries the engine-neutral fields directly and groups the
Postgres-specific parameters under `pg: PgConfinement`:

| Field | Meaning |
| --- | --- |
| `project_id` | The project id — its bytes seed the apply-serializing advisory lock (`pg_advisory_lock(hashtext(project_id))`). |
| `project_schema` | The one schema this project's migrations own and may touch; pinned into `search_path` and the guard's confinement target. |
| `pg.meta_schema` | The per-project meta schema holding the append-only `schema_migrations` journal (defaults to `<project_schema>_migrations`). |
| `pg.statement_timeout` | Per-statement run budget (`SET statement_timeout`, default 60s). |
| `pg.lock_timeout` | The **separate, short** lock-acquisition budget (`SET lock_timeout`, default 3s) — the lock-safety envelope so a blocking DDL fails fast instead of stalling a live tenant table. |
| `pg.migrator_role` | The least-privilege `migrator` role the apply flow runs each migration under via `SET ROLE`/`RESET ROLE`. `None` runs as the connecting role (dev/test only). |
| `pg.extension_schemas` | Schemas hosting shared extension types the engine emits unqualified (e.g. `public` for pgvector's `vector`, PostGIS's `geography`). Appended to the migrator `search_path`; the migrator gets `USAGE` only (resolution, never CREATE). |

The trust posture (`Confined` / `Platform` / `Trusted`) and its allowlists are
private (`pub(crate)`): the default `ExecutorConfig::new` builds a **Confined**
config (the creator path), and the privileged profiles are reachable only
through token-gated in-crate seams, so an external embedder cannot flip the
executor into a privileged posture.

### The sentinel-prefix knob

The persisted encryption/mask sentinel prefixes are a `SentinelPrefix` value
(`schema::mask_codec`), defaulting to `zero-migrate:enc:` / `zero-migrate:mask:`.
A host that must interoperate with a legacy writer sharing the same schema
injects that writer's prefix (for example the legacy `zsenc:`); the standalone
default carries this project's own brand so no stranger's `pg_dump` carries a
foreign one. Both codec directions (`build_*_with` / `parse_*_with`) take the
prefix so build and parse stay symmetric.

### The policy seam

The guard's trust posture is selected by which `GuardConfig` constructor a host
passes:

- `GuardConfig::confined(project_schema)` — the creator path (deny-list +
  single-schema confinement). Dialect peers exist: `confined_sqlite`,
  `confined_mysql`.
- `GuardConfig::platform(cap, schemas, exts)` — a cross-schema allowlist +
  `CREATE EXTENSION` allowlist, token-gated.
- `GuardConfig::trusted(cap)` — the deny-list is skipped entirely (the public
  dbmate-like posture), token-gated.

The richer operator-ceiling ⊓ author-draft policy is modeled by `PolicyProfile`
/ `SealedProfile` + `seal_effective_profile` (re-exported from the engine root),
used where a host seals an effective profile from an operator ceiling and an
author draft. A privileged profile without a token fails closed to Confined.

### Per-apply identity

Identity is threaded per `apply` call, distinct from the config:

| Argument | Concern |
| --- | --- |
| `plan: &MigrationPlan` | The linted dry-run preview from `engine.plan(...)`. |
| `approval: Approval` | `Approved` / the caller's approval verdict — the gate refuses a destructive plan without it. |
| `backend: &B` | The dialect backend (carries the injected `SqlSession` for the network dialects). |
| `exec_cfg: &ExecutorConfig` | Project id/schema, migrator role, timeouts. |
| `applied_by: &str` | The audit label recorded in the journal. |

For the `.ir.json` **load gate**, ownership identity is threaded through
`load_ir_document` / `enforce_ir_ownership`: the deploying app id is
server-stamped as `owner_app`, and a `{ live table → owning app }` registry is
supplied so a partial-union deploy cannot mass-drop another tenant's tables
(fail-closed drop ownership). `apply_with_lock` additionally takes an explicit
`LockMode` (`Acquire` vs `AlreadyHeld`) for an outer caller that already holds
the project advisory lock.

---

## Node host: the napi verbs

A Node host loads the addon and calls its typed verbs. There is **no** JSON
string plumbing — every verb takes and returns typed DTOs (`wire.rs` is the
source of truth; TS imports the generated `.d.ts`). Integers cross as `bigint`
(napi6).

| Verb | Kind | Purpose |
| --- | --- | --- |
| `irVersion()` | sync, DB-free | The IR-format version this addon was built against (the fail-closed floor + the single source of truth for the recorder's envelope). |
| `loadVerify(irJson, deployingApp, dialect, registry)` | sync, DB-free | Load + verify an IR document: structural + confinement + ownership validation. Returns a typed `LoadVerifyReply`; never throws for a malformed document. |
| `applyIr(hostDriver, request)` | async, host-driven | Lower the authored envelope in Rust (stamp `owner_app`, fold `Checksum::of_ir`), then drive `executor::apply` over the injected host driver. |
| `status(hostDriver, request)` | async, host-driven | Reconcile against the live journal over the host driver. |
| `history(hostDriver, request)` | async, host-driven | The journal audit trail over the host driver. |

The `zero-migrate-engine` npm package wraps these into an ergonomic facade so a
creator never sees N-API or the host-driver callback:

```ts
import { apply, plan, status, history, validate } from "zero-migrate-engine";

await apply({
  migration,                         // the imported migration module
  ownerApp: "app_1234",              // stamped as owner_app + folded into checksum
  projectSchema: "app_1234",
  driver: { kind: "postgres", url }, // or { kind: "mysql", url }
  migratorRole: "migrator_app_1234",
  approved: false,
});
```

- `apply` — authors the `{ ir_version, name, ops }` envelope with the pure-JS
  recorder, opens a pinned `pg`/`mysql2` session, and drives `applyIr` over it.
- `plan` / `validate` — the DB-free pre-checks (`loadVerify`); no driver needed.
- `status` / `history` — reconcile / audit against the live journal.

The facade takes a `DriverConfig` of `{ kind: "postgres" | "mysql", url }`;
SQLite has no host-driver path (it runs in-process via rusqlite and never
crosses the seam). A full shadow `dryRun` is deferred in v1 (`backend.shadow()`
is `None` on the `host-pg` build).

### `rollback`

Rollback is available in the Rust engine (`RollbackRequest` / `RollbackOptions` /
the executor rollback path). The napi addon surfaces the apply/status/history
verbs plus the DB-free load-verify; a host that needs rollback drives it through
the engine's rollback surface directly (it is not exposed as a host facade verb
in v1).

---

## The CLI

`zero-migrate-engine` ships the one CLI (`bin: zero-migrate`, `cli.ts`) over the
same facade verbs plus pure-JS scaffolding:

- `zero-migrate new <name>` — scaffold a fresh `<14-digit-ts>_<name>.ts` op-DSL
  migration.
- `zero-migrate plan [dir]` — DB-free load + structural/confinement/ownership
  verify of every migration in `dir` (the fast pre-apply gate, offline).
- `zero-migrate preview [dir]` — DB-free: print the authored
  `{ ir_version, name, ops }` envelope for each migration in `dir`.
- `zero-migrate apply [dir]` — apply every migration in `dir` over the
  `--database-url` driver, in order.
- `zero-migrate status [dir]` — reconcile against the live journal over the
  `--database-url` driver.

See also [`architecture.md`](./architecture.md) for the crate structure and
[`driver-authors.md`](./driver-authors.md) for the `SqlSession` contract.
