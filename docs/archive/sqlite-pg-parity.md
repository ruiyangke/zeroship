> Archived 2026-05-25: shipped. Live reference: docs/reference/sqlite-divergences.md.

# SQLite ↔ Postgres parity — SQLite as the dev backend

**Status**: design v4, for critic/reviser loop. Not committed to main.
**Date**: 2026-05-24
**Phase**: implementation gated behind P9 (API alignment) + P6a (multi-tenancy infra) — both touch `orchestrator/register_model/`, which this phase rewrites. Assumes the concurrent **`hardening`-feature removal** (§1.2) has landed or lands alongside PR 0.
**Author**: AI pilot.

---

## 0. Goal + contract

Replace **pglite** (WASM Postgres embedded in the Vite dev server) with the native **SQLite backend** as the local-dev database, and make SQLite a viable small-scale / edge / single-tenant **deploy target**. Requirements from the user (2026-05-24):

1. **Full compat** — the same `@zeroship/db` SDK code produces the same observable behavior on PG and SQLite.
2. **Both backends in one binary** — PG (`compio-postgres`) *and* SQLite (`rusqlite` + `flume` + `sqlite-vec`) compile into the *same* `worker`/`cli` binary, unconditionally. Not a build-time slice.
3. **Selective by configuration** — the backend is chosen **at runtime from the connection string**, not at compile time. `postgres://…` picks PG; `sqlite:`/`file:`/a bare path/`:memory:` picks SQLite.

<!-- Added in this round: requirements 2+3 are new hard constraints (2026-05-24). They move the design from "compile-time Cargo-feature backend selection" to "both backends always compiled in; runtime URL-scheme dispatch." This is precisely the model `sqlx`'s `AnyPool`, SeaORM's `DatabaseConnection`, and Diesel's `MultiConnection` ship: one binary holds an enum of compiled-in backends and the scheme of the connection URL selects the arm at connect time (sqlx docs.rs; SeaORM docs.rs `enum.DatabaseConnection`; Diesel `MultiConnection`). Diesel frames the alternative — a Cargo-feature slice gives "zero runtime overhead but separate binaries per backend" vs the enum's "single binary, runtime match" (colliery.io/blog/dual_backends) — which is the exact trade §1.1 resolves in favor of one binary. -->

§1.1 specifies the binary-shape decision (remove the `sqlite` Cargo feature) and §4.1 specifies the runtime dispatch. The selection **granularity** (per-deployment vs per-app) is settled against the code in §8.

### The compat contract — Option A (three tiers)

Two different engines cannot be bit-identical everywhere. "Full compat" is scoped to what's deterministic; the rest is best-effort with documented caveats. The user's directive: *"minor mismatch is acceptable for local dev; make the SQLite3 impl as close to PG as possible."*

> **Tier-1 is a *read-decode* property, and it is not satisfied today.** The R3 code review established that the two backends decode rows to **different JSON types** for the most common fields — `created_at` is a JSON **Number** on PG (unix-ms, `v8_bridge.rs:425`) but a JSON **String** on SQLite (`mod.rs:1556`); `boolean` is `Bool` on PG (`v8_bridge.rs:377`) vs `Number` on SQLite (`mod.rs:1551`); `json` is a parsed object on PG (`v8_bridge.rs:464`) vs a String on SQLite (`mod.rs:1556`); `bytes` decodes to `null` on PG today (no BYTEA arm, `v8_bridge.rs:500-503`) and an array-of-byte-ints on SQLite (`mod.rs:1557`). Neither decoder consults the Zeroship declared schema. **So the Tier-1 entries below are *targets after* the §4.4 read-side normalization layer + §4.7 deterministic-ordering fixes land — not properties of the code as it stands.** The matrix (§2) is the verifier of that contract, not its definition.

| Tier | Surface | Contract | Enforced / made true by |
|---|---|---|---|
| **1 — guaranteed identical** | CRUD (find/insert/update/delete/upsert), system fields, soft-delete/purge/restore, optimistic concurrency (`version`), `count`, encryption, masking, transactions + savepoints, schema-history table (name + columns; requires §4.3 rename) | Byte-identical SDK-observable JSON | **§4.4 normalization layer** + **§4.7 ordering** + parity matrix (CI, both backends) |
| **1 — by construction** (no new work) | `id`/text → String; `int`/`version`/`count` → Number | already identical on both decoders (`v8_bridge.rs:387/392`, `mod.rs:1551`) | parity matrix asserts no regression |
| **1 — via normalization layer** (§4.4) | `timestamp`/`created_at`/`updated_at`/`deleted_at`, `boolean`, `json`/`jsonb`, `bytes`/encrypted, `numeric` | canonical wire shape per type (§4.4 contract table) | the new schema-typed read-side normalizer; **largest Tier-1 work item** |
| **1 — via ordering normalization** (§4.7) | ordered `find`/`distinct` over nullable + multi-row order; `distinct` ordering | identical row *order* | inject `NULLS`, implicit `id` tiebreaker, forced `COLLATE` |
| **2 — best-effort, documented** | vector search ranking, FTS ranking, geo `near` ordering; `avg`/`sum` float precision | Same result *set* for unambiguous queries; ordering of near-ties + float ULPs may differ (engine math) | parity matrix with tolerance + doc caveats |
| **3 — documented non-goal** | concurrency throughput (SQLite single-writer vs PG MVCC), full-Unicode locale collation, float/NaN value edge cases | Documented difference; force consistent collation for ASCII (§4.6) | docs + a "known divergences" reference |

**Tier movement vs prior rounds (R3):** `timestamp`, `boolean`, `json`, `bytes`, `numeric` were implicitly filed as Tier-1-by-construction; they are *not* — they are Tier-1 only *after* the normalization layer (§4.4). `boolean` was absent from every prior table — added. NULL sort placement and default multi-row order were mis-filed under Tier-3 "float/NULL edge cases" (a *value* bucket); they are *sort-placement* issues, now Tier-1-via-§4.7. `distinct` was Tier-1 while built on `ORDER BY col` whose collation+NULL order is Tier-3 (an internal contradiction); resolved in §4.7.

**Why Option A is acceptable**: SQLite's role is dev (single developer, no concurrency, ranking-order-in-dev is irrelevant) + small-scale deploys (low concurrency by definition). The multi-tenant platform stays Postgres. A creator who needs MVCC concurrency or exact pgvector recall deploys to the Postgres platform; SQLite is the zero-setup dev mirror + the low-end deploy option.

---

## 1. Motivation

- **Kill pglite's WASM weight in dev.** `@electric-sql/pglite` + `pglite-socket` are tens of MB of WASM spawned per dev session (`sdks/vite-plugin/src/dev-db.ts`). The native SQLite backend (bundled rusqlite, already in the tree) is lighter and in-process.
- **A no-Postgres deploy target.** Single-tenant / edge / self-hosted creators get a zero-infra option (one SQLite file) instead of requiring a Postgres server.
- **Finish the dual-backend design.** The `BackendHandle` enum + capability traits (`SqlExecutor`, `LockManager`, `NamespaceManager`, `SchemaIntrospect`, `IndexBuilder`, `ChangeStream`, `EncryptedColumn`, and the cross-backend `AuditWriter` + `DialectBuilder` seams) were built for exactly this — and the enum-of-compiled-in-backends shape is precisely how `sqlx` (`AnyPool`), SeaORM (`DatabaseConnection`), and Diesel (`MultiConnection`) deliver one binary that talks to multiple engines. The SQLite *backend impls* are substantial and real (see §3, B8) — but the `register_model` orchestrator cannot reach them, and that reachability gap is the heart of this work. With both backends now unconditionally compiled in (§1.1) and selected at runtime (§4.1), the enum is no longer "PG today, SQLite someday" — both arms are live in the same process.
- **Parity testing is a correctness multiplier.** Running every SDK op against both backends surfaces latent bugs in *both*.

### Why pglite can't simply stay

pglite gives Postgres-wire parity in dev (the runtime's `compio-postgres` driver talks to it unchanged). Switching to SQLite trades that automatic parity for an *engineered* parity (this doc). The user accepts the Tier-2/3 cost for the WASM-weight + deploy-target wins. (If we kept pglite, none of this work is needed — but we'd keep the WASM dep and have no SQLite deploy story.)

---

## 1.1 Binary shape — remove the `sqlite` Cargo feature (RECOMMENDED)

<!-- Added in this round: requirement #2 ("both backends in one binary") forces a decision on the `sqlite` feature flag. -->

Requirement #2 says both backends ship in one binary. Today `sqlite` is an **opt-in Cargo feature**: `rusqlite`/`flume`/`sqlite-vec` are optional deps, the whole `crate::backend::sqlite` module is `#[cfg(feature = "sqlite")]` (`backend/mod.rs:81-90`), the `BackendHandle::Sqlite` arm is gated (`backend/mod.rs:1771`), and **dozens** of consumer sites carry `#[cfg(feature = "sqlite")]` (the CRUD search arms `crud/mod.rs:1462/1631/1771`, the mask-policy sidecar `crud/mask_policy.rs:237/323/406`, `mask_drift.rs`, `cross_app_fk.rs:15`, the production-vs-test installer split `lib.rs:151-153`). The CLI and dev-runtime are built *without* `--features sqlite` (this was the doc's blocker **B5**).

**Recommendation: remove the `sqlite` feature flag entirely.** Promote `rusqlite` (bundled amalgamation) + `flume` + `sqlite-vec` to **unconditional** dependencies; delete every `#[cfg(feature = "sqlite")]` and un-gate the `BackendHandle::Sqlite` arm. The runtime/CLI then *always* contain both backends, and B3 dissolves — there is no longer a feature to forget to enable.

This mirrors the concurrent decision to remove the **`hardening`** feature (the platform ships one fully-capable binary, not feature-sliced builds — see §1.2). It is also the canonical "one binary, many backends" shape: `sqlx`'s `any` feature, SeaORM's `DatabaseConnection`, and Diesel's `MultiConnection` all keep every compiled-in backend present and select at runtime; none of them gate a backend's mere *presence* behind a consumer-visible build flag (sqlx `any` docs.rs; SeaORM docs.rs; Diesel `MultiConnection`).

**The real trade-off (stated honestly):** the bundled SQLite amalgamation (~250k lines of C) **and** the `sqlite-vec` C extension now compile into *every* build — a build-time cost (one C-compile of the amalgamation per clean build) and a binary-size cost (the amalgamation + sqlite-vec object code, present even in a deployment that only ever talks to PG). Diesel notes this is the inherent price of the enum-runtime model vs the feature-slice model (colliery.io/blog/dual_backends).

