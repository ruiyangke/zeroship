# zero-migrate — Consolidated Redesign Plan

**Status:** draft (uncommitted). **Date:** 2026-07-12.
**Basis:** two independent harsh reviews (codex 16/100, fable 18/100 — both in
`docs/reviews/2026-07-12-zero-migrate-arch-{codex,fable}.md`), converging on the
same structural findings. This plan adopts **fable's spine** with **codex's
independent confirmation**, and one locked naming decision.

---

## Locked decisions

1. **Spine = fable's:** 4 Rust crates + 2 npm packages. Reject codex's ~11-crate
   split — it re-commits the exact sin both reviews flagged (splitting a crate for
   a consumer that isn't in the repo, which is how `zero-migrate-schema` became
   dead weight). Cut crates only where a consumer exists.
2. **One brand, long form:** `zero-migrate` everywhere — crate names, npm, env,
   SQL, wire brands. **No `zmg` short token.** fable's redesign used `zmg`; every
   `zmg`/`zmg-`/`ZMG_`/`zmg:` in its doc is translated to the long form here.
   Accept the verbosity; it buys the single name both reviews said the project
   most lacks. Only abbreviate if a hard length limit forces it (none found —
   Postgres' 63-char identifier limit leaves ample room for `__zero_migrate_*`).
3. **The 4 verified CRITICALs are must-fix regardless of redesign scope** (each
   independently confirmed against source — see below).

---

## The 4 verified CRITICALs (confirmed against source this session)

| # | Flaw | Proof | Impact |
|---|---|---|---|
| C1 | **MySQL host path routes into the Postgres executor** | `host/index.ts:33` advertises `{kind:"mysql"}` → `openMysqlSession` cast `as HostDriver`; `node/lower.rs` lowers `SqlDialect::Mysql`; `executor::apply<D: PgSession>` unconditionally builds `PostgresBackend` + issues `pg_advisory_lock(hashtext($1))`; real `MysqlBackend` is `#[cfg(feature="v8-host")]` (removed feature). Zero MySQL tests. | Constraint 4 unmet on the only shipped path; `apply({driver:{kind:"mysql"}})` is a typed lie that errors on first statement. |
| C2 | **Embeddability seam is dead API** | `AuthoringHost`/`RecorderPlatform`/`JsDriverHost` have **zero `impl` blocks**; only consumers are `frontend/` (v8-host-gated, dead). No `RuntimeHost` type exists at all. | Constraint 2 (runtime customization — *the reason for extraction*) is orphaned trait declarations. |
| C3 | **~12.5k uncompilable lines + dead-code detection disabled to hide them** | `lib.rs:81` `allow(dead_code, unused_imports)` crate-wide; its justifying comment ("default native-pg-on build stays strict") is false — **`native-pg` is not a declared feature**, so the allowance is permanent. ~8.7k lines behind undeclared `v8-host` + ~3.8k behind undeclared `native-pg` referencing a non-dependency (`compio_postgres`). | ~12% phantom source; public API advertises capability no build produces; genuine dead code now undetectable. |
| C4 | **Shipped PG path has no in-crate live-DB regression suite** | **42 of 92** test files open `#![cfg(feature="native-pg")]` (undeclared) → never compile. ~38k lines of coverage dark. | The most safety-critical component — apply against real Postgres — is covered only by a TS-side facade smoke test. |

> Honest reframe: the extraction's earlier "green 679/0" was green **only on the
> compilable unit subset**. `cargo build` passing did *not* make the five
> constraints structurally true. This plan is what makes them true.

---

## Driver policy (decided 2026-07-12)

"npm/host-injected drivers only" applies to the **network** dialects — **Postgres
(`pg`) and MySQL (`mysql2`) go through the `SqlSession` seam**. **SQLite stays
in-process via `rusqlite`** (not moved to the seam). Rationale: the seam earns its
keep for network DBs where the host owns the connection/pool/auth; SQLite is
*embedded*, so in-process is the natural fit. The engine is already a native napi
cdylib that already links C (`pg_query` in `zero-migrate-guard`), so `rusqlite`'s
bundled SQLite is incremental — and it keeps in-process speed, the fast Node-free
test loop, the statically-registered `vec0` + FTS5 with `load_extension` locked
down, and the SQLite C-authorizer defense-in-depth. Considered and rejected:
`node:sqlite` (experimental extension API, Node-dependent tests) and `better-sqlite3`
(a *second* native addon for no gain over rusqlite). So there are three backends:
`PostgresBackend<S: SqlSession>`, `MysqlBackend<S: SqlSession>` (both seam), and
`SqliteBackend` (in-process rusqlite) — unified by the dialect-agnostic
`MigrationBackend` trait, where `SqlSession` is an impl detail of the two network
backends, not a bound on the trait.

## Target architecture

### Rust — one workspace, `crates/*`, no excluded members

```
zero-migrate-ir        Wire contract: MigrationIr, closed Op enum, Expr AST, IrScalar,
                       typed-id + precondition vocab, STRUCTURAL validator, canonical
                       checksum, fail-closed load gate, schemars emit of op-ir.schema.json.
                       Pure data, zero I/O, zero C deps. Consumed alone by CI TS-codegen
                       and by hosts that validate/checksum envelopes without applying.
    ▲        ▲
zero-migrate-guard      pg_query(libpg_query)-backed security: classify, deny-list,
    ▲        │          advisories. Owns the only C dep on the non-SQLite path.
    │        │
zero-migrate  ◄────────  THE engine (fable's "zmg-engine"): schema snapshot/diff/DDL,
    ▲                    IR→script compile, policy validation, dialect backends
    │                    (PG-over-seam, MySQL-over-seam, in-process SQLite), journal,
    │                    executor, drift, policy, role provisioning, the SqlSession seam.
    │                    The ONLY crate an embedder (zeroship control plane) depends on.
    │
zero-migrate-node       napi cdylib, IN the workspace: #[napi(object)] wire DTOs as the
                        single source of truth (TS imports the generated .d.ts),
                        JS-driver→SqlSession adapter, typed verbs (validate/plan/apply/
                        status/history/rollback).
```

Cross-registry note: the flagship **Rust crate** `zero-migrate` = the *engine*
(what embedders depend on); the flagship **npm package** `zero-migrate` = the
*DSL* (what migration files import). Different audiences, different registries —
each is "the main thing you touch" in its world.

### npm — `packages/*`

```
zero-migrate          The authoring DSL: op.*, defineMigration, types, the pure-JS
                      recorder that drains up() into { irVersion, name, ops }. ZERO
                      native code, zero deps — what a migration file imports. Exposes
                      the recorder to the engine package via a documented
                      "./internal/recorder" subpath (one sanctioned consumer).
    ▲
    │ depends on (drains migration modules through ./internal/recorder)
zero-migrate-engine   Host runtime: loads the zero-migrate-node addon, ships pg/mysql2
                      driver adapters (optionalDependencies), exposes apply/plan/status/
                      history/validate, ships the ONE CLI (bin: `zero-migrate`, incl.
                      `zero-migrate new` scaffolding). napi per-platform binary packages.
```

### Deliberately absent (with reasons)

- **No `zero-migrate-schema` crate.** 77% of today's schema crate (`query.rs`,
  11.5k lines: `build_find`/`build_aggregate`/`build_vector_search`) has **zero
  engine callers** — it's a data-plane query language riding along for a consumer
  (plugin-db) not in this repo. The parts the engine uses (type map, DDL vocab,
  `diff.rs`, sentinel codec) become `zero-migrate::schema`; `query.rs` goes back
  to the monorepo next to plugin-db, or is deleted here.
