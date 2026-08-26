# Proposal: apply migrations ahead of the runtime on the SQLite dev tier

**Status:** PROPOSED — not implemented · **Tier:** T1 (dev-tier schema init) · **Date:** 2026-08-09

This is a self-contained implementation spec. Everything needed to build it is
here; there is no companion branch.

---

## TL;DR

On the SQLite dev tier the **worker runtime creates the schema** — `env.db`'s
`registerModel` drives the migration engine's *declarative* apply, one collection
at a time, lazily, in registration order. That is backwards from the platform
model: on Postgres a **migration process applies the schema ahead of the worker**
(the `migrated` service, at deploy) and `registerModel` is a pure no-op. The
anomaly is the root of a class of dev-only bugs (the "SCHEMA-INIT bug") and is why
a migration-first app with a foreign key (`examples/db-todos`) does not run.

**Proposal:** make dev match prod. Apply the committed `migrations/*.ts` to the dev
SQLite file **at dev-server boot, ahead of the worker**, in authored order, via the
addon's in-process `applyIrSqlite` verb. Turn the SQLite `registerModel` into a
read/no-op like the Postgres arm. The generated `schema.runtime.json` descriptor
becomes **typing-only** for the runtime; it is never rendered into DDL.

This removes the register-order / partial-union / cross-app-FK / double-injection
failure modes by construction, and it retires a large amount of runtime schema
machinery.

---

## Problem: a migration-first app with an FK does not run