**The alternative — keep `sqlite` as a *default-on, optional* feature.** `rusqlite` stays optional but defaults on, so the normal `worker`/`cli` build includes it (satisfying requirement #2 for the shipped binary) while a `--no-default-features` PG-only build remains *possible* for a size-sensitive PG-only deployment. This preserves the size escape hatch at the cost of keeping every `#[cfg(feature = "sqlite")]` alive (the maintenance burden the no-back-compat stance otherwise lets us delete) and keeping a build configuration that CI must still exercise.

**Why "remove the flag" wins:** (a) requirement #2 wants *one* binary, and a default-on flag still admits a second shape that must be built and tested; (b) the no-back-compat / one-binary stance (AGENTS.md, §1.2's `hardening` removal) is explicitly about deleting feature-slice complexity, and ~30 `#[cfg(feature = "sqlite")]` sites are exactly that complexity; (c) the SQLite backend is the *dev* default (§7) and a small-scale *deploy* target — it is on the hot path for most builds anyway, so the PG-only slice it protects is a narrow, low-value configuration. The binary-size cost is real but bounded (one embedded SQLite, not per-app), and the platform already bundles V8.

**This is a recommendation, not a balanced open question** — it is filed as the Q2 default (§11) with the size trade documented, not held for the user. The only reason to take the default-on-optional alternative is a hard binary-size ceiling on a PG-only edge image, which is not a stated constraint.

---

## 1.2 The `hardening` feature is becoming unconditional — implications for this design

<!-- Added in this round: the concurrent `hardening`-removal refactor changes the RegisterBackend trait surface this doc reasons about. -->

A concurrent refactor (landed 2026-05-24 on branch `refactor/remove-hardening-flag`, pending merge to main) removes the **`hardening`** Cargo feature, making the security model (encryption, masking, the `auth/*` SECURITY DEFINER bootstrap) **unconditional**. Two consequences this doc must assume:

1. **A single `RegisterBackend` trait.** Today there are *two* definitions: with `hardening` it requires `+ EncryptedColumn` (`backend/mod.rs:1623-1636`); without it, that bound is dropped (`backend/mod.rs:1653-1665`). Post-removal there is **one** `RegisterBackend` that *always* requires `EncryptedColumn`. There is no "sqlite-without-hardening" combination to design around — every register-capable backend must satisfy `EncryptedColumn`.
2. **SQLite already satisfies it.** `impl EncryptedColumn for SqliteBackend` ships at `backend/sqlite/mod.rs:1932`. So when SQLite's `register_model` arm (B1) is built (§4.2), the `EncryptedColumn` super-bound is **already met** — no new SQLite encryption work is gated by this. The mask-policy sidecar and mask-drift paths likewise already have SQLite arms (`crud/mask_policy.rs:237`, `crud/mask_drift.rs`); un-gating them (§1.1) makes them unconditional alongside the PG arms.

**Net effect on the rest of this doc:** every prior caveat about "mirror the `hardening` cfg gate on any new register trait" (former §4.1 caveat, former §9 PR 2 note) is **deleted** — there is no dual gate to mirror. The register trait is single, always-`EncryptedColumn`, and SQLite meets it.

---

## 2. The parity test matrix (the contract artifact)

The matrix is the **verifier** of the Tier-1 contract; it cannot *define* it. A literal "Tier-1 = deep-equal" assertion is unrunnable against today's decoders — it fails on row one of every fixture, because `created_at`, `boolean`, and `json` decode to different JSON *types* on the two backends (§0 note, §4.4). So the dependency is: **first the §4.4 canonical wire-shape contract + the normalization layer that makes both backends emit it; then the matrix asserts deep-equal against that contract.** The matrix is the regression guard for the normalization layer, not a substitute for it.

Structure (`crates/plugin-db/tests/parity/` or a dedicated harness):
- A fixture schema exercising every field type, index kind, encryption, masking, system fields, refs.
- For each SDK operation: run on PG (via test pool) + SQLite (via temp file), then assert:
  - **Tier 1**: deep-equal *after* both rows have passed the §4.4 normalization layer — i.e. the matrix asserts each backend independently produces the canonical wire shape, then asserts the two are equal. A divergence is a normalization-layer bug, surfaced at the field that broke (the matrix names the field + declared type, not just "rows differ").
  - **Tier 2**: same result-set membership; ordering / float precision compared with documented tolerance (e.g. "top-k set equal; intra-set order not asserted", "avg within 1 ULP").
  - **Tier 3**: not asserted; a doc note links the divergence.
- Runs in CI on every plugin-db PR. With the `sqlite` feature removed (§1.1) the SQLite leg needs **no special features** — `cargo test -p zeroship-plugin-db` builds both backends. The PG leg still needs a live Postgres listener (`Pool::connect`, lib.rs:550) — see §11 Q4 for the CI shape.

A `docs/reference/sqlite-divergences.md` enumerates every Tier-2/3 difference (the honest "here's where dev differs from prod").

---

## 3. Blocker inventory (verified against code 2026-05-24)

Every row below was checked against the cited `file:line` this round. The headline correction from v0: the register pipeline is **already generic over `RegisterBackend`** — but that genericity is a dead end for SQLite, because `RegisterBackend`'s super-bounds pin the client type to Postgres *and* the pipeline body calls PG-only helpers directly. The real blocker is structural (trait hierarchy + lock-guard lifetime + the audit *provisioning* gap), not "generalize a concrete type."

<!-- Added in this round: B4/B5 reclassified as resolved-by-architecture under the new requirements. -->
**Two blockers are now resolved *by the architecture* (requirements #2 + #3), not by further implementation work — they move out of the table into the design narrative:**

- **B4 (DbPlugin had no SQLite production path) → resolved by §4.1.** The runtime URL-scheme dispatch in `init_pool_async` (§4.1) *is* the production SQLite installer. `DbPlugin::new` (lib.rs:253) was never the gap — it only stores the URL string; the construction happens in `init_pool_async` (lib.rs:544) / `set_pool` (context.rs:280), which §4.1 teaches to branch on scheme. The test-only `set_sqlite_backend_for_tests` (lib.rs:383) seam is replaced by a production `set_sqlite_backend` installer. *Note: B4-as-installable is resolved by PR 1; generic CRUD still can't run on the installed SQLite backend until B9/B10 (PR 3) — installability and read-path are distinct.*
- **B5 (CLI/dev-runtime built without `--features sqlite`) → resolved by §1.1.** With the `sqlite` feature removed, the CLI and dev-runtime *always* contain the SQLite backend; there is no feature to forget. The dispatch (B4 → §4.1) does the rest. CLI no longer needs any feature-selection change beyond removing the now-defunct flag.

The remaining real implementation work is **B1/B1a/B2/B3** (register_model SQLite arm — trait hierarchy, audit provisioning, lock lifetime), **B7** (SQLite apply pass: `execute_batch` + `build_create_index(spec, false)`), and **B9/B10** (SQLite generic CRUD read path + the read-side normalization layer). B6 (vite/pglite) is mechanical dev rewiring, resolved concretely in §7.

**Two cross-backend seams already ship — the doc is scoped around them, not around reinventing them:**

- **`AuditWriter` trait** (`backend/mod.rs:660`) is implemented for *both* `PostgresBackend` (`backend/postgres.rs:316`) and `SqliteBackend` (`backend/sqlite/mod.rs:1054`). The SQLite impl writes audit rows through the session actor into the per-app `__zs_migrations` table. So there is **no "build a Connection-based audit-write path" work** — that path exists. The remaining SQLite audit work is narrow: (a) the audit-table *provisioning* DDL and (b) `next_schema_version`, both explicitly flagged as not-yet-shipped at `backend/sqlite/mod.rs:1049-1052`. See B2.
- **`DialectBuilder` trait** (`backend/mod.rs:815`) is implemented for `SqliteBackend` (`backend/sqlite/mod.rs:1096`): `map_zs_type`, `now_fn`, `build_create_index(spec, online)` (online no-ops on SQLite, `mod.rs:826-829`), `build_ensure_app_schema`, `last_insert_rowid_sql`. This *already* solves the index-dialect problem (B7), the type mapping (§4.4), and the RETURNING-vs-rowid question (former Q7). See B7.

| # | Blocker | Evidence (verified file:line) | Tier impact |
|---|---|---|---|
| **B1** | The register pipeline cannot dispatch to SQLite even though it is already generic over `RegisterBackend`. `run_pipeline<B: RegisterBackend>` (mod.rs:197) and `bootstrap<'p, B: RegisterBackend>` (bootstrap.rs:98) are generic, but `RegisterBackend`'s super-bounds **pin Postgres**: `PgSqlExecutor: SqlExecutor<Client = compio_postgres::Client>` (mod.rs:874), `PgLockManager: LockManager<Client = compio_postgres::Client>` (mod.rs:907), plus a direct `LockManager<Client = compio_postgres::Client>` bound (mod.rs:1626/1641). `SqliteBackend::SqlExecutor::Client = SqliteSessionHandle` (sqlite/mod.rs:376) can never satisfy these. The entry `exec_register_model` confirms it by hard-pulling `backend.as_postgres().ok_or_else(\|\| DbError::backend_unsupported("register_model"))?` (mod.rs:162-164). | foundational — no schema install on SQLite |
| **B1a** | The pipeline *body* is PG-pinned, not just the trait bound. `build_ctx<B: RegisterBackend>` (bootstrap.rs:206) calls `backend.pool_handle().as_ref()` (bootstrap.rs:229) — `pool_handle` exists only on `PgSqlExecutor` (mod.rs:880, returns `&Rc<compio_postgres::Pool>`) — then passes that `&Pool` to the audit free functions. Relaxing the trait bound alone would not compile: the body names Postgres types. | foundational |
| **B2** | The audit *write* path is dual-backend already; only *provisioning* + `next_schema_version` are missing on SQLite. The `AuditWriter` trait (mod.rs:660) is implemented for both backends — `impl AuditWriter for SqliteBackend` (sqlite/mod.rs:1054) routes a parameterised INSERT through the session actor into the per-app `__zs_migrations` table (sqlite/mod.rs:1069), and `impl AuditWriter for PostgresBackend` (postgres.rs:316) wraps the free fn. What is *not* yet on SQLite, per the in-code note (sqlite/mod.rs:1049-1052): **(a)** audit-table provisioning DDL (the PG analogue is `ensure_audit_table_exists`, audit.rs:202, emitting `GENERATED ALWAYS AS IDENTITY`/`JSONB`/`TIMESTAMPTZ`, audit.rs:209-219) and **(b)** `next_schema_version` (PG free fn at audit.rs:316). The PG free fns over `&Pool` (audit.rs:202/316/333) stay PG-only; the cross-backend seam is `AuditWriter`, not those fns. | Tier 1 (schema history) |
| **B3** | The advisory-lock guard carries the **PG pool-borrow lifetime** with no SQLite analog. `bootstrap` calls `backend.acquire_pooled_client_for_lock()` (bootstrap.rs:155 → `PgLockManager`, mod.rs:918-920, returns `compio_postgres::PooledClient<'p>`), threaded into `LockGuard<'p>` (lock_guard.rs:100, field `client: Option<PooledClient<'p>>` line 104; `acquire` bounded `B: LockManager<Client = compio_postgres::Client>` line 157), which `run_pipeline` returns and threads into `apply`. SQLite's lock is the in-process `InProcessLockRegistry` (HashMap of `Rc<Cell<bool>>`, sqlite/lock.rs:35-41) with **no pool-borrow lifetime** — and cross-process `BEGIN IMMEDIATE` is explicitly a "P5+ concern", not yet implemented (sqlite/lock.rs:9-10). The `'p` is baked into the generic pipeline's return types. | foundational |
| **B4** ✅ **resolved by §4.1** | `DbPlugin` had no SQLite production path; the wiring gap is in pool/backend construction, not `new()`. `init_pool_async` (lib.rs:544) unconditionally calls `Pool::connect(&url, 8)` (lib.rs:550) with no scheme dispatch, then `set_pool` builds a `PostgresBackend` and installs `BackendHandle::Postgres` (context.rs:280-283); SQLite was installed **only** through test-gated `set_sqlite_backend_for_tests` (lib.rs:383 → context.rs:308-313). **Resolution:** §4.1 makes `init_pool_async` branch on URL scheme and adds a production `set_sqlite_backend` installer — requirement #3 *is* this dispatch. (Installable ≠ CRUD-capable: B9/B10 still gate generic CRUD on the SQLite arm.) | switchability — **resolved by design** |
| **B5** ✅ **resolved by §1.1** | CLI/dev-runtime built without `--features sqlite`; the dev binary is the CLI (`cli/main.rs` PG path; `dev-server.ts:229` spawns `node_modules/.bin/zeroship`). **Resolution:** §1.1 removes the `sqlite` feature, so the SQLite backend is *always* compiled into the CLI/worker — no feature to enable. Requirement #2 dissolves this. | dev wiring — **resolved by design** |
| **B6** | vite-plugin dev wiring is pglite-specific. `dev-db.ts` spawns pglite + forwards `DATABASE_URL`; `dev-server.ts` forwards it. | dev wiring |
| **B7** | Apply pass must call `DialectBuilder` for index DDL, and use batch exec. The index-dialect concern is **already solved**: `DialectBuilder::build_create_index(spec, online)` (mod.rs:826-829) yields `CREATE INDEX CONCURRENTLY` on PG and a plain `CREATE INDEX` on SQLite (the `online` flag no-ops there); `SqliteBackend` impls it (sqlite/mod.rs:1111). The remaining apply-stage work is mechanical: the SQLite orchestrator's apply (a) routes its index DDL through `build_create_index(spec, false)` and (b) uses `Connection::execute_batch` for multi-statement DDL rather than PG's one-shot `pool_exec`. *Downstream of B1/B3 — irrelevant until the SQLite pipeline can be entered at all.* | Tier 1 (schema) |
| **B8** | SQLite backend impls are **implemented but unreachable**, not merely unverified. The SQLite arm is substantial — `sqlite/` is ~7.7k lines (`session.rs`, `cdc.rs`, `fts.rs`, `vector.rs`, `spatial.rs`, `lock.rs`); `SqliteBackend` implements `SqlExecutor` (sqlite/mod.rs:375), `LockManager` (413), `NamespaceManager` (497), `SchemaIntrospect`, `IndexBuilder`. But the `BackendHandle::as_sqlite` accessor (mod.rs:1874) has **no production caller for register/migrations** — the rustdoc says so explicitly (mod.rs:1869-1872). The *capability-trait* search paths (FTS/vector/geo) **do** have `as_sqlite()` arms (`crud/mod.rs:1464`, etc.) and decode rows themselves. So the gap splits: **(a) reachability** for register/migrations (B1–B3) and **(b) equivalence** of the reachable bits (the matrix, §2). | all tiers |
| **B9** | **The generic CRUD execute+decode path is PG-pool-pinned; SQLite has no generic read/write path at all.** `dispatch_find` → `exec_query` (crud/mod.rs:341) → `run_sql` (exec.rs:43) which returns `Vec<compio_postgres::Row>` and pulls `context.pool()` unconditionally (exec.rs:60-70); `exec_mutation`/`exec_count` do the same (exec.rs:94-116). `SqlExecutor for SqliteBackend` (sqlite/mod.rs:375-411) exposes only `pool_exec`/`client_exec`/`acquire_dedicated_client` — **no row-returning read method**. So on a SQLite-bound runtime, `find`/`insert`/`update`/`delete`/`upsert`/`count` hit `run_sql` → `context.pool()` → `None` → "pool not initialized". Only FTS/vector/geo work on SQLite (their explicit `as_sqlite()` arms). This is **not** "implemented but unreachable" (cf. B8) — for generic CRUD it is **not implemented**. Building this SQLite generic read/write+decode path is a prerequisite for *any* Tier-1 CRUD claim, and is the natural home for B10. | Tier 1 (all CRUD) |
| **B10** | **No schema-typed read-side normalization layer exists; both decoders are schema-blind.** PG decodes by OID (`column_to_json`, v8_bridge.rs:372-505); SQLite by storage class (`TypedCell`→JSON, mod.rs:1551-1563, and `run_query` which stringifies *everything*, session.rs:832-844). Neither sees the Zeroship declared type at decode time, so they disagree on JSON type for `timestamp`/`boolean`/`json`/`bytes`/`numeric` (§0 note). Achieving Tier-1 byte-identity requires a new normalizer that *does* know each field's declared type. The seam already exists: the CRUD read chain fetches `context::schema_for(app, coll)` (crud/mod.rs:315) and runs schema-aware per-row post-passes — `apply_encryption_on_read` (crud/mod.rs:345/1966) and `apply_mask_wrap_on_read` → `mask_pass::wrap_row_on_read` (crud/mod.rs:357/2120-2135, mask_pass.rs:388). The normalizer is a sibling post-pass. Design + scope in §4.4. **This is the single largest Tier-1 work item and was entirely unscoped before R3.** | Tier 1 (timestamp/boolean/json/bytes/numeric) |
| **B11** | **Native `env.db.transaction()` is PG-bound; on SQLite it is `backend_unsupported`.** P9 PR 3 made the native transaction path PG-only: the v8_method entry `transaction_dispatch` (orchestrator/transaction.rs:142) spawns `exec_begin_or_savepoint` (transaction.rs:265), whose top-level-BEGIN arm hard-pulls `.as_postgres().ok_or_else(\|\| DbError::backend_unsupported("transaction"))?` (transaction.rs:298), then `pg.acquire_dedicated_client()` (transaction.rs:299) opens a dedicated libpq connection, runs `BEGIN [ISOLATION LEVEL …]`, and parks the `compio_postgres::Client` in the per-isolate `tx_conn` slot via `install_tx_client` (transaction.rs:318). CRUD callbacks then route through `tx_conn`. **The savepoint/commit/rollback state machine already exists and is mostly backend-agnostic** — P9 PR 3 shipped it in orchestrator/transaction.rs (`TxFinalizer` transaction.rs:110, `MAX_SAVEPOINT_DEPTH = 8` transaction.rs:86, nested SAVEPOINT via `run_on_tx_conn` transaction.rs:363); only the connection *acquisition* and the `tx_conn` client *type* are PG-bound, not the savepoint SQL (which is plain-standard and is already exercised against the SQLite engine in `tests/sqlite_integration.rs` per the module doc, transaction.rs:62-65). **This is a Tier-1 parity gap** — the same `env.db.transaction(fn)` SDK call must work identically on both backends. The fix is feasible without a dedicated connection: `SqlExecutor for SqliteBackend` already exposes `acquire_dedicated_client` (returns a `SqliteSessionHandle` multiplexing the actor, sqlite/mod.rs:378-386) and `client_exec` (sqlite/mod.rs:398). So the SQLite arm runs `BEGIN`/`SAVEPOINT`/`COMMIT` through `client_exec` against that handle, reusing the existing state machine. Design in §4.8; required parity-matrix row. | Tier 1 (transactions + savepoints) |

**Note on B7/B11 priority**: B7 (apply-stage DDL — now mostly wiring, since `DialectBuilder` ships the index dialect) and B8(b) (equivalence) are *real* but strictly downstream of B1/B3. **B9/B10/B11 sit on the CRUD/transaction path, not the schema-install path** — B9/B10 are the bulk of the *value-correctness* Tier-1 work the prior rounds under-counted; B11 is the transaction-parity gap P9 introduced. v0 listed B7/B8 as peers of B1; they are not.

**§4.4 ("map_zs_type is a shipped seam, so the type problem is solved") was wrong about *which* seam.** `map_zs_type` (dialect.rs:109) is the **write/DDL** seam — it picks the storage column type at CREATE TABLE. Tier-1 byte-identity is a **read-decode** property (B10), a different and unbuilt seam. The prior doc conflated the two and hand-waved the read side into "the matrix asserts it". Corrected in §4.4.

---

## 4. Architecture — "as close to PG as possible"

### 4.1 Backend selection (requirement #3, resolves B4) — runtime URL-scheme dispatch

<!-- Rewritten in this round: requirement #3 makes backend selection a runtime decision driven by the connection string. This section defines that dispatch concretely. -->

**This is the concrete meaning of goal #2 ("selective by configuration"): the connection-string *scheme* picks the backend at connect time, in one process that holds both.** This is exactly the `sqlx` `AnyPool` / SeaORM `DatabaseConnection` / libSQL pattern — the URL scheme is the selector, the compiled-in backends are an enum, and the arm is chosen when the connection is established (sqlx `any` docs.rs: "the database driver used is determined by the scheme of the connection url"; libSQL: `file:` → local SQLite, `libsql://`/`http(s)://` → remote, *same API* across all). zeroship's `BackendHandle` enum (`backend/mod.rs:1755`) is that enum; this section adds the scheme parse that selects the arm.

The dispatch point is **`init_pool_async`** (lib.rs:544) — *not* `DbPlugin::new` (lib.rs:253, which only stores the URL string). Today `init_pool_async` reads `context::db_url()` (lib.rs:545) and unconditionally `Pool::connect`s (PG). The change: parse the scheme first, then construct the matching backend.

**Scheme → backend mapping** (modeled on sqlx's scheme table + libSQL's local-vs-remote split):

| URL form | Backend | Construction |
|---|---|---|
| `postgres://…` , `postgresql://…` | `PostgresBackend` | `Pool::connect(&url, 8)` → `set_pool` → `BackendHandle::Postgres` (existing path, **unchanged**) |
| `sqlite://<path>` , `sqlite:<path>` | `SqliteBackend` | parse path → `SqliteBackend::open(path)` → new production `set_sqlite_backend` → `BackendHandle::Sqlite` |
| `file:<path>` | `SqliteBackend` | same; the libSQL/SQLite-URI spelling (libSQL uses `file:` for local) |
| `:memory:` | `SqliteBackend` | in-memory DB (dev/test ephemeral) |
| bare filesystem path (`./dev.sqlite`, `/var/lib/zs/app.db`) | `SqliteBackend` | a path with no scheme defaults to SQLite-file — the zero-ceremony spelling the CLI/dev path uses (§7) |

The scheme parse belongs in a small helper (e.g. `backend_for_url(&url) -> BackendKind`) called at the top of `init_pool_async`, so the same parse is unit-testable and reused by the CLI. **Unknown scheme → a clear configuration error** ("unsupported database URL scheme `<x>`; expected postgres://, sqlite:, file:, or a filesystem path"), not a fallthrough to PG. This mirrors sqlx's behavior of erroring on an unrecognized scheme rather than guessing.

```rust
// init_pool_async (lib.rs:544), after reading `url`:
match backend_for_url(&url) {
    BackendKind::Postgres => {
        let pool = Pool::connect(&url, 8).await.map_err(/* …source-chain walk… */)?;
        ctx_mut(|c| c.set_pool(Rc::new(pool)));        // installs BackendHandle::Postgres
    }
    BackendKind::Sqlite { path } => {
        let backend = SqliteBackend::open(&path).await?;
        ctx_mut(|c| c.set_sqlite_backend(Rc::new(backend))); // installs BackendHandle::Sqlite
    }
}
```

**Installer promotion (replaces the test-only seam).** The SQLite arm is installed today only through `set_sqlite_backend_for_tests` (lib.rs:383 → `context::set_sqlite_backend`, context.rs:308-313), gated `#[cfg(all(any(test, feature = "test-helpers"), feature = "sqlite"))]`. PR 1 promotes `context::set_sqlite_backend` to an **un-gated production method** (the `sqlite` feature is gone per §1.1; drop the `test-helpers` gate too) and keeps a thin `set_sqlite_backend_for_tests` wrapper only if the integration tests still want the public re-export. This is the production replacement for the test-only seam called out in the mandate.

**`SqliteBackend::open(path)` is the one genuinely new constructor.** The capability impls exist (§3 B8); what's missing is the production entry that mints the session actor for a given file path (the test path constructs it inline). It computes/creates the DB directory, mints the `SqliteSession`, and runs the boot PRAGMAs (`session.rs:370` already runs `BOOT_PRAGMAS` incl. `journal_mode=WAL`). Per-app files are then ATTACHed by `ensure_app_schema` at register time (§4.5) — `open` establishes the *base* connection/session, not the per-app files.

**CRUD still needs B9/B10.** The PG arm installs an `Rc<Pool>` *and* the `BackendHandle`; the SQLite arm installs only `BackendHandle::Sqlite` (there is no pool). The production CRUD path assumes a pool exists (**B9**): `exec_query`/`exec_mutation`/`exec_count` → `run_sql` (exec.rs:43-116) call `context.pool()` unconditionally and return `compio_postgres::Row`. So PR 1's scheme dispatch makes the SQLite backend *installable*, but generic CRUD still hits `context.pool() == None` until PR 3(a) gives it a SQLite read/write path. **PR 1 must make the "no pool on SQLite arm" failure an explicit, typed error** (`backend_unsupported` / "SQLite CRUD path not yet wired") — not a panic — so the gap is visible and the SQLite dev backend (PR 6) is correctly gated behind PR 3.

**Both backends compiled in (requirement #2):** with the `sqlite` feature removed (§1.1), the runtime/CLI *always* contain both arms; there is no feature combination to get wrong. The `hardening` feature is also gone (§1.2), so there is **no `RegisterBackend` dual-gate to mirror** in §4.2 — the prior "per-launch caveat" about mirroring `#[cfg(feature = "hardening")]` is **deleted**.

### 4.2 register_model SQLite arm (B1, B1a, B2, B3) — the core design decision

**v0 was wrong.** It prescribed "generalize the pipeline over `RegisterBackend` so it dispatches to either backend." That is a no-op: the pipeline is *already* generic over `RegisterBackend` (mod.rs:197, bootstrap.rs:98), and `RegisterBackend` is PG-pinned by construction (B1) — so the generic bound can never be instantiated with `SqliteBackend`. Worse, the body names Postgres types directly (B1a: `pool_handle()` + `&Pool` audit calls at bootstrap.rs:229-232). Relaxing the bound would not even compile.

The real decision is *how to give SQLite a register pipeline at all*. Two viable paths:

#### Path A (alternative) — split the trait hierarchy

Carve `RegisterBackend` into a **client-agnostic register marker** (over `SqlExecutor` *without* the `Client` pin, `NamespaceManager`, `SchemaIntrospect`, `IndexBuilder`, plus generic capability traits) and push the Postgres-specific machinery behind the existing PG extension traits. (With `hardening` now unconditional — §1.2 — there is a *single* `RegisterBackend` to carve, always requiring `EncryptedColumn`; SQLite already satisfies it, sqlite/mod.rs:1932. The former dual-`cfg` carve is gone.) Then make the pipeline body backend-agnostic by abstracting the two PG-pinned operations:

1. **Audit-write** — route the per-op audit INSERT through the **existing** `AuditWriter::write_audit_row` trait method (mod.rs:660), not the PG `&Pool` free fn (audit.rs:333). Both backends already implement it (PG postgres.rs:316, SQLite sqlite/mod.rs:1054). The only net-new audit work is the two missing SQLite ops from B2 — provisioning DDL + `next_schema_version` — which can ride as default-free methods alongside `write_audit_row` or as a small sibling trait. **Do not introduce a new `AuditStore` trait; `AuditWriter` is that seam.**
2. **Lock guard** — abstract `LockGuard<'p>`'s pool-borrow lifetime. This is the hard part. A **GAT on `LockManager`** (`type LockClient<'p>`) is the route that reopens the Open Q5 HRTB fight (mod.rs:889-898); an **associated guard type** is less invasive but still threads a backend-chosen guard through the generic pipeline's return path. Either keeps `PooledClient<'p>` on the PG arm and a `'static` in-process guard on the SQLite arm.

**Why this is the *alternative*, not the recommendation**: the codebase authors already considered and **deliberately deferred** the GAT path. The `PgLockManager` rustdoc (mod.rs:889-898) records "Open Q5 resolution": a `LockManager` GAT "is workable but fights the trait solver in subtle ways (HRTB-style bounds at consumer sites)… future backends (sqlite, planetscale) would have their own session-management primitive on a different extension trait — we take the PG extension-trait path and defer cross-backend lifetime threading." The **GAT** variant of Path A reopens precisely that fight; the associated-guard-type variant avoids the worst of it but still threads a backend-chosen guard lifetime through the generic pipeline's return path. Either way the payoff is thin, because the two backends' bootstrap and apply stages genuinely diverge (CONCURRENTLY vs plain index, `&Pool` audit vs session-actor `AuditWriter`, pooled-client lock vs in-process lock) — a "unified" generic pipeline would be a thin shell wrapping two different bodies anyway.

#### Path B (RECOMMENDED) — fork the pipeline at the dispatch site, share the genuinely-agnostic stages

Branch at `exec_register_model` (mod.rs:162), replacing the `as_postgres()?`-or-error pull with a match on the **accessor results**. The capability traits are dyn-incompatible (`async fn` in trait — see the `SessionMinter` dyn-compat note, mod.rs:717-722), so the idiom must be the typed-handle accessors `as_postgres()` / `as_sqlite()` (mod.rs:162, 1874) the codebase already uses — there is **no `backend.kind()`** method:

```rust
match (backend.as_postgres(), backend.as_sqlite()) {
    (Some(pg), _) => run_pg_pipeline(pg, app_id, collection, schema, indexes, &deploy_id).await,
    (_, Some(sq)) => run_sqlite_pipeline(sq, app_id, collection, schema, indexes, &deploy_id).await,
    _ => Err(DbError::backend_unsupported("register_model")),
}
```

- **`run_pg_pipeline`** = today's `run_pipeline`, renamed and re-pinned to the PG extension traits (or kept generic over `RegisterBackend` — it already is). The `LockGuard<'p>` machinery, `&Pool` audit, and `CONCURRENTLY` apply pass stay **exactly as today**. Zero churn to hard-won PG code.
- **`run_sqlite_pipeline`** = a new orchestrator against `SqliteBackend` (or a small SQLite-specific register trait). Its bootstrap uses the in-process lock (`InProcessLockRegistry`, sqlite/lock.rs) — no `'p` lifetime — and the existing `AuditWriter` impl for row writes plus the two new SQLite audit ops (provisioning DDL + `next_schema_version`, B2). Its apply uses `execute_batch` + `DialectBuilder::build_create_index(spec, false)` (B7).
- **Shared stages — `plan` and `validate` are *not* free to share; both need rerouting first.** v0/v1's "narrow plan/validate to `SchemaIntrospect` and share them" is wrong in two concrete ways, verified this round:
  - **`plan`** is already `SchemaIntrospect`-bounded (`compute_plan<B: SchemaIntrospect<LiveSchema = LiveSchema>>`, plan.rs:41 — narrowed in P0 PR 2, so that part is *done*), but its **body emits Postgres DDL**: it calls `query::build_create_table_with_fks` (plan.rs:62), which hard-codes `SqlDialect::Postgres` (query.rs:475→481). Sharing `plan` requires rerouting that call through the dialect-parameterised `build_create_table_with_fks_for_dialect` (query.rs:507) with the backend's dialect — otherwise the "shared" plan stage bakes PG `CREATE TABLE` text into the `DiffOp`s the SQLite apply stage would try to run. The narrowed bound is necessary but not sufficient.
  - **`validate`** is **not** pure and **not** `SchemaIntrospect`-shaped: it is `validate<B: PgSqlExecutor>` (validate.rs:63) and writes a best-effort audit row per destructive op via `crate::audit::write_audit_row(backend.pool_handle()…)` (validate.rs:108-109). Narrowing it to `SchemaIntrospect` would strip the exact capability it uses; routing it unchanged would re-pin the SQLite arm onto Postgres. The fix is to **re-point validate's audit write onto `AuditWriter::write_audit_row`** (which SQLite already implements) and then bound it on `AuditWriter` (or a combined register marker), *not* `SchemaIntrospect`. Only after both reroutings do the two orchestrators call the same `plan`/`validate` code.

**Why B is recommended**: it matches the design the trait doc-comments already anticipate (SQLite reaches the shipped `AuditWriter`/`DialectBuilder` seams; the PG lock machinery stays on its extension traits, mod.rs:889-898); it leaves the PG pipeline's deliberately-PG-pinned lock/lifetime machinery untouched (honoring Open Q5); the seam is a clean accessor `match` at one call site; and it shares plan/validate *after* the two reroutings above (dialect-parameterise `plan`'s DDL emission; move `validate`'s audit write onto `AuditWriter`) without forcing a GAT refactor whose payoff is mostly cosmetic. The cost — two thin orchestrators sequencing the same four stages, plus those two reroutings — is acceptable and honest about how different the bootstrap/apply stages are between engines.

> Decision is recorded as the recommended default in §11 Q1. If the user prefers the "one true generic pipeline" aesthetic over the lower-risk fork, Path A is the fallback — at the cost of the GAT refactor the team explicitly deferred.

### 4.3 SQLite audit ops (B2) — finish what `AuditWriter` started, then align the table to PG

The audit *write* path is **not** net-new work: `AuditWriter::write_audit_row` (mod.rs:660) is implemented for SQLite (sqlite/mod.rs:1054), already routes a parameterised INSERT through the session actor, and already alias-qualifies the table via `self.quote_ident(app_id)` (sqlite/mod.rs:1067) — so the §4.5 ATTACH-alias requirement is *already satisfied in the write path*. The remaining work is two ops + a shape alignment:

1. **Provisioning DDL** — add a SQLite audit-table creator (the analogue of `ensure_audit_table_exists`, audit.rs:202). SQLite specifics: `INTEGER PRIMARY KEY` rowid instead of `GENERATED ALWAYS AS IDENTITY`; `TEXT` for the JSON `details` column instead of `JSONB`; `TEXT` ISO timestamps instead of `TIMESTAMPTZ`. Provisioning is missing today per the in-code note (sqlite/mod.rs:1049-1052).
2. **`next_schema_version`** — `SELECT COALESCE(MAX(schema_version), 0) + 1 …`, portable SQL, run via the session actor. Currently absent (sqlite/mod.rs:1051).
3. **RETURNING vs rowid** — already answered by code: `DialectBuilder::last_insert_rowid_sql()` returns `Some("SELECT last_insert_rowid()")` on SQLite and `None` on PG (mod.rs:847-854, sqlite/mod.rs:1123). The SQLite `write_audit_row` doesn't need it (it writes a terminal row, mod.rs:670-681), but any future status-transition consumer uses this hook — no user decision (former Q7, retired).

**Tier-1 alignment work (PR 2):** the shipped SQLite table is named `__zs_migrations` with a **subset** column set — no `applied_at`, no `parent_id` (sqlite/mod.rs:1049, 1062-1064) — whereas PG's is `__zeroship_migrations` with `applied_at` (audit.rs:219) and `parent_id` (audit.rs:223). For Tier-1 "byte-identical schema history" (§0, the Option-A directive), the provisioning DDL **renames the SQLite table to `__zeroship_migrations` and adds the missing columns**, and `AuditWriter for SqliteBackend` (sqlite/mod.rs:1067-1072) updates its INSERT target + column list to match. **This is a free rename pre-launch** (AGENTS.md: no published users, no back-compat — no shim, no alias; change the name and every reference in one PR). If the user instead wants to keep the lean SQLite shape, schema-history-table-shape drops to Tier-2 — but the recommended path is align-to-identical, matching Option A.

### 4.4 Two seams: write/DDL type mapping (shipped) vs read-side normalization (the missing Tier-1 layer)

There are **two** type seams, and the prior doc conflated them.

**(1) Write/DDL — `map_zs_type` — SHIPPED.** `DialectBuilder::map_zs_type(zs_type, opts)` (mod.rs:836, SQLite impl dialect.rs:109-144) picks the *storage* column type at CREATE TABLE and `build_create_table_with_fks_for_dialect` (query.rs:507) uses it. Verified SQLite mappings: `boolean`→`INTEGER` (dialect.rs:128), `timestamp`→`TEXT` (132), `json`→`TEXT` (134), `bytes`/`encrypted`→`BLOB` (114/124). This seam is done; it is **not** what makes Tier-1 true.

**(2) Read-side — row→JSON decode — NOT BUILT (B10).** Tier-1 byte-identity is whether `doc.field` is the *same JSON value* on both backends. It is not, because the decoders are **schema-blind**:

| Zeroship type | PG decode (OID) | SQLite decode (storage class) | Same JSON type? |
|---|---|---|---|
| `id` / text | String (`v8_bridge.rs:500`) | String (`mod.rs:1556`) | ✅ yes |
| `int` / `version` / `count` | Number (`v8_bridge.rs:387/392`) | Number (`mod.rs:1551`) | ✅ yes |
| **`timestamp` / `created_at`** | **Number, unix-ms** (`v8_bridge.rs:425`) | **String `'YYYY-MM-DD HH:MM:SS'`** (`CURRENT_TIMESTAMP`, dialect.rs:150; decoded `mod.rs:1556`) | ❌ Number vs String |
| **`boolean`** | **Bool** (`v8_bridge.rs:377`) | **Number 0/1** (`mod.rs:1551`) | ❌ Bool vs Number |
| **`json` / `jsonb`** | **parsed object/value** (`v8_bridge.rs:464/472`) | **String** (`mod.rs:1556`) | ❌ object vs String |
| **`bytes` / encrypted** | **`null` — live bug** (no BYTEA/oid-17 arm; falls to `_ → try_get::<String>` → err → Null, `v8_bridge.rs:500-503`) | **Array-of-byte-ints** (`mod.rs:1557-1563`) | ❌ null vs Array |
| `numeric` | Number-or-String (`v8_bridge.rs:485-496`) | f64 Number / Text (`mod.rs:1554/1556`) | ⚠️ both Number for typical values; precision Tier-2 |

The biggest hidden violation is `created_at`: it is on **every** row, and PG gives a Number while SQLite gives a non-ISO String. A deep-equal fails on row one.

#### The canonical wire contract

Tier-1 needs one agreed JSON shape per type that **both** backends emit. **Default = "match what shipped PG already produces"**, because PG is the production backend and its decode output is the de-facto contract the SDK consumes today; this minimizes churn on the launched path and confines most new code to the SQLite arm.

| Zeroship type | Canonical wire shape | PG action | SQLite action |
|---|---|---|---|
| `timestamp` | **Number, unix-ms** (PG today, `v8_bridge.rs:425`) | none | normalizer parses `'YYYY-MM-DD HH:MM:SS'` (and SDK-written ISO strings) → unix-ms |
| `boolean` | **Bool** (PG today, `v8_bridge.rs:377`) | none | normalizer maps Integer `0/1` → `Bool`; **+ write-path fix below** |
| `json`/`jsonb` | **parsed value** (PG today, `v8_bridge.rs:464`) | none | normalizer `serde_json::from_str` the TEXT cell (no JSON1 needed at read) |
| `bytes`/encrypted | **base64 String** | **fix the live bug**: add oid-17 BYTEA arm → base64 | normalizer maps the byte array → base64 |
| `numeric` | Number | none | normalizer coerces Text/Real → Number; precision delta is Tier-2 |
| `id`, `int`, `version`, `count` | String / Number | none | none (already identical) |

(Canonical timestamp and bytes shapes are the only genuinely open choices — see §11 Q8/Q9.)

#### Where the normalizer lives + how it gets the declared type

A new schema-typed post-pass, e.g. `normalize::normalize_row_on_read(&schema, collection, &mut row)`, modeled directly on the **already-shipped** `mask_pass::wrap_row_on_read` (mask_pass.rs:388) and its driver `apply_mask_wrap_on_read` (crud/mod.rs:2120-2135). That driver already does exactly the three things the normalizer needs:

1. fetches the declared schema — `context::schema_for(app_id, collection)` (crud/mod.rs:2125, same call the find dispatcher already makes at crud/mod.rs:315);
2. early-returns when the schema is absent / trivial;
3. mutates each row in place per field def.

So the normalizer slots into the CRUD read chain alongside encryption + mask passes (crud/mod.rs:345-357), keying off each field's declared `type` in the schema. **It is a post-decode pass, not a change to the raw decoders** — the raw decoders stay storage-class/OID driven; the normalizer is the schema-aware layer on top. (The one raw-decoder change is the PG BYTEA arm, because a lost byte string can't be recovered by a post-pass.)

**Symmetry note:** because the canonical shape = PG's current output, the normalizer is *effectively a no-op on the PG arm* for everything except `bytes`. But it must still run on PG so the contract is enforced in one place and a future canonical-shape change (e.g. Q8 picking ISO strings) is a single edit. On the SQLite arm it does the real coercion work.

**Coupling to B9:** the SQLite generic read path doesn't exist yet (B9). The normalizer is therefore built *as part of* standing up that SQLite read+decode path, not bolted onto an existing one — the SQLite CRUD decoder is schema-typed from day one.

#### Boolean write-path fix (independent of decode)

`value_to_param_inner` binds a `Bool` as the text `"true"`/`"false"` (query.rs:4356). PG accepts those as boolean input, but against SQLite's INTEGER affinity `"true"` is not a numeric literal, so it is **stored as TEXT `"true"`**, not `1` — corrupting the column before any read happens. Fix: emit `"1"`/`"0"` for booleans in `value_to_param_inner`. PG's boolean input accepts `'1'`/`'0'` too, so the single change is correct for both backends (no dialect branch needed). The read-side normalizer then maps the stored `0/1` → `Bool`.

### 4.5 Per-app isolation on SQLite — ALREADY DECIDED & IMPLEMENTED (ATTACH-per-file)

**This was an open question in v0; the code already answered it.** `NamespaceManager::ensure_app_schema` (sqlite/mod.rs:521) does **ATTACH-per-file**: it computes `<db_dir>/zs-<app_id>.sqlite` (sqlite/mod.rs:532), attaches it under the alias `<app_id>` via `self.session.attach(app_id, &path_str)` (sqlite/mod.rs:544), and guards with an `app_id_cache` for idempotency (sqlite/mod.rs:526, 546-548). Duplicate-ATTACH errors are treated idempotently (sqlite/mod.rs:560-564). The cross-app-FK rejection (`cross_app_fk::reject_cross_app_fk`, bootstrap.rs:133) is the policy that makes file-per-app isolation safe — and it already runs on both backends.

So the design is option (c)+(a) combined: one file per app, ATTACHed under a schema-like alias — which is the closest possible structural mirror of PG's per-app schema. **No decision needed; do not re-litigate.** The SQLite register pipeline (§4.2 Path B) must call `ensure_app_schema` exactly as PG bootstrap calls it (bootstrap.rs:215-216 already does, generically). The audit write **already** alias-qualifies via `self.quote_ident(app_id)` (sqlite/mod.rs:1067), so §4.3's table reference is correctly scoped today; the new provisioning DDL must alias-qualify the same way.

### 4.6 Collation (Tier 3 → push toward Tier 1 where cheap)

PG default collation is locale-aware; SQLite default is `BINARY`. `ORDER BY name` can differ. Mitigation: force a consistent collation on text columns (SQLite `COLLATE NOCASE`/`BINARY` to match PG's behavior for the common case) at CREATE TABLE time, so sort order matches for ASCII. Document the Unicode-collation edge as Tier 3.

### 4.7 Deterministic ordering (Tier 1) — the query builder emits engine-dependent order today

Three ordering divergences make ordered/multi-row results differ even when the *rows* are identical. All three are in the shared query builder and fixable by emitting more-explicit SQL on both backends:

1. **NULL sort placement.** `build_order_by` emits bare `col ASC|DESC` with **no `NULLS FIRST/LAST`** (query.rs:4318, 4341). PG defaults ASC→NULLS LAST, DESC→NULLS FIRST; SQLite is the opposite. Any ordered query over a nullable column diverges. **Fix:** emit explicit `NULLS LAST` (ASC) / `NULLS FIRST` (DESC) on both backends, matching PG's defaults. SQLite has supported explicit `NULLS FIRST/LAST` since 3.30 (2019); the bundled amalgamation is well past that. This was mis-filed under Tier-3 "float/NULL value edge cases" (§6) — it is a *sort-placement* issue, not a value-precision one. Now Tier-1.

2. **Unsorted multi-row `find` has no `ORDER BY`.** `build_find` appends `ORDER BY` only when the caller supplies a sort (query.rs:2240). With none, PG returns heap/scan order and SQLite returns rowid order — different arrays for the same `find()`. **Fix:** always append `id` as the final (tiebreaker) ordering key — `id` is `TEXT PRIMARY KEY` on both, unique and present on every collection (§4.4 / system fields), so `ORDER BY … , id ASC` is a total, identical order on both engines. Tier-1 "find" was overstated; this makes it true.

3. **`distinct` ordering — resolving the Tier-1/Tier-3 contradiction.** `build_distinct` appends `ORDER BY {col}` (query.rs:3733). For text columns that ordering is exactly the §4.6 collation divergence *and* the NULL-placement divergence — so a Tier-1 op was built on two Tier-3 behaviors. **Resolution:** the DISTINCT *set membership* is collation-sensitive only for text case-variants, and forced collation (§4.6) makes it identical for ASCII; the `ORDER BY` gets the same forced `COLLATE` + explicit `NULLS` as (1). With both applied, `distinct` is Tier-1 for ASCII text and numeric/temporal columns, with the full-Unicode-collation edge documented Tier-3 (same caveat as §4.6). No internal contradiction remains.

These fixes live in `query.rs` (`build_order_by`, `build_find`, `build_distinct`) and apply to both backends — they are *not* SQLite-only normalization. The parity matrix asserts identical ordering for fixtures with NULLs, duplicate sort keys, and no explicit sort.

### 4.8 Native transactions on SQLite (B11, Tier 1) — give the session actor a BEGIN/SAVEPOINT/COMMIT path

<!-- Added in this round: B11. P9 made native transactions PG-bound; this is a Tier-1 parity gap that must be closed for env.db.transaction() to work identically on both backends. -->

`env.db.transaction(fn)` is **Tier-1** (§0): the *same* SDK transaction call must behave identically on both backends. (P9 PR 3 removed the standalone `env.db.beginTransaction()` + the `Transaction` v8_class — `transaction(fn)` is the sole transaction entry now.) Today it is PG-bound and SQLite returns `backend_unsupported` (B11):

- The v8_method `transaction_dispatch` (orchestrator/transaction.rs:142) spawns `exec_begin_or_savepoint` (transaction.rs:265); its top-level-BEGIN arm pulls `.as_postgres()?` (transaction.rs:298), then `pg.acquire_dedicated_client()` (transaction.rs:299) opens a *dedicated libpq connection*, runs `BEGIN [ISOLATION LEVEL …]`, and parks the `compio_postgres::Client` in the per-isolate `tx_conn` slot via `install_tx_client` (transaction.rs:318).
- Every CRUD callback inside the transaction routes its SQL through `tx_conn` instead of the pool. There is **no user-facing `.commit()`/`.rollback()`** — P9 PR 3 removed the `Transaction` v8_class; commit/rollback is automatic via the `TxFinalizer` resolve/reject handlers (transaction.rs:110): resolve → `COMMIT` / `RELEASE SAVEPOINT`, throw → `ROLLBACK` / `ROLLBACK TO SAVEPOINT`, all on that PG `Client`. The nested-savepoint state machine (P9 PR 3) emits standard `SAVEPOINT`/`RELEASE`/`ROLLBACK TO` via `run_on_tx_conn` (transaction.rs:363) — engine-agnostic SQL.

**The SQLite arm is feasible without a dedicated connection — route through the session actor.** `SqlExecutor for SqliteBackend` already provides the two primitives:

- `acquire_dedicated_client()` returns a `SqliteSessionHandle` that multiplexes the single actor (sqlite/mod.rs:378-387). On SQLite there is no per-client connection — "the actor IS the only writer" — so the "dedicated client" is just an `Rc` handle to the actor.
- `client_exec(client, sql, params)` runs a statement against that handle (sqlite/mod.rs:398-410).
- The actor **serialises every command** — "BEGIN / INSERT / COMMIT flows through the same single-threaded worker" (sqlite/mod.rs:382-384) — so a transaction's statements are naturally ordered and isolated from other callers on that connection.

**Design:** `exec_begin_or_savepoint` (transaction.rs:265) branches on `as_postgres()`/`as_sqlite()` (the same accessor fork as §4.2 Path B) — the savepoint state machine above it (`transaction_dispatch` + `TxFinalizer`) is already backend-agnostic and unchanged. The SQLite arm:

1. `acquire_dedicated_client()` → a `SqliteSessionHandle`; store it in `tx_conn` as a backend-tagged handle (the `tx_conn` slot becomes an enum over `compio_postgres::Client` | `SqliteSessionHandle`, or the context grows a parallel `sqlite_tx_conn` slot — the enum is cleaner and pre-launch makes it a free change).
2. Run `BEGIN` via `client_exec`. **Isolation level:** SQLite has no `ISOLATION LEVEL` clause; under WAL it is effectively snapshot-isolation for readers + a single writer. The SQLite arm **ignores** the SDK isolation-level argument (or maps it to `BEGIN`/`BEGIN IMMEDIATE`) and the divergence is documented Tier-3 (concurrency, §6) — the *successful-path* behavior (atomic commit, rollback, savepoints) is Tier-1; only the concurrency semantics under contention are Tier-3.
3. **Savepoints:** SQLite supports `SAVEPOINT name` / `RELEASE name` / `ROLLBACK TO name` natively — the same verbs P9's PG state machine emits. The nested-transaction state machine drives `client_exec` with those statements; the savepoint-name bookkeeping is engine-agnostic.
4. The `TxFinalizer` resolve handler (transaction.rs:110) → `client_exec("COMMIT")` (or `RELEASE SAVEPOINT` when nested); the reject handler → `client_exec("ROLLBACK")` (or `ROLLBACK TO SAVEPOINT`). (Unlike PG, there is no connection-close auto-rollback — the actor outlives the `SqliteSessionHandle` — so if the wrapper drops un-settled the SQLite finalizer must *actively* `ROLLBACK`. This is a real behavioral difference the implementer must honor: PG leans on connection-close, SQLite must explicitly roll back.)

**Tier classification:** atomicity, commit/rollback, savepoint nesting, and CRUD-inside-tx are **Tier-1** and get a parity-matrix row (§2). Concurrency-under-contention (PG MVCC vs SQLite single-writer `SQLITE_BUSY`) stays **Tier-3** (§6) — already documented. This row is required; without it `env.db.transaction()` is the one core SDK verb that throws on the SQLite dev backend.

---

## 5. Tier 2 — best-effort features (documented divergence)

- **Vector** (`backend/sqlite/vector.rs`, sqlite-vec): same top-k for well-separated vectors; near-tie ordering may differ from pgvector. Parity matrix asserts set membership, not exact order.
- **FTS** (`backend/sqlite/fts.rs`, FTS5 bm25): different ranking formula than PG `ts_rank`. Same matching docs; order differs. Documented.
- **Geo** (`backend/sqlite/spatial.rs`, haversine flat-scan): precision + order at boundaries; O(n) scan vs PG index. Fine for dev/small-scale; documented perf caveat.

These backend impls already exist (§3 B8) but are **unreachable** until B1–B3 land. The Tier-2 work is the parity-matrix tolerance assertions + the divergence doc — and it cannot run until the pipeline reaches `as_sqlite`.

---

## 6. Tier 3 — documented non-goals

- **Concurrency**: SQLite single-writer serializes writes that PG runs concurrently. A `db.transaction` under contention behaves differently (SQLite `SQLITE_BUSY` / serialized vs PG MVCC), and SQLite has no `ISOLATION LEVEL` clause (the SDK isolation-level argument is ignored on SQLite — §4.8). Document: "SQLite is single-writer; concurrent-write workloads should target Postgres." The transaction's *successful-path* semantics (atomic commit, rollback, savepoints) are **Tier-1** and built in §4.8 (B11) — only the contention/isolation behavior is Tier-3. The parity matrix asserts the Tier-1 successful path on both backends (§4.8).
- **Collation/Unicode sort**: §4.6 — forced collation closes the ASCII case; full Unicode locale collation is Tier 3.
- **Float value precision**: `avg`/division and `t.double` may differ at the last ULP (SQLite f64 vs PG `float8`/`numeric` arithmetic). Tier-2 with tolerance where the SDK exposes the value; Tier-3 for NaN/±infinity sentinels.

> **Moved out of Tier-3 in R3:** "NULL placement" and "default sort order" are *not* value-edge cases — they are sort-placement bugs in the query builder, now Tier-1-via-§4.7. Only float *value* precision and full-Unicode *collation* remain genuine Tier-3 value edges.

---

## 7. pglite removal (B6) — vite-plugin rewire (concrete resolution)

<!-- Rewritten in this round: requirement #2+#3 make this concrete — the always-compiled runtime dispatches a sqlite:/file: URL; no pglite spawn, no DATABASE_URL forwarding, deps removed. -->

This is the concrete B6 resolution, and it is *enabled* by requirements #2 + #3: because the SQLite backend is now always compiled into the runtime (§1.1) and selected by URL scheme (§4.1), the dev server no longer spawns a separate database process — it just hands the runtime a `sqlite:`/`file:` URL and the in-process backend takes over.

**Before:** `dev-db.ts` spawns a pglite instance (WASM Postgres) on a socket and `dev-server.ts` forwards a `DATABASE_URL` pointing at it; the runtime's `compio-postgres` driver connects to pglite. The `@electric-sql/pglite` + `@electric-sql/pglite-socket` deps carry tens of MB of WASM per dev session.

**After:**

- `dev-db.ts`: **deleted, or reduced to "resolve a SQLite file path"** (`.zeroship/dev.sqlite`). No pglite spawn, no socket, no WASM, no port allocation, no readiness polling.
- `dev-server.ts`: forward `sqlite:.zeroship/dev.sqlite` (a SQLite scheme URL, §4.1 table) to the spawned `zeroship` runtime *instead of* a `DATABASE_URL`. The always-compiled runtime parses the scheme and dispatches to `SqliteBackend` (§4.1) — exactly the same code path a SQLite *deploy* uses.
- `package.json`: **drop `@electric-sql/pglite` + `@electric-sql/pglite-socket`.** This is the dependency win that motivated the whole proposal (§1).
- **Escape hatch (not back-compat)**: if `DATABASE_URL` is set, honor it — the dev server forwards *that* URL (`postgres://…` → PG, by the same §4.1 dispatch), so dev-against-real-PG stays available for anyone wanting exact prod parity. This is a forward configuration choice (PG is still a first-class backend selectable by scheme), not a migration shim for a deprecated default, so it does not violate the no-back-compat stance. **Default becomes SQLite.**

Note this is *pure dev wiring* (PR 6) and is **downstream of PR 3** — the SQLite dev backend is only usable once generic CRUD works on SQLite (B9/B10). Removing pglite before PR 3 lands would leave dev with a SQLite backend that can't run `find()`.

---

## 8. Switchable mechanism — selection granularity, grounded in the context model

<!-- Rewritten in this round: requirement #3 demands a precise statement of selection granularity (per-deployment vs per-app). This section settles it against the code, not by assumption. -->

**Selector:** URL scheme — `postgres://`/`postgresql://` → PG; `sqlite:`/`file:`/`:memory:`/bare path → SQLite. Dispatched in `init_pool_async` (§4.1), the existing config path, no new knob. (libSQL uses exactly this: a scheme distinguishes local-file from remote, one API over both — Turso libSQL URLs.)

**Both compiled in (requirement #2):** the `sqlite` feature is removed (§1.1), so runtime/CLI always hold both `BackendHandle` arms. The arm is chosen at *backend-construction* time. There is no `hardening` register-trait dual-gate to mind (§1.2).

### Selection granularity: **per-deployment is the primary model; per-app is architecturally possible**

This is the precise answer requirement #3 needs, and the code settles it:

**Where the backend handle lives — per-isolate context.** The `BackendHandle` is stored in `IsolateDbContext.backend` (context.rs:260, `backend()` accessor context.rs:322). That context is **per-isolate**: the module doc says "Each isolate (worker thread) carries one context" (context.rs:25-26). Combined with the platform invariant **"V8 per thread, one isolate per app"** (AGENTS.md), the context — and therefore the chosen backend — is effectively **per-app**. So *structurally*, nothing stops two apps on the same worker process from resolving to different backends: each app's isolate has its own `backend` slot.

**Where the URL comes from — one configured URL per worker, today.** The worker reads a single `db_url` at startup (`init_cache(max_size, db_url)`, worker/src/cache.rs:27-37) and constructs one `DbPlugin::new(url)` per app from that *same* URL (cache.rs:40-45). The CLI does the same (`cli/main.rs:106`). So in the **current wiring**, every app on a worker gets the identical URL → identical backend. `DbPlugin::register` then stamps that URL into the per-isolate context via `set_db_url` (lib.rs:294); the comment there is explicit: *"in today's production each worker thread hosts a single DB URL, so the `different` branch is a no-op. It exists for the multi-URL-per-thread case"* (lib.rs:286-292) — and on a URL change it `clear_pool()`s so the next CRUD call rebuilds against the new URL.

**Conclusion (stated precisely):**

- **Per-deployment is the primary, intended model.** A worker/CLI process is configured with one connection string; that scheme picks the backend for *every* app it serves. The multi-tenant platform configures `postgres://…`; an edge / single-tenant / dev process configures `sqlite:`/`file:…`. This is the model §0 requirement #3 and the "SQLite = dev + small-scale, PG = platform" framing assume. It needs **no new code** beyond §4.1's scheme dispatch — the single-`db_url` wiring already enforces it.
- **Per-app is *possible* by construction but not wired.** Because `backend` is a per-isolate (per-app) slot and `DbPlugin::register` already handles the "two `DbPlugin` instances with distinct URLs on the same thread" case (lib.rs:286-296, `clear_pool` on change), one worker *could* serve PG apps and SQLite apps simultaneously — but only if something feeds per-app URLs into `init_cache`/`DbPlugin` construction. That plumbing (per-app DB-URL in the deploy manifest / control-plane app record, threaded to the worker) **does not exist today** and is **out of scope** for this proposal. The design neither requires nor forecloses it: the §4.1 dispatch is per-`init_pool_async`-call (i.e. per-isolate-boot), so it already works per-app the moment a per-app URL source exists.

**Decision (user 2026-05-24): ship per-deployment** — the worker's configured connection string picks the backend for that process. Per-app stays a latent capability the architecture supports, gated only on a per-app-URL config source — a future, separate change, out of scope here. This matches how `sqlx`/SeaORM/Diesel are deployed: one process picks its backend from config; per-tenant backend routing is an application-level concern layered on top, not a property of the connection layer.

### What the SDK layer sees

**The v8_class + SDK layers don't change** — they call into the plugin's op layer, which dispatches by backend arm (same as Prisma: identical query API across providers — Prisma docs). But the trait abstraction does **not** transparently cover CRUD today (correcting a prior overstatement): the generic CRUD execute+decode path is PG-pool-pinned (B9 — `run_sql` returns `compio_postgres::Row`, exec.rs:43-73), `SqlExecutor for SqliteBackend` has no read method (sqlite/mod.rs:375-411), and native transactions are PG-bound (B11, §4.8). The FTS/vector/geo capability traits *do* dispatch cleanly (`as_sqlite()` arms, crud/mod.rs:1464). So there are **three** places the abstraction doesn't transparently cover both backends: the register pipeline (§4.2, PG-pinned lock/audit), the generic CRUD read/write path (§4.4/B9), and native transactions (§4.8/B11). Closing all three is the implementation work of PRs 2–3 (+§4.8).

---

## 9. PR sequence (after P9 + P6a land)

<!-- Reworked in this round: the old "PR 1 — backend selection + dual-feature build" splits into PR 0 (remove the sqlite feature) + PR 1 (runtime URL-scheme dispatch); a new PR 4 covers native transactions (§4.8/B11). -->

0. **PR 0 — remove the `sqlite` Cargo feature; both backends unconditional** (§1.1, B5): promote `rusqlite`/`flume`/`sqlite-vec` to unconditional deps; delete every `#[cfg(feature = "sqlite")]` (~30 sites: `backend/mod.rs:81-90/1771`, `crud/mod.rs:1462/1631/1771`, `crud/mask_policy.rs`, `crud/mask_drift.rs`, `cross_app_fk.rs`, `context.rs:308`, `lib.rs:151-153/381`) and un-gate the `BackendHandle::Sqlite` arm. Pairs naturally with the concurrent `hardening`-removal (§1.2) so the `RegisterBackend` trait collapses to one definition. **No behavior change** — it only un-gates already-written code; both backend test gates must stay green. This is the precondition for everything else: B5 dissolves, and PR 1's dispatch has a SQLite arm to reach. *(If the `hardening` removal lands separately, PR 0 still stands alone — it's purely the `sqlite` gate.)*
1. **PR 1 — runtime URL-scheme backend dispatch** (§4.1, resolves B4): add `backend_for_url(&url)` scheme parser (postgres/postgresql → PG; sqlite/file/`:memory:`/bare-path → SQLite; unknown → typed error); branch `init_pool_async` (lib.rs:544) on it; promote `context::set_sqlite_backend` to an un-gated production installer (replacing the test-only `set_sqlite_backend_for_tests` seam); add the `SqliteBackend::open(path)` production constructor (mints the session actor + boot PRAGMAs). The generic CRUD path *does* assume `ctx.pool` (B9, confirmed) — PR 1 makes the SQLite arm install cleanly and surface an **explicit typed "no SQLite CRUD path yet" error** (not a panic), leaving the actual CRUD path to PR 3. No CLI feature change needed (PR 0 removed the flag).
2. **PR 2 — SQLite register pipeline** (B1, B1a, B2, B3, B7): the §4.2 **Path B** fork. Branch `exec_register_model` (mod.rs:162) on the `as_postgres()`/`as_sqlite()` accessors; write `run_sqlite_pipeline` (in-process lock, no `'p`; row writes via the existing `AuditWriter` impl; apply via `execute_batch` + `DialectBuilder::build_create_index(spec, false)`). **The two reroutings that make plan/validate shareable** (§4.2): (a) re-point `plan`'s DDL emission from `build_create_table_with_fks` to `build_create_table_with_fks_for_dialect` (query.rs:507) with the backend dialect; (b) move `validate`'s audit write (validate.rs:108-109) off the PG free fn onto `AuditWriter::write_audit_row` and re-bound `validate` on `AuditWriter` instead of `PgSqlExecutor`. **Audit alignment** (§4.3): add SQLite provisioning DDL + `next_schema_version`, and rename the SQLite table `__zs_migrations` → `__zeroship_migrations` with the `applied_at`/`parent_id` columns added (free pre-launch). `ensure_app_schema` ATTACH is reused unchanged (§4.5). `RegisterBackend` is now a single trait (post-§1.2) always requiring `EncryptedColumn`, which `SqliteBackend` already satisfies (sqlite/mod.rs:1932) — **no `hardening` cfg gate to mirror**.
3. **PR 3 — SQLite generic CRUD read/write path + read-side normalization** (B9, B10): the value-correctness core, **the largest PR**. (a) Build the SQLite generic read+decode path so `find`/`insert`/`update`/`delete`/`upsert`/`count` route to `SqliteBackend` instead of the PG-pool-pinned `exec_query`/`run_sql` (exec.rs:43-116) — dispatch on the backend handle the way the FTS arm already does (crud/mod.rs:1464). (b) Add the schema-typed `normalize_row_on_read` post-pass (§4.4), modeled on `apply_mask_wrap_on_read` (crud/mod.rs:2120), run on both arms; canonical wire shapes per the §4.4 contract table. (c) Fix the PG BYTEA→`null` live bug (add oid-17 base64 arm, v8_bridge.rs:500). (d) Fix the boolean write-path: `value_to_param_inner` emits `"1"`/`"0"` (query.rs:4356). (e) The §4.7 ordering fixes (`build_order_by` NULLS, `build_find` implicit `id` tiebreaker, `build_distinct` COLLATE+NULLS). *Each of (a)–(e) is independently testable; if PR 3 is too large, split (a)+(b) from (c)–(e).*
4. **PR 4 — native transactions on SQLite** (§4.8, B11): branch `exec_begin_or_savepoint` (orchestrator/transaction.rs:265) on `as_postgres()`/`as_sqlite()` — the `transaction_dispatch` + `TxFinalizer` + savepoint state machine above it (P9 PR 3) stays unchanged, only the begin/savepoint executor and `tx_conn` type gain a SQLite arm; SQLite arm acquires a `SqliteSessionHandle` and drives `BEGIN`/`SAVEPOINT`/`RELEASE`/`ROLLBACK TO`/`COMMIT` through `client_exec`; widen the `tx_conn` slot to an enum over `compio_postgres::Client | SqliteSessionHandle` (free pre-launch); make the `TxFinalizer` actively `ROLLBACK` on SQLite (no connection-close auto-rollback). Ignore/`map` the isolation-level argument on SQLite (Tier-3 concurrency, §6). *Independent of PR 2/3 in code (different dispatch site) but only meaningfully testable once SQLite CRUD works (PR 3) — sequence after PR 3.*
5. **PR 5 — parity test matrix + divergence doc** (B8): the matrix harness asserting the §4.4 contract + §4.7 ordering + §4.8 transaction successful-path on both backends; `sqlite-divergences.md` enumerating the Tier-2/3 differences (incl. the number-vs-Date-wrapped `created_at`, and SQLite's ignored isolation level, so devs aren't blindsided). The matrix is the regression guard for PRs 3–4.
6. **PR 6 — vite-plugin rewire** (B6, §7): SQLite dev backend via `sqlite:` URL; remove pglite + the `@electric-sql/pglite*` deps; `DATABASE_URL` escape hatch. Downstream of PR 3 (dev SQLite must run CRUD first).

Each PR green on both backend gates before the next. **PR 2** (new register pipeline) and **PR 3** (new CRUD read path + normalization) are the two large ones — both are *new code*, not generalizations. PR 0 is large in *line count* (deleting ~30 cfg gates) but mechanically trivial and behavior-preserving. The prior plan folded all of B8–B10 into a single "matrix + close gaps" PR; that under-counted the work by an order of magnitude — closing the Tier-1 divergences *is* building a schema-typed read layer (B10) on top of a SQLite CRUD path that doesn't exist yet (B9), plus a native-transaction path (B11).

---

## 10. Risks

| Risk | Mitigation |
|---|---|
| PR 2 mis-scoped as "generalize the pipeline" (the v0 framing) | §4.2 makes explicit it is a **new forked pipeline** (Path B), not a generic relaxation; the trait abstraction is the *obstacle* here, not the lever |
| Path A chosen, GAT refactor fights the trait solver | The Open Q5 rustdoc (mod.rs:889-898) already documents this hazard; Path B avoids it. If A is chosen, budget for HRTB consumer-site churn |
| SQLite audit table diverges from PG (today it's `__zs_migrations`, subset columns) | PR 2 renames it to `__zeroship_migrations` + adds `applied_at`/`parent_id` (§4.3, free pre-launch); parity matrix then asserts identical row shape + `schema_version` monotonicity on both backends |
| Parity matrix is large to build | It's the contract — non-negotiable; build incrementally per feature, Tier 1 first |
| **Tier-1 mis-scoped as "write a matrix" (the R2 framing)** | R3 split out **B9** (SQLite generic CRUD path doesn't exist) + **B10** (schema-typed read normalizer doesn't exist) as the real Tier-1 value-correctness work; PR 3 builds them, the PR 5 matrix only *verifies* them |
| **Silent type divergence ships undetected** (`created_at` Number-vs-String, `boolean` Bool-vs-Number, `json` object-vs-String, `bytes` null-vs-Array) | These are the §4.4 read-decode bugs; the normalizer closes them and the matrix asserts per-field. Until PR 3 lands, every SQLite `find` returns wrong-typed system fields — so PR 3 gates the SQLite dev backend (PR 6) |
| **Canonical-wire choice wrong** (timestamp/bytes, §11 Q8/Q9) | Default = match shipped PG output → near-zero churn on the production backend; if Q8 picks ISO strings instead, the normalizer runs the conversion on *both* arms (one code path), not a PG-decoder rewrite |
| **`env.db.transaction()` throws on SQLite** (B11) — P9 left it PG-bound | §4.8 gives the SQLite session actor a BEGIN/SAVEPOINT/COMMIT path via `client_exec`; PR 4; without it the core transaction verb is a hard error on the dev backend |
| **Binary-size / build-time cost of always-compiling SQLite** (§1.1 trade) | Accepted: one embedded SQLite amalgamation + sqlite-vec per binary, not per-app; the platform already bundles V8. The PG-only `--no-default-features` slice is *not* preserved (Q2) — if a hard size ceiling later emerges on a PG-only edge image, re-introduce `sqlite` as a default-on optional feature (the §1.1 alternative) |
| Tier-2 divergence surprises a creator who dev'd on SQLite then deployed to PG | The divergences doc + the "SQLite = dev/small-scale" framing; default dev backend matches the *intended* deploy target where possible. (Prisma documents the same class of cross-provider gaps — enums, migrations, schema namespaces — as explicitly non-portable; our Tier-2/3 doc is the analogue.) |
| **Per-app backend selection assumed by a future caller before it's wired** | §8 states per-app is *latent* (the `backend` slot is per-isolate) but **not wired** — no per-app URL source exists. Document it as out-of-scope so no one builds against it prematurely; the §4.1 dispatch already supports it the moment a per-app URL config lands |
| Concurrency semantics bite a real app | Documented Tier 3; SQLite deploys are for low-concurrency by definition |

---

## 11. Needs user decision

Only genuinely open questions where the code does *not* settle the answer and the answer changes the plan. Each has a crisp default.

| # | Question | Default |
|---|---|---|
| Q1 ✅ **DECIDED** | **register_model strategy: Path B (fork) or Path A (split trait hierarchy + GAT)?** (§4.2) | ✅ **DECIDED (user 2026-05-24): Path B — fork at `exec_register_model` (mod.rs:162), share plan/validate via `DialectBuilder` + `AuditWriter`.** The two backends genuinely diverge in lock (pooled-client `LockGuard<'p>` vs in-process `'static` guard) + apply (`pool_exec` vs `execute_batch`), so a unified pipeline (Path A) would be a thin shell over two different bodies *while* reopening the deliberately-deferred GAT/HRTB lock-lifetime fight (Open Q5, mod.rs:889-898) to abstract a lifetime SQLite doesn't even have. Path B shares only what already has trait seams. Revisit Path A only if a 3rd backend appears. |
| Q2 ✅ **DECIDED** | **Both backends always compiled in (remove the `sqlite` feature), or keep a `--no-default-features` PG-only slice?** (§1.1) | ✅ **DECIDED (user 2026-05-24): remove the `sqlite` feature entirely — SQLite is an unconfigurable default.** Both backends unconditional; no opt-out / no `--no-default-features` PG-only slice. Runtime picks by URL scheme (§4.1). Accepted cost: bigger binary + one C-compile of the SQLite amalgamation per clean build (one embedded SQLite, not per-app). "Unconfigurable" = the backend's *presence* is unconditional; which backend an app *uses* stays runtime-config (Q11). Mirrors the `hardening`-removal (§1.2, now landed on branch `refactor/remove-hardening-flag`). |
| Q3 | Dev default: SQLite always, or SQLite-unless-`DATABASE_URL`? | SQLite unless `DATABASE_URL` set (escape hatch to real PG, §7). |
| Q4 | Does the parity matrix run in default `cargo test`, or a dedicated gate? The PG leg needs a live listener (`Pool::connect`, lib.rs:550). | Dedicated CI gate with a compose/CI Postgres step. **No special Cargo features needed** (the `sqlite` feature is gone, §1.1) — `cargo test -p zeroship-plugin-db` builds both backends; the gate exists for the *live-Postgres* dependency, not a feature toggle. (Lens R4 will pressure-test the CI shape.) |
| Q5 | Forced collation: match PG locale, or settle for BINARY/NOCASE ASCII parity? (§4.6) | NOCASE/BINARY ASCII parity; Unicode locale = Tier 3 documented. |
| Q6 | CDC parity (`live`): `preupdate_hook` event shape vs WAL — assert in matrix? | Yes — `live` is Tier 1; event shape must match. (`backend/sqlite/cdc.rs` exists but is unreachable until B1–B3; §3 B8.) |
| Q7 | **Schema-history table: align SQLite to PG's `__zeroship_migrations` name+columns (Tier 1), or keep the lean `__zs_migrations` (Tier 2)?** (§4.3) | **Align to identical** — rename + add `applied_at`/`parent_id`; free pre-launch, matches Option A "as close to PG as possible". Tier-2-lean is the only reason to decline. |
| Q8 ✅ **DECIDED** | **Canonical timestamp wire shape** (§4.4): unix-ms **Number** or ISO-8601 **String**? | ✅ **DECIDED (user 2026-05-24): unix-ms Number.** Already the SDK's published read contract (`deleted_at: number`, types.ts:134) and what PG emits today (`v8_bridge.rs`, µs + 2000-epoch → unix-ms) — so the production backend *and* the `@zeroship/db` read types are untouched; only the SQLite normalizer converts (parse its `strftime` ISO text → unix-ms). The write path stays permissive (accepts `Date`/ISO, validate.ts:67); read is canonical Number. |
| Q9 ✅ **DECIDED** | **Canonical bytes wire shape** (§4.4): base64 String, hex String, or array-of-ints? | ✅ **DECIDED (user 2026-05-24): base64 String.** Already the SDK's bytes convention on the encryption/mask path (types.ts:619 — "plaintext arrives base64-encoded"), so the generic `t.bytes()` path matches it → one bytes convention SDK-wide. Both backends need the normalizer regardless: PG adds the missing `oid 17 (bytea)` → base64 arm (**fixes the live `null` bug**); SQLite emits base64 instead of the current array-of-ints (`mod.rs:1557`). |
| Q10 | **Is the read-side normalization layer (B10) + SQLite CRUD path (B9) in scope for this proposal, or a prerequisite PR before it?** | **In scope — they ARE PR 3**, the value-correctness core (§9). They are not register_model work (PR 2); they sit on the CRUD read/write path. Splitting them out would leave the SQLite backend unable to return correctly-typed CRUD rows, making every other Tier-1 claim vacuous. |
| Q11 ✅ **DECIDED** | **Selection granularity: per-deployment, or also wire per-app now?** (§8) | ✅ **DECIDED (user 2026-05-24): per-deployment.** The worker's configured connection string picks the backend for the whole process. Per-app stays *latent* (the `backend` slot is per-isolate; `DbPlugin::register` already clears the pool on URL change, lib.rs:286-296) but needs a per-app-URL config source that doesn't exist — deferred to a separate future change, not in scope here. |
| Q12 | **Native transaction parity (B11): in scope here, or deferred?** (§4.8) | **In scope — PR 4.** `env.db.transaction()` is Tier-1; leaving it `backend_unsupported` on SQLite means the core transaction verb throws on the dev backend. The SQLite session actor already has `acquire_dedicated_client`/`client_exec` (sqlite/mod.rs:378/398) — the work is a dispatch branch + savepoint state machine, not a new primitive. |

### Resolved by code review (retired from v0's open list)

- **~~Per-app isolation on SQLite (file-per-app / table-prefix / ATTACH)~~** — **answered: ATTACH-per-file**, already implemented in `ensure_app_schema` (sqlite/mod.rs:521-548): `zs-<app_id>.sqlite` ATTACHed under an `<app_id>` alias with `app_id_cache` idempotency. v0 listed this as open and guessed file-per-app with "cross-app FK impossible" — the shipped mechanism is ATTACH (its schema-like alias *is* per-file), and cross-app FK is rejected by policy on both backends (`cross_app_fk::reject_cross_app_fk`, bootstrap.rs:133). Do not re-open or rebuild this.
- **~~"register_model orchestrator is PG-only because it pulls `&PostgresBackend`"~~** — refined: the *entry* pulls `as_postgres()` (mod.rs:162), but the *pipeline* is already generic over `RegisterBackend` (mod.rs:197). The true blocker is the PG-pinned trait bound + PG-pinned body + audit *provisioning* gap + lock lifetime (§3 B1/B1a/B2/B3).
- **~~SQLite `RETURNING` for audit-row insert vs `last_insert_rowid()`~~** (v1's Q7) — **answered: `DialectBuilder::last_insert_rowid_sql()`** already provides the hook — `Some("SELECT last_insert_rowid()")` on SQLite, `None` on PG (mod.rs:847-854, sqlite/mod.rs:1123). The shipped SQLite `write_audit_row` writes a terminal row and needs neither (mod.rs:670-681). No user input.
- **~~"Build a Connection-based SQLite audit-write path / an `AuditStore` trait"~~** (v1's §4.2/§4.3) — **answered: the `AuditWriter` trait (mod.rs:660) already exists and is implemented for SQLite** (sqlite/mod.rs:1054). Only provisioning DDL + `next_schema_version` remain (B2); the write seam is built. Do not introduce a new audit trait.
- **~~"Index DDL dialect (CONCURRENTLY) is unstarted work"~~** (v1's B7) — **answered: `DialectBuilder::build_create_index(spec, online)`** ships the dialect split, SQLite impl no-ops `online` (mod.rs:826-829, sqlite/mod.rs:1111). PR 2 wires it; nothing to design.
- **~~"`distinct` is Tier-1 but built on `ORDER BY col` whose collation/NULL order is Tier-3 (contradiction)"~~** — **resolved by design, not user decision (§4.7):** force `COLLATE` (§4.6) + explicit `NULLS` on the `distinct` ordering → Tier-1 for ASCII/numeric/temporal, full-Unicode-collation edge documented Tier-3. Not an open question.

### Surfaced by R3 code review (newly scoped, not open questions)

- **B9 — generic CRUD execute+decode is PG-pool-pinned.** `exec_query`/`exec_mutation`/`exec_count` → `run_sql` return `compio_postgres::Row` and pull `context.pool()` (exec.rs:43-116); `SqlExecutor for SqliteBackend` has no read method (sqlite/mod.rs:375-411). SQLite generic CRUD is *not implemented*, not merely unreachable. Scoped into PR 3(a). No user decision — it's required for any Tier-1 CRUD.
- **B10 — schema-typed read normalization is absent.** Both decoders are schema-blind (OID on PG, storage-class on SQLite); the canonical wire contract + a `normalize_row_on_read` post-pass (modeled on `apply_mask_wrap_on_read`, crud/mod.rs:2120) is the fix. Scoped into PR 3(b). The only *choices* it raises are the canonical shapes (Q8/Q9).

### Surfaced by this round (requirements #2+#3, verified against code)

<!-- Added in this round. -->

- **B11 — native `env.db.transaction()` is PG-bound.** `exec_begin_or_savepoint` (orchestrator/transaction.rs:265) pulls `as_postgres()?` (transaction.rs:298) then `acquire_dedicated_client()` (transaction.rs:299) + `BEGIN` on a `compio_postgres::Client` parked in `tx_conn` (transaction.rs:318). SQLite returns `backend_unsupported`. The savepoint/commit/rollback state machine above it (`transaction_dispatch` + `TxFinalizer`, P9 PR 3) is already backend-agnostic. Tier-1 gap; fix in §4.8 (PR 4) via the SQLite session actor's `client_exec` — the SQLite primitives already exist (sqlite/mod.rs:378/398), no new native surface. Scoped, not open.
- **Binary shape resolved (§1.1).** Requirement #2 ("both in one binary") + the no-back-compat / one-binary stance → **remove the `sqlite` Cargo feature** (PR 0), promoting `rusqlite`/`flume`/`sqlite-vec` to unconditional. Mirrors the concurrent `hardening`-removal (§1.2). Recommendation, filed as Q2; the only counter is a hard PG-only-image size ceiling (not stated).
- **Selection granularity resolved against the code (§8).** The `backend` slot is **per-isolate (= per-app)** (context.rs:25-26, AGENTS.md "one isolate per app"), but the URL is fed **once per worker** (`init_cache`, cache.rs:27-45; lib.rs:286-292 confirms "one DB URL per worker thread today"). → **per-deployment** is the primary model and needs no new code; **per-app** is latent (architecturally supported, not wired). Filed as Q11.

---

## 12. Status

- **v3**: R1 re-grounded the blocker inventory in code; R3 corrected the Tier classification — the read-decode divergences (B10), the missing SQLite CRUD path (B9), and the §4.7 ordering fixes are the scoped Tier-1 value-correctness core.
- **v4 (this draft)**: folded in the two new hard requirements (2026-05-24) — **both backends in one binary** (§1.1: remove the `sqlite` feature; assumes the concurrent `hardening`-removal, §1.2) and **runtime config-driven backend selection** (§4.1: URL-scheme dispatch in `init_pool_async`). Re-grounded selection granularity against the per-isolate context model (§8: per-deployment primary, per-app latent). Moved **B4/B5 to resolved-by-architecture**; added **B11** (native transactions PG-bound, §4.8) as a new Tier-1 gap. Re-sequenced PRs (PR 0 feature-removal + PR 1 dispatch + PR 4 transactions). In `.claude/worktrees/sqlite-pg-parity`; not committed to main.
- **v4.1 (this update)**: the design worktree had been forked at `2b1fdacd` (P9 **PR 2**), so v4's transaction sections were grounded on **pre-P9-PR3 code** — they cited `begin_transaction_dispatch` (which P9 PR 3 *deleted*) at stale line numbers. Refreshed the worktree to current `main` (the P6a merge `262e78cc`) and **re-grounded B11 / §4.8 / §9-PR4 / §11** against the shipped architecture: `transaction_dispatch` (transaction.rs:142) + `exec_begin_or_savepoint` (transaction.rs:265) + the **already-shipped** `TxFinalizer`/`MAX_SAVEPOINT_DEPTH` savepoint state machine — so the SQLite work is a backend arm on `exec_begin_or_savepoint` + a `tx_conn` enum, not building the state machine. **Locked two user decisions (2026-05-24): Q2** (remove the `sqlite` feature — unconfigurable default) and **Q11** (per-deployment selection).
- **Next**: continue critic/reviser loop (next lens).
- **All sign-off questions DECIDED (user 2026-05-24): Q1 = Path B (fork), Q2 = remove `sqlite` feature (unconfigurable default), Q8 = unix-ms Number, Q9 = base64 String, Q11 = per-deployment.** Design is decided modulo the in-flight Codex + Opus reviews.
- **Then**: fold in Codex/Opus review findings → PR dispatch (PR 0–6, §9), gated behind P9 + P6a settling (both now landed on `main`).