- **No `runtime_host.rs` / `AuthoringHost` / `RecorderPlatform` / `JsDriverHost`.**
  Dead API (C2). Runtime customization is delivered by three *real* seams instead
  (below): `EngineConfig`, the `SqlSession` driver trait, `EngineHooks`.
- **No `sandbox` feature, no Rust CLI bin, no excluded sub-workspace.** All dead
  or misplaced today; see migration steps 1 and 5.

### Constraint-2 embedding surface — three real seams (replacing the dead ones)

```rust
// zero-migrate crate root — ~25 names, 4 tiers, every other module pub(crate).
// #![deny(unreachable_pub)]; CI fails on any pub use reaching deeper than 2 levels.
// NO #![cfg_attr(..., allow(dead_code))] — ever.

pub use zero_migrate_ir as ir;               // Tier 1: the wire contract

pub struct Engine;                            // Tier 2: plan/apply/status/history/rollback
pub struct EngineConfig {                     //   seam #1 — per-engine config
    pub schemas: Vec<String>,                 //     default ["public"]  (zeroship injects ["zeroship","public"])
    pub journal_schema: Option<String>,       //     default derives <primary>_migrations
    pub sentinel_prefix: SentinelPrefix,      //     default "zero-migrate:" (zeroship injects "zsenc:")
    pub policy: SealedProfile,                //     operator ceiling ⊓ author draft
    pub trust: TrustProfile,
    pub hooks: Option<Box<dyn EngineHooks>>,  //   seam #3 — observability/audit
}
pub struct ApplyRequest<'a> {                 //   per-APPLY identity (constraint 5) — distinct from config
    pub plan: &'a Plan, pub owner_app: AppId, pub project_schema: String,
    pub migrator_role: RoleName, pub registry: &'a OwnershipRegistry, pub approval: Approval,
}
pub struct Plan;                              // THE dry-run preview

pub mod seam {                                // Tier 3 — seam #2: the ONE injected runtime dep
    pub trait SqlSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError>;
        async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError>;
        async fn query(&self, sql: &str, binds: &[Bind]) -> Result<Vec<Row>, DbError>;
        async fn query_one(&self, sql: &str, binds: &[Bind]) -> Result<Row, DbError>;
        /// All params as server-inferred text. Load-bearing (executor.rs:2327 runs
        /// lowered DML through this to dodge PG concrete-OID text→timestamptz refusal);
        /// mysql2 implements it as exec.
        async fn exec_text(&self, sql: &str, params: &[Option<String>]) -> Result<u64, DbError>;
    }
    #[non_exhaustive] pub enum Bind  { Null, Bool(bool), Int(i64), Decimal(String), Text(String), Bytes(Vec<u8>) }
    #[non_exhaustive] pub enum Value { Null, Bool(bool), Int(i64), Decimal(String), Text(String), Bytes(Vec<u8>), TextArray(Vec<Option<String>>) }
    pub struct Row;    // try_get only — NO panicking get()
    pub struct DbError { pub message: String, pub sqlstate: Option<String> }
}

pub mod backend {                             // Tier 4 — each backend owns ITS dialect's
    pub struct PostgresBackend<S: SqlSession>; //   lock/journal/session SQL + placeholder style
    pub struct MysqlBackend<S: SqlSession>;    //   ($N vs ?), rendered BEFORE SQL crosses the seam
    pub struct SqliteBackend;                  //   in-process rusqlite actor (feature "sqlite")
}
```

