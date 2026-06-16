# Migration Engine — Foundation (Plan 1: crate + types + SQL security guard) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the `zeroship-migrate` crate with the migration data types and the **SQL security guard** (parse-time deny-list + statement classification) — the §1 security core, fully unit-testable without a database.

**Architecture:** A new workspace crate. SQL is parsed with the real Postgres parser (`pg_query`/libpg_query) so the guard sees exactly what Postgres would execute. Every statement is classified (DDL kind · additive/destructive · dangerous) and checked against a hard deny-list (RCE/priv-esc/cross-tenant vectors). The guard is pure logic — no DB, no async — so it's exhaustively testable and is the first line of defense-in-depth (the least-priv `migrator` role, built later, is the second).

**Tech Stack:** Rust, `pg_query` (libpg_query bindings) for parsing, `zeroship-core::typed_id` (UUIDv7) for migration ids, `sha2` for checksums. No tokio/compio needed here (parsing is sync, runs out-of-band at deploy).

Spec: `docs/proposals/2026-06-16-db-migration-engine-design.md` (§1 security, §2.1 migration unit).

---

## File Structure

- `crates/zeroship-migrate/Cargo.toml` — crate manifest (workspace member)
- `crates/zeroship-migrate/src/lib.rs` — public API surface + module wiring
- `crates/zeroship-migrate/src/migration.rs` — `Migration`, `MigrationId`, `MigrationFlags`, `Checksum`
- `crates/zeroship-migrate/src/classify.rs` — statement parsing + `StatementClass` (DDL kind, additive/destructive)
- `crates/zeroship-migrate/src/guard.rs` — `SqlGuard`, the deny-list, `GuardError`, `GuardReport`
- `crates/zeroship-migrate/src/guard/denylist.rs` — the enumerated dangerous-construct rules (data, not logic)
- `crates/zeroship-migrate/tests/guard_security.rs` — the attack-vector test matrix (the security heart)
- `crates/zeroship-migrate/tests/classify.rs` — classification correctness
- Modify: root `Cargo.toml` — add `crates/zeroship-migrate` to `members`

---

## Task 1: Crate scaffold + parser dependency

**Files:**
- Create: `crates/zeroship-migrate/Cargo.toml`
- Create: `crates/zeroship-migrate/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Add the crate to the workspace + manifest.** Decide the parser: use **`pg_query`** (libpg_query — the actual Postgres parser). Rationale: a security deny-list must not *misparse* or *miss* a dangerous statement; `sqlparser-rs` is pure-Rust but incomplete for exotic PG syntax, which is a security gap. `pg_query` carries a C build dep (libpg_query) — confirm it builds in the Nix toolchain (C compiler is available). If `pg_query` cannot build in-environment, fall back to `sqlparser-rs` **and** record the reduced-coverage risk in `lib.rs` docs + lean harder on the DB-privilege layer. Cargo.toml deps: `pg_query`, `sha2`, `thiserror`, `zeroship-core` (path), `serde` (derive).

- [ ] **Step 2: `cargo build -p zeroship-migrate`** → expect clean (empty lib compiles, deps resolve). If `pg_query` fails to build, execute the fallback decision above and re-run.

- [ ] **Step 3: Commit.**
```bash
git add crates/zeroship-migrate/Cargo.toml crates/zeroship-migrate/src/lib.rs Cargo.toml
git commit -m "feat(migrate): scaffold zeroship-migrate crate + pg_query parser dep"
```

---

## Task 2: Migration types

**Files:**
- Create: `crates/zeroship-migrate/src/migration.rs`
- Test: inline `#[cfg(test)]` in `migration.rs`

Public surface (define exactly these — later tasks/plans depend on the names):
```rust
pub struct MigrationId(String);                 // "mig_<base62 uuidv7>"
impl MigrationId { pub fn generate() -> Self; pub fn as_str(&self) -> &str;
    pub fn parse(s: &str) -> Result<Self, IdError>; pub fn timestamp_ms(&self) -> u64; }

pub struct MigrationFlags { pub transactional: bool, pub destructive: bool,
    pub online: bool, pub requires_approval: bool }
impl Default for MigrationFlags { /* transactional: true, rest: false */ }

pub struct Checksum(String);                    // hex sha256
impl Checksum { pub fn of(up: &str, down: Option<&str>) -> Self; pub fn as_str(&self) -> &str; }

pub struct Migration {
    pub version: MigrationId, pub name: String,
    pub up: String, pub down: Option<String>,   // None = explicitly irreversible
    pub checksum: Checksum, pub flags: MigrationFlags,
    pub owner_app: String,                       // app typed-id
    pub depends_on: Vec<MigrationId>,
}
```

