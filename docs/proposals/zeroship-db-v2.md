# @zeroship/db v2 — proposal

**Status:** Draft 2026-05-12, rev. R14 (uncommitted; lives in `docs/proposals/` until landed; loop converged at composite score 89)
**Scope:** evolve `@zeroship/db` + `crates/plugin-db` to close V1 schema-safety gaps and adopt the design patterns we verified against Convex's docs.
**Backend scope:** Postgres-only for V2 (matches today's `compio-postgres` driver). SQLite/MySQL backends would require re-deriving the diff classifier, replacing the WAL fanout with a backend-specific change-feed primitive, and revising the CHECK-constraint usage. Not in V2. Sections below call out Postgres-specific mechanisms.
**Postgres version pinning:** V2 requires Postgres 16+ as the baseline. Specific feature dependencies:
- 11+: fast-path `ALTER TABLE ADD COLUMN DEFAULT` with immutable default (A2)
- 12+: `REINDEX INDEX CONCURRENTLY` (A1 recovery), generated columns
- 13+: `max_slot_wal_keep_size` (C1 slot watchdog)
- 14+: predefined `pg_read_all_stats` role (maintenance cron)
- 15+: publication-row filters (C1 — optional optimisation; falls back to per-table publication for 14)
- 16+: `synchronized_standby_slots` for failover (HA section)
The CI test matrix pins **16 (LTS), 17 (current stable)**. Postgres 18 testing is opportunistic until stable release.
**Reference review:** the gap analysis at the top of this doc came out of reading the current source — `sdks/db/src/{db,types,schema,validate,collection,query,model}.ts` and `crates/plugin-db/src/{query,callbacks}.rs` — and verifying Convex claims against [docs.convex.dev](https://docs.convex.dev/) via context7.

<!-- Round 1 revision: addressed factual errors in Convex citations, added INVALID-index recovery, reversed ON DELETE default, downgraded reactive-query mechanism to WAL-based, addressed typed_id invariant, expanded operational surface. -->

---

## Motivation

`@zeroship/db` ships today with a Mongoose-style declarative schema, end-to-end TypeScript inference, `{ data, error }` returns, transactions, and 11 native primitives. The shape is right for an AI-builder platform: declarative, type-safe, opinionated.

But a code-level review surfaced **three classes of silent gaps** that ship to production today:

1. **Schema markers that don't materialize.** `t.string().index()` sets `FieldDef.index = true` in the SDK but `crates/plugin-db/src/query.rs` has no `CREATE INDEX` codepath. Every AI-generated app that thinks it has an indexed email column actually doesn't. `.unique()` may set a column-level constraint but never a unique index. Silent correctness bug across every shipped app.
2. **No deploy-time data validation.** `registerModel` does additive DDL (`CREATE TABLE IF NOT EXISTS`, `ALTER TABLE ADD COLUMN IF NOT EXISTS`) but never checks existing rows against the new schema. Type changes, new required fields, and new unique constraints can leave invalid data behind that surfaces as runtime errors much later.
3. **No migration log (ISS-24).** Schema changes happen invisibly. DataCanvas's Migrations tab is a stub.

Beyond V1 safety, the comparison to Convex (the closest peer for an AI-friendly TypeScript database SDK) reveals **structural design choices** we should adopt:

- Capability-scoped function kinds (query/mutation/action) so AI-generated code can't accidentally call `fetch()` inside a transaction
- Typed cross-table IDs so `db.posts.findOne({ authorId: postId })` is a compile error
- Reactive queries so live UI doesn't need polling
- A separate **migrations component** for data backfills (vs. coupling DDL and data transformation)

This proposal lays out a four-tier roadmap. **Tier A is V1-blocking.** Tier B is launch quality. Tier C is transformative but post-V1. Tier D is polish.

---

## Design overview

Three architectural separations frame the rest of this doc:

1. **DDL vs data transformation.** Convex's biggest design win is splitting these. DDL (column add/drop, constraint change) happens automatically at deploy time, with a validation guard against existing data. Data transformations (backfill old documents to a new shape) are explicit, batched, resumable migration functions the user writes. We adopt the same split.
2. **Schema definitions are the source of truth.** No separate migration files. The `createDb({...})` declaration in user source IS the schema. The platform diffs declared vs live on deploy. Matches Convex / Prisma / Drizzle. Departs from Rails / Sequelize numbered migration files which don't fit AI codegen.
3. **Capability-scoped server functions.** Today's RPC markers (`procedure` / `query` / `mutation` / `stream`) become real capability boundaries. A `query` is read-only and pure. A `mutation` is transactional and can read+write. An `action` can call external APIs. Enforced by types AND by the runtime.

---

## Tier A — V1 schema safety (must ship before GA)

### A1. Materialized indexes

**Motivation.** Today's silent no-op: `t.string().email().unique()` parses correctly, types correctly, never creates a unique index in Postgres. The SDK's own type inference advertises uniqueness; the runtime doesn't enforce it. This is a correctness regression that ships to every app, AI-generated or hand-written.

**Design.**

User-facing API (compatible with existing markers):

```ts
const db = createDb({
  users: {
    email:    t.string().required().unique(),    // → CREATE UNIQUE INDEX
    handle:   t.string().required().index(),     // → CREATE INDEX
    fullName: t.string(),
  },
}).index("by_handle_email", ["handle", "email"]); // composite (new API)
```

Field-level `.unique()` and `.index()` produce single-column indexes named by convention: `<table>_<column>_idx` (non-unique) or `<table>_<column>_key` (unique). Composite indexes get an explicit name via a new `defineCollection().index(name, columns[])` builder.

**Implementation.**

- `crates/plugin-db/src/query.rs` — new `build_create_indexes(app_id, collection, schema)` returning a `Vec<String>` of `CREATE INDEX CONCURRENTLY IF NOT EXISTS ...` statements
- `crates/plugin-db/src/callbacks.rs::exec_register_model` runs these after the `CREATE TABLE` / `ALTER TABLE ADD COLUMN` cascade
- `CONCURRENTLY` is critical — never block writes on index creation; tradeoff is can't run inside a transaction, so this is a separate phase after DDL
- Index naming via a deterministic helper so re-runs are idempotent
- Identifier length: Postgres truncates names beyond 63 bytes. Naming helper produces `<table>_<col>_idx`; if the result exceeds 60 bytes, the suffix is replaced by an 8-char base32 hash of the full name (Atlas's strategy in `migrate/sqltool`). Hash is deterministic so re-runs hit the same name.
- SDK additions: `defineCollection(fields).index(name, columns[])` builder for composite/named indexes; the existing `t.X().index()` keeps working. Inner array is named `columns` to disambiguate from outer field map.

<!-- Round 1 revision: added INVALID-index recovery section, identifier length handling, columns naming. -->

**INVALID-index recovery.** `CREATE INDEX CONCURRENTLY` is a two-phase build (catalog entry, then physical build). On any failure — deadlock, disk pressure, unique violation, worker restart — Postgres leaves the index marked `indisvalid = false` in `pg_index`. Invalid indexes are written by every INSERT/UPDATE but never used by the planner: pure overhead ([PostgresAI: hidden cost of invalid indexes](https://postgres.ai/blog/20260106-invalid-index-overhead), [pganalyze: invalid indexes check](https://pganalyze.com/docs/checks/schema/index_invalid)). Required mitigation:

1. After each `CREATE INDEX CONCURRENTLY`, the diff engine runs:
   ```sql
   SELECT indisvalid FROM pg_index
   WHERE indexrelid = '<schema>.<idxname>'::regclass;
   ```
2. If `false`, **short-circuit on deterministic data errors**. Inspect the SQLSTATE captured during the failed CREATE:
   - `23505 unique_violation` — data has duplicates; retrying will fail again. Log the violation, query `COUNT(*)` of conflicting rows, exit with `validation_refused` envelope (see A2). No DROP/retry loop.
   - `23514 check_violation`, `23503 foreign_key_violation`, `23502 not_null_violation` — same; do not retry, surface as a structured refusal.
   - Other errors (deadlock `40P01`, disk pressure `53100`, worker crash) are transient — `DROP INDEX CONCURRENTLY <idxname>` then re-issue. Postgres 12+ exposes `REINDEX INDEX CONCURRENTLY` for in-place replacement ([Bytebase](https://www.bytebase.com/blog/postgres-create-index-concurrently/)).
3. Transient retries are capped (default 3). Beyond cap, the change is escalated to operator approval (B1 surface).
4. Per-deploy `__zeroship_migrations` row records each retry with `change_kind='index_retry'` and a parent `migration_id` linking back to the original attempt — operators can audit the recovery trail.

**Risks.**

- `CREATE INDEX CONCURRENTLY` can't run inside an existing transaction — the DDL cascade splits into two phases (table + columns, then indexes). Acceptable: registration is already idempotent.
- Renaming an indexed column triggers `DROP INDEX` + `CREATE INDEX` — surfaces in A2 as a destructive change requiring operator approval.
- Unique-index creation on a table that already contains duplicates fails with a `unique_violation` mid-build. The recovery loop above catches this, marks the migration `failed` with the violating row count, and returns a structured error to the deploy pipeline (rather than retrying forever).

### A2. Deploy-time data validation

**Motivation.** Convex's most important safety invariant: when a schema is first added or modified, Convex validates that all existing documents match it, and the push fails if validation errors occur ([docs.convex.dev/database/schemas](https://docs.convex.dev/database/schemas)). Convex offers `schemaValidation: false` in `defineSchema` as an escape hatch for rapid prototyping and external data ingestion ([Convex DefineSchemaOptions](https://docs.convex.dev/api/interfaces/server.DefineSchemaOptions)). We have no equivalent today.

**Design.**

`registerModel` becomes a four-phase operation:

1. **Diff phase.** Compare the desired schema against the live schema (via `pg_catalog` introspection).
2. **Classify changes** into three buckets:
   - **Additive** (auto-apply): add column nullable, add index, relax constraint (NOT NULL → nullable)
   - **Compatible** (auto-apply with backfill): add column with **constant** default, add unique constraint after validation passes, type widening (e.g. `int4` → `int8`)
   - **Destructive** (refuse and report): drop column, type narrowing, tighten constraint, drop index, change FK target, **add column with volatile default** (`DEFAULT NOW()`, `DEFAULT gen_random_uuid()`), **add NOT NULL column to a non-empty table**

   Reference: PlanetScale's deploy-request taxonomy maps almost 1:1, with the same three-bucket split.

   **Volatile-default trap.** Postgres 11+ implements a fast path for `ALTER TABLE ADD COLUMN c TYPE NOT NULL DEFAULT 'literal'` — purely metadata, no table rewrite. But a volatile default (`DEFAULT NOW()`, `DEFAULT gen_random_uuid()`, or any non-`IMMUTABLE` function) forces a full table rewrite under ACCESS EXCLUSIVE lock. The diff classifier inspects the default expression via `pg_get_expr` + `pg_proc.provolatile`: `v` (volatile) and `s` (stable) defaults are escalated to destructive; only `i` (immutable) defaults take the fast path. Volatile cases route through the migrations component (B1) as an explicit `expand-migrate-contract` flow — the user first adds the column nullable, then backfills via `migrations.run(...)`, then tightens.

3. **Validate phase.** For any compatible/destructive change involving an existence check, run a validation query. Configurable strictness — adopts the Convex `schemaValidation` knob, per-collection rather than schema-wide:

   ```ts
   const db = createDb({
     users: defineCollection({ email: t.string().required() })
       .strictness("strict"),  // default — refuse push on any violation
     events: defineCollection({ payload: t.json() })
       .strictness("lenient"), // warn on violations, allow push
     legacy_imports: defineCollection({...})
       .strictness("off"),     // skip validation entirely (Convex's schemaValidation:false)
   });
   ```

   For `strict` (default), validation must be exhaustive on tables under a budget; sampling is not sound.

**Shorthand vs builder form.** Both `users: { email: t.string().required() }` (shorthand) and `users: defineCollection({ email: t.string().required() })` (builder) **default to strict**. The shorthand has no `.strictness()` method; users wanting non-strict must switch to the builder form. The SDK rejects an unknown collection shape at module-init time with `invalid_collection_form`.

**Error-code naming convention.** All error codes in this proposal use `lower_snake_case` to match `sdks/db/src/errors.ts` (`not_found`, `validation_failure`, etc.). The R2 draft mixed `VALIDATION_REFUSED` (SCREAMING_SNAKE) — corrected throughout this revision. Final inventory:

- `validation_refused` — A2 deploy refusal
- `capability_violation` — B3 wrapper violation
- `optimistic_lock_failure` — D4 CAS failure
- `unique_violation`, `check_violation`, `foreign_key_violation`, `not_null_violation` — passthrough from Postgres SQLSTATE classes 23xxx
- `ref_target_not_found` — `t.ref()` to non-existent collection
- `invalid_collection_form` — shorthand+builder mixup
- `migration_failure_budget_exceeded` — B1 per-row failure-budget reached (status becomes `failed`). Distinct from the *successful* status `applied_with_dead_letter` (some rows skipped but migration completed).
- `migration_cancelled` — B1 kill switch
- `migration_not_cancellable` — `cancel(name)` called on a migration in `applied`/`applied_with_dead_letter`/`failed`/`cancelled`/`rolled_back` state
- `subject_map_corrupt` — B2 GDPR safety net
- `slot_invalidated` — C1 broker slot loss

4. **Validation budget.** Atlas's approach: validation queries are paginated by primary key with a wall-clock budget (default 60s, override via `--validate-timeout`). Pseudocode:
   ```
   cursor = 0
   while elapsed < budget:
     batch = SELECT id, <fields> FROM tbl WHERE id > cursor ORDER BY id LIMIT 10_000
     fails += validate(batch)
     cursor = batch.last.id
     if fails > 0 and strictness == strict: ABORT with structured error
   if cursor < max(id): defer to migrations component, return pending=true
   ```
   The migrations component (B1) then resumes from `cursor` in the background and unblocks the deploy when complete. We never silently skip rows; we either complete, or commit to a deferred path with a tracking row in `__zeroship_migrations`.

5. **Apply phase.** Run additive + compatible DDL. Log every operation to `__zeroship_migrations` (see A3). Surface destructive changes via a structured error returned to the deploy pipeline; the control plane handles the user approval workflow (see B1).

**Error envelope.** When validation refuses a deploy, the worker returns a structured envelope (consumed by the SDK's module-init throw and the deploy pipeline):

```jsonc
{
  "code": "validation_refused",
  "deploy_id": "dep_01H…",
  "violations": [
    {
      "collection": "users",
      "field": "email",
      "constraint": "unique",
      "rows_violating": 47,
      "sample_failing_pks": [12, 87, 102, 233, 401],   // up to 5
      "remediation": "Backfill duplicates via @zeroship/migrations or relax to `defineCollection().strictness('lenient')`"
    }
  ],
  "destructive_pending": [
    { "collection": "posts", "change_kind": "drop_column", "field": "legacy_score",
      "approval_url": "https://console.zeroship.ai/apps/<id>/migrations/<mig_id>" }
  ]
}
```

CLI prints the failing PK samples (`zeroship deploy` exits non-zero with the human-readable form). Convex's CLI is the model.

**Implementation.**

- New module `crates/plugin-db/src/diff.rs` — schema introspection (`pg_class` + `pg_attribute` + `pg_constraint` + `pg_index` + `pg_proc` for default-expr volatility) and diff classification
- DDL ordering: the emitter topologically sorts `CREATE TABLE` statements by FK dependency (Kahn's algorithm); FK additions on existing tables are ordered after their target tables; cyclic FKs use `DEFERRABLE INITIALLY DEFERRED` to permit any insert order (B2).
- `callbacks.rs::exec_register_model` becomes the orchestrator
- Returns a structured response: `{ applied: [...], pending_destructive: [...], pending_validation: [...], errors: [...] }`
- SDK side: `createDb` awaits the response; if `errors.length > 0` AND strictness=`strict`, throws at module-init time so the app fails fast in dev. If `pending_destructive`, surfaces a deploy-pipeline gate.
- Idempotency: every `registerModel` call carries a `deploy_id` (issued by the control plane). A duplicate call with the same `deploy_id` short-circuits to the cached result. Prevents two concurrent worker cold-starts racing the diff engine — the second waits on the first via the same two-key advisory lock used in the *Concurrent-deploy semantics* section below: `pg_advisory_xact_lock(hashtext('zs_reg:<app_id>')::int4, hashtext(<deploy_id>)::int4)`.
- Runtime validation of `t.ref` targets: at `createDb` finalisation (SDK-side, before any DDL is sent to the worker), `t.ref("non_existent")` is detected by inspecting the schema map and throws `RefTargetNotFoundError` with the offending field path. The TS `Tables<S>` constraint catches this at compile time; the runtime check is the safety net for `t.ref("x" as any)` escapes.

**Concurrent-deploy semantics.** Only one `registerModel` execution per app at a time (advisory lock `pg_advisory_xact_lock(hashtext('zs_reg:<app_id>')::int4, hashtext('register_model')::int4)`); subsequent calls block until the first completes, then re-introspect and confirm no further diff. Uses the **two-key form** for the same collision-space reason as B1. Transaction-scoped is correct here because `registerModel` completes in one transaction. `schema_version` for the deploy is computed inside this critical section (`SELECT MAX(schema_version) FROM __zeroship_migrations WHERE phase='ddl' AND status='applied'`) so two racing deploys serialise on the lock and produce distinct versions. Matches Atlas's `apply --lock-timeout` design.

**Risks.**

- Validation on very large tables can exceed the budget. Mitigation: validation pagination + deferred completion via the migrations component. If a validation defers, the deploy proceeds in **pending** state — new writes are validated at insert time, but old rows remain unverified until the background sweep finishes. Operators see the deploy as "partially-validated" in DataCanvas.
- Schema-introspection latency adds to cold-start. Mitigation: cache the live-schema snapshot keyed by `(app_id, deploy_id)`. Cache invalidates on `__zeroship_migrations` write.
- Privilege model: introspection runs as the app's per-schema role with `USAGE` on `pg_catalog`. We do **not** grant access to other apps' `pg_namespace` rows; the diff queries filter by `nspname = '<app_id>'`.
- Schema-name length: Postgres `NAMEDATALEN` is 64 bytes. typed_id `app_<base62-uuidv7>` is 26 + 4 = 30 bytes — fits. If a future entity prefix overruns, the schema name is hashed (8-char prefix `app_` + 8-char base32 of typed_id hash + truncated suffix). Identifier-quoting is `"<schema>"."<table>"` everywhere; mixed-case identifiers are preserved verbatim.

### A3. Migration audit log (closes ISS-24)

**Motivation.** ISSUES.md ISS-24 calls for it. DataCanvas's Migrations tab is a stub. Operators have no audit trail of what changed when.

**Design.**

Per-app `__zeroship_migrations` table (auto-managed by the platform, lives in the app's schema). The `__zeroship_` prefix avoids collisions with Postgres-internal `_*` names and with downstream tools that filter `_*` (Atlas uses `atlas_`, Flyway uses `flyway_`):

```sql
CREATE TABLE __zeroship_migrations (
  id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  collection          TEXT NOT NULL,
  phase               TEXT NOT NULL,              -- 'ddl' | 'validation' | 'backfill' | 'audit'
  change_class        TEXT NOT NULL,              -- 'additive' | 'compatible' | 'destructive'
  change_kind         TEXT NOT NULL,              -- 'add_column' | 'drop_column' | 'add_index' | 'index_retry' | 'migration_batch' | ...
  details             JSONB NOT NULL,             -- column metadata, constraints; secrets-redacted (see PII note below)
  ddl_sql             TEXT,                       -- the actual SQL run (NULL for non-DDL phases)
  created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- immutable; when the row was inserted
  updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),  -- mutable; updated on every status transition
  applied_at          TIMESTAMPTZ,                -- when the DDL/backfill actually executed (NULL while pending/running)
  applied_by_kind     TEXT NOT NULL,              -- 'auto' | 'user' | 'ai-builder' | 'operator'
  applied_by_id       TEXT,                       -- typed_id (usr_…, op_…) when applied_by_kind != 'auto'
  deploy_id           TEXT NOT NULL,              -- links to deploys table (ISS-15); 'cold_start' for pre-deploy DDL
  parent_id           BIGINT REFERENCES __zeroship_migrations(id),  -- for retries / rollbacks / batch-of
  schema_version      INTEGER NOT NULL,           -- monotonic per app; rollback uses this (see open Q #6)
  status              TEXT NOT NULL,              -- see status state machine below
  error               TEXT,                       -- failure reason if status='failed'
  duration_ms         INTEGER,
  validate_cursor     BIGINT,                     -- last PK validated/backfilled for deferred rows
  owner_session_id    TEXT,                       -- worker session pid for in-flight migrations
  last_heartbeat_at   TIMESTAMPTZ,                -- updated every 5s by the running worker
  dead_letter_pks     JSONB,                      -- PKs that failed all retries (B1)
  CONSTRAINT __zeroship_migrations_phase_chk CHECK (
    phase IN ('ddl','validation','backfill','audit')
  ),
  CONSTRAINT __zeroship_migrations_class_chk CHECK (
    change_class IN ('additive','compatible','destructive')
  ),
  CONSTRAINT __zeroship_migrations_status_chk CHECK (
    -- State machine: pending → running → (applied | applied_with_dead_letter | failed | cancelled | rolled_back)
    status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back')
  )
);
CREATE INDEX __zeroship_migrations_created_at_idx ON __zeroship_migrations (created_at DESC);
CREATE INDEX __zeroship_migrations_status_pending_idx
  ON __zeroship_migrations (status, updated_at) WHERE status IN ('pending','running');
```

Lives in the app's schema namespace (security-isolated; survives control-plane disaster recovery). Single monotonic key (the IDENTITY primary key); the duplicate `ordinal` from the draft is removed. `applied_by` split into `kind` + `id` so typed_id IDs are queryable.

<!-- Round 2 revision: added phase column, heartbeat columns for B1 session-locking, expanded status state machine including cancelled and applied_with_dead_letter, added schema_version, tamper-evident write pattern below. -->

**Status state machine.**

```
              ┌─────────┐
       create │ pending │ ── worker picks up ──▶ running
              └─────────┘                          │
                                                   ├─ success ─▶ applied
                                                   ├─ dead_letter > 0 ─▶ applied_with_dead_letter
                                                   ├─ failure_budget exceeded ─▶ failed
                                                   ├─ kill switch ─▶ cancelled
                                                   └─ admin revert ─▶ rolled_back
```

The CHECK constraint on `status` enumerates every state above. Workers transition under the session advisory lock (B1) to prevent split-brain.

**`pending` vs immediate-`running`.** Two row sources, two patterns:

- **A1/A2 synchronous DDL** runs inside the deploy's diff orchestrator, which holds the advisory lock for the whole operation. Rows for synchronous DDL go directly to `running` on INSERT (no queueing for a future worker). On success, transition `running → applied`; on failure, `running → failed`. The `pending` state is skipped.
- **B1 asynchronous migrations** are queued — `defineMigration` results are logged with `status='pending'`, awaiting a worker to acquire ownership. Worker acquires the session advisory lock, transitions `pending → running`, drives the batch loop, then transitions to the terminal state.

The state machine accepts both paths; the CHECK constraint allows `pending → running` and `running → terminal`. Both initial states (`pending` and `running`) are also allowed as INSERT defaults (no `pending` requirement before `running`).

**Tamper-evident writes.** App code runs as a per-app role (`app_<id>_role`) with `USAGE` on the schema and CRUD on user tables — **but NOT direct DML on `__zeroship_migrations`**. Writes go through a `SECURITY DEFINER` function owned by a privileged `__zeroship_admin` role.

**Provenance is platform-controlled, not user-supplied.** The function does NOT accept `applied_by_kind`/`applied_by_id` as parameters — a malicious app role could otherwise forge audit entries (`applied_by_kind='operator'` with a fake UUID).

**Why custom GUCs are insufficient.** A naïve design would use custom namespaced GUCs (`SET LOCAL zeroship.actor_kind`). But custom GUCs in Postgres are settable by any session by default (verified against [Postgres ALTER ROLE docs](https://www.postgresql.org/docs/current/sql-alterrole.html) — `ordinary roles can only set defaults for themselves` and there is no permission gate on per-session `SET` of unrestricted custom GUCs). An attacker inside an app worker could `SET zeroship.actor_kind = 'operator'` and forge audit entries before calling `__zeroship_log_migration`. Trust model fails.

**Actual mechanism: PID-keyed privileged context table.** Temp tables created inside a SECURITY DEFINER end up owned by the invoking *session* role (verified against Postgres docs and the [Cybertec abusing-SECURITY-DEFINER analysis](https://www.cybertec-postgresql.com/en/abusing-security-definer-functions/)) — REVOKE against PUBLIC has no effect on the owner. Custom GUCs are also user-settable. The only mechanism that actually constrains the app role is a regular table owned by `__zeroship_admin`, keyed by `pg_backend_pid()`, with access mediated by SECURITY DEFINER functions.

**Schema placement.** The session-context table and HMAC-key table live in a **platform-wide** schema `__zeroship_admin` (one per Postgres cluster, not per app). The schema is owned by the `__zeroship_admin` role; app roles have `USAGE` (to call functions) but no privileges on the tables. This matches the multi-tenant pattern used by Supabase's `auth` schema:

```sql
-- Owned by __zeroship_admin; app role has NO direct privilege.
CREATE TABLE __zeroship_session_ctx (
  pid          INTEGER PRIMARY KEY,                   -- pg_backend_pid()
  actor_kind   TEXT NOT NULL,
  actor_id     TEXT,
  session_nonce BYTEA NOT NULL,                       -- random per init; rotated each acquire
  signature    BYTEA NOT NULL,                        -- HMAC over (actor_kind || actor_id || pid || nonce)
  initialised_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX __zeroship_session_ctx_age_idx ON __zeroship_session_ctx (initialised_at);
REVOKE ALL ON __zeroship_session_ctx FROM PUBLIC;
-- The app role has no privilege on this table; only SECURITY DEFINER functions touch it.

CREATE FUNCTION __zeroship_init_session(
  p_actor_kind TEXT, p_actor_id TEXT,
  p_signature BYTEA, p_nonce BYTEA,
  p_expires_at TIMESTAMPTZ
) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp  -- prevent search-path injection (Cybertec hardening)
AS $$
BEGIN
  -- Expiry check: signed payload includes expires_at; rejects replay beyond window.
  IF p_expires_at < NOW() THEN
    RAISE EXCEPTION 'session-init signature expired';
  END IF;
  -- Anti-replay: expires_at is the canonical bound (signed payload covers it).
  -- Nonce uniqueness adds a defense for the worst case where an attacker captures
  -- a still-valid signature; window must outlive both the signature expiry AND the
  -- key rotation grace (so a replay can't outlive both):
  IF EXISTS (SELECT 1 FROM __zeroship_session_ctx
             WHERE session_nonce = p_nonce
               AND initialised_at > NOW() - INTERVAL '25 hours') THEN
    RAISE EXCEPTION 'session-init nonce replay detected';
  END IF;
  -- Verify HMAC; algorithm pinned to sha256, key resolved via __zeroship_session_keys.
  IF NOT __zeroship_verify_signature(p_actor_kind, p_actor_id, pg_backend_pid(),
                                     p_nonce, p_expires_at, p_signature) THEN
    RAISE EXCEPTION 'invalid session-init signature';
  END IF;
  IF p_actor_kind NOT IN ('auto','user','operator','ai-builder','platform') THEN
    RAISE EXCEPTION 'invalid actor_kind: %', p_actor_kind;
  END IF;

  INSERT INTO __zeroship_session_ctx (pid, actor_kind, actor_id, session_nonce, signature)
  VALUES (pg_backend_pid(), p_actor_kind, p_actor_id, p_nonce, p_signature)
  ON CONFLICT (pid) DO UPDATE
    SET actor_kind = EXCLUDED.actor_kind,
        actor_id   = EXCLUDED.actor_id,
        session_nonce = EXCLUDED.session_nonce,
        signature  = EXCLUDED.signature,
        initialised_at = NOW();
END $$;
REVOKE ALL ON FUNCTION __zeroship_init_session FROM PUBLIC;
GRANT EXECUTE ON FUNCTION __zeroship_init_session TO app_<id>_role;

CREATE FUNCTION __zeroship_verify_signature(
  p_actor_kind TEXT, p_actor_id TEXT, p_pid INTEGER,
  p_nonce BYTEA, p_expires_at TIMESTAMPTZ, p_signature BYTEA
) RETURNS BOOLEAN
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, extensions, pg_temp
AS $$
DECLARE
  v_payload BYTEA;
  v_key BYTEA;
BEGIN
  v_payload := convert_to(
    p_actor_kind || '|' || COALESCE(p_actor_id, '') || '|' || p_pid::TEXT
      || '|' || encode(p_nonce, 'hex') || '|' || to_char(p_expires_at, 'YYYY-MM-DD"T"HH24:MI:SS.MS'),
    'UTF8'
  );
  -- Try each active HMAC key (rotation window: any key with retired_at IS NULL or retired_at > NOW() - INTERVAL '24h')
  FOR v_key IN
    SELECT hmac_key FROM __zeroship_session_keys
    WHERE retired_at IS NULL OR retired_at > NOW() - INTERVAL '24 hours'
    ORDER BY created_at DESC
  LOOP
    -- Constant-time compare to prevent timing side-channel:
    IF __zeroship_const_eq(p_signature, hmac(v_payload, v_key, 'sha256')) THEN
      RETURN TRUE;
    END IF;
  END LOOP;
  RETURN FALSE;
END $$;
REVOKE ALL ON FUNCTION __zeroship_verify_signature FROM PUBLIC;
-- Only called from other SECURITY DEFINER functions; no GRANT to app_<id>_role.
```

`__zeroship_log_migration` (and the other audit-writing functions) reads `__zeroship_session_ctx WHERE pid = pg_backend_pid()` inside their SECURITY DEFINER body. The app role cannot observe or modify the context table directly — it goes through the function gate.

**Signature anchor.** The platform's control plane holds an HMAC key (rotated via the existing secret-store rotation); the worker requests a signed `(actor_kind, actor_id, pid, nonce)` tuple at connection acquisition. The function verifies via `__zeroship_verify_signature` (also SECURITY DEFINER, reading from `__zeroship_session_keys` populated only by `__zeroship_admin` — declared below):

```sql
-- The HMAC key table, owned by __zeroship_admin, no PUBLIC access:
CREATE TABLE __zeroship_session_keys (
  key_id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  hmac_key   BYTEA NOT NULL,                        -- 32 random bytes
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  retired_at TIMESTAMPTZ                            -- when set, key enters 24h grace window
);
REVOKE ALL ON __zeroship_session_keys FROM PUBLIC;
```

**HMAC key lifecycle.**

1. **Initial provisioning.** When the platform's DB cluster is created, the control plane bootstraps `__zeroship_admin`, runs the CREATE TABLE above, and inserts an initial key generated from `gen_random_bytes(32)` (pgcrypto). The key never leaves the database (the control plane signs by calling a SECURITY DEFINER signing function over a privileged connection — never reading the raw key).
2. **Active window.** Verification accepts any key with `retired_at IS NULL OR retired_at > NOW() - INTERVAL '24h'` so rotation has a grace window.
3. **Rotation cadence.** Weekly automated rotation, triggered by the control plane:
   - INSERT new key.
   - UPDATE old key SET `retired_at = NOW()`.
   - 24h later, DELETE retired keys (`retired_at < NOW() - INTERVAL '24 hours'`).
4. **Emergency rotation.** Operator can force rotation in < 1 minute via `POST /api/admin/session-keys/rotate` (control plane). Effective immediately; sessions initialised with the old key continue working until `retired_at` expires.

**pgcrypto schema convention.** `pgcrypto` exposes `hmac(data, key, algo)` and `gen_random_bytes(n)`. The platform pins the extension to schema `extensions` (control-plane bootstrap: `CREATE EXTENSION pgcrypto WITH SCHEMA extensions`); all SECURITY DEFINER functions include `extensions` in their `search_path` clause: `SET search_path = pg_catalog, extensions, pg_temp`. Apps can't shadow extension functions because their schemas aren't in the path.

**Constant-time HMAC comparison.** Naïve BYTEA equality (`a = b`) returns at the first differing byte — a remote timing side-channel can extract the HMAC byte-by-byte ([standard timing-attack analysis](https://codahale.com/a-lesson-in-timing-attacks/)). The platform installs a constant-time comparator:

```sql
CREATE FUNCTION __zeroship_const_eq(a BYTEA, b BYTEA) RETURNS BOOLEAN
LANGUAGE plpgsql IMMUTABLE PARALLEL SAFE
SET search_path = pg_catalog
AS $$
DECLARE
  v_diff INTEGER := 0;
  v_len  INTEGER := GREATEST(octet_length(a), octet_length(b));
BEGIN
  v_diff := v_diff | (octet_length(a) # octet_length(b));   -- length difference
  FOR i IN 1..v_len LOOP
    v_diff := v_diff | (COALESCE(get_byte(a, i-1), 0) # COALESCE(get_byte(b, i-1), 0));
  END LOOP;
  RETURN v_diff = 0;
END $$;
```

Loop runs `max(len(a), len(b))` iterations regardless of where bytes differ; OR-accumulator keeps execution time independent of content. The verify function uses `__zeroship_const_eq(p_signature, computed_hmac)` instead of `=`.

**Cost note.** A plpgsql byte-loop is interpreter-bound — measurably slower than a C-level comparator. For a 32-byte HMAC the cost is bounded (~32 interpreter iterations + the surrounding function-call overhead). Connection-acquisition is the hot path (one verify per checkout, not per request), so the absolute latency is acceptable. If benchmark `crates/runtime/benches/db_connect_init.rs` shows this dominates, a follow-up adds a C extension `zeroship_crypto.const_eq` to the platform's pgcrypto-style extension footprint.

**Sign function (called by the control plane only, never by app code):**
```sql
CREATE FUNCTION __zeroship_sign_session(
  p_actor_kind TEXT, p_actor_id TEXT, p_pid INTEGER,
  p_nonce BYTEA, p_expires_at TIMESTAMPTZ
) RETURNS BYTEA
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, extensions, pg_temp
AS $$
DECLARE v_key BYTEA;
BEGIN
  SELECT hmac_key INTO v_key FROM __zeroship_session_keys
    WHERE retired_at IS NULL ORDER BY created_at DESC LIMIT 1;
  IF v_key IS NULL THEN
    RAISE EXCEPTION 'no active session-init key (rotation lag?)';
  END IF;
  RETURN hmac(
    convert_to(p_actor_kind || '|' || COALESCE(p_actor_id, '') || '|' || p_pid::TEXT
               || '|' || encode(p_nonce, 'hex') || '|' || to_char(p_expires_at, 'YYYY-MM-DD"T"HH24:MI:SS.MS'),
               'UTF8'),
    v_key, 'sha256'
  );
END $$;
REVOKE ALL ON FUNCTION __zeroship_sign_session FROM PUBLIC;
-- Role-based gating, NOT GUC-based: only the platform role can execute.
GRANT EXECUTE ON FUNCTION __zeroship_sign_session TO __zeroship_platform_role;
-- app_<id>_role is explicitly NOT granted — the function is the platform's trust anchor.
```

The function uses **role-based access control** (the GRANT is to `__zeroship_platform_role` only) rather than checking a GUC. Custom GUCs are user-settable in Postgres and cannot enforce access boundaries; role grants are the only enforceable mechanism.

**Dead-letter audit SECURITY DEFINER:**
```sql
CREATE FUNCTION __zeroship_log_dead_letter(
  p_migration_id BIGINT, p_pk BIGINT, p_error_class TEXT, p_error_message TEXT
) RETURNS VOID
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp AS $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM __zeroship_session_ctx WHERE pid = pg_backend_pid()) THEN
    RAISE EXCEPTION 'audit context not set';
  END IF;
  INSERT INTO __zeroship_migration_dead_letter_overflow (migration_id, pk, error_class, error_message)
  VALUES (p_migration_id, p_pk, p_error_class, p_error_message);
END $$;
REVOKE ALL ON FUNCTION __zeroship_log_dead_letter FROM PUBLIC;
GRANT EXECUTE ON FUNCTION __zeroship_log_dead_letter TO app_<id>_role;
```

`__zeroship_verify_signature` is also SECURITY DEFINER and accepts `(actor_kind, actor_id, pid, nonce, signature)`. The signing key is queried inside the function; the app role cannot read the key.

Compromise of the app role alone is insufficient to forge a session context — the attacker would also need the HMAC key (which lives outside the app's reach in `__zeroship_session_keys`).

**Latency.** `__zeroship_init_session` adds one round-trip at connection acquire. For the worker, this is a fixed per-checkout cost — measured cost will be added to the benchmark in `crates/runtime/benches/db_connect_init.rs`. If the latency dominates, the worker can amortise via long-lived connections (per the AGENTS.md invariant that V8 is per-thread, the worker's PG connections are already long-lived; init runs once per connection lifetime, not per request).

**PgBouncer pool-checkout safety.** PgBouncer in `transaction` mode reuses Postgres connections across clients. The PID-keyed model handles this correctly: every checkout calls `__zeroship_init_session(...)` which uses `ON CONFLICT DO UPDATE` on `pid = pg_backend_pid()`. The prior tenant's row is replaced atomically with the new tenant's. Reads inside `__zeroship_log_migration` always see the most recent init for that PID — there is no leak across checkouts.

**Stale-context cleanup.** When a Postgres session ends (backend exits), the row for that PID becomes stale (no foreign-key cleanup). A weekly sweeper (added to the *Maintenance cron* section) deletes rows where `pid` is no longer present in `pg_stat_activity`. Without the sweeper, the table accumulates rows over weeks but stays bounded by the connection count × churn rate.

The function derives `applied_by_*` from the session context table and accepts `p_schema_version` (computed once per deploy by the orchestrator, not per-row):

```sql
CREATE FUNCTION __zeroship_log_migration(
  p_collection TEXT, p_phase TEXT, p_change_class TEXT, p_change_kind TEXT,
  p_details JSONB, p_ddl_sql TEXT, p_status TEXT, p_deploy_id TEXT,
  p_schema_version INTEGER
) RETURNS BIGINT
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp  -- standard SECURITY DEFINER hardening (Cybertec)
AS $$
DECLARE
  v_id BIGINT;
  v_actor_kind TEXT;
  v_actor_id   TEXT;
BEGIN
  SELECT actor_kind, actor_id INTO v_actor_kind, v_actor_id
    FROM __zeroship_session_ctx WHERE pid = pg_backend_pid();
  IF v_actor_kind IS NULL THEN
    RAISE EXCEPTION 'audit context not set: __zeroship_init_session was not invoked';
  END IF;
  IF v_actor_kind NOT IN ('auto','user','operator','ai-builder') THEN
    RAISE EXCEPTION 'invalid actor_kind: %', v_actor_kind;
  END IF;

  INSERT INTO __zeroship_migrations (collection, phase, change_class, change_kind, details,
    ddl_sql, status, deploy_id, applied_by_kind, applied_by_id, schema_version)
  VALUES (p_collection, p_phase, p_change_class, p_change_kind, p_details,
    p_ddl_sql, p_status, p_deploy_id, v_actor_kind, v_actor_id, p_schema_version)
  RETURNING id INTO v_id;
  RETURN v_id;
END $$;
REVOKE ALL ON __zeroship_migrations FROM PUBLIC;
REVOKE ALL ON FUNCTION __zeroship_log_migration FROM PUBLIC;
GRANT EXECUTE ON FUNCTION __zeroship_log_migration TO app_<id>_role;
```

The orchestrator computes `p_schema_version` once per deploy as `SELECT COALESCE(MAX(schema_version), 0) + 1 FROM __zeroship_migrations WHERE phase='ddl' AND status='applied'` and passes the same value to every log call within that deploy. Within a deploy, all rows share a schema_version; across deploys, the value is monotonic.

The app role cannot `DELETE` or `UPDATE` the migration log. Status transitions are routed through a second `SECURITY DEFINER` function that enforces the state machine in SQL:

```sql
CREATE FUNCTION __zeroship_transition_migration(
  p_id BIGINT, p_from_status TEXT, p_to_status TEXT,
  p_error TEXT DEFAULT NULL, p_dead_letter_pks JSONB DEFAULT NULL
) RETURNS BOOLEAN
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE v_current TEXT;
BEGIN
  -- Allowed transitions (must match the state-machine diagram above)
  IF NOT (
    (p_from_status = 'pending' AND p_to_status = 'running')
    OR (p_from_status = 'running' AND p_to_status IN ('applied','applied_with_dead_letter','failed','cancelled'))
    OR (p_from_status IN ('applied','applied_with_dead_letter') AND p_to_status = 'rolled_back')
    OR (p_from_status = 'failed' AND p_to_status = 'pending')  -- operator retry
  ) THEN
    RAISE EXCEPTION 'illegal status transition: % -> %', p_from_status, p_to_status;
  END IF;

  UPDATE __zeroship_migrations
    SET status = p_to_status,
        error = COALESCE(p_error, error),
        dead_letter_pks = COALESCE(p_dead_letter_pks, dead_letter_pks),
        updated_at = NOW(),
        applied_at = CASE
          WHEN p_to_status IN ('applied','applied_with_dead_letter') AND applied_at IS NULL THEN NOW()
          ELSE applied_at
        END
    WHERE id = p_id AND status = p_from_status;

  RETURN FOUND;
END $$;
REVOKE ALL ON FUNCTION __zeroship_transition_migration FROM PUBLIC;
GRANT EXECUTE ON FUNCTION __zeroship_transition_migration TO app_<id>_role;
```

Pattern: PostgreSQL row-level security + privileged functions, same as Supabase's auth schema and PostgREST recipes.

**Server-log redaction.** The unredacted DDL is still sent over the wire to Postgres; if the operator has `log_statement = 'ddl'` set, the unredacted statement lands in `postgresql.log`. Mitigations, in priority order:

1. **Parameterise defaults where possible.** The diff engine uses `DEFAULT $1::text` for literal default values, so the literal never appears in the statement text and `log_statement` doesn't capture it. Postgres restricts parameterisation in DDL — column defaults via parameters work in `ALTER TABLE ADD COLUMN ... DEFAULT $1` but other contexts may not.
2. **Privilege-gated `log_statement` override.** Where parameterisation isn't available, the migration session runs `SET LOCAL log_statement = 'none'` before executing the unredacted DDL. **Privilege requirement:** the platform's DB role must have permission to alter `log_statement` per-session. On vanilla Postgres this is open; on managed services (RDS, Aurora, Cloud SQL) it requires the `rds_superuser`-equivalent. The control plane provisions the platform role with this privilege at cluster setup time; documented in `docs/runbooks/db-cluster-provision.md`.
3. **Fallback on managed services without the privilege.** If `SET LOCAL log_statement` is denied, the operator is required to set the cluster-wide `log_statement = 'mod'` (not `'ddl'` or `'all'`). `mod` logs INSERT/UPDATE/DELETE but not DDL, so the unredacted DDL doesn't reach the log either. This requirement is part of the platform's cluster provisioning checklist; deployment fails fast if the setting isn't compliant.

Either path keeps the unredacted DDL out of `postgresql.log`. The `__zeroship_migrations` audit row stores only the redacted version, executed via `__zeroship_log_migration`.

**PII / secrets.** `details.default_value` and `ddl_sql` can contain user-chosen defaults that reference env vars or literal secrets. The diff engine applies **two-layer redaction** before logging:

1. **Known-prefix denylist** — adapted from [GitGuardian's detector taxonomy](https://docs.gitguardian.com/secrets-detection/detectors) and the [truffleHog](https://github.com/trufflesecurity/trufflehog) detector set (over 700 patterns). Strict prefixes: `sk_live_`, `sk_test_`, `rk_live_`, `pk_live_` (Stripe), `AKIA`, `ASIA` (AWS), `ghp_`, `gho_`, `ghs_`, `github_pat_` (GitHub), `xoxb-`, `xoxp-` (Slack), `glpat-` (GitLab), `Bearer eyJ` (JWT), plus a configurable per-deployment list.
2. **Entropy fallback** — Shannon entropy ≥ 4.0 bits/char on strings of length ≥ 20. Catches base64-encoded API keys not on the prefix list.

Redacted values are replaced with `<redacted:prefix=<first-4-chars>:len=<original-length>>` so the audit log still tells you a value was set without leaking it. The unredacted DDL is executed against Postgres directly from the worker memory and never written to disk.

**Retention.** `__zeroship_migrations` is append-only and would grow unbounded over years. Policy:
- Last 90 days: kept verbatim.
- 90 days to 1 year: monthly rollup row per `(collection, change_kind)`.
- Beyond 1 year: deleted by a control-plane sweeper (cron, opt-out per app).

Modelled on Atlas's `atlas_schema_revisions` partitioning and Cloudflare D1's migrations-table TTL.

**Observability.** The diff engine emits metrics per phase:
- `zeroship_db_diff_duration_seconds{app, phase}` — phase = `introspect | classify | validate | apply`
- `zeroship_db_diff_validation_rows_total{app, collection, result}` — result = `pass | fail`
- `zeroship_db_diff_destructive_total{app, change_kind}` — every refused destructive change increments
- `zeroship_db_diff_concurrent_index_invalid_total{app}` — alerts on chronic CONCURRENTLY failures

**Implementation.**

- `exec_register_model` writes one row per DDL operation it runs (both successful and failed)
- Control plane exposes `GET /api/apps/:id/migrations` reading from the per-app `__zeroship_migrations` table (no separate control-plane table needed — the per-app table is the source of truth)
- `agents.ts` proxies the call; DataCanvas Migrations tab swaps from `SAMPLE_MIGRATIONS` to the real list
- Admin actions surfaced via `POST /api/apps/:id/migrations/:migration_id/{approve,reject,cancel}` for destructive-change workflow; each action writes a child row (`parent_id` set, `applied_by_kind='operator'`)

---

## Tier B — V1 launch quality

### B1. `@zeroship/migrations` component

**Motivation.** A2 surfaces destructive changes but refuses to apply them. The expand-migrate-contract pattern Convex teaches needs a real data-backfill mechanism so users can land breaking changes safely. Today the only "migration" is DDL on cold start; there's no platform-native way to write "for each existing row, transform field X".

**Design.**

Mirror Convex's `@convex-dev/migrations` ([docs](https://github.com/get-convex/migrations)):

```ts
import { defineMigration } from "@zeroship/migrations";

export const backfillRole = defineMigration({
  name: "backfillRole",            // required; used as the lock key + audit-log key
  collection: "users",
  batchSize: 100,
  migrateOne: async (doc, ctx) => {
    if (doc.role === undefined) {
      return { role: "user" };  // returned fields are applied via $set
    }
  },
});

// Trigger from CLI or post-deploy hook:
await migrations.run(backfillRole);

// Dry-run one batch:
await migrations.run(backfillRole, { dryRun: true });

// Resume after a crash:
await migrations.run(backfillRole, { resume: true });
```

**Semantics:**
- Migrations are stateful — state is a row in `__zeroship_migrations` (`status` follows the state machine in A3, `validate_cursor` = last PK processed, `dead_letter_pks` = JSONB array, `owner_session_id` + `last_heartbeat_at` for ownership)
- **Ownership via session advisory lock + heartbeat.** `pg_advisory_xact_lock` would be wrong: it releases at the end of each batch transaction, freeing the lock between batches. We use the **two-key form** `pg_advisory_lock(hashtext('zs_mig:<app_id>')::int4, hashtext(<migration_name>)::int4)` (session-scoped — held across multiple batch transactions until the worker explicitly releases or its session ends; cited from [Postgres advisory-lock semantics](https://www.postgresql.org/docs/current/explicit-locking.html)). The two-key form avoids cross-app hash collisions in the single-int8 keyspace at platform scale. The owner worker writes `owner_session_id = pg_backend_pid()` and heartbeats `last_heartbeat_at = NOW()` every 5s. Other workers checking the row see the live heartbeat and back off. On worker crash, the heartbeat staleness exceeds 30s and the maintenance cron (see *Maintenance cron* section) terminates the stale backend via `pg_terminate_backend(stale_pid)` — session termination releases all session-level advisory locks as a side effect — then clears `owner_session_id` so the next worker can claim. Pattern modelled on Sidekiq Enterprise's job-lease and [pg-cron](https://github.com/citusdata/pg_cron)'s coordinator.
- **PgBouncer caveat.** Session-level advisory locks require a non-pooled connection (or transaction mode at the boundary). The worker reserves a dedicated connection for the migration owner via `compio-postgres`'s connection-pinning surface; the same connection is used for every batch transaction in the run. Documented limitation: migrations cannot run through a PgBouncer in `transaction` mode without dedicated bypass.
- Batched — process N documents per transaction; safe to crash and resume
- Online — the app stays live; reads see whichever shape exists (schema must be compatible with both pre- and post-migration shapes during the run)
- Dry-run — runs one batch, rolls back, logs what would have changed
- Reset — `migrations.run(fn, { reset: true })` discards state and starts over
- Cancel — `migrations.cancel(name)` writes `status='cancelled'` via the `SECURITY DEFINER` transition function. The owner worker checks `status` under the advisory lock at the start of each batch; if `cancelled`, releases the lock and exits cleanly. Cancel happens-before the next batch — in-flight batches commit normally to preserve the cursor. Cancel semantics by current state:
  - `pending` → `cancelled` (writes status, never picked up by a worker)
  - `running` → `cancelled` (owner worker exits at next batch boundary; in-flight batch commits)
  - `applied` / `applied_with_dead_letter` / `failed` / `cancelled` / `rolled_back` → `migration_not_cancellable` error

**`migrateOne` capability surface.** The callback runs with a `MutationCtx` (same as a `mutation` wrapper) — DB read+write, no `fetch`. Side effects to external systems are not permitted; migrations are restartable and dry-run-able, both of which require deterministic replay. If you need to call an external API as part of migration, write a separate `action` that pages the data and a `mutation` (called from the action) that writes — and run the action separately, not as a migration.

**`defineMigration` registration model.** `defineMigration({...})` returns a `MigrationDescriptor` object — a plain JS value, no side effects at import time. To register the migration with the platform, it must be exported from `src/migrations/*.ts`; the vite-plugin's synthetic entry walks this directory at build time and registers every exported `MigrationDescriptor` with the worker. Side-effect-free imports are an explicit platform invariant — Tier B AI codegen prompts emit `defineMigration` exports, never auto-running constructors.

**Failure modes and dead-letter pattern.** A row that always fails `migrateOne` would loop forever without bounds. Per Convex's component design and Sidekiq's job-failure-budget convention:

1. Batch transaction wraps N rows. If any row throws, the entire batch aborts and rolls back.
2. Failing batch retries with `batchSize = batchSize / 2` (halving). Bottom-out at 1.
3. If a single-row batch fails, the row's PK is appended to `__zeroship_migrations.dead_letter_pks` (JSONB array column) and skipped. Cursor advances.
4. After the migration completes, `dead_letter_pks` exposes the unprocessed PKs to the user for manual remediation; the migration's `status` is `applied` only if the array is empty, else `applied_with_dead_letter`.
5. Per-row failure count is also capped (default 100); beyond the cap, the migration `status` becomes `failed` with a structured error and operator approval is required to continue.
6. **Dead-letter cap.** The `dead_letter_pks` JSONB array is capped at 1000 PKs in the row to keep the audit row small. Beyond 1000, the column stores `{ "first": [...1000 PKs...], "truncated_at": <count>, "stream_url": "/api/apps/<id>/migrations/<mig_id>/dead-letter.ndjson" }`. The streaming endpoint reads from the `__zeroship_migration_dead_letter_overflow` side table populated by the worker when the cap is reached:
   ```sql
   CREATE TABLE __zeroship_migration_dead_letter_overflow (
     id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
     migration_id  BIGINT NOT NULL REFERENCES __zeroship_migrations(id) ON DELETE CASCADE,
     pk            BIGINT NOT NULL,
     error_class   TEXT NOT NULL,         -- exception class from migrateOne
     error_message TEXT NOT NULL,
     recorded_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
   );
   CREATE INDEX __zeroship_migration_dead_letter_overflow_mig_idx
     ON __zeroship_migration_dead_letter_overflow (migration_id, recorded_at);
   ```
   Retention follows the parent migration's retention (A3). Writes go through `__zeroship_log_dead_letter(migration_id, pk, error_class, error_message)` (SECURITY DEFINER, same provenance pattern as `__zeroship_log_migration`).

**Implementation.**

- New SDK package: `@zeroship/migrations`
- Native: a single new primitive `zeroship.db.runMigrationBatch(migrationId, cursor, batchSize, dryRun)` returning `{ processed, nextCursor, isDone, deadLetter }`
- SDK orchestrates: loop calling the batch primitive, updating `__zeroship_migrations` state, retrying with halving batchSize on failure
- Trigger surfaces: CLI (`zeroship migrate run <name>`), post-deploy hook in `defineApp`, programmatic via SDK
- Kill switch: `zeroship migrate cancel <name>` writes `status='cancelled'`; the next batch call notices and exits the loop. WebSocket-side workers polling cursor also see the status change.

**Quota & cost.** Migrations consume database CPU. Each batch increments the app's `db_migration_cpu_ms` counter via `zeroship.meter` (see `docs/reference/billing-metering.md`); large backfills can be rate-limited via per-app quota set in the control plane.

**Metrics.** The B1 runtime emits:
- `zeroship_db_migration_rows_processed_total{app, migration, result}` — result = `applied | failed | skipped`
- `zeroship_db_migration_batch_duration_seconds{app, migration}` — histogram
- `zeroship_db_migration_dead_letter_total{app, migration}` — gauge of cumulative dead-letter count
- `zeroship_db_migration_owner_handoffs_total{app, migration}` — incremented when the heartbeat sweeper hands off a stale lock

App-cardinality labels are aggregated at the dashboard layer where the app count is large.

**The expand-migrate-contract pattern.**

AI codegen / docs teach the three-deploy pattern for breaking changes:

1. **Expand** — schema accepts both old and new shape (e.g. `role: t.string().enum("user", "admin")` with optional)
2. **Migrate** — `migrations.run(backfillRole)` fills in `role` for old rows
3. **Contract** — schema tightens (`role` becomes required); deploy refuses if any row still has the old shape; once green, push

### B2. Typed cross-table relations

**Motivation.** Today's `id: number` means `db.posts.findOne({ authorId: postId })` is a runtime bug — types accept any number. Convex's `v.id("users")` brand makes this a compile error ([docs.convex.dev/database/document-ids](https://docs.convex.dev/database/document-ids): "IDs are strings at runtime, but the Id type can be used to distinguish IDs from other strings at compile time").

**ID system alignment.** zeroship's platform-level entities use typed_id (UUIDv7 + base62 + entity prefix — `usr_01H…`, `app_01H…`). Application data inside `@zeroship/db` collections currently uses `BIGINT IDENTITY`. Three options were considered:

| Option | Storage | Brand carrier | Cross-system join |
|---|---|---|---|
| (a) typed_id everywhere | TEXT | string | natural — same shape across platform & app data |
| (b) keep BIGINT | int8 | number | manual mapping at boundaries |
| (c) hybrid: opt-in `.id("typed_id")` | TEXT or int8 | string-or-number | hardest TS inference |

**Decision: (b) for V2.** Switching app-data IDs to typed_id requires a destructive schema migration across every shipped app and changes index sizes (text > int8 for FK columns). The migration cost outweighs the consistency win. We document the boundary explicitly: platform entities (apps, users, sessions) use typed_id; per-app collection rows use BIGINT IDENTITY. The `Id<T>` brand documented below applies only to per-app collection IDs.

Future work (C-tier addendum): if user demand surfaces, expose `defineCollection().id("typed_id", "msg")` to opt collections into typed_id storage. Diff engine treats the change as destructive.

**Design.**

New field builder `t.ref(collection)`:

```ts
const db = createDb({
  users:    { name: t.string().required() },
  posts:    { authorId: t.ref("users"), title: t.string().required() },
  comments: { postId: t.ref("posts"), authorId: t.ref("users") },
});

// At the type level:
// db.users.create returns Document<{...}> with id: Id<"users">
// db.posts.create({ authorId: x }) requires x to be Id<"users">

const u = await db.users.create({ name: "alice" });
const p = await db.posts.create({ authorId: u.data!.id, title: "hi" });  // ✓
const bad = await db.posts.create({ authorId: 42, title: "hi" });         // ✗ type error

// Cross-table typo:
db.posts.findOne({ authorId: p.data!.id });  // ✗ p.data.id is Id<"posts">, not Id<"users">
```

**Implementation.**

- Brand type: `type Id<T extends string> = number & { readonly __table: T }`. The brand parameter is a string literal type; at definition time, `t.ref("users")` returns `TypeBuilder<Id<"users">>` (literal type preserved by `T extends string`). The check that `"users"` is a *declared* table happens at `createDb({...})` finalisation via a phantom-type check:
  ```ts
  type Refs<S> = { [K in keyof S]: { [F in keyof S[K]]: S[K][F] extends TypeBuilder<Id<infer T>> ? T : never }[keyof S[K]] }[keyof S];
  type AssertAllRefsValid<S> = Refs<S> extends keyof S ? S : { __error: "t.ref targets must be declared collections" };

  function createDb<S extends Record<string, unknown>>(schema: S & AssertAllRefsValid<S>): Db<S> { ... }
  ```
  The conditional type forces all `t.ref(T)` calls to point at a `keyof S` — anything else turns the inferred schema into an error type, killing the call site. Mechanism borrowed from Convex's `DefineSchemaOptions` parameter constraints. Cross-schema escapes like `t.ref("control.apps")` produce a compile error because `"control.apps"` is not a key of the user's schema literal.
- TypeBuilder: `t.ref<T extends string>(table: T, opts?: { onDelete?: "restrict" | "cascade" | "set_null" | "no_action", deferrable?: boolean }): TypeBuilder<Id<T>>`
- DDL: `t.ref("users")` generates `FOREIGN KEY (author_id) REFERENCES "<app_schema>"."users"(id) DEFERRABLE INITIALLY DEFERRED`. The schema name is always quoted as the app's own schema; cross-schema refs cannot be emitted because the builder has no surface to express them.
- Runtime: refs stored as `BIGINT`; brand is type-side only and is erased at JSON serialisation (Postgres returns plain numbers; the SDK re-brands at the boundary using the declared schema).
- **`ON DELETE` default: `RESTRICT`** (was CASCADE in the draft — reversed per industry convention: Postgres, MySQL, SQLite, Prisma, Drizzle, SQLAlchemy all default to RESTRICT/NO ACTION). Silent cascading deletes cause catastrophic data loss; opt-in is the safer default. Override via `t.ref("users", { onDelete: "cascade" })`. Convex's own model is explicit cascade via triggers ([stack.convex.dev/triggers](https://stack.convex.dev/triggers)) — they have no implicit cascade.

**Deferred-constraint cost.** All `t.ref` FKs are emitted `DEFERRABLE INITIALLY DEFERRED` so circular references can be inserted in any order within a transaction. Cost: Postgres queues the constraint check until `COMMIT`, adding a per-row entry to the deferred-trigger queue. We have **not yet measured** this overhead against zeroship's workload; the cost will be characterised by a micro-benchmark in `crates/runtime/benches/db_deferred_fk.rs` before P4 ships, and the result will be recorded in this section. If the measured overhead exceeds the team's threshold (open question #8), the default flips to `NOT DEFERRABLE` and topological insert ordering shifts to the SDK. Users can opt-out per ref with `t.ref("users", { deferrable: false })` for hot insert paths today.

**Multi-hop relation queries.** `t.ref` solves the *typing* of single-column foreign keys but does not address join ergonomics. The Drizzle ecosystem's `defineRelations` API ([orm.drizzle.team/docs/relations-v2](https://orm.drizzle.team/docs/relations-v2)) extends FK declarations with named relations and offers `db.query.posts.findMany({ with: { author: true, comments: { with: { author: true }}}})` — type-safe nested loading. We **defer** the equivalent to a separate proposal:

- V2 ships `t.ref` for typed FKs + simple `db.posts.find({authorId})` queries
- Future proposal addresses `db.posts.find({}, { with: { author: true } })` semantics, including the SQL strategy (separate query per relation vs. a single JOIN — Drizzle defaults to the former since v0.36)
- Reason for deferral: the join-typing surface is large enough to warrant its own design pass against Drizzle, Prisma's `include`, and Convex's recommended `ctx.db.get(parentId)` composition pattern

**Soft delete.** `t.softDelete()` collection modifier — adds a `deleted_at TIMESTAMPTZ NULL` column, all queries default to `WHERE deleted_at IS NULL`, `db.users.delete(id)` becomes `UPDATE … SET deleted_at = NOW()`. FK cascade interaction: `ON DELETE CASCADE` *physical* deletes, not soft deletes. Soft-deleted parent rows are not auto-cascaded — the user must `db.posts.deleteWhere({ authorId, includeSoftDeleted: true })` or compose explicitly. Mirrors Prisma's soft-delete-by-middleware pattern.

**GDPR / right-to-erasure.** Platform-level users (typed_id `usr_…`) and per-app user rows live in different tables; the typed_id maps to an app-collection PK via a registration record kept in the per-app `__zeroship_subject_map` table (`subject_typed_id TEXT, collection TEXT, row_id BIGINT`). On an erasure request:

1. Control plane calls `zeroship.db.eraseSubject(appId, subjectTypedId)`.
2. The primitive looks up `__zeroship_subject_map` → resolves to one or more `(collection, row_id)` pairs.
3. The platform walks the FK graph (read from `pg_constraint`) from the resolved rows outward, computing the deletion order.
4. Each delete is logged to `__zeroship_migrations` with `phase='audit'`, `change_kind='subject_erasure'`.
5. Refused if any non-cascade RESTRICT ref blocks — the operator sees a structured refusal with the blocking collection so they can pre-soft-delete or detach.
6. Billing interaction: erasure does NOT back-bill metered storage; the audit row persists past the erasure (retention policy in A3 applies). Same model as Stripe's `customers.delete` keeping the audit trail.

Subject-map registration is opt-in via `defineCollection().subject(t.ref("users"))` so apps that don't process subject data are unaffected.

**Subject-map schema.** Per-app table, lives in the app's schema (security-isolated like `__zeroship_migrations`):

```sql
CREATE TABLE __zeroship_subject_map (
  subject_typed_id TEXT NOT NULL,                    -- platform-level typed_id (usr_…)
  collection       TEXT NOT NULL,                    -- which app collection the subject is in
  row_id           BIGINT NOT NULL,                  -- the row's BIGINT PK in that collection
  registered_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (subject_typed_id, collection)         -- one row per (subject, collection)
);
CREATE INDEX __zeroship_subject_map_subject_idx ON __zeroship_subject_map (subject_typed_id);
CREATE INDEX __zeroship_subject_map_row_idx ON __zeroship_subject_map (collection, row_id);
REVOKE ALL ON __zeroship_subject_map FROM PUBLIC;
```

**Subject-map security.** `__zeroship_subject_map` bridges platform identity to per-app data — a leaked map is a cross-app correlation oracle. Access controls:

- The app role has **no** direct DML on `__zeroship_subject_map`. Writes go through `__zeroship_register_subject(subject_typed_id, collection, row_id)` (SECURITY DEFINER). Reads go through `__zeroship_resolve_subject(subject_typed_id)` (SECURITY DEFINER), invoked only by the platform's GDPR primitive — never by app code.

  ```sql
  -- Per-app functions: live in the app's schema; search_path includes the schema
  -- so the (unqualified) __zeroship_subject_map resolves correctly. The function
  -- itself is owned by __zeroship_admin so SECURITY DEFINER has the right grants.
  -- NOTE: the "<app_schema>" placeholder is substituted with the actual app_id when
  -- the control plane templates this DDL during per-app provisioning. The substituted
  -- string is quoted and parameterised via psql variable expansion, not concatenation.
  CREATE FUNCTION __zeroship_register_subject(
    p_subject_typed_id TEXT, p_collection TEXT, p_row_id BIGINT
  ) RETURNS VOID
  LANGUAGE plpgsql SECURITY DEFINER
  SET search_path = pg_catalog, "<app_schema>", pg_temp
  AS $$
  BEGIN
    -- Trust anchor: PID-keyed session context must be initialised.
    IF NOT EXISTS (SELECT 1 FROM __zeroship_admin.__zeroship_session_ctx WHERE pid = pg_backend_pid()) THEN
      RAISE EXCEPTION 'audit context not set';
    END IF;
    INSERT INTO __zeroship_subject_map (subject_typed_id, collection, row_id)
    VALUES (p_subject_typed_id, p_collection, p_row_id)
    ON CONFLICT (subject_typed_id, collection) DO UPDATE SET row_id = EXCLUDED.row_id;
  END $$;

  CREATE FUNCTION __zeroship_resolve_subject(p_subject_typed_id TEXT)
  RETURNS TABLE(collection TEXT, row_id BIGINT)
  LANGUAGE plpgsql SECURITY DEFINER
  SET search_path = pg_catalog, "<app_schema>", pg_temp
  AS $$
  DECLARE v_actor TEXT;
  BEGIN
    -- Role-based check via session_ctx (not GUC — see A3 trust model note)
    SELECT actor_kind INTO v_actor FROM __zeroship_admin.__zeroship_session_ctx
      WHERE pid = pg_backend_pid();
    IF v_actor NOT IN ('operator','platform') THEN
      RAISE EXCEPTION 'resolve_subject restricted to platform actors (got %)', v_actor;
    END IF;
    RETURN QUERY SELECT m.collection, m.row_id
                 FROM __zeroship_subject_map m
                 WHERE m.subject_typed_id = p_subject_typed_id;
  END $$;

  REVOKE ALL ON __zeroship_subject_map FROM PUBLIC;
  GRANT EXECUTE ON FUNCTION __zeroship_register_subject TO app_<id>_role;
  -- resolve grant is to a dedicated platform role only:
  REVOKE ALL ON FUNCTION __zeroship_resolve_subject FROM PUBLIC;
  GRANT EXECUTE ON FUNCTION __zeroship_resolve_subject TO __zeroship_platform_role;
  ```
- The platform-managed tables (`__zeroship_subject_map`, `__zeroship_migrations`) are explicitly excluded from the C1 publication (see C1 step 1), so subscribers cannot observe subject mappings via the change feed.
- Encryption at rest: rely on the underlying Postgres storage encryption (Aurora/RDS-managed key or the platform's own KMS-wrapped TDE). Per-column encryption is deferred — the table is small and the bridging risk is mitigated by access control.
- Operational: writes to `__zeroship_subject_map` are metered via `zeroship.meter.subject_registrations` to detect anomalous registration patterns (e.g. a buggy app calling `register` in a hot loop).
- Async erasure: large FK chains may take minutes. `zeroship.db.eraseSubject` returns an `erasure_id` and runs asynchronously; the control plane exposes `GET /api/erasures/:id` for status. Same model as AWS S3's object-versioning erasure.
- Map corruption fallback: if `__zeroship_subject_map` becomes inconsistent (missing rows for a known subject), the platform falls back to scanning every FK-to-`subjects` collection — slow but exhaustive — and rebuilds the map row. The fallback is the audit safety net for legal compliance.

**Upgrade path from existing `id: number` fields.** Apps shipping today use `authorId: t.number()` for FK columns (no FK constraint, no brand). The V2 migration is type-only at the storage level — the underlying column stays BIGINT — but compile-error producing if applied naively:

```ts
// Before V2:
posts: { authorId: t.number(), title: t.string() }

// After V2:
posts: { authorId: t.ref("users"), title: t.string() }
```

The change adds an FK constraint (caught by A2 as an additive change — accepted if no orphan rows; surfaces as destructive otherwise). On the TS side, all call sites that pass a bare `number` to `authorId` now fail to type-check; the migration is to source from a `db.users.create(...)` or `db.users.findOne(...)` return (which gives `Id<"users">`).

Codemod: `zeroship migrate codemod refs` walks the app sources, finds `t.number()` fields used as FK targets (heuristic: column name matches `<table>Id` where `<table>` is a declared collection) and rewrites them to `t.ref("<table>")`. The codemod is conservative — it only rewrites unambiguous cases and emits a manual-review report for ambiguous ones.

**Risks.**

- Circular references between collections (users → posts → users) handled via `DEFERRABLE INITIALLY DEFERRED` constraints (see above) plus a topological sort for DDL emission ordering. Insert ordering is irrelevant because deferred constraints validate at commit.
- Cross-app refs not supported (per-app schema isolation invariant); enforced via `Tables<S>` constraint on `t.ref`.
- Identifier-truncation on FK constraint names mirrors A1's hash-suffix strategy.

### B3. Capability-scoped function kinds

**Motivation.** The RPC markers (`procedure`/`query`/`mutation`/`stream` from `@zeroship/server`) are routing-only today. They don't constrain what the function can do. AI-generated code freely mixes `fetch()`, DB writes, and reads inside a single `procedure`. This breaks transactionality guarantees and makes reactive-query subscription (Tier C) impossible.

**Design.**

Adopt Convex's three-kind model ([docs.convex.dev/tutorial/actions](https://docs.convex.dev/tutorial/actions)):

| Wrapper | Reads DB? | Writes DB? | External `fetch()`? | `ctx.db.invalidate()`? | `ctx.runMutation`? | Atomic? |
|---|---|---|---|---|---|---|
| `query()` | yes (snapshot) | no | no | no | no | yes (read-only tx) |
| `mutation()` | yes | yes | no | yes (synchronous fanout, same node) | no | yes (RW tx, retried) |
| `action()` | indirectly via `ctx.runQuery` | indirectly via `ctx.runMutation` | yes | yes (issues invalidations to the broker; no tx) | yes | no |

`migrateOne` (B1 callback) uses `MutationCtx` — DB read+write, no fetch, no invalidate (the migration component issues a coarse invalidation per batch).

Existing markers map:
- `procedure` → `action` (default — most permissive)
- `query` → `query`
- `mutation` → `mutation`
- `stream` → `action` (with stream return; see streaming note below)

**Streaming wrappers and reactivity.** The existing `stream` wrapper supports bidirectional WebSocket payloads (client→server and server→client) with user-defined framing. After C1 ships, two patterns:

- **For one-way live data (server→client read updates):** prefer `query` + `useQuery`. The subscription mechanism is automatic and re-execution is invalidation-driven.
- **For bidirectional or coarse-grained streams (chat-with-AI, file uploads):** `stream` remains the right primitive — it maps to `action` semantics (no DB transaction; fetch allowed) with a `WritableStream`/`ReadableStream` pair on `ctx`.

`stream` is not deprecated. The two wrappers solve different problems: `query` is for *typed reactive data*, `stream` is for *arbitrary byte/JSON flows*.

**Enforcement.**

Two layers:
1. **Type-level.** Each wrapper provides a `ctx` typed to its capability surface. A `mutation`'s `ctx` has `ctx.db.find`, `ctx.db.create`, etc. but **no `ctx.fetch`**. An `action`'s `ctx` has `ctx.fetch` + `ctx.runMutation` but no direct `ctx.db.create`. Types prevent the misuse from compiling.
2. **Runtime.** The kernel observes the wrapper kind from the synthetic registry and refuses operations that violate the capability. Defense-in-depth against type erasure. Violation throws a `CapabilityViolationError` (a normal JS exception inside V8, NOT a Rust panic) which the wrapper converts to a `{ data: null, error: { code: "capability_violation", … } }` tuple (note: `code`, not `kind` — consistent with the rest of `@zeroship/db`'s error taxonomy). Matches Convex's runtime-error model.

**Return-contract bridge.** Today `@zeroship/db` returns `{ data, error }` tuples; Convex `mutation` handlers throw. We keep the tuple contract **inside** the handler body so existing code is unchanged:

```ts
export const createPost = mutation({
  args: { authorId: t.ref("users"), title: t.string() },
  handler: async (ctx, args) => {
    const { data: post, error } = await ctx.db.posts.create(args);
    if (error) return { ok: false, code: error.code };  // user owns the surface
    return { ok: true, id: post.id };
  },
});
```

The wrapper does NOT auto-unwrap; user code keeps the tuple discipline. If the wrapper handler itself throws (uncaught), the wrapper translates to a structured error at the RPC layer (matches Convex). Migration impact for existing apps: zero — `procedure(...)` keeps the current semantics; `query` / `mutation` / `action` are new wrappers chosen at the wrapper-import site.

**Transactional boundary of `action.runMutation`.** Each `ctx.runMutation` call from an `action` is **its own transaction**, committed before the action continues. This matches Convex's documented model and is forced by the action's ability to call external `fetch()` — a long-running action holding a DB transaction would block other writers indefinitely. Implication: two consecutive `ctx.runMutation` calls are **not** atomic with each other. If the second fails after the first commits, the user must compose compensation explicitly. This is called out in the docs (`docs/reference/db.md`) with an example.

**HTTP wire format.** All wrappers (`procedure`, `query`, `mutation`, `action`, `stream`) share the same HTTP RPC wire format — the `query` wrapper's reactive path is overlaid on top of an identical one-shot HTTP fetch. This guarantees that the SSR fallback for `useQuery` (which uses the HTTP path) sees the same `{data, error}` envelope as a `procedure` call. The reactive subscription is a WS upgrade layered on the same wire shape; invalidation messages carry only `{ subscription_id, lsn }` and trigger a re-fetch over the WS multiplexed RPC.

**Why this matters for AI codegen.**

When AI generates a server function, the wrapper choice (`query` vs `mutation` vs `action`) commits to a capability. If the model emits `query(...)` and then writes `await ctx.db.users.create(...)`, that's a compile error before the code ever runs. The wrapper becomes a strong nudge toward the right shape.

**Implementation.**

- `@zeroship/server` ships new `ctx` types per wrapper kind
- Runtime: vite-plugin's synthetic entry tags each registered procedure with `kind: "query" | "mutation" | "action"`. Worker dispatches via kind-specific paths (read-only tx for query — `SET TRANSACTION READ ONLY`; RW tx for mutation with deterministic retry on serialisation conflict; no tx for action).
- Migration path: existing `procedure(...)` keeps working (maps to `action` semantics with a deprecation warning). Users opt into `query` / `mutation` for stricter guarantees. AI codegen prompt (`docs/research/ai-builder-features.md`) updates to prefer the more-specific kinds.

---

## Tier C — post-V1 transformative

### C1. Reactive queries via logical replication (WAL fanout)

**Motivation.** Convex's killer feature: `useQuery(api.foo)` auto-subscribes; mutations broadcast invalidations; clients re-render with new data. Live apps without polling, manual cache invalidation, or `revalidatePath` ceremony.

Today zeroship has WS + SSE infrastructure and no reactive DB. The naïve Postgres primitive is LISTEN/NOTIFY — and the prior draft of this proposal chose it. After deeper review (round 1 critic + Supabase's own [migration away from LISTEN/NOTIFY](https://supabase.com/blog/realtime-row-level-security-in-postgresql) and the [Stacksync analysis](https://www.stacksync.com/blog/beyond-listen-notify-postgres-request-reply-real-time-sync)), we **do not** build C1 on LISTEN/NOTIFY. Documented limits that disqualify it:

| Constraint | Limit | Source |
|---|---|---|
| Payload size | < 8000 bytes per notification | [Postgres NOTIFY docs](https://www.postgresql.org/docs/current/sql-notify.html) |
| Queue ceiling | 8 GB notification-queue; commits fail when full | Postgres docs |
| Listener fan-out | ~1000 listeners before contention dominates | Stacksync benchmarks |
| Commit serialisation | Single global queue serializes notifications across the cluster | [Postgres internals: NOTIFY uses a single global queue](https://www.postgresql.org/docs/current/sql-notify.html) |
| MVCC ordering | Notifications fire on commit; listener `SELECT` may still see pre-commit snapshot if not in a fresh tx | Postgres docs |

For a multi-tenant platform where every app could subscribe to its own collections, listener fan-out alone disqualifies LISTEN/NOTIFY.

**Chosen mechanism: logical replication → broker → fanout.**

Same architecture Supabase Realtime uses. Phases:

1. **Per-app publication.** Each app's schema gets `CREATE PUBLICATION __zeroship_pub_<app_id> FOR ALL TABLES IN SCHEMA "<app_id>"` **followed by** `ALTER PUBLICATION __zeroship_pub_<app_id> DROP TABLE __zeroship_migrations, __zeroship_subject_map` (Postgres 15+ also supports `WHERE` filters per-table; we use explicit drop for portability). The platform-managed tables MUST NOT appear in the change feed — they contain audit + PII bridge data that subscribers should never see. One publication per app keeps the WAL stream tenant-scoped.
2. **Replication slot.** The control plane creates one logical-decoding slot (`pgoutput` plugin) per app: `__zeroship_slot_<app_id>`. The slot is `wal_status='reserved'` so disconnections don't lose events. Naming convention is stable: `__zeroship_pub_<app_id>` for the publication, `__zeroship_slot_<app_id>` for the slot — both filterable via `LIKE '__zeroship_%'` in the watchdog query.
3. **Broker (new crate `crates/replication`).** A compio-native consumer of the replication slot decodes `Insert`/`Update`/`Delete` events from `pgoutput`. Each event becomes `{app_id, schema, table, op, pk, changed_columns, new_tuple_excerpt}` — `new_tuple_excerpt` is the indexed columns only (kept under 1 KB so the broker's per-app fanout stays cheap).
4. **Fanout.** The broker maintains per-app subscriptions keyed by `(app_id, query_fingerprint)`. On each event, the broker looks up matching fingerprints and pushes invalidations to the worker subscription manager via WebSocket multiplex on the existing transport.
5. **Read-set capture (see below).** When a worker's `query` runs, it records its dependency set; the broker uses this to match incoming events.

```ts
// Server:
export const listMessages = query({
  args: { channelId: t.ref("channels") },
  handler: async (ctx, { channelId }) => {
    return ctx.db.messages.find({ channelId }).sort({ createdAt: -1 }).limit(50);
  },
});

// Client (React):
const messages = useQuery(api.listMessages, { channelId });
// Auto-subscribes. Any commit affecting matched rows fires the WAL event;
// the broker matches against the query's read set; the worker re-executes
// and pushes new results via WebSocket.
```

**Read-set capture.** The runtime instruments `ctx.db` calls inside a `query` handler — it does **not** rely on `EXPLAIN` or static analysis. Capture grammar:

```
ReadSet := { table: string, predicate: NormalisedPredicate, columns: Set<string> }
NormalisedPredicate := Conjunction of (column, op, value | "*")
```

Each `ctx.db.<col>.find({...})` appends a ReadSet entry. The runtime serialises the read set as a stable JSON fingerprint hashed against the wire arguments. The broker indexes active fingerprints in-memory; lookup on an incoming WAL event is `O(predicates_touching_this_table)` per event, not `O(subscriptions)`. Concretely: an event on `messages` with `channelId=42` matches any active fingerprint with `(messages, channelId, =, 42 | *)`.

False positives (re-execute when not actually invalidated) are accepted as a tradeoff vs. Convex's engine-level exactness, but the predicate-filtered fingerprint cuts the vast majority of unnecessary re-execution.

**Trade-offs vs Convex.**

- Convex's reactivity is exact (every write knows what queries to invalidate at the engine level)
- Ours is predicate-filtered conservative — events on `messages` with `channelId=42` re-run only queries that include `channelId=42 | *`, not all `messages` queries
- Eventually consistent: lag = WAL emission delay + broker processing + WS hop. Lag bounds will be characterised by a benchmark in `crates/replication/benches/fanout_latency.rs` before C1 ships; the SLO will be set from the measurement, not committed up-front. For strict-consistency cases users can call `await ctx.db.invalidate(...)` from a mutation (synchronous fanout path on the same node).

**Backpressure.** If a subscriber falls behind, the broker maintains a bounded per-subscription queue (default 1024 events). Overflow triggers `"resync"` — the client re-fetches once and drops queued events. Same model as Supabase Realtime.

**Per-app isolation.** Slots and publications are per-app; the broker maintains per-app workers with their own LSN cursors. One app's high write rate cannot starve another app's slot consumption.

**Slot WAL retention.** A replication slot retains all WAL since its `confirmed_flush_lsn`. If the broker stops consuming (crash, deploy bug, network partition), Postgres holds WAL forever — `pg_wal` fills, primary stops accepting writes. This is the [single biggest operational risk of logical replication](https://www.morling.dev/blog/mastering-postgres-replication-slots/). Mitigations:

1. `max_slot_wal_keep_size = 32GB` on the primary (cluster setting). Beyond this, Postgres marks the slot `invalidated` and reclaims WAL; the broker's restart will see `confirmed_flush_lsn` lost and trigger a full re-sync of subscriptions for that app.
2. Watchdog query, run every 60s by the control plane:
   ```sql
   SELECT slot_name, active, restart_lsn, confirmed_flush_lsn,
          pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)) AS lag_bytes
   FROM pg_replication_slots
   WHERE slot_name LIKE '__zeroship_%';
   ```
   Alerts fire at lag > 8 GB (warn) and > 24 GB (page). [Cybertec's runbook](https://www.cybertec-postgresql.com/en/why-does-my-pg_wal-keep-growing/) is the model.
3. Slot GC: any slot inactive for > 1 hour is dropped by the sweeper (`pg_drop_replication_slot`). Affected apps see a `resync` event on next subscriber connection.
4. Per-app slot creation lifecycle: created on first `useQuery` subscription; dropped 24 hours after the last subscriber disconnects.

**Backup/restore.** Logical-replication slots are NOT preserved by `pg_dump` or PITR restore. After a restore, the broker re-creates the slot from the head of the restored WAL; subscribers see a one-time `resync`. Documented in the runbook (`docs/runbooks/disaster-recovery-db.md`, to be authored alongside C1).

**Sandboxing.** The broker runs as a dedicated OS user with read access to its own slot only. Privilege separation: each app's broker reader process is forked with `setuid` to a per-app UID derived from `app_id`, enforced by the existing sandbox (`crates/sandbox`). Compromise of one broker process leaks at most one app's WAL stream, not all apps'.

**Client API surface.**

```ts
// React hook — auto-subscribes on mount, unsubscribes on unmount
const messages = useQuery(api.listMessages, { channelId });
// messages: { data: Message[] | undefined, error: Error | null, isLoading: boolean, isStale: boolean }

// Vanilla JS / non-React
const sub = db.subscribe(api.listMessages, { channelId }, (data) => { /* re-render */ });
sub.close();
```

**SSR / React Server Components.** No `ctx` exists in a server-rendered React tree (Node render or RSC), so `useQuery` cannot subscribe. Behavior by environment:

- **Server (Node render or RSC):** `useQuery` detects the server env and synchronously *or* via Suspense fetches the query over the existing HTTP RPC path — exactly the same as a normal `procedure(...)` call. No subscription is opened.
- **Hydrate:** on the client, `useQuery` reads the SSR-emitted snapshot from the React data cache, then opens a WS subscription. The first re-render shows the snapshot; subsequent re-renders react to broker invalidations.
- **Mechanism:** mirrors Convex's `<ConvexHttpClient>` + `<ConvexProvider>` split — HTTP for one-shot fetches, WS for subscriptions. The decision is made at runtime via a `typeof window` check inside the hook (or via a React env-detection primitive when one stabilises).
- **RSC particular case:** Server Components can't useState/useEffect, so they never subscribe. The pattern: render the initial data in an RSC, pass it as a prop to a Client Component that *then* calls `useQuery` for live updates. Matches Next.js's recommended pattern for any client-realtime data.

**Reconnect.** On WS drop, the client re-establishes by replaying its subscription registry. Each subscription resumes at its last-known LSN where possible; if the broker's slot has advanced past that LSN, the client gets a one-time fresh fetch + new subscription. Same model as Supabase Realtime's `replay-from-lsn` reconnect.

**Multi-region.** Initial rollout: broker co-located with the primary DB region. Subscribers in other regions accept the round-trip latency. A future addendum (C1.1) covers regional broker replicas reading from physical replicas with replication-lag-aware LSN gating.

**Implementation scope.**

Substantial. New crate `crates/replication` for the broker. New SDK surface (`useQuery`, `subscribe`). WAL slot lifecycle management in the control plane (create on first subscription, GC on app deletion). WS multiplexing on top of the existing transport.

**Defer to post-V1.** This is the single biggest UX upgrade we could ship; also the largest scope. The mechanism is well-trodden — Supabase Realtime, Materialize, ReadySet, Sequin all use pgoutput-based fanout.

### C2. Discriminated union document shapes

**Motivation.** Some collections hold polymorphic shapes — `events` with `{ kind: "login", ... } | { kind: "error", ... }`. Today users either flatten into one ugly union of optionals or use `t.json()` and lose type safety.

**Design.**

New `t.union(...)` builder:

```ts
events: t.union(
  t.object({ kind: t.literal("login"),  userId: t.ref("users"), ip: t.string() }),
  t.object({ kind: t.literal("error"),  message: t.string(), stack: t.string().optional() }),
  t.object({ kind: t.literal("metric"), name: t.string(), value: t.number() }),
);
```

**Storage strategy: two modes, user-chosen.**

| Mode | DDL | Pros | Cons |
|---|---|---|---|
| `flat` (default) | one column per union-wide field, `kind` discriminator, CHECK constraints per variant | columnar query speed, indexable per-variant fields | wide tables for sparse variants |
| `jsonb` | single `payload JSONB NOT NULL` column + indexed `kind` extraction | compact for highly-divergent variants | no per-field indexing without GIN |

**Flat mode integrity.** Variant-required fields must be NOT NULL within their variant; the prior draft made them globally nullable, which silently allowed `kind='login'` rows with NULL `userId`. The diff engine emits Postgres CHECK constraints per variant:

```sql
ALTER TABLE events ADD CONSTRAINT events_login_chk CHECK (
  kind <> 'login' OR (user_id IS NOT NULL AND ip IS NOT NULL)
);
ALTER TABLE events ADD CONSTRAINT events_error_chk CHECK (
  kind <> 'error' OR message IS NOT NULL
);
ALTER TABLE events ADD CONSTRAINT events_metric_chk CHECK (
  kind <> 'metric' OR (name IS NOT NULL AND value IS NOT NULL)
);
```

Adding a new variant is an additive change (new CHECK is auto-applied if no existing rows violate it — A2 validation runs the check). Removing a variant is destructive. Adding a required field to an existing variant is destructive unless the variant has zero rows.

**Implementation.**

- Schema serialization gets a `union` variant; DDL widens columns to nullable + per-variant CHECK
- Validation: dispatch on `kind` value, run the matching variant's validator
- Type generation: TS union via `InferSchema` with discriminated narrowing

---

## Tier D — polish

### D1. Index awareness — lint + dev-mode runtime warnings

**Mechanism.**

- **ESLint rule** in `@zeroship/eslint-config` flagging `.find({...})` calls where the filter is selective (single-field equality on a frequently-queried column) but no `index: true` marker exists on that field. Match Convex's ESLint rule shape.
- **Dev-mode runtime warning.** When the runtime detects a query that does a sequential scan with a filter that could have used an index, log a one-time warning per query shape pointing at the suggested index.

Neither is a hard error. Both are nudges.

### D2. Nested object validators

**Mechanism.**

Add `t.object(...)`:

```ts
profile: t.object({
  bio:     t.string().max(500),
  avatar:  t.string().pattern(/^https:\/\//),
  social:  t.object({
    twitter: t.string().optional(),
    github:  t.string().optional(),
  }),
}),
```

Storage: JSONB column. Validation: nested traversal in `validate.ts`. Type inference cascades through the structure.

**Schema evolution of nested objects.** Renaming a nested key (e.g. `social.twitter` → `social.x`) is invisible to the column-level diff classifier — the column is still `JSONB`. To make nested evolution observable, the diff engine hashes the declared nested schema and stores it alongside the column metadata in `__zeroship_migrations.details.nested_schema_hash`. A hash change emits a `nested_schema_change` event classified as **destructive** by default — users must explicitly migrate via `migrations.run(renameNestedKey)`. Inspired by Convex's `defineSchema` per-table validators; same pattern as Sanity's nested-schema migration tooling.

**JSONB CHECK constraints.** D2 does NOT auto-emit CHECK constraints on JSONB values (Postgres CHECK on JSONB is expressible but expensive at write time). Validation is application-side via `validate.ts`. Raw SQL writes that bypass the SDK can store malformed JSONB; documented limitation. Apps that need strong shape guarantees should use C2 (discriminated union with `flat` mode) instead.

### D3. Calendar dates

**Mechanism.**

`t.calendarDate()` for `YYYY-MM-DD` without timezone. Stored as Postgres `DATE`. Distinct from `t.date()` (timestamp; Unix ms).

### D4. Optimistic concurrency

**Mechanism.**

Optional `version` column on a collection:

```ts
posts: defineCollection({
  title: t.string().required(),
  body:  t.string().required(),
}).withVersioning();  // auto-injects `version: number`, auto-increments on update

// Update with CAS:
await db.posts.updateOne({ id, version: 7 }, { title: "x" });
// → returns error if version mismatch
```

DDL: `version INTEGER NOT NULL DEFAULT 1`. Update SQL: `UPDATE ... SET version = version + 1 WHERE id = ? AND version = ?`. On mismatch the SDK returns `{ data: null, error: { code: "optimistic_lock_failure", expected_version: 7, actual_version: 9 }}` — matches the rest of `@zeroship/db`'s error taxonomy.

---

## Phased delivery

The proposal naturally splits into shippable PRs. Order optimizes for landing V1 safety quickly:

| Phase | Scope | Closes | Risk |
|---|---|---|---|
| **P1** | A1 (materialize indexes) | silent uniqueness bug | low |
| **P2** | A2 (deploy-time data validation) + A3 (migration log) | ISS-24, silent breakage | low |
| **P3** | B1 (`@zeroship/migrations` component) | destructive change workflow | medium |
| **P4** | B2 (typed refs / `t.ref`) | cross-table typo class | medium |
| **P5** | B3 (capability-scoped function kinds) | AI-codegen safety | medium (touches `@zeroship/server` + vite-plugin) |
| **P6** | D1 (lint + dev warnings) + D2 (nested validators) + D3 (calendar dates) + D4 (optimistic concurrency) | polish | low (each piece independent) |
| **P7** | C2 (discriminated unions) | polymorphic collections | medium |
| **P8** | C1 (reactive queries) | the UX upgrade | high (large scope) |

P1 + P2 are the V1 blockers. P3-P5 are launch quality. P6-P8 are post-V1.

The diff engine (A2) is the foundation for many later phases:
- B1 uses it to detect schema-incompatible migrations
- B2 uses it to detect FK additions/removals
- C2 uses it to detect union-variant additions
- D1's runtime warnings use the same introspection

So the order is correct: build the foundation in A2, then everything else composes on top.

---

## Testing strategy

Test surfaces, per phase:

| Component | Test surface | Crate / location |
|---|---|---|
| Diff classifier | Table-driven unit tests with desired/live snapshot pairs → expected change list | `crates/plugin-db/src/diff.rs` (`#[cfg(test)]`) |
| DDL emitter | Snapshot tests: golden SQL output for each (change_class, change_kind) | `crates/plugin-db/src/query.rs` |
| INVALID-index recovery | Integration: induce CONCURRENTLY failure via a parallel `INSERT` violating a unique constraint, assert retry + DROP | `crates/plugin-db/tests/concurrent_index.rs` |
| Validation budget | Run on 1M-row fixture, assert budget honoured + deferred path | `crates/plugin-db/tests/validation_budget.rs` |
| Migration ownership | Spawn two workers, both call `migrations.run(name)`, assert only one runs; kill owner, assert sweeper hands off | `sdks/migrations/tests/concurrent_run.test.ts` |
| WAL broker | Replay golden `pgoutput` byte stream, assert events match | `crates/replication/tests/decode_golden.rs` |
| Read-set capture | Run a `query` handler, assert captured ReadSet matches expected predicates | `crates/runtime/tests/readset_capture.rs` |
| End-to-end deploy refusal | E2E test: deploy schema, insert violating row, redeploy with stricter schema, assert refusal envelope | `tests/e2e_db_v2.sh` |

WPT pattern: integration tests against a real Postgres in a CI matrix (**16 LTS + 17 current**; 18 opportunistic — see Postgres version pinning above).

## Installation order

The SECURITY DEFINER functions reference each other and must be installed in dependency order. The control-plane bootstrap runs this sequence (idempotent — wrapped in `CREATE OR REPLACE` where Postgres allows):

1. Cluster-wide (run once at cluster setup):
   - `CREATE ROLE __zeroship_admin`, `__zeroship_platform_role`, `__zeroship_maintenance_role` (the last has `pg_read_all_stats`)
   - `CREATE EXTENSION pgcrypto WITH SCHEMA extensions`
   - `CREATE SCHEMA __zeroship_admin AUTHORIZATION __zeroship_admin`
   - `CREATE TABLE __zeroship_admin.__zeroship_session_keys (...)`, `__zeroship_admin.__zeroship_session_ctx (...)`
   - `__zeroship_const_eq` function (no dependencies)
   - `__zeroship_verify_signature` (uses `__zeroship_const_eq`, `__zeroship_session_keys`, `extensions.hmac`)
   - `__zeroship_sign_session` (uses `__zeroship_session_keys`, `extensions.hmac`)
   - `__zeroship_init_session` (uses `__zeroship_verify_signature`, `__zeroship_session_ctx`)
   - Initial key INSERT into `__zeroship_session_keys`
2. Per app (run on app provisioning, see *Per-app role provisioning* below):
   - `CREATE ROLE app_<id>_role NOLOGIN`
   - `CREATE SCHEMA "<app_id>" AUTHORIZATION __zeroship_admin`
   - `CREATE TABLE "<app_id>".__zeroship_migrations (...)`, `__zeroship_subject_map (...)`, `__zeroship_migration_dead_letter_overflow (...)`
   - `__zeroship_log_migration`, `__zeroship_transition_migration`, `__zeroship_log_dead_letter`, `__zeroship_register_subject`, `__zeroship_resolve_subject` (all in the app schema)
   - GRANT EXECUTE on the app-facing functions to `app_<id>_role`
3. Per session (run by worker on connection acquire — covered in *Per-app role provisioning* §5)

The order is encoded in `crates/control/src/migrations/zeroship_admin/*.sql` (cluster-wide, run by the control plane at cluster setup) and `crates/control/src/migrations/per_app/*.sql` (templated per app, run on app create).

---

## Per-app role provisioning

The Postgres role `app_<id>_role` is created by the control plane at app-provisioning time (the existing `POST /api/apps` flow). Role lifecycle:

1. **Create.** When an app is provisioned, the control plane (running as `__zeroship_admin`) executes:
   ```sql
   CREATE ROLE app_<id>_role NOLOGIN;
   CREATE SCHEMA IF NOT EXISTS "<app_id>" AUTHORIZATION __zeroship_admin;
   GRANT USAGE, CREATE ON SCHEMA "<app_id>" TO app_<id>_role;
   GRANT EXECUTE ON FUNCTION __zeroship_log_migration TO app_<id>_role;
   GRANT EXECUTE ON FUNCTION __zeroship_transition_migration TO app_<id>_role;
   GRANT EXECUTE ON FUNCTION __zeroship_register_subject TO app_<id>_role;
   GRANT EXECUTE ON FUNCTION __zeroship_log_dead_letter TO app_<id>_role;
   ```
2. **Credentials.** A login role `app_<id>_user LOGIN PASSWORD '<random>'` is created and `GRANT app_<id>_role TO app_<id>_user`. Password is stored in the control plane's secrets store (existing `crates/control/src/secret_store.rs`) and injected into the worker as an env-var-style connection string.
3. **Rotation.** Quarterly automated rotation: control plane generates a new password, dual-grants (old + new accepted for 24h), then revokes the old. Workers reconnect with new credentials on next pool-acquire.
4. **Deletion.** App deletion triggers `REVOKE ALL ... FROM app_<id>_role`, `DROP ROLE app_<id>_user`, then `DROP ROLE app_<id>_role`. Schema is preserved for compliance retention; ownership transfers to `__zeroship_admin`.
5. **Per-RPC session init.** `__zeroship_init_session` is called at the **start of every RPC handler**, not just at connection acquire. This means the actor context is bound to the RPC, not the connection — a connection used for a `procedure(...)` call carries `actor_kind='user'`; the same connection used for a GDPR-erasure path carries `actor_kind='operator'`. Per-RPC init prevents "operator-context bleeding" into general app code on a shared connection.
   ```sql
   SELECT __zeroship_init_session($1, $2, $3, $4, $5);
   --                              actor_kind, actor_id, signature, nonce, expires_at
   ```
   At RPC handler exit, the worker calls `__zeroship_reset_session()` (SECURITY DEFINER, deletes the row for `pg_backend_pid()`) to ensure no stale context remains on the connection. The session-ctx PID-GC sweeper (Maintenance cron) handles missed resets on crashed workers.
   ```sql
   CREATE FUNCTION __zeroship_reset_session() RETURNS VOID
   LANGUAGE plpgsql SECURITY DEFINER
   SET search_path = pg_catalog, pg_temp AS $$
   BEGIN
     DELETE FROM __zeroship_admin.__zeroship_session_ctx WHERE pid = pg_backend_pid();
   END $$;
   REVOKE ALL ON FUNCTION __zeroship_reset_session FROM PUBLIC;
   GRANT EXECUTE ON FUNCTION __zeroship_reset_session TO app_<id>_role;
   ```
   The signature is computed by the control plane (HMAC-SHA256 over `actor_kind || actor_id || pg_backend_pid || session_nonce || expires_at`); the SECURITY DEFINER function verifies before writing the session context row. App SQL cannot forge a valid signature because the HMAC key lives only in `__zeroship_admin`-owned `__zeroship_session_keys` (not readable by `app_<id>_role`). See A3 for the full mechanism.
   **Per-RPC overhead.** Two extra SQL calls per RPC (init + reset). Both are SECURITY DEFINER lookups on a small table keyed by PID — measured cost will be in the same `db_connect_init.rs` benchmark. The trade-off (per-RPC overhead vs. context-bleeding risk) is judged worth it because reactive queries (C1) keep connections open for subscription state, making per-connection actor-binding insecure.

The role provisioning runs inside the existing app-create transaction; failure rolls back the app record. Documented in `docs/runbooks/app-provisioning.md` (to author alongside Tier A).

---

## HA and replication topology

- **Primary-only writes.** Logical-decoding slots only live on the primary. Failover promotes the standby; the broker reconnects after a slot-recreate (the slot is NOT physically replicated). Subscribers see a `resync`.
- **Standby reads.** Future addendum. V2 routes all `query`/`mutation` to the primary.
- **Slot promotion.** Postgres 16+ supports failover slots via `synchronized_standby_slots`. We track this; for V2 the slot is primary-only and the resync after failover is documented as a known one-time client effect.
- **DR.** PITR restore re-creates slots from head of restored WAL. Subscribers re-fetch.

---

## Maintenance cron

A unified list of every periodic sweeper this proposal introduces, where it runs, and at what cadence. All run on the control plane (single coordinator, leader-elected via the existing control-plane Raft).

| Sweeper | Source | Cadence | Action |
|---|---|---|---|
| Migration heartbeat sweeper | B1 ownership | 30s | `SELECT id, owner_session_id FROM __zeroship_migrations WHERE status='running' AND last_heartbeat_at < NOW() - INTERVAL '30 seconds'`; for each, `pg_terminate_backend(owner_session_id)` (releases advisory lock) and `UPDATE ... SET owner_session_id=NULL` |
| Stuck-pending GC | B1 | 5min | `status='pending'` migrations queued > 1 hour without a worker pickup — alert operator, do not auto-cancel |
| Migration log retention rollup | A3 | daily | Roll up rows older than 90 days into `(collection, change_kind, month)` aggregate rows; delete originals |
| Migration log archival | A3 | weekly | Rows > 1 year deleted (opt-out per app via `app_config.retain_migrations_forever`) |
| Replication-slot lag watchdog | C1 | 60s | Watchdog query in C1; warn at 8 GB, page at 24 GB |
| Inactive-slot GC | C1 | hourly | Slots with `active=false` for > 1 hour are dropped; affected apps' next subscriber gets a resync |
| Slot disk-pressure auto-invalidate | Postgres (`max_slot_wal_keep_size`) | continuous | Postgres auto-marks slots invalid past the limit; the control plane subscribes to the `pg_stat_replication_slots` view changes and emits an alert |
| Subject-map consistency check | B2 GDPR | weekly | Detect orphaned subject_typed_id rows (no matching row in target collection) and emit operator alert |
| Dead-letter retention | B1 | weekly | Migrations in `applied_with_dead_letter` for > 30 days without operator action are escalated |
| Session-ctx PID GC | A3 trust model | hourly | `DELETE FROM __zeroship_session_ctx WHERE pid NOT IN (SELECT pid FROM pg_stat_activity)` — removes rows from terminated backends. Sweeper role must hold `pg_read_all_stats` (Postgres 10+ predefined role) to read `pg_stat_activity` across all backends; granted at cluster setup. |
| HMAC key rotation | A3 session-init | weekly | `INSERT __zeroship_session_keys (...gen_random_bytes(32)...); UPDATE prior keys SET retired_at = NOW(); DELETE keys WHERE retired_at < NOW() - INTERVAL '24h'` |

All sweepers are idempotent. Failure of a sweeper run never corrupts state; the next run picks up where the previous left off.

---

## Open questions

1. **Composite indexes API.** `defineCollection().index("name", ["columns"])` borrows from Convex. Acceptable? Or prefer a `schema()` builder option?
2. **Migration scheduling for multi-tenant.** If many apps deploy simultaneously with destructive changes pending, the operator approval queue might back up. Should the platform offer auto-approval for trivially-safe destructive changes (e.g. dropping a column that was added in the same session)?
3. **typed_id for app data?** Today's split: platform entities use typed_id; per-app collection rows use BIGINT IDENTITY. Future opt-in via `defineCollection().id("typed_id", "msg")` is technically straightforward but adds a destructive-migration path for adopters. Defer or surface in V2?
4. **Internal-fetch from `mutation`.** Should the runtime allow `fetch()` to *internal* RPC endpoints (e.g. `ctx.fetch("/api/internal/billing")`)? Decision for V2: **no** — `fetch` is an action capability only. Internal RPCs are reached via `ctx.runMutation` / `ctx.runQuery` typed bindings. Moved out of open questions; recorded here for traceability.
5. **Schema versioning under rollback.** When code rolls back from v3 to v2, the live schema is at v3 (DDL is additive-by-default). v2 reads should still work (forward-compatible columns are nullable additions). v2 writes that depend on a v3-only column will fail. The `schema_version` column in `__zeroship_migrations` is added in A3; the control-plane gate that warns operators on rollback if the target version's schema is unreachable from the live state remains an **open design** — sketch a "compat matrix" stored alongside deploys.
6. **DEFERRABLE INITIALLY DEFERRED measurement.** Throughput cost on a representative insert workload is unknown. Plan: micro-benchmark in `crates/runtime/benches/db_deferred_fk.rs` before P4 ships. If the cost exceeds a threshold the team accepts (TBD from measurement), switch the default to NOT DEFERRABLE.
7. **WAL fanout latency measurement.** Per-hop latency (Postgres commit → pgoutput emission → broker decode → WS push → client render) needs measurement on representative app shapes before publishing an SLO. Bench in `crates/replication/benches/fanout_latency.rs` before C1 ships.
8. **PgBouncer compatibility.** Session-level advisory locks (B1) require a non-pooled connection. If the deployment topology uses PgBouncer in `transaction` mode, migrations need a bypass path. Open: do we ship our own connection management for migrations, or document the PgBouncer constraint and require operators to provide a direct connection string?
9. **Multi-region brokers (C1).** Co-locate broker with primary in V2; multi-region replicas with replication-lag-aware fanout is C1.1.
10. **Composite-PK validation cursor.** `validate_cursor` is `BIGINT` — works for single-column PKs. Collections with composite PKs (future, via D2/D4 composition) need a tuple cursor or a stable ordering by `(pk1, pk2)`. Note this limitation for V2.
11. **"Current" schema_version semantics.** `__zeroship_migrations.schema_version` is set per-deploy (computed once in the orchestrator, applied to every row of that deploy). For rollback gating, "current schema" = max `schema_version` where `phase='ddl' AND status='applied'`. Alternative: track current separately in a `__zeroship_schema_state` singleton row. Open which model is cleaner.
12. **Cross-region data residency (EU/GDPR).** Single-region DB + broker for V2. EU-only apps need EU-only DB + EU-only broker placement. Spec deferred to a follow-up data-residency proposal alongside per-region control-plane fanout.
13. **Spatial / full-text / time-series.** Not in V2: PostGIS `geometry`, `tsvector` full-text, timescale time-series. Each is feasible as a future field-type addition; the type-builder is extensible (`t.geometry()`, `t.fts()`, `t.timeseries()`). Spec'd in follow-up proposals when user demand is verified.
14. **Pagination cursor primitive.** `db.posts.find({}, { cursor: nextCursor, limit: 50 })` is missing from the proposal. Defer to D-tier follow-up; meanwhile users compose via `id > lastSeenId ORDER BY id`.

---

## References

**Convex (primary peer):**
- [docs.convex.dev/database/schemas](https://docs.convex.dev/database/schemas) — schema definitions, validation, push-time refusal, `schemaValidation` opt-out
- [docs.convex.dev/api/interfaces/server.DefineSchemaOptions](https://docs.convex.dev/api/interfaces/server.DefineSchemaOptions) — full `schemaValidation` semantics
- [docs.convex.dev/tutorial/actions](https://docs.convex.dev/tutorial/actions) — query/mutation/action capability model
- [docs.convex.dev/database/document-ids](https://docs.convex.dev/database/document-ids) — `Id<"users">` brand model (strings at runtime)
- [docs.convex.dev/production](https://docs.convex.dev/production) — schema-constraints / safe-changes guidance
- [docs.convex.dev/client/react](https://docs.convex.dev/client/react) — `useQuery` reactive subscription
- [github.com/get-convex/migrations](https://github.com/get-convex/migrations) — batched + resumable migration component (the model for B1)
- [stack.convex.dev/triggers](https://stack.convex.dev/triggers) — explicit-cascade pattern (informs our `ON DELETE RESTRICT` default)

**Postgres operational:**
- [PostgresAI: hidden cost of invalid indexes](https://postgres.ai/blog/20260106-invalid-index-overhead) — CONCURRENTLY failure mode (A1 recovery)
- [pganalyze: invalid indexes check](https://pganalyze.com/docs/checks/schema/index_invalid) — `indisvalid` detection
- [Bytebase: CREATE INDEX CONCURRENTLY guide](https://www.bytebase.com/blog/postgres-create-index-concurrently/) — REINDEX CONCURRENTLY in-place replacement
- [Postgres docs: NOTIFY](https://www.postgresql.org/docs/current/sql-notify.html) — 8000-byte payload, queue limits
- [Stacksync: beyond LISTEN/NOTIFY](https://www.stacksync.com/blog/beyond-listen-notify-postgres-request-reply-real-time-sync) — scalability ceiling justifying WAL approach

**Other industry references:**
- [Supabase Realtime](https://supabase.com/blog/realtime-row-level-security-in-postgresql) — pgoutput-based fanout (the C1 model)
- [Sequin: change-data-capture in Postgres](https://blog.sequinstream.com/all-the-ways-to-capture-changes-in-postgres/) — LISTEN/NOTIFY vs WAL vs triggers comparison
- [Morling: Mastering Postgres Replication Slots](https://www.morling.dev/blog/mastering-postgres-replication-slots/) — abandoned-slot risk, `max_slot_wal_keep_size` (C1 retention strategy)
- [Cybertec: Why does my pg_wal keep growing?](https://www.cybertec-postgresql.com/en/why-does-my-pg_wal-keep-growing/) — operational runbook (C1 watchdog)
- [PlanetScale deploy requests](https://planetscale.com/docs/concepts/deploy-requests) — destructive-change approval workflow (informs A2 + B1 gating)
- [Atlas schema-revisions](https://atlasgo.io/concepts/migration-directory) — partitioned audit table (informs A3 retention)
- [Prisma soft-delete middleware](https://www.prisma.io/docs/orm/prisma-client/queries/soft-delete-middleware) — informs B2 soft-delete shape
- [Drizzle: defineRelations (v2)](https://orm.drizzle.team/docs/relations-v2) — multi-hop join typing (B2 deferred addendum)
- [Postgres explicit locking](https://www.postgresql.org/docs/current/explicit-locking.html) — advisory lock session vs transaction scope (B1)
- [GitGuardian detectors](https://docs.gitguardian.com/secrets-detection/detectors) — known-prefix denylist for A3 redaction
- [Cybertec: Abusing SECURITY DEFINER functions](https://www.cybertec-postgresql.com/en/abusing-security-definer-functions/) — search-path injection + timing-attack hardening for A3 SECURITY DEFINER
- [Postgres ALTER ROLE](https://www.postgresql.org/docs/current/sql-alterrole.html) — custom-GUC permission model rationale for the PID-keyed context table
- [truffleHog detectors](https://github.com/trufflesecurity/trufflehog) — 700+ secret-pattern reference (A3)
- [pg_cron](https://github.com/citusdata/pg_cron) — Postgres-native cron, ownership-pattern reference for B1
- [Materialize](https://materialize.com/), [ReadySet](https://readyset.io/), [Sequin](https://sequinstream.com/) — production pgoutput-based CDC systems (C1 mechanism references)
- [Liquibase](https://www.liquibase.org/), [Flyway](https://flywaydb.org/) — incumbent imperative-migration tools (contrast for the "schema-is-source-of-truth" model)
- [Inngest](https://www.inngest.com/), [Trigger.dev](https://trigger.dev/) — modern background-job orchestration patterns (informs B1 migration runtime)

**zeroship internal:**
- `docs/reference/db.md` — current `@zeroship/db` reference
- `docs/proposals/rpc-v2.md` — current RPC contract (procedure/query/mutation/stream wrappers)
- `docs/reference/billing-metering.md` — `zeroship.meter` integration for migration quotas (B1)
- `AGENTS.md` — typed_id invariant referenced in B2 ID-system discussion
- `ISSUES.md` ISS-24 — migration log requirement
- `sdks/db/src/{db,types,schema,validate,collection,query,model}.ts` — current SDK
- `crates/plugin-db/src/{query,callbacks}.rs` — current native primitives
