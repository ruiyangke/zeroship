# JS `op.*` Portable Migration DSL — Normative Specification

- **Status**: Proposal (drives Task #69) — normative, buildable form
- **Date**: 2026-06-23
- **Crate / branch**: `zeroship-migrate` @ `feat/db-migration-engine`; new sibling JS package `@zeroship/migrate` (the op builder); existing adapter crate `zeroship-migrate-js`

> This document is the self-contained normative contract. An implementer (human or AI coding agent) can build from it alone. It states only final decisions. A short non-normative changelog of superseded readings lives in Appendix B.

> **Citations are NAME-anchored, not line-anchored.** Every `file.rs:NNN` citation in this document is an aid for locating a named symbol (a type, function, trait, or field), not a precise patch target. Line numbers track a snapshot of `feat/db-migration-engine` and drift by a few lines as the branch evolves; the **named symbol is authoritative**. An implementer (especially an AI agent applying `sed`/patch edits) MUST resolve each edit by the symbol name (e.g. `pub struct ChecksumInput`, `fn run_online`) and treat the line number as a hint. A mechanical pre-implementation pass MAY regenerate every citation against current HEAD.

---

## 0. Decision in one paragraph

Today a creator authors a migration as a `.sql` file in a single dialect (Flyway `V0001__x.sql`, or dbmate `<ts>_x.sql`). That forces a choice of Postgres *or* SQLite, hand-maintenance of two files for portability, and gives no portable path for **data migrations** (DML). This design makes a **JavaScript/TypeScript `op.*` DSL** the primary authoring path. The author writes **one** `.ts` migration with `up(op)` / `down(op)`; at **build/dev time** (Node, not the engine) the DSL is evaluated into a **checksummed `.ir.json` artifact** — the migration **intermediate representation (IR)**. The Rust engine loads that IR, **lowers it per-dialect into an ordered `AppliedPlan`** (a sequence of the engine's existing phase artifacts — a `Migration`, a `BackfillSpec`, an `ExpandContractPlan`; §2.0), and runs it through the existing guard → least-priv role → immutable journal → apply pipeline. A pure-DDL migration lowers to a single-step plan and applies exactly like a `.sql` migration today; a mixed DDL+DML or online migration lowers to a multi-step plan whose interleave/journal/pending logic is executed by a **single shared plan orchestrator, `apply_plan`** (§2.0, §6).

**What the committed scope delivers.** The committed deliverable is **the imperative `op.*` authoring surface (DDL + DML) + online rename + the AI-loop portable-expression validator & structured-error envelope + a portable, bi-dialect data path (insert/update/delete + batched backfill on BOTH Postgres and SQLite)**. Portable DDL is a byproduct that substantially overlaps the declarative-diff path Task #46 already shipped; the genuinely non-overlapping value is the imperative surface, online rename, the AI feedback loop, and **portable bi-dialect DML — the primary reason this DSL exists** (one script, both backends, DDL *and* DML; §1.1). The bi-dialect DML executor (the SQLite backfill/DML path) is **committed scope**, not a demand-gated maybe. The "one portable script, DDL+DML, both backends" headline is backed by committed PRs (§10), not aspiration.

**Typing stance (binding).** Table and column **names** in migration ops are **plain strings** (`op.addColumn("users", "last_seen", …)`, `op.update("orders", …)`), **never** bound to the live `@zeroship/db` schema. Migrations are immutable historical artifacts; a migration that referenced `users.lastSeen` must still compile after a later migration drops that column. Structural type-safety (op method arg shapes, the `ColType` builder lexicon, `op.raw({pg,sqlite})` shape, insert-row value shapes) is preserved; **name existence is validated at apply time against the real DB**, never at `tsc` time against the declared schema (§3.3). This follows industry precedent: Kysely uses `Kysely<any>` in migration files on purpose, Alembic uses string names, Drizzle migrations are generated SQL — none bind migration files to the live schema.

Two non-negotiable invariants pin the whole design:

1. **The engine stays zero-tokio, zero-V8, trusted-Rust.** The DSL never runs inside the engine. It runs in Node at build time and emits *data* (JSON IR). The engine only ever ingests JSON it can validate.
2. **The IR is the single source of truth, anchored by golden-fixture byte-equality.** The JS builder and the Rust engine share one IR schema. Neither side may add an op the other cannot represent. The load-bearing anti-drift mechanism is a golden-fixtures CI gate (§2.5): a corpus of `.ir.json` files both sides must agree on (value-equality via the typed-value checksum). Code-generation of TS types from the Rust schema (`schemars` → `json-schema-to-typescript`) is a best-effort ergonomics layer, not a correctness guarantee — JSON-Schema does not reliably round-trip every serde enum tagging, so the fixtures are authoritative.

### 0.1 The generic plan executor is net-new engine work

There is **no** pre-existing `apply_expand` orchestrator and no plan-shape-neutral executor. The DDL+backfill+online interleave (`pending_contract` at `engine.rs:521`/`:550`, `run_online` at `:537`, `run_expand_pg` at `expand_contract.rs:693`) lives **only inside `apply_declarative_locked` (`engine.rs:446`)**, bound to the declarative `DeclarativePlan` shape (it consumes `plan.renames` as pre-authored `RenameStep`s). The other apply path, `apply_inner` (`engine.rs:628`), flattens `plan.items` to a `Vec<Migration>` (`:648`) and calls `executor::apply_with_lock_backend` (`:661`) with no backfill and no online handling. Therefore the generic plan executor the IR needs is **net-new engine work**: PR0 builds a single shared `apply_plan(Vec<PlanStep>)` by lifting the interleave/journal/pending logic out of `apply_declarative_locked` into a plan-shape-neutral orchestrator, and the shipped declarative path is re-pointed onto it via a thin shape-adapter (§6). The IR is a thin *front door*, but it sits on a plan executor that is net-new.

---

## 1. Motivation & goal

### 1.1 The one-script-both-backends + DDL+DML goal

zeroship runs **two backends in one binary**: Postgres in production, SQLite in the dev tier (`docs/reference/sqlite-divergences.md`, `docs/reference/auth-dev-tier.md`). The engine already lowers a single logical schema to *both* dialects via the `DdlEmitter` seam — `PgEmitter` and `SqliteEmitter` (`crates/zeroship-migrate/src/declarative.rs:3810` trait, `:3864` PG impl, `:3999` SQLite impl). What is missing is a **portable authoring surface**: today the author writes raw SQL in one dialect. A creator who wants their app to run the same migration in dev (SQLite) and prod (Postgres) must hand-maintain two SQL files and keep them in sync — exactly the duplication the engine's internal seam was built to avoid.

The goal: **a creator (or the AI builder) writes one migration; it applies faithfully to Postgres and SQLite, doing both schema changes (DDL) and data migrations (DML)** — portably for the common cases (all DDL, literal insert/delete, the portable update/backfill grammar, and the `op.fn.splitPart` helper within its pinned single-ASCII envelope, §9), and dialect-explicit (with a structured, machine-readable error rather than a silent mis-apply) for the long tail. The headline is "one script, both backends" for the common case, not an unbounded "any transform is portable" — §9 draws the boundary precisely, with the SQLite `split_part` lowering exhibited in-doc and proven byte-identical to PG against real SQLite 3.51.2. The proof requires adding `instr` to the SQLite authorizer allow-list — a deterministic, side-effect-free builtin — as an explicit, separately-reviewed change (§9); a `substr`-only split is mathematically infeasible.

**The AI builder is the dominant author.** Per `AGENTS.md` ("AI builds it"), the AI builder, not a human, authors most `op.*` migrations. An LLM emitting `op.sql\`split_part(...)\`` or a non-portable transform is the common case, not the edge case. The design is shaped around the AI loop's feedback mechanism throughout:

- **Structured, machine-readable rejections are a load-bearing contract** (§3.3.1.1 / §8.8): every authoring-time rejection carries `{ code, op_index, ts_location, dialect, reason, suggested_fix? }`, because the AI loop self-corrects on structured payloads, not prose. The human-readable message is the projection of the structured error.
- **The portable-expression-grammar validator (§3.3.1.1) and the structured-error envelope land at IR-freeze (PR1)** — when the IR shape is frozen — because they are the AI loop's primary feedback signal and must exist from the first IR PR.
- **The hero example (§3.1) is presented in the shape the AI builder emits**, with the human-edited form as a variant.
- **AI-authored migrations are PG-only-by-default until a both-backends dry-run passes** (§8.8): an un-checked AI migration cannot silently ship claiming bi-dialect portability.

### 1.2 Why declarative-diff alone is insufficient

The engine already has a declarative path: `desired_snapshot(...)` compiles a `t.*` schema into a `DesiredSchema`, and `DeclarativeAuthor::diff(...)` emits migrations from the schema delta (`crates/zeroship-migrate-js/src/generate.rs:93` snapshot, `:112` diff, `:114` `all_migrations`). That path is excellent for **schema-as-code** and yields portable DDL for free. It is structurally **incapable of DML**:

- The differ's *entire input* is a description of the **desired schema shape**. It has no vocabulary for "for every row where X, set Y" — a data transform is not a snapshot delta.
- Real migrations routinely interleave DDL and DML: add a column, **backfill it**, then add `NOT NULL`; split a `name` column and **copy the data**; seed reference rows; delete tombstones; normalize a denormalized field. None of these are diffable.
- The differ is also **non-authorable for intent**: it can infer an `ADD COLUMN`, but it cannot express "this add is paired with a backfill that must complete before the `NOT NULL`," the expand-contract pattern the engine already models (`crates/zeroship-migrate/src/expand_contract.rs`).

So we need an **imperative op surface** (Alembic / Kysely shape) *in addition to* the declarative one. They are not competitors: §7 shows the declarative path can **scaffold** an `op.*` migration (autogenerate parity), and both paths emit the *same IR*.

### 1.3 What "primary path" means concretely

- New migrations are authored as `op.*` `.ts` files. `zeroship-migrate new` scaffolds a `.ts`, not a `.sql`.
- `zeroship-migrate generate` (declarative diff) emits an `op.*` `.ts` **scaffold** (Alembic-autogenerate parity), which the author reviews/edits and which carries DML hooks the differ left as TODOs.
- The dbmate-style raw-SQL **loader stays** (the engine still ingests raw SQL — both for `op.raw` rendering and for the public dbmate CLI in §7), but it is no longer the recommended way to *author* a new migration. Raw SQL ultimately survives only behind `op.raw({ pg, sqlite })` plus the retained loader for the dbmate CLI and the platform Flyway-mode profile.

---

## 2. The migration IR contract

This is the heart of the design. The IR is a **JSON-serializable op list** the JS builder emits, which the Rust engine lowers into an **ordered `AppliedPlan`** — a sequence of the engine's existing artifact types. The engine already serializes/deserializes `Migration` via serde (`crates/zeroship-migrate/src/migration.rs:379`). The IR is a new **front door** to the existing step *types*; the generic plan executor that runs an arbitrary `Vec<PlanStep>` is net-new (PR0), lifted out of `apply_declarative_locked`.

### 2.0 The applied artifact is an `AppliedPlan`, not a single `Migration`

**Naming — a deliberate collision avoidance.** The applied-artifact type introduced here is named **`AppliedPlan`**, NOT `MigrationPlan`. The name `MigrationPlan` is **already taken** by an existing public type (`engine.rs:58`, re-exported at `lib.rs:113`): it is the read-only lint/dry-run **preview** result (`{ items: Vec<PlannedMigration>, destructive, requires_approval, denied }`), returned by `MigrationEngine::plan()` and consumed by the dry-run API. That type keeps its name and meaning, untouched. The net-new ordered execution artifact this design lowers an `.ir.json` into is `AppliedPlan`, with steps `PlanStep` and rename steps `RenameStep`. The existing `MigrationPlan` (dry-run preview) and the new `AppliedPlan` (ordered execution artifact) are distinct symbols with distinct meanings; both coexist on the public surface (§5.2).

One `.ir.json` lowers (at load, in trusted Rust) to:

```rust
/// What one authored .ir.json becomes after IrAuthor::lower. NOT a single Migration.
/// NOT the existing dry-run `MigrationPlan` (engine.rs:58) — a distinct, new type.
pub struct AppliedPlan {
    pub version: MigrationId,        // derived from the filename version (loader.rs)
    pub name: String,
    pub steps: Vec<PlanStep>,        // ordered; the executor runs them in sequence
    pub checksum: Checksum,          // ONE checksum over the canonical op list (§2.4)
    pub flags: MigrationFlags,       // derived ∪ overridden (§2.4)
    pub dialect_scope: DialectScope, // Both | PgOnly — derived from the artifact's ops; a
                                     // SEPARATE journaled column, NOT folded into the
                                     // identity checksum (§2.4). The REJECTION ("PgOnly
                                     // against a SQLite target") is a DEPLOY-TARGET
                                     // validation parameterized by the deploy-target
                                     // dialect (§2.4.1), never artifact-intrinsic.
    pub rollbackable: bool,          // false if ANY step is down:None (Backfill/Dml/
                                     // in-flight OnlineRename) — derived at load,
                                     // surfaced by status/rollback BEFORE attempt (§2.1.2).
    pub owner_app: String,           // server-stamped (§8.6)
    pub depends_on: Vec<MigrationId>,
    pub supersedes: Vec<MigrationId>,
    pub preconditions: Vec<PreconditionCheck>,
}

pub enum PlanStep {
    /// A transactional or non-txn DDL statement bundle — an existing `Migration`
    /// (single `up: String`, no parameter slot — migration.rs:385).
    Ddl(Migration),
    /// A parameterized DML statement (insert/update/delete) — NEW variant (§2.3.2).
    /// `template` is the placeholder SQL the journal hashes; `binds` are the typed
    /// values, which `Migration{up:String}` has NO slot to carry.
    Dml { template: String, binds: Vec<BindValue> },
    /// A crash-safe batched data backfill — an existing `BackfillSpec` (backfill.rs:76),
    /// run by run_backfill (PG) / the SQLite backfill executor (§2.3.1).
    Backfill(BackfillSpec),
    /// A rename, lowered to ONE of two DIALECT-DISTINCT executable shapes (§2.6.2).
    OnlineRename(RenameStep),
}

/// One `renameColumn` op lowers to exactly ONE of these, chosen by the deploy-target
/// dialect at lowering (IrAuthor::lower, §2.6). It is the *executable* shape, so the
/// dual-execution dispatch in `apply_plan` is structural (match on the variant).
pub enum RenameStep {
    /// PG: an online expand-contract. Executed via `OnlineSchemaChange::run_online`
    /// (PgOnline, expand_contract.rs:760). `ExpandContractPlan` = Vec<Migration>
    /// (E1..C2) + BackfillSpec (`struct ExpandContractPlan` expand_contract.rs:104). The contract (C1/C2) is
    /// partitioned across deploys as `pending_contract` (§2.0.2).
    PgExpandContract(ExpandContractPlan),
    /// SQLite: an OFFLINE 12-step table rebuild. Executed via
    /// `MigrationBackend::rebuild_one(&spec, &migration, applied_by)`
    /// (backend_sqlite/mod.rs:550 → rebuild_sql.rs), NOT via `run_online`.
    ///
    /// This variant REUSES the existing `declarative::SqliteRebuild`
    /// (`declarative.rs:2053`, which already bundles `{ migration: Migration,
    /// spec: SqliteRebuildSpec }`) — it does NOT define a parallel struct. The
    /// declarative `plan.rebuilds` loop (engine.rs:491-503) feeds exactly that
    /// type to `rebuild_one` today, so the shape-adapter (§6.0) maps a
    /// `declarative::SqliteRebuild` straight into this variant with no
    /// conversion. Single source of truth: one `{migration, spec}` type.
    SqliteRebuild(declarative::SqliteRebuild),
}
```

**Why a plan, not a `Migration`.** The Rust `Migration` (migration.rs:379-423; `up: String` at `:385`) is a single `up: String` with no slot for a `BackfillSpec` or a step sequence. The engine already models a multi-phase change as a plan: `ExpandContractPlan` is `expand: Vec<Migration>` + `backfill: BackfillSpec` + `contract: Vec<Migration>` (`struct ExpandContractPlan` at `expand_contract.rs:104`; fields `expand:106`/`contract:108`/`backfill:111`/`intent:121`), and `apply_declarative_locked` (`engine.rs:446`) runs E1..E3 via `run_online` (`run_online` call at `engine.rs:537`)→`run_expand_pg` (`expand_contract.rs:693`), then `run_backfill`, then holds C1..C2 as `pending_contract` (`:521`/`:550`) — interleaving DDL `Migration`s with a non-atomic backfill phase, exactly the §3.1 hero shape. The IR does not invent the phase semantics; it composes the engine's existing phase types into one ordered list. The net-new piece is the plan-shape-neutral executor that runs that list (`apply_plan`, PR0).

**The single-`Migration` case is the degenerate one-step plan.** A migration of pure portable DDL with no DML, backfill, or online op lowers to `steps = [Ddl(one Migration)]` — the overwhelming common case, indistinguishable from "one `.ir.json` → one `Migration`." Richer cases (the hero example: addColumn + backfill + dropColumn; an online rename) lower to multi-step plans. No `.ir.json` is ever hand-split by the author — a single op list with mixed phases becomes a single multi-step plan under one version.

#### 2.0.1 Sub-step versioning, checksum, and journal

A plan carries **one** outer `version` (the filename's `<NNNN>`) and **one** `checksum` (over the whole canonical op list, §2.4). Every `PlanStep` gets a deterministic sub-version `step_id = uuidv7_derive(plan.version, step_index)` — the same deterministic-derivation discipline the loader uses for `migration_id_for_version` (`loader.rs:197`) and that `ExpandContractAuthor` uses to derive E1..C2 ids + their `depends_on` chain.

- **Exception — retained data/online steps in a SQUASHED plan keep their ORIGINAL sub-version (they are NOT re-derived from the squash's version).** When a squash `S` retains a historical `Backfill`/`Dml`/`OnlineRename` step verbatim (§5.4), that step preserves its **pre-squash sub-version** — the id `uuidv7_derive(<original plan's version>, <original step_index>)` it already carries in the journal — and is **exempt** from the `uuidv7_derive(S.version, idx)` rule. This is required for §5.4 Context A's net-applied-skip to work: the retained step is recognized as already-journaled only if its id matches the original, which a fresh `S`-derived id would not. ONLY the collapsed DDL-spine steps of `S` get `S`-derived sub-versions; the retained data/online steps carry their original ones. A fixture asserts a retained step's journaled sub-version in `S` equals the original historical step's sub-version (§5.4 fixture 4).

- **Journal.** Each step (DDL, DML, backfill, and each E1..C2 of an online rename) journals **independently** under its sub-version, so crash recovery resumes mid-plan (the engine already does this for expand-contract: contract steps stay `pending_contract` until the backfill's journal row lands, `engine.rs:550`). A `Dml` step journals its **parameterized template** (binds fold into the plan checksum, §2.3.2). The outer plan `version` is journaled as a **plan-group marker** carrying the one plan checksum; a step is "satisfied" iff its sub-version is net-applied, and the plan is satisfied iff all steps are. This reuses the supersedes-style "satisfied-by" logic (`migration.rs:396-410`).
- **`depends_on`.** Cross-plan deps attach to the **first** step; intra-plan ordering is the `steps` vector order plus the derived intra-chain `depends_on` (E2 depends on E1, etc.) that `ExpandContractAuthor` already emits.
- **Drift.** The drift anchor is the **plan checksum** (over the canonical op list), not the per-step rendered SQL. Editing the `.ts` changes the op list ⇒ changes the one plan checksum ⇒ the executor's first-pass drift check aborts (§5.3). A step's rendered SQL differing per dialect does not move the checksum (§2.4).

#### 2.0.2 Cross-deploy partition for a PG `OnlineRename` (the multi-deploy pending-contract)

A `RenameStep::PgExpandContract` is **not** a single-deploy in-process sequence. The engine applies only the rename's *expand* (E1..E3 + backfill) in the deploy that runs it, and returns the *contract* (DROP TRIGGER C1 + DROP COLUMN C2) as `pending_contract` to be applied in a **subsequent deploy** under `Approval::Approved` — C2 is `destructive` and the pending set is `requires_approval`-gated (`engine.rs:270-300`, `:550`). `apply_plan` **preserves this multi-deploy partition for the `PgExpandContract` variant**: deploy N applies plain + EXPAND (backfill runs; E3 journals only after the backfill row lands) and surfaces C1/C2 as `pending_contract`; deploy N+1 applies that pending contract under approval. The **`SqliteRebuild` variant has no such partition** — it is one atomic offline step, fully applied in its own deploy (§2.6.2).

#### 2.0.3 Concurrency, racing deploys, and the pending-contract interlock

The cross-deploy partition (§2.0.2) means in-flight online state lives **in the journal between deploys**. Within a single deploy, in-flight state is protected by a held lock; across deploys, it is protected by the journal plus a fail-closed pending-contract check. `apply_plan` inherits the engine's existing whole-deploy locking unchanged and adds the cross-deploy check:

1. **The project advisory lock is held across the ENTIRE deploy, including the backfill — exactly as the shipped engine does.** The engine holds a **session-scoped project advisory lock across the whole declarative deploy** (`engine.rs:316`, acquire `:345`, release `:355`), and the inner sub-batches re-enter it with `LockMode::AlreadyHeld` (`:457`/`:544`). `run_online` → `run_expand_pg` runs E1+E2 → backfill → E3 as one sequence under that held lock: the per-batch `pg_advisory_xact_lock` inside `run_backfill` is re-entrant under the session lock the caller already holds, "so whole-deploy serialization is preserved through the backfill without ever freeing the project lock" (`expand_contract.rs:678-688`, `:719-744`). `apply_plan` reuses `run_online` verbatim as the EXPAND-execution destination (§2.6.2), so it inherits this lock discipline by construction — it does **not** decompose the online sequence or release the lock across the embedded backfill.
   - **The availability cost is accepted, deliberately and consistently with the shipped engine.** Holding the project lock across a long backfill blocks *other deploys for that one project* for the backfill's duration. This is the cost the shipped engine already pays; the rename's *contract* is still partitioned to a later deploy (§2.0.2) so the lock is never held across a deploy boundary, only within one deploy. The lock is per-project (`pg_advisory_lock(hashtext(project))`, `engine.rs:340`), so a backfill on project P never blocks a deploy on a different project; only concurrent deploys *of the same project* serialize — which is the correct and intended behavior, since two concurrent migrations of one project's schema must not interleave.
   - **No per-table row interlock is needed.** Because the project lock is held for the whole deploy, two concurrent deploys of the same project cannot both be mid-apply at once: the second blocks on the project advisory lock at acquire (`engine.rs:345`) until the first commits and releases. There is no within-deploy TOCTOU window and therefore no `table_inflight` row-lock machinery — the held coarse project lock already provides whole-deploy mutual exclusion.
2. **A deploy with a `pending_contract` for table `T` refuses any new op touching `T` until the contract is applied — a CROSS-deploy check, made race-free by (1).** When `apply_plan` begins (already holding the project lock from `engine.rs:345`), it reads the journal's outstanding `pending_contract` set. If the current bundle's op list contains any op (DDL or DML) targeting a table with an outstanding pending contract — including a second `renameColumn` on the same column — the deploy **fails closed**: "table `users` has an in-flight online rename (contract pending from a prior deploy); apply that contract before authoring further changes to `users`."
   - **Why this is not a TOCTOU.** The read-pending-set → act sequence runs entirely *inside* the held project lock (acquired at deploy start, released at deploy end). A second deploy of the same project cannot run concurrently: it blocks at `acquire_project_lock` until the first deploy commits its journal and releases. So the second deploy always reads the *committed* pending-contract set the first deploy left — it can never observe a stale "T is clear" the first deploy is about to invalidate. The serialization is the coarse project lock itself, exactly the mutual exclusion the shipped engine already relies on; no finer-grained interlock is introduced.
   - This mirrors how AWS RDS Blue/Green and Stripe's online-schema tooling treat an in-flight online change as **exclusive** — here exclusivity is whole-deploy (project-scoped), which subsumes per-object exclusivity for the single-writer-per-project model the engine enforces.
3. **Orphaned pending-contract → fail closed, operator-resolved.** If deploy N+1's bundle op list no longer contains the rename whose contract is pending (e.g. the author deleted the `renameColumn` after deploy N applied EXPAND), the pending contract is **orphaned**. The engine does not silently drop it (that leaves a dual-write trigger + shadow column live forever) and does not silently apply it (intent is now ambiguous). It **fails closed** and requires explicit resolution: re-add the rename op, or run `migrate resolve-pending --apply|--abort <version>` (the abort path drops the shadow column + trigger as a deliberate, journaled, approval-gated step). The orphan is surfaced by `status` as a distinct state.
4. **Idempotent re-run of deploy N (EXPAND already journaled).** Each E1..C2 sub-step journals independently (§2.0.1); `apply_plan` skips any sub-step whose sub-version is already net-applied (the same net-applied skip the SQLite `rebuild_one` arm uses, `engine.rs:491-503`). A retried deploy N re-acquires the lock, sees E1..E3 + backfill already journaled, skips them, re-surfaces the same `pending_contract`, and is a no-op.
5. **Each stuck state is a machine-readable obligation the AI orchestrator can act on.** Because zeroship's pitch is "AI builds it, the platform handles everything," each stuck state emits the structured envelope (§8.8) carrying enough to *execute* the remedy:
   - **pending-contract refusal** (2) ⇒ `{ code: "TABLE_HAS_PENDING_CONTRACT", table, pending_version, remediation: "apply_pending", apply_action: { command: "migrate apply-pending", version: <pending_version> } }`.
   - **blocked `depends_on`** (§2.0.4) ⇒ `{ code: "DEPENDENCY_PENDING_CONTRACT", blocked: B, dependency: A, pending_version, remediation: "apply_dependency_contract" }`.
   - **orphaned pending-contract** (3) ⇒ `{ code: "ORPHANED_PENDING_CONTRACT", table, orphan_version, remediation: ["readd_rename_op", "resolve_pending_abort"], abort_action: { command: "migrate resolve-pending --abort", version: <orphan_version> } }`.
6. **The actual safety guarantees are ENGINE-LEVEL and author-agnostic; the AI scaffolder's add+backfill+drop preference is a DX default, NOT a containment boundary.**
   - **What actually holds, for ANY author including a misbehaving AI.** The interlock's safety does NOT depend on who authors the migration. `op.renameColumn` is in the closed op set (§2.3) and PR2 builds it unconditionally — nothing in the engine *prevents* an `op.renameColumn` appearing in any `.ir.json`, including an AI-authored one (a buggy or jailbroken AI loop, or a future scaffolder change, can emit one). The guarantees that contain the resulting multi-deploy state are all engine-level and hold regardless of author: the held-project-lock serialization (1), the fail-closed pending-contract refusal (2), the orphan fail-closed (3), the `blocked-awaiting-approval` retained state and `depends_on` block (§2.0.4), and the destructive-contract approval gate. These are what make the design safe — not any assumption about what the AI emits.
   - **The AI scaffolder preferring non-online add+backfill+drop is a UX/default-preference note, not a structural mitigation.** A multi-deploy pending-contract lifecycle is cross-deploy state an AI loop is poorly suited to reason about, so the scaffolder *prefers* a non-online column add+backfill+drop (no pending-contract phase) and surfaces online rename as an advanced / operator-opt-in op. This reduces how *often* AI-authored migrations hit the pending-contract path; it does NOT bound what is *possible*. The engine treats an AI-authored online rename exactly as a human-authored one — same interlock, same fail-closed refusals, same approval gate.
   - **Orphan-abort is policy-automatable.** A per-project policy `orphan_pending_contract: "operator_resolve" | "auto_abort"`; under `auto_abort`, after a configurable grace window the engine executes the journaled, approval-bypassing-but-logged abort automatically. The default is `operator_resolve` (fail-closed, human-in-the-loop); `auto_abort` is an explicit opt-in.
- **Tests:** (a) two concurrent deploys for one project serialize on the held project advisory lock (`engine.rs:345`) — the second blocks at acquire until the first commits, then reads the first's committed pending-contract set and either proceeds (unrelated tables) or refuses (touches the pending table), producing a consistent journal that is always one of the two legal serial orders, never a both-mid-apply interleave; (a1) **concurrency property test — serial journal under the held project lock**: two real concurrent deploys of the same project (one carrying a long online-rename backfill) always linearize on the project advisory lock — the second observes the first's committed journal state and never interleaves; asserted by racing the two deploys and checking the journal is one of the two legal serial orders; (a2) **cross-project independence**: a concurrent deploy of a DIFFERENT project proceeds while project P's large online-rename backfill is in flight (the lock is per-project `pg_advisory_lock(hashtext(project))`, so a different project is never blocked); (b) a deploy whose op list touches a table with an outstanding pending contract is refused with the `TABLE_HAS_PENDING_CONTRACT` payload carrying an executable `apply_action`; (c) a second `renameColumn` on the same column while a prior contract is pending is refused; (d) an orphaned pending contract fails closed, is surfaced by `status`, and emits the `ORPHANED_PENDING_CONTRACT` payload; (e) re-running deploy N after EXPAND is a no-op; (f) under `auto_abort` an orphan is auto-resolved after the grace window, and under `operator_resolve` it stays pending.

#### 2.0.4 Cross-plan `depends_on` satisfaction in the presence of a pending contract

When plan B `depends_on`s plan A, and A is an online rename whose **contract (C1/C2) is still `pending_contract`**, is A "satisfied" for B's dependency resolution?

- **A plan with a pending online-rename contract is NOT fully satisfied** — its C1/C2 sub-steps are not net-applied, so by "satisfied iff all steps satisfied" (§2.0.1) the plan is **unsatisfied**. Therefore **any plan B with `depends_on: [A]` BLOCKS until A's contract is applied.** B cannot apply against a half-applied A.
- **This composes with the §2.0.3 interlock.** If B's ops touch A's pending table, B is already refused by §2.0.3(2). The `depends_on` rule additionally blocks B even when B touches a different table but explicitly declared a dependency on A.
- **Roll-forward, not deadlock.** Blocking is resolved by applying A's contract (the next deploy that includes A's rename, or `migrate resolve-pending --apply`); once A is fully satisfied, B unblocks. There is no deadlock because the contract application does not itself depend on B.
- **The double-bind (B `depends_on` A AND B's own ops also touch A's in-flight table T).** Deploy N applies A's EXPAND on T; A's contract is held pending; A is unsatisfied and T has a pending contract. Deploy N+1 carrying B is blocked **two ways**: the `depends_on` rule (A unsatisfied) and the pending-contract refusal of §2.0.3(2) (B touches T while T has an outstanding pending contract). The only resolution is an intervening deploy that applies A's pending contract first.
  - **Who issues it.** The block holds and is resolved the same way for any author (§2.0.3(6) — the guarantees are engine-level, author-agnostic). In the common "AI builds it" path the scaffolder *prefers* non-online add+backfill+drop, so the double-bind arises *less often* there; but when it does arise — whether a human opted A into an online rename or an AI emitted one — the resolution is identical: an intervening deploy applies A's pending contract under approval, then B re-deploys. The contract-apply deploy carries a destructive `Approval::Approved` checkpoint, so it is operator-confirmed regardless of who authored A.
  - **The structured payloads chain so an automated loop can self-resolve where policy allows.** B's refusal emits both `DEPENDENCY_PENDING_CONTRACT` and `TABLE_HAS_PENDING_CONTRACT { apply_action: { command: "migrate apply-pending", version: <A's pending_version> } }`. An orchestrator issues the `apply-pending` deploy for A's contract (which requires `Approval::Approved` because C2 is destructive — the human checkpoint) and then re-deploys B.
- **Liveness requirement: a plan blocked on a pending-contract dependency PROACTIVELY notifies and escalates.**
  - **`blocked-awaiting-approval` is a DISTINCT, retained status — NOT `failed`.** B is recorded as `blocked-awaiting-approval` (naming A's pending version), a terminal-pending state the scheduler **keeps in the queue**. When A's contract applies, B is automatically eligible again.
  - **Proactive notification on entering the blocked state** via the platform's existing creator-notification channel (the surface dunning/spend-limit alerts use), carrying the structured `DEPENDENCY_PENDING_CONTRACT` payload.
  - **Default timeout + escalation, modeled on the orphan `auto_abort` policy.** A plan blocked beyond a configurable threshold (defaulting to the orphaned-pending-contract horizon, §2.0.3) **escalates**: a second notification, and surfacing in the operator dashboard's "needs attention" set. The design does not auto-apply A's destructive contract on timeout; it escalates *visibility*.
- **Fixture:** B `depends_on` A where A has a pending contract — B's apply is blocked (clear error naming A's pending contract; status `blocked-awaiting-approval`, NOT `failed`; a notification is emitted); after A's contract applies, a re-deploy applies B (no re-submission). A second fixture: the SQLite leg has no such block (a `SqliteRebuild` rename is atomic, so A is fully satisfied in its own deploy; §2.6.2).

### 2.1 The build artifact: `MigrationIr`

The on-disk `.ir.json` (one per authored `.ts`) is:

```jsonc
{
  "ir_version": 1,                 // IR schema version (code-evolution discipline, §5.3)
  "name": "split_name_column",     // human label
  "owner_app": "app_…",            // stamped server-side; builder MUST NOT spoof (§8)
  "ops": [ /* ordered Op[] — see §2.3 */ ],
  "flags": { /* MigrationFlags overrides; defaults derived (§2.4) */ },
  "depends_on": ["mig_…"],         // ordered; usually empty, auto-derived for online (§2.6)
  "supersedes": [],                // squash identity (engine-internal; builder leaves empty)
  "preconditions": [ /* PreconditionCheck[] — §2.7 */ ]
}
```

This `.ir.json` lowers to the `AppliedPlan` of §2.0. Mapping from IR fields to the plan and its constituent steps (`crates/zeroship-migrate/src/migration.rs:379-423`):

| Plan / step field | Source in IR |
| --- | --- |
| `AppliedPlan.version: MigrationId` | **derived at load** from the filename's numeric version, deterministic UUIDv7 (`loader.rs` `migration_id_for_version`). Never in `.ir.json`. Each step's sub-version is `uuidv7_derive(version, step_index)` (§2.0.1). |
| `name: String` | `name` |
| step `Migration.up: String` | **rendered** from the ops assigned to that step by the engine's `DdlEmitter` (per-dialect) — NOT stored in the IR (§2.2). Backfill phases are not in any `up`; they are `Backfill(BackfillSpec)` steps run by `run_backfill` (§2.0). |
| step `Migration.down: Option<String>` | **rendered per the down-derivation matrix (§2.1.1)**. Plan-level `down` = reverse-ordered concatenation of step downs (§2.1.2). |
| `AppliedPlan.checksum: Checksum` | **computed by the engine** via a new `Checksum::of_ir` front door keyed on the canonical IR op list (not the rendered per-dialect SQL, and not per-step), so one checksum spans both backends and all steps — §2.4. |
| `AppliedPlan.flags: MigrationFlags` | `flags`, with unset fields **derived** from the ops (e.g. any `dropColumn`/`delete` ⇒ `destructive: true`). Flags are also computed per step where the orchestrator needs them. |
| `owner_app: String` | `owner_app` (artifact value is a hint; the deploy path overrides it server-side — §2.4 note / §8.6) |
| `depends_on / supersedes / preconditions` | as named; attach to the plan's first step + intra-plan derived chain (§2.0.1) |

#### 2.1.1 The `down` derivation matrix (per op)

`down` is not universally reversible. The engine derives it per op (matching how the declarative author returns `down: None` for `alterColumnType` — `declarative.rs:3609`):

| Op | `down` derivation |
| --- | --- |
| `addColumn` | auto: `dropColumn` |
| `dropColumn` | **`None`** (the dropped column's data is gone) unless author supplies `down` |
| `createTable` | auto: `dropTable` |
| `dropTable` | **`None`** unless author supplies `down` |
| `createIndex` | auto: `dropIndex` — **inheriting the forward op's txn flags**: a `createIndex{concurrently:true}` (forward `transactional:false`) auto-derives `DROP INDEX CONCURRENTLY IF EXISTS` (also `transactional:false`, idempotent) |
| `dropIndex` | **`None`** unless author supplies the index definition in `down`; if the forward `dropIndex{concurrently:true}`, an author-supplied recreate `down` must be `CREATE INDEX CONCURRENTLY IF NOT EXISTS` to pass the PG-only non-txn idempotency validator (on SQLite `concurrently` is dropped) |
| `addConstraint` | auto: `dropConstraint` |
| `dropConstraint` | **`None`** unless author supplies `down` |
| `alterColumnType` | **always `None`** (lossy cast — `declarative.rs:3609`) |
| `alterColumnNullability` | auto for `SET NOT NULL`→`DROP NOT NULL`; the reverse direction is auto |
| `renameColumn` | auto **only for a fully-applied rename** (contract→expand reversal, online on PG, rebuild on SQLite). **`None` for an in-flight expand-contract** — recovery there is roll-forward, not down (§2.6) |
| `insert` / `update` / `delete` / `backfill` | **always `None`** — no general inverse of a DML statement |
| `raw` | author-supplied `down: { pg, sqlite }` or `None`; a creator-supplied `down:{sqlite}` is refused fail-closed in `Confined` exactly as the forward `up:{sqlite}` (§8.7). A `PgOnly` `op.raw` migration's down is PG-only (see below) |

**Composition rule:** a migration's `down` is the reverse-ordered concatenation of each op's `down` only if **every** op is auto-reversible. If any op yields `None` and the author did not provide an explicit module-level `down(op)`, the whole migration is `down: None` (non-rollbackable). The §3.1 hero example (which contains a `backfill` and a `dropColumn`) therefore **must** hand-write `down(op)`.

**`op.raw`'s `down:{sqlite}`.** `down:{sqlite}` is author-supplied raw SQLite of the same untrusted class as a forward `op.raw({sqlite})` — libpg_query cannot parse it, so in `Confined` it is refused fail-closed, identically to the forward (§8.7). A creator-supplied `down:{sqlite}` is a hard authoring error at build/render — never a silently-stored string. A `PgOnly` `op.raw` migration never deploys to SQLite (the deploy-target gate refuses it on a SQLite target, §2.4.1), so its rollback is exercised only on PG: its `down` is `down:{pg}` or `None`, and a `down:{sqlite}` on it is meaningless and rejected. Fixture: an `op.raw` migration carrying a `down:{sqlite}` is refused on the SQLite leg (same `RAW_SQLITE_REFUSED` error, §8.8); a `PgOnly` `op.raw({pg})` migration's `down:{pg}` rolls back correctly on a PG target.

**Non-txn idempotency propagation rule (PG-only by construction).** The executor rejects a `transactional:false` migration whose `up` is not crash-recovery-idempotent — it requires the `IF [NOT] EXISTS` form (`executor.rs:518`, `validate_non_txn_idempotent`). This validator is Postgres-only: it is `pg_query::parse`-based (it walks the libpg_query `NodeEnum`), and libpg_query cannot parse SQLite.
- **PG leg.** `createIndex{concurrently:true}` (forward non-txn) ⇒ down is `DROP INDEX CONCURRENTLY IF EXISTS` (non-txn, idempotent); the engine asserts this on the PG render at load (a non-idempotent derived PG down is a hard authoring error).
- **SQLite leg — the requirement is vacuous, proven structurally.** SQLite has no `CONCURRENTLY` and no non-txn DDL: a SQLite `createIndex`, its derived `DROP INDEX` down, a column rebuild, and every other SQLite DDL run inside a transaction. The `concurrently` flag is dropped when lowering to SQLite, so a `createIndex{concurrently:true}` op lowers to a txn-safe `DROP INDEX [IF EXISTS]` down. The IrAuthor rejects any attempt to mark a SQLite-rendered step `transactional:false`, so a non-txn SQLite down cannot exist.
- **Fixture:** a `createIndex{concurrently:true}` op lowers to a non-txn `DROP INDEX CONCURRENTLY IF EXISTS` down on PG (asserted to pass `validate_non_txn_idempotent`) and to a txn-safe `DROP INDEX IF EXISTS` down on SQLite (asserted `transactional:true`).

#### 2.1.2 Plan-level rollback (across interleaved reversible + irreversible steps)

Existing rollback operates per `Migration`: `rollback_one_transactional` (`executor.rs:2947`), reached via `rollback`/`rollback_locked` (`executor.rs:2562`/`:2891`). A `AppliedPlan` is a sequence of steps, several of which (`Backfill`, `Dml`, an in-flight `OnlineRename`) are `down: None`. So plan rollback cannot be "reverse-apply every step":

- **Rollback executes step downs in reverse step order**, each via the existing per-`Migration` rollback path (`rollback_one_transactional`) for `Ddl` steps and the contract→expand reversal for a fully-applied `OnlineRename` step. Only the plan-level driver that walks the steps in reverse is new (PR0).
  - **Why reverse step order is referentially safe.** Each forward step's effects are fully undone by its own `down` before the prior step's `down` runs. For step 1 `addColumn A`, step 2 `addForeignKey FK→A`: reverse-order rollback runs `dropConstraint FK` first, then `dropColumn A` — the constraint that depends on A is removed before A is dropped. A step S_k can only reference objects created by S_1..S_{k-1}, so undoing S_k before S_1..S_{k-1} never leaves a dangling reference. Reverse insertion order is the correct dependency order for downs; no separate dependency analysis is needed.
  - **Fixture:** an all-reversible plan `[addColumn A, addForeignKey FK→A, createIndex on A]` rolls back via reverse-order downs (`dropIndex, dropConstraint FK, dropColumn A`) on a real PG, asserting the FK is dropped before its referenced column.
- **Hard-stop at the first `None`-down step.** Walking reverse, the moment the driver reaches a step with `down: None` (any `Backfill`, any `Dml`, an in-flight `OnlineRename`) it **stops** and refuses to proceed further. The steps already rolled back (the reversible tail after the irreversible boundary) stay rolled back; everything at/before the boundary is left in place. The driver reports exactly which step blocked and why.
- **A plan is `rollbackable` only if every step has a `down`.** Computed and surfaced **at plan time** (load): the loader sets `plan.rollbackable: bool`, and `status`/`rollback` shows a plan as non-rollbackable by construction before the operator attempts it. Any plan containing a `Backfill`, a `Dml`, or an in-flight `OnlineRename` is non-rollbackable unless the author supplied a complete module-level `down(op)` that itself lowers to an all-reversible plan. The recovery posture for a non-rollbackable plan is **roll-forward**.
- **Author-supplied `down(op)` is its own plan.** It lowers to its own `AppliedPlan` (with its own DDL + reverse-backfill steps), applied as a forward plan via `apply_plan`, subject to this same rollbackability analysis.

### 2.2 The IR stores *ops*, not SQL — rendering is the engine's job

**`.ir.json` does NOT contain SQL** for portable ops. It contains the logical op (`{ "op": "addColumn", "table": "users", "column": "email", "type": "string", "nullable": false }`). The engine renders the dialect SQL at load time through `DdlEmitter` — the exact same code path the declarative differ uses (`declarative.rs:3810`). This is what makes one artifact apply to both backends: the artifact is dialect-neutral; the engine lowers it.

The **one exception** is `op.raw`, which carries dialect-specific SQL strings by definition. Those strings are stored verbatim, and the chosen dialect's string becomes the `up`. The `pg` string is parse-guarded by libpg_query; the `sqlite` string cannot be parse-guarded and is **refused fail-closed in the `Confined` profile creators run under** — i.e. `op.raw({ sqlite })` is unavailable to creators on the dev tier (§8.7, §9).

> **Why not store rendered SQL in the IR?** Because then the artifact would be dialect-pinned (a `pg.ir.json` and a `sqlite.ir.json`), and the checksum would diverge per dialect. By storing ops and rendering at load, one artifact + one checksum covers both backends. Drift between dialects is structurally impossible because they are rendered from the same op list by the same emitter pair.

### 2.3 The op vocabulary (closed set)

The op set mirrors the existing Rust IR `AuthorRequest` variants (`crates/zeroship-migrate/src/author.rs:79`), the declarative differ's emitted operations, the backfill spec (`crates/zeroship-migrate/src/backfill.rs:76`), and the expand-contract online intent (`crates/zeroship-migrate/src/expand_contract.rs:70`). The closed set is the contract; adding an op means adding it on both sides in one PR (§2.5).

> **Reading the "Render seam" column.** The DDL render seams are split across three distinct entry points — not all `DdlEmitter` (which has exactly 5 methods, all taking `Snapshot` types — `declarative.rs:3810`):
> 1. **`zeroship_schema::query` CREATE-TABLE emitter** (consumed by `DeclarativeAuthor`, `declarative.rs:~430-565`) — new-table DDL incl. inline columns/constraints/indexes. `DdlEmitter` has no `create_table` method.
> 2. **`DdlEmitter` trait** (`declarative.rs:3810`) — the 5 ops it renders: `add_column`, `create_index`, `drop_table_up`, `drop_column_up`, `drop_index_up`. Methods take `ColumnSnapshot`/`IndexSnapshot`, so `IrAuthor` must **construct snapshots** from op fields (net-new lowering, §6).
> 3. **`DeclarativeAuthor` render methods** (e.g. `render_alter_column_type` `declarative.rs:3609`) — `ALTER` ops, PG-only, snapshot-driven.
>
> Constraint ops against an existing table and `alterColumn*` have no standalone `DdlEmitter` method today; stand-alone render coverage is net-new Rust (§6).

**Note on identifiers.** All `table`/`column`/`name`/`from`/`to` fields below are **plain strings** in the op, both in the `.ir.json` and in the TS surface (§3). They are not bound to the live schema; their existence is checked at apply time against the real DB (§3.3).

**DDL ops — portable (render on BOTH dialects):**

| `op` | Fields | Render seam (actual) | SQLite path |
| --- | --- | --- | --- |
| `createTable` | `name, columns[], constraints[], indexes[]` | `zeroship_schema::query` CREATE-TABLE emitter via `DeclarativeAuthor` (`declarative.rs:~430-565`) | native CREATE (inline constraints/indexes) |
| `dropTable` | `table, { ifExists?, cascade? }` | `DdlEmitter::drop_table_up` (`declarative.rs:3936`) | qualification differs only |
| `addColumn` | `table, column, type, { nullable?, default? }` | `DdlEmitter::add_column` (`declarative.rs:3865`); IrAuthor builds a `ColumnSnapshot` | native ADD COLUMN |
| `dropColumn` | `table, column, { ifExists? }` | `DdlEmitter::drop_column_up` (`declarative.rs:3940`) | **rebuild** if column is constrained (§6.3) |
| `createIndex` | `table, columns[], { name?, unique?, using?, where?, concurrently? }` | `DdlEmitter::create_index` (`declarative.rs:3898`); IrAuthor builds an `IndexSnapshot` | FTS5 vtable is the `engine_goodie_ddl` special case |
| `dropIndex` | `name, { table?, ifExists?, concurrently? }` | `DdlEmitter::drop_index_up` (`declarative.rs:3948`) | unqualified on SQLite |

**DDL ops — Postgres native / SQLite-rebuild (engine routes automatically):**

| `op` | Fields | PG path | SQLite path |
| --- | --- | --- | --- |
| `alterColumnType` | `table, column, type, { using? }` | `render_alter_column_type` (`declarative.rs:3609`, gated/destructive, `down: None`) | 12-step rebuild via the rebuild planner (§6.3) |
| `alterColumnNullability` | `table, column, nullable` | `render_alter_column_nullability` (`SET`: two-step `CHECK NOT VALID` + `VALIDATE`) | 12-step rebuild |
| `renameColumn` | `table, from, to, type: ColType` (neutral) | IrAuthor maps neutral `ColType`→PG type, then online expand-contract — `ExpandContractAuthor` lowers `OnlineIntent::RenameColumn{ty:<pg-type>}` (`expand_contract.rs:70/:82`) | IrAuthor maps neutral `ColType`→SQLite affinity, then **rebuild planner** (NOT `ExpandContractAuthor`) — §2.6 |
| `addConstraint` / `dropConstraint` | `table, kind(pk\|fk\|unique\|check), …` | `ALTER TABLE ADD/DROP CONSTRAINT` (net-new stand-alone render — §6) | inline at create / rebuild for existing (net-new — §6) |

**DML ops — the portable data-migration surface.** All four require net-new creator-DML Rust (§6): no creator-facing INSERT/UPDATE/DELETE assembler exists today (the only `INSERT INTO` strings in the engine are internal journal/supersedes/author-fixture statements — `executor.rs:1951`, `author.rs:614`).

| `op` | Fields | Engine assembly (net-new) | Portable? |
| --- | --- | --- | --- |
| `insert` | `table, columns[], rows[]` | NEW assembler → a `PlanStep::Dml { template, binds }` step (§2.3.2). Rows are a typed `values` facet (not inlined literals); the assembler emits `INSERT INTO <schema>.<t> (cols) VALUES ($1,$2,…)` with parameterized binds. NULL/typed-literal handling; guard (PG) / authorizer (SQLite); migrator role. The journal hashes the parameterized template; the bind values fold into the plan checksum (§2.4, §2.3.2). | portable for literal rows. `onConflict`/upsert is PG-only; on SQLite it is a hard authoring error (§9) |
| `update` (one-shot) | `table, set{}, where?` | NEW assembler → a `PlanStep::Dml` step: `UPDATE … SET … WHERE …`; the portable `set`/`where` body is validated against the §3.3.1 grammar | portable for portable `set`/`where` (§9) |
| `update { batch }` / `backfill` | `table, cursorColumn, batchSize, set{}, filter?, name` | lowers to a `Backfill(BackfillSpec)` plan step (`backfill.rs:76`), run by `run_backfill` (PG) / the new SQLite executor (§2.3.1) | portable on BOTH backends (PG via the existing executor; SQLite via the committed §2.3.1 executor) |
| `delete` | `table, where` (mandatory), `{ limit? }` | NEW assembler → a `PlanStep::Dml` step: `DELETE FROM … WHERE …`; `destructive: true` derived; `where` required (no accidental full-table delete) | portable |

**The escape hatch:**

| `op` | Fields | Behavior |
| --- | --- | --- |
| `raw` | `{ pg?: string, sqlite?: string, down?: { pg?, sqlite? } }` | the chosen dialect's string becomes `up`/`down` verbatim. **PG string**: parse-guarded by libpg_query deny-list (§8.3) + migrator role. **SQLite string**: refused fail-closed in `Confined` (libpg_query cannot parse it — `guard.rs:165-172`); creators **cannot use `op.raw({ sqlite })`** (§8.7). If a dialect string is required by the target backend and is absent/unavailable, the engine fails closed (no silent skip). |

#### 2.3.1 The SQLite backfill executor (net-new, committed)

The headline "one script, both backends, DDL+DML" requires this; it is committed scope (PR6b). The existing executor (`backfill.rs:286-330`) is structurally Postgres-only:
- It emits a data-modifying CTE (`WITH _bf_window AS (…), _bf_upd AS (UPDATE … RETURNING …)`) — SQLite has no writable CTEs.
- It derives `cursor_type` from `pg_catalog` introspection (`backfill.rs:283`, `resolve_cursor_type` `:488`) — no SQLite arm.
- `backfill.rs` contains zero `Sqlite`/`dialect` references.

The SQLite executor is a loop of plain statements, not a writable CTE:
```sql
-- per batch, inside a transaction:
UPDATE "<table>" SET <set_clause>
  WHERE rowid IN (
    SELECT rowid FROM "<table>"
     WHERE <cursor_col> > :cursor AND (<filter>)
     ORDER BY <cursor_col> ASC LIMIT :batch
  );
-- then SELECT max(<cursor_col>) over the just-touched window for the next :cursor
```
It must be type-affinity-aware for the cursor cast (SQLite affinity rules, not `pg_catalog`), persist crash-safe progress in the same meta backfill-progress table the PG path uses (`backfill.rs:340`), and pass the assembled statement through the SQLite authorizer (no libpg_query).

#### 2.3.2 On-artifact representation of `insert` rows (the values facet)

`insert` must not inline row values as literals into the `up` string — that would reintroduce the injection surface the migrator-role + assembler exist to remove, and a multi-thousand-row seed would bloat one `up`. So:

- **In the `.ir.json`**, an `insert` op carries a structured `values` facet: `{ "op": "insert", "table": "...", "columns": ["a","b"], "rows": [[<lit>,<lit>], …] }`, where each `<lit>` is a typed JSON scalar — never a SQL string. Per the §2.5 constrained numeric domain, a numeric `<lit>` is an exact `i64` integer or a decimal-string (`{ "decimal": "…" }`); a fractional/exponential JS `number` or an integer ≥ `2^53` is **rejected at record time** (use a `bigint` literal or decimal-string). Transform expressions on the RHS of an `update`/`backfill` `set` remain opaque `op.sql` strings (§3.3), but `insert` row *values* are always typed scalars.
- **At lowering**, the assembler emits one (or, for large seeds, a chunked sequence of) parameterized `INSERT … VALUES ($1,$2,…)` statement(s) into a `PlanStep::Dml { template, binds }` step. The `Dml` variant carries the parameterized statement template (placeholders, no values — what the journal stores/hashes) and the typed `binds` alongside.
- **Checksum.** The bind values fold into the plan checksum via the canonical IR op list (§2.4) — an `insert`'s `rows` array is canonicalized (stable column order, canonical scalar serialization) and length-prefix-folded (§2.4 point 2), and an `update`/`delete`/`backfill` `op.sql` fragment's interpolated `${v}` binds fold via the fragment-node rule (§2.4 point 3, with the same typed-scalar canonicalization as the `rows` facet). So changing a seed value OR an interpolated threshold value is drift, and the checksum is dialect-stable because the template + typed values are dialect-neutral (per-dialect rendering only affects `$n` vs `?n`, which is not hashed). A fixture asserts two migrations differing only in an interpolated `op.sql` bind value have different `Checksum::of_ir`.
- **Large seeds.** Row count above a chunk threshold splits into multiple `PlanStep::Dml` steps within the one plan, each a bounded parameterized multi-row insert. All chunks share the one plan version/checksum (§2.0.1).
- **Binds MUST use the driver's native parameter protocol on BOTH backends — never string interpolation.** The guard/authorizer screens the template (placeholders, no values); the `binds` are bound through the driver's real parameter protocol (`$1,$2,…` via `compio-postgres` on PG; `?1,?2,…` via the SQLite driver) and are never interpolated. A value containing a quote, semicolon, or comment cannot alter statement shape. This applies to the SQLite DML assembler exactly as to PG; an interpolating assembler voids the guarantee and is a design violation.

### 2.4 Flags & checksum — the load-bearing contract

The IR must produce a `MigrationFlags` (`migration.rs:128`) and a `Checksum` byte-identical to what the engine would compute, because the checksum is the tamper-evidence + drift anchor (`migration.rs:261`; drift abort at `executor.rs` first pass). Two rules:

1. **Flags are orthogonal facets — 6 bools + 2 `Option` fields — derived-then-overridable.** `MigrationFlags` (`migration.rs:128-185`) has **8 fields, but they are NOT all bools**: 6 are `bool` (`transactional`, `destructive`, `online`, `requires_approval`, `repeatable`, `engine_goodie_ddl`) and 2 are optional facets — `timeout_ms: Option<u64>` and `phase: Option<OnlinePhase>` (kept optional + separate precisely so the bools stay orthogonal, `migration.rs:124-185`). The builder derives the conservative default from the ops (`dropTable`/`dropColumn`/`delete`/`alterColumnType`-narrowing ⇒ `destructive: true`; `createIndex { concurrently: true }` ⇒ `transactional: false`; `renameColumn` ⇒ `online: true` + `phase: Some(Expand|Contract)`), then applies explicit `flags` overrides. This mirrors `RawSqlAuthor` (`author.rs:354`) and the loader's `flags_for_file_opts`. All 8 fields are emitted in full, and the checksum fold covers all 8 via `serde_json::to_string(input.flags)` at `migration.rs:325`.
   - **The derive-then-override MERGE RULE, made precise for the two `Option` fields.** "Unset" for an `Option` field means **absent in the `.ir.json` `flags` object** (JSON key omitted), not `null`. The merge is **replace, not merge**: for every field, the explicit `flags` override (if the key is present) *replaces* the derived value; if the key is absent, the derived value stands.
     - For the 6 bools: an explicit `false`/`true` replaces the derived bool (an author may, e.g., force `transactional: true` over a derived `false` and accept the executor's non-txn-idempotency consequences).
     - For `timeout_ms`/`phase`: a present key (including an explicit `null`, which deserializes to `None`) replaces the derived value; an absent key keeps the derived value. So an author raising a long backfill's ceiling writes `"timeout_ms": 600000` (replaces the derived `None`); omitting the key keeps the derived `None` (the executor falls back to `ExecutorConfig::statement_timeout`). `phase` is engine-derived for `renameColumn` and is **not** author-overridable — an explicit `phase` in `flags` on a non-online op is rejected at load (`EXPR_*`-class authoring error), because `phase` is meaningful only as the expand/contract tag the rename lowering stamps.
   - **Drive-by cleanup for PR1 — bring the `:226` doc-comment into line with the already-correct `:321` one.** The `ChecksumInput.flags` doc-comment at `migration.rs:226` enumerates only 7 fields (omitting `engine_goodie_ddl`) **and** mislabels `timeout_ms`/`phase` as "flags" alongside the bools. The fold comment *inside* `Checksum::of` at `migration.rs:321-323` is ALREADY correct — it names all 8 fields including `engine_goodie_ddl`. So the two adjacent comments are internally inconsistent, and the fix is **one-directional**: edit only the `:226` doc-comment to match `:321-323` (list all 8 fields; mark `timeout_ms`/`phase` as the two optional facets, not bools). Do NOT touch `:321-323` (already correct) and do NOT touch the fold at `:325` (already folds the whole struct via serde) — touching either would introduce a fresh mismatch.

2. **The checksum is computed by the engine — not by the JS builder — over a canonical-IR input shape.** The JS builder does not reimplement the SHA-256 length-prefixed folding. It emits ops; the engine computes the checksum. There is exactly one checksum implementation, in trusted Rust. The `.ir.json` may carry a `checksum` field as an integrity hint, but the engine recomputes and is authoritative; the hint is advisory and need not be present.
   - **`owner_app` is excluded from the hint.** Because `owner_app` is always server-stamped (§8.6 — a spoofed/absent artifact value is discarded and overwritten with the deploying app's id before hashing), a builder cannot reliably predict it. So the hint, if emitted, covers only `ops` + `flags` + `depends_on` + `supersedes` + `preconditions` — never `owner_app`. The engine's authoritative checksum that enters the journal does include the server-stamped `owner_app` (`migration.rs:330`). The engine recomputes the hint-domain checksum (everything except `owner_app`) and compares to the hint; a hint-domain mismatch is a hard error (genuine drift). Stamping `owner_app` never moves the hint, so there is no "expected mismatch" case.
   - **Mechanism: one shared `fold_common` folder, two front-end inputs — NOT a new `ChecksumInput` variant.** `ChecksumInput` is a **struct**, not an enum (`migration.rs:220`: `pub struct ChecksumInput<'a> { up: &str, down: Option<&str>, flags, owner_app, depends_on, supersedes, preconditions }`), and its first two fields are rendered SQL (`up: &str` `:222`, `down: Option<&str>` `:224`) — exactly what the IR path must not hash, because `up`/`down` differ per dialect. The change is:
     1. **Extract the shared tail of `Checksum::of` (`migration.rs:303`) into `fold_common(hasher, flags, owner_app, depends_on, supersedes, preconditions)`** — the part that folds everything except `up`/`down` (the existing `:322`–`:340` body). A pure refactor with no byte change to existing output.
     2. **Keep the existing rendered-SQL front door**: `Checksum::of(ChecksumInput)` folds `up` then `down` (`:308`–`:321`) and then calls `fold_common`. The struct, `from_migration` (`:243`), and every existing caller (the `.sql` loader path, squash, drift re-derive at `migration.rs:613`) are unchanged.
     3. **Add a parallel canonical-IR front door**: `Checksum::of_ir(canonical_ops: &CanonicalOpList, flags, owner_app, depends_on, supersedes, preconditions)` folds the canonicalized dialect-neutral op list (stable field ordering, canonical literal serialization, the §2.3.2 `rows`-facet canonicalization) in place of `up`/`down`, then calls the **same `fold_common`**. Both front doors share one folding tail — provably one `Sha256` implementation.
   - **`CanonicalOpList` — the typed, dialect-neutral input `Checksum::of_ir` folds (defined here).** `CanonicalOpList` is the named type the golden-fixture value-equality gate (§2.5) hinges on, so it has a concrete shape:
     ```rust
     /// The canonical, dialect-neutral op list Checksum::of_ir folds. It is the
     /// deserialized `Op` vec (§2.3), NOT rendered SQL — so it is identical across PG/SQLite.
     pub struct CanonicalOpList<'a>(pub &'a [Op]);
     ```
     Its canonicalization contract — the byte sequence `Checksum::of_ir` folds it into — is:
     1. **Per-op canonical encoding via RFC 8785 JCS** (§2.5 / §4.3): each `Op` is serialized through the Rust JCS path (stable lexicographic field ordering, canonical scalar serialization), so a portable op like `addColumn` contributes its JCS bytes, not any dialect SQL.
     2. **The §2.3.2 `rows` facet** of an `insert` op canonicalizes its `rows` as a length-prefixed sequence of rows, each a column-ordered (stable, by the op's declared `columns` order) sequence of canonical typed scalars (`i64` / decimal-string / string / bool / null / base64-bytes — never a JS float; §2.5 numeric domain). The typed values fold in, so changing a seed value is drift.
     3. **`op.sql` fragment nodes (interpolated binds fold in).** A `set`/`filter`/`where` fragment authored via `op.sql\`…${v}…\`` records a fragment template plus its ordered typed binds (§3.3.1.2). The canonical encoding of such a fragment folds **both** the fragment template text (length-prefixed) **and** its ordered typed binds — each bind canonicalized as the same typed scalar the §2.3.2 `rows` facet uses (`i64` / decimal-string / string / bool / null / base64-bytes; never a JS float). So two migrations differing only in an interpolated `${threshold}` value have different `Checksum::of_ir` — the "changing a seed/threshold value is drift" guarantee holds for fragment-embedded binds, not only for the `rows` facet. (`op.fn.*` helper nodes are structured `FnSynth` data, §3.3.1.2, and fold via their JCS encoding like any other op field.)
     4. **The §2.4 mixed-raw hybrid:** an `op.raw` op contributes **both** its `pg` and `sqlite` strings (length-prefixed, in that fixed order), so a change to either dialect string is drift; a portable op contributes its JCS bytes. The list is folded **in op order**, each element length-prefixed (the same length-prefixed-fold discipline `Checksum::of` already uses for `up`/`down`), so reordering or inserting an op changes the checksum.
     5. **Scope of the JCS claim — the op-list region only, NOT `fold_common`'s tail.** The whole folded op-list region (1–4) stands **in place of** the `up`/`down` region of `Checksum::of`, and that region IS RFC 8785 JCS-canonical. `fold_common` then folds flags/owner/deps/supersedes/preconditions **using the EXISTING `Checksum::of` discipline — `serde_json::to_string(input.flags)` (`migration.rs:325`), which emits fields in serde struct-declaration order with NO key sorting, i.e. NOT JCS** — byte-for-byte identical to the `.sql` path's tail. So `of_ir` deliberately MIXES a JCS region (the op list) with a non-JCS region (the `fold_common` tail); this is correct and required for byte-equality with `Checksum::of`. The "RFC 8785 JCS-canonical / invariant under any JCS-formatting difference" claim (§2.5, §4.3) is scoped to the op-list region of `of_ir` and to the on-disk `.ir.json` bytes — it does NOT apply to `fold_common`'s flags/owner/deps tail, which an implementer MUST leave at the existing serde discipline. (This matters only to the implementer reproducing the fold; the flags are recomputed by the engine from the typed struct, so a JS builder cannot influence the checksum via them regardless.)
   - **PR1 fixture obligation:** a fixture asserts the `.sql` path's checksum for a fixed `up`/`down`/flags/owner is byte-unchanged after the `fold_common` extraction (a golden hash compared pre/post-refactor).
   - **Mixed portable + `op.raw` migrations** use a hybrid input to `Checksum::of_ir`: portable ops contribute their canonical IR; `op.raw` ops contribute both the `pg` and `sqlite` strings (so a change to either is drift). The whole list is folded in order, then `fold_common` folds flags/owner/deps/supersedes/preconditions.

**`dialect_scope` — the SOLE portability signal, journaled separately from the identity checksum.**
- **The checksum is an IDENTITY / tamper anchor — NOT a portability certificate.** A dialect-stable checksum means only "this is the artifact, unedited"; for a `PgOnly` migration its dialect-stability is vacuous. The single authoritative answer to "does this apply on both backends?" is `dialect_scope` (`Both` vs `PgOnly`), and nothing else.
- **`dialect_scope` is DERIVED and version-dependent, so it is NOT folded into the identity checksum — it is journaled as a SEPARATE column.** `dialect_scope` is a function of the ops AND the engine's allow-list/helper version. A previously-`PgOnly` op can become portable when the allow-list broadens — concretely, adding `instr` to the SQLite authorizer allow-list (§9) can flip a helper's verdict. If `dialect_scope` were in the identity hash, an engine upgrade would change an already-applied migration's checksum and trigger a spurious drift abort. So:
  - **The identity checksum (`Checksum::of_ir`) folds ONLY the immutable artifact** — the canonical ops + flags + owner_app + deps + supersedes + preconditions. It is invariant under allow-list/helper evolution.
  - **`dialect_scope` is journaled as the APPLY-TIME verdict (immutable, stamped once at apply)**, alongside the allow-list/helper version it was computed under. An applied `PgOnly` migration's journal row always reads `PgOnly` (the truth at apply: it deployed PG-only and was never SQLite-tested), never silently flipping to `Both` on a later engine upgrade.
  - **`status` exposes a SEPARATE, clearly-labelled COMPUTED current-portability field** — `current_portability_under_engine_vX` (the verdict recomputed against the current allow-list version, labelled with that version), shown beside — never overwriting — the immutable journaled apply-time `plan_dialect_scope`. An operator sees both: "applied as `PgOnly` under helper-set v3" (history) and "now `Both` under helper-set v4" (current, computed). A re-verdict to `Both` does not mean it was tested on SQLite — re-running it on SQLite dev still requires a dry-run (§8.8).
  - **Verdict travels with the checksum by co-location, not hashing.** They are columns on the same journal row, returned by the same `status`/integrity query, so an operator cannot observe a checksum in isolation and infer portability.
  - **Fixture:** broaden the allow-list (add `instr`) and re-load an already-applied migration; assert no drift abort (identity checksum unchanged) and assert the `current_portability` field may legitimately update from `PgOnly` to `Both` while the journaled apply-time column stays `PgOnly`.

### 2.4.1 Lowering/validation is parameterized by the deploy-target dialect

`dialect_scope` is an intrinsic property of the artifact's ops and is computed at lower time. But the **rejection** is not intrinsic: the identical `.ir.json` must load-and-lower when the deploy target is Postgres and hard-fail when it is SQLite. That decision depends on the deploy target, known only at deploy time — so load/lower **receives the target dialect as a parameter**:

- **Today `load_dir` takes only a directory** (`loader.rs:606`). The IR path threads the deploy-target dialect into `IrAuthor::lower(ops, target_dialect)` (§2.6 routes PG vs SQLite on it). The plumbing source is the backend selection that already exists at deploy time: `deploy_migrate.rs` selects `PostgresBackend` (prod) vs `SqliteBackend` (dev-tier). A `.sql` migration is unaffected.
- **The two-part rule.** (1) `dialect_scope` is computed intrinsically at lower (Both vs PgOnly), journaled as a separate apply-time-immutable column (§2.4). (2) The **deploy-target gate** asserts `!(dialect_scope == PgOnly && target == Sqlite)` — a `PgOnly` artifact against a SQLite target is the hard error; the same artifact against a PG target loads fine. The gate lives in the IR loader/`IrAuthor` because the target is a parameter there.
- **Portability is discovered at AUTHOR/CI time against BOTH targets unconditionally** — not deferred to whichever target a given deploy uses. Production is PG (the permissive target): if the only check were per-deploy, a `PgOnly` migration would deploy cleanly to prod and silently break the dev (SQLite) tier. Modeled on validating against the full target matrix at proposal time:
  - The CI gate (the §5.1 consistency gate, extended) lowers every not-yet-applied `.ir.json` with `target_dialect = Postgres` and with `target_dialect = Sqlite`; if the SQLite lowering fails, the artifact is flagged `dialect_scope = PgOnly` at CI/author time and the AI loop is told *then* (a structured `EXPR_NOT_PORTABLE`/`DIALECT_SCOPE_PGONLY` payload, §8.8).
  - The AI builder's both-backends dry-run (§8.8) is the author-time enforcement: an AI migration is `PgOnly`-by-default until it passes the SQLite lowering + dry-run.
  - The deploy-target gate only re-confirms what CI established; it is the defense-in-depth backstop, not the first time anyone learns the migration is PgOnly.
- **Signature-change list (added to §5.2 / §6).** The IR loader branch and `IrAuthor::lower`/validate take a `target_dialect: Dialect` argument threaded from `deploy_migrate.rs`'s backend selection; the dbmate CLI's `validate`/`apply` pass the `--engine`-selected dialect.
- **Fixtures:** the identical `op.raw({pg})`-only `.ir.json` loads successfully with `target_dialect = Postgres` and is rejected at load with `target_dialect = Sqlite`; a CI dual-target lowering of an `op.raw({pg})`-only artifact fails the author-time portability gate (flagged `PgOnly` with a structured payload) without any deploy; the golden corpus includes (a) a portable migration whose checksum is identical across the PG and SQLite renders and (b) a `PgOnly` migration whose load sets `dialect_scope = PgOnly` and whose SQLite deploy is rejected at load.

### 2.5 Single-source-of-truth: no builder↔engine drift

**The source of truth is a golden-fixtures value-equality gate, NOT the codegen chain.** Two mechanisms, in priority order:

1. **Authoritative — golden `.ir.json` fixtures, with a named canonicalization and a value-level hard gate.** A corpus of `.ir.json` files is loaded by both the JS builder (it emits JSON for the equivalent `op.*` source) and the Rust engine (it deserializes + renders + applies on PG and SQLite).
   - **Canonicalization standard: RFC 8785 JCS for the OP-LIST region**, implemented on both sides (a JCS serializer in `@zeroship/migrate`, and the Rust JCS path that `Checksum::of_ir` recanonicalizes the op list through, §4.3 mechanism 4). One spec to conform to for the op list, not two ad-hoc "sorted keys" conventions. (The `fold_common` tail — flags/owner/deps/supersedes/preconditions — is NOT JCS; it uses the existing `Checksum::of` serde discipline, §2.4 point 5. JCS governs the dialect-neutral op list and the on-disk `.ir.json` bytes only.)
   - **Authoritative hard gate = the typed-value checksum.** Each fixture is deserialized by Rust into the typed `Op` list and `Checksum::of_ir` is computed over the Rust-recanonicalized op list (plus the serde-folded `fold_common` tail); the JS builder's recorded op list, parsed the same way, must yield the same checksum. This is invariant under any JCS-formatting difference between the two serializers — what is gated is value equality.
   - **Advisory canary = raw-byte equality.** A separate, non-blocking-for-deploy CI canary asserts the JS-emitted `.ir.json` bytes equal the Rust-re-emitted JCS bytes for each fixture. A failure flags a JCS-implementation divergence to fix, but cannot brick a deploy.
   - **Edge-case fixtures.** The corpus explicitly exercises cross-implementation footgun shapes: a large integer beyond JS safe-integer range carried as a typed scalar, an integral-valued float (`1.0` vs `1`), negative zero, a number in exponential range (`1e10`), and identifiers/string literals containing non-ASCII/unicode + escape characters. Each is asserted to (a) produce the same `Checksum::of_ir` on both sides and (b) round-trip through both JCS serializers.
   - **The IR scalar numeric domain is CONSTRAINED at record time**, so the typed-value checksum cannot diverge on an unfixtured number: a numeric `<lit>` is an exact `i64` or a decimal-string (`{ "decimal": "…" }`); a fractional/exponential JS `number` or an integer ≥ `2^53` is **rejected at record time** on the JS side and **rejected at load by the Rust deserializer** (`EXPR_INVALID_NUMERIC`) before `Checksum::of_ir` runs. So a row value cannot parse to a different typed scalar on the two sides.
2. **Best-effort ergonomics — codegen.** TS types generated from `op-ir.schema.json` (`schemars` → `json-schema-to-typescript`), with manual types for any serde shape codegen cannot express. The fixtures, not the codegen, are authoritative.

3. **Variant-exhaustiveness gate.** A CI test derives the `Op` variant list from `op-ir.schema.json` and fails if any variant has zero fixtures.

### 2.6 Online ops are emitted as *intent*, lowered by the engine

The builder emits `renameColumn` carrying a **dialect-neutral `ColType`** (the §3.2 lexicon, e.g. `"string"`/`{ ref }`/`{ vector: n }`), not a raw dialect type string and not the Postgres-specific expand-contract plan. The builder authors no triggers, dual-write functions, or dependency edges; lowering is the engine's job.

**The neutral-`ColType` → PG-type / SQLite-affinity split.** `OnlineIntent::RenameColumn.ty` is, in today's code, "the Postgres type of the column (emitted verbatim)" (`expand_contract.rs:82`) — a raw PG type string. The IR must not carry that PG string; it carries the dialect-neutral `ColType`, and `IrAuthor` renders it per dialect at lowering, using the same type-mapping the `DdlEmitter`/`t.*` lexicon applies for `addColumn`:
- **PG lowering** ⇒ `IrAuthor` maps the neutral `ColType` to its PG type string and constructs `OnlineIntent::RenameColumn { ty: <pg-type> }`, which `ExpandContractAuthor::author` emits verbatim (unchanged — the neutral→PG mapping happens before it, in `IrAuthor`).
- **SQLite lowering** ⇒ `IrAuthor` maps the neutral `ColType` to a SQLite affinity type and feeds it into the rebuild planner's desired-column set — it never passes a PG type string to the SQLite leg.

No change to `OnlineIntent` is required; the neutral→PG / neutral→affinity translation is part of the net-new `IrAuthor` rename lowering (§6, PR2).

**The cross-subsystem bridge (net-new in `IrAuthor`).** The two lowering targets live in different subsystems not wired together today:
- **Postgres**: `ExpandContractAuthor::author` (`expand_contract.rs:285`) lowers the intent into E1→E2→E3→C1→C2, deriving `depends_on` from the chain. It has no dialect parameter and no SQLite arm.
- **SQLite**: the 12-step rebuild is produced by the declarative differ's rebuild planner (`sqlite_existing_table_needs_rebuild` `declarative.rs:3110` → `SqliteRebuildSpec` in `backend_sqlite/rebuild_sql.rs`), reached via `DeclarativePlan.rebuilds` — not via `OnlineIntent`.

Therefore `IrAuthor::lower(rename_op, dialect)` routes by dialect (new code):
- `dialect == Postgres` ⇒ map neutral `ColType`→PG type, build `OnlineIntent::RenameColumn{…, ty: <pg-type>}`, call `ExpandContractAuthor::author(…)`, set `online: true` + `phase`.
- `dialect == Sqlite` ⇒ bypass `ExpandContractAuthor` entirely, map neutral `ColType`→SQLite affinity, synthesize the rebuild input the planner consumes (the desired post-rename column set), invoking the SQLite rebuild planner to emit the 12-step sequence (offline, `online: false`).

What is reused is the *destination* code on each side; what is new is the dialect router connecting a single `renameColumn` op to the correct one.

#### 2.6.1 One `renameColumn` op → one `OnlineRename` plan step (cardinality, journal, down)

A single `op.renameColumn` lowers to **one `OnlineRename(RenameStep)` plan step** (§2.0) whose `RenameStep` is dialect-chosen at lowering: on PG it is `RenameStep::PgExpandContract(ExpandContractPlan)`, that plan being the five `Migration`s E1→E2→E3→C1→C2 plus the `BackfillSpec` that `ExpandContractAuthor::author` produces (`struct ExpandContractPlan` `expand_contract.rs:104`, fields `:106-121`; `ExpandContractAuthor::author` `:285`).

- **Sub-versions — `ExpandContractAuthor` is the id authority; the IR plan does NOT re-mint them.** `PgOnline::run_online` ignores `_intent` and runs the pre-authored, version-stable `expand` steps verbatim (`expand_contract.rs:777-783`: "re-authoring would mint fresh ids and diverge from the stamped manifest"). So `IrAuthor::lower` calls `ExpandContractAuthor::author(intent)`, and the returned `ExpandContractPlan` — with its already-stamped E1..C2 `version`s and intra-chain `depends_on` — is wrapped **verbatim** into `RenameStep::PgExpandContract(plan)`. The IR path inherits the author's ids by construction. They are sub-steps under the one plan version (§2.0.1); the `.ir.json` still has one `<NNNN>` filename.
- **Set-integrity manifest reconciliation (Task #51).** Because the IR path stamps the rename's E1..C2 via the same `ExpandContractAuthor` the declarative path uses, the ids the set-integrity manifest records are identical to those for the equivalent `t.*`-diff-authored rename. A fixture asserts this id-equality.
- **Journal.** Each of E1..C2 journals independently (a crash between E2 and the backfill resumes correctly: `pending_contract` `engine.rs:521` holds C1/C2 until the backfill journal row lands, `:550`). The IR path inherits this behavior through the shared `apply_plan`.
- **Checksum.** One plan checksum over the canonical op list (the single `renameColumn` op + fields), §2.4 — not five per-`Migration` checksums and not the rendered E1..C2 SQL. The five-way explosion is a lowering detail, invisible to the drift anchor.
- **`depends_on`.** Cross-plan deps attach to E1; the E1→C2 intra-chain deps are the ones `ExpandContractAuthor` derives.
- **SQLite.** No five-way explosion: the SQLite leg is a single offline `OnlineRename(RenameStep::SqliteRebuild(declarative::SqliteRebuild))` step (the 12-step rebuild, `online:false`). The op is still one op; only the PG lowering fans out. The SQLite leg is a different executable shape and a different execution interface (§2.6.2).
- **Fixture:** an `op.renameColumn`'s E1..C2 ids (and intra-chain `depends_on`) are byte-equal to the ids the declarative `t.*`-diff path produces for the equivalent rename.

#### 2.6.2 The dual-EXECUTION dispatch for an `OnlineRename` step

PG and SQLite execute a rename through two entirely unrelated engine interfaces:

- **PG online execution** = the `OnlineSchemaChange` trait, whose **only** impl is `PgOnline` (`expand_contract.rs:760`), driven by `run_online` (`expand_contract.rs:660`), reached from `apply_declarative_locked` via `backend.online()` → `run_online(…)` (`engine.rs:523/537`). There is **no `SqliteOnline` impl** — `SqliteBackend::online()` returns `None` (`backend_sqlite/mod.rs:567`), and the declarative path asserts a SQLite backend must receive an **empty** `plan.renames` (`engine.rs:521-532`), failing closed if not.
- **SQLite rename execution** = the 12-step table rebuild through `MigrationBackend::rebuild_one(&spec, &migration, applied_by)` (`backend.rs:262`; SQLite impl `backend_sqlite/mod.rs:550` → `rebuild_sql.rs`), driven via `plan.rebuilds` (`engine.rs:491-503`), **not** via `OnlineIntent`/`run_online`.

So a `PlanStep::OnlineRename` lowered for SQLite is a `SqliteRebuildSpec` + journal `Migration` executed by `rebuild_one`, not an `ExpandContractPlan` run by `run_online`. `apply_plan` dispatches on the `RenameStep` variant:

`apply_plan` already holds the session-scoped project advisory lock (acquired at deploy start, `engine.rs:345`) and threads the migrator connection + `ExecutorConfig` it was constructed with; it passes `LockMode::AlreadyHeld` so the inner sub-batches re-enter the held lock (`engine.rs:457`/`:544`). The exact `run_online` signature is `run_online(&self, intent, expand, backfill, approval, cfg, applied_by, lock_mode) -> Result<ApplyOutcome, OnlineError>` (`expand_contract.rs:660`):

```rust
// inside the single shared apply_plan — PR0.
// `conn`, `cfg`, `approval`, `applied_by` are apply_plan's own parameters;
// the session project lock is already held (engine.rs:345), so lock_mode = AlreadyHeld.
PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
    let online = backend.online().ok_or(/* routing bug, fail closed */)?;
    // run_online runs E1+E2 -> backfill -> E3 as ONE atomic sequence UNDER the held
    // project lock (run_expand_pg, expand_contract.rs:693): the per-batch
    // pg_advisory_xact_lock inside run_backfill is re-entrant under the session lock,
    // so the lock is NOT freed across the backfill (expand_contract.rs:678-688).
    // After this returns, EXPAND (E1..E3) + backfill are fully applied and journaled.
    let _out = online
        .run_online(&ec.intent, &ec.expand, &ec.backfill,
                    approval, cfg, applied_by, LockMode::AlreadyHeld)
        .await?;
    // The CONTRACT (C1/C2) is NOT run now — it is surfaced as pending_contract,
    // applied in a SUBSEQUENT deploy under Approval::Approved (§2.0.2).
    pending_contract.extend(ec.contract.iter().cloned());
}
PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => {
    // rb: declarative::SqliteRebuild { migration, spec } (declarative.rs:2053) — REUSED.
    // SQLite path: the SAME seam the declarative `plan.rebuilds` loop uses
    // (engine.rs:491-503). NO run_online, NO pending_contract.
    if approval != Approval::Approved { return Err(ApprovalRequired); } // mirrors engine.rs:468
    if !already_applied(&rb.migration.version) {
        backend.rebuild_one(&rb.spec, &rb.migration, applied_by).await?;
    }
}
```

Consequences:
- **EXPAND + backfill + E3 are ATOMIC inside `run_online`, under the held project lock — `apply_plan` does NOT interleave its own per-sub-batch lock cycling within an online rename.** `run_online` → `run_expand_pg` (`expand_contract.rs:693`) encapsulates the E1+E2 → backfill → E3-journaled-last sequence and runs it without freeing the project lock (`expand_contract.rs:678-688`). `apply_plan` reuses that call **opaquely**: it has no seam to acquire/release a finer lock inside the online rename, and it does not try to. The only thing `apply_plan` drives at the plan level for a PG rename is (a) calling `run_online` for EXPAND+backfill and (b) deferring the contract to `pending_contract`. The §2.0.3(1) "lock held across the whole deploy" property is therefore exactly what `run_online` already guarantees — not a new behavior `apply_plan` adds on top.
- **`apply_plan` owns BOTH execution arms by DISPATCH, not by re-implementation.** PR0 lifts the PG online arm (`engine.rs:533-552`) and the SQLite `rebuild_one` arm (`engine.rs:491-503`) out of `apply_declarative_locked` into the single shared `apply_plan`, dispatching by `RenameStep` variant. Each arm calls the existing destination (`run_online` / `rebuild_one`) unchanged. The declarative path is then re-pointed onto `apply_plan` via the shape-adapter (§6) — there is one orchestrator.
- **No `pending_contract` on the SQLite leg.** A SQLite rebuild is a single atomic offline step; the cross-deploy partition (§2.0.2) is a PG-only property of `RenameStep::PgExpandContract`.
- **Faithful-e2e proof (per `feedback_faithful_e2e_tests.md`).** A test applies a single `op.renameColumn` IR plan on a real SQLite DB and asserts it executes through `rebuild_one` (a row survives the rename, the old column is gone, the journal records the rebuild migration's version) and journals identically to the same rename run through the declarative path now routed through `apply_plan`.
- **`down` and mid-flight.** A fully-applied online rename auto-derives `down` = a fresh online rename `to`→`from`. A partially-applied expand-contract (crash between phases) has no single-statement inverse and is `down: None`; recovery is roll-forward (resume the remaining phases under the project lock). An in-flight `OnlineRename` step makes the plan `rollbackable: false`, and the plan-level rollback driver hard-stops at it (§2.1.2).

### 2.7 Preconditions & backfill are structured, never raw

- `preconditions` fold into the checksum exactly as today (`migration.rs` `preconditions` field, folded by `Checksum::of`/`fold_common`). The builder emits `PreconditionCheck` JSON; the engine evaluates them pre-`up` with `Halt`/`Skip` policy.
- `backfill` emits a `BackfillSpec` (`backfill.rs:76`): `table, cursor_column, batch_size, set_clause, filter?, name`. The builder passes these fields; the engine owns the cursor loop. On Postgres it assembles the guarded windowed `UPDATE` via the existing writable-CTE executor (`backfill.rs:286`). On SQLite it uses the SQLite backfill executor (§2.3.1) — a plain batched `UPDATE … WHERE rowid IN (SELECT … LIMIT n)` loop. The author never writes the loop SQL.

---

## 3. The TypeScript `op.*` DSL API surface

The DSL is the package `@zeroship/migrate`. A migration is a `.ts` module exporting `up` (required) and `down` (optional). Each receives an `op` builder. **The builder is NOT generic over the live schema** (see the typing stance, §3.3): table/column names are plain strings.

### 3.1 Module shape

**This is the shape the AI builder emits (the common case).** The example is what the AI builder generates for "split the `name` column into `first_name`/`last_name` and copy the data." It is also what a human writes — there is one API — but we read it AI-first: the `split_part` transform is exactly the non-portable case the AI loop must be told about, and the structured feedback follows.

```ts
// migrations/0007_split_name.ts
import type { Migration } from "@zeroship/migrate";

export default {
  name: "split_name_column",
  async up(op) {
    await op.addColumn("users", "first_name", "string", c => c.nullable());
    await op.addColumn("users", "last_name", "string", c => c.nullable());
    await op.backfill("users", {
      cursorColumn: "id",
      batchSize: 1000,
      // PORTABLE form (recommended, AI-scaffolded): the engine-synthesized helper
      // (§9) emits split_part on PG and a PINNED, exhibited, authorizer-legal
      // instr+substr expression on SQLite (proven byte-identical to PG against
      // SQLite 3.51.2; instr is added to the SQLite allow-list for this engine-owned
      // lowering). The " " delimiter is a single ASCII char and n is a positive
      // literal (1..8), so this call is squarely IN the helper's proven envelope (§9)
      // and is portable across both backends. A multi-char/empty/non-ASCII delimiter,
      // n<=0, or n>8 is a hard SQLite error, not a silent mis-split.
      set: { first_name: op.fn.splitPart("name", " ", 1),
             last_name:  op.fn.splitPart("name", " ", 2) },
      // A raw op.sql`split_part(...)` would instead be PG-only (§3.3.1). The helper
      // is the portable path.
      filter: op.sql`first_name IS NULL`,
    });
    await op.dropColumn("users", "name");
  },
  async down(op) {
    await op.addColumn("users", "name", "string", c => c.nullable());
    // … reverse backfill, drop split columns …
  },
} satisfies Migration;
```

**Why `down` is hand-written here:** this migration contains a `backfill` (DML — no general inverse) and a `dropColumn` (data-destroying). Per the down-derivation matrix (§2.1.1), the engine cannot auto-derive a reverse for either, so it would yield `down: None`. Authoring `down(op)` is the only way to make this migration rollbackable; omitting it is legal but leaves it irreversible. The DSL never silently fabricates an inverse for DML or lossy DDL.

> **Portability for this example (§3.3.1 / §9).** As written with the `op.fn.splitPart` helper and a single-ASCII-space delimiter + positive literal `n ≤ 8`, this call is inside the helper's pinned, proven envelope (§9) and **is portable** across PG and SQLite. The **raw** `op.sql\`split_part(name,' ',1)\`` form is **PG-only** (`split_part` is not in the portable allow-list). A helper call **outside** the envelope (multi-character/empty/non-ASCII delimiter, `n<=0`, `n>8`, non-literal args) is **not silently mis-split — it is a hard `EXPR_NOT_PORTABLE` error on the SQLite leg** (§9). The boundary is precise: portable via the engine-synthesized helper within its pinned envelope; a hard SQLite error outside it; PG-only via raw dialect functions.

**What the AI loop receives for the `split_part` raw body (the machine-readable feedback, §8.8 / §3.3.1.1):**

```jsonc
{ "code": "EXPR_NOT_PORTABLE",
  "op_index": 2,
  "ts_location": "migrations/0007_split_name.ts:9",
  "dialect": "sqlite",
  "reason": "function `split_part` is PG-only; not in the portable expression allow-list",
  "suggested_fix": "use op.fn.splitPart (portable for a single-ASCII delimiter, §9), a dialect-aware op.sql body, or mark the migration PG-only" }
```

The AI loop then either supplies a dialect-aware `op.sql` for the SQLite leg or explicitly marks the migration `dialect_scope = PgOnly` (§2.4.1) — it cannot silently ship a migration that no-ops/mis-applies on SQLite, because an AI migration is PG-only-by-default until the both-backends dry-run passes (§8.8). This is why the validator + structured-error envelope are PR1, not PR6.

`up`/`down` take `op` and register ops on it; **they do not execute SQL** (§4). `await` resolves when the op is recorded (Alembic offline-mode discipline), letting the build render, lint, and produce plan/`--sql` output.

#### 3.1.1 Transaction model for a mixed DDL + backfill migration

A single `transactional: bool` on one `Migration` cannot express "atomic DDL **and** non-atomic backfill in one unit" — which is why §2.0 makes the artifact a **plan of phases**, each carrying its own transaction discipline (the same split the engine runs for expand-contract):

- **DDL phases run transactionally** as `Ddl(Migration)` steps under the executor's default boundary — `BEGIN; SET LOCAL ROLE migrator; <up>; journal; COMMIT`. The two `addColumn`s lower into a DDL step that commits atomically before the backfill begins. The `dropColumn` lowers into a later DDL step that runs atomically after the backfill completes.
- **The backfill phase runs as its own `Backfill(BackfillSpec)` step** — a batched, per-batch-transactional, resumable loop driven by `run_backfill` (PG) / the SQLite executor (§2.3.1). It is never inside the DDL phase's wrapping transaction (on PG the executor rejects DML in a non-txn migration `up` via `validate_non_txn_idempotent`, `executor.rs:518`; on SQLite the backfill is the §2.3.1 batched per-batch-txn loop). It persists crash-safe cursor progress and is re-entrant under the session-scoped project lock.
- **Ordering guarantee (the expand-contract invariant, reused).** A `NOT NULL` / column-drop that depends on the backfill is emitted as a DDL step after the `Backfill` step, and the executor only advances to it once the backfill's journal row lands (`engine.rs:550`, the `pending_contract` discipline). For the §3.1 example: `addColumn(nullable)` → `Backfill` → `dropColumn(name)`, the drop gated on backfill completion.

The mixed migration is an ordered plan of phases, each with the correct boundary, all under one version/checksum (§2.0.1) — the same execution model `apply_declarative_locked` implements for online changes, now run through the shared `apply_plan`.

### 3.2 The `op` interface (abridged; full signatures generated from the IR schema)

**All table/column/name references are plain `string`. There is NO generic `S extends Schema` parameter on `Op`, and no `TableName<S>` / `keyof RowOf<S,T>` / `RowInsert<S,T>` binding to the live schema.** Structural type-safety is preserved for op argument *shapes*, the `ColType` builder lexicon, the `op.raw` shape, and insert-row *value* shapes. Name existence is an apply-time check against the real DB (§3.3).

```ts
interface Op {
  // ── DDL: tables ──
  createTable(name: string, build: (t: TableBuilder) => void, opts?: { ifNotExists?: boolean }): Promise<void>;
  dropTable(name: string, opts?: { ifExists?: boolean; cascade?: boolean }): Promise<void>;

  // ── DDL: columns ──
  addColumn(table: string, name: string, type: ColType, build?: (c: ColumnDef) => ColumnDef): Promise<void>;
  dropColumn(table: string, name: string, opts?: { ifExists?: boolean }): Promise<void>;
  renameColumn(table: string, from: string, to: string, type: ColType): Promise<void>; // → online intent
  alterColumn(table: string, name: string, build: (a: AlterColumn) => AlterColumn): Promise<void>;

  // ── DDL: constraints / indexes ──
  addForeignKey(table: string, columns: string[], target: string, targetColumns: string[],
                opts?: { name?: string; onDelete?: FkAction; onUpdate?: FkAction }): Promise<void>;
  addUnique(table: string, columns: string[], name?: string): Promise<void>;
  addCheck(table: string, expr: Raw, name?: string): Promise<void>;
  dropConstraint(table: string, name: string, opts?: { type?: "pk"|"fk"|"unique"|"check"; ifExists?: boolean }): Promise<void>;
  createIndex(table: string, columns: (string|Raw)[],
              opts?: { name?: string; unique?: boolean; using?: "btree"|"gin"|"gist"|"ivfflat"|"hnsw"|"fts5";
                       where?: Raw; concurrently?: boolean }): Promise<void>;
  dropIndex(name: string, opts?: { table?: string; ifExists?: boolean; concurrently?: boolean }): Promise<void>;

  // ── DML (identifiers are plain strings; transform bodies are opaque, §3.3) ──
  // onConflict is PG-only; passing it while targeting SQLite is a hard build error (§9).
  // Row VALUE shapes are a loose Record<string, ScalarValue> by default; a caller MAY
  // supply a generic for editor convenience, but it is NEVER auto-bound to the live schema.
  insert<R extends Row = Row>(table: string, rows: R | R[],
    opts?: { onConflict?: { columns: string[]; doUpdate?: Partial<R> } /* PG-only */ }): Promise<void>;
  update<R extends Row = Row>(table: string, where: Filter | Raw, set: Partial<R> | Raw): Promise<void>;
  delete(table: string, where: Filter | Raw): Promise<void>;
  backfill(table: string, opts: {
    cursorColumn: string; batchSize: number;
    set: Record<string, ScalarValue | Raw>; filter?: Raw;
  }): Promise<void>;

  // ── portable transform helpers (engine-synthesized; cross-dialect semantics PINNED +
  //     fixture-proven by the engine WITHIN A BOUNDED ENVELOPE, §9) ──
  // Each returns a Raw the engine lowers to a PINNED per-dialect expression (PG split_part /
  // SQLite pinned instr+substr expression, exhibited & proven in §9; instr is added to the
  // SQLite authorizer allow-list for this engine-owned lowering). splitPart is portable ONLY
  // inside the proven envelope (single-ASCII delimiter, positive literal n in 1..8); out-of-
  // envelope args are a HARD SQLite error (EXPR_NOT_PORTABLE), never a silent mis-split.
  // replace is PG-only until `replace` is allow-listed; substr is DEFERRED (its byte-identity
  // is not yet proven — §9). The `col` argument is a column NAME string or a Raw fragment.
  fn: {
    splitPart(col: string | Raw, delim: string, n: number): Raw;   // portable (single-ASCII delim, 1<=n<=8); ships PR6b
    replace(col: string | Raw, from: string, to: string): Raw;     // PG-only until `replace` allow-listed (§9)
    substr(col: string | Raw, start: number, len?: number): Raw;   // DEFERRED: portability unproven, not admitted until the substr proof PR lands (§9)
  };

  // ── escape hatch (still guard+role limited) ──
  raw(stmt: { pg?: string; sqlite?: string; down?: { pg?: string; sqlite?: string } }): Promise<void>;
  sql(strings: TemplateStringsArray, ...v: unknown[]): Raw;
  // Typed identifier-interpolation slot for op.sql (§3.3.1.2): produces an identifier
  // token validated against the enclosing step's target table at apply/render time.
  col(name: string): Raw;   // alias: ident

  // ── SQLite-safe rebuild (Alembic batch_alter_table analog) ──
  batchAlterTable(table: string, build: (b: Pick<Op,
    "addColumn"|"dropColumn"|"renameColumn"|"alterColumn"|"addForeignKey"|"addCheck">) => void): Promise<void>;
}

type ScalarValue = string | number | bigint | boolean | null | { decimal: string } | Uint8Array;
type Row = Record<string, ScalarValue>;

type ColType =
  | "string" | "number" | "boolean" | "timestamp" | "date" | "calendarDate"
  | "json" | "bytes" | "uuid" | { ref: string } | { vector: number } | "geoPoint" | Raw;
```

Note `{ ref: string }` carries a **plain string** target-table name, not `TableName<S>`.

### 3.3 Typing stance: names are strings, not live-schema-bound (BINDING)

**Migration files are immutable historical artifacts. They MUST NOT bind table/column names to the live `@zeroship/db` schema.** This is a deliberate, binding decision, and it is the single most important typing rule in this spec.

**Why binding live-schema-typed names is wrong (the rot bug).** If `addColumn`/`update`/`insert` typed their names against the current schema (`TableName<S>`, `keyof RowOf<S,T>`, `RowInsert<S,T>`), then a migration that referenced `users.lastSeen` would **stop compiling** after a *later* migration drops that column. A project's whole migration history would become un-compilable as the schema evolves, and authors would be tempted to **edit committed migrations** to make them compile again — which changes the op list, changes the plan checksum, and triggers a drift abort (§5.3). "A migration referencing a column that does not exist fails `tsc`" is therefore an **anti-feature**, not a feature: it confuses *the schema at authoring time* with *the schema as it evolves*, and it punishes the correct behavior (never editing applied migrations).

**Industry precedent.** No mature migration tool binds migration files to the live schema:
- **Kysely** deliberately uses `Kysely<any>` inside migration files (its docs state migrations should not be typed against the current schema, precisely because the schema changes over time).
- **Alembic** uses string table/column names in `op.add_column("users", …)`.
- **Drizzle** migrations are *generated SQL*, not type-checked against the live schema.

**What IS type-checked (structural safety, preserved):**
- **Op method argument shapes** — `addColumn(table, name, type, build?)` etc. are typed: you cannot pass a number where a `ColType` is expected, or omit a required argument.
- **The `ColType` builder lexicon** — `t.*` / the `ColType` union is typed; an invalid type literal fails `tsc`.
- **`op.raw({ pg, sqlite, down })` shape** — the object shape is typed.
- **Insert-row VALUE shapes** — `insert` rows are a loose `Record<string, ScalarValue>` (`Row`) by default; a caller MAY supply a generic `insert<R>(…)` / `update<R>(…)` for editor convenience, but `R` is **never auto-derived from the live schema**. The value *kinds* (string/number/bigint/boolean/null/decimal/bytes) are typed; the *column-name → type correspondence against the live schema* is not.

**What is NOT type-checked (validated at apply time instead):**
- **Table and column NAMES** — `table`, `column`, `from`, `to`, `name`, `cursorColumn`, the keys of a `set`/`where`/row — are plain `string`. Their existence is **not** a `tsc`-time check. They are validated at **apply time against the real DB** (and `op.col(...)`/`ColRef` resolution is an apply/render-time check against the actual target table, §3.3.1.1). A migration referencing a non-existent column fails when it is applied (with the structured error envelope, §8.8), not when it is type-checked — which is correct, because the column may exist at the migration's historical point even if it does not exist in the current declared schema.
- **Transform expressions** — the right-hand side of a `set` and any `filter` body authored via `op.sql\`…\`` or `Raw` is an opaque SQL string. `BackfillSpec.set_clause`/`filter` are free-form SQL (`backfill.rs:77`). TypeScript cannot verify it against the schema, its return type, or its dialect-portability. It is validated by the §3.3.1 portable-expression grammar (for portability) and the guard at render time (PG) / authorizer (SQLite) (for safety) — not by `tsc`.

So the guarantee is: **op shapes, the type lexicon, `op.raw` shape, and value kinds are typed; names and transform bodies are validated at apply/render time, never `tsc`-bound to the live schema.** The part most likely to be semantically wrong (the transform) is the part the type system cannot see — authors test it (the shadow-DB dry-run, §6.2).

#### 3.3.1 How an `op.sql` transform body is admitted on SQLite (reconciling §8.7)

§8.7 establishes that `Confined` SQLite **refuses raw author-supplied SQLite statements fail-closed** (libpg_query cannot parse SQLite; the authorizer is a capability gate, not an expression deny-list). An `op.sql\`…\`` body embedded in a portable `update`/`backfill` `set` is author-supplied SQLite text — so without a rule it would be refused. The resolution makes the boundary precise:

- **EVERY author-supplied fragment, regardless of the enclosing op, goes through ONE validator.** The `set`/`filter` of an `update`/`backfill`, the `where` of a one-shot `update`, and the `where` of a `delete` are ALL admitted through the identical portable-expression grammar validator (§3.3.1.1), with the same target-table-scoped `ColRef` resolution (the enclosing op's target table). There is no per-op variation: an author-supplied SQL fragment in *any* DML op is the same validated expression-fragment surface. A `delete(table, where)` whose `where` is an `op.sql` fragment with a non-portable function is rejected on SQLite exactly as a backfill `filter` would be.
- **`op.sql` bodies are expression fragments, not statements, and are admitted on SQLite only through a whitelisted expression grammar.** The engine assembles the *statement* (the `UPDATE … SET <col> = <expr> WHERE …` shell, the `DELETE FROM … WHERE …` shell) from trusted templates; only the `<expr>`/`<filter>`/`<where>` fragments come from the author. Those fragments are admitted iff they parse against a **restricted, engine-validated expression grammar** (the portable subset of §9): column references, literals, arithmetic, comparison/boolean operators, parenthesization, `CASE … WHEN … THEN … ELSE … END` (searched and simple forms), and a **closed allow-list of provably-identical portable scalar functions** (`coalesce`, `nullif`, `lower`, `upper`, `trim`, `length`, `abs`, `||` concat, `cast(... as <portable type>)`). Anything outside this grammar (a function not on the allow-list, a subquery, a statement-level token like `;`, a window clause) is a **hard build/render error**, on both dialects.
- **`CASE` is in the grammar.** PG and SQLite implement `CASE WHEN … THEN … ELSE … END` (both searched and simple forms) with identical evaluation semantics: first-match wins, `ELSE` (or NULL) when no branch matches. The validator parses it as a first-class AST node (`Case { operand?, branches: [(cond, result)], else? }`) whose sub-expressions must each be in-grammar.
- **`substr` and `replace` are NOT on the portable allow-list.** Their cross-dialect semantics diverge enough to be the "passes on PG, mis-applies on SQLite" silent-divergence trap: `substr`/`substring` differ on negative-start behavior, start-beyond-length, and 2-arg vs 3-arg forms; `replace` differs on collation/affinity edges. A *raw* `substr`/`replace`/`instr` body is therefore PG-only (via `op.raw({pg})` / a dialect-aware `op.sql`), or the author writes a dialect-aware body per leg, or — for the common split case — uses the `op.fn.splitPart` helper whose SQLite lowering the engine pins (§9; `op.fn.substr` is deferred until its byte-identity is proven). (Note: even though `instr` is added to the SQLite *authorizer* allow-list (§9) so the engine's pinned `op.fn.splitPart` lowering can use it, `instr` is **not** on the *portable-expression grammar* allow-list, which is a distinct list: a creator-authored raw `op.sql\`instr(...)\`` is still rejected by the grammar validator. The authorizer gates what the migrator role may execute; the grammar gates what an author may name.)
- **`split_part` is NOT on the portable allow-list as a raw function** (a raw `split_part` is PG-only). The portable path for the split is the `op.fn.splitPart` helper (§9).
- **Why this is safe on SQLite without a parse deny-list.** The author never supplies a *statement*, so the §8.7 "no raw SQLite statement" guarantee is preserved: the only author-controlled SQLite text is an *expression fragment* validated against the closed grammar, then spliced into an engine-owned statement that the runtime authorizer additionally screens for capability (table/column write access under the migrator role). The grammar is the deny-list-equivalent for expressions; the authorizer remains the capability gate.
- **On PG**, the same fragment also passes the full libpg_query deny-list when its enclosing statement is guard-checked, so PG retains its statement-level defense in addition to the grammar.

**The `op.fn.*` helpers serialize as a structured neutral fragment node, not an opaque `op.sql` string.** A helper emits a typed AST node the validator's closed AST models as a first-class variant: `FnSynth { fn: "splitPart"|"replace"|"substr", args: [ColRef|Literal|…] }`. Consequences:
- **The IR carries the helper as data, not SQL.** A `set: { first_name: op.fn.splitPart("name", " ", 1) }` is `{ "fn": "splitPart", "args": [{"col":"name"}, {"lit":" "}, {"lit":1}] }` — dialect-neutral, so the one plan checksum is dialect-stable (the per-dialect rendering is the engine's lowering, never hashed).
- **The engine owns the per-dialect lowering** (PR6b): `FnSynth` lowers to `split_part(...)` on PG and the engine's pinned, exhibited `instr`/`substr` expression on SQLite (§9). The cross-dialect equivalence is the engine's invariant (exhibited and proven byte-identical to PG over a bounded envelope against real SQLite 3.51.2).
- **It passes the validator by construction ONLY inside the §9 envelope.** A well-formed in-envelope helper fragment is admitted on both dialects; a *raw* `split_part(...)` string is not an `FnSynth` node and is rejected on SQLite. An **out-of-envelope** `FnSynth` (multi-char/empty/non-ASCII delimiter, `n<=0`, `n>8`, non-literal delimiter/`n`) is rejected on the SQLite leg by the validator with a structured `EXPR_NOT_PORTABLE` error.

Net: transform bodies are portable **iff** they fit the closed expression grammar; anything else is PG-only via `op.raw({pg})`/`op.sql` with a dialect-aware body, or a hard error on SQLite. §9 is the authoritative list.

##### 3.3.1.1 The validator: a net-new, dialect-neutral fragment parser (not libpg_query)

The grammar is enforced by a net-new validator (§6, PR1). Its design is pinned so it cannot become a green-build-then-render-time-rejection footgun:

- **Parsing approach — one shared token/AST validator, not two dialect parsers.** libpg_query parses Postgres only; it cannot parse a SQLite fragment, so the validator does not reuse it. The validator tokenizes the fragment with a small dialect-neutral lexer and parses it into a tiny closed AST (`ColRef | Literal | BinOp | UnaryOp | Case(branches) | FnCall(allow-listed) | FnSynth(op.fn helper) | Cast(portable-type) | Paren`; `Case` and `FnSynth` recursively require in-grammar sub-expressions, and `FnSynth` additionally requires in-envelope args — §9). Anything it cannot represent — a subquery, a window clause, a statement token (`;`, `--`, `/* */`), an identifier that is not a known column, a function not on the allow-list — is a parse failure ⇒ hard render error on both dialects.
- **`ColRef` resolution is an APPLY/RENDER-TIME check against the actual target table** — it is scoped to the enclosing DML op's target table (the `update`/`backfill`/one-shot-`update`/`delete` step's target): a column reference to a column not on that table is a hard error. The scoping rule is identical for every author-supplied fragment regardless of which DML op encloses it. This is an injection defense **and** a deliberate capability boundary (a cross-table/correlated backfill is not expressible in the portable grammar — an explicit portability exclusion, §9). It is checked when the fragment is spliced for a real target, **not** as a `tsc`-time check against the declared schema (consistent with §3.3: names are validated at apply/render time, not `tsc`-bound).

- **VALIDATOR ANTI-DRIFT — the Rust recognizer is the SOLE authoritative apply-time gate; the JS side is a strictly-more-rejecting lint (DEFAULT).** The unbounded `op.sql`/`op.fn` expression grammar must not depend on an unbuilt parser-codegen. Two independently-written parsers over an unbounded fragment language cannot be proven equivalent by any finite corpus, which would reintroduce builder↔engine drift on the highest-churn surface (transform bodies the AI loop emits constantly). The design therefore pins, as the **default**:
  - **The engine's Rust recognizer is the only gate that can fail an apply.** It runs at lowering/render, on the exact fragment spliced.
  - **The JS-side recognizer is a STRICTLY-MORE-REJECTING lint.** It may reject things the Rust recognizer would accept (surfacing a fast "this might not pass" hint), but it is **FORBIDDEN from accepting anything the Rust recognizer rejects.** This one-directional guarantee — *JS-accept ⇒ Rust-accept, never the reverse* — is enforced by a **one-directional differential fuzzer**: a generator produces randomized fragments over the token alphabet and asserts that for every input the JS recognizer accepts, the Rust recognizer also accepts (it does not require the converse). A violation (JS accepts something Rust rejects) fails CI.
  - This removes the green-JS/red-Rust **accept-drift** footgun without building a parser generator: the JS side can only ever be conservative, so a JS-green fragment that the Rust side later rejects at apply cannot occur in the *accept* direction (the only direction that matters for "my build passed but my deploy failed"). The apply-time Rust reject (with the §8.8 structured payload) is the authoritative feedback; the AI loop keeps a conservative local hint and self-corrects on the apply-time verdict.
  - **The over-rejection (reject-drift) DX cost is BOUNDED, not unaddressed.** A strictly-more-rejecting lint can in principle *reject* fragments the engine would accept — a false-negative authoring hint that could send the AI loop chasing a non-problem. Two bounds cap this:
    - **A floor the JS lint MUST accept by construction.** The JS lint is **required to accept everything in the §9 portable allow-list and the §3.3.1 closed grammar** — the closed, enumerable surface: the allow-listed scalar functions (`coalesce`/`nullif`/`lower`/`upper`/`trim`/`length`/`abs`/`||`/`cast(<portable type>)`), `CASE`, the `op.fn.*` helpers **inside their §9 envelope**, column refs, literals, arithmetic/comparison/boolean operators, and parenthesization. The one-directional fuzzer is run **in both directions over this CLOSED set**: it asserts JS-accept ⇔ Rust-accept for every fragment generated from the closed allow-listed function set + grammar productions (the converse direction is decidable here because the set is finite/enumerable), and JS-accept ⇒ Rust-accept only for the *unbounded* fragment language (arbitrary identifiers, nesting depth) where bidirectional equivalence is not finitely provable. So over-rejection is impossible on the surface the AI loop actually emits constantly (the allow-listed transforms); it is permitted only on exotic unbounded shapes that are non-portable anyway.
    - **Where over-rejection is still possible (the unbounded tail), it is a NON-AUTHORITATIVE hint, never a build failure.** The JS lint never fails a build by itself — it surfaces a "this might not pass at apply" warning; the authoritative gate is the Rust recognizer at lowering. So a JS over-rejection costs at most one wasted local hint, not a blocked deploy or a wrong apply. The AI loop is instructed to treat the JS hint as advisory and the apply-time Rust verdict as authoritative.
    - **Fixture:** every member of the §9 allow-list + a generated corpus over the closed grammar productions is asserted JS-accept ⇔ Rust-accept (the bidirectional gate on the closed set); separately, the unbounded-fragment fuzzer asserts only JS-accept ⇒ Rust-accept.
  - **OPTIONAL FUTURE UPGRADE (not a prerequisite): a shared serialized grammar table + a per-language table-driven recognizer.** A stronger (bidirectional-equivalence) design defines the grammar as a serialized token + precedence table (`op-expr-grammar.json`, data not code) and has one small fixed Pratt/precedence-climbing recognizer per language interpret that table, differentially fuzzed for *identical* accept/reject. This would give the AI loop the *guarantee* (not just a hint) that a JS-green fragment renders. It is documented as an optional future upgrade and is **not** a prerequisite for shipping — the default (Rust-sole-gate + strictly-more-rejecting JS lint + one-directional fuzzer) is what PR1 builds.
- **`cast(... as <portable type>)` and `||` are admitted but their semantics are pinned, not assumed identical.**
  - **`cast`** is restricted to the closed portable type set (`text`/`integer`/`real`/`boolean`/`blob`), never PG-only types; the validator rejects a cast outside it. A `cast` result's bit-exact value may differ across backends for edge inputs, so a cast in a portable transform is "portable in shape, dialect-defined in numeric edge semantics" — authors needing exact cross-dialect equality must test it (shadow-DB dry-run, §6.2).
  - **`||` concat** is admitted with the explicit NULL rule documented: a NULL operand yields NULL on both backends (wrap with `coalesce` for empty-string concat). This is the one place PG and SQLite agree, which is why `||` is allow-listed while `concat()` (PG-only NULL-skipping semantics) is not.
  - **Fixtures:** a `cast(x as integer)` fragment proving identical accept on the Rust validator (and accept-or-stricter on the JS lint) and applying on both backends; an `a || b` fragment with a NULL operand asserting both backends produce NULL.

##### 3.3.1.2 `op.sql` interpolation (`...v` args) — bound as parameters, never spliced as structure

`op.sql\`…${v}…\`` is a tagged template; its `...v` interpolations are **not** spliced into the fragment as text:

- **Each `${v}` is captured as a typed scalar parameter, never as raw SQL text.** The recorder records `op.sql\`x > ${threshold}\`` as a fragment template `x > $1` plus a typed bind `[threshold]` (the §2.3.2 mechanism) — the interpolated value becomes a native driver parameter (`$n`/`?n`), bound, never concatenated. So an interpolated `${userControlledString}` is **data**, parsed by the DB as a value, and cannot become a column reference, a function name, a table reference, or a statement token.
- **Therefore interpolation can express only literals, not identifiers.** A `${v}` cannot be a column name or function — those must be written literally in the static template text, which the grammar validator parses.
- **The structural fix for identifier interpolation: a typed identifier slot `op.col(...)`.** An author who genuinely wants a column reference writes `op.sql\`${op.col(columnName)} > ${threshold}\`` — the `op.col(...)` is validated by the §3.3.1.1 `ColRef`-resolution rules (it MUST be a column on the enclosing step's target table at apply/render time, else a hard error); the `${threshold}` is a bound value. So a bare `${v}` is **unambiguously a value, always**.
- **AI-footgun guard: a specific structured error when a bare `${v}` sits in identifier position.** The dominant author is an LLM that overwhelmingly expects template interpolation to splice an identifier; an AI emitting `op.sql\`${columnName} > 0\`` would otherwise get a silently-wrong string-literal comparison. So when a bare interpolated `${v}` appears where the grammar expects an identifier/column, the validator emits `EXPR_INTERPOLATED_IDENTIFIER { op_index, ts_location, dialect, reason: "an interpolated ${…} is bound as a VALUE, not spliced as an identifier; use op.col('name') for a column reference, or write the identifier as static template text", suggested_fix }` (added to the §8.8 taxonomy), whose `suggested_fix` points at `op.col(...)`. The heuristic is the on-ramp; `op.col` is the destination.
- **Column-ownership scoping on the static text (defense in depth).** The static template text is parsed by the §3.3.1.1 validator, whose `ColRef` resolution is scoped to the step's target table; a literally-written `other_table.secret` is rejected.
- **Fixtures:** (a) `op.sql\`x > ${userValue}\`` records a bound parameter (a metacharacter-laden `${v}` cannot alter the statement); (b) `op.sql\`other_table.secret > 0\`` is rejected by the target-table-scoped `ColRef` resolution; (c) `op.sql\`${'other_table.secret'} > 0\`` is admitted only as a literal string bind on the LHS; (d) a bare `op.sql\`${columnName} > 0\`` emits `EXPR_INTERPOLATED_IDENTIFIER` whose `suggested_fix` names `op.col(...)`, and `op.sql\`${op.col('first_name')} = ${v}\`` validates and binds `v`.

### 3.4 Dialect-agnostic by default

There is **one** API. Authors never branch on dialect except inside `op.raw`. The engine owns lowering (Postgres native vs SQLite rebuild). `using: "gin"|"ivfflat"|"hnsw"|"fts5"` index methods are Postgres-only logical hints; on SQLite the engine maps `fts5` to the FTS5 virtual-table path and rejects the PG-only ANN methods with a clear error (no silent degradation).

---

## 4. Execution model — build/dev time, not apply time

**The engine is zero-tokio and has no V8** (`AGENTS.md` key invariants). The DSL therefore **never runs in the engine.** It runs in Node at build/dev time and emits IR data the engine ingests.

### 4.1 Where it runs

| Phase | Where | What happens |
| --- | --- | --- |
| **Build** (`pnpm build` / `zeroship-migrate generate`) | Node (the vite-plugin / CLI build step) | discover `migrations/*.ts`; for a version that does **not** yet have a committed `.ir.json`, evaluate that module's `up`/`down` with a recording `op` builder and serialize the recorded op list to `<version>_<name>.ir.json` (+ source-map hint) for the author to commit; for a version that already has a committed `.ir.json`, do **not** re-evaluate the `.ts` — the committed artifact is the input (the build-once skip, §5.1) |
| **Bundle** (`.zship` pack) | Node + `crates/bundle` | the packer consumes the **committed `.ir.json` blob verbatim** and content-addresses it into the manifest as a `MigrationFileEntry` (§5.2) — same as `.sql` today, and it does **not** re-evaluate the `.ts` at pack time (§5.1). A CI/build assertion checks the packed blob hash equals the committed `.ir.json` hash |
| **Deploy / apply** | **Rust engine** (zero-tokio) | reconstruct `.ir.json` from blobs, `load_dir`, deserialize IR, render per-dialect, guard, role, journal, apply |

The recording builder is a **pure JS object**: each `op.*` call pushes a plain-data op onto an array and returns a resolved `Promise`. It performs no I/O, no DB connection, no SQL execution. This is the Alembic "build the op tree, execute later" model.

### 4.2 Why not run it in V8 inside the engine?

Two reasons: (1) the engine is deliberately zero-V8 and zero-tokio; embedding a runtime would violate a key invariant. (2) Security: the DSL is untrusted creator/AI code. Running it at build time in Node and shipping only validated IR data means the engine's apply path only ever sees data, never code (§8). The `op.*` builder does not even need V8 because it runs in the author's own Node build, not the engine.

### 4.3 Determinism

The recorded op list is deterministic: same `.ts` source ⇒ identical `.ir.json` ⇒ identical checksum across builds.

1. **Byte-stable serialization for a given recorded value (RFC 8785 JCS).** The build emits RFC 8785 JSON Canonicalization Scheme — the named standard both the JS builder and the Rust side implement. JCS fixes the cross-implementation footguns: lexicographic key ordering of nested objects, ECMAScript-`Number`-format number serialization (`1e10`→`10000000000`, no trailing `.0`, no `-0`), and minimal `\uXXXX` string escaping. Cross-language JCS agreement is **verified, not assumed** — the drift authority is the Rust-recomputed checksum over the parsed-then-recanonicalized op list (mechanism 4), so a raw-byte JCS mismatch can never silently brick a deploy.
2. **Build-time capture is "as-of build" (by design, acceptable for seeds).** A value computed at build time (a seed row's timestamp) is captured once and frozen into the artifact.
3. **Non-determinism is a false-drift hazard — neutralized structurally by the build-once committed artifact (§5.1), with lint + docs as a pre-commit catch.** Because the `.ir.json` is committed build-once and never regenerated for an already-applied version (§5.1), a later rebuild where `Date.now()` differs cannot change a deployed migration's artifact or checksum. The remaining concern is pre-commit: an author should not bake a non-deterministic value into a not-yet-committed artifact. For that: (a) an ESLint/DSL lint flags `Date.now()`, `Math.random()`, `crypto.randomUUID()`, `new Date()` syntactically appearing inside an `op.*` argument (best-effort, AST-based), run in CI on changed migrations before commit; and (b) docs: "for values that must be computed at apply time use `op.sql` (DB-evaluated)."
   - **`op.sql` is the default scaffold for any time/uuid-valued seed column.** `generate`/`new` scaffolds, when seeding a column whose `ColType` is `timestamp`/`date`/`uuid` (or whose name matches a `created_at`/`updated_at`/`id` convention), emit `op.sql\`now()\``/`op.sql\`gen_random_uuid()\`` (DB-evaluated at apply time) by default.
   - **The AI-path determinism check is SCOPED to the actual risk surface (time/uuid-typed columns), NOT a blanket "reject any non-literal value."** Real AI migrations legitimately compute values (a seed from an imported constant, a `rows.map(...)`-built array); rejecting all of them raises iteration cost for little gain, since the build-once contract already makes post-deploy non-determinism unreachable. So: the strict reject fires **only** for a non-literal value bound to a time/uuid-typed column, flagged with "use `op.sql\`now()\``/`gen_random_uuid()\`` for apply-time evaluation." A computed string/number or a `map`-built array on a non-time/uuid column is accepted. The syntactic RNG/clock lint (a) still runs on all columns (it flags a *direct* `Date.now()` call anywhere).
   - **Fixture:** an AI migration building rows via `rows.map(r => ({ name: r.name, slug: slugify(r.name) }))` and seeding a config-derived constant is NOT rejected; a migration binding `Date.now()` to a `created_at: timestamp` column IS flagged with the `op.sql\`now()\`` suggestion.
4. **The checksum authority is the parsed-and-recanonicalized op list, not the raw `.ir.json` bytes.** Two independent JCS implementations can disagree on a number/string shape no fixture exercised. To make that harmless, the engine never hashes raw `.ir.json` bytes for drift: it deserializes the `.ir.json` into the typed `Op` list, re-canonicalizes the OP LIST through the Rust JCS path, and `Checksum::of_ir` folds that Rust-recanonicalized op list (then `fold_common` folds the flags/owner/deps tail via the existing serde discipline, NOT JCS — §2.4 point 5). So the drift anchor is invariant under any JCS formatting difference in the op-list region: as long as both sides parse to the same typed op values, the checksum matches. (The flags/owner tail never comes from the JS builder — the engine recomputes it from the typed struct — so it cannot be a cross-implementation divergence source.) Raw byte-equality of the whole `.ir.json` is an advisory CI canary, not a hard deploy gate.

---

## 5. On-disk & bundle format

### 5.1 Authoring file → artifact

- **Author writes**: `migrations/0007_split_name.ts` (TypeScript, type-checked for op shapes — not for names, §3.3).
- **Build emits**: `migrations/0007_split_name.ir.json` (the checksummed IR artifact).
- The `.ts` is the source of truth for humans; **the committed `.ir.json` is the sole authority for the engine** on the trusted operator/platform path, and a *provenance input* for the creator/AI path (see the provenance gate below). To make a `.ts`/`.ir.json` divergence visible, two gates run:
  - **CI consistency gate (for not-yet-applied versions):** for every version without a journal row, CI asserts `the .ts would emit == the committed .ir.json` (re-evaluate the `.ts`, JCS-canonicalize, compare the §2.4 typed-value checksum). A divergence fails CI.
  - **Deploy-time provenance gate — MANDATORY for the creator/AI path.** Independent of *where* the first `.ir.json` was recorded (locally or hosted, §8.9.2), **deploy always runs on the platform**, so the deploy-time re-record runs through the canonical platform recorder under the kernel sandbox. **The bundle MUST carry the `.ts` provenance blob for any creator/AI-authored migration**; at deploy, for every not-yet-applied version, the engine re-records the bundled `.ts` through the canonical platform recorder and asserts `Checksum::of_ir(.ts-would-emit) == committed .ir.json checksum` **as a hard deploy gate** — so a `.ir.json` whose ops diverge from its `.ts` (a force-merge, a non-platform build, a CI-skipped repo) cannot deploy, even if it bypassed the CI consistency gate. A creator/AI bundle that *lacks* the provenance blob is **refused at deploy** (`PROVENANCE_BLOB_MISSING`, §8.8). A `.ts`/`.ir.json` checksum divergence at deploy is `PROVENANCE_MISMATCH` (§8.8).
  - **"Committed `.ir.json` as sole authority" is reserved for the TRUSTED operator/platform path only.** The platform's own changelog (Task #54 Flyway-mode) and operator-run `dbmate` deploys run through enforced CI on a trusted toolchain; for that path the deploy may accept the committed `.ir.json` without a `.ts` re-record. The distinction is by *path*: creator/AI ⇒ blob mandatory + gate fires; trusted operator/platform ⇒ committed `.ir.json` accepted (CI is the trusted line there).
- **The `.ir.json` is a committed, build-once artifact.** When the build emits a new `.ir.json` for a not-yet-deployed version, the author commits it to VCS alongside the `.ts` (like a lockfile). Once committed it is the immutable artifact that ships and applies; the `.ts` is **not** re-evaluated to regenerate the `.ir.json` for a version that already has a committed artifact.
- **The bundle packer MUST consume the committed `.ir.json` blob verbatim and MUST NOT re-evaluate the `.ts` for an already-committed version.** A CI/build assertion enforces this: for every version with a committed `.ir.json`, the packed `MigrationFileEntry.hash` must equal the sha256 of the committed blob — a mismatch (the packer re-emitted instead of copied) fails the build. So a benign rebuild where `Date.now()` differs cannot change an already-applied migration's artifact. A drift error then means exactly one thing: someone edited a committed `.ir.json` (or its `.ts` and re-committed) for an applied version — which is drift and should abort.

### 5.2 Loader & manifest integration

The loader gains an `.ir.json` recognizer alongside the Flyway/dbmate `.sql` grammars (`crates/zeroship-migrate/src/loader.rs:19`). Filename grammar reuses the versioned prefix: `<NNNN>_<desc>.ir.json` (or `V<NNNN>__<desc>.ir.json` for Flyway-mode). The numeric prefix → deterministic UUIDv7 mapping (`migration_id_for_version`) is identical to the SQL path — IR migrations interleave with SQL migrations in one ordered history.

In the bundle, an IR migration is a `MigrationFileEntry { name, hash }` (`crates/bundle/src/manifest.rs:160`) exactly like a `.sql` file. Deploy reconstructs the files from blobs and calls `load_dir`, which branches on extension to deserialize IR vs parse SQL.

**Loader return shape (reconciled with the plan model, §2.0).** `load_dir` returns `Vec<Migration>` today (`loader.rs:606`). A `.sql` file → one `Migration`; an `.ir.json` → a `AppliedPlan`. The loader's signature generalizes to **`Vec<AppliedPlan>`** (a pure-DDL `.sql` or `.ir.json` is a single-step plan), preserving the order-by-version contract. This is a deliberate, owned wire-format change to the loader's return type (pre-launch, no back-compat), not a hidden shim.

**Blast radius — every `load_dir` caller, enumerated:**

| Caller | Edit |
| --- | --- |
| `crates/control/src/deploy_migrate.rs:133` | binds `Vec<AppliedPlan>`; the IR-path apply routes to `apply_plan` over each plan's steps and passes the target dialect (§2.4.1) into the IR lower. A pure-`.sql` deploy is a `Vec` of single-step plans. |
| Public dbmate CLI `crates/zeroship-migrate/src/bin/zeroship-migrate.rs` (`validate` at `:553`, plus `apply`/`status`/`rollback`) | `validate` now also accepts `.ir.json`. `apply`/`status`/`rollback` iterate `Vec<AppliedPlan>`; observable behavior preserved (see CLI-semantics below). |
| `crates/zeroship-migrate/src/guard/platform_runner.rs` — the **7 `load_dir` call sites, one per runner fn** (resolve by the enclosing `fn`, not the line: `run_migrate_sqlite` ~`:607`, `run_migrate_pg` ~`:622`, `run_status` ~`:650`, `run_validate_pg` ~`:711`, `run_validate_sqlite` ~`:770`, `run_rollback_pg` ~`:1542`, `run_rollback_sqlite` ~`:1572`; grep `load_dir` within the file to confirm) | platform Flyway-mode loads only `.sql` (single-step plans); the runner consumes them through the thin `AppliedPlan::single_step() -> Result<&Migration, NotSingleStep>` facade (yields the one `&Migration`, fails closed on a multi-step plan), staying decoupled from plan-shape evolution. Behavior preserved, proven by the differential gate **and the single-step-shape precondition test** below. |
| `crates/zeroship-migrate/src/lib.rs:144-146` (`loader` re-export) + `lib.rs:111-113` (`engine` re-export) | The `load_dir` re-export (`lib.rs:144-146`) changes return type to `Vec<AppliedPlan>`. The **three new symbols `AppliedPlan` / `PlanStep` / `RenameStep` are added** to the public surface (from the new `apply_plan`/plan module). The **existing dry-run `MigrationPlan` re-export (`lib.rs:113`) is UNCHANGED** — it stays exported, still meaning the lint/dry-run preview. There is no collision because the two are distinct symbols: the public API diff is `+AppliedPlan, +PlanStep, +RenameStep` (added) with `MigrationPlan` (kept), not a redefinition. |
| Tests: `control/tests/deploy_migrate_test.rs`; `loader.rs` tests; `cli_dbmate_e2e_sqlite.rs`; `schema-authority-e2e/tests/capstone_pipeline_pg.rs` | updated to assert over `Vec<AppliedPlan>` (a `.sql` migration is `plans[i].steps == [Ddl(_)]`); existing version-order/duplicate/destructive assertions move onto the plan's single step. |

**Public dbmate CLI semantics are preserved under the plan model.**
- **`status`** lists one row per *version* — a single-step `.sql` plan is one version = one row; a multi-step IR plan is still one version row (its steps are sub-versions, §2.0.1), with the per-step journal visible only in a `--verbose` view.
- **`rollback`** is per-version via the plan-level rollback driver (§2.1.2); for a `.sql` plan this is byte-identical to today's per-`Migration` rollback.
- **`apply`** applies pending plans in version order via `apply_plan`; a `.sql` plan's single `Ddl` step applies exactly as `apply_with_lock_backend` does today.
A faithful-e2e test drives the real CLI over a mixed `.sql`-only directory and asserts `status`/`apply`/`rollback` output is byte-identical to the pre-change CLI.

**Platform Flyway-mode (Task #54) gets a mandatory differential gate — "single-step plans are equivalent" is NOT accepted as proof.** Platform Flyway-mode is the shipped Liquibase replacement (Task #54) with its own invariants and the highest-blast-radius caller. The differential test runs the **full existing platform changelog through both loaders** — the pre-change `Vec<Migration>` loader and the post-change `Vec<AppliedPlan>` loader — applied against a real Postgres, asserting **byte-identical applied schema + byte-identical journal rows** (version order, checksums, flags, baseline markers). The platform changelog itself is the oracle, captured pre-change and reproduced post-change.

**Decoupling: platform Flyway-mode consumes a thin `Migration`-facade, NOT raw `plan.steps`.** `load_dir` returns `Vec<AppliedPlan>` (the IR feature needs it), but `platform_runner.rs` consumes plans through `AppliedPlan::single_step() -> Result<&Migration, NotSingleStep>` — the platform runner keeps operating over `Migration` and never touches `PlanStep`/`RenameStep`; the adapter fails closed if a platform changelog ever produced a multi-step plan (it cannot today). A future `PlanStep`/`RenameStep` change for a creator feature does not ripple into `platform_runner.rs`.

**The single-step precondition the facade relies on is PROVEN by a test, not asserted in prose.** The differential gate above checks schema/journal equality; it does NOT, by itself, prove the shape precondition `single_step()` depends on. So PR0 adds a dedicated precondition test: **every `.sql` loaded in platform Flyway-mode lowers to a plan whose `steps == [Ddl(_)]`** — asserted by iterating the full platform changelog (and a property test over generated Flyway-mode `.sql`) and checking each produced `AppliedPlan` has exactly one `Ddl` step. With that test green, `single_step()`'s fail-closed `Err(NotSingleStep)` arm is provably unreachable on the platform path — the fail-closed branch exists for defense-in-depth, but the precondition test demonstrates the platform path never exercises it.

### 5.3 Versioning & drift over the IR

- **Versioning**: the filename numeric prefix is the version (author-assigned at `new`, or by `generate`). Stable across builds because it is baked into the filename. The `MigrationId` is derived deterministically; `V1 < V2 < V10` ordering holds.
- **Drift**: the checksum (over the canonical op list + flags + deps + supersedes + preconditions — `owner_app` excluded from the hint, §2.4) is the drift anchor. Because the `.ir.json` is committed build-once and never regenerated for an applied version (§5.1), a drift abort means exactly one intentional thing: the committed artifact for an already-applied version was edited. This is identical to how editing an applied `.sql` is caught, and a benign rebuild does not trigger it.
- **IR schema version** (`ir_version`): code-evolution discipline (per `AGENTS.md`: "wire-format versioning is for code-evolution, not user-compat"). Bumping `ir_version` is how the IR shape evolves across engine versions; the loader rejects an unknown future `ir_version` fail-closed.
- **A v2 engine reading an OLD committed v1 `.ir.json`** — resolved by build-once + journaled-checksum:
  - **Already-applied v1 artifacts: read v1 deserialization only to re-derive the checksum for the drift check — never re-apply.** A v2 engine retains a v1 deserializer used solely on the drift path: it parses the committed v1 `.ir.json`, recomputes `Checksum::of_ir`, and compares to the journaled checksum. If they match, the migration is satisfied and skipped — no v2 lowering needed. (This is not a rejected back-compat shim: it is the minimal read-only deserialization required so an already-journaled artifact's drift check passes.)
  - **Not-yet-applied v1 artifacts: re-emit through the canonical toolchain (§8.9.1)**, not the running engine, and re-commit. No checksum is invalidated (the artifact is not yet journaled).
  - **Forbidden: bumping `ir_version` in a way that would change an applied migration's checksum.** An `ir_version` bump must be checksum-neutral for already-applied artifacts; a bump that changes an op's required fields is permitted only for not-yet-applied ones.
  - **Fixture:** a committed v1 `.ir.json` already journaled is read by a (simulated) v2 engine, its `Checksum::of_ir` re-derived via the retained v1 deserializer, and asserted equal to the journaled checksum.

### 5.4 Squash and `generate` over committed IR plans

The §5.1 "committed, build-once, never-regenerated" contract is for an *individual* version's artifact. Squash and `generate` legitimately synthesize new content over existing history:

**Squash composes a *union op list*, not a union of rendered SQL.** Today squash collapses `v1..vN` into one superseding migration `S` with `S.supersedes = [v1..vN]` (`migration.rs:396-410`). For IR plans:
- `S` is itself an `.ir.json` whose `ops` is the **canonicalized union of the superseded plans' op lists**, in version order, with the squash optimizer collapsing trivially-cancelling pairs (an `addColumn x` in v1 then `dropColumn x` in v3 both drop out; an `addColumn` then a later `alterColumnType` on the same column collapse to a single add of the final type). Squash composes over **ops** (the dialect-neutral source of truth), never over rendered per-dialect SQL.
- **`S.checksum` is computed exactly like any IR plan's checksum** — the engine folds `S`'s canonical op list via `Checksum::of_ir`. It has no derived relationship to the superseded plans' checksums; the linkage is carried structurally by `S.supersedes = [v1..vN]`, which folds into `S`'s checksum as an ordered list. The "satisfied-by" model (`migration.rs:404`) treats `v_i` as satisfied if `S` is net-applied.
- **The combined `up` is re-derived per dialect at load**, from `S`'s union op list, by the same emitters.

**The squash predicate retains data/online steps un-collapsed; it does NOT refuse a mixed range.** Today's `squash` (`squash.rs:173`) is **content-agnostic** — the operator authors `S` (a `Migration` with an arbitrary `up: String` + a `supersedes` list); the engine guards `S.up` (`squash.rs:194-200`) and records the supersession via `record_squash` (`backend.rs:229`), marking `v1..vN` satisfied (`migration.rs:404`). It never inspects the superseded range's contents, never drops a data step, and never refuses a range for containing one. A real long-lived project's history is full of mixed DDL+backfill versions; refusing to squash any range containing a backfill would mean such projects can never squash. So the IR squash must not be stricter:
- **Squash the DDL spine; RETAIN the data/online steps as un-collapsed members of `S`, in version order.** `S` is an `.ir.json` whose `steps` are: the collapsed DDL spine (the trivially-cancelling-pair optimizer applied only across the `Ddl` ops) interleaved with the original `Backfill`/`Dml`/`OnlineRename` steps from the superseded range, carried through verbatim in their original relative order. A `Backfill` in v3 between DDL in v1 and v5 stays a `Backfill` step in `S`, positioned after the collapsed v1-spine and before the collapsed v5-spine.
- **Why retain rather than collapse data steps.** A `Backfill`/`Dml`/`OnlineRename` step has no general "collapse two data transforms into one" algebra — collapsing is undecidable in general, so the design does not attempt it. "Do not collapse" is **not** "refuse the squash": the safe, capability-preserving answer is to keep each data step as its own member of `S` and let `apply_plan` re-run it in order.
- **Rollbackability follows §2.1.2.** Because `S` may contain `down: None` data steps, `S` itself may be `rollbackable: false` — surfaced at plan time. This is strictly more honest than a hand-authored `S.up` of data SQL that is opaquely irreversible.
- **A retained data step is a SUPERSESSION MARKER, with two application contexts:**
  - **Context A — `S` applied as a supersession over an EXISTING database that already applied `v1..vN` (the normal squash path).** `S`'s job is purely to mark `v1..vN` satisfied (`migration.rs:404`) and collapse the file count; the DB already has the data the historical steps produced. The retained data steps carry the **original historical step's sub-version verbatim** (the §2.0.1 squash exception — NOT a fresh `uuidv7_derive(S.version, idx)` id), which is already journaled, so `apply_plan` **net-applied-skips** each by id match. (If the retained step were given an `S`-derived id, the skip would fail and the data step would re-execute — which is exactly why the §2.0.1 exception preserves the original id.) The retained step is a *record* that the data transform happened, not a re-execution.
  - **Context B — `S` applied to a FRESH/EMPTY database (a from-scratch rebuild). Retained steps are split by replayability:**
    - **A literal-rows `insert` `Dml` IS REPLAYED on a fresh rebuild — and ONLY that provably-structural case.** The `replayable_on_fresh: true` tag is decided by a **purely structural predicate the squash optimizer can evaluate without any data-flow analysis**: the step is a `PlanStep::Dml` whose op is an `insert` of **literal rows** (the §2.3.2 `rows` facet — typed scalar values, no expression referencing existing data), optionally carrying `onConflict do nothing`. These insert fixed reference rows that exist in every environment; their correctness is independent of pre-existing data by construction of the op shape, not by a judgement about runtime behavior.
    - **EVERYTHING ELSE is tagged `replayable_on_fresh: false`** — any `update`, any non-literal `insert` (one whose values are an expression / reference other columns), any `backfill`, any `OnlineRename`. The design deliberately does **not** attempt to decide whether a given `update` is "idempotent" or "independent of prior data" — that is the same undecidable data-flow analysis the design refuses elsewhere (it is why collapsing two backfills is undecidable). So the predicate is the structural one above and nothing finer: a literal-rows `insert` is `true`; all other steps are `false`. (A batched backfill's cursor/filter predates the collapsed spine; an online rename's expand-contract against a DB already in final shape is incoherent; a data-dependent `update` against an empty table is a no-op-or-wrong — all correctly fall under `false`.) These are skipped on a fresh rebuild.
    - **The skip is SIGNALLED at runtime, not buried in prose.** When a fresh rebuild skips any `replayable_on_fresh: false` retained step, the apply emits a loud status flag (`SQUASH_FRESH_REBUILD_DATA_NOT_RECONSTRUCTED`, surfaced in `status` and the apply output, §8.8) naming exactly which historical data steps were not reconstructed.
  - **Why this split, not blanket-replay or blanket-skip.** Blanket-replay is wrong (re-running a historical batched backfill against an empty DB is a no-op-or-wrong); blanket-skip silently loses reference rows. Replay the structurally-provable case (literal-rows `insert`), skip-and-signal the rest.
- **Decidable + reviewer-stable.** The predicate is purely structural: DDL ops collapse via the algebra; non-DDL steps pass through unchanged in order. (The operator-authored `.sql` squash via the existing `squash()` path remains available for a raw `S.up`.)
- **Build-once interaction**: `S` is a *new* version with its own filename and its own committed `.ir.json` — emitted once, committed, then immutable. It does not regenerate the superseded files; it supersedes them.
- **Fixtures:** (1) squashing two `op.*` DDL migrations into one `S.ir.json`, asserting the union op list is canonical, `S.checksum` is stable and dialect-identical, `S` renders + applies on both backends, `S.supersedes` marks the inputs satisfied; (2) a mixed-range fixture squashing `v1 (addColumn)` + `v2 (backfill)` + `v3 (addColumn)` into one `S` whose `steps` are `[Ddl(collapsed v1+v3 spine), Backfill(v2 retained verbatim)]` in correct relative order, asserting the backfill survives un-collapsed, `S.rollbackable == false`, and `S` applies + supersedes correctly on PG; (3) a fresh-DB fixture applying `S = [Ddl(spine), Dml(idempotent seed insert, replayable_on_fresh:true), Backfill(retained, replayable_on_fresh:false)]` to an empty DB and asserting (i) the schema matches the spine, (ii) the seed `Dml` IS replayed (reference rows present), (iii) the retained `Backfill` is not executed AND the apply emits `SQUASH_FRESH_REBUILD_DATA_NOT_RECONSTRUCTED`, (iv) applying the same `S` as a supersession over a DB that already ran `v1..vN` net-applied-skips both data steps and leaves data intact (no double-insert); (4) **retained-step sub-version preservation**: in a squash `S` retaining a `v2` backfill, the retained step's sub-version in `S` equals the original `uuidv7_derive(v2.version, original_idx)` (the §2.0.1 exception) and is NOT `uuidv7_derive(S.version, idx)` — asserted directly, and asserted to drive the Context A net-applied-skip.

**`generate`/regenerate is keyed on APPLIED-ness, not on whether a committed file exists** — so the AI iterate-and-fix loop does not churn versions. The build-once contract makes an *applied* version immutable; it does not make a *not-yet-applied draft* untouchable. Distinguished by the **journal**, not by file presence:
- **Target version is already APPLIED (has a journal row): refuse fail-closed** ("version `<NNNN>` is already applied; author a new forward migration"). The immutable-deployed-history guarantee.
- **Target version is NOT yet applied (no journal row), even if a committed `.ir.json` exists: OVERWRITE freely.** The AI builder's natural cycle is `generate → validator rejects → regenerate the SAME version`. Forcing a version bump on every correction would produce churn (`0007, 0008, 0009, …`) for one logical migration fixed before it ever deploys. So `generate`/the AI loop overwrites a not-yet-journaled `<NNNN>_<desc>.ir.json` (and its `.ts`) without refusal, re-recording through the canonical recorder (§8.9.2) and re-committing. No checksum is invalidated; no deployed history changes.
- **The human `generate` keeps a friendlier guard for an accidental collision.** When a human runs `generate` and the computed next version collides with an existing not-yet-applied draft, the CLI prompts/warns (interactive); `--force` (and the AI loop's non-interactive mode) overwrites.

---

## 6. Threading the existing engine

The IR reuses the existing **DDL apply machinery** (guard, role, journal, executor, drift, shadow-DB), but the front door is **not** "thin dispatch." The genuinely net-new Rust:

| New Rust surface | Why it is new (not reuse) | PR |
| --- | --- | --- |
| **`AppliedPlan` + `PlanStep` + `RenameStep` + a single shared `apply_plan` executor with dual-execution rename dispatch** (§2.0/§2.6.2) | `Migration` is a single `up:String`; an `.ir.json` lowers to an ordered plan of phases (Ddl/Dml/Backfill/OnlineRename). The step *types* are reused, but **no generic ordered-plan executor exists**, and a rename has two unrelated execution interfaces: PG via `OnlineSchemaChange::run_online` (PgOnline only) and SQLite via `MigrationBackend::rebuild_one` — driven today by two separate code paths inside `apply_declarative_locked` (the `run_online` loop `engine.rs:533-552` and the `plan.rebuilds` loop `:491-503`), both welded to the declarative `DeclarativePlan` shape; `apply_inner` (`:628`) flattens to `Vec<Migration>` with no backfill/online/rebuild. PR0 **builds the single shared `apply_plan(Vec<PlanStep>)`** by lifting BOTH rename-execution arms + the interleave/journal/`pending_contract` logic out of `apply_declarative_locked`, dispatching `OnlineRename` by `RenameStep` variant, reusing `run_online`/`rebuild_one`/`apply_with_lock_backend`/`run_backfill` as destinations — and **re-points `apply_declarative_locked` onto `apply_plan` via a thin shape-adapter** so there is one orchestrator (§6.0). Also generalizes `load_dir`→`Vec<AppliedPlan>` behind the `single_step()` facade (§5.2). | PR0 |
| **IR deserialization** (`MigrationIr` + `Op` enum, serde + `schemars`) | `schemars` is not a dependency today; the IR types don't exist | PR1 |
| **`IrAuthor` snapshot-construction lowering** | `DdlEmitter` methods take `ColumnSnapshot`/`IndexSnapshot` (`drift.rs:245`/`:319`), not op fields — IrAuthor must build snapshots. The default/system-field/sentinel logic is shared with the differ via an **extracted shared snapshot-builder** (§6.5 enumerates the per-op fields + their source-of-truth helpers); IrAuthor calls it, never re-implements it | PR1 |
| **Stand-alone constraint + `alterColumn*` render coverage** | today these are produced inside `DeclarativeAuthor::diff`/the rebuild planner, not as stand-alone callable renders | PR1 (constraints/alter) / PR2 (online) |
| **`OnlineIntent`→SQLite-rebuild bridge + neutral-type translation** | `ExpandContractAuthor` is PG-only and its `ty` is a raw PG type string (`expand_contract.rs:82`); the SQLite rebuild is a different subsystem; nothing wires them. `IrAuthor` must bridge them and map the IR's dialect-neutral `ColType` → PG type / → SQLite affinity (§2.6). Execution rides PR0's dual-dispatch (§2.6.2) | PR2 (online) |
| **Creator DML assembler** (INSERT/UPDATE/DELETE: identifier + parameterized value binding, NULL/typed-literal handling, onConflict routing) | no creator-DML assembler exists (`executor.rs:1951`/`author.rs:614` are internal-only) | PR6a (PG + SQLite one-shot DML) |
| **SQLite backfill executor** (§2.3.1) | existing executor is Postgres-only (writable CTE + `pg_catalog`) | **PR6b — committed scope** (the largest net-new piece; the bi-dialect headline) |
| **Portable-expression grammar validator** (§3.3.1) | a shared token/AST validator that parses an `op.sql` `set`/`filter` fragment against the closed portable grammar and rejects anything outside it on both dialects. libpg_query parses PG only, so this cannot reuse it for SQLite — a new dialect-neutral fragment parser. **Rust recognizer is the sole apply-time gate; the JS side is a strictly-more-rejecting lint, enforced by a one-directional differential fuzzer (JS-accept ⇒ Rust-accept)** (§3.3.1.1). The shared-grammar-table + per-language Pratt interpreter is an optional future upgrade, not a prerequisite. | PR1 (the AI loop's primary feedback signal; pure analysis, no executor dependency; PR6a only wires it into the DML path) |
| **Structured-error envelope** (§8.8) | every authoring-time rejection carries `{ code, op_index, ts_location, dialect, reason, suggested_fix? }`; the human string is its projection | PR1 |
| **Guard-per-fragment + reassembly** (§6.1.1) | today `SqlGuard::check` runs over a whole migration's `up`; the IR guards each rendered op fragment individually (for op-index + `.ts` attribution) then concatenates the guarded fragments into the step `up`, with a byte-identity invariant | PR1 |
| **IR-path ownership check** (§8.6) | `DeclarativeAuthor::diff` ownership enforcement is bypassed by the op path; the IR check must replicate both the union-table enforcement (`declarative.rs:2826`) and the drop-path unknown-owner fail-closed rule (`declarative.rs:2820-2824`) | PR1 |

What is reused verbatim is everything downstream of the `PlanStep` types: guard, least-priv role, journal, the per-`Migration` executor (`apply_with_lock_backend`), drift, squash, baseline, shadow-DB, preconditions, the existing PG backfill (`run_backfill`) + PG expand-contract renderers (`run_expand_pg`). What is not reused-as-is — and is the largest single PR0 item — is the *phase orchestration*: today it exists only inside `apply_declarative_locked` (`engine.rs:446`), welded to the declarative shape; PR0 lifts it into the single shared `apply_plan` and re-points the declarative path onto it.

### 6.0 ONE shared orchestrator (the convergence is core, not deferred)

The design end-state is a **single shared plan-orchestrator, `apply_plan`**, over the ordered `PlanStep` list. There are **not** two orchestrators with a "maybe-converge-later" framing. PR0:
1. Builds `apply_plan(Vec<PlanStep>)` by lifting the interleave/journal/`pending_contract`/rebuild logic out of `apply_declarative_locked`, dispatching `OnlineRename` by `RenameStep` variant (§2.6.2), reusing the existing downstream primitives (`run_online`/`run_expand_pg`/`rebuild_one`/`apply_with_lock_backend`/`run_backfill`) as destinations.
2. **Re-points the shipped declarative path (`apply_declarative_locked`, from Task #46/#54) onto `apply_plan` via a thin shape-adapter** that maps a `DeclarativePlan` to a `Vec<PlanStep>` (its `items` → `Ddl` steps, its `renames` → `OnlineRename(PgExpandContract)` / `OnlineRename(SqliteRebuild)` steps by dialect, its `rebuilds` → `OnlineRename(SqliteRebuild)` steps). After PR0 there is exactly one orchestrator; the declarative path is a producer of `Vec<PlanStep>` feeding it.

**The regression safety net is the EXISTING declarative test suite (~507 tests), which MUST stay green when the declarative path is routed through `apply_plan`.** This is REQUIRED CORE scope, not an optional later PR. The re-point and its safety net are part of PR0. The verification discipline:
- **Frozen-commit golden trace as an immutable oracle — covering EVERY distinct behavioral path `apply_declarative_locked` can take.** Before re-pointing, capture golden journal+schema traces from the current `apply_declarative_locked` (run at a frozen git commit) and check them in as immutable fixtures (`crates/zeroship-migrate/tests/golden-traces/`). Because once re-pointed the old path *becomes* the new path, a shared bug would pass a self-equivalence test — so the diff is against artifacts captured **before** the re-point. Required frozen paths: (a) multi-deploy online PG rename (deploy-N EXPAND + `pending_contract` → deploy-N+1 contract under approval); (b) SQLite-rebuild rename (the 12-step `rebuild_one` path); (c) destructive-requires-approval REFUSAL (`engine.rs:468`); (d) net-applied-skip idempotent re-run (`engine.rs:491-503`); (e) crash-resume at each phase boundary (after E1, E2, E3-but-before-backfill-journal, after backfill, between the two contract steps); (f) mixed rebuild + rename + plain deploy in one declarative apply; (g) empty-renames SQLite fail-closed assertion (`engine.rs:521-532`); (h) `LockMode::AlreadyHeld` re-entrancy across sub-batches (`engine.rs:457/:544`). PR0 asserts the post-convergence `apply_declarative_locked`-via-`apply_plan` reproduces each pre-convergence trace byte-for-byte.
- **The full existing declarative suite (~507 tests) re-runs green against the re-pointed path.** It is the standing regression net; it must stay green when routed through `apply_plan`.
- **Property-based / fuzz over plan shapes WITH INJECTED CRASHES, for the net-new topologies that have no declarative oracle.** `apply_plan` runs shapes the declarative path never produced (a standalone `PlanStep::Dml`, a single-`.ir.json` `Ddl → Backfill → Ddl` interleave, multi-`Backfill` plans). A frozen trace of old behavior cannot cover them. A property test generates random in-grammar plans and injects a crash at every step/sub-step/journal-write boundary, asserting "resume from any crash point reaches the same final DB state + journal as the no-crash apply," cross-checked against a **model of the journal state machine** (net-applied/pending/satisfied transitions). A failing seed is a minimized counterexample.

### 6.1 Pipeline

```
.ir.json
  └─ loader: deserialize MigrationIr (loader.rs, new branch); load_dir -> Vec<AppliedPlan> (§5.2)  ← NEW
       └─ owner_app := deploying app id (server-stamped, §8.6)       ← NEW
       └─ target_dialect := backend selection from deploy_migrate.rs (§2.4.1)  ← NEW
       └─ IrAuthor::lower(ops, target_dialect) -> AppliedPlan     ← NEW; routes per op+target_dialect
            ├─ createTable  → zeroship_schema::query CREATE-TABLE    declarative.rs:~430-565
            ├─ add/drop col, create/drop idx, drop table
            │               → DdlEmitter (build Snapshot first)      declarative.rs:3810  ← NEW lowering
            ├─ alterColumn* / constraints
            │               → DeclarativeAuthor render / rebuild     declarative.rs:3609  ← NEW stand-alone coverage
            ├─ renameColumn → PG: ExpandContractAuthor (E1..C2)      expand_contract.rs:285
            │                  SQLite: rebuild planner (bridge)      backend_sqlite/rebuild_sql.rs  ← NEW bridge (§2.6)
            ├─ insert/update/delete → DML assembler → PlanStep::Dml  ← NEW (PR6a; binds carried, §2.3.2)
            │                  (set/filter body validated by the §3.3.1 grammar validator ← NEW PR1)
            ├─ backfill/update{batch}
            │               → PG: BackfillSpec (backfill.rs:286)
            │                  SQLite: NEW backfill executor (§2.3.1)  ← NEW (PR6b, committed)
            └─ raw          → PG: verbatim+guard / SQLite: refused (§8.7)
       └─ per-table ownership check (IR path, §8.6)                  ← NEW
       └─ AppliedPlan { steps: Vec<PlanStep>, … } (§2.0)           ← NEW
            └─ Checksum::of_ir(canonical-IR, §2.4)  NEW front door; shares fold_common w/ Checksum::of (migration.rs:303)
  └─ engine.plan: guard.check PER RENDERED OP FRAGMENT (PG) / authorizer (SQLite)  guard.rs / engine.rs
  │    └─ on denial: attribute to op index + .ts source-map location (§6.1.1)  ← NEW DX
  └─ provision_migrator (least-priv role)                            role.rs:197      ← reused
  └─ executor.apply_plan: run each PlanStep in order —               NEW (PR0): the ONE shared orchestrator
  │    Ddl txn / Dml (template+native binds) / Backfill batched (run_backfill)
  │    OnlineRename → DISPATCH by RenameStep (§2.6.2): PG PgExpandContract→run_online (+pending_contract);
  │                                                    SQLite SqliteRebuild→MigrationBackend::rebuild_one
  │    (per-Migration step delegates to apply_with_lock_backend — engine.rs:661 ← reused)
  │    (apply_declarative_locked re-pointed onto apply_plan via the shape-adapter, §6.0)
  └─ journal: per-step + plan-group marker (immutable triggers)      journal.rs       ← reused (§2.0.1)
```

#### 6.1.1 Per-op guard attribution (DX, not just a yes/no gate)

The guard runs **per rendered op fragment**, not over the opaque concatenated `up` blob. `IrAuthor` renders each op into its own fragment(s) and records, for each, the originating op index and the `.ts` source-map location. When `SqlGuard::check` (PG) or the authorizer (SQLite) denies a fragment, the engine surfaces:

```
migration 0007_split_name: op #3 (op.raw at migrations/0007_split_name.ts:14)
  denied by guard: statement `CREATE EXTENSION` is not permitted under the Confined profile
```

rather than a bare "statement denied." The §4.1 source-map hint is load-bearing for guard errors.

**This is a real executor change, with a pinned order and byte-identity invariant.**
- **Order: guard-each-fragment, then concatenate.** Every fragment is passed through `SqlGuard::check` (PG) / the authorizer (SQLite) individually (carrying its op-index + source-map location); only after all fragments pass is the step's `up` assembled by concatenating exactly those guarded fragments, in order, with a single `;`-newline separator and no other interstitial text.
- **Byte-identity invariant.** `applied_up == join(guarded_fragments)`. Nothing may be inserted, rewritten, re-quoted, or normalized between guarding and concatenation.
- **Why fragment-then-concatenate.** A single op can render multiple statements (e.g. `createTable` emits the table + inline index + a `comment_stmt` side output); guarding the pre-concatenation fragments keeps each attributable and guarantees no statement is born from concatenation.
- **Test:** for a multi-statement op (`createTable` with an inline index and a comment), assert `applied_up` is byte-identical to the concatenation of the individually-guarded fragments, and that a denied fragment (an `op.raw` `CREATE EXTENSION`) aborts the whole step with the op-index + `.ts` location, with nothing applied.

Each plan step that renders SQL (`Ddl`, the `Dml` template, the E1..C2 of an `OnlineRename`) carries its own fragments, so attribution + the byte-identity invariant hold across the multi-step plan model.

### 6.2 What is reused unchanged

- **Emitters**: `PgEmitter` / `SqliteEmitter` (`declarative.rs:3864` / `:3999`) render the 5 ops the `DdlEmitter` trait covers. The IR author calls the same methods the declarative differ calls — but must construct the `ColumnSnapshot`/`IndexSnapshot` inputs first (net-new lowering, §6). `createTable` and constraints/`alterColumn*` go through other seams.
- **Guard (PG) / authorizer (SQLite)**: every rendered PG `up` passes `SqlGuard::check` (`guard.rs`) in `Confined`. The SQLite path has no libpg_query parse guard; its line-2 defense is the SQLite runtime authorizer, and `Confined` SQLite accepts only descriptor/IR-generated DDL, refusing raw SQLite fail-closed (§8.3, §8.7).
- **Least-priv role**: applies under `migrator_<project>` (`role.rs:197`), zero grants on foreign/meta schemas.
- **Journal**: admin-only append-only writes, immutability triggers (`journal.rs:332`).
- **Backfill executor (PG), expand-contract sequencer, SQLite rebuild, preconditions, drift, squash, baseline, shadow-DB dry-run**: all reused. The IR is a new front door to the same machinery.

### 6.3 SQLite rebuild & online routing are automatic

The author does not choose "rebuild" — routing to the SQLite rebuild is `IrAuthor`'s job, and for renames it is a net-new bridge (§2.6):
- For type/nullability/constraint changes and constrained drop-column on an existing SQLite table, the rebuild input is constructed by `IrAuthor` and handed to the rebuild planner (the same `SqliteRebuildSpec`/`rebuild_sql.rs` machinery; the `sqlite_existing_table_needs_rebuild` detector at `declarative.rs:3110` informs *when* a rebuild is needed).
- For **renames specifically**, the PG path uses `ExpandContractAuthor` (`OnlineIntent`) while the SQLite path uses the rebuild planner — two different subsystems `IrAuthor` must bridge by dialect (§2.6). Pre-existing code does not connect `OnlineIntent` to the SQLite rebuild; that wire is new (PR2).

### 6.4 Render-seam parity: stand-alone `IrAuthor` render must be byte-identical to in-diff render

`createTable` routes through the `zeroship_schema::query` CREATE-TABLE emitter (via `DeclarativeAuthor`) and constraints/`alterColumn*` through render methods that today exist only inside `DeclarativeAuthor::diff`. Those helpers are tuned for the declarative path (system-field injection, `comment_stmt` side outputs at `declarative.rs:48`), so extracting a stand-alone render for `IrAuthor` carries a parity risk.

**The mitigation is a cross-path golden gate (PR1).** For `createTable` and each constraint / `alterColumn*` op, a test asserts that the stand-alone `IrAuthor` render is byte-identical to the render the declarative path produces for the same logical shape — construct the same table/constraint two ways (a `t.*` schema fed through `DeclarativeAuthor::diff`, and the equivalent `op.*` IR fed through `IrAuthor`) and assert the emitted SQL (including `comment_stmt` side outputs and injected system fields) is identical on both PG and SQLite. If a future change forks their output, the golden fails.

### 6.5 Snapshot construction — the single largest net-new lowering, specified

The `DdlEmitter` methods take `ColumnSnapshot`/`IndexSnapshot` (`drift.rs:245`/`:319`), **not** op fields, and the declarative path derives those snapshots from a `desired_snapshot` the op path never computes (§8.6). So the highest-parity-risk net-new piece is `IrAuthor` constructing a faithful `ColumnSnapshot`/`IndexSnapshot` from an op's plain fields. This is specified here, not left to reverse-engineering.

**MANDATE: `IrAuthor` does NOT hand-construct snapshots — it calls a shared snapshot-builder helper extracted from the declarative path.** The default/system-field/sentinel logic must exist in exactly ONE place. PR1 **extracts the per-column / per-index snapshot construction the differ already performs in `desired_snapshot`/`desired_snapshot_for_dialect` (`declarative.rs:1013`/`:1044`) into a shared, dialect-parameterized builder** that BOTH `desired_snapshot` (unchanged behavior) and `IrAuthor` call. `IrAuthor` lowering an `addColumn`/`createIndex` op routes the op's fields through this same builder; hand-constructing a second copy of the default/system-field/sentinel logic inside `IrAuthor` is a design violation (it would make the §6.4 byte-identity gate a perpetual maintenance tax — every future schema-emitter change would have to be mirrored in two places). The shared builder is the single source of truth; the golden gate (§6.4) then merely guards against accidental regressions, not against two independent implementations.

**Per-op snapshot-field population (what the shared builder must fill).**

| Op | `ColumnSnapshot` / `IndexSnapshot` fields the shared builder populates | Where that logic lives today |
| --- | --- | --- |
| `addColumn(table, name, type, {nullable?, default?})` | `ColumnSnapshot { name, data_type, nullable, default, encryption_sentinel, comment_sentinel }` — `data_type` from the neutral `ColType`→dialect mapping (`def_to_column_type_for_dialect`); `nullable` from the op (default `false` per the `t.*` lexicon unless `.nullable()`); `default` rendered by the shared kernel; the two sentinels built by `zeroship_schema::{query,mask_codec}` for an encrypted/masked column (NEVER re-spelled in `IrAuthor`) | `desired_snapshot` per-field loop (`declarative.rs:1069-1101`); `def_to_column_type_for_dialect`; `system_field_columns` (`:844`); the `mask_codec` kernel |
| `createTable(name, columns[], …)` | a `TableSnapshot` whose columns are the **seven platform system fields injected first** (`system_field_columns`, `declarative.rs:844`) then one `ColumnSnapshot` per declared column via the addColumn rule above, plus the **three system-field indexes** (`system_field_indexes`, `:870`). `createTable` routes through the `zeroship_schema::query` CREATE-TABLE emitter, which consumes this `TableSnapshot` exactly as `DeclarativeAuthor` feeds it | `desired_snapshot` table-assembly (`declarative.rs:1069-1101`); `system_field_columns`/`system_field_indexes` |
| `createIndex(table, columns[], {name?, unique?, using?, where?})` | `IndexSnapshot { name, unique, columns, access_method (from `using`, default `btree`), expression (from `where`/an expression key), opclass (for an ANN `using`) }` | `vector_index_snapshot`/`geo_index_snapshot`/`fts_index_snapshot_sqlite` (`declarative.rs:1456`/`:1482`/`:1598`) + the plain-index path |
| `dropColumn` / `dropIndex` / `dropTable` | `DdlEmitter::drop_*_up` take only the name + qualification — no rich snapshot to build; `IrAuthor` passes the identifier through | `DdlEmitter` drop methods (`declarative.rs:3936-3948`) |

**System-field injection rule (the trap).** A creator does NOT author the seven platform system fields (`id`/`created_at`/`updated_at`/…) — `createTable` injects them via the SAME `system_field_columns()`/`system_field_indexes()` the differ uses, in the same canonical order. An `addColumn` of a user column NEVER re-injects a system field. The shared builder owns this; `IrAuthor` does not decide it.

**Sentinel rule (the second trap).** The `encryption_sentinel` and `comment_sentinel` are emission-only metadata built by the shared `zeroship_schema::{query,mask_codec}` kernel (`drift.rs` type docs). `IrAuthor` MUST obtain them from that kernel (the same call the differ makes), never re-spell the `/* zsenc:… */` / `__zsmask:…` strings. An `addColumn` of an encrypted/masked `ColType` carries the sentinel by routing through the shared builder; a hand-spelled sentinel is a design violation.

**Fixtures (PR1, beyond the §6.4 byte-identity gate).** (1) `IrAuthor`'s `addColumn` of an encrypted column produces a `ColumnSnapshot` whose `encryption_sentinel`/`comment_sentinel` are byte-equal to the differ's for the equivalent `t.encrypted(...)` field — proving the shared kernel is the source, not a re-spell; (2) `IrAuthor`'s `createTable` injects exactly the seven system fields + three system-field indexes in canonical order, byte-equal to `desired_snapshot`; (3) a refactor-safety test: the extracted shared builder, called from the differ's old call site, reproduces the pre-extraction `desired_snapshot` output byte-for-byte (the extraction is behavior-preserving).

---

## 7. Relationship to the declarative path and the dbmate CLI

### 7.1 Both paths produce the same IR (autogenerate parity)

The declarative differ (`generate.rs:112` `DeclarativeAuthor::diff`) currently emits `Migration`s (SQL). It will instead emit an **`op.*` `.ts` scaffold** (and/or the `.ir.json` directly), because both paths converge on the same op vocabulary. This is **Alembic autogenerate parity**: `zeroship-migrate generate` diffs the `t.*` schema against the live/snapshot schema and writes an `op.*` migration the author reviews and augments with DML (the differ leaves `// TODO: backfill` markers where a non-null add or a type narrowing needs data). The differ's emitted DDL ops are exactly the portable op set in §2.3.

The two paths are complementary, not competing:
- **Declarative** = "describe the desired shape, let the engine diff" (great for pure schema changes; zero DML).
- **`op.*`** = "author the change imperatively, including data" (the primary path; covers everything).
- `generate` bridges them: schema diff → `op.*` scaffold.

### 7.2 The dbmate-style Rust CLI

The public CLI (Track A, Task #55) keeps `apply` / `status` / `rollback` / `load`. Changes:
- `new` scaffolds a `.ts` `op.*` migration by default (was: `.sql`).
- `generate` emits `op.*` scaffolds (autogenerate).
- The raw-SQL loader **stays** — the engine still ingests `.sql` (Flyway + dbmate grammars at `loader.rs:19`), because (a) `op.raw` renders to SQL, (b) the public dbmate-compatible CLI still accepts hand-written `.sql`, and (c) platform-internal Flyway-mode migrations are SQL.
- **Retired as default for new authoring**: hand-writing dialect-specific raw SQL. It is still loadable, just not what `new`/`generate` produce or what docs recommend. Per the pre-launch no-back-compat stance, we do not build a "convert my old .sql to op.*" shim.

---

## 8. Security & threat model

**Threat**: untrusted creator/AI `op.*` code at build time tries to escalate, reach other tenants, run RCE, forge history, or read files.

The three defense layers stay locked because the DSL emits only IR *data*, and all rendering/guarding/applying stays trusted-Rust:

1. **Build-time op code emits data, not execution.** The recording `op` builder is a pure JS object in the author's own Node build. It produces a JSON op list, opens no DB connection, executes no SQL. The engine never runs the author's code — it ingests JSON it deserializes and validates. An attacker controlling the `.ts` controls only the op list, which the engine subjects to the full guard/role/journal pipeline.
2. **Rendering is trusted-Rust.** Portable ops are rendered by `PgEmitter`/`SqliteEmitter`, which emit only the DDL shapes they know how to emit. There is no path from an op to arbitrary SQL except `op.raw`.
   - **DML bind values use native parameter binding on BOTH legs.** The `PlanStep::Dml { template, binds }` step journals/guards only the placeholder template; the `binds` are bound via `$n` on PG / `?n` on SQLite, never string-interpolated. This holds for the net-new SQLite DML assembler as much as for PG; an interpolating assembler voids the guarantee and is forbidden.
3. **Guard coverage is per-dialect, NOT uniform — and `op.raw` differs sharply between PG and SQLite.**
   - **Postgres.** Every rendered `up` (including `op.raw({ pg })`) passes `SqlGuard::check` in `Confined` (`guard.rs`): the libpg_query parse + deny-list blocks RCE (`LOAD`, `plpython`), privilege escalation (`CREATE ROLE`, `ALTER SYSTEM`, `GRANT`), cross-tenant (foreign schema refs), file (`pg_read_file`) and network (`dblink`) functions, plus deny-by-default. Here `op.raw({ pg })` is not a bypass — it still goes through the full deny-list.
   - **SQLite.** There is no libpg_query parse guard (`guard.rs:165-172`). In `Confined` the SQLite path accepts only descriptor/IR-generated DDL, and an arbitrary raw SQLite string is refused fail-closed. `op.raw({ sqlite })` is not available to creators at all (§8.7). For IR-rendered SQLite DDL, the only line of defense is the SQLite runtime authorizer.
   - The portable-op abstraction (rendered DDL) is uniformly defended — PG by deny-list, SQLite by authorizer — but the raw escape hatch is PG-only.
4. **Least-priv role is the DB backstop.** Even if a statement slips the parse guard, it runs under `migrator_<project>` (`role.rs:197`): `NOSUPERUSER`, owns only the project schema, zero grants on foreign/meta schemas.
5. **Journal stays unforgeable & immutable.** Journal writes are admin-only; immutability triggers block UPDATE/DELETE/TRUNCATE (`journal.rs:332`). The checksum is engine-computed (§2.4), so an attacker cannot ship a `.ir.json` whose `checksum` hint lies about its ops.
6. **`owner_app` is stamped server-side, and the IR path runs its OWN ownership check (the declarative one does not cover it).**
   - **The declarative ownership enforcement does not fire on the op path.** `DeclarativeAuthor::diff`'s check is keyed on the per-table ownership `BTreeMap` from a `desired_snapshot` (`declarative.rs:283-294`, enforcement at `:904-913`). The op path lowers ops directly and never computes a `desired_snapshot` — so it inherits no ownership check for free.
   - **The IR path performs its own per-table ownership check (net-new, §6), replicating BOTH halves of the declarative check.** The declarative path enforces ownership in two places: (a) `enforce_ownership` over the union `BTreeMap` rejects a structural change to a union table the deploying app does not own (`declarative.rs:2826`); and (b) a separate fail-closed drop-ownership check for a live-only table absent from the union — an unknown owner fails closed (`declarative.rs:2820-2824`). The IR path reproduces both. Before lowering, the deploy/`IrAuthor` path: (1) overrides `owner_app` to the deploying app's id (a spoofed value is discarded); (2) for every op that targets a table (DDL and DML), looks up that table's owner in the project's ownership registry and rejects the migration if the deploying app is not the owner; (3) **a table absent from the ownership registry FAILS CLOSED** — a DML `insert`/`update`/`delete` (or DDL op) targeting a never-declared table with no registry entry is refused, exactly as the declarative drop path refuses an unknown-owner drop. Two regression tests: (i) an op targeting another app's table is refused; (ii) an op targeting a table with no registry entry is refused.
   - **Reconciling "stamped server-side" with "`owner_app` is checksummed."** `owner_app` folds into the authoritative journal checksum (`migration.rs:330`), but is excluded from the advisory `.ir.json` hint (§2.4). The server overwriting a spoofed/absent `owner_app` never changes the hint-domain checksum, so there is no "expected mismatch" case.
7. **SQLite raw is unavailable to creators (fail-closed), with a consequence for `onConflict`.** `op.raw({ sqlite })` cannot be parsed by libpg_query, so `Confined` SQLite accepts only descriptor/IR-generated DDL; arbitrary raw SQLite is refused fail-closed. An author who needs raw SQLite must be on the operator-only `Trusted`/`Platform` profile, gated by the capability token (`guard.rs` `OperatorCapability`, unreachable from the creator path). **Direct consequence (§9):** any op whose only documented SQLite lowering was "routes to `op.raw`" — notably `insert { onConflict }` — has no valid SQLite path and is a **hard authoring error on SQLite**, not a silent route to a rejected raw string.

**Net (per-dialect, honest):** `op.*` adds zero new privilege — it is a new way to request operations the same role/journal backstops vet. On PG the request is additionally deny-list-guarded (including PG raw). On SQLite there is no deny-list; the defenses are: only descriptor/IR-generated DDL is accepted, the runtime authorizer screens it, the least-priv role contains it, and creator raw SQLite is refused entirely.

### 8.8 The AI builder is the dominant author — the machine-readable feedback path

`AGENTS.md` frames zeroship as "AI builds it": the AI builder is the dominant author. Two consequences:

- **Rejections must be machine-readable.** Every authoring-time rejection the engine/validator can emit carries a structured error code + payload `{ code, op_index, ts_location, dialect, reason, suggested_fix? }`, not only a prose string. This is the canonical taxonomy of authoring-time error codes (new validators add their code here): a guard denial (§6.1.1), a non-portable expression (`EXPR_NOT_PORTABLE`, §3.3.1.1), an out-of-`i64` numeric (`EXPR_INVALID_NUMERIC`, §2.5), an interpolated `${v}` in identifier position (`EXPR_INTERPOLATED_IDENTIFIER`, §3.3.1.2), a `dialect_scope = PgOnly` against a SQLite target (`DIALECT_SCOPE_PGONLY`, §2.4.1), a SQLite `onConflict`/`op.raw({sqlite})` refusal (`RAW_SQLITE_REFUSED`, §8.7/§9), a pending-contract/dependency block (`TABLE_HAS_PENDING_CONTRACT`/`DEPENDENCY_PENDING_CONTRACT`/`ORPHANED_PENDING_CONTRACT`, §2.0.3), an unknown-owner ownership fail-closed (§8.6), a build-recorder budget overrun (`BUILD_RECORDER_BUDGET_EXCEEDED`, §8.9), a missing `.ts` provenance blob (`PROVENANCE_BLOB_MISSING`, §5.1) or a `.ts`/`.ir.json` checksum divergence at deploy (`PROVENANCE_MISMATCH`, §5.1), and a fresh-rebuild squash skipping non-replayable data steps (`SQUASH_FRESH_REBUILD_DATA_NOT_RECONSTRUCTED`, §5.4 — a status signal, not an authoring reject). The human-readable message is the projection.
- **AI-authored migrations are PG-only-by-default until a portability check passes.** An LLM will confidently emit dialect-specific SQL, the "passes on PG, mis-applies on SQLite" trap §9 fights — and there is no human to read the §6.1.1 attribution. So: (1) the portable-expression grammar validator (§3.3.1.1) runs on every `op.sql`/transform fragment and surfaces its structured reject; (2) a migration is treated as `dialect_scope = PgOnly` until it passes the portability check (renders + dry-runs clean on SQLite); (3) **the shadow-DB dry-run (§6.2) on BOTH backends is MANDATORY in the AI path** — an AI migration not dry-run on SQLite is not eligible for a `Both` `dialect_scope`. A human author may opt out of the SQLite dry-run and accept a PG-only migration knowingly; the AI loop may not.
- **Who reviews the `// TODO: backfill` markers (§7.1)?** In the human path, the author. In the AI path, the marker is a structured prompt back to the AI loop: `generate`'s scaffold emits each `// TODO` as a machine-readable open-obligation the orchestrator must resolve or explicitly defer before the migration is eligible to deploy. An AI migration with unresolved TODO obligations fails the pre-deploy gate.

### 8.9 Build-time execution is untrusted JS — the recorder runs in a constrained context

§8.1–§8.7 reason about the *emitted IR data*. They do not cover the *build-time execution* of the author's `.ts`, a distinct threat surface: the recording `op` builder evaluates untrusted creator/AI TypeScript in Node at build time (§4.1). The `.ts` is "pure" only by convention — a malicious or buggy `.ts` could `import` a package that opens a network connection, spawns a subprocess, reads the build host's filesystem, or loops/allocates without bound at build time. Resolution:

- **DECISION: the recorder runs in a dedicated OS-isolated child process with network and filesystem denied by the kernel, a CPU/wall/memory rlimit budget, and a module allow-list enforced by a custom loader — NOT a same-process Node `vm` context.** A `vm` context or userland module allow-list cannot be made airtight (a malicious `import` can touch `fs`/`process`/`child_process` before a userland allow-list bites; a native addon escapes the JS sandbox; prototype-pollution defeats in-VM guards). So the recorder runs the author's `.ts` in a fresh child process under OS-level isolation — on Linux, a **seccomp-bpf** syscall filter (default-deny; no `socket`/`connect`, no write `open`/`openat`, no `execve`/`fork`/`clone`) combined with a **landlock** filesystem ruleset (read-only access to the `node_modules` allow-list path + the migration source dir) and a **network namespace with no interfaces**. This is the build-time analogue of the apply-time least-priv `migrator_<project>` role: the kernel enforces it. (The specifics mirror the platform's existing sandbox backend — `crates/sandbox` / the Nomad+Cloud-Hypervisor path.)
- **Resource budget via rlimits + a watchdog**: `RLIMIT_CPU` + a wall-clock watchdog + `RLIMIT_AS`, so a build-time infinite loop or allocation bomb is bounded and killed, surfaced as `BUILD_RECORDER_BUDGET_EXCEEDED` (§8.8).
- **Module allow-list enforced by a custom resolver, as a second layer**: the child uses a Node loader hook that permits only `@zeroship/migrate` + type-only imports from the app schema. Defense-in-depth behind the kernel sandbox.
- **The surrounding CI sandbox is defense-in-depth, NOT the alternative.** The design forbids relying on the CI sandbox alone and forbids unconstrained Node on a trusted host. The in-recorder OS isolation is the hard precondition; the CI sandbox is the belt over those suspenders. The mechanism (OS-isolated child process, not a `vm`) is decided here; the concrete syscall/landlock ruleset is a dedicated-PR implementation deliverable (the recorder service is its own PR, **PR4a**, split from CLI ergonomics — §10).
- **Kernel baseline + degraded floor (so the host requirement is explicit).** The full ruleset is **seccomp-bpf + landlock + netns**. `landlock` requires **Linux ≥ 5.13** (and an ABI that supports filesystem rules; later ABIs add more). On the platform build image this baseline is met by construction (the image pins a recent kernel). On a host **lacking landlock** (older kernel, or landlock disabled), the recorder runs at a **degraded floor of seccomp-bpf default-deny + an interface-less network namespace + the `RLIMIT_*`/watchdog budget + the userland module-allow-list resolver** — i.e. syscall-level RCE/network containment is retained; only the kernel-enforced filesystem read-restriction (landlock) is replaced by the userland module-allow-list resolver as the fs boundary. In the **hosted multi-tenant path**, the recorder refuses to run with neither seccomp nor netns available (no silent unconstrained-Node fallback); because the hosted recorder always runs in the platform image (§8.9.2), it always has the full ruleset. In the **local single-tenant/self-host path** (§8.9.2), the recorder runs the developer's own `.ts` at the developer's own trust level under the userland-budget floor (module-allow-list resolver + `RLIMIT_*`/watchdog), opportunistically adding seccomp/landlock/netns when the local kernel supports them — the kernel sandbox is hardening, not a precondition, for one's own code on one's own machine.
- **Why the engine is still unaffected.** Even in the worst case (build-time RCE inside the recorder's child), the engine's apply path is untouched: it ingests only the JSON IR, fully re-validated/guarded/role-limited; the recorder's child can neither reach the network nor write files outside the migration dir.

#### 8.9.2 The recorder is server-side for the HOSTED multi-tenant path; LOCAL recording is permitted for the single-tenant/self-host case

The threat the §8.9 kernel sandbox contains is **untrusted JS at build time** — and "untrusted" is a property of *whose* `.ts` is being recorded *on whose host*, not of `.ts` recording in the abstract. Two cases, two postures:

- **The trust boundary is multi-tenancy, not `.ts`-recording-per-se.** A solo developer (or self-host operator) recording **their own** migration `.ts` on **their own** laptop is running code at the **same trust level as their own application code** — which they already run unsandboxed in `pnpm dev` (the dev-tier `env.db`→SQLite / dev-auth provider all execute the developer's own code locally). OS-isolating a developer's own `.ts` on their own machine defends against a threat that does not exist in the single-tenant case: there is no other tenant to protect, and the developer already has full control of the host. The hosted multi-tenant builder is different: there, the platform records **one creator's / the AI builder's** `.ts` on **shared platform infrastructure**, alongside other tenants — exactly the multi-tenant trust boundary the worker's one-isolate-per-app and the apply-time `migrator_<project>` role enforce. The kernel sandbox is **mandatory there** and **optional locally**, for the same reason the worker sandbox is mandatory in prod but the dev tier runs the creator's code directly.
- **HOSTED multi-tenant path — recorder runs server-side under the kernel sandbox (mandatory).** When a creator or the AI builder records through the platform, the recording of `.ts`→`.ir.json` happens server-side in the canonical platform build image (the same Linux image §8.9.1 pins), under the full seccomp+landlock+netns ruleset, one fresh sandboxed child per invocation (§8.9). The local CLI is a thin client: it scaffolds + edits + lints the `.ts` locally (all pure analysis), ships the `.ts` to the recorder service, and commits the returned `.ir.json` (+ structured-error payload if rejected). This is the path that protects co-tenants and removes cross-toolchain JCS divergence for the shared builder.
- **LOCAL single-tenant / self-host path — recording runs locally (permitted, first-class).** A developer running fully offline (`pnpm dev`, self-host) records their own `.ts`→`.ir.json` **on their own machine**, with no network round-trip and no container required. Because it is the developer's own code at their own trust level, the recorder still applies the **userland defense-in-depth** (the module-allow-list resolver + the `RLIMIT_*`/wall-clock watchdog budget of §8.9) to catch *accidental* runaway builds, but does **not** require the kernel sandbox. On Linux the recorder opportunistically applies seccomp+landlock+netns if available (free hardening); on macOS / a non-landlock kernel it runs under the userland-budget floor. This keeps the offline/self-host inner `generate → record → validate` cycle a pure-local loop, consistent with the project's first-class offline stance.
- **An operator MAY pin local recording to the hosted/containerized recorder.** A self-host operator who wants the kernel-sandbox guarantee locally (e.g. they record `.ts` authored by an untrusted third party) runs the canonical platform build image as a local container (Docker/Podman provide a Linux VM on macOS, so the kernel features are present). This is an opt-in for the operator who has a multi-tenant-like threat locally — not the default for a solo developer recording their own migration.
- **Determinism across paths.** The first-committed `.ir.json` bytes are canonicalized identically regardless of path: the typed-value checksum (§4.3 mechanism 4) is invariant under JCS-byte differences, so a locally-recorded `.ir.json` and a server-recorded one for the same ops carry the same authoritative checksum. The §8.9.1 CI gate regenerates each not-yet-applied `.ir.json` through the canonical image and asserts the typed-value checksum matches — catching any divergence whether the first emission was local or hosted.

**The recorder-service contract.**
- **API.** `POST /v1/recorder/record` taking `{ ts_source, app_id, schema_types_blob }`, returning `{ ir_json, ts_provenance_blob, checksum }` on success or the §8.8 structured-error payload on rejection. Stateless per invocation. The thin-client commits the response's `.ir.json` + provenance blob (powering the §5.1 mandatory provenance gate).
- **Auth/transport.** Authenticated with the same creator PAT / `ZEROSHIP_TOKEN` the deploy path uses, over TLS; the `app_id` is cross-checked against the token's owned apps server-side (the §8.6 ownership check applied at record time).
- **Per-invocation tenant isolation.** Each `record` call spins a fresh seccomp+landlock+netns child (§8.9) destroyed after the single recording — no pooling across tenants, no reuse across invocations. The build-time analogue of the worker's one-isolate-per-app invariant.
- **Resource fairness / DoS.** Each invocation carries the §8.9 budget; the service enforces per-token concurrency + rate limits and a global concurrency cap with a queue so a burst degrades to latency.
- **Offline / failure behavior.** If the hosted recorder is unreachable, the supported path is local recording (§8.9.2 — the local userland-budget recorder for the single-tenant/self-host case, or the local-container recorder for an operator wanting the kernel sandbox); the AI loop's orchestrator treats recorder-unreachable as retry/fallback-to-local, not a build failure. (This contract section describes the HOSTED multi-tenant service; local recording is governed by §8.9.2.)
- **AI-loop latency.** The recorder adds one network round-trip + one sandboxed `.ts` evaluation to the inner `generate → record → validate` cycle. The expensive part the AI loop iterates on most — the portable-expression validator + typecheck/lint — runs **locally** (§8.9.2), so the common rejection is caught locally without the recorder hop; the recorder hop is needed only to produce the committed `.ir.json` once the fragment passes local analysis. (No millisecond figure is asserted — unmeasured.)

#### 8.9.1 First-commit canonicalization: one canonical toolchain

The build-once contract (§5.1) defeats post-commit non-determinism but leaves the first emission window. Resolution:
- **Canonicalization is anchored on the typed-value checksum, not on a single emitting toolchain.** Because the authoritative drift anchor is the typed-value checksum — invariant under any JCS-byte difference between two conformant serializers (§4.3 mechanism 4) — a first-commit `.ir.json` is canonical regardless of whether it was emitted by the hosted recorder, a local container, or a local single-tenant recorder (§8.9.2). The value-equality gate (§2.5) makes "which toolchain emitted the bytes" irrelevant to correctness.
- **CI gate (the toolchain anchor for any path):** CI regenerates each not-yet-applied `.ir.json` through the canonical platform build image and asserts the typed-value checksum matches (and, as a non-blocking canary, raw-byte equality — §2.5). This catches a divergence whether the first emission happened locally or on the hosted recorder, so a locally-recorded artifact is held to the same canonical bytes without forbidding local recording.
- **The re-record runs in the SAME sandbox as the first emission, so any value the §4.3 lint permits is reproducible BY CONSTRUCTION — the exact-match gate and the lint do not disagree on a legal artifact.** The §4.3 determinism lint permits a build-time-computed value (a `slugify(...)`, a config-derived constant) on a non-time/uuid column. For the re-record exact-match to be sound, that computation must reproduce identically when CI re-runs the `.ts`. The gate guarantees this by running the re-record under the **same kernel sandbox the §8.9 recorder uses** — no network, no filesystem reads outside the migration dir / module-allow-list, and a **fixed clock** — i.e. the `.ts` has no admitted input that can vary between the first emission and the CI re-record. A value derived from `process.env`, a file read, or the wall clock is unreachable under the sandbox, so it cannot be the source of a re-record divergence; a value derived purely from the committed source + the type-only schema blob is deterministic. So the lint (which permits computed non-time/uuid values) and the CI gate (exact-match) are reconciled: every value the lint permits is, under the sandbox, reproducible. (A time/uuid value, the one genuinely non-reproducible class, is steered to apply-time `op.sql\`now()\``/`gen_random_uuid()\`` by the §4.3 scaffold default and flagged by the lint, so it never enters the recorded `.ir.json` as a frozen literal in the first place.)

---

## 9. DML portability boundary (honest)

Portability for DML is real but bounded. The principle (Alembic/Kysely): **engine owns control flow & statement assembly; author owns the data transform expression.** Where the transform expression is itself dialect-divergent, the author reaches for `op.raw` or `op.sql` with dialect-aware bodies.

**Portable (works on both PG and SQLite from one op):**
- `insert` with literal rows — engine assembles `INSERT … VALUES …` for both dialects (DML assembler, PR6a).
- `delete` with a `where` — `DELETE FROM … WHERE …` is identical.
- One-shot `update` whose `set`/`filter` use only the **closed portable expression grammar of §3.3.1**: column refs, literals, arithmetic, comparison/boolean operators, `CASE`, the allow-listed provably-identical scalar functions (`coalesce`, `nullif`, `lower`, `upper`, `trim`, `length`, `abs`, `cast(... as <portable type>)`, `||` concat), **and the engine-synthesized `op.fn.splitPart` helper within its pinned envelope** (single-ASCII delimiter + positive literal `1 ≤ n ≤ 8`). `op.fn.replace` is PG-only until `replace` is allow-listed; `op.fn.substr` is **deferred** (portability not yet proven, §9). Raw `substr`/`replace`/`instr`/`split_part` *strings* are excluded (cross-dialect semantics diverge, §3.3.1) — use the helper. A fragment outside this grammar, OR a helper call outside its envelope, is a hard error on SQLite (and PG-only via `op.raw({pg})`/`op.sql`).
- `backfill` / `update { batch }` — **portable on BOTH backends** (PG via the existing executor `backfill.rs:286`; SQLite via the committed §2.3.1 executor, PR6b).

**Engine-SYNTHESIZED portable transform helpers — pinned, bounded, and PROVEN with an exhibited expression.** The plain portable allow-list excludes raw `split_part`/`substr`/`replace` because their raw cross-dialect semantics diverge. A small set of high-level, engine-synthesized helpers recovers portability for the *common* transforms — but only inside an explicitly bounded, in-doc-pinned envelope. We commit to the exact, exhibited SQLite expression, the precise input envelope it is proven over, and a fail-closed error for everything outside it.

- **Why a `substr`-only (no `instr`) lowering is infeasible.** `substr(s, start, len)` takes numeric `start`/`len`; locating the k-th occurrence of a delimiter is a **position search**, which `substr` cannot perform. A fixed-depth expression can only test a bounded set of positions `1..D`; for a value whose k-th delimiter sits past `D`, the chain never finds it. **Verified against SQLite 3.51.2:** `substr('aaaaaaaaaa,b', 5, 1)` is `'a'` and `substr(...,11,1)` is `','` — to *find* the `','` a pure-`substr` expression would need an unbounded `CASE` over every position `1..length(s)`. The only pure-builtin ways to locate a delimiter position are (i) `instr` (a position primitive) or (ii) the count trick `length(s) - length(replace(s, d, ''))` (needs `replace`). A `substr`-only `split_part` is impossible.
  - **The other constraint:** SQLite `substr` is byte-or-character-based depending on the value's storage class/encoding, with 1-based and negative-from-end semantics — so any split must avoid relying on character-vs-byte semantics. The single-ASCII-delimiter envelope makes the byte-wise scan provably safe over arbitrary UTF-8 *values*.
- **DECISION: `instr` is added to the SQLite authorizer allow-list as an explicit, separately-reviewed security change, and the helper is lowered on top of `instr`.** `instr` is a deterministic, sandboxed, side-effect-free builtin (no extension load, no fs/network, no tenant escape — the same property `substr`/`length`/`coalesce` already on the allow-list have), so adding it does not widen the capability surface that matters to the threat model (the authorizer is a capability gate, and `instr` grants no new capability — it is a pure scalar over data the role can already read). Adding `instr` to `FUNCTION_ALLOWLIST` (`backend_sqlite/authorizer.rs:131`) is its own reviewed line item in PR6b, kept in lockstep with the emitter's function set per the authorizer's standing "MUST be kept in lockstep" rule. `replace` is not added; the allow-list grows by exactly one deterministic scalar.
- **The helper is PROVEN-PORTABLE only for the ASCII-single-character-delimiter case AND a BOUNDED literal `n`, and HARD-ERRORS otherwise.** `op.fn.splitPart(col, delim, n)` is admitted as portable **iff**: `delim` is a literal single ASCII character (one byte, code point < 0x80), `n` is a literal positive integer with `1 ≤ n ≤ 8`, and `col` is a column ref or a portable fragment. Any other shape — a multi-character delimiter, a non-literal/runtime delimiter, an empty delimiter, `n = 0`, negative `n`, `n > 8`, or a non-ASCII delimiter — is a **hard build/render error on the SQLite leg** (`EXPR_NOT_PORTABLE`, §8.8). **The fallback is unambiguous: mark the migration PG-only.** Because creators have no raw SQLite escape (`op.raw({sqlite})` is refused fail-closed, §8.7), there is **no SQLite tested-expression option** for an out-of-envelope split — the only real fallbacks are (i) on PG, a dialect-aware `op.raw({pg})` / `op.sql` using `split_part` (which makes the migration `dialect_scope = PgOnly`), or (ii) restructure to stay in-envelope (e.g. split into ≤8 parts). The structured `suggested_fix` therefore names exactly these two, and does **not** imply a SQLite raw escape exists. The value being split *may* contain multibyte content (the proof covers UTF-8 *values*); it is the **delimiter** that is constrained to single-ASCII, because an ASCII byte cannot occur inside a UTF-8 multibyte sequence.
  - **Why `n` is bounded at 8 (the depth-vs-limit reasoning).** The exhibited SQLite unroll references the running sub-expression `curᵢ₋₁` **twice** at each level (`substr(curᵢ₋₁, …)` and `instr(curᵢ₋₁, …)`). Inlining doubles the expression size each level — **O(2ⁿ) bytes/AST nodes, not O(n)**. Measured against **SQLite 3.51.2**: ~17 KB at `n=8`, ~373 KB at `n=12`, ~1.5 MB at `n=14`, exceeding parse limits beyond. An unbounded literal `n` would build a deeply-nested expression that can exceed `SQLITE_LIMIT_EXPR_DEPTH` (default 1000) / `SQLITE_LIMIT_SQL_LENGTH` and **fail at apply on SQLite while the PG `split_part(col,'d',n)` succeeds** — the silent dialect divergence the envelope prevents. So `n` is capped at 8 (the highest part index measured to compile comfortably as the simple inline form, ~17 KB), and `n > 8` hard-errors on the SQLite leg. A required boundary fixture asserts `n=8` applies identically on PG and SQLite and `n=9` hard-errors on the SQLite leg.
  - **Raising the bound is a deferred, scoped follow-up via a CSE-shared form.** A common-sub-expression-shared lowering (a `WITH` iterative CTE, or a chain of single-reference assignments) is O(n) in size and could support a larger `n`. That form is deferred (it requires the engine to emit a statement-level CTE around the transform, which interacts with the §3.3.1 "engine assembles the statement shell, author supplies only the fragment" boundary). Until it lands, the simple inline form with `n ≤ 8` is the contract.
  - **The `n > 8` cliff is surfaced at authoring time, consistently with the cross-table gap (§9 cross-table marker).** The `n > 8` `EXPR_NOT_PORTABLE` (§8.8) carries a structured `suggested_fix` naming the **two real fallbacks — there is no SQLite raw option** (§8.7): `{ pg: "op.raw({pg}) using split_part(col,'d',n) — makes the migration PG-only", restructure: "split into ≤8 parts to stay in-envelope", note: "SQLite has no creator raw escape, so an out-of-envelope split is NOT authorable on the SQLite leg; mark the migration PG-only" }` — and `generate`/`new` emit the same `// TODO: split with n>8 — exceeds the portable op.fn.splitPart bound (n≤8); PG-only via op.raw({pg}), or restructure to ≤8 parts (no SQLite raw escape)` structured marker when scaffolding a high-n split, exactly mirroring the cross-table `// TODO` marker (§9). So the AI loop is told about the high-n cliff — and that the only SQLite path is mark-PG-only — at authoring time through the same channel as the cross-table gap, not only at render-time rejection.
- **The exact PG and SQLite expressions are EXHIBITED in-doc and PROVEN against real SQLite 3.51.2.** For `op.fn.splitPart(col, d, n)` with a single-ASCII `d` and positive literal `n`, the engine appends a sentinel delimiter (`s' = col || d`) so every token (including the last) is delimiter-terminated, then unrolls the boundary walk to literal depth `n`. The n-th part is the substring between the (n−1)-th and n-th delimiter in `s'`:
  - **PG:** `split_part(col, 'd', n)` — verbatim, returns `''` when the part index exceeds the token count.
  - **SQLite (allow-listed builtins `substr`/`length`/`instr`/`||` — `instr` newly allow-listed):** unroll with `cur₀ = col || 'd'`, `curᵢ = substr(curᵢ₋₁, instr(curᵢ₋₁, 'd') + 1)` for `i = 1 … n−1`, and the result is `substr(cur_{n-1}, 1, instr(cur_{n-1}, 'd') − 1)`. Exhibited for the first three `n`:
    ```sql
    -- n = 1
    substr(col || 'd', 1, instr(col || 'd', 'd') - 1)
    -- n = 2:   r1 = substr(col||'d', instr(col||'d','d')+1)
    substr( substr(col||'d', instr(col||'d','d')+1),
            1, instr( substr(col||'d', instr(col||'d','d')+1), 'd') - 1 )
    -- n = 3:   r1 as above; r2 = substr(r1, instr(r1,'d')+1); part = substr(r2,1,instr(r2,'d')-1)
    ```
    Because `'d'` is a single ASCII byte and UTF-8 never embeds an ASCII byte inside a multibyte sequence, the byte-wise `instr` scan finds exactly the same boundaries a PG character-wise `split_part` does — the structural reason the equivalence holds for single-ASCII delimiters and fails for multi-byte/multi-char delimiters. The engine owns and unit-pins this expression; the author never sees `instr`/`substr`. The inline unroll references `curᵢ₋₁` twice per level, so it grows O(2ⁿ) (~17 KB at `n=8`), which is why `n` is capped at 8.
  - **Proof run (SQLite 3.51.2, the exhibited expressions, compared to PG `split_part`):** `split('a,b,c',',',1)='a'`, `…,2)='b'`, `…,3)='c'`, `…,4)=''`; `split('a',',',2)=''`; `split('',',',1)=''`; `split('a,',',',2)=''`; `split(',b',',',1)=''` and `…,2)='b'`; `split('a,,b',',',2)=''` and `…,3)='b'`; `split('héllo,wörld',',',1)='héllo'`, `…,2)='wörld'` (multibyte value, ASCII delim — no corruption); `split('first last',' ',1)='first'`, `…,2)='last'` (the §1.1 hero space-split); NULL input → NULL. Every case matches PG `split_part`.
  - **`op.fn.replace` is PG-only until `replace` is reviewed onto the allow-list.** `op.fn.replace` lowers to PG/SQLite `replace`; SQLite `replace` is on the allow-list's candidate set under the same review — until that review lands `op.fn.replace` is PG-only.
  - **`op.fn.substr` is DEFERRED — NOT shipped in PR6b.** Unlike `op.fn.splitPart`, `op.fn.substr` is **not** exhibited with a byte-identical proof corpus against real SQLite 3.51.2 in this spec, and §3.3.1 explicitly names raw `substr`/`substring` as a silent-divergence trap (negative-start behavior, start-beyond-length, 2-arg-vs-3-arg). Shipping a portability claim for the exact function flagged as a divergence trap without an exhibited proof would be exactly the "passes on PG, mis-applies on SQLite" failure the design fights. So `op.fn.substr` is **deferred to a follow-up PR** whose scope is to exhibit the precise PG (`substring(col from start for len)`) and SQLite (`substr(col, start, len)`) expressions and PROVE them byte-identical over the positive-literal-`start`/`len` envelope — including the start-beyond-length and length-overrun edges — against real SQLite 3.51.2, mirroring the §9 `splitPart` proof corpus. **PR6b ships ONLY `op.fn.splitPart`** (proven below). Until the substr proof lands, `op.fn.substr` is not admitted; the `op.fn.substr` entry in the §3.2 interface is marked deferred, and a positional substring transform is PG-only (via `op.raw({pg})` / dialect-aware `op.sql`), or restructured.
- **REQUIRED edge-matrix fixtures (PR6b).** The corpus MUST assert `op.fn.splitPart` produces byte-identical results on PG and SQLite for the full envelope AND that every out-of-envelope shape hard-errors identically on both legs:
  - **In-envelope, asserted IDENTICAL on PG and SQLite:** (a) two-token ASCII value; (b) single-token value with `n=2` → `''`; (c) empty-string input → `''`; (d) trailing-delimiter (`"a,"`, `n=2`) → `''`; (e) leading-delimiter (`",b"`, `n=1`) → `''`; (f) consecutive delimiters (`"a,,b"`, `n=2`) → `''`; (g) NULL input → NULL; (h) **multibyte/UTF-8 VALUE with an ASCII delimiter** (`"héllo,wörld"` split on `,`) → identical tokens; (i) value containing the delimiter byte adjacent to multibyte content.
  - **Out-of-envelope, asserted to HARD-ERROR (`EXPR_NOT_PORTABLE`) on the SQLite leg on BOTH the Rust validator and the JS lint:** (j) multi-character delimiter (`", "`); (k) non-ASCII delimiter (`"·"`, U+00B7); (l) empty delimiter; (m) `n = 0`; (n) negative `n`; (o) non-literal/runtime delimiter or `n`; (p) `n = 9` (just past the bound) — paired with an in-envelope `n = 8` fixture asserting the boundary applies byte-identically.
  - **Collation:** the helper performs no collation-dependent comparison (it splits on a literal byte), asserted by a fixture with a value under a non-default collation producing the same split.
- **Why helpers, not allow-listed raw functions.** A raw `substr(...)`/`split_part(...)`/`instr(...)` in an `op.sql` body is excluded (the author would rely on un-pinned dialect semantics). Adding `instr` to the SQLite allow-list makes it available to the **engine's pinned lowering**, not to author-written raw bodies: a creator `op.sql\`instr(...)\`` is still rejected by the expression-grammar validator (the two lists are distinct — the authorizer gates which builtins the migrator role may execute; the grammar gates which functions an author may name). The helper is safe **only because the engine pins the exact expression and the envelope**, proven with the required fixtures. So the §1.1 hero example written as `op.fn.splitPart(name, ' ', 1)` is portable; written with a multi-char delimiter or as a raw `op.sql\`split_part(...)\`` it is PG-only / a hard SQLite error.

**NOT portable — author must supply dialect-specific SQL (PG via `op.raw({ pg })` / `op.sql`; SQLite has no creator raw):**
- **Dialect-only functions** (anything outside the §3.3.1 allow-list, as a raw `op.sql` fragment): a raw `split_part` (PG-only — use the `op.fn.splitPart` helper instead), `json_agg`, `array_agg`, `jsonb_*`, `regexp_replace`, SQLite `json_extract` vs PG `->>`. Also excluded as raw functions despite existing on both dialects: `substr`/`substring` and `replace` (argument/affinity semantics diverge). For the common split case the `op.fn.splitPart` helper is the portable path within its envelope; `op.fn.replace` is PG-only until `replace` is allow-listed, and `op.fn.substr` is deferred until its byte-identity is exhibited + proven (§9).
- **Window functions in UPDATE** (`ROW_NUMBER() OVER …`) — syntax/support differs.
- **Generated columns**, **vendor extension DML** (pgvector ops, PostGIS functions) — Postgres-only.

**Upsert / `insert { onConflict }` — the "routes to `op.raw`" escape is a dead end on SQLite.** PG `INSERT … ON CONFLICT … DO UPDATE` and SQLite `INSERT … ON CONFLICT`/`INSERT OR REPLACE` have incompatible clauses. Because `op.raw({ sqlite })` is refused fail-closed (§8.7):
- **PG**: `insert { onConflict }` renders natively (engine-assembled `ON CONFLICT … DO UPDATE`).
- **SQLite**: a **hard authoring error** at build/render — "`onConflict` is not supported on SQLite; restructure as separate insert + update, or guard the migration to PG-only." We do not silently route it to a rejected raw string and do not silently drop the conflict clause. (A future engine-authored, descriptor-derived SQLite UPSERT is out of scope, explicitly deferred.)

**Portable-DML coverage — the honest sum.** The portable-DML surface is **DDL (all of it) + literal `insert`/`delete` + `update`/`backfill` whose `set`/`filter` fit the closed grammar + the `op.fn.splitPart` helper within its single-ASCII envelope — all scoped to a SINGLE table (the enclosing step's target).** What it does *not* cover, which real apps routinely need: **JSON reshaping** (`json_extract`/`->>`/`jsonb_*`), **date arithmetic**, **regex**, **array ops**, **window functions**, **generated columns**, any vendor extension, **and any CROSS-TABLE / correlated transform**. The grammar covers most *single-table structural/conditional* transforms (rename-via-copy, `CASE` bucketing, NULL-coalescing, casing/trimming, the common space-delimited split); a substantial fraction of non-trivial real-world data transforms (JSON, dates, regex, OR another table) is PG-only. (No precise percentage is claimed — unmeasured.)
- **Cross-table / correlated transforms are an explicit portability exclusion — and a hard SQLite limit.** The validator's `ColRef` resolution is scoped to the single target table (§3.3.1.1): a transform body cannot reference another table's columns. A common data migration — derive a column from a JOINed/correlated other table (e.g. backfill `orders.customer_name` from `customers` via `customer_id`) — is not expressible in the portable grammar.
  - **On PG:** a cross-table backfill is `op.raw({ pg })` with `UPDATE … FROM other_table …` — which makes that migration `dialect_scope = PgOnly`.
  - **On SQLite:** no creator raw escape (§8.7), so a cross-table backfill is a hard authoring error on the SQLite leg.
  - **Why not a portable cross-table grammar now.** A controlled `FROM`/join facet is the strongest fix but materially enlarges the grammar (join resolution, multi-table `ColRef` scoping that re-opens the cross-table-reference surface, a SQLite lowering of `UPDATE … FROM`). It is **deferred to a scoped follow-up**, recorded as a known capability gap. Until it lands, `generate`/`new` scaffold a `// TODO: cross-table backfill — PG-only via op.raw until the join facet ships` structured marker (§8.8) so the AI loop is told at authoring time.
- **The dev-tier (SQLite) consequence, stated plainly.** Because creators have no raw SQLite escape (§8.7), a PG-only transform is not authorable on the SQLite dev tier at all — a hard authoring error there. An app whose data migration needs JSON/date/regex/cross-table reshaping cannot run that migration on SQLite dev; the guidance is to run those migrations against a PG dev database (the engine supports a PG dev target), not the SQLite dev tier.
- **Considered and DEFERRED: a vetted SQLite creator-expression path.** The strongest fix would be an engine-screened SQLite expression authorizer (the analogue of the libpg_query deny-list, for SQLite expressions). Deferred (it requires a second SQLite-specific validator with its own correctness burden, the very thing §3.3.1.1 avoided); the safe interim is the hard-error.

**Design stance**: the DSL does not pretend the non-portable cases are portable. `insert`/`update`/`delete` accept a portable subset; `backfill` is portable on both backends once §2.3.1 ships (committed, PR6b); anything beyond is a typed `op.sql` fragment (guard-inspected on PG; authorizer-screened on SQLite) or, on PG only, an explicit `op.raw({ pg })`. SQLite has no creator raw escape — a SQLite-only need the portable ops cannot express is a hard authoring error, not a silent leak. Honesty here prevents the worst failure mode: a migration that passes on PG and corrupts/no-ops/silently-rejects on SQLite.

---

## 10. Phased implementation plan (concrete PRs)

Each PR is independently testable with **faithful e2e on real Postgres (`:5440`) + SQLite** (`feedback_faithful_e2e_tests.md`): tests load a real `.ir.json`, render through the real emitters, guard, provision a real migrator role, apply against a real DB, and assert the journal + schema state. No shims, no PG-gated skips that hide the SQLite leg.

> Discipline (per memory): commit-only, **never push**; every fix/feature ships with a regression test that would fail pre-change; dual-review each PR.

**Two committed epics. Portable bi-dialect DML is committed, not demand-gated.**

```
EPIC 1 — the IR front door + DDL + online + the AI feedback loop + a PG data path
PR0  (the ONE shared apply_plan + AppliedPlan/PlanStep/RenameStep; RE-POINT
      apply_declarative_locked onto apply_plan via the shape-adapter; loader→Vec<AppliedPlan>
      behind single_step(); the ~507 declarative tests stay green + frozen-trace oracle)  ← CORE convergence
  ├─▶ PR1  (IR JSON contract + loader IR branch + IrAuthor DDL-only + anti-drift gate
  │         + the portable-expression validator + structured-error envelope + skeletal JS)
  │     ├─▶ PR2  (online: renameColumn → OnlineRename, PG expand-contract + SQLite rebuild bridge
  │     │         + dual-execution dispatch — runs on PR0's apply_plan)
  │     ├─▶ PR3  (full @zeroship/migrate JS op builder)
  │     │     └─▶ PR4a (kernel-sandboxed recorder service — its OWN PR; seccomp+landlock+netns
  │     │     │         + recorder-service contract + faithful kernel-level e2e)  ← riskiest deliverable, split out
  │     │     │     └─▶ PR4  (build/dev execution + `new` + `generate`; wires in PR4a's recorder)
  │     │     │           └─▶ PR5  (structural type-safety: op shapes + ColType lexicon from @zeroship/db
  │     │     │                     — names stay strings, §3.3)
  │     └─▶ PR6a (DML: insert/update/delete + ONE-SHOT update on BOTH backends + PG batched backfill;
  │             builds the shared SQLite-DML-assembly module reused by PR6b;
  │             wire the validator into the DML path)

EPIC 2 — portable bi-dialect DML (COMMITTED; the §1.1 headline)
  └─▶ PR6b (SQLite BATCHED-backfill executor §2.3.1 on PR6a's shared SQLite-DML module
        + instr allow-list + portable op.fn.splitPart SQLite lowering)
        └─▶ PR7  (CLI rewire + raw-SQL demotion)   GATED ON: PR6b (SQLite DML) AND PR2 (online)
              └─▶ PR8  (docs)
```

Reading the plan: **PR0 builds the single shared `apply_plan` AND re-points the declarative path onto it via the shape-adapter (§6.0) — there is ONE orchestrator from PR0, no fork.** The regression safety net is the existing ~507-test declarative suite staying green through `apply_plan`, plus the frozen-commit golden-trace oracle (captured before the re-point) and the crash-injection property fuzz for net-new topologies. PR1 freezes the IR shape, the anti-drift gate, and — because the AI builder is the dominant author — the **portable-expression validator + structured-error envelope** (the AI loop's primary feedback signal; pure analysis, no executor dependency). PR2 (online) and PR6a (DML) are siblings off PR1, each consuming `apply_plan`. The JS/build/type-safety chain (PR3→PR4a→PR4→PR5) is a separate branch off PR1, with the **kernel-sandboxed recorder service carved out as its own PR4a** (the riskiest single deliverable — OS-level isolation of untrusted JS — split from the `new`/`generate` CLI ergonomics in PR4, with a faithful kernel-level e2e). **Epic 2's PR6b (the SQLite backfill/DML executor) is committed scope — the bi-dialect "one script, both backends, DDL+DML" headline — not a demand-gated maybe.** PR7 (the raw-SQL demotion) is gated on PR6b + PR2 so `op.*` is a credible full bi-dialect replacement before raw SQL is demoted.

### PR 0 — The single shared `apply_plan` executor + `AppliedPlan` shape + re-point the declarative path (lands FIRST)

This is an explicit, separately-reviewed deliverable, before any online or backfill op, because every later PR depends on it (§2.0). It is engine work, not a shape decision: a generic ordered-plan executor does not exist — the interleave/journal/`pending_contract` logic lives only inside `apply_declarative_locked` (`engine.rs:446`), welded to the declarative shape, while `apply_inner` (`:628`) flattens to a `Vec<Migration>` with no backfill/online.

- Define `AppliedPlan { version, name, steps: Vec<PlanStep>, checksum, flags, dialect_scope, rollbackable, owner_app, depends_on, supersedes, preconditions }`, `PlanStep = Ddl(Migration) | Dml{template,binds} | Backfill(BackfillSpec) | OnlineRename(RenameStep)`, and `RenameStep = PgExpandContract(ExpandContractPlan) | SqliteRebuild(declarative::SqliteRebuild)` (§2.0/§2.6.2). These reuse the existing `Migration`/`BackfillSpec`/`ExpandContractPlan` types and **wrap the existing `declarative::SqliteRebuild` (`declarative.rs:2053`, already `{migration, spec}`)** — no new rebuild struct, no change to `Migration`.
- **Build the single shared `apply_plan(Vec<PlanStep>)` by lifting the interleave/journal/pending orchestration out of `apply_declarative_locked`**, invoking the same `run_online`→`run_expand_pg` (`engine.rs:537`/`expand_contract.rs:693`), the `pending_contract` discipline (`engine.rs:521`/`:550`), per-`Migration` `apply_with_lock_backend` (`:661`), and `run_backfill` as execution destinations.
- **`apply_plan` dispatches `OnlineRename` to TWO unrelated execution interfaces by `RenameStep` variant (§2.6.2).** `PgExpandContract` ⇒ `run_online` (PgOnline, `expand_contract.rs:760`) with the `pending_contract` partition; `SqliteRebuild(declarative::SqliteRebuild)` (reusing `declarative.rs:2053`) ⇒ `MigrationBackend::rebuild_one` (`backend_sqlite/mod.rs:550`) — re-expressing the `plan.rebuilds` loop (`engine.rs:491-503`, incl. its destructive/approval gate `:468` and net-applied skip).
- **RE-POINT `apply_declarative_locked` onto `apply_plan` via the thin shape-adapter (§6.0) — REQUIRED CORE scope, not deferred.** The declarative path becomes a producer of `Vec<PlanStep>` (its `items`/`renames`/`rebuilds` → the corresponding steps) feeding the single orchestrator. After PR0 there is exactly one orchestrator. The regression net: the existing ~507-test declarative suite stays green through `apply_plan`, plus the frozen-commit golden-trace oracle below.
- **Frozen-commit golden trace as an immutable oracle, captured BEFORE the re-point**, covering every distinct path `apply_declarative_locked` can take (§6.0): (a) multi-deploy online PG rename; (b) SQLite-rebuild rename; (c) destructive-requires-approval refusal; (d) net-applied-skip idempotent re-run; (e) crash-resume at each phase boundary; (f) mixed rebuild + rename + plain deploy; (g) empty-renames SQLite fail-closed assertion; (h) `LockMode::AlreadyHeld` re-entrancy. PR0 asserts the re-pointed path reproduces each pre-re-point trace byte-for-byte.
- **Generalize `load_dir` to `Vec<AppliedPlan>` and fix every caller (§5.2 blast-radius table)**: `deploy_migrate.rs:133`, the dbmate CLI, `platform_runner.rs` (the 7 `load_dir` sites — one each in `run_migrate_sqlite`/`run_migrate_pg`/`run_status`/`run_validate_pg`/`run_validate_sqlite`/`run_rollback_pg`/`run_rollback_sqlite`, resolved by enclosing fn — via the `single_step()` facade), the `lib.rs` `loader` re-export (`:144-146`), the ~20 loader/deploy tests. **Add the three new symbols `AppliedPlan`/`PlanStep`/`RenameStep` to the `lib.rs` public surface; leave the existing dry-run `MigrationPlan` re-export (`lib.rs:113`) untouched** — they are distinct symbols, no rename of the dry-run type. Confirm the public CLI's `status`/`rollback`/`apply` output is byte-identical for the existing `.sql` path.
- **Reproduce the cross-deploy `pending_contract` partition in `apply_plan`** (§2.0.2): deploy-N applies plain+EXPAND (backfill runs, E3 journals only after backfill) and surfaces C1/C2 as pending; deploy-N+1 applies the pending contract under `Approval::Approved` (C2 destructive). The `SqliteRebuild` arm has no such partition.
- Sub-step versioning (`uuidv7_derive(version, step_index)`), per-step + plan-group journaling, and the "satisfied iff all steps satisfied" logic (§2.0.1).
- **Tests:** (1) a single-step (pure-DDL) plan applies on real PG identically to today's single-`Migration` path; (2) a hand-built two-step plan (DDL + trivial backfill) applies in order on PG with correct per-step journaling and crash-resume; (3) **the ~507 existing declarative tests stay green when routed through `apply_plan`** (the regression safety net); (4) the multi-deploy pending-contract partition test on real PG (first-principles assertion of the §2.0.2 semantics); (5) the SQLite `OnlineRename` dual-execution test on real SQLite via `rebuild_one` (a seeded row survives, the old column is gone, the journal records the rebuild migration's version, no `run_online` path taken); (6) CLI byte-identity over a `.sql`-only directory before/after the `Vec<AppliedPlan>` generalization; (7) **Platform Flyway-mode differential (mandatory)**: the full existing platform changelog (Task #54) applied via the pre-change `Vec<Migration>` loader and the post-change `Vec<AppliedPlan>` loader → byte-identical applied schema + journal; (7a) **single-step-shape precondition (the `single_step()` facade)**: every `.sql` in platform Flyway-mode (the full platform changelog + a property test over generated Flyway-mode `.sql`) lowers to an `AppliedPlan` with `steps == [Ddl(_)]`, proving `single_step()`'s `Err(NotSingleStep)` arm is unreachable on the platform path; (8) the frozen-golden-trace oracle for paths (a)–(h) reproduced by the re-pointed `apply_declarative_locked`-via-`apply_plan` (§6.0). Net-new-topology tests, with crash-injection property fuzz: (9) a standalone `PlanStep::Dml` step (hand-built via a trusted constructor — **not** the creator-DML assembler, which is net-new in PR6a; the authoring-path e2e for `Dml` is a PR6a test) applies on real PG, the bound row present, the parameterized template journaled, re-run a no-op; (10) a single-`.ir.json` `Ddl → Backfill → Ddl` interleave on real PG (backfill runs after the first DDL's column exists, before the second DDL drops the source, kill-and-resume at the backfill boundary completes); (11) a multi-`Backfill` plan; (12) **PROPERTY-BASED CRASH FUZZ**: a generator produces random in-grammar plans and injects a crash at every step/sub-step/journal-write boundary, asserting "resume from any crash point reaches the same final DB state + journal as the no-crash apply," cross-checked against a model of the journal state machine.

### PR 1 — IR JSON contract + loader + IrAuthor (DDL only) + the anti-drift gate + the validator (Rust + skeletal JS)
- **Add `schemars` as a dependency** and decide+pin the serde representation of `Op`: internally-tagged `#[serde(tag = "op")]`, no `untagged`, no `flatten`. Record as an ADR line.
- Define `MigrationIr` + the closed `Op` enum (`Serialize`/`Deserialize` + `JsonSchema`); emit `op-ir.schema.json`. **All identifier fields are plain `String`** (§3.3); the schema does not encode any live-schema binding.
- **Checksum refactor (§2.4): extract `fold_common` + add `Checksum::of_ir`** — `ChecksumInput` is a struct, not an enum, so do not add a variant. Lift the non-`up`/`down` fold tail of `Checksum::of` (`migration.rs:322-340`) into `fold_common`, leaving every existing caller byte-stable; add `Checksum::of_ir(canonical_ops, flags, owner_app, deps, supersedes, preconditions)` folding the canonical op list (owner_app excluded from the hint). Fixture: the `.sql` path's checksum is byte-unchanged after the extraction (golden hash pre/post). Drive-by: bring the `migration.rs:226` `ChecksumInput.flags` doc-comment into line with the already-correct fold comment at `:321-323` — list all 8 fields (the `:226` site omits `engine_goodie_ddl`) and stop calling `timeout_ms`/`phase` "flags" (they are the 2 optional facets, not bools). Edit ONLY `:226`; `:321-323` already names all 8 and the fold at `:325` is already correct — do not touch either (§2.4).
- `IrAuthor::lower(ops, target_dialect) -> AppliedPlan` for the **portable + native DDL ops only**, including the net-new lowering (§6): snapshot construction for `DdlEmitter` ops, `createTable` via `zeroship_schema::query`, stand-alone constraint/`alterColumn*` render coverage. `target_dialect` is threaded from `deploy_migrate.rs` (§2.4.1). `renameColumn`/online and DML/backfill are NOT in PR1 (errors "deferred to PR2/PR6a").
  - **Extract the shared snapshot-builder (§6.5).** Lift the per-column / per-index snapshot construction out of `desired_snapshot`/`desired_snapshot_for_dialect` (`declarative.rs:1013`/`:1044`) into a dialect-parameterized shared builder; route BOTH the differ and `IrAuthor` through it (system-field injection, default rendering, `encryption_sentinel`/`comment_sentinel` from the `zeroship_schema::{query,mask_codec}` kernel — never re-spelled). A refactor-safety fixture asserts the extraction is byte-preserving for `desired_snapshot` (§6.5 fixture 3), and `IrAuthor`'s encrypted-column / system-field snapshots are byte-equal to the differ's (§6.5 fixtures 1–2).
- **IR-path per-table ownership check** (§8.6): override `owner_app` server-side; reject any op targeting a non-owned table and any op targeting a table absent from the registry (unknown-owner fail-closed, mirroring `declarative.rs:2820-2824`).
- Loader branch: recognize `<NNNN>_<desc>.ir.json`, deserialize, lower to a (usually single-step) plan, derive flags (§2.1.1 down matrix), compute checksum. Commit-once artifact contract (§5.1).
- **Skeletal JS builder + round-trip fixtures (the gate).** Ship a minimal `@zeroship/migrate` recorder in this PR — just enough to emit the golden corpus — so the byte-equality gate exists at the moment the IR shape is frozen.
- **Guard-per-fragment + reassembly** (§6.1.1): guard each rendered op fragment individually (op-index + `.ts` attribution), then concatenate the guarded fragments with the byte-identity invariant `applied_up == join(guarded_fragments)`.
- **Structured-error envelope + the portable-expression-grammar validator land HERE, at IR-freeze.** (1) Every authoring-time rejection carries the `{ code, op_index, ts_location, dialect, reason, suggested_fix? }` payload (§8.8); (2) the **dialect-neutral portable-expression-grammar validator** (§3.3.1.1 — the lexer + tiny closed AST + allow-list, not libpg_query) ships in PR1. **The Rust recognizer is the sole apply-time gate; the JS side is a strictly-more-rejecting lint, with a one-directional differential fuzzer (JS-accept ⇒ Rust-accept).** The shared-grammar-table + per-language Pratt interpreter is an optional future upgrade, not built here. (The DML *executors* remain PR6a/PR6b; only the validator + error envelope move to PR1, since the validator is pure analysis with no executor dependency and the AI loop needs it before any DML PR.)
- **Tests**: golden `.ir.json` fixtures authored by both the skeletal JS builder and read by Rust, value-equal (typed-value checksum) → render → apply on PG + SQLite; a portable migration's checksum is identical across PG and SQLite renders (§2.4); checksum stability; unknown `ir_version` fail-closed; `op.raw({sqlite})` refused fail-closed; non-owner op rejected; an op targeting a table with no registry entry rejected (unknown-owner); a `createIndex{concurrently}` auto-derives a non-txn idempotent `DROP INDEX CONCURRENTLY IF EXISTS` down on PG and a txn-safe `DROP INDEX IF EXISTS` down on SQLite (§2.1.1).
  - **Exhaustiveness gate**: derive the `Op` variant list from `op-ir.schema.json` and fail if any variant has zero fixtures.
  - **Numeric-domain invariant**: a hand-crafted `.ir.json` (simulating a malicious/buggy builder) carrying a fractional/exponential number or a plain integer ≥ `2^53` is rejected at load by the Rust deserializer (`EXPR_INVALID_NUMERIC`) before `Checksum::of_ir` runs; a decimal-string and an exact `i64` both load and produce the same `Checksum::of_ir` as the JS-builder equivalent.
  - **Cross-path render parity**: for `createTable` and each constraint / `alterColumn*` op, the stand-alone `IrAuthor` render is byte-identical to the same shape via `DeclarativeAuthor::diff` (incl. `comment_stmt` side outputs + injected system fields), on PG + SQLite (§6.4).
  - **Guard byte-identity**: for a multi-statement `createTable` (inline index + comment), `applied_up` is byte-identical to the concatenation of the individually-guarded fragments; a denied `op.raw` fragment aborts the step with op-index + `.ts` location and applies nothing.
  - **PG-only surfacing**: an `op.raw({pg})`-only migration loads with `dialect_scope = PgOnly`, and deploying it against a SQLite target is rejected at load.
  - **Validator + structured-error envelope**: the validator accepts the allow-listed fragments and rejects `split_part`/subquery/window/`;` on both dialects; `ColRef` resolution rejects a cross-table column reference (target-table-scoped, an apply/render-time check); every rejection emits the structured payload (§8.8).
  - **Anti-drift one-directional fuzz**: a differential fuzzer generates randomized fragments over the token alphabet and asserts the JS lint **never accepts what the Rust recognizer rejects** (JS-accept ⇒ Rust-accept). The JS lint is allowed to be strictly more rejecting. A guard test asserts the Rust recognizer is the sole apply-time gate.

### PR 2 — Online ops (`renameColumn`): the `OnlineRename` step + the cross-subsystem bridge + dual-execution dispatch
- `IrAuthor` lowers `renameColumn` to an `OnlineRename(RenameStep)` step (§2.6.1): PG ⇒ `RenameStep::PgExpandContract` via `ExpandContractAuthor` (E1..C2 + backfill, sub-versioned), SQLite ⇒ `RenameStep::SqliteRebuild` via the rebuild planner (the net-new `OnlineIntent`→rebuild bridge). Parameterized by the deploy-target dialect (§2.4.1).
- **Execution rides PR0's dual-dispatch**: this PR authors the two `RenameStep` variants; PR0's `apply_plan` already routes `PgExpandContract`→`run_online` and `SqliteRebuild`→`rebuild_one` (§2.6.2). This PR wires authoring into the execution PR0 built; its e2e proves both legs from a single `op.renameColumn`.
- **Neutral-type translation**: the op carries a dialect-neutral `ColType`; `IrAuthor` maps it to a PG type string before building `OnlineIntent` (PG) and to a SQLite affinity type before the rebuild bridge (SQLite). No PG type string is ever passed to the SQLite leg.
- Down = contract→expand reversal for a fully-applied rename; `None` + roll-forward for in-flight (§2.6.1).
- **Tests**: faithful e2e — `op.renameColumn` applies as online dual-write on PG (5 sub-steps + backfill, correct journaling + crash-resume) and as offline rebuild on SQLite via `rebuild_one` (§2.6.2); mid-sequence crash resumes roll-forward; a `renameColumn` whose new type is a neutral `ColType` renders the correct PG type in `OnlineIntent` AND the correct SQLite affinity in the rebuild; an `op.renameColumn`'s E1..C2 ids + intra-chain `depends_on` are byte-equal to the equivalent declarative `t.*`-diff rename's ids (reconciling with `expand_contract.rs:777-783` + the set-integrity manifest); the SQLite leg executes through `MigrationBackend::rebuild_one`, not `run_online` (asserted via the journal).

### PR 3 — `@zeroship/migrate` JS op builder package (full surface)
- Flesh out the recording `op` builder (pure, no I/O) to the full §3.2 surface; emit canonical `.ir.json`. **No generic `S extends Schema` on `Op`; all identifier args are `string`** (§3.3).
- TS types generated from `op-ir.schema.json` (`json-schema-to-typescript`) **as ergonomics**, with manual types for any serde shape codegen cannot express; the PR1 fixtures remain the source of truth.
- `Migration` module shape; `op.sql`/`op.raw`/`op.col`; determinism lint (Date.now/RNG AST flag, §4.3).
- **Tests**: builder emits the full fixture corpus value-equal to the Rust-read goldens; determinism (same source → same JSON); lint flags a `Date.now()` in an op argument.

### PR 4a — The kernel-sandboxed recorder service (its OWN PR — the riskiest single deliverable, split from CLI ergonomics)
This is the highest-risk net-new surface (OS-level isolation of untrusted JS), so it ships as a dedicated, separately-reviewed PR ahead of CLI ergonomics — never folded into `new`/`generate`. It delivers the §8.9/§8.9.2 recorder service alone.
- **Build-time recorder runs in a dedicated OS-isolated child process** (§8.9): seccomp-bpf default-deny + landlock read-only fs + an interface-less network namespace, `RLIMIT_CPU`/`RLIMIT_AS` + wall-clock watchdog, and a custom-resolver module allow-list — NOT a same-process Node `vm`, and NOT "rely on CI sandbox alone." Reuse the `crates/sandbox` posture.
- **State the kernel baseline + degraded floor explicitly** (§8.9): full ruleset is seccomp + landlock + netns; landlock requires Linux ≥ 5.13; on a landlock-less host the degraded floor is seccomp default-deny + netns + rlimits + the userland module-allow-list resolver as the fs boundary. In the **hosted multi-tenant service** the recorder **refuses to run** with neither seccomp nor netns (no unconstrained-Node fallback); the **local single-tenant/self-host recorder** (§8.9.2) runs the developer's own code under the userland-budget floor with the kernel ruleset applied opportunistically — kernel isolation is mandatory only on the multi-tenant path.
- **The HOSTED recorder-service contract** (§8.9.2): `POST /v1/recorder/record` taking `{ ts_source, app_id, schema_types_blob }` → `{ ir_json, ts_provenance_blob, checksum }` or the §8.8 structured error; PAT/`ZEROSHIP_TOKEN` auth + server-side `app_id` ownership cross-check (§8.6); a fresh sandbox child per invocation (no pooling across tenants); per-token concurrency/rate limits + a global cap with a queue; recorder-unreachable ⇒ retry/fallback-to-local-recording (§8.9.2), not a build failure.
- **Tests — FAITHFUL kernel-level e2e** (per `feedback_faithful_e2e_tests.md`: the real path, not a userland-resolver stub):
  - A `.ts` that `import`s/uses `child_process` (`spawn`), `net`/`http` (outbound connect), and `fs` (read outside the migration dir / any write) is **killed by the kernel** (seccomp `SIGSYS` / netns no-route / landlock-or-resolver fs denial), surfaced as the §8.8 structured error — asserted by observing the child's termination cause, NOT merely that the userland resolver refused the import. A separate assertion proves the kernel layer fires even when the userland resolver is bypassed (e.g. a native addon or a dynamic `require`).
  - `RLIMIT_CPU`/wall-watchdog/`RLIMIT_AS` bound an infinite loop / allocation bomb → `BUILD_RECORDER_BUDGET_EXCEEDED` (§8.8).
  - On a landlock-less test host, the degraded floor still kills the `child_process`/`net` cases (seccomp+netns), and the recorder refuses to start with neither seccomp nor netns.
  - Per-invocation isolation: two concurrent `record` calls get separate sandbox children; no fs/state bleed between them.

### PR 4 — Build/dev execution + `new` + `generate` (autogenerate) — depends on PR4a
The recorder (PR4a) already exists; PR4 wires it into the build/CLI ergonomics, with no kernel-sandbox work of its own.
- Build step (CLI + vite-plugin): discover `migrations/*.ts`, evaluate **via the PR4a recorder service**, emit `.ir.json`; bundle as `MigrationFileEntry`. **The packer consumes the committed `.ir.json` verbatim and MUST NOT re-evaluate the `.ts` for an already-committed version** (§5.1); a CI assertion checks the packed `MigrationFileEntry.hash` equals the committed blob's sha256.
- **First-commit canonicalization** (§8.9.1): canonical bytes are anchored on the typed-value checksum, not on a single emitting toolchain. The **hosted multi-tenant path** records through the pinned platform build image's PR4a recorder (thin client, §8.9.2); the **local single-tenant/self-host path** records locally under the userland-budget floor (§8.9.2). Either way the CI gate re-records each not-yet-applied `.ir.json` through the canonical image and asserts the typed-value checksum matches.
- `zeroship-migrate new` scaffolds `.ts` (time/uuid seed columns default to `op.sql\`now()\``/`gen_random_uuid()\``, §4.3).
- `generate` emits an `op.*` `.ts`/`.ir.json` scaffold from the declarative diff (autogenerate parity, §7.1), with `// TODO: backfill` markers (machine-readable open-obligations in the AI path, §8.8).
- **Tests**: e2e `new` → edit → build → deploy → apply on PG + SQLite, run **both** as a local single-tenant record (under the userland-budget floor) and as a hosted thin-client → recorder round-trip, asserting the committed `.ir.json` has the same typed-value checksum on either path; `generate` round-trips a `t.*` schema diff into an applyable IR; the hosted thin-client → recorder → committed `.ir.json` round-trip (recorder-unreachable falls back to local recording, not a build failure); CI regenerates each not-yet-applied `.ir.json` via the platform build image and asserts the typed-value checksum matches the committed blob; packed `MigrationFileEntry.hash` == committed `.ir.json` sha256.

### PR 5 — Structural type-safety from `@zeroship/db` (names stay strings)
- Wire `ColType`↔`t.*` from `sdks/db` so the migration DSL and runtime schema share one type lexicon. **Do NOT bind table/column NAMES to the live schema** (§3.3): there is no `TableName<S>`/`keyof RowOf<S,T>`/`RowInsert<S,T>` on the op surface. The optional row generic `insert<R>`/`update<R>` is caller-supplied, never auto-derived from the schema.
- `tsc` gate: a migration with a malformed op *shape* or an invalid `ColType` fails type-check. A migration referencing a non-existent table/column does **not** fail `tsc` — it is validated at apply time against the real DB (§3.3).
- **Tests**: type-level tests (expect-error for a bad op shape / bad `ColType`); a real migration whose names are plain strings type-checks even when those names are not in the current `t.*` schema (proving names are not live-schema-bound); an apply-time test that a non-existent column fails at apply with the structured error, not at `tsc`.

### PR 6a — DML surface (`insert`/`update`/`delete`/one-shot + PG batched `backfill`)
- **New creator-DML assembler** for `insert`/`update`/`delete`: identifier quoting + **parameterized value binding**, NULL/typed-literal handling, PG `onConflict` rendering, migrator role, guard (PG) / authorizer (SQLite). Dialect-aware (binds via `$n` on PG and `?n` on SQLite), so literal-`insert`/`delete`/portable-`update` **one-shot** DML is portable on both backends here; only the *batched backfill* SQLite executor is PR6b.
  - **ONE SQLite-DML-assembly module, shared by PR6a one-shot DML and PR6b batched backfill.** The SQLite affinity-aware value-binding + authorizer-screening path is factored into a single module in PR6a; PR6b's batched-backfill SQLite executor (§2.3.1) reuses it for per-batch statement assembly rather than re-implementing value binding. This prevents two divergent copies of the SQLite affinity/`?n`-binding logic across the two PRs. (PR6a builds the one-shot assembler + the shared module; PR6b builds the batched cursor-loop executor *on top of* that shared module.)
- **PG `backfill` uses the existing `BackfillSpec` executor** (`backfill.rs:286`); the SQLite backfill executor is PR6b. So in PR6a, batched `backfill`/`update{batch}` is PG-only (a plan whose only data step is a batched backfill against a SQLite target surfaces as a hard error on a SQLite deploy, never silent — until PR6b lands).
- **Wire the PR1 validator into the DML/one-shot-update path** so a non-portable fragment is rejected before assembly.
- `insert { onConflict }`: native on PG; hard authoring error on SQLite (§9) — not a silent raw route.
- **Tests**: faithful e2e — seed via `insert`, prune via `delete`, transform via one-shot `update` on **both real PG (`:5440`) AND real SQLite** (literal/portable forms — the one-shot SQLite DML path is exercised here, distinct from and ahead of PR6b's batched-backfill SQLite e2e, so PR6a's "portable on both backends for one-shot DML" claim is backed by a real-SQLite apply, not only PG), and via batched `backfill` on **PG** (resumable, crash-safe); **the AUTHORING-PATH e2e for `Dml`** (completing PR0 test (9)): a creator `op.insert`/`op.update` flows through the NEW assembler → a `Dml{template,binds}` step → PR0's `apply_plan` → a real DB, asserting the assembler-produced template/binds match what the executor records; assert a non-portable `set`, a SQLite `onConflict`, and a SQLite-targeted batched `backfill` are rejected at build/render (the last as `dialect_scope = PgOnly`), not silently mis-applied. **Bind-safety**: an `insert`/`update` whose bind value contains SQL metacharacters cannot alter statement structure on either backend (native `$n`/`?n` binding, not interpolation). **Validator fixtures**: a `cast(x as integer)` and an `a || b` with a NULL operand apply identically on both backends; a `split_part`, subquery, window clause, and `;` reject. **Interpolation/column-scoping fixtures**: `op.sql\`x > ${v}\`` binds `v` as a native parameter; `op.sql\`other_table.secret > 0\`` is rejected; `op.sql\`${'other_table.secret'} > 0\`` is admitted only as a literal string bind; `op.sql\`${columnName} > 0\`` emits `EXPR_INTERPOLATED_IDENTIFIER` whose `suggested_fix` names `op.col(...)`, and `op.sql\`${op.col('first_name')} = ${v}\`` validates.

### PR 6b — SQLite backfill executor + portable-helper SQLite lowering (COMMITTED — the bi-dialect headline)
- **New SQLite backfill executor** (§2.3.1): plain batched `UPDATE … WHERE rowid IN (SELECT … LIMIT n)`, affinity-aware cursor cast, crash-safe progress in the shared meta table. After this lands, `backfill`/`update{batch}` is portable across both backends — the §1.1 "one script, both backends, DDL+DML" promise for batched data transforms.
- **Add `instr` to the SQLite authorizer allow-list — its own reviewed line item (§9).** Append `"instr"` to `FUNCTION_ALLOWLIST` (`backend_sqlite/authorizer.rs:131`), with the doc-comment justification (deterministic, side-effect-free scalar; no extension load / fs / network / tenant escape; required by the engine's pinned `op.fn.splitPart` lowering; kept in lockstep with the emitter function set). The *authorizer* allow-list only — `instr` is NOT added to the *portable-expression grammar* allow-list (§3.3.1), so author-written raw `instr` bodies remain rejected.
- **Portable-helper SQLite lowering** (§9): the pinned, exhibited, authorizer-legal `instr`/`substr` SQLite expression for **`op.fn.splitPart` only** (the `cur₀ = col||d`, `curᵢ = substr(curᵢ₋₁, instr(curᵢ₋₁,d)+1)`, `result = substr(cur_{n-1},1,instr(cur_{n-1},d)-1)` unroll, proven against SQLite 3.51.2) within the single-ASCII-delimiter + positive-literal-`n` envelope, plus the out-of-envelope hard-error path. **`op.fn.substr` is NOT shipped here** — it is deferred to a follow-up PR that must exhibit + prove its PG/SQLite byte-identity (§9); `op.fn.replace` stays PG-only until `replace` is allow-listed.
- **Tests**: faithful e2e — transform via `backfill` (resumable, crash-safe) on **SQLite** (no PG-gated skip hiding the SQLite leg); the §3.1 hero DDL+backfill shape applies on **both** backends from one `.ir.json`. **Portable-helper edge-matrix fixtures (the full §9 envelope, MANDATORY)**: `op.fn.splitPart(x,' ',n)` byte-identical on PG and SQLite across the in-envelope cases (two-token, single-token→`''`, beyond-tokens→`''`, empty input, trailing/leading/consecutive delimiter→`''`, NULL, a multibyte/UTF-8 VALUE split on an ASCII delimiter, a non-default-collation value); every out-of-envelope shape (multi-char/empty/non-ASCII delimiter, `n=0`/negative `n`, `n>8`, non-literal delimiter/`n`) rejected with `EXPR_NOT_PORTABLE` on the SQLite leg on BOTH the Rust validator and the JS lint. A raw `split_part(...)`/`substr(...)`/`instr(...)` string is rejected on SQLite while the in-envelope `op.fn.*` helper is accepted; the SQLite lowering's `instr` is asserted to apply cleanly under the SQLite authorizer (the allow-list addition exercised end-to-end). `op.fn.replace` asserted PG-only until `replace` is allow-listed.

### PR 7 — CLI rewire + raw-SQL demotion — **GATED ON PR6b (SQLite DML) AND PR2 (online)**
**Precondition: PR6b (SQLite backfill executor) and PR2 (online) are merged.** The demotion of raw-`.sql` authoring below `op.*` is only honest once `op.*` can express data migrations and online rename portably **on both backends**; before PR6b, batched data migrations are PG-only, so raw `.sql` is still the only way to author a portable bi-dialect data migration.
- `new`/`generate` default to `op.*` for the full surface (DDL + DML + online); raw `.sql` loader retained (for `op.raw` + dbmate CLI + platform Flyway-mode) but no longer the recommended authoring path.
- **Tests**: mixed history (IR + legacy `.sql`) loads in one ordered timeline; `apply`/`status`/`rollback` unchanged; an e2e proves an `op.*`-authored DDL+backfill migration (the §3.1 hero shape) applies on **both** backends without any raw `.sql` — the concrete evidence the "replaces raw SQL" headline is true.

### PR 8 — Docs
- New `docs/reference/migrate-op-dsl.md`: the op vocabulary, the names-are-strings typing stance (§3.3), DML portability boundary, `op.raw` guidance.
- Update `docs/reference/sqlite-divergences.md` cross-link; update the migration-engine design doc; AGENTS.md task-router row.
- **Tests**: doc examples are compiled/applied in CI (no rotted snippets).

---

## Appendix A — IR ⇄ Rust type mapping (citations)

| IR concept | Rust type / path |
| --- | --- |
| `MigrationIr` → applied artifact | **`AppliedPlan { steps: Vec<PlanStep> }`** (NEW, §2.0 — NOT named `MigrationPlan`, which is the existing dry-run preview type at `engine.rs:58`) — not a single `Migration`; steps reuse `Migration` (`migration.rs:379`), `BackfillSpec` (`backfill.rs:76`), `ExpandContractPlan` (`expand_contract.rs:104`) |
| `PlanStep` orchestration | **NEW single shared `apply_plan` (PR0)** — built by lifting the interleave out of `apply_declarative_locked` (`engine.rs:446`), reusing the same downstream primitives as destinations: PG online via `run_online` (`:537`) → `run_expand_pg` (`expand_contract.rs:693`) + `pending_contract` (`:521`/`:550`), and SQLite rebuilds via `MigrationBackend::rebuild_one` (the `plan.rebuilds` loop `:491-503`). There is no `apply_expand`. `apply_plan` dispatches `OnlineRename` by `RenameStep` variant; the per-`Migration` step delegates to the reused `apply_with_lock_backend` (`:661`). **PR0 re-points `apply_declarative_locked` onto `apply_plan` via a shape-adapter (§6.0) — one orchestrator, regression-netted by the ~507-test declarative suite + the frozen-trace oracle** |
| `RenameStep` dual-execution | **NEW (§2.0/§2.6.2)** — `PgExpandContract(ExpandContractPlan)` via `OnlineSchemaChange::run_online` (only impl `PgOnline`, `expand_contract.rs:760`); `SqliteRebuild(declarative::SqliteRebuild)` (REUSES `declarative.rs:2053`, not a new struct) via `MigrationBackend::rebuild_one` (`backend_sqlite/mod.rs:550`). No `SqliteOnline` impl exists (`backend_sqlite/mod.rs:567` returns `None`) |
| single `Migration` (one plan step) | `Migration` — `migration.rs:379` (single `up:String`, `:385`; no BackfillSpec slot) |
| `flags` | `MigrationFlags` — 8 orthogonal fields = **6 bool** (`transactional`/`destructive`/`online`/`requires_approval`/`repeatable`/`engine_goodie_ddl`) + **2 optional facets** (`timeout_ms: Option<u64>`, `phase: Option<OnlinePhase>`) — `migration.rs:128-185` (NOT 8 bools; the 2 Options are kept separate so the bools stay orthogonal) |
| checksum folding (reused) | `Checksum::of`, length-prefixed SHA-256 — `migration.rs:303`; the shared tail extracts to **`fold_common`** (no byte change, §2.4); **NEW** parallel front door `Checksum::of_ir(canonical-IR)` reusing `fold_common`. `ChecksumInput` is a **struct** (`migration.rs:220`, `up: &str` `:222`); it is not converted to an enum |
| DDL render seam (5 ops only) | `DdlEmitter` trait — `declarative.rs:3810`; `PgEmitter` `:3864`; `SqliteEmitter` `:3999`. Takes `ColumnSnapshot`/`IndexSnapshot` — IrAuthor builds snapshots (NEW) |
| createTable render seam (NOT DdlEmitter) | `zeroship_schema::query` CREATE-TABLE emitter via `DeclarativeAuthor` — `declarative.rs:~430-565` |
| constraint / alterColumn render | inside `DeclarativeAuthor::diff` / `render_alter_column_type` `declarative.rs:3609` — stand-alone coverage is NEW (§6) |
| online rename intent (PG only) | `OnlineIntent::RenameColumn` — `expand_contract.rs:70`; **`ty` is a raw PG type string** (`:82`) — so the IR carries a neutral `ColType` and `IrAuthor` maps it to PG type before building the intent (§2.6); `ExpandContractAuthor::author` `:285` (no dialect param, no SQLite arm); `ExpandContractPlan` `:106` |
| SQLite rename lowering (NEW bridge) | rebuild planner — `SqliteRebuildSpec`, `backend_sqlite/rebuild_sql.rs`; `OnlineIntent`→rebuild bridge is net-new in IrAuthor (§2.6) |
| SQLite rename **execution** | `MigrationBackend::rebuild_one` — `backend.rs:262`, SQLite impl `backend_sqlite/mod.rs:550`; driven today via `plan.rebuilds` (`engine.rs:491-503`), NOT `run_online`. `apply_plan` dispatches `RenameStep::SqliteRebuild` here (§2.6.2) |
| `load_dir` return type | `Vec<Migration>` → **`Vec<AppliedPlan>`** (`loader.rs:606`); blast radius: `deploy_migrate.rs:133`, dbmate CLI bin, `platform_runner.rs` ×7 (via `single_step()`), `lib.rs:145` re-export, ~20 tests (§5.2) |
| backfill spec | `BackfillSpec` — `backfill.rs:76`; PG-only assembled writable-CTE UPDATE `:286`; **SQLite executor is NEW (§2.3.1, PR6b, committed)** |
| SQLite rebuild routing | `sqlite_existing_table_needs_rebuild` — `declarative.rs:3110`; `SqliteRebuildSpec` — `backend_sqlite/rebuild_sql.rs` |
| creator DML assembly | **none today** (internal-only `INSERT INTO` at `executor.rs:1951`, `author.rs:614`); assembler is NEW (§6, PR6a) |
| SQLite function allow-list | `FUNCTION_ALLOWLIST` — `backend_sqlite/authorizer.rs:131`; `instr` added in PR6b (§9) |
| IR-path ownership check | **none today** on op path (`DeclarativeAuthor::diff` map at `declarative.rs:283-294`,`:904-913` is bypassed); NEW (§8.6) |
| declarative diff (autogenerate source) | `desired_snapshot` / `DeclarativeAuthor::diff` — `generate.rs:93`/`:112` |
| loader & versioning | grammar `loader.rs:19`; `load_dir`; `migration_id_for_version` |
| bundle entry | `MigrationFileEntry { name, hash }` — `crates/bundle/src/manifest.rs:160` |
| deploy reconstruct+apply | `crates/control/src/deploy_migrate.rs` |
| guard | `SqlGuard::check`, `Confined` default — `crates/zeroship-migrate/src/guard.rs` |
| least-priv role | `provision_migrator` — `role.rs:197` |
| journal immutability | triggers — `journal.rs:332` |

---

## Appendix B — Changelog (non-normative)

This appendix is non-normative. The body above is the single normative contract. This records the design's final resolution of points that earlier review rounds had stated differently, so a reader comparing against the annotated draft can see what changed and why.

| Topic | Final normative reading (body) |
| --- | --- |
| **Typing** | Table/column NAMES are plain strings; never bound to the live schema (`TableName<S>` / `keyof RowOf<S,T>` / `RowInsert<S,T>` are NOT used). Structural type-safety (op shapes, `ColType` lexicon, `op.raw` shape, insert-row value kinds) is preserved; name existence is an apply-time check against the real DB, `op.col(...)`/`ColRef` resolution is an apply/render-time check against the target table. Precedent: Kysely `Kysely<any>`, Alembic string names, Drizzle generated SQL. (§0.1, §3.2, §3.3) |
| **Validator anti-drift** | The Rust recognizer is the sole authoritative apply-time gate; the JS side is a strictly-more-rejecting lint (JS-accept ⇒ Rust-accept, one-directional differential fuzzer). The shared-grammar-table + per-language Pratt interpreter is an optional future upgrade, not a prerequisite. (§3.3.1.1) |
| **Orchestrator** | ONE shared `apply_plan`. PR0 builds it and re-points the shipped declarative path (`apply_declarative_locked`, Task #46/#54) onto it via a thin shape-adapter as REQUIRED CORE scope. The regression net is the existing ~507-test declarative suite staying green through `apply_plan`, plus the frozen-trace oracle. No two-orchestrator fork. (§6.0, PR0) |
| **Scope / DML** | Portable bi-dialect DML (incl. data migrations on BOTH PG and SQLite) is committed scope. The SQLite backfill/DML executor (PR6b) is in the delivered plan (Epic 2, committed), not a demand-gated maybe. (§9, §10) |
| **`dialect_scope`** | Derived; journaled as a separate apply-time-immutable column, NOT folded into the identity checksum; current portability is a separate computed `status` field. (§2.4) |
| **Checksum canonicalization scope** | `Checksum::of_ir` is a HYBRID fold: the op-list region IS RFC 8785 JCS-canonical, but the `fold_common` tail (flags/owner/deps/supersedes/preconditions) uses the EXISTING serde discipline (`serde_json::to_string(flags)`, `migration.rs:325`, no key sorting), byte-for-byte identical to `Checksum::of`'s tail — NOT JCS. The "JCS-canonical / invariant under JCS formatting" claim is scoped to the op-list region and the on-disk `.ir.json` bytes, never to `fold_common`. (§2.4 point 5, §2.5, §4.3) |
| **`op.sql` fragment bind canonicalization** | An interpolated `${v}` bind inside an `op.sql` `set`/`filter`/`where` fragment folds into `Checksum::of_ir` via the fragment-node rule (template text + ordered typed binds, same typed-scalar canonicalization as the `insert` `rows` facet), so changing an interpolated threshold value is drift. (§2.4 point 3, §2.3.2) |
| **IrAuthor snapshot construction** | IrAuthor does NOT hand-build snapshots — PR1 extracts a shared, dialect-parameterized snapshot-builder from the differ's `desired_snapshot` that BOTH the differ and IrAuthor call (system-field injection, default rendering, encryption/mask sentinels from the `zeroship_schema` kernel). The §6.4 byte-identity gate guards regressions, not two implementations. (§6.5) |
| **§5.4 squash** | Collapse the DDL spine; **retain** `Backfill`/`Dml`/`OnlineRename` steps un-collapsed in version order. On an existing DB a retained step is a supersession marker (net-applied-skipped). On a fresh rebuild, a **literal-rows `insert`** `Dml` IS replayed (a purely structural predicate, `replayable_on_fresh:true`); every other step — any `update`, non-literal `insert`, backfill, or online rename — is `false` and skipped, and the skip is signalled (`SQUASH_FRESH_REBUILD_DATA_NOT_RECONSTRUCTED`). The design does not attempt the undecidable "is this update idempotent" judgement. The earlier "pure-DDL-only; refuse any range with a data step" reading was a capability regression and is not the rule. (§5.4) |
| **`op.fn.splitPart` SQLite lowering** | `instr`-based, exhibited, CSE-not-required inline form bounded to `n ≤ 8` (the inline unroll is O(2ⁿ); ~17 KB at n=8 against SQLite 3.51.2); a CSE-shared form to raise the bound is deferred. `instr` is added to the SQLite authorizer allow-list (PR6b). PR6b ships ONLY `op.fn.splitPart`; `op.fn.substr` is deferred until its PG/SQLite byte-identity is exhibited + proven (it is the divergence trap §3.3.1 names), and `op.fn.replace` is PG-only until `replace` is allow-listed. (§9) |
| **Provenance gate** | Mandatory for the creator/AI path (`.ts` always bundled; deploy-time consistency gate always fires for not-yet-applied versions). "Committed `.ir.json` as sole authority" is reserved for the trusted operator/platform path. (§5.1) |
| **Determinism AI-path check** | Scoped to time/uuid-typed columns; the `op.sql`-default scaffold is primary; the blanket "reject any non-literal value argument" is not the rule. (§4.3) |
