# Schema Authority Separation — the Drizzle Model

Status: **proposal** (pre-implementation; user-approved direction). Date: 2026-06-18.
Supersedes: `docs/proposals/2026-06-17-runtime-deploy-migration-convergence-design.md`
(the "registerModel → verify-only" convergence variant).

## 1. Goal

Make **`zeroship-migrate` the single schema authority** for creator apps and
**plugin-db a pure data-access plane**, while keeping a **Drizzle-style typed
schema** for authoring + strong types. Three layers, one owner each:

- **`schema.ts`** (the `@zeroship/db` `t.*` DSL, incl. the goodies
  `t.encrypted`/`t.vector`/`t.mask`/`t.geoPoint`/`t.ref(onDelete…)`/`t.fts`) —
  a **dev/build-time authoring surface + the type source**. NOT consumed at
  runtime; NOT `export default { schema }`.
- **`zeroship-migrate`** — diffs `schema.ts` → emits **versioned migrations**
  (`generate`, like `drizzle-kit generate`); applies them at deploy; owns all
  DDL incl. the goodies (relocated from plugin-db); is the source of truth.
- **plugin-db** — pure data access (CRUD/txn/AEAD/mask-read/metering). Learns
  column behavior by **introspecting the live schema + the engine's sentinels**
  at runtime. No migration, no diff, no verify, no `registerModel`, no declared
  schema.

### Why this beats the verify-only convergence
The prior design-critic gave the verify-only convergence 46/100 with 4 CRITICALs.
This model dissolves all four by construction:

| Prior CRITICAL | Dissolved because |
| --- | --- |
| #1 two-applier race (plugin-db + engine both apply DDL; disjoint advisory-lock namespaces) | **One migrator** — only the engine applies DDL, at deploy. plugin-db applies none. |
| #2 engine descriptor-gap (engine rejects vector/encrypted/mask) | **Relocation** — plugin-db's full DDL/diff *moves into* the engine; the engine becomes complete, not a subset. |
| #3 verify-by-rediff false-positives brick apps | **There is no verify.** Deploy-ordering (migrate-before-serve) replaces it; if the schema were behind, runtime CRUD errors normally. |
| #4 build-time JS schema materialization | **No declared schema** is consumed at deploy/runtime; `schema.ts` is consumed at dev time by `generate`, which produces a static migration file. |

## 2. The two models, side by side (why dropping `default.schema` ≠ dropping the typed schema)

| | `default.schema` (DROPPED) | `schema.ts`, Drizzle-style (KEPT) |
| --- | --- | --- |
| Consumed | **Runtime** (`registerModel` auto-migrates on boot) | **Dev/build time** (`zeroship-migrate generate`) |
| Produces | magic auto-migrate, no history | a **versioned migration file** |
| Source of truth | the declared object | the **migrations** |
| plugin-db reads it? | yes (runtime) | **no** (introspects live + sentinels) |
| Types | from the object | **inferred from the object** (Drizzle-style) |

Dropping `default.schema` killed the *runtime auto-migrate*, not the *typed
authoring surface*.

## 3. The `generate` flow

```
schema.ts  (t.string / t.encrypted / t.vector / t.mask / t.ref(onDelete) / t.fts)
   │  zeroship-migrate generate   — diff schema.ts (desired) vs migration-state
   ▼
db/migrations/V<ts>__<name>.sql   — versioned migration = SOURCE OF TRUTH (committed/reviewed)
   │  zeroship-migrate (deploy)   — applies (Confined, schema "<app_id>", provisions role/schema)
   ▼
live DB  (+ sentinels: /* zsenc:… */, __zsmask:…, vector(N), geography)
   │
   ├─ types: inferred from schema.ts (Drizzle-style)        → typed env.db.users.find()
   └─ plugin-db: introspect live + sentinels → data access  → AEAD / mask / vector ops
```

- **`generate`** reuses the engine's existing declarative differ (v3 Plan A,
  `crates/zeroship-migrate/src/declarative.rs`) as the diff engine — but as a
  dev-time *generate* step, not a runtime auto-migrator. (The differ's v1 SUBSET
  type coverage is REPLACED by the relocated full impl — §5.)