**Why this fixes C1 structurally:** the executor is generic over `Backend`;
`pg_advisory_lock`/journal/`SET ROLE` become `PostgresBackend` methods,
`MysqlBackend` issues `GET_LOCK`, and no dialect SQL ever lives in the shared
executor. MySQL can finally ride the same seam.

> **codex's contribution, considered and narrowed:** codex proposed decomposing
> the kitchen-sink `MigrationBackend` trait into `DbSession`/`LockProvider`/
> `JournalStore`/`CatalogReader`/`TransactionManager`. We adopt the *diagnosis*
> (the god-trait is wrong) but fable's *cure* — one `SqlSession` seam + per-dialect
> `Backend` structs that own lock/journal SQL — is cleaner: it puts dialect logic
> in concrete types, not 5 traits every driver must implement-or-reject. Keep
> fable's shape.

### Naming translation table (long form — the decision)

| Class | Today | New (long form) |
|---|---|---|
| Crates | `zeroship-migrate`, `-schema`, `-node` | `zero-migrate` (engine), `zero-migrate-ir`, `zero-migrate-guard`, `zero-migrate-node` |
| npm | `zero-migrate` (combined) | `zero-migrate` (DSL) + `zero-migrate-engine` (host+CLI) |
| Env vars | `ZEROSHIP_MIGRATE_*`, `ZEROSHIP_MIGRATE_NATIVE` | `ZERO_MIGRATE_*`, `ZERO_MIGRATE_ADDON_PATH` |
| recorder-child override | — | **deleted** with `frontend/` |
| JS symbol brands | `Symbol.for("zeroship.migrate.decimal/v1")` | `Symbol.for("zero-migrate.decimal/v1")`, `…/bytes/v1` |
| Ephemeral marker | `__zeroshipMigrateNextvalDefault` | `__zeroMigrateNextvalDefault` |
| Persisted sentinel | `zsenc:` / `__zsmask:` | **`EngineConfig` knob**, default `zero-migrate:enc:` / `zero-migrate:mask:`; zeroship injects `zsenc:` until its engine-swap (see below) |
| Reserved SQL prefix | `__zeroship` | `__zero_migrate` (case-insensitive) — and **reserve it** (today nothing is reserved for the new brand → live collision bug) |
| `TOUCHES_UNKNOWN` | `"\0__zeroship_touches_unknown__"` (pub) | `"\0__zero_migrate_touches_unknown__"` + **de-pub** |
| Default schemas | `["zeroship","public"]` | config default `["public"]`; journal derivation `<primary>_migrations` kept |
| `.d.ts` marker | `__zeroshipPartitionBound` | `__zeroMigratePartitionBound` |
| Generated user code | `import … from "@zeroship/migrate"` | **deleted** with `scaffold.rs`; rebuilt as `zero-migrate new` emitting `import { op } from "zero-migrate"` |

