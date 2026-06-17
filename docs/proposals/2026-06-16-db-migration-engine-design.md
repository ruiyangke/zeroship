# DB Migration Engine — Design

Status: **proposal** (pre-implementation; security-first). Date: 2026-06-16.
Scope: zeroship's **own versioned migration engine** for **creator project databases**, under the new `project`-umbrella model (one project = one shared db serving multiple apps). The platform's *own* db (`control`/`auth`/`billing`) **also runs on `zeroship-migrate`** under the **Platform** trust profile — see §1.7 (this reverses the original "stays on Liquibase" rule; the reversal is specified in `docs/proposals/2026-06-17-platform-migrations-flyway-mode-design.md`).

> Authoring note (per `feedback_proposal_workflow`): this draft is **uncommitted** until the implementing PR; written here for review.

---

## 0. Context & premises (already agreed)

- **Project umbrella.** A `project` (`prj_…`) is the umbrella over resources: one **db**, **kv**, **storage**, and one *or more* **apps**. Apps in a project **share** the db (web `storefront` + `storebackend` hit the same `products`/`orders`).
- **Schema = creator-managed, declarative authoring.** Each app `export default { schema }` declares the tables it owns; the **project db schema is the union** of all member apps' declarations. `env.db` binds every app to the **merged** schema (an app can *use* a table it didn't declare).
- **Declare vs use.** Using a table (read/write rows) is always shared & free. *Declaring* a table (its structure) is ownership: one owner per table; identical re-declaration is idempotent; conflicting declaration is a deploy error.
- **Build our own engine.** We build the **versioned executor** (bounded, in-stack on `compio-postgres`, integrates with our advisory-lock/immutability conventions). We do **not** build the declarative-diff engine; migration *authoring* is pluggable (AI / deterministic / Atlas-later).
- **Tiered apply.** Additive/safe → auto; destructive/ambiguous → gated; everything journaled.
- **Top priority: security.** Migrations are privileged arbitrary-SQL authored by untrusted creators *and* an AI. The security model (§1) anchors everything.

---

## 1. Security model (the foundation)

### 1.1 Threat model
A migration executes **DDL** (needs elevated privilege) and its SQL comes from **untrusted authors** (creator, or AI subject to prompt-injection via app content/templates). Vectors:
1. **Cross-tenant access** — touching another project's schema or `control`/`auth`/`billing`.
2. **Privilege escalation** — `CREATE ROLE`, `GRANT`, `ALTER SYSTEM`, `pg_authid`.
3. **Postgres host-escape / RCE** — `COPY … FROM/TO PROGRAM` (shell), untrusted PLs (`plpythonu`/`plperlu`), `LANGUAGE C` functions, dangerous `CREATE EXTENSION`, `dblink`/`postgres_fdw` (SSRF + reach other DBs), `lo_import`/`lo_export` + `pg_read_server_files` (filesystem).
4. **Prompt-injection → malicious migration** — AI authors SQL; the engine must enforce guardrails *regardless of submitted SQL*.
5. **Tampering / supply-chain** — migration edited after approval; mutable journal.
6. **DoS** — indefinite locks / unbounded ops starving the project or shared infra.
7. **Destructive ops** — `DROP`/`TRUNCATE` data loss.

### 1.2 Principle: untrusted-by-default, defense-in-depth
Treat every migration as untrusted input; confine by **DB privilege** *and* verify independently at **parse time**. Belt and suspenders.

### 1.3 Least-privilege per-project `migrator` role
- A dedicated role that can DDL **only within the project's own schema**: `NOSUPERUSER NOCREATEROLE NOCREATEDB`, no grants on `control`/`auth`/`billing` or other projects' schemas, `search_path` pinned to the project schema.
- Builds on the existing role/RLS foundation (changesets `0025_roles_rls`, `0026_sandbox_role_login`). The DB itself rejects cross-tenant/privileged ops even if SQL gets through.

### 1.4 Hard-deny the dangerous surface — by privilege AND by parse
- **By privilege:** `REVOKE`/deny `COPY PROGRAM`, untrusted PLs, C functions, file-access roles (`pg_read_server_files`, `pg_write_server_files`, `pg_execute_server_program`), `dblink`/FDW, `ALTER SYSTEM`; `CREATE EXTENSION` only from a tight **allowlist**.
- **By parse (defense in depth):** the engine parses every statement and **rejects** the dangerous set at submission. AI-authored SQL passes the *same* gate — the AI cannot bypass it.

