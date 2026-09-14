# Historical in-tree migration-engine reference

**What this is:** a historical engineering snapshot of zeroship's former in-tree,
security-first, dialect-neutral database-migration engine. The engine was extracted
to a vendored `third_party/zero-migrate` tree and has since been IN-SOURCED again:
it now lives in this workspace as the `crates/zeroship-migrate*` crates, and
`third_party/` no longer exists. The old single workspace crate and the standalone
executables are gone.

**Who it's for:** engineers modifying the crate, authors writing platform/creator migrations, and reviewers auditing the security substrate.

Do not use the old commands in this snapshot as operational instructions. Current
platform apply instructions live in [Database migrations](../runbooks/db-migrations.md),
and the current creator build path is documented in
[Vite plugin](./vite-plugin.md#migration-first-type-generation-gen-types).

**How to read the citations.** An unqualified `file:line` citation names a path
inside the former `crates/zeroship-migrate/` crate and records what was read
there when this guide was written; it is a historical reference, not a live
pointer. A citation that begins with `sdks/` or `crates/` resolves against the
tree as it stands today, and names the file that carries the claim now.
(Citations once began with `third_party/zero-migrate/`, from the period when the
engine was vendored. Those have been repointed at the in-sourced crates; a
`third_party/` path resolves against nothing.)

**Current authoring contract (2026-09-01).** There is one npm authoring package,
`@zeroship/migrate`, in `packages/zero-migrate/`. A current module exports either
`schema()` for DDL, `data()` plus recorded `inverse()` for reversible DML, or
`data()` plus a non-empty `irreversible` reason. Schema and data cannot share a
module. The generic forward/reverse phase names discussed in historical engine
sections below are not aliases and are rejected by the current recorder. For
current examples, use [the op DSL reference](./migrate-op-dsl.md).

---

## Table of contents

- [§1 Overview — what & why](#1-overview--what--why)
- [§2 Crate architecture](#2-crate-architecture)
- [§3 Authoring: schema-structure DSL](#3-authoring-schema-structure-dsl)
- [§4 Authoring: the expression sublanguage](#4-authoring-the-expression-sublanguage)
- [§5 Authoring: declarative desired-state & the fold](#5-authoring-declarative-desired-state--the-fold)
- [§6 The IR & its wire contract](#6-the-ir--its-wire-contract)
- [§7 The validate gate & error taxonomy](#7-the-validate-gate--error-taxonomy)
- [§8 One IR, three dialects: render & portability](#8-one-ir-three-dialects-render--portability)
- [§9 The apply engine & durability](#9-the-apply-engine--durability)
- [§10 Security-first design](#10-security-first-design)
- [§11 Platform self-hosting & build integration](#11-platform-self-hosting--build-integration)
- [§12 Testing & operating](#12-testing--operating)

---

## §1 Overview — what & why

`zeroship-migrate` is zeroship's **own, purpose-built, security-first, versioned database-migration engine**. It replaces the industry-standard tools (Flyway / Liquibase) with an in-house engine because the platform's threat model, execution model, and authoring model are all fundamentally different from what those tools assume. This section explains the problem it solves, why it was built from scratch, the "project-umbrella" data model it serves, where it sits in the platform, and its dialect-neutral checksummed-IR philosophy.

### 1.1 The one-paragraph definition

Per the crate's own module doc (`lib.rs:1-19`), `zeroship-migrate` was:

- a **security core** — the migration data types, the parse-time SQL security guard (deny-list + cross-schema confinement), and defense-in-depth trust profiles;
- a **migration unit + executor** — an append-only tamper-evident journal, a per-project advisory lock, and a transactional / two-phase-non-transactional apply flow with idempotent crash recovery and drift/tamper checksum verification, all run under a least-privilege `migrator` role;
- a **JS-first authoring front-end** — V8-backed schema evaluation, `op.*` recording, dialect-neutral IR canonicalization, and type generation (`lib.rs:4-7`), packaged for creators as the `@zeroship/migrate` npm SDK and formerly exposed to the platform through a JS authoring CLI that was removed with the in-tree engine.

The crate index summarizes it as: "*Migration engine. Multi-dialect apply: native compio-postgres fast path (PG) + in-process SQLite + live MySQL via the JsDriverBackend... Carries V8 (depends on zeroship-runtime) for the JS authoring front-end + the MySQL driver isolate.*" (`AGENTS.md`, crate index).

### 1.2 The problem it solves

A zeroship migration is an unusually hostile artifact. The crate threat model is summarized in `lib.rs:42-47`:

> Migrations are **privileged arbitrary-SQL** authored by **untrusted** creators *and* a **prompt-injectable AI**.

That single sentence is why an off-the-shelf tool doesn't fit. A migration must run **DDL** (which needs elevated privilege), but the SQL comes from a creator who is untrusted, or — worse — from an AI builder that can be prompt-injected via app content or templates. The threat vectors are:

| # | Vector | Concrete danger |
| --- | --- | --- |
| 1 | Cross-tenant access | touching another project's schema, or `control`/`auth`/`billing` |
| 2 | Privilege escalation | `CREATE ROLE`, `GRANT`, `ALTER SYSTEM`, `pg_authid` |
| 3 | Host-escape / RCE | `COPY … FROM/TO PROGRAM` (shell), untrusted PLs (`plpythonu`/`plperlu`), `LANGUAGE C`, dangerous `CREATE EXTENSION`, `dblink`/`postgres_fdw` (SSRF), `lo_import`/`lo_export` + `pg_read_server_files` (filesystem) |
| 4 | Prompt-injection → malicious migration | the AI author must be gated *regardless of what SQL it emits* |
| 5 | Tampering / supply-chain | migration edited after approval; mutable journal |
| 6 | DoS | indefinite locks / unbounded ops starving shared infra |
| 7 | Destructive ops | `DROP` / `TRUNCATE` data loss |

Flyway and Liquibase are **trusted-operator** tools: they assume a human DBA authored the changesets, execute SQL through the connection privileges you hand them, and perform no adversarial parse-time analysis of statement content. None of vectors 1–4 are in their model. zeroship needed an engine where every migration is treated as **untrusted input, confined by DB privilege *and* independently verified at parse time** — "belt and suspenders." The full security substrate is documented in [§10 Security-first design](#10-security-first-design), which is the canonical home for the two-line defense model.

### 1.3 Why zeroship built its own engine

Four forcing functions:

1. **Untrusted-by-default, defense-in-depth security** (the decisive reason). The engine enforces two independent lines (`lib.rs:49-62`): **Line 1** — the `SqlGuard` parses every statement with the **real Postgres parser** (`pg_query` / `libpg_query` C bindings) and checks a hard deny-list (the choice of real parser is itself a security decision — a pure-Rust `sqlparser-rs` is incomplete for exotic PG syntax = a security gap; `Cargo.toml` comment). **Line 2** — the least-privilege per-project `migrator` role (`NOSUPERUSER NOCREATEROLE NOCREATEDB`, no grants on platform/other-project schemas, `search_path` pinned) so the DB itself rejects the same ops even if SQL slips past parse. Defense is layered *and redundant*: the engine gate refuses denied/destructive-unapproved plans, then the executor **independently re-runs the guard and re-applies the role** (`lib.rs:36-40`, `engine.rs:20-27`).
2. **Zero-tokio, in-stack execution.** A platform key invariant is "*Zero tokio in the stack — everything is compio/io_uring*" (`AGENTS.md`). The apply fast path runs through the bespoke **`compio-postgres`** driver. An external JVM tool or a tokio-based Rust migrator would violate that invariant. The guard itself runs out-of-band at deploy time, so it is plain synchronous logic with no async runtime (`lib.rs:63-65`).
3. **UUIDv7 versioning for concurrent multi-app authoring.** Migration version = `UUIDv7` (`mig_…` typed-id), *not* Flyway-style sequential integers (which collide under concurrent multi-app authoring) or raw timestamps (which skew). UUIDv7 gives a collision-free total order by its time component.
4. **Integration with zeroship's own conventions** — the immutable/append-only journal reuses "*the billing-ledger pattern*", the `typed_id` scheme, the project advisory lock, and the deploy/`.zship` artifact flow. A third-party tool cannot participate.

### 1.4 The project-umbrella model

The engine is built for a data model off-the-shelf tools don't contemplate:

- A **project** (`prj_…`) is the umbrella over resources: one **db**, one **kv**, one **storage**, and *one or more* **apps**.
- Apps in a project **share** the DB. A `storefront` and a `storebackend` app hit the same `products`/`orders` tables.
- **Schema is the union of all member apps' declarations.** Each app `export default { schema }` declares the tables it owns; the project DB schema is the *merged union*.
- **Declare vs use.** *Using* a table (read/write rows) is always shared and free. *Declaring* a table (its structure) is ownership: one owner per table; identical re-declaration is idempotent; a conflicting declaration is a deploy error.

This drives three engine features Flyway has no notion of: (a) **UUIDv7 versioning** so concurrent app deploys don't collide; (b) the **per-project advisory lock** so concurrent deploys serialize and the second no-ops already-applied migrations; and (c) **per-table ownership + `depends_on` ordering** for app-B's FK pointing at app-A's table. Removing an app never auto-drops its tables.

### 1.5 Where it sits in the platform

`zeroship-migrate` is the surface a **caller (control plane / CLI / builder)** drives (`engine.rs:4`). The pipeline is a single spine (`lib.rs:21-25`):

```text
author -> plan (lint) -> gate (approval) -> executor::apply (guard + role)
```

1. **Author** (`MigrationAuthor` seam): `DeterministicAuthor` handles the trivial additive set with no AI; `RawSqlAuthor` is the **AI-author hook** — the AI generates complex migrations *externally* and this engine validates + executes them. **The engine never calls an LLM** (`lib.rs:27-32`): AI output is untrusted input to the gate, not a privileged actor.
2. **`MigrationEngine::plan`** runs the guard read-only (no DB) and returns a `MigrationPlan` — the dry-run/preview (`engine.rs:11-17`).
3. **`MigrationEngine::apply`** is the gate: refuses any denial, refuses destructive-without-`Approval::Approved`, else delegates to the executor (`engine.rs:18-20`).

**Integration touchpoints:** the control plane owns app CRUD/deploy/route-registry and runs migrations "*out of band*" (`crates/zeroship-control/src/main.rs:1292`, `registry.rs:121`); the migration set ships in the `.zship` bundle as an immutable, replayable artifact. The `@zeroship/db` fold (`gen-types`) makes the migration set **the single source of truth for the schema** — the typed `env.db` surface is *generated from it* (see [§5](#5-authoring-declarative-desired-state--the-fold) and [§11](#11-platform-self-hosting--build-integration)).

### 1.6 The dialect-neutral, checksummed IR philosophy

The engine's portability and integrity both rest on a **single canonical intermediate representation** — a frozen, dialect-neutral wire shape (full treatment in [§6](#6-the-ir--its-wire-contract)):

- **One IR, one recorder.** The TypeScript authoring surface and recorder live in `packages/zero-migrate/src/`, published as `@zeroship/migrate`. The engine CLI and Vite plugin consume that package's internal recorder export, so authoring and draining share one ambient singleton. The canonical IR shape is the frozen contract.
- **The IR is a closed discriminated union.** `MigrationIr` + the closed `Op` enum derive `schemars` JSON Schema, emitting `op-ir.schema.json` — the discriminated union the JS builder targets.
- **Canonicalization defends the checksum.** Because `checksum = hash(up + down)` (or `Checksum::of_ir` over the neutral op-list) is the tamper-evidence anchor, the IR serializes *canonically* — `IrScalar::Bytes` stored decoded and re-encoded canonical base64; declared column ORDER preserved through the fold.
- **Dialect-neutral, engine-owned rendering.** One script lowers per-dialect to Postgres, SQLite, and MySQL; the author describes DDL/DML once and the engine owns 100% of per-dialect rendering. A construct with no native realization on a target **fails closed** at validate (`DIALECT_UNSUPPORTED`), never silently degrading.

### 1.7 Standalone product vs. managed server

The engine is a **generic, standalone-capable engine** with a **thin managed profile** layered on by call-site — trust separation is the call-site invariant, not tool separation:

- The creator-migration engine's roles have **zero** access to `control`/`auth`/`billing`.
- The platform's *own* DB **also runs on `zeroship-migrate`**, under the **Platform** trust profile, from the committed JS DSL corpus in `db/migrations-ts/`. There is *no legacy platform SQL source*.
- The Platform profile is constructible **only at a trusted capability call site** (gated by an `OperatorCapability` token); the creator submission ingress is hard-wired to **Confined** with *no API path to Platform*. This is mechanically enforced by the `standalone-cli` Cargo feature and `compile_fail` doctests ([§2.8](#2-crate-architecture), [§10](#10-security-first-design)).

### 1.8 The "JS DSL is the sole migration source — no raw SQL / no Flyway" stance

Both an authoring mandate and a security property. The platform's own schema has **no** hand-authored SQL/Liquibase — the JS DSL corpus (`db/migrations-ts/`) is the whole source (`AGENTS.md:42`). On the creator surface there is **no raw SQL** — no `Raw` type, no ``sql`` escape, no string fragments; every transform/predicate is a closed `Expr` AST and the engine owns rendering (`migrate-op-dsl.md:22-27`, "property A"). The gated `raw({ sql, reason })` escape is reserved for trusted platform use, carries its `reason` inside the checksummed IR, and is counted against a committed baseline.

A migration module separates structural and data intent. `schema()` records DDL
and receives an engine-synthesized structural inverse. `data()` records DML and
must carry either a separately recorded `inverse()` or a non-empty
`irreversible` reason. This makes rollback posture explicit and checksummable.

### 1.9 Summary — the "why" in one table

| Design pressure | Off-the-shelf (Flyway/Liquibase) | `zeroship-migrate`'s answer |
| --- | --- | --- |
| Migrations by **untrusted creators + prompt-injectable AI** | assume trusted DBA; no adversarial parse | real-PG-parser `SqlGuard` deny-list (Line 1) + least-priv `migrator` role (Line 2), re-run in the executor |
| **Zero-tokio, in-stack** platform invariant | JVM / generic async driver | native `compio-postgres` apply path; synchronous guard |
| **Concurrent multi-app authoring** into one shared DB | sequential-int versions collide | UUIDv7 (`mig_…`) total order + per-project advisory lock + `depends_on` |
| **Tamper-evidence + drift detection** | mutable/optional history | append-only immutable-trigger journal; `hash(up+down)` re-verified at apply |
| **Multi-dialect from one script** | dialect-specific SQL files | dialect-neutral canonical IR; engine owns 100% rendering; fail-closed off-target |
| **Schema = source of truth for typed `env.db`** | no SDK/type integration | `gen-types` folds IR → `schema.runtime.json` + `env.db.ts` |
| **Trust separation without tool separation** | separate installs | one engine, `OperatorCapability`-gated Platform profile vs hard-wired Confined creator ingress; `standalone-cli` feature-gated |
| **No back-door SQL** | encourages raw SQL | closed op DSL is the sole creator source; trusted raw use requires `raw({ sql, reason })` and is counted |

**Key files:** `lib.rs:1-101` (charter + security stance), `engine.rs:1-27` (public pipeline), `Cargo.toml:7-40` (feature gates + IR/parser rationale), `guard/mod.rs` (parse-time deny-list), and `apply/role.rs` (least-privilege role).

---

## §2 Crate architecture

The former crate's own one-line `Cargo.toml` self-description:

> "zeroship's versioned DB migration engine — native Postgres fast path plus V8-backed JS authoring front-end"

That sentence *is* the architecture: one engine with two personalities — a native, zero-tokio SQL executor on the deploy/apply fast path, and a V8-backed JS/TS authoring front-end bolted onto the same crate.

### 2.1 The big picture: one crate, three layers

The public modules declared in `src/lib.rs:67-82` fall into three conceptual layers:

| Layer | Directories | Role |
| --- | --- | --- |
| **model** (data / IR / policy kernel) | `src/model/` | Wire types, the closed `op.*` IR, the expression AST, the structural validator, snapshots, policy/trust profiles. Pure, sync, DB-free. |
| **apply** (executor / backends) | `src/apply/`, `src/conn.rs`, `src/engine.rs`, `src/approval.rs`, `src/command/`, `src/ops/` | The journal, the least-privilege role, the confined apply flow, drift/tamper checks, and the `MigrationBackend` dialect seam (Postgres / SQLite / MySQL). |
| **frontend** (JS authoring pipeline) | `src/frontend/`, `src/render/`, `src/plan/`, `src/analysis/`, `src/guard/` | Evaluate creator `schema.js`/migration `.ts` in a sandboxed V8 child, record into IR, lower to migrations, lint, and preview. |

The top-level `mod.rs` files carry no doc comment — they are bare `pub mod` re-export shims; responsibility statements live in the leaf files. The crate root frames itself as the **security core + migration unit** (§2.1 data + §1.4/§1.5 guard), the **Postgres executor** (§2.3 journal, advisory lock, transactional/two-phase apply), and the **public authoring pipeline + engine API** (`src/lib.rs:9-19`).

### 2.2 `src/model/` — the data & IR kernel

| File | Responsibility |
| --- | --- |
| `model/ir.rs` | The portable `op.*` migration **IR**: `MigrationIr`, the closed `Op` enum, `IrColumn/IrConstraint/IrIndex/IrDefault/IrScalar/IrValue`, `CURRENT_IR_VERSION`. Property A: no `Raw`/`RawDown`. |
| `model/expr.rs` | The **closed expression AST** (`Expr`, `BinaryOp`, `ScalarFn`, `CastTarget`, …). Constructed in JS, serialized as data, "NEVER parsed from text." |
| `model/validate.rs` | The **structural expression-AST validator** + `AuthoringError` envelope + `CODE_*` error constants. "No parser, no fuzzer — a pure allow-list walk." |
| `model/migration.rs` | Migration unit + value types: `Migration`, `MigrationId`, `Checksum`, `MigrationFlags`, `migration_id_for_version`. |
| `model/snapshot.rs` | Schema-shape snapshot types: `SchemaSnapshot`, `TableSnapshot`, `ColumnSnapshot`, `IndexSnapshot`, `ConstraintSnapshot`. |
| `model/load.rs` | The fail-closed `.ir.json` **load gate**: deserialize → `ir_version` → `validate_ir` → ownership stamp → checksum-hint compare. |
| `model/policy.rs` | Policy value types shared by model validation and the SQL guard (`SchemaScope`, `TrustProfile`). |
| `model/profile.rs` | Declarative **policy profiles** + sealed apply profiles (`PolicyProfile`, `SealedProfile`, `PolicyMeet`, `CONFINED_PROFILE_TOML`, `PLATFORM_PROFILE_TOML`, `seal_effective_profile`). |
| `model/capability.rs` | The VENDOR capability-composition policy for privileged root-exported Postgres primitives. |
| `model/table_shape.rs` | Resolve profile-managed table shape into explicit `createTable` IR (`resolve_create_table_policy`). |
| `model/backfill.rs` | Pure data for large-table backfill plan steps (`BackfillSpec`). |
| `model/precondition.rs` | Precondition declaration data (`Precondition`, `CmpOp`, `OnUnmet`, `PreconditionCheck`). |
| `model/probe.rs` | Pure data carried by executor-side existence probes (`GuardProbe`, `ExpectColumn`). |
| `model/support.rs` | Static support declarations for the op DSL (consumed by validation before expression walking). |
| `model/dialect_table.rs` | **GENERATED** — do not hand-edit; generated from `dialect-support.toml`. |

A key decision (`Cargo.toml:43-52`): the model layer **adopts `zeroship-schema`** for its schema-description layer (the DSL→SQL type map, DDL vocabulary, sentinel codec, shape enums) instead of re-implementing a v1-subset. That reuse gives the declarative differ full type capability (vector/encrypted/mask/geoPoint/literal). The engine's *lifecycle* (journal/guard/executor/rollback) and its snapshot-diff/introspection stay engine-owned on top of `zeroship-schema`.

### 2.3 `src/apply/` — the executor & the `MigrationBackend` dialect seam

| File | Responsibility |
| --- | --- |
| `apply/executor.rs` | "The heart of the engine." The versioned apply flow: transactional + two-phase non-txn apply with idempotent recovery, guard wired in front of every `up`, drift/tamper checks. Exports `apply`, `rollback`, `ApplyError`, `ApplyOutcome`, `LockMode`, `RollbackTarget`. |
| `apply/journal.rs` | The **journal** — `schema_migrations`. Append-only + tamper-evident. `ensure_journal`, `record_started/completed/rolled_back`, `applied`, `history`, `Phase`, `Resolution`, `PendingContract`. |
| `apply/role.rs` | The least-privilege per-project **`migrator` role** (line-2 defense). `provision_migrator`, `deprovision_migrator`, `migrator_role_name`. |
| `apply/drift.rs` | Read-only **drift detection**. `snapshot_schema`, `diff_snapshots`, `check_checksum_drift`, `ChecksumDrift`. |
| `apply/baseline.rs` | Baseline an existing project DB — the adoption path. |
| `apply/precondition.rs` | Preconditions — state/data-conditional apply. `evaluate`, `PreconditionError`. |
| `apply/backend/mod.rs` | The **`MigrationBackend` dialect seam**. |

**The `MigrationBackend` trait** is the crate's most important structural seam. The apply/rollback *orchestration* (partition versioned vs repeatable, drift/tamper gate, squash/expand gates, FIRST/SECOND pass, reverse-topo rollback ordering) is dialect-agnostic and single-sourced in `executor.rs`. Everything dialect-coupled lives behind the trait: connection/session I/O (the `pg_advisory_lock`, GUC snapshot/restore, `RESET ROLE`, txn begin/commit/rollback), the per-migration confined apply, journal row I/O (exposed as dialect-neutral owned structs like `AppliedEntry`, never a raw `compio_postgres::Row`), non-txn idempotency validation, and drift introspection. The trait uses **static dispatch** (`<B: MigrationBackend>`) so native `async fn`-in-trait is used directly — no boxing, no `dyn`, no `async-trait` allocation. Three live implementations:

- **`apply/backend/postgres.rs`** (+ `postgres/{shadow,online,backfill}.rs`) — talks to PG over `compio-postgres` (native, zero-tokio). Adds shadow-DB dry-run (`PgShadow`), online expand-contract (`PgOnline`), bounded backfill.
- **`apply/backend/sqlite/`** — `mod.rs`, `actor.rs` (single-writer flume queue), `authorizer.rs`, and the `*_sql.rs` renderers. In-process `rusqlite` on a dedicated hardened CDC-free connection.
- **`apply/backend/mysql/`** — `mod.rs`, `session.rs`, and the `*_sql.rs` renderers (`journal_sql`, `backfill_sql`, `drift_sql`, `identity_sql`, `primary_key_sql`). Live MySQL rides the dialect-neutral `driver::SqlSession` seam, exactly as Postgres does; the `SqlSession` impl is supplied by the host, which reaches the server through the real `mysql2` npm driver in the Node process. The engine itself opens no socket and embeds no V8.

The `apply` error type deliberately avoids naming a driver: `BackendError` boxes any `Error + Send + Sync` and lets callers `downcast_ref::<compio_postgres::Error>()` for a SQLSTATE when needed; non-PG backends surface dialect-neutral text through `ApplyError::Backend(String)`.

### 2.4 Authoring runs host-side, not in this crate

The design is Atlas-shaped: *many schema front-ends -> one internal
representation (`CollectionDescriptor` IR) -> one diff/migrate engine.* What
changed is where the front-end runs. `src/frontend/` no longer exists in the
engine: there is no V8 embedding, no sandboxed recorder child and no vendored
`mysql2` bundle in the Rust build graph. The host recorder evaluates the
`t.*` / `op.*` DSL in the Node process and hands the engine an op-IR envelope,
which is where this crate picks the pipeline up.

Supporting modules complete the pipeline: `render/lower.rs` (`IrAuthor` — the DDL Lower phase, [§8.5](#8-one-ir-three-dialects-render--portability)), `render/declarative.rs` (desired-schema differ, [§5](#5-authoring-declarative-desired-state--the-fold)), `render/fold.rs` (the offline ops→snapshot fold), `render/sql_preview.rs` (offline `--sql` preview), `plan/loader.rs` (Flyway-style file loader), `plan/manifest.rs` (Atlas-`atlas.sum`-style integrity manifest, [§9.11](#9-the-apply-engine--durability)), `analysis/analyze.rs` (Atlas-style advisory lint, [§7.9](#7-the-validate-gate--error-taxonomy)), `analysis/classify.rs` (statement classification through the real PG parser), and `guard/mod.rs` (the parse-time deny-list — [§10](#10-security-first-design)).

### 2.5 The engine does NOT depend on `zeroship-runtime`, and embeds no V8

`crates/zeroship-migrate/Cargo.toml` declares no zeroship
dependency at all, and its own package description states the engine ships no
embedded V8. Both of the jobs a V8 dependency used to do now run in the Node
process instead:

1. **The JS authoring front-end.** Creators still author schema in the
   `@zeroship/db` `t.*` / `op.*` DSL as JavaScript, but the host recorder
   evaluates the DSL and hands the engine an op-IR envelope. The engine
   consumes IR; it does not execute JS.
2. **The MySQL and Postgres drivers.** The engine ships no network driver of
   its own for either. Both go through the dialect-neutral
   `driver::SqlSession` seam, whose production implementation is the
   `zeroship-migrate-node` napi bridge over the host `pg` / `mysql2` npm drivers.
   SQLite is the exception and runs in-process on a `rusqlite` actor.

Because the engine opens no socket, it names no egress policy type. The
zeroship-side raw-stream policy (`zeroship_runtime::NetPolicy`) is a rule set
of verdict/destination/port rules with no `allowlist` constructor and no
`HostPort`; the vendored `net_policy.rs` inside the engine is a separate,
self-contained copy that nothing in this repo's crates reads.

### 2.6 The compio-postgres native fast path (zero tokio)

`Cargo.toml:61-65`: the Postgres executor talks to PG over the bespoke compio-native driver — ZERO tokio, runs out-of-band at deploy (not the request hot path), async on compio. `compio_postgres` is used across `conn.rs`, `apply/role.rs`, `apply/baseline.rs`, `apply/precondition.rs`, `apply/backend/postgres.rs`+`shadow.rs`, `engine.rs`, `command/runner.rs`, `ops/status.rs`, `test_support.rs`. The SQLite leg uses `rusqlite` (`bundled`, `load_extension` — pinning SQLite 3.51.3, `load_extension` enabled *only* to call `Connection::load_extension_disable()`); the writer uses `flume` for its single-writer actor queue. The guard runs **out-of-band and fully synchronous** — no tokio/compio, exhaustively unit-testable without a database (`lib.rs:63-65`).

### 2.7 Dependencies and their rationale

From `Cargo.toml:14-100` (the comments are unusually explicit about *why*): `pg_query = "6"` (real Postgres parser via libpg_query C bindings — a pure-Rust parser is a security gap); `sha2`/`hmac`/`hex` (checksums/tamper-evident journal); `schemars` (JSON-Schema derive → `op-ir.schema.json`); `base64` (strict decode + canonical padded re-encode for `IrScalar::Bytes` so two encodings can't hash differently; refuses invalid alphabet/padding at LOAD, before checksum); `indexmap` (`fold_to_field_defs` must preserve `createTable` column *declared order*); `zeroship-core` (typed_id); `zeroship-schema` (adopted schema layer); `zeroship-runtime` (V8); `uuid` (UUIDv7 version→id); `compio-postgres`+`compio` (zero-tokio PG); `rusqlite`+`flume` (hardened SQLite); `futures` (`FutureExt::catch_unwind` for shadow-DB teardown); `tracing`; `clap` (`env` feature, sync CLI); `toml` (`zeroship-migrate.toml`); `zeroship-bundle` (`.zship` manifest entry + hash validation); `v8`; `libc` + Linux-only `seccompiler`/`landlock` (kernel sandbox for the recorder child). Dev-only: `compio`, `tempfile`, `rcgen`.

### 2.8 Feature flags & the standalone/managed split

There is exactly **one** flag beyond `default = []` (`Cargo.toml:7-12`):

```toml
[features]
default = []
standalone-cli = []   # raw standalone apply/runner for user-owned DBs; server/control MUST NOT enable
```

`standalone-cli` gates the raw standalone apply surface for user-owned databases; `resolver = "3"` prevents dev/test feature use from leaking into embedder binaries. The crate exposes these feature flags:

- `apply_standalone` is only exported under the feature (`lib.rs:254-255`, `command/ir_apply.rs:408`). `lib.rs:84-101` asserts the *absence* via `compile_fail` doctests: in a default build, `zeroship_migrate::apply_standalone`, `command::runner::RunProfile::Trusted`, and `guard::GuardConfig::trusted` must **not** compile.
- The `zeroship-migrate` operator CLI bin requires it (`required-features = ["standalone-cli"]`) — the only place a `PlatformCapability`/`OperatorCapability` token is minted.
- `profile_allows_load` returns `true` for `RunProfile::Trusted` only under `#[cfg(any(test, feature = "standalone-cli"))]`, `false` otherwise (`command/runner.rs:1751-1760`).

The takeaway: in a normal embedder (worker/control) build, the "Trusted" / user-owned-DB profile is **compiled out entirely**.

### 2.9 Binaries in the former in-tree layout

| Bin | Path | Notes |
| --- | --- | --- |
| `zeroship-migrate` | `src/bin/zeroship-migrate.rs` | Operator CLI. `required-features = ["standalone-cli"]`. `#[compio::main]` (NOT tokio). |
| JS authoring CLI (removed) | former JS CLI source | Vite/PATH-facing authoring entry point; always built in the former layout. |
| Recorder-child executable (removed) | `src/bin/recorder-child.rs` | Kernel-sandboxed recorder child — one child per tenant in the former layout. |

### 2.10 Public API surface

`src/lib.rs:103-303` flattens the deep tree into a flat `zeroship_migrate::*` namespace. Major clusters: analysis/lint (`analyze`, `Advisory`, `Severity`, `classify`); backends (`MigrationBackend`, `PostgresBackend`, `SqliteBackend`, `MysqlBackend`); authoring (`MigrationAuthor`, `DeterministicAuthor`, `RawSqlAuthor`); declarative render (`desired_snapshot`, `DeclarativeAuthor`, `CollectionDescriptor`, `dsl_to_pg_data_type`, `SqliteRebuild`); engine (`MigrationEngine`, `MigrationPlan`, `EngineError`, `DeclarativeDeployPlan`); connection/config (`connect`, `ExecutorConfig`, `PgConfinement`); drift/journal (`snapshot_schema`, `diff_snapshots`, `ensure_journal`, `record_*`, `AppliedEntry`, `Phase`, `PendingContract`); the IR + expr AST + validator (`MigrationIr`, `Op`, `IrScalar`, `Expr`, `validate_ir`, all `CODE_*`, `CURRENT_IR_VERSION`); load gate + lower (`load_ir_document`, `enforce_ir_ownership`, `IrAuthor`, `LoweredArtifact`); plan/preview/apply (`AppliedPlan`, `PlanStep`, `render_plan_sql`, `apply`, `rollback`, `RollbackTarget`); policy/profile/guard (`SchemaScope`, `TrustProfile`, `PolicyProfile`, `SealedProfile`, `SqlGuard`, `GuardConfig`, `MigrationGuard`); IR-apply commands (`apply_bundle_ir_postgres/sqlite`, `discover_ir_files`, feature-gated `apply_standalone`); ops/lifecycle (`squash`, `status`, `history`, `submit_migration`, `compute_manifest`/`verify_manifest`). One dialect enum is re-exported from a dependency: `pub use zeroship_schema::query::SqlDialect` (`lib.rs:182-183`).

### 2.11 End-to-end connectivity

```text
1. AUTHOR (JS/TS, frontend/)  →  2. RECORD → IR (model/ir.rs)  →  3. LOAD GATE + VALIDATE (model/load.rs + validate.rs)
   →  4. LOWER (render/lower.rs — IrAuthor)  →  5. PLAN / GATE (engine.rs + approval.rs + guard/)
   →  6. APPLY (apply/executor.rs via MigrationBackend: PostgresBackend | SqliteBackend | MysqlBackend)
```

The crate root's compressed form is `author -> plan (lint) -> gate (approval) -> executor::apply (guard + role)` (`lib.rs:23-25`), with the explicit note that the executor **independently re-runs the guard + the migrator role** — the engine gate is an additional check, not a replacement (`lib.rs:38-40`).

---

## §3 Authoring: schema-structure DSL

This section documents the TypeScript authoring surface a creator imports to describe schema changes: the migration-module shape, the fluent `table()`/`view()` handles, the `t.*` column lexicon, facets/defaults, constraints, indexes, enums/domains/sequences/schemas/extensions/roles, views, triggers, and partitions. Verified against `packages/zero-migrate/src/types.ts` (manual authoring types) and `packages/zero-migrate/src/ops.ts` (recorder implementation), cross-checked against `docs/reference/migrate-dsl-examples.md`. The **expression sublanguage** (`(col) => Expr`) is owned by [§4](#4-authoring-the-expression-sublanguage); this section shows only which builder tier each slot receives and cross-refs §4 for node-level detail.

### 3.1 Architecture in one paragraph

`packages/zero-migrate/src/ops.ts` is the one operation producer, and
`src/internal/recorder.ts` is the host seam exported as
`@zeroship/migrate/internal/recorder`. Code imported by a migration and code
drained by the host therefore share one module instance. It emits the
dialect-neutral op objects that the closed Rust `Op` enum and IR schema
deserialize. Every terminal records eagerly and synchronously and returns its
handle, so handles are reusable and chainable.

### 3.2 One import root

| Import | Scope | Runs on |
| --- | --- | --- |
| `@zeroship/migrate` | Tables, columns, constraints, indexes, expressions, enums, views, partitions, triggers, core DML, domains, sequences, schemas, extensions, roles, grants, functions, RLS/policies, vendor index options, `raw` | PG first; non-PG targets fail closed where no native realization exists |

The former `/pg` root is retired. `table()` is the one table handle, and public vendor names (`domain`, `schema`, `extension`, `role`, `sequence`, `grant`, `revoke`, `createFunction`, `dropFunction`, `dropOwnedBy`, `raw`) are exported directly from `@zeroship/migrate`; the internal `__pg*` factories remain implementation hooks in `ops.ts`. The import path is **not** a security boundary: confined creator deploys reject privileged vendor ops with `VENDOR_OP_DENIED`; operator/platform callers pass an explicit trusted capability. See [§3.15](#315-the-postgres-vendor-authoring-surface) for the JS-authoring surface, and [§10.4](#10-security-first-design) for the capability gate.

### 3.3 Migration module shape

A migration is a `.ts` module. The typed contract is `Migration`:

```ts
type Migration =
  | { name?: string; schema(): void }
  | { name?: string; data(): void; inverse(): void }
  | { name?: string; data(): void; irreversible: string };
```

Each phase is parameterless and authors against the ambient recorder. A schema
module contains DDL only; a data module contains DML only:

```ts
import { now, table, t, uuidV4 } from "@zeroship/migrate";

export default {
  name: "create_users",
  schema() {
    table("users").create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        email: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
  },
};
```

Names are plain strings, **never live-schema-bound** (`types.ts:8`, the all-strings typing stance).

### 3.4 The recorder lifecycle & structured errors

The build evaluator drives the recorder: `__begin(phase)` opens a fresh buffer
and `__drain()` returns the op list and clears it. Schema, data, and inverse are
recorded in independent passes. Two properties matter:

1. **Recording outside a recorder throws `OP_OUTSIDE_RECORDER`** — a `table()` handle may only be used synchronously inside the active phase.
2. A selector handed out but never terminated is a hard `SELECTOR_NOT_TERMINATED` at drain (not eagerly, so a var-held selector terminated on a later line is fine) (`ops.ts:305-324`); terminating twice throws `SELECTOR_ALREADY_TERMINATED` (`ops.ts:476-490`).

Author-facing structured error codes emitted by this surface: `OP_INVALID` (any arg/shape failure), `OP_OUTSIDE_RECORDER`, `SELECTOR_NOT_TERMINATED`, `SELECTOR_ALREADY_TERMINATED`, `EXPR_NOT_PORTABLE` (`.splitPart()` grammar), `NONDETERMINISTIC_OP_ARG` (a lint finding, not a throw). Engine-side codes (`VENDOR_OP_DENIED`, off-target `EXPR_NOT_PORTABLE`/`DIALECT_UNSUPPORTED`) fire in the Rust validator ([§7](#7-the-validate-gate--error-taxonomy)).

The **op-producer registry** (`defineOp(kind, producer, { deferrable })`, `ops.ts:358-465`) is a discipline mechanism asserting one-producer-per-op-kind. The one multi-producer kind is `addConstraint`, minted five times (`addColumn.unique`, `foreignKey`, `unique`, `check`, `exclusion`).

### 3.5 The `t.*` ColType lexicon (full enumeration)

`t` is the physical `TypeLexicon` of immutable factories, each returning a
chainable `ColumnDef`. `ids` is the separate validated-text-format lexicon for
TypeID and ULID columns. The universal-ID shortcut, untyped-reference factory,
loose `integer` alias, and `{notNull,default}` options-bag overload are removed.
Every factory returns a fresh `ColumnDefImpl`; the emitted wire `ColType` plus
its optional facets are what the engine renders per dialect.

| `t.*` factory | Options | Wire `ColType` | Per-dialect intent |
| --- | --- | --- | --- |
| `t.text(opts?)` | `{ caseSensitive }` | `text`; facet if `caseSensitive:false` | PG `text`; `false` → citext / `COLLATE NOCASE` / `_ci` |
| `t.string(opts?)` | `{ length?, caseSensitive? }` | bounded string | `VARCHAR(N)` on PG/MySQL; SQLite `TEXT`; length defaults to 255 |
| `t.textArray()` | — | `textArray` | PG `text[]` / SQLite `TEXT` / MySQL `JSON` |
| `t.numeric(opts?)` | `{ precision, scale }` default **(38, 9)** | `{ decimal: { precision, scale } }` | `NUMERIC(p,s)` |
| `t.char(opts)` | `{ length }` **required** | `{ char: { length } }` | `CHAR(n)` |
| `t.timestamp()` | — | `timestamp` | timestamp |
| `t.date()` | — | `date` | SQL DATE (validates as PG domain base type only) |
| `t.uuid()` | — | `uuid` | uuid |
| `t.bytes()` | — | `bytes` | `bytea` / blob |
| `t.boolean()` | — | `boolean` | boolean |
| `t.json()` | — | `json` | `jsonb` |
| `t.vector(opts)` | `{ dimensions, metric? }` **dims required** | `{ vector: { vector: n } }`; `vectorMetric` facet | pgvector / sqlite-vec; metric ∈ closed set |
| `t.geoPoint()` | — | `geoPoint` | spatial point |
| `t.smallInt()` | — | `smallInt` | int2 |
| `t.int()` | — | `int` | int4 (canonical integer spelling) |
| `t.bigInt()` | — | `bigInt` | int8 |
| `t.real()` | — | `real` | float4 |
| `t.double()` | — | `double` | float8 — **not** an alias of `t.real()` |
| `t.inet()` | — | `inet` | PG `inet` |
| `t.enum(name)` | `string \| EnumHandle` | `{ enum: { name } }` | references an enum type |
| `t.domain(name)` | `string \| DomainHandle` | `{ domain: { name } }` | references a Postgres domain |
| `t.encrypted(arg)` | `{ of } \| ColumnDef \| ColType` | `{ encrypted: { of: innerType } }` | app-level encrypted column |

Validated ID formats are ordinary text columns until the normal column facets
opt into constraints; neither helper supplies a key or database default:

| ID factory | Wire facet | Effect |
| --- | --- | --- |
| `ids.typeId({ prefix })` | `valueFormat: { typeId: { prefix } }` | TypeID 0.3 text validation; prefix may be empty and is at most 63 bytes |
| `ids.ulid()` | `valueFormat: "ulid"` | canonical ULID text validation |

Closed token sets validated client-side (friendly `OP_INVALID` before serde): `VECTOR_METRICS = ["cosine","l2","innerProduct"]` (`ops.ts:631`); `SEQUENCE_AS_TYPES = ["int","bigInt"]` (`ops.ts:634`); `MASK_KINDS = ["full","last4","first4","email","name","date-year","date-decade","none"]` (`ops.ts:642-651`); `MASK_CLASSIFICATIONS = ["public","pii","spi","phi","pci","internal"]` (`ops.ts:652-659`).

**Bridging from `@zeroship/db`:** `fromDb(field)` lifts a `@zeroship/db` `TypeBuilder`/`FieldDef` into a migration `ColumnDef` through the shared `colTypeFromDbField` reduction, carrying `.required()`→`.notNull()` and `.unique()` (`ops.ts:1518-1528`). Names are never bridged.

### 3.6 `ColumnDef` — facets & defaults

`ColumnDef` (`types.ts:193-224`, impl `ops.ts:661-868`) is **nullable by default and immutable** — every modifier returns a fresh def (`.with(...)`), so a hoisted type var is safe to reuse without aliasing.

| Facet | Signature | Effect |
| --- | --- | --- |
| `.notNull()` | `(): ColumnDef` | `NOT NULL` (`ops.ts:743`) |
| `.primaryKey()` | `(): ColumnDef` | table PK; **implies `NOT NULL`** (`ops.ts:749-751`) |
| `.unique()` | `(): ColumnDef` | single-column UNIQUE |
| `.references(table, column, opts?)` | `string`, `string`, `{ onDelete?, onUpdate?, name? }` | typed single-column FK **facet** (`IrColumn.references`): keeps this column's storage type and records the full target `{ table, column }`. Both names required (a missing target column is `OP_INVALID`); create-table only — an added/retyped/nested position rejects it |
| `.default(v)` | `DefaultValue \| DefaultExprFn \| ExprChain \| Expr` | structured default (never raw SQL) |
| `.mask(opts)` | `{ kind, classification? }` | column mask; `classification` defaults `"pii"`; `kind:"none"` opts out; overrides an encrypted column's auto-mask (`ops.ts:763-786`) |
| `.generated(expr, opts?)` | `expr`, `{ virtual? }` | computed column; omitted ⇒ STORED, `{virtual:true}` ⇒ SQLite VIRTUAL (rejected on PG) |
| `.identity(opts?)` | `{ always? }` | `GENERATED ALWAYS` if `always:true`, else `BY DEFAULT` |
| `.autoIncrement()` | `(): ColumnDef` | portable sugar for `.identity({ always: false })` |

Two lowering rules matter here: a column that is both `.unique()` and
`.primaryKey()` emits no separate UNIQUE; a `.references(...)` facet is
create-table-only, while `ids.typeId(...)` and `ids.ulid()` retain their
`valueFormat` facet on both create-table and add-column operations.

**Default forms** (resolved by `toIrDefault`, `ops.ts:1189-1214`):

```ts
t.bigInt().default(0)            t.text().default("pending")      t.boolean().default(true)
t.uuid().default(uuidV4())   t.timestamp().default(now())   // function defaults are expressions
t.json().default({})   t.json().default([])   t.textArray().default([])   // empty-container defaults
t.json().notNull().default({ max_sockets: 4, egress_ceiling_bytes: 10485760 })  // arbitrary jsonb VALUE — integers only in v1
t.bigInt().notNull().default(nextval("orders_id_seq", { schema: "zeroship" }))   // sequence-backed (PG vendor)
```

Scalars pass through `toIrScalar` (`ops.ts:949-967`): branded `decimal("...")` → `{decimal}`, `byteValue(...)`/`Uint8Array` → `{bytes:base64}`, a non-integer JS number → `{decimal}`, a `bigint` throws. JSON defaults accept integers only (`|v| < 2**53`). Column defaults are validated immutable/non-volatile by `validateDefaultExpr` ([§4.7](#4-authoring-the-expression-sublanguage)). Value constructors exported for defaults/expressions are top-level imports: `now()`, `uuidV4()`, `uuidV7()`, `currentSetting(name, {missingOk?})`, `currentUser()`, `interval(duration)` (`ops.ts:1241-1273`), `nextval(name, {schema?})`, `decimal(str)`, `byteValue(bytes)`, `lit(value)`.

### 3.7 The fluent `table()` handle grammar

`table(name, opts?)` returns a reusable PG-first `TableHandle` carrying `{ name, schemaDefault }` (`types.ts`, impl `ops.ts`). It includes portable table operations and vendor table-scoped methods on the same handle; the Rust validator remains the capability/dialect security gate. Schema precedence: the `{ schema }` from `table()` is the default; a per-op `schema` overrides via `pickSchema`.

**Direct table methods:** `.create(args)` → `createTable`; `.drop({ifExists?, cascade?, schema?})` → `dropTable`; `.rename({to, ifExists?, schema?})` → `renameTable` (fast `ALTER TABLE … RENAME TO`, engine emits inverse as down); `.setOptions(args)` → `setTableOptions` (`{ softDelete?, versioning?, strictness? }`, must set ≥1); `.comment(text|null, {schema?})` → `comment` (`null` clears); `.partition(name)` → `PartitionRef`.

**Selector sub-handles:** `.column(name)`, `.foreignKey(name)`, `.unique(name)`, `.check(name)`, `.constraint(name)`, `.index(name)`, `.trigger(name)`, `.exclusion(name)`, `.policy(name)`, and `.setRls(...)`.

**Direct DML (no existence guard):** `.insert(args)`, `.update(args)`, `.delete(args)`, `.backfill(args)`.

#### `CreateTableArgs` — the all-object create payload

`create({...})` is fully declarative — table-level constraints/indexes are **fields**, each carries a required `name`, no `build` callback (`types.ts:1056-1118`):

```ts
interface CreateTableArgs {
  columns: Record<string, ColumnDef>;
  options?: TableRuntimeOptions;          // softDelete / versioning / strictness
  primaryKey?: string[] | null;           // undefined=policy default, null=no PK, [...]=explicit/composite
  uniques?: Array<{ name; columns }>;
  checks?: CheckDef[];
  foreignKeys?: Array<{ name; columns; references; onDelete?; onUpdate?; deferrable?; initiallyDeferred? }>;
  exclusions?: Array<{ name } & ExclusionConstraintArgs>;
  indexes?: Array<{ name; on; unique?; using?; where?; include?; with?; only?; nullsNotDistinct? }>;
  partitionBy?: PartitionByInput;         // { range | list | hash: string[] }
  ifNotExists?: boolean;
  schema?: string;
}
```

Apply-level lowering (`recordCreateTable`, `ops.ts:2663-2741`): `uniques`/`foreignKeys`/`indexes` lower to DDL on PG; `indexes` also lower on SQLite (plain btree) but table-level `uniques`/`foreignKeys` on SQLite are a hard authoring error; partial-index `where` renders on PG+SQLite but MySQL refuses it fail-closed. An explicit `primaryKey` wins over collected per-column `.primaryKey()`. Nothing is a silent no-op — unsupported specs fail closed at lower time. See [§8.3](#8-one-ir-three-dialects-render--portability) for the per-dialect disposition of each construct.

### 3.8 Per-intent column alters (`.column(name)`)

Selecting a column returns a `ColumnRef` (`types.ts:1122-1137`, impl `ops.ts:3774-3830`). **There is no `.alter({...})` bag** — each change is its own single-intent op:

```ts
table("users").column("bio").add({ type: t.text() });                        // addColumn
table("users").column("bio").drop({ ifExists: true });                       // dropColumn
table("users").column("bio").rename({ to: "biography", type: t.text() });    // renameColumn (carries post-rename type)
table("users").column("age").setType({ to: t.bigInt() });                    // setColumnType ({ using } for a cast expr)
table("users").column("email").setNotNull();                                 // setColumnNotNull
table("users").column("email").dropNotNull();                                // dropColumnNotNull
table("users").column("status").setDefault("active");                        // setColumnDefault
table("users").column("status").dropDefault();                               // dropColumnDefault
table("users").column("email").comment("primary contact");                   // comment
```

`.add({ type })` honors modifiers: `.unique()` emits a follow-on `addConstraint` unique (an ADD COLUMN has no inline UNIQUE); `.primaryKey()` records **no** pk op (PK is create-time only). Every terminal returns the parent `TableHandle`, enabling add-then-backfill:

```ts
table("users").column("first_name").add({ type: t.text() })
  .backfill({
    set: { first_name: (col) => col("name").splitPart(" ", 1) },
    cursorColumns: ["id"],
    cursorStability: { mode: "guardUpdates" },
  });
```

### 3.9 Constraints

All named constraints use the **selector form as the sole grammar** — the old `addForeignKey`/`addCheck` verb twins are deleted (`ops.ts:3832-3837`).

```ts
table("users").unique("users_email_key").add({ columns: ["email"] });
table("orders").check("orders_qty_positive").add({ expr: (col) => col("qty").gt(0) });
table("posts").foreignKey("posts_author_fkey")
  .add({ columns: ["author_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });
table("line_items").foreignKey("line_items_order_fkey").add({
  columns: ["order_id", "tenant_id"], references: { table: "orders", columns: ["id", "tenant_id"] },
  onDelete: "restrict", onUpdate: "cascade" });
table("reservations").exclusion("no_overlap").add({    // PG only — fails closed on SQLite/MySQL
  using: "gist", elements: [{ target: "room_id", operator: "=" }, { target: "during", operator: "&&" }], deferrable: true });
table("orders").constraint("orders_qty_positive").drop({ ifExists: true });   // kind-agnostic
table("orders").constraint("orders_fk").validate();   // PG-only: validate a NOT VALID constraint
```

- `RefAction` = `"cascade" | "restrict" | "setNull" | "setDefault" | "noAction"` (camelCase wire tags, rendered now, C1); emitted compacted so an action-free FK is byte-identical to the pre-C1 image.
- Cross-schema FKs are **not representable** in the frozen IR: `references.schema` must match the table schema or it throws `OP_INVALID` (`ops.ts:2971-2979`).
- `.foreignKey(...).add({ notValid: true })` / `.check(...).add({ notValid: true })` are PG-only online constraint adoption (add `NOT VALID`, then `.constraint(name).validate()` later).

### 3.10 Indexes

Select with `.index(name)`; `.add({...})` takes target elements plus modifiers. `IndexRef.add` accepts the full PG-first surface: `on`/`unique`/`ifNotExists`/`schema`, `using`, `where`, `include`, `with`, `only`, `nullsNotDistinct`, and per-element `order`/`opclass`/`collation`/`nulls`. Vendor options stay fail-closed at validate/render time on targets without a native realization.

```ts
table("app_members").index("app_members_user_idx").add({ on: ["user_id"] });
table("users").index("users_email_uq").add({ on: ["email"], unique: true });
table("posts").index("posts_created_desc").add({ on: [{ column: "created_at", order: "desc" }] });  // only "desc" serialized
table("users").index("users_lower_email").add({ on: [{ expr: (col) => col("email").lower() }] });
table("embeddings").index("embeddings_vec").add({ on: ["vec"], using: "hnsw" });
table("app_session_anchors").index("app_session_anchors_user_idx")
  .add({ on: ["app_id", "global_user_id"], where: (col) => col("revoked_at").isNull() });
table("orders").index("orders_customer_idx")
  .add({ on: ["customer_id"], include: ["total", "status"], with: { fillfactor: 90 }, only: true });
table("orders").index("orders_customer_idx").drop({ ifExists: true });   // drop args also support concurrently
```

- `IndexMethod` = `"btree" | "hash" | "gin" | "gist" | "spgist" | "brin" | "ivfflat" | "hnsw"`.
- `IndexStorageParams` (`with`) recognizes `pagesPerRange` and `fillfactor` (u32, compacted).
- On `.drop(...)`, `unique: true` is kept because `Op::DropIndex.unique` drives destructive/approval gating (dropping a unique index removes a data-integrity guarantee).

### 3.11 Enums, domains, sequences, schemas, extensions, roles

```ts
enumType("order_status").create({ values: ["pending","paid","shipped"], schema: "zeroship" });
enumType("order_status").comment("lifecycle of an order");   enumType("order_status").drop({ ifExists: true });
// empty values array throws OP_INVALID (ops.ts:2430-2435)

domain("account_state").create({ as: t.text(),                       // Postgres vendor
  check: (col) => col("VALUE").in(["active","past_due","suspended"]), schema: "zeroship" });
// domain CHECK may reference only the VALUE pseudo-column (ops.ts:2084-2103)

sequence("orders_id_seq").create({ schema: "zeroship", start: 1, increment: 1 });  // as ∈ {int, bigInt}
sequence("orders_id_seq").alter({ restart: 1000 });   sequence("orders_id_seq").drop({ ifExists: true });

schema("zeroship").create({ ifNotExists: true });   extension("vector").create({ schema: "zeroship" });
role("app_rw").create({ login: false });            // .setOptions({ setSearchPath, resetSearchPath }) / .drop
```

Handle interfaces: `EnumHandle` (`types.ts:286-291`), `DomainHandle` (`types.ts:306-311`), `SequenceHandle` (`types.ts:424-430`), `SchemaHandle`/`DroppedSchemaHandle`, `ExtensionHandle`, `RoleHandle`. Sequence numeric args are JS-safe-integer-validated (`increment` non-zero, `cache` ≥1, `minValue ≤ maxValue`); `as` must be `int`/`bigInt`.

### 3.12 Views

`view(name, opts?)` returns a `ViewHandle` (`types.ts:970-974`). `create({ as })` takes either the structured `ViewQueryBuilder` callback or a raw escape `{ raw: string }`:

```ts
view("active_users").create({ as: (q) => q.from("users").select(["id","email"])
  .where((col) => col("deleted_at").isNull()).orderBy(["created_at"]).limit(100) });
view("order_totals").create({ materialized: true, as: (q) => q.from("orders")
  .select(["customer_id", () => countStar(), (col) => col("amount").sum()])
  .where((col) => col("status").eq("paid"))
  .groupBy(["customer_id"])
  .having((col) => col("id").count().gt(5)) });
view("legacy_report").create({
  as: { raw: "SELECT a.id, percentile_cont(0.5) WITHIN GROUP (ORDER BY b.n) FROM a JOIN b USING (id) GROUP BY a.id" } });
```

`ViewQueryBuilder` methods: `from · select · join(kind,…) · innerJoin · leftJoin · where · groupBy · having · orderBy · limit`. `groupBy` takes column names or expressions; `having` is a grouped SELECT context, so aggregate expressions are valid there. `join` accepts only `"inner"`/`"left"`; `q.from(...)` is mandatory or `__selectAst()` throws.

### 3.13 Triggers

`.trigger(name)` returns a `TriggerRef`. `TriggerCreateArgs` is a discriminated union — exactly one of `{ execute: string }` (a function to EXECUTE) or `{ body: (b) => TriggerStmt[] }`:

```ts
table("app_audit", { schema: "zeroship" }).trigger("app_audit_block_delete").create({
  timing: "before",   events: ["delete"],   forEach: "row",   execute: "app_audit_block_tamper" });
```

`TriggerTiming` = `before | after | insteadOf`; `TriggerEvent` = `insert | update | delete | truncate`; `ForEach` = `row | statement`. The `body` builder yields `raise`/`insert`/`update`/`delete`/`select`; `raise` levels are the closed set `["abort","fail","ignore","rollback"]`.

### 3.14 Partitions

Authored from the parent handle: `table(parent).partition(child)` → `PartitionRef`. The parent declares strategy at create via `partitionBy: { range | list | hash: string[] }`; children declare bounds:

```ts
table("sandbox_events", { schema: "zeroship" }).create({ columns: {…}, primaryKey: ["id","occurred_at"],
  partitionBy: { range: ["occurred_at"] } });
table("sandbox_events").partition("y2026_05").create({ from: ["2026-05-01"], to: ["2026-06-01"] });  // RANGE
table("events").partition("events_eu").create({ in: ["de","fr","es"] });                             // LIST
table("events").partition("events_h0").create({ modulus: 4, remainder: 0 });                         // HASH
table("events").partition("events_default").create({ default: true });                               // DEFAULT
table("events").partition("events_head").create({ from: [minValue], to: ["2026-01-01"] });           // unbounded sentinel
table("sandbox_events").partition("y2026_05").detach();   // PG concurrently
table("sandbox_events").partition("y2026_05").drop();
```

`minValue`/`maxValue` are frozen sentinels. `PartitionBoundArgs` is exactly one of `{from,to}`, `{in}`, `{modulus,remainder}`, `{default:true}`.

### 3.15 The Postgres vendor authoring surface

`@zeroship/migrate` is also the privileged Postgres vendor authoring surface — platform/operator migrations can author closed vendor IR from the same root import and `table()` handle, but **the import path is not the security gate**. Confined creator deploy/load paths reject privileged vendor ops with `VENDOR_OP_DENIED`; operator callers must pass an explicit platform/trusted capability. Each method records the same op payload shape as before. Vendor value exports include `domain`, `schema`, `extension`, `role`, and `sequence`; the package also exports six free functions:

| Function | Args (interface) | Records | Validation |
| --- | --- | --- | --- |
| `dropOwnedBy({ roles })` | `roles: string[]` | `{ op:"dropOwnedBy", roles }` | `roles` must be an array |
| `grant({ privileges, on, to, withGrantOption? })` | `privileges: Privilege[]`, `on: GrantTarget`, `to: string[]` | `{ op:"grant", privileges, on, to, withGrantOption }` | `privileges` non-empty; `on` an object; `to` non-empty |
| `revoke({ privileges, on, from })` | as above, `from: string[]` | `{ op:"revoke", privileges, on, from }` | non-empty privileges/from; `on` an object |
| `createFunction({ name, returns, language, body, schema?, args?, replace?, volatility? })` | `language: FuncLanguage`, `args?: FuncArg[]`, `volatility?: FuncVolatility` | `{ op:"createFunction", … }` | `name`/`returns`/`language`/`body` must be strings |
| `dropFunction({ name, schema?, argTypes?, ifExists? })` | `argTypes?: string[]` | `{ op:"dropFunction", … }` | `name` must be a string |
| `raw({ sql, reason })` | `sql: string`, `reason: string` | `{ op:"pgRaw", sql, reason }` | both must be strings — the **required** audit `reason` is enforced client-side, and again as `PGRAW_REASON_REQUIRED` server-side |

Real corpus usage: `grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: [...] })` (`db/migrations-ts/20260702000900_grants.ts:6`); `createFunction({ body, language: "plpgsql", … })` and `raw({ sql, reason })` where the trigger DSL can't express a column-list UPDATE trigger (`..._functions_triggers_comments.ts:27`). All these ops carry a `VendorCapability` gated by the active profile — see [§10.4](#10-security-first-design). Every free function passes through `record → compact` which drops `undefined` keys so an omitted optional never perturbs the checksum.

### 3.16 DML

Available on any table handle (no existence guard — DML is unguardable):

```ts
table("plans").insert({ rows: [{ id:"free", name:"Free" }, { id:"pro", name:"Pro" }],
  onConflict: { columns: ["id"], doUpdate: { name: "Pro" } } });   // PG-only; SQLite target => hard build error
table("plans").update({ set: { name: (col) => lit("Professional") }, where: (col) => col("id").eq("pro") });
table("plans").delete({ where: (col) => col("id").eq("legacy") });  // where mandatory (no unfiltered delete)
table("users").backfill({ set: { display_name: (col) => col("nickname").coalesce(col("name")) },
  cursorColumns: ["id"], cursorStability: { mode: "guardUpdates" } });  // both required; batchSize defaults 1000
```

`insert` rows must be non-ragged; `onConflict` is PG-only (SQLite surfaces the structured envelope at build); `delete({ where })` throws `OP_INVALID` if `where` is missing.

### 3.17 The builder-tier map (which slot gets which restricted builder)

Every predicate/value position uses a closed expression builder, but the *tier* differs by position — deliberately, so a default cannot reference a column, an index predicate cannot use volatile functions, etc. The node-level enumeration of chain operators, top-level value constructors, and validate backstops is [§4](#4-authoring-the-expression-sublanguage); the tier map:

| Builder | Where | Column refs? | Scalar chain methods | Aggregates | PG-first nodes | source |
| --- | --- | --- | --- | --- | --- | --- |
| `ExprBuilder` | DML where/set, policy, view, trigger | yes | yes | yes (`.count`/`.sum`/…/`.stringAgg` plus `countStar()`) | yes; fail-closed off-target | `types.ts:652-656` |
| `DefaultBuilder` | column defaults | **no** | default-safe subset | no by Rust backstop | no | `types.ts:624-628`, `ops.ts:1085-1175` |
| `IndexExprBuilder` | index expr/predicate | yes | immutable subset | no by Rust backstop | no | `types.ts:624-635` |
| `GeneratedColumnBuilder` | generated columns | yes | immutable subset | no by Rust backstop | no | `types.ts:638-639` |
| `CheckBuilder` | table CHECK | yes | immutable subset | no by Rust backstop | allowed on PG, fail-closed off-target | `types.ts` |
| `DomainValueBuilder` | domain CHECK | VALUE only | immutable subset | no by Rust backstop | allowed on PG | `types.ts:676-685` |

### 3.18 Determinism lint

`lintDeterminism(source)` is a best-effort whole-source regex scan flagging `Date.now()` / `Math.random()` / `crypto.randomUUID()` / `new Date(...)` leaking into recorded values; returns `DeterminismFinding[]` (code `NONDETERMINISTIC_OP_ARG`), warnings only, never a hard reject (`ops.ts:4059-4091`). The bare native symbols (no parens) are the opt-in to DB-side evaluation.

### 3.19 Companion examples

`docs/reference/migrate-dsl-examples.md` is the cookbook companion for this reference. This guide is the normative source for argument shapes and method names: column-type factories use their documented options objects, table runtime options are set with `.setOptions({...})`, and row deletion is authored with `.delete(args)` (wire tag `"delete"`). Postgres vendor helpers are documented from the JS-author side in [§3.15](#315-the-postgres-vendor-authoring-surface).

---

## §4 Authoring: the expression sublanguage

The migration DSL never accepts raw SQL in an expression position ("property A"). Every `where`, `check`, generated-column, index-predicate, trigger `when`, RLS `using`, default, and `update.set` value is authored as a **closed expression AST** — either a `(col) => Expr` callback that receives an injected builder handle, or a pre-built chain value / top-level value constructor. Citations in this section are relative to `packages/zero-migrate/src/`. The Rust mirror of every node is [§6.3](#6-the-ir--its-wire-contract); the validate-time gate that walks it is [§7](#7-the-validate-gate--error-taxonomy).

### 4.1 The two authoring shapes

An expression slot accepts three JS forms, resolved by `resolveExpr` (`ops.ts:2039`) / `resolveImmutableExpr` (`ops.ts:2049`): (1) a **`(col) => Expr` callback** (`col` is the injected `ExprBuilder`; `resolveExpr` calls `slot(makeBuilder())` and unwraps via `exprArg`); (2) a **pre-built `ExprChain`** (e.g. `now()` or a saved chain value passed directly → `slot.__node`); (3) a **raw closed `Expr` node** (an object with a string `node` field — the escape for machine-generated IR). `ExprChainImpl` (`ops.ts:1702`) is the sole runtime class; it wraps `__node` and every method returns a fresh `chain(...)`, so chains are immutable and reusable.

### 4.2 The builder handle `col` — column-reference maker

The injected handle is a **callable object** (`ExprBuilder`, `types.ts:652`; built by `makeBuilder`, `ops.ts:2008`), invoked to make a column ref and carrying only `case` as a property:

| Form | Produces | Wire node | Source |
|---|---|---|---|
| `col("status")` | unqualified column ref | `{ node: "colRef", name }` | `ops.ts:1987` |
| `col("orders", "id")` | **qualified** column ref (the join-ON fix) | `{ node: "colRef", table, name }` | `ops.ts:1991` |
| `col.case({ branches, else? })` | searched CASE | `{ node: "case", … }` | `ops.ts:1948` |

Scalar functions, aggregate functions, regex/column-size operators, and extract fields are chain methods on the returned `ExprChain` ([§4.3](#43-chain-operators-exprchainimpl-opsts1702)); receiver-less functions are top-level imports ([§4.8](#48-value-constructors--top-level-imports)). The two-arg form lets expression callbacks table-qualify a column, which is required for view and trigger join `ON` clauses. The injected authoring handle is conventionally spelled `col`; the callback parameter name does not affect the recorded IR.

### 4.3 Chain operators (`ExprChainImpl`, `ops.ts:1702`)

Every method returns a fresh `ExprChain`. Bare JS values auto-wrap to a `literal` node via `exprArg`; a `Uint8Array`/`decimal()`/non-integer number normalizes to the closed `IrScalar` carrier via `toIrScalar`; a JS function (other than recognized native symbols) is rejected `OP_INVALID`.

- **Comparison** — `bin(op, x)` → `{ node:"binOp", op, lhs, rhs }` (`ops.ts:1707`): `.eq/.ne/.lt/.le/.gt/.ge`. `eq(null)`/`ne(null)` **throw** — "always UNKNOWN in SQL — use isNull()".
- **Boolean**: `.and(...)`/`.or(...)` left-folded `binOp`; `.not()` → `unaryOp op:"not"`.
- **Arithmetic** — `binOp`: `.add/.sub/.mul/.div`.
- **String**: `.concat(...parts)` → left-folded `binOp op:"concat"` (raw `||`; NULL-**skipping** joins are the top-level `concatWs(sep, ...parts)` import).
- **Null/bool tests** — `unaryOp`: `.isNull/.isNotNull/.isTrue/.isFalse`.
- **Cast**: `.cast({ to })` → `{ node:"cast", operand, target }`; the closed target set (`castTargets`, `ops.ts:1626`) is **`text, int, real, boolean, bytes, uuid`**.
- **Portable predicates**: `.between(low,high)` → `between`; `.like(pattern)` → `like`; `.in(values)`/`.notIn(values)` → `inList` (require a **homogeneous** `Scalar[]`; empty strings, NUL bytes, non-finite numbers rejected; PG renders `= ANY(ARRAY[...])`); `.distinctFrom(x)` → `distinctFrom` (PG/SQLite `IS DISTINCT FROM` vs MySQL `NOT (x <=> y)`).
- **PostgreSQL-first chain operators**: `.regex(pattern)` → `{ node:"pgRegexMatch", expr, pattern }` (`~` on PG, `REGEXP` on MySQL, **error on SQLite**); `.columnSize()` → `{ node:"pgColumnSize", expr }` (`pg_column_size()` on PG, **error elsewhere**). The dialect gate lives in the Rust validator and fails closed off-target (`ops.ts:1770-1774`, `types.ts:575-577`).
- **Scalar functions**: `.lower/.upper/.trim/.length/.abs/.coalesce/.nullif/.mod/.round/.floor/.ceil/.substr/.replace/.extract/.splitPart` record `fnCall`, `extract`/`pgExtract`, or `fnSynth` nodes; see [§4.4](#44-scalar-functions-chain-methods).
- **Aggregates**: `.count/.sum/.avg/.min/.max` record `agg` nodes with the receiver as `arg`; receiver-less `COUNT(*)` is `countStar()`. `.stringAgg(delimiter)`, `.arrayAgg()`, `.boolAnd()`, and `.boolOr()` are PostgreSQL-first chain methods that fail closed off-PG unless wrapped in `dialect({...})`; see [§4.5](#45-aggregate-functions-chain-methods).

### 4.4 Scalar functions (chain methods)

Receiver-ful scalar functions are authored off the expression chain and build `fnCall`/`extract`/`fnSynth` nodes:

| Member | Wire node | Notes |
|---|---|---|
| `.lower()` / `.upper()` / `.trim()` / `.length()` / `.abs()` | `fnCall` | receiver-first: `col("email").lower()` |
| `.coalesce(...rest)` | `fnCall fn:"coalesce"` | variadic after the receiver: `col("nickname").coalesce(col("name"), "unknown")` |
| `.nullif(b)` | `fnCall fn:"nullif"` | |
| `.mod(b)` | `fnCall fn:"mod"` | portable `%` |
| `.round(n?)` | `fnCall fn:"round"` | optional precision |
| `.floor()` / `.ceil()` | `fnCall` | |
| `.substr(start, len?)` | `fnCall` | 1-based |
| `.replace(from, to)` | `fnCall` | |
| `.extract(field)` | `{ node:"extract", field, from }` or `{ node:"pgExtract", ... }` | portable fields record `extract`; PG-only fields record `pgExtract` and fail closed off-PG |
| `.splitPart(delim, n)` | `{ node:"fnSynth", fn:"splitPart" }` | engine-synthesized portable helper; `splitPartGrammarLint` guards literal `delim`/`n` |
| `concatWs(sep, ...parts)` | `{ node:"fnSynth", fn:"concatWs" }` | NULL-skipping safe join; this is a top-level import, not a chain method |

Portable `.extract(field)` fields are `year, month, day, hour, minute, dow`. PG-only fields are accepted by the same chain method and recorded as `pgExtract`: `second, doy, epoch, quarter, week, isodow, isoyear, century, decade, millennium, microseconds, milliseconds, timezone, timezone_hour, timezone_minute`. The Rust validator rejects a `pgExtract` node on SQLite/MySQL unless the author supplies a dialect leg.

### 4.5 Aggregate functions (chain methods)

Aggregate chain methods call `aggNode` → `{ node:"agg", func, arg?, delimiter?, distinct? }`:

| Member | Wire node | Notes |
|---|---|---|
| `.count(opts?)` | `{ node:"agg", func:"count", arg:<receiver>, distinct? }` | `opts` is `{ distinct?: boolean }` |
| `.sum(opts?)` / `.avg(opts?)` / `.min(opts?)` / `.max(opts?)` | `agg` | receiver-first: `col("total").sum({ distinct: true })` |
| `.stringAgg(delimiter)` | `{ node:"agg", func:"stringAgg", arg:<receiver>, delimiter }` | PostgreSQL `string_agg(<expr>, <delimiter>)`; `delimiter` may be a string or expression |
| `.arrayAgg()` | `{ node:"agg", func:"arrayAgg", arg:<receiver> }` | PostgreSQL `array_agg`; fail-closed off-PG |
| `.boolAnd()` / `.boolOr()` | `{ node:"agg", func:"boolAnd"|"boolOr", arg:<receiver> }` | PostgreSQL `bool_and` / `bool_or`; fail-closed off-PG |
| `countStar()` | `{ node:"agg", func:"count" }` | top-level import for receiver-less `COUNT(*)` |

The standard five (`count/sum/avg/min/max`) render byte-identically on all three dialects (only quoting differs) so there is **no dialect gate** for those variants. The long-tail PostgreSQL aggregates (`stringAgg/arrayAgg/boolAnd/boolOr`) are PG-first and validate as `DIALECT_UNSUPPORTED` on SQLite/MySQL unless the author supplies explicit alternatives with `dialect({...})`. The position check is enforced by the Rust validator: `AGGREGATE_IN_SCALAR_CONTEXT` rejects aggregates in scalar contexts such as index expressions/predicates, generated columns, CHECK constraints, and column defaults ([§7.4](#74-the-full-structured-error-code-taxonomy)). `jsonb_agg`, aggregate-local `ORDER BY`, and aggregate `FILTER` clauses are outside the current surface.

### 4.6 PG extract fields and vendor-neutral spelling

`regex` and `columnSize` are core chain operators ([§4.3](#43-chain-operators-exprchainimpl-opsts1702)); PG EXTRACT fields are reached through the core `.extract(field)` chain method ([§4.4](#44-scalar-functions-chain-methods)). The surface is PostgreSQL-first: these nodes are authorable on the core surface and fail closed when the target dialect lacks a native realization and the author did not provide an explicit `dialect({...})` leg.

### 4.7 Context-typed builders — the immutable/mutable split

Rather than one polymorphic handle, the recorder hands out **different builder objects** per position (see the tier map in [§3.17](#317-the-builder-tier-map-which-slot-gets-which-restricted-builder)): `makeBuilder()` (full column accessor + `case`), `immutableExprBuilder()` (same handle shape, then validated for immutable scalar contexts), `checkWithPgBuilder()` (same, but allows PG-immutable nodes for PG checks), `domainValueBuilder()` (only the `VALUE` chain plus `case`), and `defaultBuilder()` (only `case`). There are no builder namespaces. Two JS-side walkers and two Rust backstops enforce the contexts:

- **`validateImmutableExpr`** (`ops.ts:2123`): rejects volatile `fnSynth` (`now`) and the volatile `uuidV4`/`uuidV7` nodes, `currentSetting`/`currentUser`, non-immutable scalar/synth helpers, and — unless `allowPgImmutable` — the PG nodes `pgRegexMatch`/`pgColumnSize`/`pgExtract`/`pgInterval`. It walks aggregate arguments and `stringAgg` delimiters but does not reject the aggregate node itself; the Rust `AGGREGATE_IN_SCALAR_CONTEXT` backstop is authoritative for aggregate placement.
- **`validateDefaultExpr`** (`ops.ts:1085`): rejects `colRef` ("a column default cannot reference a column"), `extract`, `dialect`, and all `pg*` nodes. Allowed synth `DEFAULT_SYNTH_FNS` = `now`, `concatWs`, `splitPart`, and the dedicated `uuidV4`/`uuidV7` nodes are admitted by their own arms — so `now()`/`uuidV4()` **are** permitted in a default despite being volatile, but a column ref is not. It also walks aggregate arguments and delimiters; the Rust `AGGREGATE_IN_SCALAR_CONTEXT` backstop rejects aggregates in defaults.
- **Rust backstops**: `IMMUTABLE_CONTEXT_VOLATILE` rejects volatile functions in immutable SQL contexts, and `AGGREGATE_IN_SCALAR_CONTEXT` rejects aggregates in scalar contexts. These are the fail-closed gates for artifacts that bypass or outpace the TS/JS surface.

The `DefaultBuilder` exposes only `{ case }` — deliberately no column accessor — so a default callback cannot reference columns by construction. Aggregates remain type-reachable through prebuilt chains/top-level imports and are rejected by Rust validation.

### 4.8 Value constructors — top-level imports

Receiver-less value producers are **top-level named exports** (from `index.ts:29-40`). Most defaults/values need no `(col) =>` callback at all.

| Import | Wire node | Notes |
|---|---|---|
| `now()` | `{ node:"fnSynth", fn:"now" }` | |
| `uuidV4()` | `{ node:"uuidV4" }` | dedicated Expr node, not a `fnSynth` |
| `uuidV7()` | `{ node:"uuidV7" }` | dedicated Expr node; fails closed on a target that cannot produce a v7 |
| `currentSetting(name, { missingOk? })` | `fnCall fn:"currentSetting"` | PG-vendor; rejected in immutable/default positions |
| `currentUser()` | `fnCall fn:"currentUser"` | PG-vendor |
| `interval(duration)` | `{ node:"pgInterval", duration }` | structured `Duration`, see below |
| `lit(value)` | `{ node:"literal", value }` | explicit literal wrap |
| `decimal("…")` | branded → `{ decimal }` | validated by `DECIMAL_STRING_RE` |
| `byteValue(bytes\|b64)` | branded → `{ bytes }` | |
| `nextval(name, { schema? })` | branded default → `{ nextval: {...} }` | default-only carrier |

`interval` takes a structured `Duration` (`types.ts:135`) with integer fields `years, months, days, hours, minutes, seconds`; `pgDuration` requires ≥1 field, rejects unknown/non-integer fields, canonicalizes order. A top-level import cannot know its final expression position, so a volatile constructor placed in an *immutable* index/generated slot is caught at **validate time** (via `validateImmutableExpr` and its Rust backstop, [§7.6](#7-the-validate-gate--error-taxonomy)), not at tsc. Native-symbol shorthand: passing the bare native identities `Date.now`/`Math.random`/`crypto.randomUUID` (no parens) in a DML/value slot still normalizes to `fnSynth now` / the `uuidV4` node; in a **default** slot those bare symbols are **rejected** (`rejectRemovedDefaultFunctionValue`, `ops.ts:1064`) with a message steering to `.default(now())`.

Typical usage:
```ts
id:         t.uuid().primaryKey().default(uuidV4()),
created_at: t.timestamp().notNull().default(now()),
using:      (col) => col("app_id").eq(currentSetting("shop.tenant").cast({ to: "uuid" })),  // callback only where you must reference a column
```

### 4.9 `dialect({...})` — the portability escape

`dialect(legs)` is the explicit escape for per-dialect value or op divergence.

In expression/value position, each leg is itself an expression wrapped by `exprArg`; the node records in canonical leg order `default, pg, sqlite, mysql` → `{ node:"dialect", default?, pg?, sqlite?, mysql? }`:

```ts
default(dialect({ pg: uuidV4(), sqlite: now(), mysql: myUuid }))
dialect({ default: lit(0), pg: col("n") })   // pg leg on PG, default(0) elsewhere
```

At least one leg must be present or it throws `OP_INVALID`. The engine's validate applies per-target scope math: a target with no own leg and no `default` is refused (`EXPR_NOT_PORTABLE`/`DIALECT_UNSUPPORTED`). `validateDefaultExpr` **rejects** any `dialect` node in a default.

In statement/op position, legs are thunks. The recorder runs each present thunk in canonical order (`default`, `pg`, `sqlite`, `mysql`), captures the ops emitted by that thunk, removes those captured ops from the outer recorder, and emits one dialectal op containing the per-target op lists. A target with no own leg and no `default` leg skips the op entirely.

### 4.10 PostgreSQL is first-class

The platform targets PostgreSQL first; the core surface **is** PG-shaped and is **not** bent toward a lowest-common-denominator portable core. PG constructs — `~` regex, `pg_column_size`, `current_setting`, RLS, roles/grants, PG EXTRACT fields, `hnsw`/`ivfflat`, `EXCLUDE`, `ON CONFLICT` — are directly usable on the core surface with no `/pg` import and no vendor-namespace casting. Portability to SQLite/MySQL is explicit and opt-in via `dialect({...})` at the exact value/op that diverges; anything with no native realization and no `dialect()` leg **fails closed** at that target.

### 4.11 Portability notes

- Aggregate position enforcement is verified in the Rust validator: `AGGREGATE_IN_SCALAR_CONTEXT` rejects aggregates in scalar contexts, while structured view projection and `having` are grouped SELECT contexts.
- `.regex()` renders as `~` on PostgreSQL and `REGEXP` on MySQL, and fails closed on SQLite.

---

## §5 Authoring: declarative desired-state & the fold

`zeroship-migrate` has **two authoring models**, both feeding the same executor. [§3](#3-authoring-schema-structure-dsl)–[§4](#4-authoring-the-expression-sublanguage) cover the imperative `op.*` migration DSL — the creator writes explicit ordered ops. This section covers the second model: the **declarative "declare desired schema → engine diffs live → generates migrations"** path (`render/declarative.rs`, ~7.5k lines; `frontend/generate.rs`), plus the **migration-first fold** that turns a migration set into the typed `env.db` surface. Both are "new authors, not new executors": every `Migration` they produce still flows through the unchanged `plan → guard → gate → executor::apply` pipeline — there is no DDL bypass (`render/declarative.rs:12-19`).

### 5.1 The declarative differ: desired snapshot → diff → generated migrations

The declarative authoring layer accepts a creator's **declared schema** as per-collection descriptor JSON (`{ _meta, _indexes, <field>: { type, required, unique, default, ref } }`). `render/declarative.rs` turns that into migrations in two passes (`declarative.rs:1-11`, re-exported at `lib.rs:137-141`):

1. **`desired_snapshot(...)`** reduces the declared descriptor to a deterministic `SchemaSnapshot` (`TableSnapshot`/`ColumnSnapshot`/`IndexSnapshot`/`ConstraintSnapshot`). Declared-only facets (typed-id `prefix`, vector `metric`, `mask` brand, encrypted/geoPoint/literal) are carried because the model layer **adopted `zeroship-schema`** for full type capability ([§2.2](#2-crate-architecture)).
2. **`DeclarativeAuthor::diff(...)`** introspects the **live** schema into a snapshot and diffs desired-vs-live, emitting the minimal `Migration` set (create tables, add columns/indexes/constraints; a destructive drop/type-change is *gated* through the approval path, never silently applied). The differ is the imperative `IrAuthor::lower` path's peer — both route through the **same shared snapshot-builder** (`build_table_snapshot`) and the **same render methods** (`DeclarativeAuthor::lower_*` → `render_create_table`/`DdlEmitter`), so the emitted SQL is byte-identical **by construction** and a cross-path golden guards it (`render/lower.rs:7-19`).

**Trust boundary.** Descriptor field/table names and types are **untrusted** (a prompt-injectable AI authored them). They are validated at the author boundary (`validate_ident`/`validate_type`, mirroring `render/expand_contract.rs`) *and* re-checked by the guard as the second line (`declarative.rs:20-27`). The DSL-type→Postgres-type table here is *deliberately replicated* from `plugin-db/src/query.rs` — the two crates are different trust domains and the migrate crate must not depend on the runtime plugin; the `desired_snapshot`-round-trips-to-live test guards the two copies against drift (`declarative.rs:28-45`); it survives in the standalone engine as `crates/zeroship-migrate/tests/pg_engine/pg_declarative.rs`.

### 5.2 `generate --schema` and `DeclarativeDeployPlan`

`frontend/generate.rs` exposed the differ through the former JS authoring CLI's
`generate --schema schema.js` verb. It evaluated the schema module in a
sandboxed V8 child to a descriptor IR, diffed it against the live DB, and
rendered a deployable `.sql` covering the full vector/postgis/FK/CHECK surface.
The `--project-schema` flag (default `public`) named the schema to introspect in
the former CLI source.

The control-plane deploy path drives the same differ through `apply_declarative` (`executor.rs`) with a `DeclarativeDeployPlan` (re-exported at `lib.rs:142-146`). A declarative deploy is several **sub-batches** (the plain additive set plus one online-rename **expand** per renamed column) that must serialize *as a whole*: the outer `apply_declarative` acquires the project advisory lock once via `acquire_project_lock_outer` and threads `LockMode::AlreadyHeld` into each inner `apply_with_lock` — the lock is taken exactly once and freed exactly once, never freed between sub-batches where a second deploy could interleave ([§9.3](#9-the-apply-engine--durability)). Per-sub-batch session hygiene (GUC snapshot/restore + `RESET ROLE`) still runs every time.

### 5.3 The migration-first fold: migration set → `env.db.ts` + `schema.runtime.json`

The reverse direction makes the `op.*` migration set the **sole source of truth** for the typed `env.db` surface — the types are *generated from the fold*, never hand-declared (`frontend/gen_types.rs`). The former JS authoring CLI's `gen-types` command produced two artifacts in `generated/zeroship/` (the *committed* output dir, chosen so `env.db.ts` can be in tsconfig):

1. **`schema.runtime.json`** — the v2 `RuntimeSchemaDescriptor`: `{ version: 2, collections: { [c]: { fields, options, indexes } } }` (`gen_types.rs:15-16,40`).
2. **`env.db.ts`** — a real `.ts` **module** (not a `.d.ts`) reconstructing `const schema = { … t.text() … } as const` of `@zeroship/db` `t.*()` builder calls, wrapping collections in `defineSchema(...)` + runtime-metadata chains (`.softDelete()`, `.withVersioning()`, `.strictness(...)`, `.index(...)`), then `declare module "zeroship" { interface Env { db: Db<typeof schema> } }` (`gen_types.rs:493-548`). It **must** be a module because `t.*()` value expressions are illegal in a `.d.ts` ambient context, and the SDK's `InferFieldDef` inference keys only off `TypeBuilder` builder calls — so the emitter reconstructs builder calls rather than a hand-rolled interface (`gen_types.rs:42-51`).

**Pipeline** (`gen_types.rs:1-25`): (1) record each committed `.ts` in version order through the sandboxed recorder and concatenate the transient `Op` lists (`load_dir_ops`); (2) `fold_to_field_defs(ops, SqlDialect::Postgres, project_schema)` folds-and-recovers per-collection wire-`FieldDef` maps (`render_artifacts`) — the *same fold the engine uses internally*; (3) render both artifacts. The `indexmap` dependency preserves `createTable` **declared column order** through the fold so a sorted-vs-declared difference can't perturb the parity comparison. A `--check` mode regenerates in memory and diffs against the on-disk files — the CI drift gate, no DB write (`GenTypesError::Drift`).

Declared-only facets survive the fold — the typed-id `prefix`, vector `metric`, and `mask` brand all flow into `env.db.ts`, so `env.db.users.email` reads back as `MaskedValue<T>` *purely from migration history*.

**Schema neutrality:** the former CLI's `gen-types` subcommand always folded under the **constant `"public"`** project schema (`render_artifacts(&ops, "public")`, line 337 of the former CLI source) — type recovery does not care which schema the tables live in, which is why creators never passed `{schema}`.

### 5.4 How the fold is consumed

The vite-plugin now runs gen-types in-process: its pure-JS recorder evaluates
the committed migration modules into IR envelopes, and `zeroship-migrate-node`'s
`genArtifacts` verb folds and renders `env.db.ts` plus
`schema.runtime.json`. There is no CLI subprocess or missing-binary no-op. The
`.zship` packer does **not** carry migration documents — it reads only the
generated `schema.runtime.json` and stages it as the manifest's
content-addressed `runtime_descriptor` blob
(`stageRuntimeDescriptor` in `sdks/vite-plugin/src/zship.ts`); deploy-time application runs through
the standalone migration service. At runtime boot, Rust validates the
descriptor and plugin-db publishes its collection field maps natively before
creator modules evaluate. The DB plugin then runs its crate-owned
`installSchema(env.db, descriptor)` adapter to plant typed `Collection`
wrappers on the native `env.db`. So both directions
meet at one wire type - the v2
`RuntimeSchemaDescriptor`: gen-types *emits* it, the `.zship` packer *carries*
it, native boot *binds* it, and `installSchema` projects its typed JavaScript
wrappers. See
[§11.6–§11.7](#11-platform-self-hosting--build-integration).

---

## §6 The IR & its wire contract

A `zeroship-migrate` migration authored in the JS `op.*` DSL never ships SQL. It ships a small, **dialect-neutral, checksummed JSON document** — the `.ir.json` — whose Rust mirror is `MigrationIr`. The engine loads that document, lowers each `Op` to per-dialect SQL ([§8](#8-one-ir-three-dialects-render--portability)) at apply time, and hashes the *neutral* op-list so a single portable migration has exactly **one** identity checksum across every render target. IR types lived in `model/ir.rs`, the expression AST in `model/expr.rs`, the migration unit + checksum in `model/migration.rs`; all three now sit in the engine's leaf wire-contract crate, `crates/zeroship-migrate-ir/src/`. `CURRENT_IR_VERSION` is **6** (`ir.rs:91`).

### 6.1 The core concept: a dialect-neutral, checksummed IR

Several invariants are baked into the *types themselves* (`ir.rs:1-45`):

- **Closed `Op` enum, internally tagged on `"op"`** (`#[serde(tag = "op")]`, no `untagged`, no `flatten`) — `ir.rs:2427-2429`. The discriminant is a stable top-level `"op"` key; ADR `docs/decisions/2026-06-23-op-ir-serde-repr.md`.
- **All identifier fields are plain `String`** — the IR carries *no* live-schema binding (`ir.rs:16-17`); existence/safety is a *structural* validator ([§7](#7-the-validate-gate--error-taxonomy)), never a `tsc`-style bind.
- **Raw SQL is admitted only in three operator-gated islands** (`ir.rs:18-23`): `Op::CreateFunction.body`, `Op::PgRaw.sql`, and `ViewQuery::Raw.sql`. Each is capability-gated + parser/deny-list scanned before apply.
- **Constrained numeric domain enforced at DESERIALIZE** (`IrScalar`, `ir.rs:24-28`): a fractional/exponential number, or an integer with magnitude ≥ 2⁵³, is rejected with `EXPR_INVALID_NUMERIC` *before any checksum*.
- **Absent optionals are OMITTED, never `"field":null`** (`ir.rs:29-40`): `#[serde(skip_serializing_if = "Option::is_none")]` on every `Option`. This is the cross-impl-determinism contract behind the single-checksum invariant. Deserialize still *accepts* explicit `null` and canonicalizes it back to omitted, so null-bearing and omitted `.ir.json` yield the same checksum.

**`MigrationIr` — the document envelope** (`ir.rs:343-382`, `#[serde(deny_unknown_fields)]`): `ir_version: u32`, `name: String`, `owner_app: String` (a hint — server overrides at submit), `ops: Vec<Op>`, `flags: IrFlagsOverride`, `depends_on: Vec<String>`, `supersedes: Vec<String>`, `preconditions: Vec<PreconditionCheck>`, `checksum: Option<String>` (advisory, §6.6).

**`ir_version` — the fail-closed evolution knob.** `MigrationIr::check_ir_version` (`ir.rs:398-406`) rejects a **future** version fail-closed (`IrVersionError`); a past/equal version validates because a bump must be checksum-neutral for already-applied artifacts. The loader calls this *after* deserialize and *before* `Checksum::of_ir` and lowering.

### 6.2 Every `Op` variant (53)

The `Op` enum (`ir.rs:2429-3394`) is closed, internally tagged on `"op"`, camel-cased, `deny_unknown_fields`. There are **53 variants** — the exhaustiveness gate `every_op_variant_has_a_fixture` hard-asserted this count (`op_round_trip.rs:233-237`; §12.1 records where that corpus check lives now). Every table-targeting variant carries optional `schema: Option<String>` and, where guardable, `existence_guard: Option<ExistenceGuard>` (`ifNotExists`/`ifExists`) — both omitted-when-absent. For the per-dialect support of each op, see [§8.3](#8-one-ir-three-dialects-render--portability).

**Table / column DDL:** `createTable` (`CreateTable@2431`), `dropTable@2533`, `renameTable@2559` (fast catalog rename, NOT expand-contract), `setTableOptions@2522` (metadata-only, no SQL but folds), `addColumn@2573`, `dropColumn@2626`, `setColumnType@2716` (`using: Option<Expr>` cast), `setColumnNotNull@2735`, `dropColumnNotNull@2748`, `setColumnDefault@2761`, `dropColumnDefault@2777`, `renameColumn@2790` (carries post-rename `type` for re-derivation).

**Constraints & indexes:** `addConstraint@2808`, `validateConstraint@2824` (PG-only online `NOT VALID`, refused off PG), `dropConstraint@2837`, `createIndex@2639` (`where: Option<Expr>` partial predicate, `using`, `concurrently`, `include`, `with`, `only`, `nulls_not_distinct`), `dropIndex@2687` (`unique: Option<bool>` drives destructive gating), `comment@2679` (`comment: Option<String>`; `None` → `IS NULL`).

**Partitioning:** `createPartition@2463`, `attachPartition@2478`, `detachPartition@2490` (`concurrently`), `dropPartition@2503` (`cascade`).

**DML:** `insert@2850` (`on_conflict: Option<IrOnConflict>` PG-only upsert), `update@2870` (`set: BTreeMap<String, IrValue>` sorted for canonicality), `delete@2889` (`where: Expr` **mandatory** — no unfiltered delete; **wire tag `"delete"`** even though the JS method is `del()`), `backfill@2903` (`cursorColumns`, `cursorStability`, `batchSize: SafeU64`, `set`, `filter`, `name` progress key).

**Views, enums, domains, sequences:** `createView@2923` (`query: ViewQuery`, `materialized`), `dropView@2942`, `createEnum@2958`, `dropEnum@2968`, `createDomain@2981` (`check: Option<Expr>` where a `ColRef` named `VALUE` = the domain value), `dropDomain@3002`, `createSequence@3016` (present-nullable `min_value/max_value/owned_by`), `alterSequence@3063` (`restart: Option<Option<SafeI64>>` — `null` → bare `RESTART`), `dropSequence@3112`. The present-nullable fields use `deserialize_present_nullable` (`ir.rs:150-156`) so the wire distinguishes "absent" (omit clause) from "`null`" (emit `NO MINVALUE`/`RESTART`).

**VENDOR — Postgres-only privileged primitives rooted on `@zeroship/migrate`** (`ir.rs:3123-3131`): each is `dialect_scope = PgOnly` (a SQLite deploy is hard-rejected at load) and refused fail-closed under a Confined capability set. `password`, `body`, and `sql` are the only free-`String` (raw) fields, still parse-scanned by the guard deny-list. `createSchema@3133`, `dropSchema@3144`, `createExtension@3157` (allowlist-gated), `dropExtension@3168`, `createRole@3180` (`superuser` DENIED at render in all profiles; `if_not_exists` synthesizes a `pg_roles` probe), `alterRole@3213`, `dropRole@3224`, `dropOwnedBy@3232`, `grant@3237` (`to: Vec<String>`, `"public"` = PUBLIC sentinel), `revoke@3249`, `setRls@3258`, `createPolicy@3274` (`using: Expr`, `with_check: Option<Expr>` — closed AST, NOT strings), `dropPolicy@3294`, `createTrigger@3309` (`when: Option<Expr>`), `dropTrigger@3331`, `createFunction@3349` (`body: String` — **the single raw-string escape in the whole DSL**, `ir.rs:3343-3348`), `dropFunction@3372`, `pgRaw@3388` (`sql: String` verbatim + mandatory `reason: String` audit). `Op::is_vendor` (`ir.rs:3402-3405`) is the runtime authority via `vendor_capabilities()`; `support()` (`ir.rs:3411`) drives the per-dialect support matrix.

### 6.3 Every `Expr` node

The expression AST (`expr.rs`) is **closed** and **never parsed from text** — no lexer, no Pratt parser, no `libpg_query`, hence no Rust-vs-JS parser drift and no differential fuzzer (`expr.rs:1-31`). `Expr` (`expr.rs:279-511`) is internally tagged on `"node"`, camel-cased, `deny_unknown_fields`.

| `"node"` | Variant @ line | Fields |
| --- | --- | --- |
| `colRef` | `ColRef@288` | `name`, `table: Option<String>` (qualified `col("t","col")` sets `table`) |
| `literal` | `Literal@302` | `value: IrScalar` |
| `binOp` | `BinOp@307` | `op: BinaryOp`, `lhs`, `rhs` (`Box<Expr>`) |
| `unaryOp` | `UnaryOp@316` | `op: UnaryOp`, `operand` |
| `case` | `Case@324` | `branches: Vec<CaseBranch>`, `else: Option<Box<Expr>>` |
| `fnCall` | `FnCall@332` | `fn: ScalarFn` (allow-listed), `args` |
| `fnSynth` | `FnSynth@340` | `fn: SynthFn` (engine-synthesized), `args` |
| `uuidV4` | `UuidV4@360` | none — a DB-evaluated RFC 9562 v4 UUID; the renderer preserves the version/variant bits |
| `uuidV7` | `UuidV7@364` | none — a DB-evaluated RFC 9562 v7 UUID; fails closed rather than substituting another version |
| `cast` | `Cast@348` | `operand`, `target: CastTarget` |
| `between` | `Between@357` | `operand`, `low`, `high` — portable inclusive range |
| `like` | `Like@373` | `operand`, `pattern` — portable syntax |
| `distinctFrom` | `DistinctFrom@385` | `left`, `right` — NULL-safe; MySQL lowers to `NOT (<=>)` |
| `agg` | `Agg@403` | `func: AggFunc`, `arg: Option<Box<Expr>>` (`None`+`Count`=`COUNT(*)`), `delimiter: Option<Box<Expr>>` (`StringAgg` only), `distinct` |
| `inList` | `InList@420` | `expr`, `elems: Vec<IrScalar>`, `negated` |
| `pgRegexMatch` | `PgRegexMatch@433` | `expr`, `pattern` (rendered as SQL string literal). PG-only |
| `pgColumnSize` | `PgColumnSize@441` | `expr`. PG-only `pg_column_size` |
| `extract` | `Extract@447` | `field: ExtractField`, `from` — portable EXTRACT |
| `pgExtract` | `PgExtract@454` | `field: PgExtractField`, `from`. PG-only |
| `pgInterval` | `PgInterval@461` | `duration: Duration`. PG-only |
| `dialect` | `Dialectal@495` | `default`/`pg`/`sqlite`/`mysql` each `Option<Box<Expr>>` — the one Layer-2 escape; renders the matching leg else `default`, else refused fail-closed |

**Closed lexicons** (serde rejects any out-of-set token at deserialize): `BinaryOp` (`eq, ne, lt, le, gt, ge, and, or, add, sub, mul, div, concat`); `UnaryOp` (`not, isNull, isNotNull, isTrue, isFalse`); `ScalarFn` (`coalesce, nullif, lower, upper, trim, length, abs, mod, round, floor, ceil, substr, replace, currentSetting, currentUser`); `SynthFn` (`concatWs, splitPart, now`); `CastTarget` (`text, int, real, boolean, bytes, uuid`); `ExtractField` (`year, month, day, hour, minute, dow`); `PgExtractField` (snake_case: `second, doy, epoch, quarter, week, isodow, isoyear, century, decade, millennium, microseconds, milliseconds, timezone, timezone_hour, timezone_minute`); `AggFunc` (`count, sum, avg, min, max, stringAgg, arrayAgg, boolAnd, boolOr`).

### 6.4 `IrScalar` — the custom serialization and why

`IrScalar` (`ir.rs:4510-4528`) has a **hand-written `Serialize`/`Deserialize`** to control the wire image:

| Variant | Serialize form | Why |
| --- | --- | --- |
| `Null`/`Bool` | `null` / `true`/`false` | plain |
| `Int(i64)` | plain JSON number | matches a bare JS integer |
| `Str(String)` | plain JSON string | matches a bare JS string |
| `Decimal(String)` | `{"decimal":"…"}` | distinguishable from a plain string; arbitrary precision as string, never float |
| `Bytes(Vec<u8>)` | `{"bytes":"<base64>"}` | binary payload, canonical STANDARD (padded) base64 |

Corpus evidence, `edge_scalars.golden.json`: `{ "decimal": "9007199254740993" }` (integer past 2⁵³), `{ "decimal": "1.5" }` (fraction), `"héllo\t\"world\" 𝟙"` (Str bare string).

**The deserialize path is the security surface** (`ir.rs:4577-4653`), funnelling through `serde_json::Value` to inspect the number token shape: an `i64`/`u64` with magnitude ≥ `MAX_EXACT_INT` (2⁵³) → `EXPR_INVALID_NUMERIC`; a fractional/exponential number → rejected; `{"decimal":"…"}` → shape-checked by `is_decimal_string` (not parsed to float); `{"bytes":"…"}` → **strict** base64 decode (rejects wrong alphabet/padding), stored DECODED so two encodings normalize to one value → one checksum. **Why 2⁵³?** It is the boundary of exact integer representation in an IEEE-754 double (the JS `number` type); an integer ≥ 2⁵³ can be silently rounded by a JS author. `SafeU64` (structural counts) and `SafeI64` (signed sequence values) apply the same bound.

**`IrValue`** (`ir.rs:4695-4704`, `#[serde(untagged)]`) is the DML value slot: `Scalar(IrScalar)` or an `Expr` (e.g. `FnSynth(now)`). **`IrDefault`** (`ir.rs:683-777`, custom single-key-object) is the column-DEFAULT slot, richer: `Literal`, `Expr`, `Container{kind}` (`{}`/`[]`), `Json{value:IrJsonValue}` (non-empty JSON, integers-only), `Nextval{sequence}`.

### 6.5 Byte-stability + value-equality: `CanonicalOpList` and JCS

The op-list checksum region is produced by `CanonicalOpList<'a>(pub &'a [Op])` (`ir.rs:4728`). Its `canonical_bytes` builds: a u64 **big-endian op count**, then for each op its RFC 8785 (**JCS**) canonicalized bytes, **length-prefixed** with a u64-BE length, in op order. The JCS encoder enforces object keys sorted by **UTF-16 code-unit sequence** (`utf16_code_unit_cmp`, `ir.rs:4810` — because an author-supplied map key may be a supplementary-plane character where UTF-16 and UTF-8-scalar orders diverge, and a conformant JS JCS serializer sorts by UTF-16) and strings minimally JSON-escaped.

Two checksum front doors share a `fold_common` tail (`migration.rs:500-542`): **`Checksum::of`** over rendered `(up, down)` SQL, and **`Checksum::of_ir`** over the canonical op-list region prefixed with domain tag `b"zeroship-migrate/of_ir/v1"`. The single-checksum invariant is enforced by construction: `of_ir` **takes no dialect parameter** — it must be passed the dialect-neutral derived-then-overridden flags, never per-dialect *lowered* flags (SQLite forcing `transactional:true` / dropping `concurrently` is a render-time divergence that must not enter the identity hash). The tail folds `flags`, `owner_app`, `depends_on`, `supersedes`, `preconditions`, each length-prefixed so `down: Some("")` ≠ `down: None`.

**Value-equality vs byte-equality.** The load-bearing cross-impl gate (`op_round_trip.rs:20-24`) does *not* require byte-identical JSON between JS and Rust; it folds *both* through `Checksum::of_ir` and asserts the two **checksums are equal** — typed-VALUE equality, invariant under any JCS-formatting difference. See [§12.1](#12-testing--operating).

### 6.6 The advisory `checksum` hint

`MigrationIr.checksum: Option<String>` is an **advisory integrity hint**: the hex `Checksum::of_ir` the *builder* computed over the hint domain (`ops` + `flags` + `depends_on` + `supersedes` + `preconditions` — **never `owner_app`**, which is server-stamped). The engine recomputes and is authoritative; a mismatch is genuine drift. The hint is **excluded from `Checksum::of_ir` itself** (folding an artifact's own checksum would be circular) and is modelled explicitly only because `deny_unknown_fields` would otherwise reject a `.ir.json` carrying it.

### 6.7 The golden corpus and the schema gate

Two anti-drift artifacts pin the contract (mechanics in [§12](#12-testing--operating)): **`op-ir.schema.json`** — the JSON Schema of `MigrationIr`, emitted by `schemars::schema_for!`, gated against the on-disk file (regenerate with `cargo test -p zeroship-migrate --test ir_contract -- --ignored update_ir_envelope_schema`); and **the golden corpus** (`tests/op_fixtures/`, 21 `.mig.js`/`.golden.json` pairs) driving `op_round_trip.rs`'s three gates (golden byte-stability, JS↔Rust value-checksum round-trip, variant exhaustiveness pinned at 53).

### 6.8 How JS authoring maps to IR nodes

The mapping is direct and mechanical. From `dml.mig.js` → `dml.golden.json`:

```javascript
sc.update({ set: { label: (col) => col("label").coalesce("unknown"), marker: "fixed" },
            where: (col) => col("code").gt(0) });
sc.delete({ where: (col) => col("code").isNull(), limit: 100 });   // method del/delete → op "delete"
```
```json
{ "op": "update", "table": "status_codes",
  "set": { "label": { "node": "fnCall", "fn": "coalesce",
      "args": [ {"node":"colRef","name":"label"}, {"node":"literal","value":"unknown"} ] },
    "marker": "fixed" },
  "where": { "node":"binOp", "op":"gt", "lhs":{"node":"colRef","name":"code"}, "rhs":{"node":"literal","value":0} } },
{ "op": "delete", "table": "status_codes",
  "where": { "node":"unaryOp", "op":"isNull", "operand":{"node":"colRef","name":"code"} }, "limit": 100 }
```

**TypeScript types for advanced callers.** `packages/zero-migrate/scripts/gen-ir-types.mjs` generates the closed string-enum tokens into `packages/zero-migrate/src/generated/enums.ts` from `op-ir.schema.json`. The **recursive structural types** (`MigrationIr`, `Op`, `Expr`, `ColType`, `IrConstraint`) are **hand-authored** in `packages/zero-migrate/src/generated/ir.ts` (codegen overflows the stack on the self-recursive `oneOf`); a drift test pins every enum token/`Op` tag/`Expr` tag against the schema. Both files stress: these are *ergonomics*; the golden `.ir.json` corpus + the `Checksum::of_ir` round-trip are the **contract source of truth**.

### 6.9 The pre-launch "update every producer/consumer together" stance

The IR is a canonical example of the AGENTS.md wire-format discipline. `ir_version` exists so dev/test databases can re-interpret artifacts across engine versions (code-evolution discipline), not so deployed apps can be left alone (`ir.rs:85-90`). A shape change means bumping `CURRENT_IR_VERSION`, updating every `Op`/`Expr` node, regenerating `op-ir.schema.json` and every golden, updating the hand-authored `ir.ts`, and letting the drift + exhaustiveness + round-trip gates prove JS and Rust still agree.

`IrAuthor::lower` and the structural validator are covered in [§7](#7-the-validate-gate--error-taxonomy) and [§8](#8-one-ir-three-dialects-render--portability). The ADR `2026-06-23-op-ir-serde-repr.md` is cited by the code; this section documents the code-level contract.

---

## §7 The validate gate & error taxonomy

The **validate gate** is the authoritative *structural* layer: a purely in-memory allow-list walk over a deserialized `MigrationIr` that runs **before any DB connection, before checksum, before lower/render**. It lives in `crates/zeroship-migrate-core/src/model/validate.rs` (which re-exports the structural half from the `zeroship-migrate-ir` leaf crate) and produces a single machine-actionable rejection envelope — `AuthoringError` — so the author/AI loop gets a stable `code`, a human `reason`, and a `suggested_fix`. It is placed here (before render/apply in [§8](#8-one-ir-three-dialects-render--portability)–[§9](#9-the-apply-engine--durability)) because it is the pre-connect gate.

### 7.1 Why validate is structural, not a SQL parser

The closed `Expr` AST is **constructed in JS and serialized to IR — never parsed from text** (`validate.rs:1-27`). So the gate is a structural allow-list walk, not a lexer/Pratt-parser/`libpg_query`/differential-fuzzer. The serde deserializer already rejects an unknown node *tag* at load; this walk additionally rejects well-typed-but-out-of-policy *shapes* (an out-of-envelope `splitPart`, a non-portable cast, a volatile function in an immutable slot).

### 7.2 Entry points and where it runs

| Function | Purpose | Cite |
| --- | --- | --- |
| `validate_ir(ir, target_dialect, ts_locations)` | Whole `MigrationIr`; **Confined** default | `validate.rs:507` |
| `validate_ir_scoped(...)` | Threaded with `SchemaScope` + `PolicyProfile` | `validate.rs:536` |
| `validate_op(...)` / `validate_op_scoped(...)` | Single op; the per-op workhorse | `validate.rs:1229/1253` |
| `validate_expr(...)` | One expression tree; builds a `Ctx` and calls `walk` | `validate.rs:315` |
| `validate_ir_resolved` / `validate_op_resolved` | Re-run with a resolved live-column set (apply/render seam) | `lib.rs:272` |

`validate_ir` is step **3 of 5** in the fail-closed `.ir.json` load gate (`load.rs:1-34`, called at `load.rs:423`): (1) deserialize → typed `MigrationIr`; (2) `ir_version` fail-closed; (3) **structural validation**; (4) server-stamped ownership; (5) advisory checksum-hint compare. Every entry takes a `target_dialect: Dialect` (`Postgres | Sqlite | Mysql`) — a `PgOnly` construct passes for a Postgres target and is refused for SQLite/MySQL. Other callers: `render/sql_preview.rs:263`, `render/fold.rs:3070/3092`, `plan/loader.rs:554`.

### 7.3 The `AuthoringError` envelope

```rust
pub struct AuthoringError {
    pub code: String,                 // one of the CODE_* consts
    pub kind: Option<UnsupportedKind>,// op-vs-expr discriminant for UNSUPPORTED
    pub op_index: usize,              // 0-based index of the offending op
    pub ts_location: Option<String>,  // e.g. "migrations/0007_split.ts:9"
    pub dialect: Dialect,             // which target the rejection pertains to
    pub reason: String,
    pub suggested_fix: Option<String>,// concrete remedy — LEADS the rendering
}
```
(`validate.rs:204-222`.) `to_json()` and `Display` lead with `suggested_fix` so a human rendering leads with the unblocking field. `UnsupportedKind` is `Op | Expr | VirtualColumn | Identity`, carried as `kind` on `UNSUPPORTED` rather than four top-level codes.

### 7.4 The full structured error-code taxonomy

Every `CODE_*` constant (`validate.rs:55-141`):

| Code / wire string | Triggered by |
| --- | --- |
| `UNSUPPORTED` | An op/expr renderable on NEITHER dialect (or, per-target, not on *this* one). Carries `kind`. `:55` |
| `EXPR_NOT_PORTABLE` | Expressible but out of portable envelope: out-of-envelope `splitPart`, or a `dialect()` with no leg + no default covering the target. `:59` |
| `DIALECT_SCOPE_PGONLY` | A `dialect_scope = PgOnly` artifact deployed against a SQLite target. `:61` |
| `OP_OUTSIDE_RECORDER` | Op-function called outside an active recorder — **emitted JS-side**. `:63` |
| `OP_INVALID` | Structurally-valid JSON with an internally inconsistent op shape. `:65` |
| `CROSS_SCHEMA` | An op naming a `schema` the active `SchemaScope` does not permit (Confined pins the project schema). `:73` |
| `INVALID_SCHEMA_IDENT` | A `schema` qualifier that is not a safe bare identifier — injection defense. `:79` |
| `GUARD_DIRECTION` | An existence guard with illegal direction (`ifExists` on create/add, `ifNotExists` on drop/rename/alter). `:83` |
| `INVALID_ID_PREFIX` | A malformed legacy internal platform `idPrefix`; this is not the public TypeID-format helper. `:101` |
| `INVALID_TYPE_ID_PREFIX` | An `ids.typeId({ prefix })` value outside the TypeID 0.3 grammar or 63-byte bound. `:104` |
| `VECTOR_METRIC_MISPLACED` | A `vector_metric` on a non-`Vector` column. `:95` |
| `COLUMN_FACET_CONFLICT` | Mutually-exclusive facets (`default`+`generated`, `identity`+`generated`). `:98` |
| `COLUMN_DEFAULT_TYPE` | A default invalid for the declared type (e.g. `{}` on `text[]`). `:101` |
| `IMMUTABLE_CONTEXT_VOLATILE` | A volatile function (`now()`, `uuidV4()`) in an immutable context. `:104` |
| `AGGREGATE_IN_SCALAR_CONTEXT` | An aggregate appeared in a scalar context (index expr/predicate, generated column, CHECK, or column DEFAULT). `:107` |
| `SEQUENCE_OPTION_INVALID` | `increment = 0`, `cache < 1`, or `minValue > maxValue`. `:110` |
| `VENDOR_OP_DENIED` | A privileged vendor op whose required `VendorCapability` isn't granted. `:116` |
| `PGRAW_REASON_REQUIRED` | A `pgRaw` op with an empty audit `reason`. `:118` |
| `PRIMARY_KEY_INVALID` | Resolved `primaryKey` empty/duplicated/absent-column. `:121` |
| `TABLE_SHAPE_POLICY` | A `createTable` violating the active profile's table-shape policy. `:123` |
| `DIALECT_UNSUPPORTED` | Target cannot realize a construct and no transparent-degradable leg applies. `:126` |
| `PARTITION_KEY_COVERAGE` | Unique-enforcing entries must cover all partition-key columns. `:129` |
| `PARTITION_BOUNDS_NOT_TOTAL` | Collapse-affirmed bound sets must be total. `:131` |
| `PARTITION_COMPOSITE_KEY_UNSUPPORTED` | v1 range collapse supports a single partition key only. `:133` |
| `PARTITION_KEY_NULLABLE_UNDER_COLLAPSE` | Collapse predicates need two-valued, non-null keys. `:136` |
| `PARTITION_BOUNDS_ILL_FORMED` | Sibling bounds must be PG-well-formed. `:139` |
| `PARTITION_HASH_DROP_UNDERIVABLE` | Hash child drops have no portable collapse predicate. `:141` |

### 7.5 Per-op gate ordering inside `validate_op_scoped`

The dispatch runs a fixed sequence BEFORE the per-op expression-slot walk (`validate.rs:1265-1286`), each fail-closed: (1) `validate_op_schema_and_guard` (`CROSS_SCHEMA`/`INVALID_SCHEMA_IDENT`/`GUARD_DIRECTION`); (2) `validate_vendor_op` (the vendor capability gate); (3) `validate_create_table_primary_key_policy`; (4) `validate_op_support` (`DIALECT_UNSUPPORTED`); (5) `validate_sequence_options`; (6) `validate_function_type_refs`. Then the `match op { … }` enumerates each variant's expression positions and calls `validate_expr` per node. The slot map is at `validate.rs:479-491`.

### 7.6 The `Ctx` structural walker — rules (a)/(b)/(c)/(d)

`validate_expr` builds `Ctx { target_dialect, scope, op_index, ts_location }` and calls `walk` → `walk_depth`. The four allow-list rules (`validate.rs:8-24`): **(a)** every node is allow-listed; **(b)** `.splitPart()` args are in-envelope — `delim` a single ASCII-`<0x80` byte `Literal`, `n` a positive integer `Literal` with `1 ≤ n ≤ 8` (`SPLIT_PART_MAX_N = 8`, the O(2ⁿ) inline-unroll bound); **(c)** every `ColRef` resolves to a column on the enclosing target table (cross-table refs impossible by construction; unknown column → `UNSUPPORTED{expr}`); **(d)** a `Cast` target is portable by the closed `CastTarget` enum. `TargetScope::structural_only` **skips (c)** when the caller couldn't resolve the live schema yet — the apply/render seam re-runs (`validate_ir_resolved`). A self-contained `createTable` DOES resolve (c) against its own declared columns at load.

**DoS guard:** `walk_depth` enforces `MAX_EXPR_DEPTH = 128` (`validate.rs:3866`), owned by the validator (an explicit counter), not left implicit to serde's `recursion_limit`.

### 7.7 The immutable-context volatility backstop

Top-level `now()`/`uuidV4()` imports make volatile nodes *type-reachable* in immutable slots. **The Rust validator is the authoritative backstop**. Three-function design:

1. **Volatility classification** — `ExprVolatility { Immutable, Stable, Volatile }` (`validate.rs:326`). `scalar_fn_volatility`: `CurrentSetting`/`CurrentUser` are **Stable**; the rest are **Immutable**. `synth_fn_volatility`: `Now`/`GenRandomUuid` are **Volatile**; `ConcatWs`/`SplitPart` are **Immutable**.
2. **The recursive walker** — `first_volatile_function(expr) -> Option<&'static str>` (`validate.rs:388-441`) descends the entire closed `Expr` AST and returns the name of the first volatile function.
3. **The enforcer** — `validate_immutable_expr_context(expr, context, …)` (`validate.rs:443`) emits `CODE_IMMUTABLE_CONTEXT_VOLATILE` (`kind: Expr`) with a reason naming the function and the context.

**The four immutable contexts:** `"CHECK constraint"` (createTable/addConstraint/createDomain; `:1294,1469`), `"index expression"` (`:1335`), `"index predicate"` (`:1360,1417`), `"generated column expression"` (`:1387,1593`). At each callsite the structural `validate_expr` runs first, then the backstop. Volatile functions remain legal in defaults, DML `set`/`where`, and `insert` values.

### 7.8 "Fail at the earliest layer" — which gate lives where

Three layered gates catch an error at the shallowest possible seam:

- **tsc (compile-time, JS):** catches shape/arity errors. But names are **plain strings, not live-schema-bound**, so a reference to a dropped column *type-checks cleanly* (`validate.rs:6788-6801`).
- **validate (load-time, Rust — THIS gate):** the authoritative structural gate, everything statically decidable from the IR + target dialect. Runs pre-connect.
- **apply/render (resolve-time, Rust):** re-runs rule (c) with the *live* column set for DML/`setColumnType`/`addConstraint`/`createIndex`, where the live schema is unknown at load. Test `pr5_nonexistent_column_name_fails_at_apply_not_at_load_with_structured_error` (`validate.rs:6802`) pins that a bad name is caught at apply with the *same* structured `AuthoringError`, never a silent mis-apply.

The one deliberate exception (A3): raw view-body validation calls the guard's read-only body scanner after the structural `SELECT` checks.

### 7.9 Advisory analysis — non-blocking lint (contrast with the hard gate)

Distinct from the hard `AuthoringError` gate above, `analysis/analyze.rs` (today `crates/zeroship-migrate-postgres/src/analysis/analyze.rs`) is an Atlas-style **advisory** lint suite. Its module doc is emphatic (`analyze.rs:10-21`): "*These are ADVISORY, NEVER load-bearing for security … Nothing here denies, blocks, or gates anything.*" An analyzer false-negative is a quality regression, not a security hole; the guard + role + approval gate remain the security boundary. It flags operationally-risky-but-not-a-threat migrations (data loss, backward-incompatible renames, lock-heavy DDL, full-table rewrites, un-validated constraints, missing FK indexes) and attaches a safer expand-contract suggestion.

**Two severities** (`Severity`, `analyze.rs:41-49`): `Warning` (downtime/data-loss/breaking — the migration still applies, a heads-up) and `Notice` (softer performance/footprint note). An `Advisory` (`analyze.rs:58-69`) carries a stable `rule: &'static str`, a `severity`, a human `message`, and an optional `suggestion` — mirroring the guard's `denylist::rule` "data-not-logic" convention.

**The full advisory rule set** (`analyze.rs:126-159`), all `Warning` unless noted:

| Rule id | Flags |
| --- | --- |
| `DATA_SECURITY_DESTRUCTIVE_OPS_WARN` | `destructive_ops = "warn"` saw a destructive op |
| `DATA_SECURITY_UNCLASSIFIED_OPS_WARN` | `destructive_ops = "warn"` saw a statement not positively classified non-destructive |
| `DESTRUCTIVE_DROP` | `DROP TABLE`/`DROP COLUMN`/`DROP CONSTRAINT` — irreversible data loss |
| `BACKWARD_INCOMPATIBLE_RENAME` | `RENAME COLUMN`/`RENAME TABLE` — breaks code reading the old name |
| `LOSSY_TYPE_CHANGE` | `ALTER COLUMN … TYPE` — may lose data / rewrites the table |
| `ADD_NOT_NULL_NO_DEFAULT` | `ADD COLUMN NOT NULL` with no default — fails on a non-empty table |
| `SET_NOT_NULL_FULL_SCAN` | `ALTER COLUMN … SET NOT NULL` — full table scan under lock |
| `CONSTRAINT_NOT_VALIDATED` | `ADD CONSTRAINT` (FK/UNIQUE/CHECK) without `NOT VALID` — validates all rows under lock |
| `NON_CONCURRENT_INDEX` | plain `CREATE INDEX` (not `CONCURRENTLY`) — blocks writes for the build |
| `TABLE_REWRITE` | an `ACCESS EXCLUSIVE` table rewrite (volatile-default ADD COLUMN / ALTER TYPE) |
| `FK_WITHOUT_INDEX` (`Notice`) | an FK referencing column with no supporting index in the same migration |
| `TRUNCATE_DATA_LOSS` | `TRUNCATE` — irreversible, not MVCC-rolled-back the way `DELETE` is |
| `LOCK_HEAVY_MAINTENANCE` | `CLUSTER`/`VACUUM FULL`/non-concurrent `REINDEX` — heavy lock for its duration |

`analyze(sql)` runs every analyzer over a parseable statement (unparseable SQL yields no advisories — the guard already denies it); `analyze_migration(&Migration)` runs it over a `Migration.up`, the seam the declarative differ uses to attach operational advisories to each generated migration. These advisories surface (never gate) in `GuardReport.advisories`, the `submit_migration` outcome ([§11.8](#11-platform-self-hosting--build-integration)), and the CLI `lint` verb ([§12.8](#12-testing--operating)).

### 7.10 Validation boundaries

- Rule (c) at load is **skipped** for DML/`setColumnType`/`addConstraint`/`createIndex` — enforcement is deferred to the apply seam (by design). A load-time `validate_ir` does *not* fully guarantee column existence for those ops.
- Qualified `ColRef` is accepted *structurally*; the full `QUALIFIED_REF_UNKNOWN_TABLE` FROM-set check, `AGG_POSITION_INVALID`, and the `Dialectal` per-leg budget ratchet are enforced by the view/FROM builder.
- `CODE_OP_OUTSIDE_RECORDER` is defined here but documented as emitted JS-side; no Rust emission site found (consistent).

---

## §8 One IR, three dialects: render & portability

This section covers the *portability model*: how a single deserialized IR is validated per target, lowered to dialect-distinct SQL, and applied through three concrete backends that share one orchestration trait. The *durability* of that apply is [§9](#9-the-apply-engine--durability); the *security* of the guard is [§10](#10-security-first-design).

### 8.1 The mental model: portable IR → per-target validate → dialect-distinct render → dialect-coupled apply

A migration is authored once (JS `op.*` DSL → serialized IR). That single IR flows through three dialect-aware stages, each keyed on a `target_dialect`:

1. **Validate (`model::validate`)** — a per-target static gate ([§7](#7-the-validate-gate--error-taxonomy)). For a chosen dialect, every op is checked against its declared `Support`; an unsupported op/feature/expression is refused *fail-closed at authoring time*, before any SQL is generated.
2. **Render/lower (`render`)** — the surviving ops are lowered to **dialect-distinct executable shapes** (§8.5).
3. **Apply (`apply::backend`)** — the lowered plan is executed through the `MigrationBackend` trait.

The key invariant: **the apply *orchestration* is dialect-agnostic and single-sourced in `apply::executor`; only the dialect-coupled seams sit behind `MigrationBackend`** (`apply/backend/mod.rs:1-37`). Static dispatch (`<B: MigrationBackend>`) means native `async fn` in trait, no `dyn`, no boxing on the apply hot path.

### 8.2 The three backends and their transport

All three implement `trait MigrationBackend` (`apply/backend/mod.rs:166`). The distinguishing axis is *how bound SQL reaches the server* — and the crate carries **zero `compio-mysql`**; MySQL is driven by the real npm `mysql2` driver inside a V8 isolate.

| | PostgresBackend | SqliteBackend | MysqlBackend |
| --- | --- | --- | --- |
| Source | `postgres.rs:26` | `sqlite/mod.rs:50` | `mysql/mod.rs:45` |
| `dialect()` | `SqlDialect::Postgres` | `SqlDialect::Sqlite` | `SqlDialect::Mysql` |
| Transport | **Native `compio-postgres::Client`** (io_uring, zero-tokio) | **In-process embedded SQLite** via a hardened `MigrationActor` | **Live MySQL over `node:net`** — a Trusted V8 isolate runs vendored, unmodified `mysql2/promise` |
| Session (`SessionSnapshot`) | `PgSessionSnapshot { statement_timeout, lock_timeout, search_path }` | no-op (no GUCs) | `MysqlSessionSnapshot { innodb_lock_wait_timeout, sql_mode }` |
| Project apply-lock | `pg_advisory_lock(int4, int4)` from `hashtextextended($1, 0)` | structural (single actor serializes) | `GET_LOCK/RELEASE_LOCK`, name capped 64 chars |
| `ddl_is_transactional()` | `true` | `true` | `false` — MySQL auto-commits DDL, forcing the two-phase path for every migration |
| Role confinement | least-priv `migrator` role, `SET LOCAL ROLE` + unconditional `RESET ROLE` | two-mode `prepare`-time **authorizer** | dedicated migrator account |
| `shadow()` dry-run | `Some(PgShadow)` | `None` (dev applies only trusted descriptor DDL) | (see impl) |
| `online()` expand-contract | `Some(PgOnline)` | `None` (every existing-table change is a rebuild) | (see impl) |

**The MySQL host driver in detail.** There is no in-engine transport module and no driver isolate. MySQL rides the same `driver::SqlSession` seam Postgres does; the host supplies the impl and reaches the server with the real `mysql2` npm driver in the Node process. Bound values cross as native `?` placeholders — never string-interpolated. The engine opens no socket, so it applies no network policy: whatever the Node host connects to is bounded by the host, not here.

### 8.3 The DIALECT TABLE — the single source of dialect truth

Per-`(op-kind, variant)` dialect disposition is a **generated const table**, `DIALECT_TABLE: &[DispositionRow]` (`model/dialect_table.rs:58`). Each row records a `Disposition` for `postgres`/`sqlite`/`mysql`:

```rust
pub enum Disposition {
    Portable,               // core construct that renders/validates here
    TransparentDegradable,  // native where supported, absence-tolerable elsewhere
    Vendor,                 // vendor-tier construct admitted on this dialect
    Unsupported,            // refused on this dialect
}
```
`Disposition::is_supported()` is "everything except `Unsupported`" (`support.rs:20-22`).

> **Counts — read carefully.** A coarse `grep -cE 'DispositionRow \{' src/model/dialect_table.rs` returned **91** when this guide was written, because it matches the `pub struct DispositionRow {` declaration and the `impl DispositionRow {` block in addition to the table rows. The generated `DIALECT_TABLE` itself had **89 row literals**, and `grep -cE 'kind: "'` returned **89**, so it covered **89 `(kind, variant)` dispositions**. (Both totals have moved since; re-measure against the current table rather than quoting these.) These 89 dispositions are **not** the same as the **54 `Op` kinds** ([§6.2](#6-the-ir--its-wire-contract)): one op kind (e.g. `addConstraint`, `createTrigger`, `createTable`) has multiple variant rows (`fkSimple`/`unique`/`check`/`exclusion`; `bodySimple`/`executeFunction`/…; `base`/`partitioned`/`partitionedCollapse`). So "89 disposition rows" and "54 op kinds" are different axes and must not be conflated.

**Generation & freshness gate.** The table is emitted from a hand-authored sidecar `dialect-support.toml` by `packages/zero-migrate/scripts/gen-dialect-table.mjs`, which writes **two** artifacts (the Rust const + the TS mirror `packages/zero-migrate/src/generated/dialect-table.ts`). Regenerate with `pnpm --filter @zeroship/migrate gen:dialect-table`. A regenerate-and-byte-diff CI gate pins both artifacts against the sidecar.

`Op::support()` **reads** `DIALECT_TABLE` at runtime keyed on `Op::op_kind_and_variant()` (`ir.rs:3412-3425`); `support_cell` maps `Unsupported → unsupported(CODE_UNSUPPORTED, reason)` and `Portable|Vendor|TransparentDegradable → supported(render_mode)`. Only the dialect gate is table-sourced; the *render strategy* (`RenderMode::Offline` vs `LiveResolved`) and diagnostic wording stay in Rust because they are not dialect truth.

`crates/zeroship-migrate/tests/dialect_matrix/dialect_table_faithfulness.rs` pins that `Op::support` and the generated table agree.

**Notable dispositions** (P=Portable, V=Vendor, U=Unsupported, TD=TransparentDegradable):

| kind / variant | PG | SQLite | MySQL |
| --- | --- | --- | --- |
| `addColumn` base / identity / nextvalDefault | P/P/P | P/U/U | P/U/U |
| `addConstraint` unique | P | U | P |
| `addConstraint` fkSimple / fkComposite | P | P | P |
| `addConstraint` check / exclusion / fkNotValid | P | U | U |
| `addConstraint` fkNoLocalColumn | U | U | U |
| `createTable` base / partitioned / partitionedCollapse | P/P/P | P/U/**TD** | P/U/**TD** |
| `createIndex` base | P | P | P |
| `createIndex` exprElement / partialWhere | P | P | **U** |
| `createIndex` pgOnlyMethodOrFeature | P | U | U |
| `createPartition` base | P | **TD** | **TD** |
| `createTrigger` bodySimple | U | P | P |
| `createTrigger` executeFunction | P | U | U |
| `createView` base / materialized | P/**V** | P/U | P/U |
| `renameColumn` base | P | P | **U** |
| `insert` base / onConflict | P/P | P/U | P/U |
| `createEnum` / `createDomain` base | P | P | P (emulated) |
| `createSequence`/`alterSequence`/`dropSequence`/`detachPartition`/`validateConstraint` | P | U | U |
| `setColumnType` base / using | P/U | P/U | P/U |
| `comment` base | P | U | U |
| Vendor (V on PG, U elsewhere): `alterRole`, `attachPartition`, `createExtension`, `createFunction`, `createPolicy`, `createRole`, `createSchema`, `drop*`, `grant`, `revoke`, `pgRaw`, `setRls` | V | U | U |
| Fully portable base ops (`backfill`, `delete`, `update`, `dropColumn`, `dropIndex`, `dropTable`, `setColumnDefault`, `setColumnNotNull`, `setTableOptions`, …) | P | P | P |

Rows that are `U/U/U` everywhere (`fkNoLocalColumn`, `renameColumn.existenceGuard`, `createRole.superuserIfNotExists`, `setColumnType.using`, `createTrigger.bodyStatementLevel/bodyTruncateEvent`, `createView.materializedReplace`) encode constructs the engine cannot render *anywhere yet* — refused as `CODE_UNSUPPORTED` regardless of target.

### 8.4 The fail-closed support gate at validate time

The per-target refusal lives in `validate_op_support` (`validate.rs:1912`): it fetches `op.support()`, checks `error_from_decision(support.decision(target_dialect), …)` (an `Unsupported` cell → `AuthoringError{code:CODE_UNSUPPORTED, …, suggested_fix}`), then checks payload-dependent **feature sub-gates** via `check_feature` against `Support::features` (`Feature::PartialIndex`, `SequenceDefault`, `TableLevelCheck`, `ExclusionConstraint`, `TriggerBody`, …; each with its own per-dialect `DialectSupport`). This is fail-closed by construction: an op reaches render only if its target cell is non-`Unsupported`. Vendor ops get a *second, redundant* gate — `validate_vendor_op` refuses any vendor op whose `VendorCapability` the active profile doesn't grant (`CODE_VENDOR_OP_DENIED`), and the Confined creator/AI profile grants none.

### 8.5 How ops render — the lower phase (`render/lower.rs`)

`IrAuthor::lower` turns migration IR into executable plans. The declarative and IR paths share column and table snapshot builders, then render SQL through the selected backend. Table injection comes from the effective policy; lifecycle roles come from declared assignments. The cross-path checks in `crates/zeroship-migrate/tests/ir_contract/ir_author_render_parity.rs` compare the emitted statements.

A lowered plan is a sequence of `PlanStep` (`render/step.rs:48`). The dialect-distinct shapes:

- A **rename** lowers to `RenameStep::PgExpandContract(ExpandContractPlan)` on PG vs `RenameStep::SqliteRebuild(SqliteRebuild)` on SQLite (`render/step.rs:23-27`). The PG expand-contract path is non-destructive (`PlanStep::OnlineRename(PgExpandContract) → destructive == false`, `step.rs:87`); the SQLite 12-step rebuild carries the migration's own `destructive` flag (`step.rs:86`).
- **DML** routes through one of three renderer singletons — `POSTGRES_DML_RENDERER` / `SQLITE_DML_RENDERER` / `MYSQL_DML_RENDERER` (`render/renderer.rs:164`).

Every `PlanStep` carries a `DialectScope` facet (§8.6). The offline `render_plan_sql` / `--sql` preview (`render/sql_preview.rs`) surfaces exactly this lowered SQL — a surfacing layer, not a reimplementation (proven byte-identical against `IrAuthor::lower_steps` by `crates/zeroship-migrate/tests/ir_contract/sql_preview.rs`), and DB-state-dependent ops emit a `-- [runtime-resolved]` label rather than fabricated SQL.

### 8.6 `dialect_scope` — fail-closed off-target + `PgOnly` opt-in

A lowered plan carries a `DialectScope` facet (a **journaled** field, *not* folded into the identity checksum, `render/plan.rs:83`, `render/step.rs:8-17`):

```rust
pub enum DialectScope {
    Both,    // applies faithfully to both Postgres and SQLite
    PgOnly,  // Postgres-only; refused against a SQLite deploy target at load
}
```

The default when lowering is `Both`. When an author uses a construct that is PG-renderable but not portable (an `insert … onConflict`, a raw fragment, an out-of-SQLite-envelope `split_part`), every diagnostic surfaces the remedy: restructure to stay in-envelope, **or mark the migration `dialect_scope=PgOnly`**. A `PgOnly` artifact then loads fine against a Postgres target and is refused with `DIALECT_SCOPE_PGONLY` against a SQLite target *at load* — the portability boundary is a hard error, never a silent mis-apply. `TransparentDegradable` covers constructs that are native where supported and absence-tolerable elsewhere; `createTable.partitionedCollapse` / `createPartition` use it so SQLite/MySQL collapse a partitioned parent into a single table + a no-DDL child leg, gated by `partitionBy.whenUnsupported: "collapse"`.

### 8.7 Intentional Postgres ↔ SQLite (and MySQL) divergences

Two divergence surfaces are deliberately kept distinct: the **runtime** `plugin-db` surface (documented in `docs/reference/sqlite-divergences.md`) and the **migration authoring** surface (this crate). The migration-relevant divergences (`sqlite-divergences.md:18-25`, cross-checked against `support.rs`):

| Construct | Postgres | SQLite | MySQL | Engine behavior |
| --- | --- | --- | --- | --- |
| Enum / domain types | standalone objects | inline column type + `CHECK`/default | emulated | logical constraint must match; SQLite creates no named type |
| Table-level `CHECK` | intended closed-AST render (currently validate-refused until expr renderer lands) | validate-refused | refused | `PG_ONLY_TABLE_LEVEL_CHECK` (`support.rs:325`) |
| Table-level FK / UNIQUE | render in `CREATE TABLE` | descriptor path doesn't thread them → validate-refused | supported | `support.rs:406-430` |
| Identity columns | `GENERATED {ALWAYS\|BY DEFAULT} AS IDENTITY` | `INTEGER PRIMARY KEY AUTOINCREMENT` for the sole int-PK case only | by-default, sole PK | `IDENTITY_ALWAYS_PG_ONLY`, `BY_DEFAULT_IDENTITY_SINGLE_PK` |
| Generated columns | STORED only (VIRTUAL fails closed) | STORED or VIRTUAL | — | shared expr AST renders on both |
| Triggers | function-backed (`EXECUTE FUNCTION`) | closed inline `Body` triggers | limited `Body` | the two forms are dialect-specific, fail closed on the other |
| Sequences / exclusion constraints | native | fail closed | fail closed | `PG_ONLY_SEQUENCE`, `PG_ONLY_EXCLUSION_CONSTRAINT` |
| Partial / expression indexes | supported | supported | **unsupported** | `support.rs:443-453` |
| Column-shape drift verify | full `information_schema` type spelling | SQLite **type affinity** only (a within-affinity change is invisible; a genuine affinity change IS detected) | native | `snapshot_schema` seam |

The runtime `plugin-db` divergences (vector metrics, `ST_DWithin`/PostGIS vs haversine, isolation-level honoring, native locking vs WAL single-writer-actor, collation-aware text ordering) are the runtime peer of the same discipline but are not migration-engine behavior (`docs/reference/sqlite-divergences.md:9-25`).

### 8.8 Why this shape

One IR gated per target keeps authoring portable-by-default while letting PG-native power surface through an explicit journaled `PgOnly` opt-in rather than silent lowest-common-denominator emulation; the generated dialect table from a hand-reviewed sidecar makes "which token is supported where" a single reviewable source of truth (proven consistent with the live engine by `crates/zeroship-migrate/tests/dialect_matrix/dialect_table_faithfulness.rs`); the `MigrationBackend` static-dispatch seam lets Postgres remain the richest regression bar while SQLite and MySQL provide dialect-specific behavior without forking the generic executor; and zero `compio-mysql` keeps MySQL a network-confined, TLS-pinned, timeout-poisoned JS-driver isolate inside the platform security boundary.

---

## §9 The apply engine & durability

This section documents the runtime that actually *runs* migrations. The heart is `apply/executor.rs` (the versioned executor), backed by `apply/journal.rs` (the append-only journal), `apply/baseline.rs` (adoption), `apply/precondition.rs`, `apply/backend/postgres/{online,shadow,backfill}.rs`, `ops/squash.rs`, `plan/manifest.rs`, and `approval.rs`. Everything runs async on compio — zero tokio.

### 9.1 The dialect seam

`apply`/`rollback` take a `&compio_postgres::Client` for signature compatibility but immediately construct a `PostgresBackend` and delegate the whole body to code generic over `MigrationBackend` (`executor.rs:809,3029`). The orchestration — partition, drift gate, squash/expand gates, ordering, the two-pass execution, the repeatable phase — is dialect-agnostic in `apply_locked`/`execute_pending`; every dialect-coupled leaf goes through `backend` methods. `BackendError` boxes any `Error + Send + Sync` (callers `downcast_ref::<compio_postgres::Error>()` for a SQLSTATE); non-PG backends surface text through `ApplyError::Backend(String)`.

### 9.2 Migration ordering (UUIDv7)

A migration's identity is `MigrationId` = `mig_<base36(UUIDv7)>`, defined in
`crates/zeroship-migrate-ir/src/migration.rs`. Its fixed-width encoding makes
lexical order match UUID order. Ordering is a version-tiebroken topological
sort over `depends_on`, implemented by `topo_order_version_tiebroken` in
`crates/zeroship-migrate-backend/src/executor.rs`. The apply path and integrity
manifest share that ordering, so their output cannot diverge. Missing
dependencies and dependency cycles abort before execution. Net-applied and
squash-superseded versions enter the graph as pre-satisfied edges, allowing an
expand and its contract to land in separate deploys.

### 9.3 Advisory-lock concurrency control & `LockMode`

Every apply/rollback/baseline serializes on a per-project advisory lock computed server-side by splitting `hashtextextended($project_id, 0)` into the two signed `int4` arguments of `pg_advisory_lock(int4, int4)`. The lock is held for the whole operation and released on every exit path. The 64-bit hash still has a theoretical collision space, but a collision only serializes two unrelated projects on the same PostgreSQL database/datastore; it cannot mix their schema-confined work, and the probability is negligible at expected per-datastore densities. PostgreSQL's database-scoped advisory lock tags also mean the same key in two databases on one cluster does not contend.

`LockMode` (`executor.rs:88-95`) handles the multi-sub-batch declarative deploy: the outer `apply_declarative` acquires the lock once and threads `LockMode::AlreadyHeld` into each inner `apply_with_lock`, so sub-batches skip the per-batch acquire/release — the lock is taken exactly once and freed exactly once, never freed between sub-batches where a second deploy could interleave. `AlreadyHeld` gates *only* the lock; per-sub-batch session hygiene still runs every time. **SQLite** achieves the same serialization structurally: a single migration connection, `BEGIN IMMEDIATE` taking the RESERVED write lock — "race-free by construction."

### 9.4 Session hygiene (no leaks onto pooled connections)

`snapshot_session` captures the three GUCs (`search_path`/`statement_timeout`/`lock_timeout`) up front; on every exit it does an **unconditional** `RESET ROLE` (even if the snapshot failed — L1) followed by best-effort `restore_session` via `set_config(...)` with bound literals. The txn path uses `SET LOCAL` (auto-reverted at COMMIT/ROLLBACK) so it never mutates the session; only the non-txn path mutates it, and `restore_session` is the backstop.

### 9.5 Least-privilege role bracketing

The migrator role's grant on the meta schema is revoked, so the journal must be written by the admin. Both paths bracket only the `<up>`/`<down>` under `SET [LOCAL] ROLE migrator` and `RESET ROLE` *before* the journal write, inside the same transaction (`executor.rs:2112-2141`, `2498-2519`, `3422-3447`). The `search_path` is pinned to the **project schema only** — the meta schema is off the migration-time path so an unqualified name in `up` can never resolve to the journal. Per-migration `timeout_ms`/`lock_timeout_ms` overrides exist; the lock-timeout default is a short 3s fail-fast envelope a single planned migration can raise for a maintenance window.

### 9.6 The two-pass apply flow

`apply_locked` (`executor.rs:1020`) runs all-up-front-then-execute so a denied or malformed batch applies *nothing*:

1. **Bootstrap** the journal idempotently (`ensure_journal`).
2. **Pre-flight** repeatable well-formedness (`RepeatableCannotSquash`, `RepeatableHasDown`, `OnceOnlyDependsOnRepeatable`).
3. **Partition** versioned vs repeatable (the full set is retained for the drift check so repeatables aren't flagged as orphans).
4. **Drift/tamper gate** — every net-applied version must still match its recorded checksum or `ApplyError::ChecksumDrift` (hard abort). Shares one implementation with the read-only status/drift API. Orphans are logged, not fatal.
5. **Expand/contract gate** + **squash gates** (`ExpandNotApplied`, `SquashAlreadyApplied`, `SquashPartialOverlap`, `OverlappingSquashes`).
6. **Compute `pending` = set − completed − superseded**, topologically ordered.
7. **FIRST PASS (static, all-up-front)** — over *every* pending migration, run the per-dialect guard over `up` and, for two-phase migrations, `validate_non_txn` idempotency. A denial (`ApplyError::Guard`) or non-idempotent non-txn `up` (`NonIdempotentNonTxn`) aborts before *any* migration executes (the H1 hoist).
8. **SECOND PASS (execute)** — `execute_pending` evaluates preconditions read-only under the lock, then applies each migration.
9. **Repeatable phase** — `apply_repeatables` re-applies each repeatable iff its checksum differs from the latest journaled `repeatable`-kind checksum.

Only the *static* checks are all-or-nothing; a migration failing at *execution* still legitimately leaves earlier ones applied. The non-txn idempotency validator (`validate_non_txn_idempotent`, `executor.rs:646`) parses `up` with `pg_query` and rejects `CREATE INDEX CONCURRENTLY` / `ALTER TYPE … ADD VALUE` without `IF NOT EXISTS`, and forbids bare DML on the non-txn path — because recovery re-runs `up` verbatim and a bare INSERT would double-apply (regression test at `executor.rs:3662`).

### 9.7 Transaction model — the atomic (default) path

`apply_transactional` (`executor.rs:2010`):

```text
-- fail-closed identifier quoting rendered BEFORE BEGIN so an IdentQuoteError leaves no dangling txn
BEGIN
SET LOCAL search_path / statement_timeout / lock_timeout   -- txn-scoped, admin
[existence-guard catalog probe under the held lock]        -- no TOCTOU
SET LOCAL ROLE migrator                                    -- brackets <up> only
<up>
RESET ROLE                                                 -- back to admin, mid-txn
INSERT schema_migrations (event_kind='applied', phase='completed', 'success', kind)
[INSERT supersedes edges if fresh-path squash]             -- same txn
COMMIT
```

DDL + journal row commit **atomically in one transaction**, so a crash leaves *applied+recorded* or *neither*. Any failure ROLLBACKs and returns a typed error (`MigrationFailed` for `up`, `Journal` for the journal insert). `RESET ROLE` mid-transaction does not end the txn. The existence-guard `decide` (pure Rust over the snapshot) yields `RunBare`, `SatisfiedNoop` (skip `up` + role switch but still journal `completed`), or `FailDrift` → ROLLBACK + typed `ExistenceGuardDrift` (never a silent skip over a divergence). On SQLite, `run_apply_txn` does `BEGIN IMMEDIATE` → CreatorUp → `up` → EngineJournal → INSERT → COMMIT as separate prepare/execute calls so the authorizer mode flips between them; its rollback path probes `is_autocommit()` and returns `Poisoned` if the connection is still wedged in a transaction.

### 9.8 Two-phase non-transactional path & crash recovery

Some DDL (`CREATE INDEX CONCURRENTLY`, `ALTER TYPE … ADD VALUE`, `VACUUM`, `DROP INDEX CONCURRENTLY`) cannot run in a transaction. `apply_non_transactional` (`executor.rs:2377`) uses a **two-phase protocol** around the *mutable* `schema_migrations_inflight` side-table:

```text
if had_inflight: recover_non_transactional(...)   -- crash recovery
record_started(version)                            -- phase 1: inflight marker (admin, ON CONFLICT DO NOTHING)
SET ROLE migrator; <up>; RESET ROLE                -- the non-txn DDL self-commits
record_completed(...) + clear inflight marker      -- phase 2: immutable row (admin)
```

A crash between phase 1 and 2 leaves a **lone `started` marker with no `completed` row**. On the next apply, `applied()` returns it as a `Phase::Started` entry, `execute_pending` sees `had_inflight = true`, and `recover_non_transactional` runs *before* the `SET ROLE`. Recovery leans on the required idempotency of non-txn `up`s: it performs exactly one cleanup `IF NOT EXISTS` can't do itself — **drop the INVALID index residue** of an interrupted `CONCURRENTLY` build (an INVALID index satisfies `IF NOT EXISTS`, so it would otherwise never be rebuilt), scoped to only the index name(s) this migration's `up` names and only if `pg_index.indisvalid = false` (so an out-of-band invalid index from a human is never collateral). It then clears the marker, **re-arms a fresh `started` marker**, and **re-runs the idempotent `up`** verbatim. This is safe for both crash cases: (a) failed mid-DDL — INVALID index dropped, `up` re-runs clean; (b) succeeded then crashed before recording — the object exists and valid, `IF NOT EXISTS` no-ops, `completed` finally lands. The re-arm is load-bearing: without it, a *second* crash during recovery would observe `had_inflight = false` and permanently wedge a half-built index. `apply_non_transactional` returns `true` when it was a recovery (surfaced in `ApplyOutcome.recovered`). SQLite has *no* non-txn path — `transaction:false` is rejected with `NonTxnUnsupportedOnDialect`.

### 9.9 The immutable journal

The journal of record is `<meta>.schema_migrations` — a **single consolidated events table**, one row per migration *event* (`journal.rs:437-459`):

| column | type / constraint | role |
| --- | --- | --- |
| `event_seq` | `BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY` | native total order — strictly increasing, never ties even within one txn |
| `event_kind` | `TEXT CHECK IN ('applied','rolled_back')` | event direction |
| `version` | `TEXT NOT NULL` (NOT unique) | migration id; multiple rows per version across rollback↔re-apply |
| `name`, `checksum`, `"by"`, `"at"`, `exec_ms` | | actor/timestamp/exec metadata |
| `phase` | `CHECK NULL OR IN ('started','completed')` | applied-only |
| `outcome` | | applied-only |
| `kind` | `CHECK NULL OR IN ('apply','baseline','squash','repeatable')` | migration type of an applied event |

A `schema_migrations_event_shape` constraint enforces per-`event_kind` shape (`applied` ⇒ `kind`/`phase`/`outcome` NOT NULL; `rolled_back` ⇒ all three NULL). The native IDENTITY total order matters because `now()` can tie across two events in one transaction — the *latest event per version* unambiguously decides net state. **Immutability triggers:** `ensure_journal` installs a `RAISE EXCEPTION 'migration journal is append-only'` plpgsql function and attaches **two** triggers to each append-only table — `BEFORE UPDATE OR DELETE FOR EACH ROW` **and** `BEFORE TRUNCATE FOR EACH STATEMENT` (the TRUNCATE trigger is essential — row-level triggers don't fire on TRUNCATE). Four tables get them: `schema_migrations`, `schema_migrations_supersedes`, `schema_pending_contracts`, `schema_deploy_recovery`. The billing-ledger pattern: a correction is a *new* row. `applied()` computes net state with `DISTINCT ON (version) … ORDER BY version, event_seq DESC`, keeps versions whose latest event is `applied`, then `UNION ALL`s the lone `started` markers. Companion reads: `net_rolled_back`, `history`, `applied_count`, `superseded_versions` (restricted to genuine `kind='squash'` events — #4), `latest_completed_checksums` (filtered to `kind='repeatable'`). The mutable `schema_migrations_inflight` side-table is deliberately *not* immutable. On SQLite the journal is the attached `_mig` database, same shape with `event_seq INTEGER PRIMARY KEY AUTOINCREMENT`, immutable via `DEFENSIVE=ON` + `trusted_schema=OFF` + the per-statement authorizer + `BEFORE UPDATE/DELETE RAISE(ABORT)` triggers.

### 9.10 Journaled `kind` and the tamper anchor

`JournaledKind` ∈ `{Apply, Baseline, Squash, Repeatable}` (`journal.rs:78-120`) is the *recorded* identity-class, load-bearing for the tamper guard: the drift check anchors the repeatable-drift exemption on the **journaled** `kind`, never on the attacker-suppliable `flags.repeatable`. So flipping an applied once-only migration to `repeatable=true` is a kind mismatch ⇒ tamper abort. `EventKind` ∈ `{Applied, RolledBack}` is the orthogonal *direction* discriminator. Every read-back routes stored strings through typed `parse`, surfacing `BadPhase`/`BadKind`/`BadEventKind` on a corrupted/out-of-band-mutated row.

### 9.11 Approval and `ApprovalScope` (version-scoped, fail-closed)

The coarse gate is `Approval` (`approval.rs:14-27`): `Approval::None` (runs only a non-destructive batch) vs `Approval::Approved` (a destructive batch — `DROP`/`TRUNCATE`/lossy-type-change `up`, or any rollback since a `down` is inherently destructive — may run). The AI never auto-applies destructive ops; it passes `None` and surfaces the approval-required error to a human. `Approval` lives in its own module (not `engine`) precisely because the **executor** is itself a public entry point a caller/retry-loop can drive directly, bypassing the engine gate — so the approval gate must live at the executor layer too, the same defense-in-depth pattern as re-running the guard + role (`approval.rs:1-12`).

Layered *orthogonally* on top is **`ApprovalScope`** (`approval.rs:29-76`). It answers which destructive ops were individually reviewed, so approving one reviewed online rename can **never blanket-authorize** an unrelated co-bundled destructive op (a `dropColumn`/`dropTable`) that was not reviewed:

```rust
pub enum ApprovalScope {
    All,                                              // blanket: any destructive op may run
    Versions(std::collections::BTreeSet<String>),    // fail-CLOSED: only listed version-ids
}
impl ApprovalScope {
    pub fn admits(&self, version: &str) -> bool {     // consulted ONLY for destructive ops
        match self { Self::All => true, Self::Versions(set) => set.contains(version) }
    }
}
```

The two compose: a destructive op runs iff `Approval::Approved` **AND** the scope admits its version-id. A NON-destructive op never reaches `admits` — scope only ever *further restricts* destruction, never widens it. `ApprovalScope::All` is fail-**open** by explicit trusted intent (dev CLI `--yes`, rollback, shadow dry-run, resolve-pending). `ApprovalScope::Versions` is fail-**closed** — an empty set authorizes NOTHING destructive **even under `Approved`**; there is no "unrecognized scope ⇒ allow" arm. This is the mechanism behind the platform's `migrations:approve` / per-version approval ([§11.3](#11-platform-self-hosting--build-integration)): the out-of-band approved-apply surface constructs `Versions` from the reviewed version-id set.

### 9.12 Preconditions — state/data-conditional apply

`model/precondition.rs` + `apply/precondition.rs` provide Liquibase-style preconditions evaluated against the live DB *before* a migration's `up` runs, read-only under the held lock (during the SECOND pass, §9.6). A `PreconditionCheck` carries a `check: Precondition` + an `on_unmet: OnUnmet` policy.

**`Precondition` variants** (`precondition.rs:47-89`): structured, engine-built **parameterized catalog queries** (injection-safe) — `TableExists { table }`, `TableNotExists { table }`, `ColumnExists { table, column }`, `ColumnNotExists { table, column }`, `RowCount { table, op: CmpOp, value: i64 }` (`count(*) <op> value`) — plus one escape hatch, `SqlBoolean { sql }`, an **untrusted single read-only `SELECT`** returning one boolean, run behind the guard + migrator role + a single-read-only-SELECT shape gate. **`CmpOp`** (`precondition.rs:7-20`) is the full comparison set `Eq | Ne | Lt | Le | Gt | Ge` with a `const fn apply(self, lhs, rhs) -> bool`.

**`OnUnmet`** (`precondition.rs:103-149`, default `Halt`):

- **`Halt`** (the default) — abort the whole apply forward: return `ApplyError::PreconditionFailed` and apply NOTHING for THIS migration. The precise semantics: this stops the batch **going FORWARD** — no later-in-order migration is applied after the failing one — but does **not** undo migrations already committed earlier in the same batch (each commits independently). "**Halt is fail-forward-stop, not a batch-wide rollback**."
- **`Skip`** — do not apply or journal THIS migration this run (it stays pending, re-evaluated next deploy) and continue with the rest of the batch — the "apply this once the DB reaches shape X" idempotent-deploy primitive. A skipped migration's declared dependents are transitively held back by the `depends_on` ordering. **Skip relies on COMPLETE `depends_on`:** an *undeclared* dependent is NOT held back and will run against a stale shape — making `Skip` MORE dangerous than `Halt` when deps are incomplete; authors choosing `Skip` MUST declare every real dependency.

`PreconditionError` is the typed failure. Preconditions are folded into `Checksum::of_ir` (§6.5), so a tampered precondition changes the migration's identity.

### 9.13 Squash / supersedes

A **squash** collapses a contiguous prefix of applied history `[v1..vN]` into one equivalent step `S` where `S.up` is the combined/equivalent DDL and `S.supersedes = [v1..vN]` (`ops/squash.rs:1-22`, `MigrationIr.supersedes` at `ir.rs:343-382`). The journal is append-only, so squash never deletes the `v1..vN` events — it models squash as a **supersession**:

- **Existing DB** (already applied `[v1..vN]`) — `squash` (`ops/squash.rs`) journals `S` as a `completed`, `kind='squash'` event **without running its `up`** (baseline-style — the effect is already present) and records the `S → v_i` supersession edges into the immutable `schema_migrations_supersedes` table. The old events remain; `S` supersedes them.
- **Fresh DB** (none applied) — an ordinary `apply` of a set containing `S` runs `S.up` once and **SKIPS** `v1..vN` (the executor's pending computation treats a version superseded by an applied/being-applied squash as satisfied via `compute_superseded`). `v1..vN` are never double-applied; a later migration that `depends_on` a superseded version is satisfied by `S` (they enter ordering as `pre_satisfied`, §9.2).

**The all-or-none rule** (`ops/squash.rs:24-34`): a squash is consistent only at the two extremes — ALL of `[v1..vN]` net-applied (record the supersession, no `up`) or NONE applied (fresh path runs `S.up`). A **partial** overlap is refused both here (`SquashError::PartialOverlap`) and in the executor (`ApplyError::SquashPartialOverlap`); overlapping squashes are refused (`OverlappingSquashes`). Safety: `S.up` is guard-checked even on the recorded-not-run path; squash runs as ADMIN under the project advisory lock; the append-only journal is preserved (an immutable `completed` row stamped `kind='squash'` + immutable supersession edges). `SquashOutcome` reports `{ version, superseded, already_present }` — a re-squash of an already-net-applied `S` is an idempotent no-op.

### 9.14 Online expand-contract, shadow-DB, and cross-deploy pending contracts

Three PG-only online-migration mechanisms sit behind the `MigrationBackend::online()`/`shadow()` capabilities (`None` on SQLite/MySQL):

**`PgOnline` — dual-write expand-contract** (`apply/backend/postgres/online.rs`). An online column rename is authored as an `OnlineIntent::RenameColumn` and lowered by `ExpandContractAuthor` (`render/expand_contract.rs`) into an EXPAND phase (add the shadow column + a dual-write trigger, then a paged backfill) and a CONTRACT phase (`render/step.rs:104` `PgExpandContract`, non-destructive). `run_expand_pg` (`online.rs:78`) runs the EXPAND sequence verbatim under the lock, threading `Approval` + `ApprovalScope` + `LockMode`. The backfill is a bounded paged runner (`apply/backend/postgres/backfill.rs`, `BackfillSpec`).

**`PgShadow` — throwaway-clone dry-run** (`apply/backend/postgres/shadow.rs`). A dry-run proves a migration batch applies cleanly — and, for a declarative deploy, that the resulting schema matches what was *desired* — **without ever touching the real project DB**. It is a throwaway **DATABASE** clone (not a shadow schema) because migration SQL hard-codes the `project_schema` name (`shadow.rs:1-12`). The control plane drives it before a destructive or AI-authored apply: preview the plan against a faithful copy, surface failures + advisories + resulting drift, then decide. Teardown is panic-safe via `FutureExt::catch_unwind`.

**Cross-deploy pending contracts.** Because an expand and its contract can land in **separate deploys**, an outstanding online-rename obligation is journaled in the immutable `schema_pending_contracts` table (`journal.rs:237-263`, DDL at `journal.rs:524-537`). A `PendingContract` carries `{ table, from_col, to_col, ty, pending_version, plan_version, contract_versions }`. The `resolve-pending --apply|--abort <version>` CLI verb ([§12.8](#12-testing--operating)) discharges it: `--apply` journals the contract migrations; `--abort` re-authors the abort DDL (drop the dual-write trigger + `DROP COLUMN IF EXISTS` the shadow column) as plain phase-less `PlanStep::Ddl` steps (`build_abort_steps`, `online.rs:12-42`) and requires the distinct `--acknowledge-shadow-data-loss` flag. The `status` verb surfaces orphan/blocked contracts by keying on `plan_version`, not the deeper `pending_version`.

### 9.15 The integrity manifest (`atlas.sum`-style)

`plan/manifest.rs` computes a single hash over the migration SET — folded in the **canonical executed order** — so a tampered/reordered/inserted/removed bundle is rejected BEFORE any apply (`manifest.rs:1-6`). It adds set-level integrity on top of the per-migration `Checksum` (which catches content **drift**): an **insertion**, a **removal**, a **content edit**, or a **`depends_on` reorder** that changes the EXECUTED order all change the `ManifestHash`. A pure cosmetic file-order change of an additive set (no `depends_on`) is deliberately **INVARIANT** — the executor re-sorts by version, so both file orders execute identically and yield the SAME manifest, so the control plane stamping one file order and the bundle arriving in another does not false-mismatch.

`compute_manifest` folds, in canonical executed order (sharing `topo_order_version_tiebroken` with the executor via `canonical_set_order`, §9.2):

```text
H( DOMAIN ‖ u64_be(count) ‖ for each migration in CANONICAL EXECUTED order:
     u64_be(len(version)) ‖ version_utf8  ‖  u64_be(len(checksum)) ‖ checksum_hex_utf8 )
```

Every variable-length field is length-prefixed (no delimiter-injection/concatenation collision), and a leading domain-separation constant means the manifest hash can never be confused with a per-migration `Checksum`. It folds over the per-migration `checksum` (not raw `up`/`down` text) so it is cheap and reuses the tamper-evident content hash. `verify_manifest` is the pre-apply gate (`executor.rs:1759` shares the canonical ordering).

### 9.16 Baseline (the adoption path)

`baseline` (`baseline.rs:133`) records a baseline migration as a `completed`, `kind='baseline'` event **without running its `up`** — for a DB that already physically carries its schema. Guarantees: **guard-checked** even though it never runs (defense-in-depth, `BaselineError::Guard`); **first-entry-only** (refuses if any net-applied migration exists — `AlreadyManaged`; a different baseline — `ConflictingBaseline`; re-baselining the *same* version is an idempotent no-op returning `already_present: true`); **privileged + serialized** (admin under the project advisory lock); **append-only** (an immutable `completed` row via `record_baseline`, bracketing row + supersession edges in one `BEGIN … COMMIT`). The SQLite arm mirrors this plus a `record_loaded_versions` batch for `db:load`/dump-restore.

### 9.17 Historical rollback model

This subsection records the former engine model. Current TypeScript migrations
do not author a generic reverse phase: schema inverses are synthesized and data
reverses are recorded as `inverse()`.

`rollback` (`executor.rs:3012`) applies the `down` SQL of net-applied-and-not-rolled-back migrations after a `RollbackTarget`, in **reverse topological order of `depends_on`**. `RollbackTarget` ∈ `{ ToVersion(id) (exclusive), Steps(n), All }`, wrapped in a `RollbackRequest` with `RollbackOptions { force, backup_acknowledged }`. Durability/safety:

- **Approval always required** — a `down` is inherently destructive, so `Approval::None` ⇒ `ApprovalRequired` (the executor's own defense-in-depth gate, independent of the engine's).
- **`down` gets the same defenses as `up`** — guarded up-front (a denial aborts the whole rollback before any down runs), executes under the least-privilege migrator role.
- **Append-only journaling** — each `down` + its `rolled_back` append run in one transaction (`rollback_one_transactional`, `executor.rs:3397`); the original `applied` row is never deleted; a rolled-back version becomes pending again and is re-appliable.
- **Full-history contract** — `migrations` must carry the complete applied history (`MissingFromSet`); a `down` must remain available forever.
- **Irreversible (`down: None`)** — refused by default (`Irreversible`); with `force` **and** `backup_acknowledged` it proceeds by *skipping* the step (recorded in `skipped_irreversible`; it stays applied).
- **Non-txn down** — refused up-front (`NonTransactionalDown`) rather than dying late with PG `25001`.

Full `RollbackError` set: `Db`, `Journal`, `Backend`, `ApprovalRequired`, `UnknownTarget`, `MissingFromSet`, `NonTransactionalDown`, `Irreversible`, `ChecksumDrift`, `DownFailed`, `KeptDependency`, `SqliteRebuildRequired`. On SQLite, rollback is additive-only this phase: `DROP TABLE`/`DROP COLUMN`/`DROP INDEX`/`RENAME` reverse natively (SQLite ≥3.35), but a `down` needing the 12-step rebuild is refused up-front with `SqliteRebuildRequired`.

### 9.18 Exactly-once semantics — how it composes

The net guarantee is **exactly-once application per version**, enforced by three cooperating mechanisms: (1) **idempotent re-runs** — `pending = set − completed − superseded` is recomputed from the journal on every apply under the lock, so a retried deploy skips completed versions; (2) **atomic DDL+journal (txn path)** — a crash leaves *applied+recorded* or *neither*; (3) **two-phase marker + idempotent re-run (non-txn path)** — the `started` marker makes a crash detectable, the mandatory idempotency makes the re-run safe, the INVALID-index cleanup recovers case (b), the marker re-arm makes recovery itself crash-safe, and fresh-path squash edges commit in the *same* transaction as the `completed` row. The one honest edge: a batch is exactly-once *per migration*, not atomically *per batch* — an execution failure on migration 5 leaves 1–4 committed (standard migration semantics). Only the up-front *static* validation is all-or-nothing.

### 9.19 Key file map

| Concern | Location |
| --- | --- |
| Apply orchestration, two-pass, ordering | `apply/executor.rs` |
| Immutable journal (schema, triggers, net-state reads) | `apply/journal.rs` |
| Approval + version-scoped approval | `approval.rs` |
| Preconditions | `model/precondition.rs` + `apply/precondition.rs` |
| Squash / supersedes | `ops/squash.rs` |
| Online expand-contract / shadow / backfill / pending contracts | `apply/backend/postgres/{online,shadow,backfill}.rs`, `render/expand_contract.rs`, `journal.rs` (`PendingContract`) |
| Integrity manifest | `plan/manifest.rs` |
| Baseline / adoption | `apply/baseline.rs` |
| SQLite journal + atomic apply + additive rollback | `apply/backend/sqlite/{journal_sql,rollback_sql}.rs` |
| MigrationId UUIDv7 ordering | `model/migration.rs` |

---

## §10 Security-first design

This is the canonical treatment of the security substrate; [§1.3](#1-overview--what--why) and [§2](#2-crate-architecture) cross-ref here rather than re-deriving. `zeroship-migrate` treats every migration as **untrusted input** — creator-authored SQL/op-DSL *and* prompt-injectable AI output flow through the same pipeline. The model is explicitly **defense-in-depth, untrusted-by-default**: confine by DB privilege *and* verify independently at parse time — "belt and suspenders." (Threat vectors enumerated in [§1.2](#1-overview--what--why).)

There are **two independent lines of defense** plus a set of orthogonal capability gates:

1. **Line 1 — the parse-time SQL guard** (`guard/mod.rs`): parses every statement with the real Postgres parser (`pg_query`) and hard-denies the dangerous surface *regardless of the submitted SQL*.
2. **Line 2 — the least-privilege `migrator` role** (`apply/role.rs`): the migration's DDL runs under a deliberately under-privileged Postgres role, so anything that slips past the parser **fails with `permission denied` at execution**.

### 10.1 Guiding principle: gate the execution surface, not a specific op

The single most important idea: **a parser cannot statically detect a specific dangerous operation** — it can only gate the *carrier that could execute one*. Runtime-constructed SQL evades any structural check:

```
DO $$ … EXECUTE format('… %I …', s) … $$   -- target computed at runtime
```

Here the target schema/object never appears as a parseable identifier the guard can confine (`apply/role.rs:6-8`). The guard's answer is not to prove what such a body will do, but to **deny-by-default the statement kinds** that could carry a dynamic escape unless they're on a curated known-safe allowlist, and **backstop the residual with line 2** (the role has no grant to reach anything outside its own schema). The guard's `set_config`/`query_to_xml`/`reg*`-cast handlers acknowledge this limit explicitly — a runtime-constructed argument "is out of parse-time scope — the line-2 defense's job" (`guard/mod.rs:1200-1205`).

### 10.2 Line 1 — the parse-time SQL guard

The guard is "the security heart of the engine" (`guard/mod.rs:1-3`), running **two postures by threat class** (`guard/mod.rs:12-20`): **Deny** (hard error, never auto-confirmed) for RCE/priv-esc/cross-tenant/FS/network; **Flag** (`GuardReport.destructive`, surfaced not denied) for data loss — the gate decides on data loss, the guard only reports it. It is **deny-by-default**: an unrecognized statement that *could* be dangerous is denied.

**The deny-list is data, not logic** (`guard/denylist.rs:1-5`) — flat constant lists, case-insensitive (`list_contains_ci`), auditable at a glance:

| Constant | Threat | Members |
| --- | --- | --- |
| `TRUSTED_LANGUAGES` | only these PLs in `CREATE FUNCTION` | `plpgsql`, `sql` |
| `FORBIDDEN_EXTENSIONS` | FS/network/RCE reach | `dblink`, `postgres_fdw`, `file_fdw`, `mysql_fdw`, `oracle_fdw`, `tds_fdw`, `plpythonu`, `plpython2u`, `plpython3u`, `plperlu`, `pltclu`, `plsh`, `plr`, `plv8`, `adminpack`, `amcheck`, `pg_background`, `lo` |
| `FILE_ACCESS_FUNCTIONS` | read/write server FS or large objects | `pg_read_file`, `pg_read_binary_file`, `pg_ls_dir`, `pg_ls_logdir`, `pg_ls_waldir`, `pg_stat_file`, `lo_import`, `lo_export`, `pg_file_read`, `pg_file_write`, `pg_file_unlink`, `pg_file_rename`, `pg_logdir_ls` |
| `NETWORK_FUNCTIONS` | SSRF + cross-DB reach | `dblink`, `dblink_connect`, `dblink_connect_u`, `dblink_exec`, `dblink_open`, `dblink_fetch`, `dblink_send_query`, `dblink_get_result` |
| `REG_TYPES` | `text → reg*` casts resolve a named object at runtime | `regclass`, `regnamespace`, `regproc`, `regprocedure`, `regtype`, `regoper`, `regoperator`, `regrole`, `regcollation`, `regconfig`, `regdictionary` |
| `NAME_RESOLVER_FUNCTIONS` | literal-carried-schema leak; `setval`/`nextval` cross-tenant mutation | `nextval`, `currval`, `setval`, `to_regclass`, `to_regnamespace`, `to_regproc`, `to_regprocedure`, `to_regtype`, `to_regoper`, `to_regoperator`, `to_regrole`, `to_regcollation`, `pg_get_serial_sequence` |
| `TEXT_RELATION_NAME_FUNCTIONS` | first `text` arg is a foreign relation name | `pg_relation_size`, `pg_total_relation_size`, `pg_table_size`, `pg_indexes_size`, `has_table_privilege`, `has_any_column_privilege`, `has_column_privilege`, `table_to_xml`, `table_to_xmlschema`, `table_to_xml_and_xmlschema` |
| `NAMESPACE_NAME_FUNCTIONS` | dumps a whole schema as XML | `schema_to_xml`, `schema_to_xmlschema`, `schema_to_xml_and_xmlschema` |
| `SQL_STRING_ARG_FUNCTIONS` | first `text` arg is a free-form SQL string the server executes | `query_to_xml`, `query_to_xmlschema`, `query_to_xml_and_xmlschema`, `cursor_to_xml`, `cursor_to_xmlschema` |
| `SET_CONFIG_FUNCTION` | function form of `SET <param>` | `set_config` |
| `OBJECT_ADDRESS_FUNCTIONS` | schema value in an array literal | `pg_get_object_address` |
| `PRIVILEGED_ROLES` | membership grants host-escape | `pg_read_server_files`, `pg_write_server_files`, `pg_execute_server_program`, `pg_read_all_data`, `pg_write_all_data`, `pg_monitor`, `superuser`, `postgres` |
| `PLATFORM_SCHEMAS` | body lexical-scan backstop | `control`, `auth`, `billing` |
| `FORBIDDEN_SET_PARAMS` | `SET`-able params that break confinement | `search_path`, `role`, `session_authorization` |

Stable rule ids (`denylist::rule`, `denylist.rs:239-284`): `copy_program_rce`, `copy_file_access`, `untrusted_language`, `forbidden_extension`, `extension_not_allowlisted`, `alter_system`, `role_management`, `superuser_role`, `privileged_role_grant`, `privilege_management`, `file_access_function`, `network_function`, `forbidden_set_param`, `set_role`, `database_management`, `fdw_management`, `load_library`, `unrecognized_dangerous_construct`, `dangerous_construct_in_body`, `internal_guard_error`, `security_definer_function`, `function_set_search_path`, `owner_change_to_privileged_role`, `unsafe_alter_table_subcommand`, `system_catalog_access`.

**The walk** (`SqlGuard::check` → `check_node`, `guard/mod.rs:537-715`): (1) **statement-kind gate, deny-by-default** — a curated allowlist passes, recognized-dangerous kinds get precise rule ids, every unenumerated kind is denied (`ALTER SYSTEM` always denied; `COPY … PROGRAM`/`COPY … <file>` denied while `COPY … TO STDOUT` is fine; `CREATE FUNCTION` sub-checks reject untrusted languages, `SECURITY DEFINER`, persisted `SET search_path`; FDW/`CREATEDB`/`LOAD` denied); (2) **cross-schema confinement** — any explicit foreign schema anywhere in the full parse tree is `CrossSchema`; (3) **system-catalog reads/writes** — qualified `pg_catalog.*`/`information_schema.*` and unqualified `pg_*` relations; (4) **dangerous function calls** across the whole tree; (5) **belt scans for literal-carried leaks** — `reg*` casts, schema-qualified objects inside `A_Const` literals, `set_config('search_path',…)`, and `query_to_xml('SELECT … FROM control.users',…)` whose embedded SQL is **re-parsed and run through the guard recursively**; (6) **body recursion** into `DO` blocks and function bodies. The full-tree scans serialize each statement subtree to JSON and walk generically (`guard_stmt_json`), not `pg_query::nodes()` (whose hand-written traversal skips column-`DEFAULT`/`CHECK`/`VALUES`/`RULE`-action subtrees). Unparseable SQL is denied (`GuardError::Parse`). Non-Postgres dialects have no libpg_query guard, so raw SQL is refused fail-closed (`SqliteRawSqlRejected`, `MysqlRawSqlRejected`) — the SQLite/MySQL Confined paths accept only descriptor-diff-generated DDL, with the `SqliteBackend`'s runtime authorizer as line-2.

### 10.3 Line 2 — the least-privilege `migrator` role

`apply/role.rs:1-2` builds the line-2 DB-privilege defense: runtime-constructed SQL the parser can't confine "fails with `permission denied` at execution" because the role has no grants on `control`/`auth`/`billing`/other projects' schemas.

**Role model: `NOLOGIN` + `SET ROLE`, not a login role.** `provision_migrator` creates a deterministic `migrator_<project>_<hash>` role as `NOLOGIN`; the executor connects as the privileged admin and runs each migration under `SET ROLE`, `RESET ROLE` on exit. Rationale (`role.rs:14-32`): no per-role passwords to rotate, no connection churn (the admin session already holds the lock + journal writes), identical DB-enforced confinement — a superuser admin that `SET ROLE`s to a `NOSUPERUSER` role is fully constrained (a superuser only bypasses checks while it is *itself* the effective role). The executor uses `SET LOCAL ROLE` on the txn path (auto-reverts at COMMIT), explicit `SET ROLE`/`RESET ROLE` on the non-txn path with journal I/O as admin, and issues `RESET ROLE` **unconditionally** as belt-and-suspenders.

**The grant set** (`role.rs:34-70`, `232-362`): `NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS`; **owns** the project schema with `CREATE, USAGE`; `search_path` pinned to the project schema first then the extension schema(s) (default `public`) only so unqualified extension types resolve; `REVOKE ALL` then `GRANT USAGE` on the extension schema (resolution-only, no `CREATE`); **no grant whatsoever** on `control`/`auth`/`billing`/other project schemas — deny-by-absence.

**Journal immutability the migrator can't drop.** The migrator gets **no access whatsoever to the meta schema** (`role.rs:43-51`, `292-315`). A migration's `up` runs as the migrator, so if it could write the journal it could plant a forged `completed` row (silently suppressing a future migration, since `pending = set − completed`) or a bogus checksum (wedging apply on `ChecksumDrift`). All journal/inflight I/O is done by the **executor as admin**; the migrator has neither `USAGE` on the meta schema nor any grant on `schema_migrations`/`schema_migrations_inflight`. The journal is unforgeable by deny-by-absence, and because the migrator doesn't own the meta schema it "can never drop the journal's immutability trigger." That immutability is enforced *by construction* (the append-only UPDATE/DELETE/TRUNCATE triggers, [§9.9](#9-the-apply-engine--durability)), not by least-privilege alone. Provisioning is idempotent; `deprovision_migrator` cleanly `REASSIGN OWNED` + `DROP OWNED` + `DROP ROLE`.

**Known limitations** (`role.rs:72-82`): `CREATE FUNCTION … SET search_path` is guard-denied but not role-backstopped (harmless — migrator functions default to `INVOKER`, and `SECURITY DEFINER` is guard-denied); the migrator can read `pg_roles` (role names are not treated as secrets).

### 10.4 The capability-composition model (VENDOR ops)

The privileged Postgres primitives exported from `@zeroship/migrate` are gated not by a hard-coded profile name but by a **composition of boolean capability flags + a schema allowlist** (`model/capability.rs:1-20`) — "the gate keys on `caps.allow_role`, never on `trust == Confined`." Every privileged op declares the closed set of `VendorCapability` it needs (`capability.rs:70-94`):

| Variant | `flag_name` | Gates |
| --- | --- | --- |
| `Extension` | `allowExtension` | `CREATE/DROP EXTENSION` |
| `Schema` | `allowSchema` | `CREATE/DROP SCHEMA` |
| `Role` | `allowRole` | `CREATE/ALTER/DROP ROLE`, `DROP OWNED BY` |
| `Grant` | `allowGrant` | `GRANT`/`REVOKE` |
| `Rls` | `allowRls` | RLS `ENABLE`/`FORCE`/`DISABLE`/`NO FORCE` |
| `Partition` | `allowPartition` | `ALTER TABLE ATTACH PARTITION` |
| `Policy` | `allowPolicy` | `CREATE/DROP POLICY` |
| `Function` | `allowFunction` | `CREATE/DROP FUNCTION` |
| `RawSql` | `allowRawSql` | the gated `pgRaw` escape |
| `RawViewBody` | `allowRawViewBody` | raw view-body SELECT escape |
| `MaterializedView` | `allowMaterializedView` | PG materialized views |

`Op::vendor_capabilities` (`ir.rs:4129-4207`) maps each op to its capabilities exhaustively (portable-core ops return an empty vec — not vendor-gated; a raw materialized view requires *both* `RawViewBody` and `MaterializedView`). Three presets (`capability.rs:175-244`): **`confined()`** (every flag false, no cross-schema — every vendor op refused fail-closed); **`operator()`** (every flag true); **`local()`** (structural vendor DDL enabled but role management and the raw escape disabled — a dev/CI posture **not wired to any `TrustProfile`**).

**Two-gate enforcement** (`render/vendor.rs:16-20`): **Gate 1 — validate/load** (`validate_vendor_op`, `validate.rs:2373-2458`) derives `VendorCapabilities::from_scope(schema_scope)` and refuses each ungranted required capability fail-closed with `CODE_VENDOR_OP_DENIED`; **Gate 2 — lower/render** (`render_vendor_op`, `render/vendor.rs:218-579`) renders raw fields (function `body`, `pgRaw`) verbatim, and the whole rendered statement is then `pg_query`-parsed by the guard so the body is scanned by the same deny-list. Even `CREATE ROLE … SUPERUSER` renders verbatim precisely because the guard's deny-list catches it downstream.

**The non-spoofable trust signal: `from_scope`** (`capability.rs:274-284`). The `SchemaScope` is produced only by capability-gated `GuardConfig` constructors: `None`/`Single(_)` ⇒ `confined()`; `Allowlist(list)` ⇒ `operator()` with `schemas = list` (Platform); `Unconfined` ⇒ `operator()` with no validate-time cross-schema confinement (Trusted).

### 10.5 Confined creator vs operator/platform trusted capability

The trust posture is set at the capability-bearing call site, never derived from SQL content (`model/policy.rs:3-4`). `TrustProfile` (`policy.rs:27-53`):

| Profile | Line-1 behavior | Constructed by |
| --- | --- | --- |
| **`Confined`** | Full deny-list + `Single(project_schema)` cross-schema pin. Untrusted creator/AI (today's default). | `GuardConfig::confined()` — needs **no** token |
| **`Platform`** | Widened: role/grant/policy/schema/RLS admitted against a fixed schema `Allowlist`, but SUPERUSER/host reach *still denied*. | `GuardConfig::platform(_cap, …)` — **requires** an `OperatorCapability` token |
| **`Trusted`** | No untrusted boundary — deny-list, cross-schema, and body walks skipped entirely (dbmate-parity). Destructive classification still derived for the CLI `--yes` gate. | `GuardConfig::trusted(_cap)` — **requires** an `OperatorCapability` token |

The trust boundary is closed **by construction** (`guard/mod.rs:71-78`, `compile_fail` doctests): `GuardConfig`'s fields are all private; `platform`/`trusted` are `pub(crate)` and require `&OperatorCapability`; `OperatorCapability` is a `pub(crate)` zero-sized token an external crate cannot even *name*. The creator submission ingress is hard-wired to `Confined` with no API path to Platform/Trusted — "trust separation is the call-site invariant, not tool separation."

**How the widened Platform posture stays safe.** Platform widens privilege *within the DB, never host reach*: role management is admitted iff `allow_role` **but `SUPERUSER` is hard-denied in ALL profiles including Platform** (`superuser_role`); `GRANT`/`REVOKE` are admitted iff `allow_grant` but granting a host-reaching built-in role (`PRIVILEGED_ROLES`) is denied in all non-Trusted profiles; schema/policy/RLS are admitted iff their flag is set, else deny-by-default. Only **Trusted** skips the whole deny-list belt (`skip_denylist_belt`) — and even then the two raw islands (`pgRaw`, `createFunction.body`) are re-scanned by the raw-island backstops so an embedded host-reaching construct can't slip through. `skip_denylist_belt` is intentionally *not* derivable from `operator()`: Platform and Trusted both grant the full vendor set, but only Trusted may skip the belt.

### 10.6 How an untrusted authored migration is contained — end to end

Four coordinated mechanisms, each closing the gap the previous one admits:

1. **Capability gate (load)** — any privileged vendor op is refused `VENDOR_OP_DENIED` because the Confined/`Single` scope grants no capability.
2. **Parse deny-list (line 1)** — the remaining portable SQL's dangerous surface is hard-denied, including inside `DO`/function bodies and literal-carried leaks.
3. **Least-privilege role (line 2)** — whatever slips past parse (runtime-constructed `EXECUTE format(...)`, dynamic names) fails at execution.
4. **Immutable journal** — the migration cannot forge or erase its own history (deny-by-absence of meta-schema grants + the append-only UPDATE/DELETE/TRUNCATE triggers).

Every layer is explicit about its own limits and names the next layer that covers the residual — the essence of "gate the execution surface, not a specific op."

### 10.7 Capability notes

- `Platform` and `Trusted` both require the same **`OperatorCapability`** token because both are trusted profiles; `Trusted` additionally skips the deny-list belt.
- The `local()` capability preset exists (`capability.rs:227-244`) but is **not wired to any `TrustProfile`** — available for a caller composing a bespoke gate.

---

## §11 Platform self-hosting & build integration

The zeroship platform is its own biggest `@zeroship/migrate` customer. The entire platform database — control-plane, auth/OIDC, billing/metering, and the extracted sandbox's tables — is authored as a committed JS-DSL corpus in `db/migrations-ts/` and applied by the same engine creators use, but under the widened **Platform** trust profile instead of **Confined**.

### 11.1 The platform migration corpus (`db/migrations-ts/`)

The platform's Postgres schema is a **single `zeroship` schema**, applied by the
`zero-migrate` Node CLI through `deploy/ops/db-migrate.sh` under its Platform
profile. The source of truth is the committed `.ts` corpus (no
SQL/Flyway/Liquibase, no committed `.ir.json`):

| File | default-exported `name` | Contents |
| --- | --- | --- |
| `20260702000100_schema_roles_extensions.ts` | `schema_roles_extensions` | `zeroship` schema, `citext` ext, 10 roles, 13 domains, 1 sequence |
| `20260702000200_control_tables.ts` | `control_tables` | 19 control-plane tables (`apps`, `app_members`, `app_secrets`, `app_schema_applies`, …) |
| `20260702000300_auth_oauth_tables.ts` | `auth_oauth_tables` | 28 auth/OIDC tables (`users`, `oauth_clients`, `gateway_sessions`, `signing_keys`, …) |
| `20260702000400_billing_metering_invoice_tables.ts` | `billing_metering_invoice_tables` | 31 billing tables (`invoices`, `invoice_lines`, `credit_ledger`, `plans`, …) |
| `20260702000500_sandbox_tables.ts` | `sandbox_tables` | 5 sandbox tables (`sandboxes`, `shares`, `hosts`, `wake_jobs`, partitioned `sandbox_events`) |
| `20260702000600_constraints_indexes_fks.ts` | `constraints_indexes_fks` | uniques, plain + partial indexes, FKs across all tables |
| `20260702000700_functions_triggers_comments.ts` | `functions_triggers_comments` | 15 `createFunction` plpgsql triggers, trigger wiring, comments |
| `20260702000800_policies_rls.ts` | `policies_rls` | `setRls` + tenant-isolation `policy()` on 9 tables |
| `20260702000900_grants.ts` | `grants` | per-role `grant`/`revoke` |

**Naming/timestamp grammar.** The 14-digit prefix `YYYYMMDDHHMMSS` is the corpus order key (`^(\d{14})_([A-Za-z0-9_]+)\.ts$`, enforced by `sdks/vite-plugin/src/gen-types/recorder.ts:37` and the Rust loader) — no master/changelog file. **Never edit an already-applied migration** — the engine validates per-migration checksums and aborts on drift; add a new timestamped file. The nine files share the date and increment the time component, split by *concern* (schema/roles → tables-per-domain → constraints → functions → RLS → grants) because objects have creation-order dependencies.

### 11.2 The "explicit `{schema}`" convention — the key confined-vs-platform difference

Every platform DDL call passes an **explicit** `{ schema: "zeroship" }` (or `"public"` for `citext`):

```ts
table("app_audit", { schema: "zeroship" }).create({ columns: {…}, primaryKey: ["id"] });          // control_tables.ts:6
domain("account_state").create({ schema: "zeroship", as: t.text(), check: (v) => v.in([...]) });   // schema_roles_extensions.ts:18
grant({ privileges: ["usage"], on: { kind: "schema", names: ["zeroship"] }, to: ["zeroship_auth", …] });  // grants.ts:6
```

A confined creator writes `table("posts").create({…})` with **no** `{schema}` — their unqualified ops resolve against a default project schema at apply time (and `gen-types` folds them under the neutral literal `"public"`, [§5.3](#5-authoring-declarative-desired-state--the-fold)). The Platform corpus spans the shared `zeroship` schema and must name it explicitly on every op. The Platform profile is the only profile that permits `cross_schema` references and schema/extension/role/grant DDL at all ([§10.5](#10-security-first-design)).

The bootstrap file `schema_roles_extensions.ts` is the infrastructure floor: the `zeroship` schema, `citext`, 10 roles (service roles `zeroship_{auth,control,gateway,worker,app}` — the first two `bypassRls: true` — plus four `sandbox_*` roles, §11.5), 13 `domain`s acting as platform-wide enums (`spend_state ∈ {allow,warn,degrade,block}`, `invoice_status`, `billing_period`), and the `audit_events_id_seq` sequence. The corpus uses one `table(...)` handle for portable and PG-vendor table operations alike: partial indexes, RLS, regex CHECKs, and constraint validation all stay capability/dialect-gated by the engine. Where the DSL cannot express a construct, the corpus uses the gated `raw({ sql, reason })` escape — e.g. a `CREATE TRIGGER … BEFORE UPDATE OF sector_identifier …` the trigger DSL cannot express (`functions_triggers_comments.ts:27`). `raw`/`raw_view_body` are capabilities *only Platform enables*. The trigger-heavy file encodes financial-integrity invariants in plpgsql (append-only audit tables, immutable ledgers, controlled state machines); RLS tenant isolation keys off `current_setting('zeroship.tenant_app', true)::uuid`.

### 11.3 Policy-defined table shape

The engine receives an `EffectivePolicy`. Covering `[[inject]]` rules declare columns, indexes, `primary_key`, and `author_primary_key`; `resolve_create_table_policy` materializes that shape into the migration IR.

The creator defaults are declared in [confined-system-shape.inject.toml](../../policies/confined-system-shape.inject.toml). Its columns carry explicit `assign` generators for identity, timestamps, actors, and revision updates. Runtime descriptors preserve those assignments and the selected primary key. The engine does not recognize lifecycle roles by column name.

The [platform policy](../../policies/platform.policy.toml) injects no table shape, so platform migrations declare their own columns and keys. An ordinary `id`, `created_at`, or `deleted_at` has only the behavior declared for it. Prefix and identity refinements follow the injected scalar key's declared name, and foreign keys name their target column explicitly.

### 11.4 Applying the platform migrations (the build/boot wiring)

Services **never migrate themselves** — `control`/`auth` connect to an already-migrated DB. Two apply vectors, both driving the same engine:

**(a) Compose one-shot `migrate` service** (`deploy/compose/docker-compose.yml`,
the `migrate` service) invokes the built `zero-migrate` Node CLI. It records each
`.ts` to transient IR, applies under Platform, and exits 0.
`control`/`auth` depend on it with `service_completed_successfully`.

**(b) By-hand wrapper `deploy/ops/db-migrate.sh`** invokes
`packages/zero-migrate-cli/dist/cli-bin.js`, targeting compose Postgres on
`localhost:5440` by default. `ZEROSHIP_MIGRATE_VERB` selects another CLI verb;
the wrapper records the committed platform corpus in-process.

The engine tracks applied work in an append-only journal, so `migrate` runs only pending work and is idempotent. Apply is `BEGIN; <forward ops>; INSERT journal; COMMIT` per step — no whole-bundle transaction.

**Where the journal lives depends on who owns the schema.** The PLATFORM corpus keeps its own meta schema, `zeroship_migrations`, because it has no tenant: nothing owns `zeroship` the way a migrator role owns an app schema. A CREATOR app's journal lives in the app's own schema (`"<app_uuid>".__zeroship_schema_migrations` and five siblings), because a tenant does own theirs. On both, the table names carry the `__zeroship_` prefix: the engine bootstraps with `CREATE TABLE IF NOT EXISTS` and its table names are literals, so an unfenced `schema_migrations` in a schema a creator can declare tables in would be silently adopted as the journal.

**A creator can drop their own journal**, and that is accepted rather than fixed: they own the schema, owner privileges are implicit and cannot be revoked away, and corrupting it breaks only them. The platform therefore never treats it as a trust anchor — the deploy precondition reads `zeroship.app_schema_applies` (§11.6), which the migration service writes on the control plane's own database.

### 11.5 The `sandbox_*` roles + tables shared with the extracted sandbox

The sandbox backend was extracted to the standalone `zeroship-sandbox` repo but **shares this deployment's Postgres**; the contract lives entirely in the platform corpus. **Roles** (`schema_roles_extensions.ts:14-17`): `sandbox_admin` (nologin, DDL-owning), `sandbox_app` (login, runtime), `sandbox_audit` (append-only event writer), `sandbox_gdpr` (deletion/erasure). **Tables** (`sandbox_tables.ts`): `sandboxes`, `shares`, `hosts`, `wake_jobs`, the range-partitioned `sandbox_events` (monthly partitions + a `default` partition via `partitionBy: { range: ["ts"] }`), with regex CHECKs enforcing typed-id shapes. **Grants** (`grants.ts:7-10,32-36`) scope each role's CRUD. The control plane reaches the sandbox over HTTP (`SANDBOX_URL`/`SANDBOX_TOKEN`), but the *database contract* is these platform-authored roles/tables — so editing `sandbox_tables.ts` here is a cross-repo API change.

### 11.6 `app_schema_applies` — the platform's own record of a creator apply

The platform schema carries ONE table for the creator-migration service: `zeroship.app_schema_applies` (`control_tables.ts`, PK `["app_id","migration_id"]`, `status ∈ {submitted, applied, failed}`, storing `request_body`/`effective_profile`/`ceiling_id`/`ceiling_version`/`descriptor_sha256`/`applied_versions`/`applied_at`/`last_error`). Granted `select, insert, update` to `zeroship_control` only.

**One row per apply REQUEST, not per applied migration**, and that is a requirement rather than an artefact of where the insert sits. The control plane's deploy precondition compares a `.zship`'s `runtime_descriptor.hash` against the NEWEST applied row's `descriptor_sha256`, so an engine upgrade that changes descriptor bytes without changing any schema is repaired by running a migrate that applies nothing — and that only works because the row is still written. `applied_versions` carries the engine's own `outcome.applied`, so a request that advanced nothing is visible as `[]`.

**THREE `migrated_*` TABLES USED TO LIVE HERE and were deleted on 2026-08-28.** `migrated_migrations` carried a `planned → pending_approval → approved → applied` workflow whose approval endpoint no dashboard, CLI or service ever called; `migrated_migration_audit` had one writer and zero readers; `migrated_app_policies` stored a policy that is now declared in the creator's repository and folded at build time, arriving with the apply request. Operator approval of destructive creator migrations is a capability removed on purpose, not an omission.

### 11.7 The build fold (cross-ref)

The `gen-types` fold that turns a creator's migration set into `env.db.ts` + `schema.runtime.json`, and how the vite-plugin / `.zship` packer / `installSchema` consume the v2 `RuntimeSchemaDescriptor`, are documented in [§5.3–§5.4](#5-authoring-declarative-desired-state--the-fold). The two directions (declarative-differ vs migration-first fold) and the runtime installation meet at that one wire type.

### 11.8 The submission ingress pipeline (`submit_migration`)

`ops/submit.rs` is the **single safe ingress** for a client-authored (builder / control plane / CLI) migration script — the confined creator path. A client hands in a `Submission` (raw `up` SQL, optional `down`, a little metadata) and `submit_migration` runs it through the FULL stack — **ingest → guard → lint → live-seeded dry-run → gate → apply** — never letting any step be skipped or its verdict forged. The submitter cannot reach `executor::apply` (nor the engine gate) except through this funnel (`submit.rs:1-9`).

**The whole point: the submitter cannot lie about danger.** `Submission` has **no** `destructive`/`requires_approval` field by construction. A `DROP`/`TRUNCATE`/`DROP COLUMN`/lossy-type-change is judged **server-side** by the `SqlGuard` and folded into `MigrationFlags` via `flags_for` — so a client can never mark a destructive migration "non-destructive" to auto-apply it; the gate decides on the SERVER-DERIVED flags (`submit.rs:11-19`).

**Flow** (`submit.rs:21-41`): (1) **Ingest → `Migration`** — mint a fresh `MigrationId`, build `up`/`down`/`depends_on`/`owner_app`, **derive flags via the guard** (`SqlGuard::check(up)` → `flags_for` gives `destructive`/`transactional`/`requires_approval`, NOT from the submission), layer the submission's `repeatable`/`timeout_ms`, compute the `Checksum`; (2) **Guard** — re-check `up` (and `down`); a denial ⇒ `SubmissionOutcome::Denied` (NOTHING runs); (3) **Lint** — collect `analyze` advisories (carried, never gating); (4) **Dry-run on a live-seeded shadow** (`dry_run_incremental`); a failure ⇒ `DryRunFailed` (real DB untouched); (5) **Gate** — if the SERVER-DERIVED `flags.destructive`/`requires_approval` AND `approval != Approved` ⇒ `ApprovalRequired` (with advisories + `dry_run_ok: true` for review); (6) **Apply** — otherwise `MigrationEngine::apply` the single-migration set under the migrator role + journal ⇒ `Applied`.

**Idempotency / dedup on the checksum** (`submit.rs:43-65`). A fresh `MigrationId` is minted every call, so version can't be the dedup key; dedup is on the migration's `Checksum` (folds the whole apply-relevant unit — `up`, `down`, `flags`, `owner_app`, `depends_on`, `supersedes`, `preconditions`; excludes `version` and `name`). Before applying, `submit_migration` reads the journal's **net-applied** checksums; if this checksum is already net-applied it returns `NoOp` without a second apply. Because the key is *net-applied*, resubmitting an identical script after a rollback **RE-APPLIES** it (MED-3) — the dedup answers "is this exact apply-relevant unit CURRENTLY live?", not "was it EVER applied?".

This ingress is where the §11.3 seal machinery meets the effective-policy meet: the Confined ingress → `effective = ceiling ⊓ draft` (`PolicyProfile::meet_ceiling_draft`) → `SealedProfile` HMAC → journal. `ops/submit.rs` is ~1,047 lines.

### 11.9 Caveats

- The corpus uses `raw({ sql, reason })` where the structured DSL cannot express a construct, such as `trigger().create` for `UPDATE OF <column>`.
- The engine journal (`zeroship_migrations.__zeroship_schema_migrations` for the platform corpus) and `zeroship.app_schema_applies` are different things (§11.4 vs §11.6): the first records what the engine ran, the second what the platform accepted. No `.ts` authors the journal itself — consistent with it being engine-internal (bootstrapped by the apply path).
- **Historical snapshot:** reverse bodies in the then-nine-file platform corpus
  were empty. The current corpus uses the same `schema()` / `data()` contract as
  every other `@zeroship/migrate` caller.

---

## §12 Historical testing and operation

This section records how the former in-tree crate was verified and operated. Its
paths and commands are not current instructions. The engine is no longer a
separate project: it is in-sourced as the `crates/zeroship-migrate*` crates and is
developed in this workspace, with the current platform-runner commands in §12.10.

### 12.1 The three-gate golden/round-trip model (`op_round_trip.rs`)

The load-bearing anti-drift mechanism was `op_round_trip.rs` because the IR wire shape is consumed by **two independent implementations** (the JS `op.*` builder and the Rust engine/loader) that must never drift. A corpus of paired fixtures drives it: `tests/op_fixtures/<name>.mig.js` (authored source) + `<name>.golden.json` (committed canonical IR). The corpus includes fixtures such as `ddl_create`, `ddl_alter`, `fluent_ddl`, `fluent_dml`, `dml_upsert`, `enums_domains`, `partition`, `pg_vendor`, `sequences_exclusion`, `views`, `in_list_scalars`, `edge_scalars`, `runtime_options`, and `p2a_facets`. Three gates:

- **Gate 1 — golden byte-stability** (`corpus_is_byte_stable_and_value_equal`, `:125-179`): each `.mig.js` is recorded through the REAL V8 recorder (`record_migration_to_json_unsandboxed`), run through `resolve_create_table_policy(ir, &PolicyProfile::confined())`, pretty-printed, and compared byte-for-byte against the golden.
- **Gate 2 — JS↔Rust value-checksum round-trip** (`:169-177`): both the fresh IR and the golden are folded through the SAME `Checksum::of_ir(&CanonicalOpList(&ir.ops), &MigrationFlags::default(), &ir.owner_app, &[], &[], &ir.preconditions)` and asserted equal — the *authoritative* check, comparing typed **values**, invariant under JCS-formatting differences.
- **Gate 3 — variant exhaustiveness** (`every_op_variant_has_a_fixture`, `:208-260`): reads `op-ir.schema.json`, extracts every `Op` discriminant, asserts the fixtures cover the whole set, and hard-codes the count: `assert_eq!(expected.len(), 53, "the closed Op set has 53 variants after the RLS quadruplet -> setRls reshape and attachPartition addition")` (`:235-236`). Adding an `Op` without a corpus fixture fails CI.

In the former tree, the corpus regeneration test was `op_round_trip`. The
standalone engine splits its job in two, each half running in the job that
already has the toolchain it needs, joined through a committed
`op_fixtures/recorded.json`: the JS half executes every `.mig.js` through the
production recorder
(`the recorded-corpus suite (DELETED with the vendored engine tree)`)
and the Rust half resolves those recorded ops through the real policy resolver
and compares against `<stem>.golden.json`
(`crates/zeroship-migrate/tests/ir_contract/op_fixture_goldens.rs`).
Run them from the standalone project's own workspace when changing that corpus;
the removed appbase package cannot be selected with `cargo -p`.

### 12.2 The IR-schema golden (`op_ir_schema.rs`)

`op-ir.schema.json` (crate root) is the JSON-Schema contract the JS builder targets. `emit_op_ir_schema` (`:28-48`) regenerates via `schemars::schema_for!(MigrationIr)` and asserts equality with the on-disk file (`UPDATE_SCHEMA=1` rewrites). `op_variant_names_from_schema` (`:55-148`) extracts every `"op"` const and asserts it equals a hard-coded list — the authoritative human-readable enumeration, grouped as **37 non-privileged** DDL/DML ops + **16 privileged vendor** ops = **53**. Adding an op touches: the Rust `Op` enum, this list, the `op_round_trip.rs:235` count, and a corpus fixture — in lockstep.

### 12.3 The full-surface behavioral suite (`full_surface.rs`)

`full_surface.rs` (63 KB) pinned individual DSL semantics that would silently regress (where `op_round_trip.rs` proved whole-fixture bytes/checksums). It recorded inline sources via `record_migration_to_ir_unsandboxed` and asserted on the wire `ops` JSON — e.g. `t.text()` OMITS `nullable` (absence is the dialect default) while `.notNull()` records `nullable: false`; `concatWs(...)` records a `fnSynth(concatWs)` node. That suite moved to the recorder's own language when the engine dropped V8: its successor is `packages/zero-migrate/tests/ops.test.ts`, which drives the recorder's `__begin`/`__drain` seam directly and is the file to read to learn what the fluent surface does op-by-op.

### 12.4 The embedded-`.ts`/`.js`-in-Rust-string gotcha

**The single most common way a surface rename breaks the build.** Many test files embed migration source *as Rust raw-string constants* rather than as separate `.mig.js` files (`full_surface.rs`, `ir_dml_pg.rs`, `ir_dml_sqlite.rs`, `build_new_generate_{pg,sqlite}.rs`, `gen_types_cli.rs`, `recorder_http_contract.rs`, `recorder_sandbox_e2e.rs`, `enums_domains.rs`, `vendor_pg.rs`, `doc_hero_apply.rs`, …). When a DSL symbol changes, grep the **whole `tests/` tree**, not just `op_fixtures/`. A missed import does NOT produce a clean compile error — it surfaces at runtime as an opaque V8 module-instantiation failure (`Failed to instantiate: op_recorder.js`). There is also a **platform `.ts` fixture tree** under `tests/platform_ts_fixtures/` split by outcome: `apply/`, `denied/` (`alter_system.ts`), `failure/` — feeding `platform_ts_apply_pg.rs`.

### 12.5 Live-DB suites and how they are selected

- **Postgres on `:5440`** — the `*_pg.rs` suites connect to a docker Postgres on port 5440 (DSN defaults + overrides differ per file: `MIGRATE_PLATFORM_IR_TEST_DB`, `MIGRATE_TEST_DB`). Selection is **connect-or-panic**, not soft-skip (`compio_postgres::connect(...).expect(...)`). If the container is down, the whole PG suite goes RED. The convention flag `MIGRATE_REQUIRE_DB=1` selects the serialized DB suites. Tests touching shared platform schemas serialize across processes via `test_support::acquire_global_platform_resource_lock` (a `pg_advisory_lock` released on drop). *Operational gotcha (repo memory):* the test DB is docker `appbase-migrate-postgres-1` on `:5440`; a Codex run can tear it down (spurious mass `*_pg` RED) — restart with `docker start appbase-migrate-postgres-1`; the DB-free render/preview/round-trip tests are the clean signal.
- **SQLite (in-process, temp-file)** — the `*_sqlite.rs` suites use `tempfile` per-test SQLite files against the hardened `rusqlite` backend. Always run.
- **MySQL via the JsDriver (`mysql_jsdriver_e2e.rs`, 119 KB)** — the only **soft-skip** backend. Default DSN `mysql://root:zeroship@127.0.0.1:3307/zeroship_e2e` (override `MYSQL_JS_DRIVER_E2E_DSN`). `live_mysql_or_skip()`: prints `SKIPPED` if MySQL is unreachable and `MIGRATE_REQUIRE_MYSQL=1` is NOT set; with it, the same condition **panics** (CI can't silently skip). The `mysql2 3.14.1` driver bundle is committed and regenerated (not fetched) via `scripts/vendor-mysql2.sh`.

### 12.6 Sandboxed-child corpus parity + the recorder sandbox

`op_round_trip.rs`'s sandboxed-child corpus parity test (`:184-202`) records every fixture through both the in-process and kernel-sandboxed-child paths and asserts byte-equal output, including fixtures that exercise rooted Postgres vendor exports. `recorder_sandbox_e2e.rs` proved the sandbox at the kernel level: it spawned the real child with `pre_exec` lockdown (netns + rlimits) + in-child seccomp-bpf + Landlock, and asserted on the **child termination cause** — `SIGSYS` (seccomp default-deny on socket/connect/execve/fork), `EACCES` (Landlock on write/out-of-dir read), `RLIMIT_CPU`/wall-watchdog/`RLIMIT_AS` → `BUILD_RECORDER_BUDGET_EXCEEDED`, plus per-invocation isolation. Capability-gated hard-fail, never silent-skip; Linux-only. Both tests went with the V8 recorder host: the standalone engine runs authoring in the Node process over the `zeroship-migrate-node` napi bridge and ships no sandboxed recorder child, so neither has a successor there.

### 12.7 Other golden/preview gates

- **SQL preview goldens** (`crates/zeroship-migrate/tests/ir_contract/sql_preview.rs`, DB-free): renders a `REPRESENTATIVE_IR` for all three dialects, byte-compares against `tests/golden/sql_preview_{pg,sqlite,mysql}.txt`, and asserts **faithfulness** (each statement byte-identical to `IrAuthor::lower_steps`), **no fabrication** (DB-state-dependent ops emit `-- [runtime-resolved]`), and **no DB connection**. Regenerate per dialect with `cargo test -p zeroship-migrate --test ir_contract -- --ignored update_golden_pg` (or `update_golden_sqlite` / `update_golden_mysql`) - finer-grained than the single switch it replaced, which rewrote all three at once.
- **Golden execution traces** (`golden_trace_pg.rs`/`golden_trace_sqlite.rs` → `tests/golden-traces/*.txt`): capture a full apply trace + resulting schema against live PG/SQLite. `assert_frozen` panics if the fixture is absent (a first-run capture is reviewed + committed, never self-blessed). The PG destructive-refusal trace is asserted identical across an oracle leg and a live leg.
- **Generated-TS `.d.ts` goldens** (`gen_types_dts_golden.rs`) + a `tsc` gate (`gen_types_dts_tsc_gate.rs`).

### 12.8 The former `standalone-cli` binary and its verbs

The removed operator CLI (`src/bin/zeroship-migrate.rs`,
`required-features = ["standalone-cli"]`, `#[compio::main]`) was a thin `clap`
argument parser delegating to `command::runner::run_*`. The table below is a
historical inventory, not a list of commands provided by appbase today:

| verb | DB? | purpose |
| --- | --- | --- |
| `new <name>` | offline | scaffold a dbmate-format raw `.sql` (validates the name, never clobbers) |
| `migrate` / `up` | yes | apply all pending; requires `--yes`/`--allow-destructive` when the plan is destructive |
| `down` | yes | roll back the single most-recent migration (`--yes`) |
| `rollback [--to <ver>` \| `--steps <N>]` | yes | unwind after a version / N most-recent / all (`--yes`) |
| `status` | yes | applied vs pending vs rolled-back (reads journal, no DDL) |
| `validate` | yes | shadow-DB dry-run + checksum-drift + destructive advisories; **exits non-zero** on a failing dry-run/drift |
| `plan [--dialect pg\|sqlite\|mysql]` | **offline** | render the exact per-dialect SQL the pending set WOULD run, WITHOUT a DB; DB-state-dependent ops print `-- [runtime-resolved]` |
| `lint [--dialect] [--json] [--deny-warnings] [--deny RULE,…]` | offline | run the [§7.9](#7-the-validate-gate--error-taxonomy) advisory analyzers over `.sql`/`.ir.json`; exit 0 by default, non-zero with the deny flags |
| `wait [--timeout-secs]` | yes | poll until the DB accepts `SELECT 1` |
| `dump` | yes | schema dump + applied-versions trailer → `schema.sql` (§12.9) |
| `load` / `setup` | yes | bootstrap a fresh DB by replaying `schema.sql` + reconstructing the journal from the trailer |
| `resolve-pending [--apply\|--abort] <version>` | yes | discharge a cross-deploy online-rename pending contract ([§9.14](#9-the-apply-engine--durability)); PG-only; `--abort` needs `--acknowledge-shadow-data-loss` |

`--profile` selects the guard posture (`trusted` — the binary DEFAULT, deny-list OFF; `platform` — widened, explicit opt-in with `--schema`/`--extension`/`--meta-schema`; `confined` — full creator deny-list, but reached via `submit_migration`, never this binary). Precedence for every setting: **CLI flag > env (`ZEROSHIP_MIGRATE_*` / `DATABASE_URL`) > `zeroship-migrate.toml` > built-in default**. Engine auto-detected from DSN unless `--engine` forces it. `new` produces raw `.sql` and is demoted to the trusted/legacy path — creators formerly authored portable op.* `.ts` through the retired JS authoring CLI's `new`/`generate` commands.

### 12.9 The former schema-dump / `pg_dump` path

The removed CLI's `dump` path (`run_dump`, `:1016-1052`) was engine-agnostic
with a shared trailer. This behavior is retained here only as historical design
context; `zeroship-platform-migrate` is apply-only and does not expose dump,
load, rollback, or schema-refresh commands.

### 12.10 Current appbase quick reference

```bash
# Build the authoring package, Node CLI, and native addon.
pnpm build

# Apply db/migrations-ts to compose Postgres through the repository wrapper.
./deploy/ops/db-migrate.sh

# JS/pnpm side of the authoring surface:
pnpm --filter @zeroship/migrate test
```

### 12.11 Caveats

- `MIGRATE_REQUIRE_DB` is a documented CI/runner convention (referenced in test headers), not a universal in-test soft-skip: the PG helpers connect-or-`.expect()`-panic unconditionally, so PG suites hard-fail (not skip) when `:5440` is down. Only the MySQL suite has an explicit code-level `*_or_skip`.
- Op-variant counts: `op_round_trip.rs:236` and the `op_ir_schema.rs` list (37 core + 16 vendor) pin the authoritative enumeration ([§8.3](#8-one-ir-three-dialects-render--portability)).

---

*End of the zeroship-migrate reference.*