- **Hand-authored migrations remain a first-class escape hatch** (the engine
  already has the Flyway/dbmate file loader + `submit_migration`) for anything
  the DSL can't express. Drizzle supports both `generate` and manual SQL; so do
  we. `schema.ts` is the primary surface.

## 4. The goodies — preserved, split by layer, sentinels as the contract

The typed authoring (`t.*`) lives in `schema.ts`. The **schema/DDL side** (emit
the column + sentinel) is owned by the **engine** (relocated from plugin-db's
`query.rs`). The **runtime data-plane side** (the actual crypto/masking) STAYS
in plugin-db. The **sentinel** is the contract: the engine writes it in the
migration DDL; plugin-db reads it at runtime.

| Goodie | `schema.ts` | Engine emits (DDL + sentinel) | plugin-db at runtime (reads sentinel) |
| --- | --- | --- | --- |
| Encryption | `t.encrypted({mode,keyId})` | `BYTEA` + inline `/* zsenc:mode:keyId:wraps */` (+ `__zeroship_meta.encrypted_columns` sidecar) | AEAD encrypt/decrypt with the keyId (the `encryption/{wire,aead,keys}.rs` machinery STAYS) |
| Mask | `t.mask({kind,classification})` (auto on `encrypted`) | `<col>_masked` sibling + `COMMENT … __zsmask:kind=…,classification=…` | mask read-pass (`crud/mask_pass.rs` STAYS) |
| Vector | `t.vector(dims,{metric})` | `vector(N)` + ivfflat index (opclass by metric, lists=100) | vector ops |
| GeoPoint | `t.geoPoint()` | `geography(POINT,4326)` + GiST index (PostGIS probe) | spatial ops |
| FK policy | `t.ref(tbl,{onDelete,onUpdate,deferrable})` | full FK clause `ON DELETE/UPDATE … [DEFERRABLE]` | (referential integrity in DB) |
| FTS | `t.fts()` | `__fts tsvector` col + GIN + `tsvector_update_trigger` | FTS query ops |
| CHECK/enum/min/max/literal | `t.literal/.enum/.min/.max` | `CHECK (…)` constraints + `DEFAULT` | (enforced in DB) |

Cross-app FK is rejected at `generate` (mirror `crates/plugin-db/src/cross_app_fk.rs`).

## 5. Reuse via a shared `zeroship-schema` core (corrected 2026-06-19)

> **Correction.** An inline check found `query.rs`/`diff.rs` are NOT pure
> lift-and-shift: their *bodies* reach into the data plane via fully-qualified
> paths — `diff.rs` → `crate::crud::mask_backfill::{parse/build_mask_sentinel,
> run_mask_backfill}`, `crate::crud::mask_policy`, `crate::backend::{EncryptionMode}`,
> the `MaskMeta`/`EncryptionMeta` types; `query.rs` → `crate::crud::{mask_backfill,
> encryption_pass,system_fields_pass}`, `crate::backend::VectorMetric`. And both
> are **dual-dialect** (`SqlDialect::{Postgres,Sqlite}` woven throughout). So the
> "move two files into the engine" framing is wrong.

The correct structure is a **shared leaf crate `zeroship-schema`** that BOTH the
engine and plugin-db depend on — because plugin-db's runtime *also* needs the
sentinel codec + metadata types to read the schema (that entanglement is the
proof). The cut is by *responsibility*, not by file:

- **Into `zeroship-schema` (shared):** descriptor types, the DSL→PG/SQLite **DDL
  builders** (`query.rs`), the **diff classifier** + **introspection** (`diff.rs`
  `read_live_schema`), the **sentinel codec** (`build/parse_mask_sentinel`, the
  `zsenc` reader — relocated out of `crud::mask_backfill`), the **metadata types**
  (`MaskMeta`/`EncryptionMeta`) and **enums** (`VectorMetric`/`EncryptionMode`),
  system-field definitions. Deps: `serde_json` + `compio-postgres` + `zeroship-core`
  ONLY — **no v8/crypto/runtime** (trust-domain preserved). Dual-dialect (keeps
  SQLite, so dev-tier survives).