### 1.5 Confinement, timeouts, audit
- **Schema confinement:** statements referencing objects outside the project schema(s) are rejected (parse) and impossible (privilege).
- **Mandatory `statement_timeout` + `lock_timeout`** per migration — no indefinite locks / DoS.
- **Immutable, tamper-evident journal** (§2.2): author, approver, full SQL, checksum, timestamp, outcome — append-only with an immutability trigger (the billing-ledger pattern). Checksum mismatch on an applied migration = hard error.

### 1.6 Plan/apply split + gate
- **Plan** (read-only): diff, lint, preview, danger-flagging — *no* mutations.
- **Apply** (gated): destructive/flagged ops require explicit confirmation. **AI output is never auto-applied for destructive ops.**

### 1.7 Total isolation from the platform db
The creator-migration engine's roles have **zero** access to `control`/`auth`/`billing`. The platform's own db **also runs on `zeroship-migrate`**, but under the **Platform** trust profile — engineer-authored SQL, applied via the operator-side CLI / compose `migrate` service, with a widened-but-RCE-backstopped guard. Trust separation is the **call-site invariant, not tool separation**: the Platform profile is constructible *only* at the operator call site (gated by a crate-private `PlatformCapability` token), and the creator submission ingress is hard-wired to Confined with no API path to Platform — so unifying the *engine* does **not** hand the creator-migration path a route toward platform schemas. The original conclusion here ("stays on Liquibase") conflated trust-domain separation (preserved) with tool separation (dropped); Liquibase's physical separation is replaced by a typed, statically-enforced in-engine invariant. See `docs/proposals/2026-06-17-platform-migrations-flyway-mode-design.md` §3 + §5 for the full reversal rationale and security analysis.

---

## 2. Core engine — the versioned executor (what we build)

### 2.1 Migration unit
Immutable, ordered artifact shipped in the `.zship` bundle (reviewable, replayable across environments), recorded in the journal on apply:

```
migration {
  version:   UUIDv7 (mig_…)      // time-ordered, collision-free across concurrent app authoring
  name:      "add_orders_table"
  up:        SQL | structured-op-list
  down:      SQL | null          // null = explicitly irreversible (no true down)
  checksum:  hash(up + down)
  flags:     { transactional, destructive, online, requires_approval }
  owner_app: app_id              // the declaring app (per-table ownership)
  depends_on: [version…]         // optional cross-slice ordering
}
```

- **Version = UUIDv7** (`typed_id`), not sequential ints (collide under concurrent multi-app authoring) or raw timestamps (skew). Total order by the time component.

### 2.2 Journal — `schema_migrations` (per project, meta schema)
Append-only + immutable trigger. Columns: `version, name, checksum, applied_at, applied_by(app/actor/AI), exec_ms, phase(started|completed), outcome`. Lives in a per-project **meta schema** writable only by the `migrator` role (not the `app` role) — a creator migration can't touch its own history.

### 2.3 Apply flow
1. **Acquire project advisory lock** `pg_advisory_lock(project_id)` — serialize all migration activity; concurrent app deploys queue (bounded by `lock_timeout`).
2. Read journal → `pending = project_set − applied`, in UUIDv7 order.
3. **Drift/tamper check:** re-verify applied checksums → mismatch aborts.
4. Per pending migration (set `statement_timeout`+`lock_timeout`):
   - **Transactional (default):** `BEGIN; up; INSERT journal; COMMIT` — DDL+journal atomic ⇒ crash leaves applied+recorded *or* neither.
   - **Non-transactional (opt-in):** for `CREATE INDEX CONCURRENTLY`, `ALTER TYPE … ADD VALUE`, `VACUUM`. Two-phase: journal `started` → run → journal `completed`. Crash leaves `started`-only ⇒ next-deploy **recovery path** inspects real state (e.g. `INVALID` index → drop + retry idempotently).
5. Release lock.
- Runs as the least-privilege `migrator` role, `search_path` pinned.

### 2.4 Crash & concurrency safety
Transactional = atomic with journal. Non-transactional = two-phase + idempotent recovery. Concurrency = project advisory lock; second deploy waits, then no-ops already-applied. Empty pending = no-op.