- [ ] **Step 1: Write failing tests** (in `migration.rs`):
  - `migration_id_has_mig_prefix_and_roundtrips` — `generate()` → starts with `mig_`; `parse(as_str())` ok; bad prefix → `Err`.
  - `migration_ids_are_time_ordered` — two `generate()` calls (sequenced) sort by `timestamp_ms()` non-decreasing and string-sort matches time order (UUIDv7 property; reuse `zeroship_core::typed_id`).
  - `checksum_is_deterministic_and_sensitive` — same `(up,down)` → equal; differing `up` *or* `down` (incl. `Some("")` vs `None`) → different.
  - `flags_default_is_transactional` — `MigrationFlags::default().transactional == true`, others false.
- [ ] **Step 2: Run** `cargo test -p zeroship-migrate migration` → FAIL (types absent).
- [ ] **Step 3: Implement** `migration.rs` using `zeroship_core::typed_id::{generate,parse_with_prefix}` (prefix `"mig"`) and `sha2::Sha256`. Checksum input = `up` + `\x00` + `down.unwrap_or("")` length-prefixed (so `Some("")` ≠ `None`).
- [ ] **Step 4: Run** `cargo test -p zeroship-migrate migration` → PASS.
- [ ] **Step 5: Commit.** `git add crates/zeroship-migrate/src/migration.rs && git commit -m "feat(migrate): migration types (UUIDv7 id, checksum, flags)"`

---

## Task 3: Statement classification

**Files:**
- Create: `crates/zeroship-migrate/src/classify.rs`
- Test: `crates/zeroship-migrate/tests/classify.rs`

Surface:
```rust
pub enum DdlKind { CreateTable, DropTable, AddColumn, DropColumn, AlterColumnType,
    RenameColumn, RenameTable, CreateIndex, CreateIndexConcurrently, AddConstraint,
    DropConstraint, CreateExtension, CreateFunction, CreateTrigger, CreateRole, Grant,
    AlterSystem, Copy, Dml, Select, Other(String) }
pub struct StatementClass { pub kind: DdlKind, pub additive: bool, pub destructive: bool,
    pub non_transactional: bool, pub referenced_schemas: Vec<String>, pub raw: String }
pub fn classify(sql: &str) -> Result<Vec<StatementClass>, ParseError>;  // one per statement
```

- [ ] **Step 1: Write failing tests** (`tests/classify.rs`) — assert `kind`, `additive`, `destructive`, `non_transactional`, `referenced_schemas` for at least:
  - `CREATE TABLE products(...)` → CreateTable, additive, not destructive.
  - `ALTER TABLE products ADD COLUMN sku text` → AddColumn, additive.
  - `ALTER TABLE products DROP COLUMN sku` → DropColumn, **destructive**.
  - `DROP TABLE products` → DropTable, destructive.
  - `CREATE INDEX CONCURRENTLY i ON products(sku)` → CreateIndexConcurrently, **non_transactional=true**.
  - `ALTER TABLE products ADD CONSTRAINT ... CHECK(...)` → AddConstraint.
  - `SELECT * FROM control.creator_billing` → Select, `referenced_schemas == ["control"]`.
  - multi-statement input → one `StatementClass` per statement, in order.