**The one genuine wire contract (not renamed unilaterally):** the persisted
`zsenc:` sentinel is co-written by the monorepo's plugin-db (its own independent
codec sites; the monorepo still ships its own engine fork). Two writers → two
sentinels in the same schema. So it becomes an `EngineConfig.sentinel_prefix`
knob: standalone defaults `zero-migrate:enc:` (no stranger's `pg_dump` carries a
foreign brand); zeroship injects `zsenc:` until — and only until — its engine-swap
PR flips the injection and resets its (disposable) dev DBs. Everything else in the
table was inertia mislabeled "deliberate," not a contract with anyone.

### Module renaming (fixing the misnomers both reviews caught)

- `render/fold.rs` (pure ops→snapshot, "NO database I/O") → `schema::replay`
- `render/lower.rs` (IR→executable compile, 8.4k lines) → `compile/` split by phase
- `render/declarative.rs` (desired-vs-live differ, 7.5k) → `schema::diff`
- `render/renderer.rs` (trust-gated vendor allowance) → `policy::vendor_allowance`
  (it's security policy, not a dialect fact — keep it out of `dialect/`)
- `model/{capability,support,dialect_table}.rs` (three portability tables) → `dialect/`
- `model/validate.rs` splits at its real fault line: structural allow-list walk →
  `zero-migrate-ir::validate`; `PolicyProfile`-dependent checks (`validate.rs:46`
  imports `profile.rs`) → `zero-migrate::validate_policy`
- "plan" means exactly one thing (the dry-run `Plan`); `render/plan.rs`'s
  execution artifact → `Script`/`ScriptStep`
- `PgSession` → `seam::SqlSession`; `Seam{Bind,Value,Row,Error}` → `seam::{Bind,Value,Row,DbError}`
- Keep the good names: `MigrationIr`, `Op`, `Expr`, `SchemaSnapshot`, `Checksum`,
  `MigrationEngine`→`Engine`, `SqlGuard`.

---

## Migration path (green at every step — fable's sequence, long-name form)

1. **The funeral (pure deletion).** Delete `frontend/` + `apply/backend/mysql/`
   (incl. the 18,752-line vendored `mysql2` bundle) + `runtime_host.rs` + the
   `sandbox` feature + all `native-pg`-gated files + the 42 `native-pg` test files
   (git keeps them; step 6 resurrects their *content*) + the ~35 phantom root
   re-exports + `build.rs`'s check-cfg suppression + **`lib.rs:81`'s crate-wide
   `allow(dead_code)`**. Move `src/snapshots/*.txt` → `tests/goldens/`. Then fix
   everything the newly-honest compiler reports. (compio stays in `[dependencies]`
   — the bin's `#[compio::main]` is live until step 5.)
2. **The rename (one mechanical commit).** Apply the naming table; regenerate the
   committed `sdks/migrate/dist/` (carries old symbols); purge sed-residue doc
   gibberish (`manifest_entry.rs:3-11`, the `lib.rs:4-7` compio-postgres lie, the
   `Cargo.toml:160` nonexistent-crate reference). **CI grep gate:**
   `git grep -iE 'zeroship|zsenc|__zs|zsv8' -- ':!CHANGELOG*'` returns zero.
   Coordinate the sentinel knob with zeroship (it keeps writing `zsenc:` via config).
3. **The crate cuts.** (a) Extract `zero-migrate-ir` from `model/{ir,expr,migration,load}.rs`
   **+ `id.rs` + `precondition.rs`** (they ride along) and split `validate.rs`
   structural→ir vs policy→engine. (b) Extract `zero-migrate-guard` from `guard/` +
   `analysis/` (takes the `pg_query` C dep). (c) Dissolve the schema crate into
   `zero-migrate::schema`; offer `query.rs`'s data-plane half back to the monorepo
   or delete. (d) Move `zero-migrate-node` into the workspace (`exclude` dies, its
   private `Cargo.lock` dies, workspace lints apply).
4. **Kill the false MySQL arm, then build the real one.** Immediately remove
   `{kind:"mysql"}` from `DriverConfig` (shipping a typed lie is worse than
   shipping less). Then introduce the `Backend` split (lock/journal/placeholder SQL
   out of `executor.rs`), implement `MysqlBackend<S>` (`GET_LOCK`, `?` params), and
   re-add `kind:"mysql"` **in the same PR as a live-MySQL integration test**.
   Rename the seam; both value enums gain `Decimal`; `SeamRow::get` panic → `try_get`.
5. **Type the napi boundary; split npm; retire the Rust CLI.** Consolidate all DTOs
   into `zero-migrate-node/src/wire.rs`; bump napi floor to **napi6** (integers
   cross as `bigint`); generate the `.d.ts`, delete the 3 TS hand-copies + the
   JSON-string verb plumbing. Split `sdks/migrate` → `packages/{migrate,engine}`;
   delete `generate()` throw-stub; make `status()` consume its `migrations` arg (or
   drop it); delete the 3 `as never` casts; reconcile `index.ts:9` vs `:53` on
   `raw`; delete the hard-coded monorepo worktree fallback in `addon.ts`. Retire
   the Rust `[[bin]]` into `packages/engine/src/cli.ts` (+ `zero-migrate new`).
   **Now** compio leaves `[dependencies]`.
6. **Resurrect the PG regression suite against the seam.** The 42 deleted `*_pg.rs`
   *scenarios* return via (a) a **dev-only** `SqlSession` impl over the blocking
   `postgres` crate (`[dev-dependencies]` — never ships, host zero-tokio invariant
   untouched) driving in-crate tests against PG :5440, and (b) the Node e2e oracle
   over the real addon+`pg`. The dev-driver is also the first `seam::conformance`
   consumer.
7. **Docs that match reality.** `docs/{architecture,embedding,driver-authors,op-dsl,security-model}.md`.

**Net:** 4 Rust crates + 2 npm packages, every one buildable and tested in every
advertised configuration; one brand; a root API of ~25 names; one driver seam
MySQL can ride; zero lines no build can compile.

---

## Open questions for you

- **Sentinel coordination:** the `zsenc:` knob assumes a future zeroship
  engine-swap PR. Confirm that's the intended end-state (monorepo eventually
  consumes this `zero-migrate` and drops its fork), so the knob is a bridge, not
  forever.
- **`query.rs` data-plane half:** offer it back to the monorepo (next to plugin-db,
  its only caller) or delete from `zero-migrate` entirely?
- **Execution model:** run steps 1–3 first (deletion + rename + cuts — an afternoon
  each, low risk, big legibility win) and re-evaluate before the heavier 4–6, or
  commit to the whole sequence up front?