- **STAY in plugin-db (data plane, on top of the shared core):** the **transforms**
  — `encryption/{wire,aead,keys}.rs` (AEAD encrypt/decrypt), `crud/mask_pass.rs`
  (mask transform), the backfill *runner*, CRUD/txn/`SET LOCAL ROLE`/metering.
  plugin-db reads schema at runtime via the **shared** introspection + sentinel
  codec (no duplication, no data-plane→engine dependency).
- **In the engine (lifecycle, on top of the shared core):** versioned journal,
  guard, executor (txn/two-phase), rollback, backfill, expand-contract, baseline,
  squash, manifest, the CLI, and `generate` (diff via the shared classifier).
- **DELETE:** `registerModel` runtime apply, `default.schema`, the engine's v1
  SUBSET `declarative.rs` (replaced by the shared full impl).

Net: ONE schema implementation (`zeroship-schema`), reused by the migration
engine (write/diff/generate) and the data plane (read/introspect) — no rewrite,
no duplication, trust-domain intact, SQLite preserved.

| Module / responsibility | Disposition |
| --- | --- |
| `plugin-db/src/query.rs` (DDL builders, all types + sentinels) | **MOVE → engine** |
| `plugin-db/src/diff.rs` (classifier + `read_live_schema` introspection) | **MOVE → engine** (the `generate` differ + deploy introspection) |
| `plugin-db/src/register_model/*` (bootstrap/plan/validate/apply) | **MOVE → engine** (becomes `generate` + deploy-apply) |
| audit journal + role/schema provisioning (`auth/bootstrap.rs ensure_per_app_role`, `CREATE SCHEMA`) | **MOVE → engine** (provisioned at deploy) |
| `zeroship-migrate/src/declarative.rs` (v1 SUBSET differ) | **REPLACE** with the relocated full impl |
| `plugin-db/src/encryption/{wire,aead,keys}.rs` (AEAD/key mgmt) | **STAY** (data plane) |
| `plugin-db/src/crud/mask_pass.rs` (mask transform) | **STAY** (data plane) |
| CRUD / transactions / `SET LOCAL ROLE` / metering | **STAY** (data plane) |
| a **lean** read-only introspection for data-access metadata | **plugin-db keeps a small reader** (types + the zsenc/zsmask/vector sentinels) |
| `registerModel` runtime call (`runtime/core/plugin.rs`, `worker/cache.rs` boot) | **DELETE** |
| `export default { schema }` (`default.schema`) in the deploy contract | **DELETE** |

The descriptor types + the DSL→PG type map become engine-owned; plugin-db's lean
runtime reader needs only the *sentinel + catalog* parsing for data-access
decisions, not the full DDL map.

## 5.1 Pluggable schema front-ends — JS as a first-class input (like HCL to Atlas)