---

## 3. Authoring — declarative → versioned (pluggable, AI-driven)

- Creator/AI edits the declarative schema; on build the engine compares declared-state vs journal and a **versioned migration is generated**.
- **`MigrationAuthor` seam** (pluggable source of up/down SQL):
  - **Deterministic generator** — trivial additive ops (new table, add nullable column, add index): pure pattern, no AI, fully safe.
  - **AI builder** (primary for non-trivial) — renames, type changes, backfills, expand-contract sequences. Output is **untrusted** → full §1 gate.
  - **Atlas (later)** — declarative diff → SQL when deterministic generation without AI is wanted.
- **Pipeline:** `plan` (read-only diff + generated SQL surfaced) → `lint` (§1 deny-list + danger flags: drops, lock-heavy, missing backfill) → `gate` (destructive/flagged ⇒ confirm; AI never auto-applies destructive) → `apply` (§2).
- **`MigrationEngine` seam:** `plan(declared_schema, journal) -> MigrationPlan`; `apply(plan) -> Outcome`. Executor (our code) is fixed; the **author** and (later) **online-executor** are pluggable.

---

## 4. Multi-app / shared-project-db semantics

- One project = one db = one project schema; every app's `env.db` binds to the **merged (union)** schema.
- **Per-table ownership:** declaring app owns a table's migrations; identical re-decl = idempotent; conflicting = deploy error; non-declarers *use* freely.
- **Ordering:** UUIDv7 total order + project advisory lock. Cross-slice FK dependency (app B FK → app A's table) → `depends_on` or clean failure surfaced.
- **Add app:** grant its role project-db access; no schema change. **Remove app:** owned tables **persist** (undeclared, re-claimable); journal retained; never auto-dropped.
- **Access scoping:** shared-full within the project by default; optional per-app capability scoping (read-only / table-subset) layered on role grants (§1.3).

---

## 5. Hard cases

- **Rollback / no-true-down:** each migration has `down` or explicit `down: null`. Rollback applies downs in reverse to a target; refuses `down: null` without explicit force + backup. **Default to roll-forward** (compensating migration) for old destructive history; reserve true rollback for recent/failed deploys.
- **Zero-downtime expand-contract** (rename, type change, NOT-NULL on big table): authored as a **multi-deploy sequence** — add nullable → batched backfill → `ADD CONSTRAINT … NOT VALID` → `VALIDATE` → switch code → drop old. Expand steps land **before** dependent code; contract/drop steps land **after** code stops using the old shape ⇒ old-code+new-schema coexist. AI generates the sequence; engine orders+executes. *Highest value & risk.*
- **Large-table backfill:** chunked, cursor-based, resumable via journal, per-batch bounded by `statement_timeout` — non-blocking.
- **Drift:** journal-checksum + optional introspection; **surface, don't auto-fix**.
- **Baseline existing db:** `baseline` records current schema as v0 without re-running (adoption path).

---

## 6. Scenario coverage matrix (all 53)

Legend: **A**=auto/deterministic · **G**=gated(confirm) · **AI**=AI-authored · **P**=phased multi-deploy · **R**=recovery-path · **D**=denied-by-security · tier **v1** / **L8r**.

| # | Scenario | Handling | Tier |
|---|---|---|---|
| 1 | Create table | A | v1 |
| 2 | Add nullable column | A | v1 |
| 3 | Add NOT NULL + const default | A | v1 |
| 4 | Add NOT NULL, no default, rows exist | AI+P (backfill) / G | v1 |
| 5 | Add index | A (CONCURRENTLY non-txn) | v1 |
| 6 | Add constraint (FK/unique/check) | AI (NOT VALID→VALIDATE) / G | v1 |
| 7 | Drop column | G (destructive) | v1 |
| 8 | Drop table | G (destructive) | v1 |
| 9 | Rename column/table | AI+P (expand-contract) | v1 (simple) / L8r (online) |
| 10 | Change column type | AI+P (lossy→backfill) / G | v1 (widen) / L8r (online) |
| 11 | Add/change/drop default | A / G(drop) | v1 |
| 12 | Add enum value / remove enum value | A (add, non-txn) / AI+G (remove) | v1 / L8r |
| 13 | Backfill new column | AI | v1 |
| 14 | Split column | AI+P | v1 |
| 15 | Merge columns | AI+P | v1 |
| 16 | Transform/normalize data | AI | v1 |
| 17 | Seed reference data | A/AI | v1 |
| 18 | Large-table backfill | P (batched/resumable) | L8r |
| 19 | One app owns, others use | A (union schema) | v1 |
| 20 | Two apps declare same table | A(identical) / error(conflict) | v1 |
| 21 | App A migrates table B reads | ordering (lock+UUIDv7) | v1 |
| 22 | Concurrent app deploys | A (project advisory lock) | v1 |
| 23 | Add app to project | A (grant access) | v1 |
| 24 | Remove app | A (tables persist) | v1 |
| 25 | Cross-slice FK dependency | depends_on / clean fail | v1 |
| 26 | Dev/preview auto-apply | A | v1 |
| 27 | Promote preview→prod | A (same versioned migrations) | v1 |
| 28 | Per-branch env data | A (separate db) | L8r |
| 29 | First deploy (empty db) | A | v1 |
| 30 | Re-deploy, no change | A (no-op) | v1 |
| 31 | Baseline existing db | A (baseline cmd) | v1 |
| 32 | Migration fails mid-way | A (txn rollback) / R(non-txn) | v1 |
| 33 | Crash during migration | R (atomic / two-phase) | v1 |
| 34 | Two instances at once | A (advisory lock) | v1 |
| 35 | Out-of-band drift | detect+surface | v1(detect)/L8r(reconcile) |
| 36 | Edited-after-applied | D (checksum hard-error) | v1 |
| 37 | Rollback bad migration | down / roll-forward | v1 |
| 38 | Rollback to version | down-chain | v1 |
| 39 | Ordering vs code rollout | P (expand before, contract after) | v1(discipline)/L8r(auto) |
| 40 | NOT NULL on huge table | P (online) | L8r |
| 41 | Rename without downtime | P (expand-contract) | L8r |
| 42 | Index on huge table | A (CONCURRENTLY non-txn) | v1 |
| 43 | Old code + new schema coexist | P (contract deferred) | v1(discipline)/L8r |
| 44 | How a change is expressed | §3 (declarative + AI author) | v1 |
| 45 | Dry-run / plan / diff preview | A (plan phase) | v1 |
| 46 | Migration history/journal | A (§2.2) | v1 |
| 47 | Lint dangerous ops | A (§1.4 + danger flags) | v1 |
| 48 | Test against shadow db | shadow-apply | L8r |
| 49 | Squash/baseline old | A (baseline) | L8r |
| 50 | Emergency/hotfix | G (operator, audited) | v1 |
| 51 | Version scheme | A (UUIDv7) | v1 |
| 52 | Out-of-order authoring | A (UUIDv7 collision-free) | v1 |
| 53 | Inter-migration deps | depends_on | v1 |
| — | Cross-tenant / priv-esc / RCE SQL | **D** (§1.3/1.4) | v1 |

---

## 7. Phasing

- **v1:** §1 security + §2 executor + deterministic-additive & AI authoring + plan/lint/gate + transactional + journal + advisory lock + roll-forward/recent-rollback + baseline. Covers all 🟢 and most 🟡.
- **Later (at scale / live traffic):** online expand-contract (pgroll-style) for the 🔴 zero-downtime cases, batched-backfill orchestration, drift reconciliation, shadow-db testing, Atlas diff-authoring — all behind the existing seams, no rewrite.

## 8. Crates / placement (proposed)
- New crate `zeroship-migrate` (the executor; depends on `compio-postgres`) — the `MigrationEngine`/`MigrationAuthor` seams + the journal + apply flow + security gate (parser/deny-list). Out-of-band at deploy (not the request hot path → zero-tokio-safe).
- Control plane invokes it as a deploy step; the sandbox/builder invokes it for preview.
- Reuses: `typed_id` (UUIDv7), advisory-lock conventions, immutability-trigger pattern, role/RLS foundation (0025/0026).

## 9. Open questions
- Per-app capability scoping default surface (read-only storefront) — opt-in mechanism shape.
- Online-executor (pgroll vs Reshape vs own) selection — deferred to the L8r phase; seam keeps it open.
- Backup/snapshot hook before destructive/irreversible applies (PITR vs logical) — recommended for the destructive gate.