- [ ] **Step 2: Run** `cargo test -p zeroship-migrate --test classify` → FAIL.
- [ ] **Step 3: Implement** using `pg_query::parse` to get the parse tree; walk statement nodes to fill `DdlKind` + flags; collect schema-qualified names into `referenced_schemas`. `destructive` = {DropTable, DropColumn, DropConstraint, lossy AlterColumnType, Truncate}. `non_transactional` = {CreateIndexConcurrently, AlterType-add-value, Vacuum}.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -m "feat(migrate): SQL statement classification via pg_query"`

---

## Task 4: The SQL security guard (deny-list) — the security heart

**Files:**
- Create: `crates/zeroship-migrate/src/guard.rs`, `crates/zeroship-migrate/src/guard/denylist.rs`
- Test: `crates/zeroship-migrate/tests/guard_security.rs`

Surface:
```rust
pub struct GuardConfig { pub project_schema: String, pub extension_allowlist: Vec<String> }
pub enum GuardError { Denied { rule: &'static str, statement: String },
    CrossSchema { schema: String, statement: String }, Parse(ParseError) }
pub struct GuardReport { pub classes: Vec<StatementClass>, pub destructive: bool, pub warnings: Vec<String> }
pub struct SqlGuard { cfg: GuardConfig }
impl SqlGuard { pub fn new(cfg: GuardConfig) -> Self;
    pub fn check(&self, sql: &str) -> Result<GuardReport, GuardError>; }
```

- [ ] **Step 1: Write the attack-vector test matrix** (`tests/guard_security.rs`). Each MUST return `Err(GuardError::Denied|CrossSchema)`. This is the security spec — be exhaustive:
  - `COPY t TO PROGRAM 'sh'` and `COPY t FROM PROGRAM '...'` → Denied (RCE).
  - `CREATE FUNCTION f() ... LANGUAGE plpythonu` / `plperlu` / `c` → Denied (untrusted PL/C).
  - `CREATE EXTENSION dblink` / `postgres_fdw` / `file_fdw` → Denied (not in allowlist; SSRF/file).
  - `ALTER SYSTEM SET ...` → Denied.
  - `CREATE ROLE evil` / `ALTER ROLE ... SUPERUSER` / `GRANT ... TO evil` → Denied (priv-esc).
  - `SELECT * FROM control.creator_billing` / `DROP SCHEMA project_other` / `INSERT INTO auth.users ...` → CrossSchema (schema ∉ {project_schema}).
  - `SELECT pg_read_file('/etc/passwd')` / `lo_import('/etc/passwd')` / `pg_read_server_files` grant → Denied (file access).
  - `SELECT dblink_connect(...)` → Denied.
  - `DO $$ ... COPY ... PROGRAM ... $$` (dangerous construct nested in a DO block) → Denied (must inspect inside DO/function bodies, not just top-level).
  - `SET search_path TO control` → Denied (search_path escape).
  - **Positive controls (must PASS, report ok):** `CREATE TABLE products(...)`, `ALTER TABLE products ADD COLUMN sku text`, `CREATE INDEX CONCURRENTLY ...`, `CREATE EXTENSION pgcrypto` when `pgcrypto` ∈ allowlist.
  - **Destructive (must PASS but `report.destructive == true`):** `DROP TABLE products`, `ALTER TABLE products DROP COLUMN sku` — the guard *flags* (gate decides), it doesn't deny data-loss.
- [ ] **Step 2: Run** `cargo test -p zeroship-migrate --test guard_security` → FAIL.
- [ ] **Step 3: Implement** `denylist.rs` (enumerated rules: forbidden statement kinds, forbidden function/PL names, forbidden extensions-unless-allowlisted, the cross-schema check against `project_schema`) and `guard.rs` (`check` = classify → for each statement walk for denied constructs **including inside DO blocks / function bodies** → cross-schema check → assemble `GuardReport` with `destructive` + `warnings`). Deny-by-default for unrecognized dangerous kinds where ambiguous.
- [ ] **Step 4: Run** → PASS (all denies + positives + destructive-flagging).
- [ ] **Step 5: Commit.** `git commit -m "feat(migrate): SQL security guard + deny-list (RCE/priv-esc/cross-tenant)"`

---

## Task 5: Danger flagging + lint warnings

**Files:**
- Modify: `crates/zeroship-migrate/src/guard.rs`
- Test: extend `tests/guard_security.rs`

- [ ] **Step 1: Write failing tests:** `GuardReport.warnings` contains a lock-warning for `ALTER TABLE ... ADD COLUMN ... NOT NULL DEFAULT <volatile>` and for a non-`CONCURRENTLY` `CREATE INDEX` on a (heuristically) large pattern; `requires_approval` semantics: a `report.destructive` migration maps to `MigrationFlags { requires_approval: true, .. }` via a helper `flags_for(&GuardReport) -> MigrationFlags`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** the warning heuristics + `flags_for`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -m "feat(migrate): danger flagging + lint warnings"`

---

## Task 6: Public API surface + crate-level test

**Files:**
- Modify: `crates/zeroship-migrate/src/lib.rs`
- Test: `crates/zeroship-migrate/tests/guard_security.rs` (smoke)

- [ ] **Step 1:** Re-export the public types from `lib.rs` (`Migration*`, `SqlGuard`, `GuardConfig`, `GuardError`, `GuardReport`, `classify`, `flags_for`). Add a module-level doc summarizing the §1 security stance + the "this is line 1 of defense-in-depth; the least-priv role is line 2" note.
- [ ] **Step 2:** `cargo build --workspace` clean; `cargo test -p zeroship-migrate` all green; `cargo clippy -p zeroship-migrate` clean.
- [ ] **Step 3: Commit.** `git commit -m "feat(migrate): public API surface + docs"`

---

## Self-Review (done by the author before handoff)
- **Spec coverage:** Plan 1 implements §1.4 (parse-time deny-list), §1.5 (cross-schema confinement check), §2.1 (migration unit/types). §1.3 (role), §2.2-2.4 (journal/executor/apply), §3 (authoring), §4-5 (multi-app/hard cases) are **explicitly out of scope** → Plan 2+ (executor on PG), Plan 3 (role provisioning), Plan 4 (authoring). Recorded so the next plans pick them up.
- **No placeholders:** test matrices enumerate concrete inputs + expected `Err`/`Ok`; signatures are exact.
- **Type consistency:** `MigrationFlags`, `GuardReport`, `StatementClass`, `DdlKind`, `flags_for` names are consistent across Tasks 2-6.

## Next plans (after this lands green)
- **Plan 2 — Executor on Postgres:** journal table + immutability, project advisory lock, apply flow (txn default + non-txn two-phase recovery), checksum/drift, statement/lock timeouts. Needs PG (:5440 dedicated DB). TDD on real PG.
- **Plan 3 — Least-priv `migrator` role + provisioning:** the DB-privilege second line of defense; builds on roles/RLS changesets 0025/0026.
- **Plan 4 — Authoring pipeline + seams:** `MigrationEngine`/`MigrationAuthor`, deterministic-additive author, plan/lint/gate wiring.