Atlas's real shape is **many schema front-ends (HCL / SQL / ORM providers) → one
internal representation → one diff/migrate engine.** `zeroship-migrate` adopts the
same shape, with the **descriptor IR** (`zeroship-schema`'s descriptor types) as
the internal representation. Two front-ends:

- **SQL migration files** (Flyway/dbmate) — the **lean core**. Pure Rust, no V8.
  This is the public dbmate-like CLI + the apply/rollback/status path.
- **JS/TS `schema.ts`** — a **first-class, but optional, front-end** in a separate
  crate **`zeroship-migrate-js`** that embeds a JS engine to evaluate the `t.*`
  DSL → descriptor IR → the core engine's diff/generate. This is the zeroship
  creator superpower (the tool natively *speaks* the app's schema, the way Atlas
  speaks HCL), the analog of Atlas's ORM/HCL providers.

**Guardrail — keep the core lean.** Evaluating `schema.ts` needs a real JS+TS
engine, which is heavy. It must NOT be welded into the base tool, or the public
CLI loses its clean-standalone-binary property. So:

- `zeroship-migrate` (core) — SQL front-end + descriptor-IR diff/migrate. **No V8.**
- `zeroship-migrate-js` (front-end crate / cargo feature) — **reuses
  `zeroship-runtime`'s existing V8 + TS toolchain** (no second JS engine), evals
  `schema.ts` **in the existing V8 sandbox** (untrusted creator schema runs under
  the same security model as app code), emits the descriptor IR. Only the
  full/platform build carries it; the lean public build does not.

So `zeroship-migrate generate --schema schema.ts` is the platform/full build
(carries the JS front-end, one self-contained tool — no separate Node/vite step);
`zeroship-migrate migrate ./db/migrations` is the lean build (no JS). Both feed
the same descriptor IR + diff/migrate. **No HCL** is needed: JS creators use
`schema.ts`; everyone else uses SQL migrations; a future declarative format would
just be another front-end emitting the same IR.

## 6. plugin-db runtime data access without a declared schema

For each collection, plugin-db builds its column metadata by **introspecting the
live catalog** (`pg_attribute`/`format_type` for types) **+ parsing the
sentinels** (`pg_description` for `__zsmask:` and the `/* zsenc:… */` inline +
the `encrypted_columns` sidecar). From that it knows: column types (bind/codec),
which columns are encrypted (key + mode), which are masked (kind/classification),
which are vector. This is the *same* read the engine's introspection does — one
source of truth for both the type generator (§7) and the data plane (kept in
lockstep).

- **Caching/invalidation:** cache per (app, schema-version) on the worker
  thread; invalidate on a deploy bump (a schema-version token bumped by the
  engine at deploy and surfaced to the worker, mirroring today's per-thread
  `is_model_registered` fast path but keyed on the deploy/schema version).
- **Failure mode if a column is missing:** normal CRUD error (column does not
  exist). Acceptable because **deploy ordering guarantees the schema is applied
  before the app serves** (§8). There is no separate verify gate to mis-fire.

## 7. The type story (Drizzle-style)

Types are **inferred from `schema.ts`** (Drizzle-style) — `@zeroship/db` infers
the row/insert types from the `t.*` definition, so `env.db.users.find()` stays
strongly typed with no separate codegen step. The encrypted/masked/vector
facets reflect in the inferred types (e.g. an `encrypted` field is typed as its
logical type; a `vector` field gets the vector type). `zeroship-migrate dump`
(schema.sql) remains available for tooling/inspection, but the *type* path is
schema.ts-inference. (Drizzle-style drift between `schema.ts` and the migrations
is prevented by `generate` keeping them in sync + a `generate --check` in CI.)

## 8. Control deploy integration + ordering

The control-plane deploy step (`crates/control/src/api.rs` deploy handler
~394–620) gains a migrate phase **after ingest, before the go-live commit**
(`set_deploy_with_manifest` ~605):

1. ingest the `.zship` (which **ships the migration files**).
2. engine: provision the per-app role + schema (if absent) → apply the bundle's
   **pending** migrations under Confined (schema `"<app_id>"`).
3. only on success → the go-live commit (routes/manifest). A failed migrate ⇒
   **no go-live** (the old bundle keeps serving its already-migrated schema).
4. **migrate-then-go-live-fails half-state:** the migration is committed
   (additive-forward), routes unchanged → old code on a forward-compatible
   schema (safe for additive; destructive uses expand-contract across deploys).
   The next deploy's roll-forward reconciles. Document the contract; no verify.

Migrations come from the **bundle** (generated/committed by the creator), plus
the `submit_migration` endpoint for out-of-band/destructive ops. Auth:
`AppsDeploy` on `Resource::App{id}`; the app identity is the trusted path id,
never a request body.

## 9. Dev experience

No more auto-migrate-on-boot. Dev parity:
- `zeroship-migrate generate` (from `schema.ts`) + `zeroship-migrate migrate`
  (apply) in the dev loop — the CLI already exists (Track A).
- The dev-tier DB (SQLite, see `docs/reference/auth-dev-tier.md` peer) gets its
  schema the same way: run the engine against the dev DB. **Non-goal for now:**
  full SQLite-backend parity in the engine — dev schema management on SQLite is
  a follow-up; document the gap (today plugin-db's diff handles SQLite; the
  relocated engine must keep the SQLite arm or dev falls back).

## 10. Non-goals

- The full **project-umbrella** multi-app-shared-DB model (prj_ ids, union
  schema, cross-app ownership ledger) — app-scoped (`project_schema :=
  "<app_id>"`, app owns its whole schema) for now.
- **SQLite-backend parity** in the relocated engine — flagged as a dev-tier
  follow-up (§9); the engine's PG arm is the target.
- A generic async job queue — destructive/long migrations via the existing
  `submit_migration` + (future) the migration-jobs surface.

## 11. Risks

- **R1 — relocation breaks plugin-db's large test suite.** Mitigate: move the
  schema/diff tests *with* the code; keep behavior byte-identical; run both
  suites green at each phase.
- **R2 — `schema.ts` ↔ migrations drift** (Drizzle's classic footgun).
  Mitigate: `generate` is the only sanctioned way to change schema; a
  `generate --check` in CI fails if `schema.ts` and the migrations disagree.
- **R3 — type-gen vs runtime introspection divergence.** Mitigate: both read
  the *same* introspection/sentinel logic (relocated, shared semantics); a
  round-trip test (schema.ts → migrate → introspect → re-infer == schema.ts).
- **R4 — SQLite dev regression** (registerModel was the dev migrator).
  Mitigate: keep the engine's SQLite arm (relocated) or document the dev path.
- **R5 — deploy half-state** (§8.4) — additive-forward is safe; destructive is
  expand-contract; documented ordering + roll-forward.

## 12. Phased plan (each phase shippable; platform stays bootable)

- **P1 — Extract the shared `zeroship-schema` core crate.** Untangle plugin-db's
  schema layer (DDL builders + diff classifier + introspection + sentinel codec +
  metadata types + dialect enums) out of `query.rs`/`diff.rs`/`crud::mask_backfill`/
  `backend` into the leaf crate (deps: serde_json + compio-postgres + zeroship-core;
  dual-dialect PG+SQLite). **plugin-db depends on it and is behavior-IDENTICAL**
  (its CRUD/transform code now calls the shared core for DDL/diff/introspection) —
  no user-visible change; plugin-db's full suite stays green. Critic + round-trip
  fidelity (the verify-bricking guard). This is a pure refactor.
- **P2 — Engine adopts the shared core.** `zeroship-migrate` depends on
  `zeroship-schema`; its v1 SUBSET `declarative.rs` is REPLACED by the shared full
  differ → the engine reaches full capability (vector/encrypted/mask/geo/FK/FTS).
  `generate` (descriptor IR diff → versioned migration) over the SQL front-end.
- **P3 — `zeroship-migrate-js` front-end crate.** V8-backed (reuse
  `zeroship-runtime`) eval of `schema.ts` → descriptor IR; `zeroship-migrate
  generate --schema schema.ts`. `@zeroship/db` infers types from `schema.ts`
  (Drizzle-style). Core stays V8-free; JS front-end is opt-in.
- **P4 — plugin-db runtime reads via the shared core.** plugin-db's data-access
  metadata comes from the shared introspection + sentinel codec (no declared
  schema); AEAD/mask transforms driven by it. Still boots via registerModel
  (parallel-safe) until P5.
- **P5 — Cut over: gut `registerModel` + remove `default.schema`.** Delete the
  runtime auto-migrate + the declared-schema deploy-contract field; the runtime
  no longer migrates. (P1–P4 made it unnecessary.)
- **P6 — Wire the engine into control deploy** (§8): ship migrations in the
  `.zship`, provision role/schema + apply at deploy before go-live. **LANDED**
  (REORDERED before P5 — gutting `registerModel` is only safe once deploy
  creates the schema). `.zship` carries `manifest.migrations[]`
  (content-addressed blobs); the control deploy handler reconstructs them and
  applies via `deploy_migrate::apply_bundle_migrations` (Confined, schema
  `"<app_id>"`, the `migrator_<app_id>` role) AFTER ingest + BEFORE
  `set_deploy_with_manifest`; a migrate failure ⇒ no go-live. Shadow dry-run
  skipped on the deploy apply for v1 (engine guard + role are the in-line
  safety). Fixed a latent engine bug: `ensure_journal` interpolated the
  immutability-trigger NAME unquoted, breaking on the hyphenated-UUID per-app
  schema. **P6b (follow-on):** the build-side `generate` of
  `manifest.migrations[]` from `schema.ts` lives in the vite-plugin (creator
  DX); P6 verifies with hand-authored migration bundles.
- **P7 — e2e:** author `schema.ts` → `generate` → deploy applies → runtime data
  access is typed and encryption/mask/vector work end-to-end on real PG.

Each phase: build → adversarial critic → PG verify; commit-only on
`feat/db-migration-engine`, never pushed.
