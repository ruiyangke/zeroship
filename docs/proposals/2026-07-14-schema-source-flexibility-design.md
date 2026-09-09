# Schema Source Flexibility — decouple schema *sourcing* from schema *applying*

> **Historical design snapshot.** Package paths and package boundaries below
> describe the 2026-07-14 tree. The live DSL is the single
> `@zeroship/migrate` package at `packages/zero-migrate/`.

- **Status:** proposal (pre-implementation)
- **Date:** 2026-07-14
- **Depends on:** Phase F (monorepo consumes the published `zero-migrate` engine — the in-tree engine was deleted in Phase F stage 5, commit `79a1f45e`).
- **Changes runtime behavior of:** the schema *source* feeding the `RuntimeSchemaDescriptor` (adds manual + introspection front-ends alongside the migration-fold front-end) — **not** `db.registerModel(...)`, which on PG is already a DDL no-op (see §1).

---

## 1. Problem

> **Framing correction (verified 2026-07-14).** An earlier draft claimed app deploy "touches the database" and that `registerModel` "converges DDL at boot." Both are already false in the shipped code. The deploy-side coupling this design once set out to sever is **already severed**. The real problem is narrower and is stated below.

Two facts about the *current* code bound the problem:

- **Deploy already refuses to apply migrations.** `crates/zeroship-control/src/api.rs:416` returns `migration_approval_removed` — *"deploy no longer applies migrations; use the migration service `/v1/apps/{id}/migrations/apply`."* The out-of-band migration-service path (`zeroship-platform-migrate`) is **shipped** (Phase F), not future.
- **`registerModel` on PG is already a DDL no-op.** `crates/zeroship-plugin-db/src/register_model/mod.rs:210` is `(Some(_pg), _) => Ok(())`. The four-phase convergence modules (`bootstrap` / `plan` / `validate`) are `#[cfg(any(test, feature = "test-helpers"))]`-gated (`mod.rs:56-63`) — they do **not** compile into the production worker. On PG, `registerModel` still *runs* at boot (via `installSchema` off `globalThis.__zsRuntimeDescriptor`, the fold's `RuntimeSchemaDescriptor`), but only to **populate the metadata cache** (idPrefix, encrypted/mask facets — `mod.rs:198-218`). No CREATE, no ALTER.

So the genuine, still-open problem is **schema sourcing**, in three parts:

1. **Typing today *requires* authoring `op.*` migrations.** To get a typed `env.db.users.find(...)`, an app must author `op.*` migrations that `gen-types` folds into `env.db.ts` + `schema.runtime.json`. There is no *manual* and no *brownfield* schema source. A creator with an existing database — Liquibase-managed, Prisma-managed, or raw SQL — cannot get a typed `env.db` without adopting our migration engine wholesale. Typing should not require handing schema *ownership* to the platform.

2. **The build shells a CLI whose home is stale/unlocated.** `sdks/vite-plugin/src/migrations.ts` spawns a retired JS authoring binary (`recordViaCli`, `genTypesViaCli`, `resolveGenTypesCli`). Phase F stage 5 (commit `79a1f45e`) deleted the in-tree engine, including the `zeroship_migrate::frontend::gen_types::RUNTIME_DESCRIPTOR_FILE` symbol that `migrations.ts:163` still references. The gen-types **emitter no longer has a home** (see §5, §7). Worse, the CLI-absent arm (`migrations.ts:254-262`) silently returns `status: "skipped"` and uses the committed `env.db.ts` "as-is" — a drift vector where the checked-in types can diverge from what a fresh regen would produce.

3. **The dev-tier DDL converger consumes the *declared* schema, on a path parallel to the fold's snapshot.** The SQLite dev tier is the only remaining real DDL converger. It does **not** consume `schema.runtime.json`; it converges from the per-call **declared** `(collection, schema, indexes)` (see §7). That second source is unreconciled with the fold's `RuntimeSchemaDescriptor` — the same schema flows through two different pipelines depending on backend.

The root cause is a conflated concern: **where the schema comes from** (sourcing) is welded to **how the schema is put into a database** (applying). They should be independent. The deploy/apply seam is already clean; the *source* seam is not.

---

## 2. Principle

> **Schema *sourcing* is decoupled from schema *applying*. Typing is source-agnostic.**

- **Sourcing** — where does the schema snapshot come from? Two first-class answers:
  - **Generated** — `zero-migrate` folds `op.*` migrations into a snapshot.
  - **Manual** — the creator declares the schema directly (a `schema(...)` literal from `@zeroship/db`).

  *Introspection is not a third source — it is a bootstrap helper for the manual source: read an existing DB once, emit the initial declaration, thereafter it is just a declared schema.*

- **Applying** — putting DDL into a real database — is **optional, explicit, and never part of app deploy**. This is *already true* for PG (deploy is a no-op; DDL is the migration service). The generated source's DDL lands via out-of-band migrations. The manual source's database already exists. Managed sync, where wanted, is a separate gated command. The one place DDL still happens automatically is the SQLite dev tier — and §7 decides whether that path changes.

- **Typing** — is produced by exactly one engine, `@zeroship/db`'s generic `Db<typeof schema>`, and it does not know or care which source produced the `schema` literal.

---

## 3. The universal artifact: the schema snapshot

Every source converges on one in-memory value — a **schema snapshot** (the existing `CollectionDescriptor` / `FieldDescriptor` / `IndexDescriptor` + per-collection options: `softDelete`, `versioning`, `strictness`, masking/encryption, naming). From that single snapshot, **one emitter, in one pass**, produces two projections:

```
                       ┌──────────────────────────┐
   source ──────────►  │      schema snapshot      │  (CollectionDescriptor…)
                       └──────────────┬───────────┘
                                      │  one emitter, one pass
                         ┌────────────┴────────────┐
                         ▼                          ▼
                  env.db.ts                 schema.runtime.json
             (compile-time types)          (runtime descriptor)
             Db<typeof schema>             worker installs Collection
             erased after build            wrappers: validation, coercion,
                                           name-mapping, softDelete/versioning,
                                           masking, defaults, strictness
```

**Invariant (co-emission):** `env.db.ts` and `schema.runtime.json` are always emitted together, from the same snapshot, in the same pass. They are two projections of one source of truth, so they cannot disagree. A human never hand-writes either file; both carry the `GENERATED … DO NOT EDIT` banner. This invariant is the whole reason the design is safe — if a human could edit `schema.runtime.json` independently, the types would start lying. See §5 for how CI *enforces* co-emission (`gen-types --check`) rather than merely asserting it.

- **`env.db.ts`** makes the *author* safe. Compile-time only; erased at build. It does `import { t, schema as defineSchema, type Db } from "@zeroship/db"` — the base export is `schema`; `defineSchema` is a generated-output alias (`examples/db-hitcounter/generated/zeroship/env.db.ts:12`). Prose below uses `schema`.
- **`schema.runtime.json`** makes the *running app* correct. Shipped in the `.zship`; the worker's actual source of truth. TypeScript does not exist at runtime, so the runtime needs a machine-readable descriptor to do field validation, value coercion, field↔column name mapping, soft-delete/versioning semantics, masking/encryption, server-stamped defaults, and strictness enforcement. This is what `installSchema` reads to build `Collection` wrappers, and what `registerModel` reads (on PG) to populate its metadata cache.

---

## 4. Sources (front-ends)

All three feed the **same emitter core**; they differ only in how they build the snapshot.

### 4.1 Generated (`zero-migrate`)
`gen-types` records the `op.*` migrations, folds their IR into a snapshot (the engine's `render::fold` / `schema::replay`, now in the standalone `zero-migrate` repo), emits both files. This is today's path — unchanged in *semantics*, but its emitter must be rebuilt (see §5/§7): it no longer lives in-tree.

### 4.2 Manual (declared)
The creator writes a `schema(...)` literal in a committed **`schema.ts` at app root** — the same `@zeroship/db` calls that appear in the generated `env.db.ts`, but authored by hand. This file **is the source**. The toolchain evaluates it (it is just `@zeroship/db` builder calls producing a descriptor in memory) → snapshot → emits `schema.runtime.json` and an `env.db.ts` that reduces to the module augmentation over the author's `schema.ts` (see §11.3). The author owns drift, exactly as with any hand-written type.

### 4.3 Introspection (bootstrap helper for the manual source)
`gen-types --from-db <dsn>` connects to an existing database, reads `information_schema` (the capability already in `crates/zeroship-data-orm/src/crud/introspect_schema.rs`), maps columns → `t.*()` fields / PK+nullable → `.required()` / indexes → `.index(...)`, and **writes the initial declaration**. After that, it is an ordinary manual source. This is how a Liquibase/Prisma/raw-SQL app gets typed `env.db` without adopting our migration engine. Introspection is **lossy** — see §9 Phase 2 for the explicit fidelity gaps (idPrefix, JSON-family logical types, SQLite introspector status).

---

## 5. The emitter: a Rust engine verb (reusing the surviving logical fold), linked in-process (no CLI subprocess)

> **The hard part survived Phase F; only the thin wrapper + its napi exposure were deleted.** The standalone `/home/ruiyang/Projects/zero-migrate` repo **already carries the logical recovery seam**: `fold_to_field_defs` (ops → per-collection logical wire-`FieldDef` map — the *logical* view, not the physical `SchemaSnapshot`), `descriptors_to_create_ops` (a declared descriptor set → ops), `descriptor_to_sdk_schema`, and a passing `gen_types_mask_roundtrip` test (`crates/zeroship-migrate/tests/`). All exported from `crates/zeroship-migrate/src/lib.rs:199-200`. What was deleted with the in-tree engine (Phase F stage 5) is only the **emitter wrapper** — `frontend/gen_types.rs`, which wrapped `fold_to_field_defs` output into the v1 `RuntimeSchemaDescriptor` (`schema.runtime.json`) and templated `env.db.ts` (a `const schema = { … t.string() … } as const` of `@zeroship/db` builder calls + the `declare module "zeroship"` augmentation), plus its `--check` diff gate. That deleted file is recoverable from git history (`git show 79a1f45e~1:crates/zeroship-migrate/src/frontend/gen_types.rs`) and is the port reference.

**Why the emitter is Rust, not JS.** An earlier draft (and a first trace) proposed napi return the physical `SchemaSnapshot` and have a JS emitter invert it (physical `data_type` → logical type, `nullable` → `required`, re-derive `encrypted`/`mask`/`idPrefix` from sentinels). That is a **lossy physical→logical inversion re-implemented in JS** — exactly the second-implementation-of-the-schema hazard the `Checksum::of_ir` discipline exists to prevent. It is unnecessary: `fold_to_field_defs` already produces the *logical* view directly from the ops (the ops are logical), and the runtime `RuntimeSchemaDescriptor` (§3) already carries the full `FieldDef` vocabulary (`ref`/`enum`/nested/facets). So the emitter stays in Rust, consumes the logical fold, and there is no inversion.

**The napi verb.** Phase 1 adds one entrypoint to `zeroship-migrate-node`, returning the two artifact *strings* Rust already knows how to render:

```
genArtifacts(source) → { envDbTs: string, runtimeJson: string }
```

`zeroship-migrate-node` today exports only the apply-side verbs (`applyIr`, `status`, `history`, `loadVerify`, `irVersion`); this is the one addition. Both sources funnel through it, so generated and manual output are **byte-identical by construction** (one renderer, not two):

| Source | JS front-end produces | Rust verb path |
| --- | --- | --- |
| `op.*` migrations | recorder → IR envelopes (ops) | ops → `fold_to_field_defs` → descriptor + `env.db.ts` |
| declared `schema.ts` | evaluate `@zeroship/db` → `CollectionDescriptor`s | `descriptors_to_create_ops` → ops → same tail |
| `--from-db <dsn>` (Phase 2) | introspect → declaration once | then the declared path |

**The two in-process libraries** the vite plugin links (no subprocess, no `zeroship-runtime` authoring vector):

- **`zero-migrate` (pure-JS DSL package)** — the recorder (evaluate a `.ts` migration → IR envelope) and the manual evaluator (`schema.ts` → `@zeroship/db` descriptors). Plain JS builder calls in the plugin's own Node engine.
- **`zeroship-migrate-node` (napi addon)** — the `genArtifacts` verb above.

**The orchestrator is a thin internal module of the vite plugin** (`sdks/vite-plugin/src/gen-types/`), *not* a separate package: it detects the source, calls the JS front-end, invokes `genArtifacts`, and writes the two files. The rendering — the correctness-critical part — is the Rust verb. (An earlier draft proposed a standalone `@zeroship/schema-emit` package; with the vite plugin as the sole consumer, that was premature — a single-consumer package. Kept as a cohesive directory so a future second host can extract it in one move; pre-launch, extraction is free.)

**Deleting the CLI seam.** The current subprocess machinery in `sdks/vite-plugin/src/migrations.ts` — `recordViaCli`, `genTypesViaCli`, `resolveGenTypesCli`, the retired JS authoring binary's PATH lookup, and the "CLI absent → `status: skipped` + committed `env.db.ts` used as-is" fallback (`migrations.ts:254-262`) — is **deleted**. That binary no longer exists post-Phase-F (the authoring path is currently *broken*, not merely indirect), so this is a repair. Linking the addon makes "binary absent" a hard install-time dependency error, not a silent build-time skip.

**CI enforcement of co-emission.** The co-emission *invariant* (§3) is not self-enforcing — a stale committed `env.db.ts` could ship if nobody regenerates. The mechanism is **`--check`** (already in the recovered `gen_types.rs`: regenerate in memory, diff against the committed artifacts, no DB write). The old `--check` was undermined by the `skipped`-when-CLI-absent arm (`migrations.ts:254-262`); linking the addon makes it a **hard gate** — no binary to be absent, so drift is always caught.

---

## 6. What changes at the runtime seam (and what stays)

Per `AGENTS.md` (no published users, no migration shims). **This is the section the earlier draft got most wrong** — it claimed "delete `registerModel` wholesale" and "app deploy no longer touches the database." Corrected:

- **On PG, there is no DDL coupling to remove — it is already nil.** `registerModel` on PG is already `Ok(())` (`mod.rs:210`). App deploy already applies no DDL (`api.rs:416`). Nothing to delete here.
- **`registerModel` on PG STAYS** as the **metadata-cache installer**. It reads the `RuntimeSchemaDescriptor` (idPrefix, encrypted/mask facets) so PG CRUD can honor them (`mod.rs:198-218`). This is orthogonal to DDL and is exactly what the design keeps.
- **`installSchema` STAYS** — the typed-wrapper install that reads `schema.runtime.json` and installs `Collection` wrappers. Untouched.
- **The four-phase convergence modules** (`bootstrap` / `plan` / `validate`) are already `#[cfg(any(test, feature = "test-helpers"))]`-gated dead-in-prod code (`mod.rs:56-63`). They can be deleted as a housekeeping item, but deleting them changes *no* production behavior — they never compiled into the worker.

**What this design actually changes at the seam:**

1. **Unify the schema SOURCE** feeding the `RuntimeSchemaDescriptor`: add the manual and introspection front-ends alongside today's fold-only front-end (§4, §5).
2. **Reconcile the SQLite dev-tier converger** with that unified snapshot — *or* explicitly decide it keeps its current declared-schema converger and unification lives only at the source (snapshot) layer. This is §7's decision, and it is a real re-architecture question, not a repoint.

There is **no** "delete `registerModel` wholesale." The runtime consumer of the snapshot is load-bearing and stays.

---

## 7. The dev-tier DDL converger — the one place DDL is automatic

The SQLite dev tier (`zeroship serve`) is the only remaining real DDL converger, and it is **not** a thin repoint. It is a security-hardened, incremental, two-connection actor. Any change here must preserve (or explicitly, deliberately replace) three load-bearing invariants.

### 7.1 What it is today
`register_model/mod.rs:219-229` routes the SQLite arm to `sqlite_engine::run_sqlite_via_engine(...)`, passing the per-call **declared** `(collection, schema, indexes)` plus `declared_collections`. It does **not** read `schema.runtime.json`. It builds `desired_snapshot` from the declared descriptors and drives the engine's `plan_declarative` (Sqlite) — **not** `zeroship_schema::diff`. It applies through `zero_migrate::apply::backend::sqlite::SqliteBackend` (backend **B**), a **different type** from plugin-db's data-plane `crate::backend::sqlite::SqliteBackend` (backend **A**).

### 7.2 The three invariants any change must respect

1. **Two-connection security invariant + ordering barrier** (`sqlite_engine.rs:16-47`). DDL MUST run on the hardened migration backend **B** (authorizer line-2 deny-list, journal immutability, ATTACH isolation), never on the CDC-armed, intentionally-un-hardened data-plane backend **A**. The sequence is a single-owner window: open B → ensure journal + baseline (H3 adoption) → `plan_declarative` + apply (B owns the file) → **drop B** → A `ensure_app_schema` re-ATTACHes the file *after B is gone* → bridge A's CDC name-cache. All six steps run **inside one awaited body** — the ordering barrier: the `installSchema` `ready` promise resolves only after this returns `Ok(())`, so B-done + A-re-ATTACH + CDC-bridge are all ordered-before the first `env.db.<coll>.find()`. Steps 4–6 must never be spawned/detached.

2. **Incremental one-collection-at-a-time union reconciliation ("H1")** (`sqlite_engine.rs:104-160`). A fresh isolate registers collections one at a time (`install-schema.ts`). The desired set for *this* register is the current collection PLUS every sibling already registered on this isolate (`build_union_descriptors` + `other_schemas`). The union is **necessarily partial** on a warm multi-collection file: when the first collection registers, the sibling cache is empty, so a live sibling table (present from a prior isolate) is absent from `desired`. Without the H1 handling, the differ's fail-closed drop pass raises `DropOfUnownedTable` and the app breaks on every warm boot of any 2+-collection schema. This partial-union logic prevents phantom sibling DROPs.

3. **Dev auto-approve, structurally-safe-in-prod** (`sqlite_engine.rs:49-56`). A rebuild on a populated table is destructive; the engine refuses it without `Approval::Approved`. Dev passes `Approved` (the operator owns the local file); prod can never reach this arm because the worker hard-aborts on a SQLite DSN (P6b-1).

### 7.3 The decision

Two options; the design **recommends Option A** and states Option B for completeness:

- **Option A (recommended) — keep the dev-tier converger as-is; unify only at the SOURCE layer.** The unification this design delivers is at the *snapshot source* (§4/§5): manual/introspection front-ends produce the same snapshot the fold does. The dev-tier keeps consuming the per-call declared schema through `run_sqlite_via_engine`, retaining all three §7.2 invariants untouched. Rationale: the declared schema the SQLite arm receives *already* originates from the bundled `RuntimeSchemaDescriptor` on the runtime path (`mod.rs:198-208` notes the descriptor is installed via `globalThis.__zsRuntimeDescriptor` and `installSchema` runs `registerModel` off it) — so the dev converger is *already* fed from the same fold snapshot for the generated source. For the manual source, the snapshot the emitter produces becomes the same `RuntimeSchemaDescriptor`, which flows to the same arm. No re-architecture of the security window needed.

- **Option B (not recommended now) — snapshot-driven materializer.** Replace `run_sqlite_via_engine`'s declared-per-call input with a whole-`schema.runtime.json` snapshot materializer. This is a genuine re-architecture: it must **preserve or explicitly replace** (a) the two-connection ordering barrier — a whole-snapshot materializer that runs outside the per-register awaited body loses the barrier and must re-establish it; and (b) the H1 partial-union logic — a whole-snapshot pass sees all collections at once and so *sidesteps* the one-at-a-time phantom-DROP problem, but only if the materializer runs once per boot rather than per-collection, which changes the `installSchema` call shape. The security invariant and the CDC-bridge step remain mandatory. Not worth it now; revisit only if the per-call path becomes a maintenance burden.

Under Option A, §9 has no Phase 3 dev-tier work; the dev tier is already unified by construction.

---

## 8. Applying schema to a real database (the optional, already-gated leg)

DDL against a *real* (non-dev) database is already out of the deploy path. This section restates the existing contract; the design adds only the manual/sync legs.

- **Production, generated source:** DDL lands via out-of-band `zero-migrate` migrations — the `zeroship-platform-migrate` binary / migration-service path proven in Phase F. Operator-run, gated, audited. Deploy applies nothing (`api.rs:416`). **This is shipped, not proposed.**
- **Production, manual/brownfield source:** the database already exists and is owned by the creator's external tool. The platform applies **nothing**.
- **Optional managed sync (opt-in):** a creator who wants the platform to converge a real DB from a declared snapshot runs an explicit, gated `zero-migrate sync --from-snapshot`, which reuses `schema::diff` (desired snapshot vs live introspection → DDL). Never automatic, never at deploy. Its idempotency/version-identity story is a genuinely-open item — see §9 Phase 2 and §11.
- **Migration-service deploy-ordering contract.** `register_model/mod.rs` (the PG-no-op comment, `mod.rs:192-208`) relies on the invariant that **the migration service applies schema *before* the deploy that depends on it goes live** — the PG arm no-ops DDL precisely because "deploy ordering guarantees the schema is present first." This design does not change that contract; it inherits it. Any manual/sync source must satisfy the same ordering (schema present before dependent code serves traffic).

---

## 9. Implementation phases

**Phase 1 — Rust emitter verb + in-process library rewire + manual source.** *No worker-runtime changes.*
- Standalone repo (`/home/ruiyang/Projects/zero-migrate`): **port the deleted emitter wrapper** into the engine (recover `frontend/gen_types.rs` from appbase history at `79a1f45e~1`, adapt its `render_artifacts` + `--check` to the standalone's already-present `fold_to_field_defs` / `descriptors_to_create_ops` — `lib.rs:199-200`). Expose one napi verb on `zeroship-migrate-node`: `genArtifacts(source) → { envDbTs, runtimeJson }`, accepting either IR envelopes (generated) or `CollectionDescriptor`s (manual, via `descriptors_to_create_ops`). Publish. Reuse the existing `gen_types_mask_roundtrip` test; add a descriptors→artifacts test.
- **Author** the orchestrator as an internal vite-plugin module `sdks/vite-plugin/src/gen-types/` (source-detect → JS front-end → `genArtifacts` → write files + `--check`). It does **not** contain the emitter logic — that is the Rust verb.
- Rewire `sdks/vite-plugin/src/migrations.ts` to call the `src/gen-types/` module in-process, linking `zero-migrate` (recorder + `schema.ts` evaluator) + `zeroship-migrate-node` (`genArtifacts`) as file deps on the vite-plugin; delete `recordViaCli` / `genTypesViaCli` / `resolveGenTypesCli` / the `status: "skipped"` warn-when-absent fallback (`migrations.ts:254-262`); repair the dangling `RUNTIME_DESCRIPTOR_FILE` comment (`migrations.ts:161-166`).
- Manual front-end: evaluate `schema.ts` (`@zeroship/db`) → `CollectionDescriptor`s → `genArtifacts`. `env.db.ts` for the manual case reduces to the module augmentation over the author's `schema.ts` (§11.3).
- Make `--check` a hard CI gate (no CLI to be absent; drift always caught).
- Proof: (a) a manually-declared `schema.ts` and an equivalent `op.*` migration set produce **byte-identical `schema.runtime.json`** (one Rust renderer); (b) the `env.db.ts` type-checks and its `Db<typeof schema>` resolves; (c) a golden app builds from a hand-written `schema.ts` with zero migrations, no subprocess spawned; (d) `--check` fails on injected drift.

**Phase 2 — Introspection bootstrap + gated sync + drift check.**
- `gen-types --from-db <dsn>` → declaration (reusing `introspect_schema`).
  - **Lossy-fidelity subsection (explicit).** Introspection cannot recover everything the snapshot carries:
    - **idPrefix** (`t.id(prefix)`) is a platform convention with no `information_schema` footprint — it must be defaulted or prompted, and the emitted declaration flags it as author-supplied.
    - **JSON-family logical type** — `information_schema` collapses several logical types (json/jsonb, enum-as-text, vector) into base column types; the introspector emits the base type and documents the widening.
    - **The SQLite introspector is a documented follow-up** per the header of `crates/zeroship-data-orm/src/crud/introspect_schema.rs` — Phase 2 ships the PG introspector; SQLite `--from-db` is deferred.
- `zero-migrate sync --from-snapshot` (gated, opt-in) → managed DDL from a snapshot.
  - **Version-identity / journal story (explicit).** A file-based migration derives a stable version from its filename (Phase F's filename-derived stable versions). A **snapshot has no filename** → `sync --from-snapshot` must define its own version identity: hash the snapshot (content-addressed version), record a synthetic journal entry keyed by that hash, and make re-runs idempotent (a snapshot whose hash matches the last-applied journal entry is a no-op). This must interoperate with the existing `_mig` journal so a later `op.*` migration on the same DB does not conflict with a sync-authored baseline. Design this before shipping `sync`.
- `zero-migrate check --against-db` → drift warning (dev/CI only; never blocks deploy, never mutates).
- Proof: an app pointed at a Liquibase-seeded PG DB gets a typed `env.db` with zero migrations authored; `sync` converges an empty DB to the snapshot and is a no-op on second run; `check` reports an injected drift.

**Phase 3 — (Only if §7 Option B is chosen.)** Dev-tier snapshot-source materializer, preserving the two-connection ordering barrier + H1 union logic per §7.2. **Omitted under the recommended Option A** — the dev tier is already unified at the source layer.

---

## 10. Consistency guarantees

| Pair | Guarantee | Mechanism |
| --- | --- | --- |
| types ↔ runtime | cannot disagree | co-emission from one snapshot (§3), enforced by `gen-types --check` hard gate (§5) |
| runtime ↔ real DB (generated) | faithful | migrations *are* the DB; fold is faithful to the applied ops; deploy applies nothing (`api.rs:416`), migration service applies before dependent deploy (§8) |
| runtime ↔ real DB (manual/brownfield) | author-owned | the snapshot is the author's claim, like any hand-written type; optional `check --against-db` warns on drift |
| runtime ↔ dev SQLite | converged | `run_sqlite_via_engine` from the same snapshot source (§7 Option A), preserving the two-connection security window + H1 union |

---

## 11. Open questions

The earlier draft listed masking/encryption expressiveness and naming strategy as blocking unknowns. **Both are answered** in the shipped `@zeroship/db` surface; they are downgraded to notes. The genuinely-open item is introspection fidelity.

1. **Masking/encryption expressiveness — ANSWERED (residual: introspection fidelity only).** `.mask({kind, classification})` (`sdks/db/src/types.ts:1190`) and `.encrypted({mode, keyId, wraps})` (`types.ts:1608`) are already declarable on `TypeBuilder`, alongside `.required/.unique/.index/.default/.ref`. A hand-declared manual schema can express every runtime field option a generated one carries. The residual gap is **introspection fidelity**: `--from-db` cannot *recover* masking/encryption intent from `information_schema` (it is a platform-side facet, not a column property), so a brownfield bootstrap emits fields *without* those facets and the author adds them. Document this as an introspection limitation, not a DSL gap.

2. **Naming strategy — ANSWERED.** `NamingStrategy` is already provided: `sdks/db/src/types.ts` exports `naming` / `NamingStrategy` with `toColumn` / `toField` / `snakeCase` / `asIs`. Default is snake_case. The manual path uses the same strategy, so the runtime descriptor and any real DB agree.

3. **Where the declared schema file lives — DECIDED: a committed `schema.ts` at app root.** The author edits `schema.ts` (the source of truth); the emitter reads it and generates *both* `generated/zeroship/env.db.ts` and `schema.runtime.json` as `DO NOT EDIT` artifacts. This keeps the generated/source split identical to the migrations path. For the manual source, `env.db.ts` reduces to the module augmentation referencing the author's `schema.ts` (`import { schema } from "../../schema"; declare module "zeroship" { interface Env { db: Db<typeof schema> } }`), while `schema.runtime.json` is derived by evaluating `schema.ts`. (Rejected: editing `generated/zeroship/env.db.ts` directly — it blurs the generated/source boundary the design rests on.)

4. **Introspection lossy-fidelity (the real open item).** idPrefix recovery, JSON-family logical-type widening, and the deferred SQLite introspector (§9 Phase 2). This is where a brownfield bootstrap can silently under-declare; the emitted declaration must flag author-supplied facets so the round-trip is honest.

5. **`sync --from-snapshot` version identity / journal interop (§8, §9 Phase 2).** How a filename-less snapshot gets a stable, idempotent version and coexists with the `_mig` journal used by file-based `op.*` migrations. Design before shipping `sync`.

6. **Dev-tier security-invariant ownership after any change (§7).** If Option B is ever chosen, the two-connection ordering barrier and H1 union logic must be re-established, not silently dropped. Under Option A this is a non-issue (unchanged), but it is called out so a future materializer author cannot miss it.