`examples/db-todos` is a `@zeroship/db` example with two collections (`users`,
`todos`) and a foreign key `todos.userId → users.id`. Running it (`pnpm dev` +
`pnpm smoke`) fails at the first DB insert. `tests/e2e_app_primitives.sh` already
records this as a known limitation ("db-todos schema-init fails on worker ⇒ env.db
unreachable").

The failure is a chain (each real, in order of discovery):

1. **The example ships no migrations.** It relied on the removed inline
   `export default { schema }` discovery; the migration-first runtime installs
   collections from the fold's descriptor, so with no `migrations/` dir there is no
   descriptor and `env.db.users` is `undefined`.
2. **Double-injection collision.** With migrations present, the gen-types emit
   ceiling already folds the seven platform system columns into
   `schema.runtime.json`, and the worker's `registerModel` re-injects them via
   `crates/plugin-db/policies/confined.policy.toml`:
   `invalid descriptor: collection 'todos' declares field 'created_at', which
   collides with an injected policy column`.
3. **Vendored-engine API/policy skews** (the pinned `zero-migrate` moved):
   - `genArtifacts` now **requires** a `dialect` field the vite-plugin does not send
     (`Missing field 'dialect'`).
   - the confined charter's `runtime.lock_timeout_ms` /
     `runtime.statement_timeout_ms` grants are now *declared-only* knobs that reject
     a non-default value (`DeclaredOnlyNonDefault`).
4. **Register-order / partial-union cross-app FK.** With the collision worked
   around, `todos` gets planned before `users` exists:
   `plan_declarative failed: table 'todos' declares a foreign key to 'users', which
   no app in the project declares (a cross-app FK target must exist in the union
   schema)`. The register runs per-collection against a **partial** union (the
   current collection + only the *cached* siblings), so the FK target is absent
   unless it happens to register first.

Items 2 and 4 are two faces of the same anomaly: **the runtime is doing the
migration's job.**

---

## Root cause

| Path | Who applies the schema | `registerModel` |
| --- | --- | --- |
| **Postgres deploy** | the `migrated` service, at deploy, ahead of the worker (imperative replay of migration ops under the confined charter) | **no-op** (`crates/plugin-db/src/register_model/mod.rs`: `(Some(_pg), _) => Ok(())`) |
| **SQLite dev (today)** | the worker, lazily, per-collection, via `registerModel → run_sqlite_via_engine` (declarative diff of the descriptor vs live) | **drives the apply** |

Because the SQLite path derives the schema from the **descriptor** (which already
carries the injected system columns) and applies **per collection in registration
order**, it hits the double-inject and partial-union checks the imperative, ordered,
whole-migration Postgres apply never sees.

The migrations are the single source of truth; nothing needs a runtime handshake to
learn the collections:

```
migrations/*.ts ──(gen-types fold)──▶ schema.runtime.json  ─▶ worker: TYPES only
       │                                                        (env.db types, idPrefix, mask facets)
       └──(imperative apply, confined charter, authored order)──▶ tables + FK + PK + journal
```

`zero-migrate` learns the collections by reading the migration ops itself, so the FK
target (`users`) is created before the referrer (`todos`) — no order problem, no
partial union.

---

## Design

### 1. Apply ahead of the worker (dev)

The vite-plugin dev-server already (a) has the `migrations/` dir, (b) records the
migration IR envelopes in gen-types, and (c) owns `.zeroship/`. Have it apply the
migrations to the dev SQLite **app file** at boot, before spawning the worker, using
the addon's in-process SQLite apply verb (already exported by `zeroship-migrate-node`):

```ts
applyIrSqlite(appPath: string, journalPath: string, req: ApplyIrSqliteRequest): Promise<ApplyReply>

interface ApplyIrSqliteRequest {
  ownerApp: string;                // dev app id — "default"
  projectSchema: string;           // "public" (SQLite renders unqualified `main`; inert)
  registry: Record<string,string>; // table -> owner_app  (all -> "default" in dev)
  charterLayers: string[];         // [CONFINED_APPLY_CHARTER_TOML]  (see below)
  approved: boolean;               // true (operator owns the local file)
  envelopes: JsonValue[];          // recordMigrationsDir(migrationsDir)
}
```

- **Paths:** `<root>/.zeroship/zs-default.sqlite` (app file) and
  `…/zs-default.migrations.sqlite` (journal) — the exact files the worker's SQLite
  backend derives from `db_dir` + `zs-<app_id>.sqlite` (dev app_id `default`;
  `run_sqlite_via_engine` builds the same paths from `backend_a.db_dir()`).
- **Charter** (`CONFINED_APPLY_CHARTER_TOML`): grants + the confined `[[inject]]`.
  Mirrors `crates/migrated/policies/confined.policy.toml` **minus** the two rejected
  timeout grants. The `[[inject]]` MUST carry the `created_at`/`updated_at` = `now()`
  and `version` = 1 defaults or the first insert fails NOT NULL (the data plane never
  sends them):

  ```toml
  policy_version = 1

  [[grant]]
  key = "schema.create_table"
  value = true
  scope = "all"

  [[grant]]
  key = "schema.rename"
  value = true
  scope = "all"

  [[grant]]
  key = "safety.destructive_ops"
  value = "allow"
  scope = "all"

  [[inject]]
  scope = "all"
  mandatory = true
  primary_key = ["id"]
  author_primary_key = "forbid"
  columns = [
    { name = "id",         type = "text",        nullable = false },
    { name = "created_at", type = "timestamptz", nullable = false, default = "now()" },
    { name = "updated_at", type = "timestamptz", nullable = false, default = "now()" },
    { name = "created_by", type = "text",        nullable = true  },
    { name = "updated_by", type = "text",        nullable = true  },
    { name = "version",    type = "integer",     nullable = false, default = "1" },
    { name = "deleted_at", type = "timestamptz", nullable = true  },
  ]
  indexes = [
    { name = "ix_deleted_at", columns = ["deleted_at"] },
    { name = "ix_updated_at", columns = ["updated_at"] },
    { name = "ix_created_by", columns = ["created_by"] },
  ]
  ```

- **Wiring** (`sdks/vite-plugin/src/dev-server.ts`): chain the apply after
  `bootRegenDone = regenTypesDev(...)` and before `spawnRuntime` awaits it, and on
  the migration hot-update path. Log-not-throw (a bad migration must not crash the
  dev server). Idempotent — the `_mig` journal skips already-applied migrations.
- Expose `applyIrSqlite` on the `MigrateAddon` interface in
  `sdks/vite-plugin/src/gen-types/addon.ts` (types from `zeroship-migrate-node`), and add
  an `applyMigrationsToDevSqlite()` helper that records envelopes, builds the
  `registry` from the descriptor's collection names (all → `default`), and calls the
  verb.

### 2. SQLite `registerModel` → read/no-op

Mirror the Postgres arm in `crates/plugin-db/src/register_model/`: on SQLite, **skip
`run_sqlite_via_engine`** (no plan, no apply). Keep the metadata-readiness contract:

- `mark_model_registered` (readiness gate),
- `cache_schema` (declared-only hints: `idPrefix`, mask/encryption facets),
- ensure the app file is ATTACHed for reads (`backend_a.ensure_app_schema`).

The worker then never renders the descriptor into DDL; it only reads the tables the
ahead-of-time apply created.

### 3. The descriptor is typing-only

`schema.runtime.json` is consumed by `installSchema` purely for `env.db` typing and
declared-only hints — no longer a DDL source at runtime on either backend.

---

## Implementation checklist

**Prerequisite fixes (needed regardless — vendored-engine compat + the missing
example migration):**

- [ ] Add `examples/db-todos/migrations/20260101000000_create_todos.ts`. System
      columns are injected by the charter — do **not** declare `id`:

  ```ts
  import { table, t } from "@zeroship/migrate";

  export default {
    name: "create_todos",
    up() {
      table("users").create({
        columns: {
          email:  t.text().notNull().unique(),
          name:   t.text().notNull(),
          handle: t.text().notNull().unique(),
        },
      });
      table("todos").create({
        columns: {
          userId:   t.text().notNull().references("users", "id"), // native FK
          title:    t.text().notNull(),
          priority: t.text().notNull().default("medium"),
          tags:     t.json(),
          done:     t.boolean().notNull().default(false),
          archived: t.boolean().notNull().default(false),
        },
        indexes: [{ name: "todos_user_idx", on: ["userId"] }],
      });
    },
  };
  ```

  Commit the regenerated `examples/db-todos/generated/` too (mirrors
  `examples/db-hitcounter`). Note `t.ref()` is not in the vendored DSL — use
  `t.text().references(...)`.
- [ ] gen-types `genArtifacts` calls pass `dialect: "postgres"`
      (`sdks/vite-plugin/src/gen-types/index.ts`, both the MANUAL and GENERATED
      calls). The runtime descriptor is otherwise dialect-neutral.
- [ ] Any confined charter used at **apply** drops the declared-only
      `runtime.lock_timeout_ms` / `runtime.statement_timeout_ms` grants.

**The architecture change:**

- [ ] `MigrateAddon` (addon.ts) exposes `applyIrSqlite`.
- [ ] `CONFINED_APPLY_CHARTER_TOML` + `applyMigrationsToDevSqlite()`.
- [ ] dev-server wires the apply after regen, before spawn, and on hot-update.
- [ ] SQLite `registerModel` → read/no-op; keep readiness + cache + ATTACH.

**Verify:**

- [ ] Reset `.zeroship`, `pnpm dev`, confirm `zs-default.sqlite` has `users` +
      `todos` (system cols, FK, id PK) created by the dev-apply; `pnpm smoke` green
      (`bash examples/db-todos/scripts/smoke.sh`); env.db reads/writes/live-queries
      work.

---

## Alternatives considered (and why rejected)

**A. Make the descriptor self-describe its primary key and keep the runtime
declarative apply.** Add `primary_key` to the runtime descriptor, have the render
honor it, project `now()` defaults through a sentinel, and make the register charter
PK-only so the descriptor's own columns are used. This *works* but is the wrong
layer: it doubles down on the runtime creating the schema, needs ongoing
partial-union / register-order patches (passing the declared set as
`known_fk_targets`, topological register ordering), and — because Postgres already
applies ahead — the descriptor-render machinery is dead code on PG. Under this
proposal it is dead on SQLite too.

**B. Batch the whole descriptor union into one runtime apply** (instead of
per-collection). Fixes the partial union but keeps the runtime as the schema
authority — the same category error as A.

---

## Risks / open questions

1. **File / app-id coupling.** The dev-server must apply to the exact file + app_id
   the worker reads (`.zeroship/zs-default.sqlite`, `default`), with the same journal
   path. Confirm the worker's dev `db_dir` + app_id derivation, or centralise the
   path convention so both sides share it.
2. **`projectSchema` / `registry` values** must be internally consistent with the
   worker's reads. SQLite renders `main` (schema-inert), but the values still flow
   through lowering/executor confinement.
3. **Charter ⇄ descriptor byte-identity.** The imperative apply's table shape must
   equal what gen-types describes (same inject shape → should hold; verify
   `now()`→`CURRENT_TIMESTAMP`, and index/PK names, match on SQLite). This is the
   engine's existing "emit == apply-side, byte-identical" contract.
4. **Register no-op + reads.** The worker must still ATTACH and read without the
   register-drives-engine path; preserve the readiness/introspection contract
   (`register_model/mod.rs`).
5. **Retirement.** With SQLite register a no-op, `run_sqlite_via_engine`,
   `build_union_descriptors`, `other_schemas`, the partial-union reconciliation, and
   the runtime copy of `crates/plugin-db/policies/confined.policy.toml` become dead
   and can be removed in a follow-up.

---

## Key references

- Addon verb: `zeroship-migrate-node` `applyIrSqlite` / `ApplyIrSqliteRequest` /
  `ApplyReply` (`third_party/zero-migrate/crates/zeroship-migrate-node/index.d.ts`).
- Confined apply charter to mirror: `crates/migrated/policies/confined.policy.toml`.
- Postgres register no-op to mirror: `crates/plugin-db/src/register_model/mod.rs`.
- The SQLite register-drives-engine path to retire:
  `crates/plugin-db/src/register_model/sqlite_engine.rs`.
- The cross-app FK union check: `validate_cross_app_fk_targets` in
  `third_party/zero-migrate/crates/zero-migrate/src/render/declarative.rs`.
- Dev DB paths: `sdks/vite-plugin/src/dev-db.ts` (`.zeroship/`), worker app files
  `zs-<app_id>.sqlite`.
