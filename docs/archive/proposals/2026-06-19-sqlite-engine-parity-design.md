# SQLite parity in the zeroship-migrate engine — design

<!-- Added in round 2 (2026-06-19, post-critic): revision log -->
## Revision log (2026-06-19, post-critic)

The first draft scored ~41/100 (Security 22). A design-critic showed that the
"line-2 confinement" the draft leaned on does **not exist** in the repo, the
migration connection is **anti-hardened** (process-global `vec0` auto-extension +
CDC hooks on the shared writer actor), the per-app ATTACH isolation is **open by
construction** until an authorizer lands, and the journal immutability trigger is
**forgeable**. This revision replaces optimism with concrete, named mechanisms.
Every change is anchored to a real `rusqlite` / SQLite API and a real
file:line. Summary by flaw id:

- **C1 (line-2 vaporware / anti-hardened conn)** → §2.5.1 specifies the **real**
  hardened connection: `Connection::authorizer(Some(F))` (real action codes),
  `Connection::load_extension_disable()`,
  `set_db_config(SQLITE_DBCONFIG_DEFENSIVE, true)`, `PRAGMA trusted_schema=OFF`,
  installed at connection open **before any creator SQL** runs. The authorizer
  is now the load-bearing line-2, with an explicit `SQLITE_FUNCTION` allowlist.
- **C1/H4 (shared writer actor inherits `vec0` + CDC + data-plane multiplex)** →
  §2.1.1 makes the migration connection a **dedicated, hardened, CDC-free,
  extension-free connection** that does NOT go through plugin-db's
  `SqliteSession` actor. `vec0` is made available **only** on a transient
  per-rebuild basis for legitimate vector-index DDL (§2.6), never left on for
  creator DDL.
- **C3 (cross-tenant ATTACH open by construction)** → §2.5.2: the engine
  ATTACHes the one app file **before** installing the authorizer, then the
  authorizer denies `SQLITE_ATTACH`/`SQLITE_DETACH` for the rest of the
  connection's life. Cross-tenant is closed by construction under the hardened
  model (proof in §2.5.2).
- **C4 (journal immutability forgeable via `writable_schema`/`DROP TABLE`)** →
  §2.2.1: DEFENSIVE=ON blocks `writable_schema` writes; the authorizer denies
  `SQLITE_PRAGMA`, and denies `SQLITE_DROP_TABLE`/`SQLITE_DROP_TRIGGER`/
  `SQLITE_ALTER_TABLE`/`SQLITE_UPDATE`/`SQLITE_DELETE` on `_mig` objects during
  the creator phase; the append-only trigger is the backstop. The `DROP TABLE`
  bypass-of-`BEFORE DELETE`-triggers is addressed head-on.
- **C5 (atomic DDL+journal-row vs confining `up` from `_mig`)** → §2.2.2: ONE
  coherent model — `_mig` stays **ATTACHED throughout, never detached**;
  confinement is **by authorizer state**, not by attach/detach. Exact phase
  sequence (`BEGIN IMMEDIATE` → authorizer=creator-mode → run `up` →
  authorizer=engine-mode → INSERT journal → `COMMIT`), race-free on one
  connection (with the prepare-time-authorizer caveat handled).
- **M4 (per-table AUTOINCREMENT breaks cross-table order)** → §2.2: a **single
  shared monotonic sequence table** (`_mig.event_seq`) issues `event_seq` to all
  three event tables, matching the PG shared sequence (journal.rs:174).
- **H1/#3 (cross-process lock honesty)** → §2.3: the current SQLite lock is
  **in-process only** (lock.rs:1-10); cross-process is a **named later phase
  (P5b)** with a concrete design (lock row + `BEGIN IMMEDIATE` + busy_timeout +
  WAL stale-read reasoning). **No concurrency parity is claimed until P5b lands.**
- **H2/#1/#6 (line-1 guard / libpg_query can't parse SQLite)** → §2.5.3: the
  Confined SQLite path is **descriptor-diff-generated DDL ONLY** (no untrusted
  raw SQL). AI/raw SQLite (incl. the 12-step rebuild) uses
  authorizer-on-`prepare_v2` validation with its **limits acknowledged** — we do
  NOT claim line-1 parity; we claim an enumerated, fail-closed line-2 defense.
- **M3/#5 (P1 trait surface bigger than "leaf I/O")** → §2.1 + P1 rewrite: the
  trait must abstract **parse-time non-txn validation** (executor.rs:490 calls
  `pg_query::parse` directly) and **drift snapshot** (information_schema /
  pg_catalog), not just connection I/O. P1's regression bar restated.
- **H3 (declarative.rs to be REPLACED, not dialect-ized)** → §2.8: ordering
  dependency made explicit — the `zeroship-schema` relocation (2026-06-18 §5,
  declarative.rs marked REPLACE at that doc's line 138) lands **first**; P4 routes
  through relocated emission and never dialect-izes soon-to-be-deleted code.
- **M2 (12-step rebuild is a precondition, not a tail item)** → §2.4 + phasing:
  elevated to **P3b**, net-new in `zeroship-schema`, sequenced **before**
  rollback (P5) and expand-contract (P6), which are gated on it.
- **M1 (security regressions laundered as "documented divergence")** → §2.6.1:
  divergences are split into **(a) legitimate dev-scale degradations** (vector
  inner-product, FTS5 language, geo no-index) and **(b) security properties**
  (journal integrity, cross-tenant isolation, line-2) which MUST be at parity,
  never divergent.
- **M5/L1/L2/L3** → §2.9: bundled SQLite 3.51 (rusqlite) vs system SQLite on the
  Trusted/CLI path; `_mig` alias + trigger identifier quoting and the
  NAMEDATALEN-class analog; `transaction:false` rejection at the dialect
  boundary in the generic apply path; `GuardConfig` SQLite consumer.
- **Phasing inversion** → §3 re-ordered so confinement (authorizer + DEFENSIVE +
  dedicated hardened connection) lands **with/before** the first SQLite apply of
  any non-engine-generated SQL. P2 no longer runs migrations before confinement.

**Round-2 spec-precision fixes (post round-2 design-critic, verified against
rusqlite 0.39 source + bundled SQLite 3.51.3).** Five targeted corrections, no
redesign: (1) **API name** — replaced the non-existent
`DbConfig::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION` (commented out at config.rs:25;
`DbConfig` is `#[non_exhaustive]`) with the real `Connection::load_extension_disable()`
across §2.1.1/§2.5.1 and the symbol list; retracted the false "verified" claim for
that symbol (DEFENSIVE/TRUSTED_SCHEMA at config.rs:38,56 kept). (2) **`_mig` match
precision** — the deny keys on the OUTER `AuthContext.database_name == Some("_mig")`,
not a per-action `database` field (`DropTable { table_name }` /
`DropTrigger { trigger_name, table_name }` carry none; hooks/mod.rs:54,114,131,230,247);
§2.9 version floor now asserts the zDb-on-DROP_TABLE authorizer-arg semantics
(sqlite3.c:127925/156743). (3) **Mode mechanism** — `Arc<AtomicU8>` captured
by-move into the single installed closure (satisfies `Send + 'static`,
hooks/mod.rs:447; `Rc<Cell<_>>` would not compile), flipped by atomic store, never
`conn.authorizer(...)` re-install (impossible mid-`execute_batch`); separate
prepare/execute calls with the flip between, spelled out for the §2.4 12-step
rebuild. (4) **Creator-trigger/view `_mig`-write vector** — new §2.2.1 item 6 +
deny-matrix row: DENY `CREATE TRIGGER`/`CREATE VIEW` bodies targeting `_mig` at
CREATE-prepare time (`AuthContext.accessor`, hooks/mod.rs:58), closing the
§2.2.2(b) defer-into-engine-mode hole. (5) **DQS off** — added
`SQLITE_DBCONFIG_DQS_DDL`/`DQS_DML=false` to the hardened profile (config.rs:46,48).
Plus: §2.2 event_seq committed to a single `… RETURNING next - 1` style; §2.3 P5b
notes `app`/`_mig` are separate files with separate lock bytes and the `_mig`
journal file's RESERVED lock is the cross-process gate.

---

**Status:** proposed (2026-06-19). Reverses the §9/§10/R4 deferral in
`2026-06-18-schema-authority-drizzle-model-design.md` ("full SQLite-backend
parity in the engine" was a non-goal). User now explicitly requires it: SQLite
apps should go through the `zeroship-migrate` engine at parity with Postgres, so
the legacy `registerModel`-SQLite dialect-split (`run_sqlite_pipeline`) can be
retired.

**Goal:** the engine (versioned journal, guard/confinement, drift, rollback,
expand-contract, goodies) applies migrations on SQLite, single-sourced with the
Postgres path — no forked executor.

<!-- Added in round 2: H1 honesty — concurrency parity is NOT claimed up front -->
**Parity scope, stated honestly up front.** Two properties are explicitly NOT at
parity in the initial phases and are called out wherever they appear, so the doc
does not over-claim:

1. **Cross-process serialization** is in-process-only today (lock.rs:1-10 says so
   itself: "cross-process serialisation … is a P5+ concern"). It is deferred to a
   **named phase P5b** (§2.3). Until P5b lands we claim **in-process** apply
   safety only, not concurrency parity with PG's `pg_advisory_lock`.
2. **Line-1 (the parser deny-list)** has **no SQLite equivalent** — libpg_query is
   the Postgres grammar and cannot parse SQLite (§2.5.3). We do not claim line-1
   parity; instead the Confined SQLite path eliminates untrusted SQL strings
   entirely (descriptor-diff DDL) and a runtime **authorizer** (line-2) is the
   load-bearing, fail-closed defense.

The **security properties that MUST be at parity** — journal integrity,
cross-tenant isolation, and a real runtime line-2 — are designed to parity below
and are NOT treated as acceptable divergences (§2.6.1, M1).

---

## 1. Current state (verified against the worktree)

### 1.1 `crates/zeroship-schema` is already dual-dialect (the describe/shape layer)

`query.rs` carries a first-class `SqlDialect` enum (`Postgres | Sqlite`) threaded
through the DDL builders:

- `build_create_table_with_fks_for_dialect(app_id, collection, schema, fk_emit, dialect)`;
  the PG-defaulting `build_create_table_with_fks` calls it with `Postgres`. The
  SQLite arm handles timestamp affinity (`TIMESTAMPTZ`→`TEXT`), default funcs
  (`NOW()`→`CURRENT_TIMESTAMP`) via `build_system_field_columns(dialect)`,
  `COMMENT ON COLUMN` suppression (SQLite bakes the `/* __zsmask:... */` sentinel
  inline into the CREATE TABLE body so `sqlite_master.sql` introspection recovers
  it), schema-qualified FK-parent rejection, and index `ON`-clause placement.
- `build_create_indexes`/`build_named_indexes` branch BTree/Vector/FTS/Spatial
  per dialect (divergences in `docs/reference/sqlite-divergences.md`:
  `sqlite-vec` cosine+L2 only, FTS5 ignores language, spatial = haversine
  flat-scan).
- `mask_codec.rs` is dialect-neutral (a string format); only the *write site*
  differs (PG `COMMENT` vs SQLite inline comment).
- Encrypted binds: `SqlDialect::encrypted_column_bind_placeholder` /
  `wrap_encrypted_param` already model PG `decode(...)::bytea` vs SQLite
  `SQLITE_ENC_BLOB_PREFIX` BLOB sentinel.

**The DDL-text generation layer is already SQLite-capable.** Everything that
*consumes/executes* that text is not.

### 1.2 The engine executor/journal/role — hard-wired to Postgres

- **`executor.rs`** (~3360 lines): `apply(conn: &compio_postgres::Client, ...)`.
  `pg_advisory_lock(hashtext($1)::bigint)` serialization; `SET LOCAL
  search_path/statement_timeout/lock_timeout` + `SET LOCAL ROLE "<migrator>"`
  confinement; GUC snapshot/restore via `current_setting()`/`set_config()`;
  non-txn two-phase recovery around `CREATE INDEX CONCURRENTLY` / `ALTER TYPE …
  ADD VALUE` (parsed with `pg_query`).
- **`journal.rs`**: journal in a per-project **meta schema**; immutability via a
  **plpgsql trigger** (`RAISE EXCEPTION` on UPDATE/DELETE/TRUNCATE); net-state
  via `DISTINCT ON` CTEs over a `BIGINT … DEFAULT nextval(...)` sequence.
- **`role.rs`**: the entire least-privilege model is PG roles — `CREATE ROLE …
  NOSUPERUSER NOCREATEROLE NOLOGIN`, `ALTER SCHEMA … OWNER TO`, `REVOKE/GRANT`,
  `ALTER ROLE … SET search_path`. **Line-2 confinement.**
- **`drift.rs`** `snapshot_schema`: `information_schema` + `pg_catalog` SQL.
- **`guard.rs`/`classify.rs`/`analyze.rs`**: 100% `pg_query` (libpg_query — the
  *Postgres* grammar). **Line-1 deny-list.** The crux of the SQLite problem.
- **`declarative.rs`** (engine differ/author): has its own PG-only emitter
  (`quote_ident`, `GENERATED ALWAYS AS (…) STORED`, `dsl_to_pg_data_type`); calls
  `zeroship_schema::diff` for *metadata* but emits PG DDL for the `up`. Does NOT
  yet call `build_create_table_with_fks_for_dialect`.

`ExecutorConfig` is PG-shaped (`meta_schema`, `migrator_role`,
`extension_schemas`, `search_path_clause()`, timeout GUCs).

### 1.3 plugin-db's SQLite path (what the engine must replicate)

`register_model/mod.rs`: PG arm `(Some(_pg), _) => Ok(())` is a **no-op** (engine
owns PG schema at deploy). SQLite arm → `run_sqlite_pipeline` still runs a
**runtime auto-migrate** from `default.schema`: reject cross-app FK → in-process
`SqliteLockGuard` → build ctx → compute plan → validate → `apply_sqlite`.
`apply_sqlite` iterates the diff, **skips `Destructive`**, audits per op, applies
via `backend.exec_batch` / typed index builders; mask/rewrite/type-rewrite →
`backend_unsupported`; DropColumn/DropIndex skipped. So today's SQLite apply is
best-effort additive: no journal, no versioning, no rollback, no destructive
handling, no checksum/drift, no two-phase recovery.

### 1.4 Driver + zero-tokio

Driver is **`rusqlite`** (bundled SQLite 3.51.3 via the `bundled` feature,
`preupdate_hook`) + `sqlite-vec` (`vec0`). No compio-native SQLite driver.
Zero-tokio is preserved by a **single `rusqlite::Connection` on a writer actor
spawned via `compio::runtime::spawn_blocking`**, draining a `flume` mpsc queue;
callers `await` a `flume` reply. Per-app DBs via `ATTACH DATABASE`;
`journal_mode=WAL` + `busy_timeout`. The lock (`sqlite/lock.rs`) is **in-process
only** (lock.rs:1-10: `RefCell<HashMap<…>>`, cross-process `BEGIN
IMMEDIATE`/sentinel-table flagged as P5+ future work).

<!-- Added in round 2: C1/H4 — the shared actor is anti-hardened; the migration conn must NOT reuse it -->
**Critical: the existing writer actor is anti-hardened and MUST NOT be reused for
migrations.** The actor (`SqliteSession::open`, session.rs:309-397):

- **Auto-loads the `vec0` C extension process-globally.** `register_sqlite_vec_once`
  (session.rs:74-96) calls `rusqlite::ffi::sqlite3_auto_extension` via
  `std::mem::transmute`. That registration is **process-global**: every
  `sqlite3_open*` thereafter — including any new migration connection in the same
  process — inherits `vec0`. So merely opening a fresh `Connection` does **not**
  give us an extension-free surface; the auto-extension must be explicitly
  accounted for (§2.1.1).
- **Installs CDC hooks** (preupdate/commit/rollback) when a `packet_tx` is present
  (session.rs:387-397) — migration DDL backfills would emit spurious
  `ChangeEvent`s.
- **Multiplexes the data plane.** `acquire_dedicated_client`
  (`backend/sqlite/mod.rs:517-526`) returns the **same** shared session, so an
  interleaved runtime CRUD `INSERT` would land **inside** a migration's open
  transaction — a correctness defect, and the inverse of PG's
  under-privileged-role isolation.

The migration path therefore uses a **dedicated, hardened, CDC-free,
extension-suppressed connection** (§2.1.1), never plugin-db's data-plane actor.

---

## 2. Design

### 2.0 Core principle: a dialect seam over execution AND validation/introspection

<!-- Rewritten in round 2: M3 — the seam is bigger than "leaf I/O" -->
The engine separates **authoring/validation (DB-free)** from **execution +
DB-coupled validation (DB-bound)**. DB-free and reusable as-is for SQLite:
`migration.rs` (ids/checksums), `approval.rs`, the topological-order / squash /
expand-contract / repeatable *gating logic* (pure functions over `&[Migration]`),
`manifest.rs`, `loader.rs`, `precondition.rs` shape, and the **checksum/tamper**
comparison (dialect-agnostic).

**The seam is larger than "leaf I/O" — this was the first draft's central
under-scoping.** `apply_locked` (executor.rs:798) does not merely run statements;
it calls **dialect-coupled validation and introspection inline**:

- **Parse-time non-txn idempotency validation:** `pg_query::parse(&m.up)`
  (executor.rs:490, again at executor.rs:2148) — this is the libpg_query
  **Postgres** parser and **breaks on SQLite DDL**. The seam must abstract a
  `validate_non_txn(up) -> Result<NonTxnClass>` step, not call `pg_query` directly.
- **Drift snapshot:** `snapshot_schema` reads `information_schema` + `pg_catalog`
  (drift.rs); SQLite needs `sqlite_master` + `PRAGMA table_info/index_list/
  foreign_key_list`. The seam must abstract `snapshot_schema() -> SchemaSnapshot`.
- **Session/GUC + role + lock leaves:** `pg_advisory_lock(hashtext($1))`
  (executor.rs:324), `current_setting`/`set_config` GUC snapshot/restore
  (executor.rs:351-390), `SET LOCAL ROLE "<migrator>"` (executor.rs:430). None of
  these exist on SQLite; each needs a trait method whose SQLite impl is a
  different mechanism (authorizer state, not GUCs/roles).
- **Journal row shapes** leak PG types (`event_seq` as `i64` over a `BIGINT`
  sequence, `TIMESTAMPTZ` formatting). The trait must expose these as
  dialect-neutral row constructors so the SQLite impl can map
  `INTEGER`/`TEXT CURRENT_TIMESTAMP` without the PG type assumptions reaching the
  generic body.

So the seam is a **`MigrationBackend` trait** spanning **execution I/O,
parse-time non-txn validation, drift introspection, and journal row I/O** —
introduced once, with `Postgres` and `Sqlite` impls, **not** a forked
`executor_sqlite.rs`. The squash/expand/repeatable correctness stays
single-sourced. "Only leaf I/O moves" was false; P1 is correspondingly larger
(see P1 in §3).

### 2.1 `MigrationBackend` trait

No backend seam exists today. Introduce (compio-async; rusqlite is
sync-behind-the-actor, so methods are `async fn` resolving over the `flume`
reply):

- `dialect() -> SqlDialect`
- `lock(project_id) -> LockHandle`
- `begin()/commit()/rollback()` — PG `BEGIN`; SQLite `BEGIN IMMEDIATE`
- `set_authorizer_mode(mode)` — **SQLite only** (`CreatorUp` | `EngineJournal` |
  `Off`); implemented as a `mode.store(...)` on an `Arc<AtomicU8>` captured into
  the single installed authorizer closure (§2.2.2) — **not** a
  `conn.authorizer(...)` re-install. The PG impl is a no-op (PG uses `SET LOCAL
  ROLE` instead, mapped through `enter_confined`/`leave_confined`). This is the
  line-2 toggle (§2.2.2/§2.5.1).
- `run_up_confined(sql)` — one confined `up` statement-set under creator-mode
- `validate_non_txn(up) -> Result<NonTxnClass>` — PG: `pg_query::parse`; SQLite:
  reject `transaction:false` at the dialect boundary (§2.3, L3) — there is no
  non-txn DDL on SQLite to classify.
- journal row I/O (admin-privileged on PG; engine-mode-authorizer on SQLite)
- `snapshot_schema() -> SchemaSnapshot` for drift (PG `information_schema`/
  `pg_catalog`; SQLite `sqlite_master` + PRAGMAs)

The existing `apply_locked` body (drift check, partition, squash/expand gates,
`order_pending`, FIRST/SECOND pass, repeatable phase) becomes generic over the
trait. The `compio_postgres::Client` path becomes `PostgresBackend` —
behavior-identical, the regression bar.

<!-- Added in round 2: H4 — SqliteBackend is a DEDICATED hardened sibling actor, NOT the data-plane actor -->
#### 2.1.1 `SqliteBackend` uses a dedicated, hardened, CDC-free migration connection

`SqliteBackend` does **NOT** reuse plugin-db's data-plane `SqliteSession` actor.
It owns a **thin migration-only sibling actor** — same `spawn_blocking` +
`flume` zero-tokio shape, but a connection opened with the **migration-hardening
profile** instead of the data-plane bootstrap:

1. **Neutralize the process-global `vec0` auto-extension for creator DDL.**
   Honest mechanism statement: `sqlite3_auto_extension(vec0)` (session.rs:84) is
   **process-global and cannot be cancelled per-connection** — once the data plane
   has registered it, every `Connection::open` in the process inherits the `vec0`
   module. We therefore do **not** pretend a fresh connection is extension-free;
   instead we make `vec0` **inert to creator SQL** by two independent controls,
   applied at open before any creator statement:
   - `conn.load_extension_disable()` — the real rusqlite API (lib.rs:877;
     internally calls `enable_load_extension(0)`). On bundled SQLite 3.51.3 this
     disables both the `load_extension()` SQL function and the C
     `sqlite3_load_extension` API, so **no further** extension can be loaded.
     (Note: there is **no** `DbConfig::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION`
     variant in rusqlite 0.39 — it is commented out at config.rs:25, and
     `DbConfig` is `#[non_exhaustive]`; the load-extension toggle is reached only
     via this dedicated method, not `set_db_config`.) And
   - the authorizer denies, in creator mode, `SQLITE_CREATE_VTABLE`,
     `SQLITE_CREATE_MODULE`, and any `SQLITE_FUNCTION` outside the allowlist
     (§2.5.1) — so creator SQL cannot `CREATE VIRTUAL TABLE … USING vec0` nor call
     `vec_*`. A merely *registered* module is unreachable if no statement is
     authorized to invoke it.
   The engine itself never calls `register_sqlite_vec_once()` on the migration
   actor. **Where a legitimate vector-index migration needs `vec0`**, the engine
   emits that one `USING vec0` / `vec_*` statement under **engine mode**
   (§2.5.1/§2.6), which is the only mode that allows it — never creator mode.
2. **No CDC.** The sibling actor opens with `packet_tx = None`, so the CDC hook
   triplet (session.rs:387) is never installed. Migration DDL backfills emit no
   `ChangeEvent`s.
3. **No data-plane multiplexing.** The actor is migration-private; runtime CRUD
   never shares its connection, so no interleaved `INSERT` can land inside a
   migration transaction (closes the H4(b) correctness defect).
4. **Hardening profile** (§2.5.1) is applied at open, before the app file is
   reachable by any creator statement.

Zero-tokio is intact: it is the **same** `spawn_blocking` + `flume` pattern, just
a second, security-scoped actor.

### 2.2 Journal on SQLite

SQLite has no schemas, so "per-project meta schema" → **a separate attached
journal file**: `ATTACH DATABASE 'file:<app>.migrations.sqlite' AS "_mig"`;
journal tables live under `_mig`. The engine attaches `_mig` **once at connection
open** (before the hardening authorizer is installed) and **keeps it attached for
the connection's whole life** — see §2.2.2 for why detach/re-attach is rejected.
`ExecutorConfig.meta_schema` generalizes to a `JournalLocation` (PG schema /
SQLite attached alias). The alias is the fixed literal `"_mig"` (not the app id),
which also sidesteps the identifier-length and quoting hazards in L2 (§2.9).

<!-- Rewritten in round 2: M4 — ONE shared monotonic sequence, not per-table AUTOINCREMENT -->
**Cross-table total order — a single shared monotonic sequence (M4).** PG uses
**ONE** sequence shared across all three event tables
(`schema_migrations_event_seq`, journal.rs:169-177) so the *latest event per
version* is decided by a strictly-increasing total order that never ties, even
when two events share a `now()` timestamp in one transaction. Per-table
`INTEGER PRIMARY KEY AUTOINCREMENT` would give each table its **own** monotonic
counter, so `event_seq` values would **not** be comparable across
`schema_migrations` / `schema_migrations_rolled_back` /
`schema_migrations_supersedes` — silently breaking `applied()`,
`net_rolled_back()`, and `superseded_versions()` net-state ordering.

SQLite has no `CREATE SEQUENCE`. Replicate the shared sequence with a **dedicated
single-row counter table** under `_mig`:

```sql
CREATE TABLE IF NOT EXISTS "_mig".event_seq (id INTEGER PRIMARY KEY CHECK (id=1), next INTEGER NOT NULL);
INSERT OR IGNORE INTO "_mig".event_seq (id, next) VALUES (1, 1);
```

Every event insert (into any of the three tables) allocates its `event_seq` from
this one row, inside the apply transaction:

```sql
-- ONE style (round-2): RETURNING yields the allocated value (the pre-increment
-- `next`) atomically in the same statement, inside the apply transaction:
UPDATE "_mig".event_seq SET next = next + 1 WHERE id = 1 RETURNING next - 1;
```

`RETURNING` is available in bundled SQLite 3.51 (≥3.35). The single committed
style above returns the allocated `event_seq` (`next - 1`, i.e. the value of
`next` before this increment) directly — no separate read-back via `changes()`,
no ambiguity. Because every allocation
happens **inside the single apply transaction on the single migration
connection**, and the engine is the only writer of `_mig` (authorizer denies all
creator writes to `_mig`, §2.2.1), the counter is a true shared monotonic source
matching PG's semantics. The `event_seq` column on each event table is a plain
`INTEGER` (the assigned value), **not** `AUTOINCREMENT`.

`DISTINCT ON` net-state → `ROW_NUMBER() OVER (PARTITION BY version ORDER BY
event_seq DESC)` (SQLite ≥ 3.25 window functions; bundled 3.51 satisfies this —
see M5/§2.9). `TIMESTAMPTZ DEFAULT now()` → `TEXT DEFAULT CURRENT_TIMESTAMP`.
Logical journal shape unchanged.

#### 2.2.1 Journal immutability — closing the `writable_schema`/`DROP TABLE` forge (C4)

The first draft relied on `BEFORE UPDATE/DELETE` triggers alone. The critic showed
two forgeries that defeat triggers:

- **`PRAGMA writable_schema=ON; DELETE FROM "_mig".sqlite_master WHERE name='…trg'`**
  drops the immutability trigger by editing the schema table directly.
- **`DROP TABLE "_mig".schema_migrations`** wipes the journal wholesale, and SQLite
  **`BEFORE DELETE` triggers do NOT fire on `DROP TABLE`** (there is no `TRUNCATE`
  in SQLite, and `DROP` is not a row-level `DELETE`). This is the SQLite analog of
  the wholesale wipe PG armed a statement-level `BEFORE TRUNCATE` trigger against
  (journal.rs:291-299) — so the first draft's "TRUNCATE leg is unneeded" was wrong.

Tamper-resistance is now **defense in depth**, not a single trigger:

1. **`set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)`** (DEFENSIVE mode)
   — makes the `sqlite_master`/`sqlite_schema` tables **read-only to SQL even
   when `writable_schema=ON`**, so the `writable_schema` forge cannot delete the
   trigger row. (DEFENSIVE also blocks direct writes to shadow tables of virtual
   tables.) Installed at connection open, before any creator SQL.
2. **`PRAGMA trusted_schema=OFF`** — prevents schema objects (triggers/views/
   generated-column expressions/CHECK constraints stored in `sqlite_master`) from
   invoking non-deterministic or non-allowlisted functions, closing the
   "poison a schema object to run a function at prepare/step time" surface.
3. **Authorizer denies `SQLITE_PRAGMA`** for the creator phase (§2.5.1) — so
   `PRAGMA writable_schema=ON` is rejected at prepare time regardless of DEFENSIVE.
   (DEFENSIVE is the backstop in case any PRAGMA path is reachable; the authorizer
   is the primary deny.)
4. **Authorizer denies, when `AuthContext.database_name == "_mig"`,** the
   action codes `SQLITE_DROP_TABLE` (11), `SQLITE_DROP_TRIGGER` (16),
   `SQLITE_ALTER_TABLE` (26), `SQLITE_UPDATE` (23), `SQLITE_DELETE` (9), and
   `SQLITE_INSERT` (18) during the **creator-up phase**.

   **Match the OUTER context field, not a per-action `database` field (CRITICAL
   precision).** The attach alias is carried **only** on the outer
   `AuthContext.database_name: Option<&str>` (hooks/mod.rs:54) — it is the 5th
   authorizer argument (`zDb`) that SQLite passes for these actions. It is **not**
   a field on the `AuthAction` variant: rusqlite's `AuthAction::DropTable { table_name }`
   and `DropTrigger { trigger_name, table_name }` (hooks/mod.rs:114,131) carry **no
   database field** — the `zDb` argument is dropped at variant construction
   (hooks/mod.rs:230,247) and surfaces only on `AuthContext.database_name`. An
   implementer who pattern-matches `DropTable.database` will find no such field; the
   deny MUST key on `ctx.database_name == Some("_mig")`. Verified in bundled SQLite
   3.51.3: DROP_TABLE (sqlite3.c:127925), DROP_TRIGGER (sqlite3.c:156743), and the
   DML sites all pass `zDb` (the attach alias) as the 5th `xAuth` argument, which
   rusqlite surfaces as `AuthContext.database_name`. This directly defeats
   `DROP TABLE "_mig".…` at prepare time — the `DROP TABLE`
   bypass-of-BEFORE-DELETE-triggers is moot because the statement never compiles.
5. **`CREATE TRIGGER … BEFORE UPDATE/DELETE … BEGIN SELECT RAISE(ABORT,
   'append-only'); END`** remains as the in-DB backstop for row mutation on the
   Trusted/operator path (where the authorizer is relaxed). It does NOT defend
   against `DROP TABLE` (acknowledged), which is why items 1–4 carry the
   Confined-path guarantee.
6. **Deny creator-authored TRIGGER/VIEW bodies that write `_mig` — at CREATE
   time (round-2, security).** Under `CreatorUp` mode, **DENY** `CREATE TRIGGER`
   and `CREATE VIEW` whose body targets `AuthContext.database_name == Some("_mig")`.
   *Attack closed:* a creator could write
   `CREATE TRIGGER t AFTER INSERT ON app.some_table BEGIN INSERT INTO "_mig".schema_migrations …; END;`.
   The trigger BODY's `_mig` write is authorized **when the firing DML is
   prepared**, not when the trigger fires — so if that firing DML were later
   prepared under `EngineJournal` mode, the deferred `_mig` write inside the
   trigger body could be allowed, smuggling a forged journal row past the
   item-4 deny. This is the §2.2.2(b) "defer execution into engine-mode" hole.
   It is **catchable at the trigger/view's own CREATE-prepare time**: rusqlite's
   `AuthContext.accessor` (hooks/mod.rs:58) carries the inner-most trigger/view
   responsible for each access attempt, so while the trigger body is being
   compiled (under `CreatorUp`, since the creator issues the `CREATE TRIGGER`),
   the authorizer sees each body statement with `database_name == Some("_mig")`
   and an `accessor` naming the creator's trigger/view — and DENIES it **there**,
   at CREATE time, before the trigger ever exists. This forecloses the
   defer-into-engine-mode vector at its root (the trigger is never created), so
   no later EngineJournal-mode firing can resurrect it.

**Net:** on the Confined path the journal is immutable *by authorizer
construction* (direct writes/DDL to `_mig` denied at prepare per item 4, AND
indirect writes via creator-authored trigger/view bodies denied at CREATE time per
item 6) with DEFENSIVE + trusted_schema=OFF as backstops, matching PG's "immutable
by construction, not by least-privilege alone" property.

#### 2.2.2 Atomic "DDL + journal-row in ONE transaction on ONE connection" (C5)

The sharpest tension: the engine commits the creator's DDL **and** the journal
row in a single transaction (PG: `apply_transactional`, executor.rs commits DDL +
journal atomically), but the creator `up` must be confined from touching `_mig`.
The first draft's open question floated "detach `_mig` during `up`, re-attach for
the journal write" — which **breaks atomicity** (you cannot detach inside a
transaction that has written, and re-attach would need a second connection,
breaking `BEGIN IMMEDIATE` single-writer serialization).

**Chosen model: keep `_mig` ATTACHED throughout; confine by authorizer STATE, not
by attach/detach.** One connection, one transaction, atomic. Exact phase sequence:

```
1. begin:               BEGIN IMMEDIATE;            -- one writer, RESERVED lock taken now
2. authorizer=Creator:  set_authorizer_mode(CreatorUp)
                        -- denies ATTACH/DETACH/PRAGMA/load_extension/CREATE_VTABLE,
                        --   FUNCTION outside allowlist, and ALL writes+DDL to "_mig"
3. run up:              prepare+step each creator/engine-generated up statement
                        -- every statement is prepared UNDER CreatorUp, so the
                        --   authorizer vets it at prepare time (see caveat below)
4. authorizer=Engine:   set_authorizer_mode(EngineJournal)
                        -- allows writes to "_mig" (the journal tables only),
                        --   still denies ATTACH/DETACH/PRAGMA/load_extension
5. insert journal:      UPDATE "_mig".event_seq …; INSERT INTO "_mig".schema_migrations …
6. commit:              COMMIT;                     -- DDL + journal row commit together
```

**Mode mechanism: one installed closure + an `Arc<AtomicU8>`, NOT closure
re-installation (round-2 precision).** rusqlite's `Connection::authorizer(Some(F))`
requires `F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static`
(hooks/mod.rs:447). The mode is therefore an `Arc<AtomicU8>` (which is `Send +
'static`) **captured by-move into the single closure installed once at connection
open**; flipping the mode is a plain `mode.store(EngineJournal, Relaxed)` (or
`SeqCst`) on that atomic — it does **not** re-call `conn.authorizer(...)`. An
`Rc<Cell<_>>` would NOT compile here: `Rc`/`Cell` are not `Send`, so they fail the
`Send + 'static` bound on `F`. The closure reads the current mode from the atomic
on each `prepare`-time invocation and branches the deny matrix accordingly.

This choice is also what makes the flip safe vs. `execute_batch`. rusqlite's
`Connection::execute_batch` holds `db.borrow_mut()` and prepares-then-steps each
statement in a loop over the whole batch string, so **`conn.authorizer(...)`
cannot be re-installed mid-`execute_batch`** (the connection is borrowed for the
batch's duration) — but a `mode.store(...)` on the captured `Arc<AtomicU8>` **can**
happen at any time because it never touches the connection. So the mode flip is
mechanically possible exactly where closure re-installation would be impossible.

**Why this is race-free on one connection.** All six steps run on the **single
migration actor's single connection**, drained from one `flume` queue — they are
strictly sequential by construction; no other statement can interleave between
steps 3 and 4 on this connection. The mode is the captured `Arc<AtomicU8>`;
flipping it between steps is a plain synchronous atomic store with no `.await`
across the flip. There is no second connection, so `BEGIN IMMEDIATE`'s
single-writer guarantee holds for the whole transaction.

**Statement granularity: separate prepare/execute calls, never one batch (the
12-step rebuild, §2.4).** Because the mode is read at each statement's `prepare`
time, the engine MUST issue creator-mode statements and engine-mode statements as
**separate `prepare`/`execute` calls** with the atomic-store flip strictly
between them — it must **never** put a creator-affecting statement and an
engine-mode statement in the same `execute_batch` string, since the whole batch is
prepared/stepped under whatever single mode was current when the batch started.
This is load-bearing for the §2.4 12-step rebuild, which interleaves engine-mode
`PRAGMA foreign_keys` toggles around creator-affecting copy DDL: each phase
(`PRAGMA foreign_keys=OFF` [engine] → create-new/copy/drop/rename [engine-emitted
DDL] → `PRAGMA foreign_key_check` [engine] → `PRAGMA foreign_keys=ON` [engine])
is a discrete `prepare`/`execute` call, with the `mode.store(...)` boundary set
correctly before each. The rebuild therefore runs entirely under engine mode (it
is engine-generated), but the same discipline — one statement per prepare, flip
between — is what lets any creator `up` step in step 3 be vetted independently
from the journal write in step 5.

**Prepare-time-authorizer caveat (made explicit).** rusqlite's authorizer fires
during `sqlite3_prepare_v2`, **not** during `step` (except a re-prepare on schema
change). Two consequences, both handled:

- **Each statement is prepared while the intended mode is active.** The engine
  prepares-and-steps creator `up` statements one at a time under `CreatorUp`
  (step 3), then sets `EngineJournal` and only **then** prepares the journal
  `INSERT`/`UPDATE` (step 5). A statement prepared under `CreatorUp` can never be
  a `_mig` write, because such a prepare would have been denied. So the mode flip
  cannot retroactively "unlock" an already-prepared creator statement.
- **Schema-change re-prepare** re-invokes the authorizer under the *current* mode.
  Because the journal write is the only `_mig` write and it is prepared under
  `EngineJournal` after all creator DDL, a creator DDL statement re-prepared
  mid-step is re-vetted under `CreatorUp` (still denied for `_mig`), which is the
  safe direction.

This preserves PG's atomic `apply_transactional` guarantee (DDL + journal commit
together) while confining the creator `up` — confinement **by authorizer state**,
exactly the recommended direction.

### 2.3 Concurrency serialization (replacing `pg_advisory_lock`)

<!-- Rewritten in round 2: H1 — honest about in-process-only; cross-process is a NAMED later phase -->
**Stated honestly: the SQLite lock is in-process-only today and we do NOT claim
concurrency parity with `pg_advisory_lock` until phase P5b.**

1. **In-process (shipped, the P2 guarantee):** the single migration actor's
   single connection serializes structurally — one writer, one `flume` queue.
   `lock.rs`'s `InProcessLockRegistry` (`RefCell<HashMap<…>>`, lock.rs:39-45)
   gives the `LockManager` surface for both `GlobalApp` and `LocalApp` scopes.
   This is **strictly weaker than PG's `pg_advisory_lock`**, which serializes
   **across processes**.

2. **Cross-process (deferred to named phase P5b, §3):** a **lock row + `BEGIN
   IMMEDIATE` + `busy_timeout`**. Design:
   - A single-row table `"_mig".apply_lock(id INTEGER PRIMARY KEY CHECK(id=1))`.
   - Apply start runs `BEGIN IMMEDIATE`, which takes SQLite's **RESERVED** write
     lock immediately (not lazily at first write), so a second process's
     `BEGIN IMMEDIATE` fails fast. **Which file's lock gates two processes (P5b
     detail):** `app` and `_mig` are **separate attached database files with
     separate lock bytes** — a transaction does not lock both unless it writes
     both. To get a single cross-process gate the apply MUST write the `_mig`
     lock row (`"_mig".apply_lock`) first under `BEGIN IMMEDIATE`, so it is the
     **`_mig` journal file's RESERVED lock** that serializes two apply processes
     (every apply writes the journal, so every apply contends on that one file).
     The `app` file's lock is incidental. Kept brief here; full treatment deferred
     to P5b.
   - `busy_timeout` provides the bounded wait; **`SQLITE_BUSY` maps to the engine's
     typed contention error** (`LockContended`), so a concurrent apply surfaces a
     retryable error rather than corrupting state.
   - The `hashtext` 32-bit collision caveat (PG) does not apply — the lock is keyed
     on the exact `project_id` (the journal file path), not a hash.

   **WAL stale-read reasoning (the open question #3, now answered).** Under WAL,
   readers do **not** block writers and see a **snapshot** as of their last read
   transaction. The risk is a second apply reading a *stale* journal snapshot
   (missing the first apply's just-committed rows) and re-deciding net state. Two
   facts close this for the P5b design: (a) every apply takes the **write** lock
   via `BEGIN IMMEDIATE` before reading the journal, so two applies cannot both be
   in their read+decide phase simultaneously — the loser blocks until the winner
   `COMMIT`s and is then a *new* transaction that starts a *fresh* WAL snapshot
   including the winner's rows; (b) the journal net-state reads happen **inside the
   same `BEGIN IMMEDIATE` transaction** that writes, so they observe a consistent,
   non-stale snapshot. The stale-read interleave is therefore only possible if a
   reader decides *outside* the write lock — which the design forbids.

   **Until P5b lands, the engine MUST refuse to run on SQLite when a cross-process
   apply could occur** (e.g. multi-worker prod). The Confined dev-tier path is
   single-process by construction (one worker owns the file), so P2–P5 ship safely;
   P5b is the gate for any multi-process SQLite deployment.

<!-- Added in round 2: L3 — transaction:false rejection at the GENERIC dialect boundary -->
**Two-phase non-txn recovery & `transaction:false` (L3).** PG needs two-phase
recovery for `CREATE INDEX CONCURRENTLY` / `ALTER TYPE ADD VALUE`. **SQLite has
neither.** `transaction:false` migrations are **rejected at the dialect boundary**
inside the now-generic `apply_locked`: the `MigrationBackend::validate_non_txn`
method returns a `NonTxnUnsupportedOnDialect` error for the SQLite impl **before**
any apply, rather than the generic body assuming a PG non-txn path exists. This is
a real guard at the seam, not an implicit assumption — SQLite DDL is transactional
anyway, so this is a place SQLite is genuinely simpler, not degraded.

### 2.4 Transactional DDL — confirmed, one exception

SQLite supports transactional DDL (CREATE/ALTER/DROP TABLE, CREATE INDEX roll
back in `BEGIN…ROLLBACK`), so the engine's default atomic path (`BEGIN; <up>;
INSERT journal; COMMIT`) maps directly — journal write + DDL commit atomically.

<!-- Rewritten in round 2: M2 — the 12-step rebuild is a PRECONDITION, not a tail item -->
**The 12-step table rebuild is a load-bearing PRECONDITION (M2), not a deferred
goodie.** `ALTER TABLE` on SQLite is limited to `ADD/RENAME/DROP COLUMN` +
`RENAME TO`. A type change, added/changed constraint, or most destructive column
operations require the canonical **12-step `ALTER TABLE` procedure** (per the
SQLite docs: `PRAGMA foreign_keys=OFF` → `BEGIN` → create new table → copy rows →
drop old → rename new → recreate indexes/triggers/views →
`PRAGMA foreign_key_check` → `COMMIT` → `PRAGMA foreign_keys=ON`). Today
`apply_sqlite` simply **refuses** `RewriteColumnType` with `backend_unsupported`
(`register_model/mod.rs:461-462`).

This rebuild **gates**, and is therefore a precondition for:

- **Destructive parity** (DropColumn-with-rewrite, type narrowing).
- **Most `.down.sql` reversals** (a forward `ADD COLUMN`'s reverse is a rebuild,
  since SQLite gained `DROP COLUMN` only recently and constraint reversals need a
  rebuild).
- **Expand-contract** column-rewrite contracts.

It is **net-new in `zeroship-schema`** (the emitter does not exist) and is
therefore sequenced as its own phase **P3b — before** rollback (P5) and
expand-contract (P6), which depend on it. It is no longer "deferred to P6". The
rebuild's index/trigger/view recreation must round-trip the inline mask sentinel
and FTS5 sync triggers (§2.6).

**Important interaction with foreign_keys.** The rebuild requires
`PRAGMA foreign_keys=OFF` around it — but the connection sets `foreign_keys=ON`
at open and the authorizer denies `SQLITE_PRAGMA` for the creator phase. The
rebuild is therefore an **engine-generated, engine-mode-authorized** operation:
the engine flips `foreign_keys` via the `EngineJournal`/engine authorizer mode
(which allows the specific `PRAGMA foreign_keys` toggles around the rebuild), not
via creator SQL. The `foreign_key_check` step runs before `COMMIT` so a rebuild
that would orphan rows aborts the transaction.

### 2.5 Security / least-privilege — the crux

**Line-2 (role confinement) does not exist on SQLite.** No roles/GRANT/SET ROLE/
ownership; `role.rs` is inapplicable. The replacement is a **real runtime
authorizer + a hardened connection**, specified concretely below. The first draft
described these mechanisms as if they existed; they do **not** in the repo today
(grep finds zero `set_authorizer`/`load_extension`/`writable_schema`/
`trusted_schema` hits) and the existing connection is *anti*-hardened (§1.4).
This section is the build spec.

#### 2.5.1 The hardened migration connection (C1) — exact APIs and install order

Installed by the dedicated migration actor (§2.1.1) **at connection open, before
the app file is reachable by any creator statement**, in this order:

```
0. ATTACH DATABASE 'file:zs-<app_id>.sqlite' AS app;     -- engine, pre-authorizer (§2.5.2)
0. ATTACH DATABASE 'file:<journal>.sqlite'   AS "_mig";  -- engine, pre-authorizer
1. PRAGMA app.foreign_keys = ON;                          -- (per-conn; via set_db_config below)
2. conn.load_extension_disable();                         -- real rusqlite API (lib.rs:877); NOT a DbConfig variant
3. conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true);
4. conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false);  // PRAGMA trusted_schema=OFF
5. conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false);         // off — no double-quoted-string literals (DDL)
6. conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false);         // off — no double-quoted-string literals (DML)
7. conn.authorizer(Some(creator_or_engine_callback));    // line-2 — installed LAST, before any creator SQL
```

<!-- Added in round 2 (round-2 spec-precision): DQS off — fail-closed identifier hygiene (config.rs:46-49) -->
Step 5/6 disable the **double-quoted-string-literal misfeature** (SQLite's legacy
"a bare double-quoted token that fails to resolve as an identifier is silently
re-interpreted as a string literal" footgun) for both DDL and DML. Leaving it on
is inconsistent with a fail-closed hardening profile: an emitter typo in a
double-quoted identifier would be silently misread as a string rather than
erroring. Both `SQLITE_DBCONFIG_DQS_DDL` (config.rs:48) and
`SQLITE_DBCONFIG_DQS_DML` (config.rs:46) exist in rusqlite 0.39.

Real `rusqlite` symbols used:

- **`Connection::authorizer(Some(F))`** (verified, hooks/mod.rs:447) where
  `F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static`.
  The callback returns `rusqlite::hooks::Authorization::{Allow, Ignore, Deny}`.
  `AuthContext` carries the `action: AuthAction` (an enum over the action codes),
  the `database_name: Option<&str>` (the ATTACH alias — how we match `_mig` vs
  `app`; **this is the OUTER context field**, not a per-action field — see §2.2.1),
  and the innermost trigger/view `accessor: Option<&str>` (`None` for top-level
  SQL; hooks/mod.rs:54,58). **It fires at `prepare` time** (see the §2.2.2
  caveat), which is exactly when we want to reject dangerous DDL — before it can
  run.
- **`Connection::load_extension_disable()`** (verified, lib.rs:877) for disabling
  extension loading. There is **no** `DbConfig::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION`
  variant in rusqlite 0.39 — it is commented out at config.rs:25 and `DbConfig` is
  `#[non_exhaustive]`. The first draft's "verified against `rusqlite::Connection`"
  claim for that *named symbol* was wrong and is retracted; the mechanism
  (disabling the C API + the `load_extension()` SQL function on bundled SQLite
  3.51.3) is correct, only the API name was.
- **`Connection::set_db_config(DbConfig::…, bool)`** (verified) for
  `SQLITE_DBCONFIG_DEFENSIVE` (config.rs:38 → `true`),
  `SQLITE_DBCONFIG_TRUSTED_SCHEMA` (config.rs:56 → `false`),
  `SQLITE_DBCONFIG_DQS_DDL` (config.rs:48 → `false`), and
  `SQLITE_DBCONFIG_DQS_DML` (config.rs:46 → `false`). These four variants all
  exist; `…ENABLE_LOAD_EXTENSION` does not (above).

**Authorizer deny matrix — two modes (the §2.2.2 toggle):**

| Action code | `CreatorUp` mode | `EngineJournal` mode |
| --- | --- | --- |
| `SQLITE_ATTACH` (24) | **Deny** | **Deny** |
| `SQLITE_DETACH` (25) | **Deny** | **Deny** |
| `SQLITE_PRAGMA` (19) | **Deny** | Allow only `foreign_keys` toggle (rebuild, §2.4); deny all else |
| `SQLITE_CREATE_VTABLE` (29) | **Deny** (creator cannot make vtables) | Allow only `fts5`/`vec0` for engine-emitted goodie DDL |
| `SQLITE_CREATE_MODULE` | **Deny** | **Deny** (no new modules at runtime) |
| `SQLITE_FUNCTION` (31) | **Allow only an explicit allowlist** (see below) | same allowlist + `vec_*` for engine vector DDL |
| `SQLITE_INSERT/UPDATE/DELETE/DROP_TABLE/DROP_TRIGGER/ALTER_TABLE` when `ctx.database_name == Some("_mig")` | **Deny** (journal immutable, §2.2.1) | **Allow** (only the journal tables) |
| `SQLITE_CREATE_TRIGGER`/`SQLITE_CREATE_VIEW` whose `ctx.database_name == Some("_mig")` (body targets `_mig`) | **Deny** (creator-trigger journal-write vector, §2.2.1 item 6) | **Allow** (engine-emitted) |
| `SQLITE_CREATE_TABLE/INDEX/TRIGGER`, `SQLITE_INSERT/UPDATE/DELETE` when `ctx.database_name == Some("app")` | **Allow** | **Allow** |
| `SQLITE_TRANSACTION` (22) | **Deny** (engine owns BEGIN/COMMIT) | **Deny** |
| `SQLITE_SAVEPOINT` (32) | **Deny** | **Deny** |
| writes when `ctx.database_name` is anything other than `Some("app")`/`Some("_mig")` | **Deny** | **Deny** |
| everything else (SELECT/READ on `app`) | Allow | Allow |

**`SQLITE_FUNCTION` allowlist (C2).** The critic's C2 is correct: a blanket
allow on `SQLITE_FUNCTION` cannot distinguish benign functions from
extension/`vec0` functions, and vtable modules issue internal SQL. So the
callback **allowlists function names explicitly** — the deterministic built-ins
the descriptor-generated DDL can legitimately reference for defaults/CHECK
(`CURRENT_TIMESTAMP` is a keyword not a function; allow e.g. `abs`, `length`,
`coalesce`, `lower`, `upper` as needed by emitted defaults) and **denies
everything else by name**, including `load_extension`, `fts3_tokenizer`, and any
`vec_*` in creator mode. Unknown function ⇒ **Deny** (fail-closed). The
engine-generated DDL uses a small, known function set, so the allowlist is
closed and auditable. This makes the authorizer a true line-2 even for
runtime-constructed SQL: the deny is at prepare, before execution, for **every**
statement compiled on the connection.

**Honest limit on vtable-internal SQL (C2).** FTS5/`vec0` modules execute
internal SQL against their shadow tables. The authorizer **does** fire for that
internal SQL (it is prepared through the same connection), but to avoid
mis-classifying legitimate shadow-table access we (a) only ever create such
vtables in **engine mode** for engine-emitted goodie DDL, never in creator mode,
and (b) rely on `DEFENSIVE=ON` to block direct writes to shadow tables from
creator SQL. We do **not** claim the authorizer perfectly models every internal
statement a third-party module might issue; we claim that **creator SQL cannot
create or invoke such a module at all** (CREATE_VTABLE/CREATE_MODULE/vec_*
denied in creator mode), so the module-internal surface is never reachable from
untrusted input. Fail-closed.

#### 2.5.2 Per-app ATTACH isolation — closed by construction (C3)

Tenant files are predictably named and co-located: `zs-<app_id>.sqlite` in a
shared `db_dir` (plugin-db `ensure_app_schema` ATTACHes
`zs-{app_id}.sqlite`, `backend/sqlite/mod.rs:660-671`). With **`SQLITE_ATTACH`
denied** by the authorizer, the previously
open cross-tenant attack (`ATTACH DATABASE 'file:zs-OTHER_APP.sqlite' AS victim`)
is closed. The mechanism, precisely:

1. The engine opens the migration connection and, **before installing the
   authorizer**, ATTACHes exactly two files: the one app's `zs-<app_id>.sqlite`
   AS `app`, and the journal AS `_mig` (§2.5.1 step 0).
2. The authorizer is installed (§2.5.1 step 7, last) with `SQLITE_ATTACH` and
   `SQLITE_DETACH` **denied in both modes** for the rest of the connection's life.
3. Every creator `up` statement is prepared under this authorizer, so a
   creator-issued `ATTACH`/`DETACH` is rejected at prepare time.

**Proof that cross-tenant is closed by construction.** For a creator `up` to read
or write another tenant's data it must name a database alias bound to another
tenant's file. The only aliases bound on this connection are `app` (this tenant)
and `_mig` (this tenant's journal). New aliases can only be bound via
`SQLITE_ATTACH`, which is denied at prepare for every creator statement. The
authorizer also **denies writes whenever `AuthContext.database_name` is anything
other than `Some("app")`/`Some("_mig")`** (matching on the outer context field —
the attach alias SQLite passes as the `xAuth` `zDb` argument, hooks/mod.rs:54 —
not a per-action field) as a belt-and-suspenders rule. Therefore no creator statement that names a foreign
file can compile, and none that writes outside `app` can execute. The two ATTACHes
in step 1 are engine-issued **before** the authorizer exists and name only this
tenant's own files (the engine constructs the path from the authenticated
`app_id`, never from creator input). Cross-tenant access is impossible under the
hardened model — by construction, not by convention.

> **Phasing consequence (the inversion the critic flagged).** This guarantee only
> holds once the authorizer is installed. Therefore **no creator/AI DDL may run on
> SQLite before confinement exists.** §3 is re-ordered so the hardened connection +
> authorizer (now P2's confinement, folded in) lands **with** the first apply, not
> a phase later.

#### 2.5.3 Line-1 (the guard): no SQLite parity — a different, enumerated defense (H2)

**State it plainly: `libpg_query` cannot parse SQLite and will NOT be reused.**
`guard.rs`/`classify.rs`/`analyze.rs` are 100% the Postgres grammar
(guard.rs ~2600 lines of libpg_query); it mis-parses SQLite syntax
(`AUTOINCREMENT`, `WITHOUT ROWID`, `STRICT`, affinity type names,
`CREATE VIRTUAL TABLE … USING fts5`). There is **no SQLite line-1 parity**, and we
do not claim one. Instead:

1. **Primary model — descriptor-diff-generated DDL ONLY (no untrusted raw SQL).**
   On the Confined SQLite path, migrations are **generated by the engine's
   declarative author from a validated descriptor** — the exact trust model
   `register_model` already uses (it never parses creator SQL; it diffs a
   validated descriptor and emits DDL). Descriptor validation
   (`validate_ident`/`validate_type`) replaces the parser's job: there is **no
   untrusted SQL string** for a parser to vet, so the absence of a SQLite parser
   is a non-issue on this path. This is the primary, recommended model.

2. **AI / raw SQLite SQL (incl. the 12-step rebuild) — authorizer-on-`prepare_v2`,
   limits acknowledged.** The `MigrationAuthor` seam produces raw SQL for cases
   (renames, backfills, rebuilds) that descriptor-diff cannot express. For these:
   - **Validation = `sqlite3_prepare_v2` with the hardened authorizer attached**,
     prepare-only (no `step`), against the live connection's schema. The real
     SQLite parser + the deny matrix (§2.5.1) vet every statement at compile time.
     This catches `ATTACH`/`PRAGMA`/`load_extension`/foreign-file writes/`_mig`
     writes — the SQLite RCE-adjacent surface — **fail-closed**.
   - **Acknowledged limits (no line-1 parity claim):** the authorizer fires at
     prepare, so (a) it cannot inspect values bound at step time (irrelevant for
     DDL, which has no binds), and (b) it classifies by action code, not by deep
     semantic intent the way libpg_query's deny-list does for PG (e.g. it does not
     distinguish a "safe" `CREATE INDEX` from a slow one). We therefore claim a
     **different** defense than PG's line-1: *enumerated capability denial at the
     real parser*, not *semantic classification*. Because the dangerous
     capabilities (ATTACH/PRAGMA/extension/foreign-write/journal-write) are denied
     by enumerated action code, the AI/raw path cannot escalate beyond
     "DDL/DML against this tenant's `app` schema" — which is the same blast radius
     as the descriptor path. AI-authored migrations additionally pass through the
     existing `approval.rs` gate (human/plan approval) before apply, which the PG
     path also requires for `MigrationAuthor` output.

**`TrustProfile` mapping (L1).** A SQLite `GuardConfig` consumer is net-new
(today `TrustProfile`/`SchemaScope`, guard.rs:56/84, are PG-coupled). The mapping:
- **Confined** SQLite = hardened connection + authorizer (always on) +
  descriptor-diff DDL as primary; raw path only via `MigrationAuthor` + approval +
  authorizer-on-prepare.
- **Trusted** SQLite = operator-owned local file (CLI/dbmate parity); the
  authorizer is **relaxed** (ATTACH/PRAGMA allowed) because the operator is the
  trust boundary. The append-only trigger (§2.2.1 item 5) remains the journal
  backstop here.
- **Platform** is a PG-only concept → **fail-closed to Confined on SQLite**.

The runtime authorizer (in its profile-appropriate mode) is **always installed**
regardless of profile; only its deny matrix relaxes on Trusted.

### 2.6 Goodies on SQLite

- **Encryption (BYTEA→BLOB):** full parity (bind sentinel already implemented;
  AEAD transform stays in plugin-db).
- **Mask:** full DDL parity (inline sentinel already dialected); transform is
  data-plane.
- **Vector:** `sqlite-vec` cosine+L2 only → `InnerProduct` →
  `vector_unsupported_metric` (documented). `vec0` virtual table vs PG
  `ivfflat/hnsw`. **Security note (C1/H4):** a `vec0` `CREATE VIRTUAL TABLE … USING
  vec0` migration is emitted by the engine **under `EngineJournal` (engine) mode**,
  which is the only mode that allows `SQLITE_CREATE_VTABLE` for `vec0` and the
  `vec_*` functions (§2.5.1). Creator-mode DDL can never create or call `vec0`.
  This is how `vec0` is "made available only where a vector-index migration
  legitimately needs it" without leaving extension/vtable capability on for
  creator DDL. (The `vec0` *module* may be process-globally registered via the
  data-plane's `sqlite3_auto_extension`; on the migration connection it is inert
  to creator SQL because every `vec0`-touching action is denied in creator mode.)
- **geoPoint:** no PostGIS → packed `(lat,lng)` BLOB, **no spatial index**,
  haversine flat-scan (dev-scale).
- **FTS:** PG `tsvector`+GIN → SQLite **FTS5 virtual table** + sync triggers
  (`ensure_fts_index` already does this). The `GENERATED … STORED` emitter that is
  PG-only today lives in `declarative.rs`, which is slated for **replacement** by
  relocated `zeroship-schema` emission (§2.8) — so the FTS5 branch is added in the
  **relocated** emitter, not bolted onto soon-to-be-deleted `declarative.rs` code.
- **FK:** full parity, with `PRAGMA foreign_keys=ON` set at connection open
  (allowlisted there, not via migration SQL).

<!-- Rewritten in round 2: M1 — separate dev-scale degradations from security properties -->
#### 2.6.1 Two kinds of "divergence" — do NOT conflate them (M1)

The first draft
laundered security regressions under "documented divergence." They are different
classes and must be tracked differently:

**(a) Legitimate dev-scale functional degradations — divergence is acceptable,
documented in `docs/reference/sqlite-divergences.md`:**
- Vector: no `InnerProduct` (sqlite-vec is cosine+L2 only); `vec0` flat/brute
  vs PG `ivfflat/hnsw`.
- FTS5: ignores language/stemming config that PG `tsvector` honors.
- geoPoint: no spatial index; haversine flat-scan.
These are dev-tier scale/quality trade-offs. They do not weaken any security
property; they are correctly "documented divergences."

**(b) Security properties — these MUST be at PARITY, never divergent:**
- **Journal integrity** (immutable by construction): parity via §2.2.1
  (DEFENSIVE + trusted_schema=OFF + authorizer deny on `_mig` + trigger backstop).
- **Cross-tenant isolation**: parity via §2.5.2 (ATTACH denied; closed by
  construction).
- **Line-2 runtime confinement**: parity via §2.5.1 (authorizer as the migrator-
  role analog), with the line-1 difference honestly enumerated (§2.5.3), not
  hand-waved as "documented divergence."
If any of (b) cannot be met, the SQLite path does **not** ship that capability —
it is a blocker, not a footnote.

Net for goodies: encryption/mask/FK = full; vector/geoPoint/FTS = the *same*
class-(a) documented degradations that already exist in plugin-db's runtime.
Nothing new is lost, and **no security property is downgraded**.

### 2.7 Rollback / drift / expand-contract

- **`.down.sql`:** works (transactional DDL); appends `rolled_back`. Many
  reversals need the 12-step rebuild.
- **Drift:** SQLite `snapshot_schema` over `sqlite_master.sql` (incl. inline
  sentinels) + `PRAGMA table_info`/`index_list`/`foreign_key_list`. Checksum
  drift (tamper check) is dialect-agnostic, reused as-is.
- **Expand-contract:** orchestration is DB-agnostic; emitted DDL must be
  SQLite-dialected; dual-write via SQLite triggers; column-rewrite contract via
  the 12-step rebuild (P3b precondition, §2.4).

<!-- Added in round 2: H3 — ordering dependency vs the schema-authority relocation -->
### 2.8 Ordering dependency: the `zeroship-schema` relocation lands FIRST (H3)

`declarative.rs` is **PG-only** (`quote_ident` PG double-quoting :53;
`GENERATED ALWAYS AS (…) STORED` :72; a deliberately-duplicated DSL→PG type table
:29-36). The 2026-06-18 schema-authority design (§5, line 138) marks
`declarative.rs` as **REPLACE** — its v1 SUBSET differ is to be deleted and
replaced by the relocated full `zeroship-schema` emission (the shared
`build_*_for_dialect` path that is already SQLite-capable, §1.1).

**Explicit ordering to avoid double work:** P4 must NOT dialect-ize
`declarative.rs` in place, because that code is slated for deletion. Instead:

1. The **`zeroship-schema` relocation (2026-06-18 §5/P-relocation) lands first**,
   deleting `declarative.rs`'s SUBSET emitter and routing the engine's
   declarative author through `zeroship_schema::build_create_table_with_fks_for_dialect`
   / `build_create_indexes` (which already branch on `SqlDialect::Sqlite`).
2. **Then** the SQLite engine work (this doc's P4) routes the Confined
   descriptor-diff path through that *relocated* emitter with `dialect=Sqlite`.

If the relocation has not landed when SQLite work begins, P4 is **blocked on it**
(a hard sequencing dependency, recorded in §3). We do not dialect-ize
soon-to-be-deleted code.

<!-- Added in round 2: M5/L1/L2 — version assumption, identifier quoting, GuardConfig consumer -->
### 2.9 Engine/library assumptions and identifier hazards (M5, L1, L2)

- **Bundled SQLite version (M5).** Window functions (`ROW_NUMBER`, §2.2) need
  SQLite ≥ 3.25; `RETURNING` (event-seq allocation, §2.2) needs ≥ 3.35;
  `DEFENSIVE`/`TRUSTED_SCHEMA` dbconfig need ≥ 3.31/3.36. The engine links
  **rusqlite's `bundled` feature (SQLite 3.51.3)**, which satisfies all of these —
  this is the supported configuration and is asserted at build time. **The
  Trusted/CLI path may link a *system* SQLite of unknown version.** The engine
  therefore checks `sqlite3_libversion_number()` at connection open and **refuses
  to run** (typed `UnsupportedSqliteVersion` error) if the linked library is below
  the floor required by the features in use. The assumption is no longer silent.

  **The floor is also tied to authorizer-argument semantics, not just feature
  presence (round-2 precision).** The journal-immutability proof (§2.2.1) depends
  on SQLite passing the attach alias (`zDb`) as the 5th `xAuth` argument for
  `SQLITE_DROP_TABLE`/`SQLITE_DROP_TRIGGER`/DML — i.e. it depends on
  `AuthContext.database_name == Some("_mig")` being populated for those actions.
  This is **asserted (verified in bundled 3.51.3:** DROP_TABLE at sqlite3.c:127925,
  DROP_TRIGGER at sqlite3.c:156743, DML sites), not assumed, on the Trusted/system-
  SQLite path: the version check at connection open additionally requires a SQLite
  whose authorizer passes `zDb` on DROP_TABLE (true for all supported versions; the
  floor is enforced so the deny-on-`_mig` rule cannot silently no-op against an
  exotic build that omits it).
- **`GuardConfig` SQLite consumer (L1).** `TrustProfile`/`SchemaScope`
  (guard.rs:56/84) are PG-coupled with no SQLite consumer today. §2.5.3's mapping
  is net-new code: a `GuardConfig` arm that, for `dialect=Sqlite`, selects the
  authorizer deny matrix and the descriptor-diff-only posture, and fail-closes
  Platform→Confined. This is called out as an implementation item, not assumed to
  exist.
- **`_mig` alias + trigger identifier quoting (L2).** PG already hit the 63-byte
  NAMEDATALEN trigger-name bug (journal.rs:301-312) and fixed it by using **short,
  table-local trigger names** that do not embed the hyphenated-UUID schema name.
  The SQLite analog: SQLite identifiers have no hard 63-byte limit, but the same
  *discipline* applies for clarity and to avoid quoting hazards — the journal
  alias is the fixed literal `"_mig"` (not `"<app_id>_mig"`), and the immutability
  trigger names are short table-local literals (`zs_immutable_trg`), double-quoted
  as identifiers. The app-id (a hyphenated UUID) appears only in the **file path**
  (`zs-<app_id>.sqlite`), never as a SQL identifier, so there is no
  unquoted-hyphen parse hazard. The `_mig` alias and trigger names are all
  ASCII-safe fixed strings.

---

## 3. Phasing (each builds + tests against a temp SQLite file; no PG required)

<!-- Re-ordered in round 2: confinement folded INTO P2 — no untrusted DDL runs before the authorizer exists -->
**Ordering invariant (the inversion the critic flagged):** **no creator/AI DDL —
indeed, no non-engine-generated SQL — runs on SQLite before the hardened
connection + authorizer are installed.** Confinement is therefore **part of P2**,
not a later phase. The first draft's P2-before-P3 inversion (apply migrations,
then add confinement a phase later) is eliminated.

**Hard external dependency:** the `zeroship-schema` relocation (2026-06-18 §5,
which marks `declarative.rs` REPLACE) must land **before** P4 (§2.8). If it has
not, P4 is blocked; P1–P3b can proceed (they do not touch declarative emission).

- **P1 — Introduce `MigrationBackend`; PG is the first impl (no behavior
  change).** The seam spans **execution I/O, parse-time non-txn validation
  (replacing the inline `pg_query::parse` at executor.rs:490/2148), drift
  snapshot, and journal row I/O** (§2.0/§2.1 — larger than "leaf I/O", M3).
  `PostgresBackend` is byte-identical. **Regression bar:** the *entire* existing PG
  suite green, including the non-txn idempotency and drift paths now routed
  through the trait (not just connection I/O). Ships alone (riskiest to regress).
- **P2 — SQLite executor WITH confinement built in (journal + hardened connection
  + authorizer + transactional apply of one additive migration).** The dedicated
  hardened CDC-free migration actor (§2.1.1); hardened connection (§2.5.1:
  load-extension off, DEFENSIVE on, trusted_schema off, authorizer installed
  before any creator SQL); `_mig` attached once and kept attached; journal with
  the **shared monotonic `event_seq` table** (§2.2, M4) + append-only triggers +
  window-function net-state; the atomic single-connection phase sequence (§2.2.2);
  in-process lock only (§2.3, honest); reject `transaction:false` at the dialect
  boundary (§2.3, L3). **Gate:** apply one engine-generated `CREATE TABLE` to a
  temp file; re-run no-op; journal `completed`; **and** the confinement suite —
  `ATTACH` denied, `PRAGMA writable_schema=ON` denied, `DROP TABLE "_mig".…`
  denied, `load_extension` denied, cross-file write denied, journal UPDATE/DELETE
  rejected by trigger AND authorizer. *Smallest phase that proves the seam
  end-to-end on SQLite **safely**.*
- **P3 — Per-app ATTACH isolation hardening + cross-tenant proof tests (§2.5.2).**
  Strengthen and prove: engine attaches only this tenant's `app`+`_mig` before the
  authorizer; cross-tenant `ATTACH 'file:zs-OTHER.sqlite'` denied; writes outside
  `app`/`_mig` denied. **Gate:** a red-team test that a creator `up` cannot reach
  another app's file by any ATTACH/alias path.
- **P3b — 12-step table rebuild emitter (net-new in `zeroship-schema`) — a
  PRECONDITION (§2.4, M2).** Engine-mode-authorized rebuild (foreign_keys toggle,
  copy, foreign_key_check). **Gate:** a column type-rewrite round-trips on a temp
  file with FK integrity preserved. *Must precede P5/P6, which depend on it.*
- **P4 — Guard posture + descriptor-only Confined path** *(blocked on the
  `zeroship-schema` relocation, §2.8)*. `GuardConfig` SQLite consumer (§2.5.3/L1):
  Confined = descriptor-diff DDL + authorizer; Platform → fail-closed Confined;
  authorizer-on-`prepare_v2` for the raw/AI path (no libpg_query, limits
  acknowledged §2.5.3); route declarative emission through the **relocated**
  `build_*_for_dialect(Sqlite)`. **Gate:** descriptor → SQLite DDL golden-file
  parity; raw dangerous SQLite (ATTACH/PRAGMA/load_extension/`_mig`-write) denied.
- **P5 — Drift + rollback + goodies (encryption/mask/FK, then vector/FTS/
  geoPoint).** SQLite `snapshot_schema`; `.down.sql` (reversals via the P3b
  rebuild); emit each goodie on a temp file with the **class-(a)** documented
  degradations (§2.6.1). **Gate:** round-trip snapshot, rollback re-pending, each
  goodie applies; no security property degraded (class-(b) parity held).
- **P5b — Cross-process serialization (§2.3, H1).** Lock row + `BEGIN IMMEDIATE`
  + busy_timeout + WAL stale-read discipline; `SQLITE_BUSY`→`LockContended`.
  **Gate:** two processes contending on one file serialize correctly; **only after
  P5b may SQLite run in a multi-process deployment.** Concurrency parity with
  `pg_advisory_lock` is claimed **only** from here.
- **P6 — Expand-contract + retire the registerModel SQLite split.** SQLite
  column-rewrite via the P3b rebuild; dual-write triggers; delete
  `run_sqlite_pipeline`/`apply_sqlite`; route SQLite registerModel/deploy through
  the engine. **Gate:** online split/merge on SQLite; dev-tier app deploys schema
  via the engine with no plugin-db auto-migrate.

**Deferred (explicit):** FTS5 language parity and inner-product vector on SQLite
(class-(a) documented divergences, §2.6.1 — *not* security). Cross-process locking
is **not** "deferred indefinitely" — it is the named phase P5b and gates
multi-process SQLite. No security property (journal integrity, cross-tenant
isolation, line-2) is deferred; all are in P2/P3.

---

## 4. Resolved questions (formerly open; closed by the round-2 revision)

The first draft's six open questions were the load-bearing gaps. Each is now
resolved in-document:

1. **Raw vs descriptor-diff DDL on the Confined path** → **Resolved (§2.5.3):**
   descriptor-diff-generated DDL is the **primary** model (no untrusted SQL
   string). Raw/AI SQL (incl. the 12-step rebuild) is allowed **only** through the
   `MigrationAuthor` seam + `approval.rs` gate + authorizer-on-`prepare_v2`, with
   the line-1 difference honestly enumerated (we claim enumerated capability
   denial, not semantic parity).
2. **Authorizer ≡ migrator role for runtime-constructed SQL** → **Resolved
   (§2.5.1):** the authorizer fires at `prepare` for **every** statement compiled
   on the connection (including runtime-constructed SQL), with an explicit
   `SQLITE_FUNCTION` allowlist (fail-closed on unknown functions) and
   CREATE_VTABLE/CREATE_MODULE denied in creator mode, so the vtable-internal-SQL
   escape (C2) is unreachable from creator input.
3. **Cross-process serialization under WAL** → **Resolved as scope (§2.3, P5b):**
   honest that today's lock is in-process-only; cross-process is the named phase
   **P5b** with the lock-row + `BEGIN IMMEDIATE` + WAL stale-read reasoning spelled
   out; **no concurrency parity claimed until P5b**.
4. **(Sharpest tension) atomic DDL+journal vs confining `up` from `_mig`** →
   **Resolved (§2.2.2):** `_mig` stays **attached throughout**; confinement is **by
   authorizer state** (creator-mode → engine-mode), preserving single-connection
   single-transaction atomicity. The detach/re-attach idea is rejected.
5. **`MigrationBackend` extraction surface** → **Resolved (§2.0/§2.1, M3):** the
   seam explicitly abstracts parse-time non-txn validation, drift snapshot, and
   journal row shapes (dialect-neutral row constructors so `event_seq`/`TIMESTAMPTZ`
   assumptions don't leak). P1's regression bar is restated to cover these paths.
6. **Descriptor-only vs AI-authored complex SQLite migrations** → **Resolved
   (§2.4 + §2.5.3):** the 12-step rebuild is engine-emitted (P3b precondition) for
   the descriptor path; where AI authors raw rebuild SQL it flows through the
   `MigrationAuthor` + approval + authorizer-on-prepare path of #1.

**Remaining genuine risk to flag for implementation (not a design hole):** the
`SQLITE_FUNCTION` allowlist must be kept in lockstep with the function set the
relocated `zeroship-schema` emitter can produce in defaults/CHECK expressions —
an emitter change that introduces a new function must update the allowlist or the
migration will be denied (fail-closed, which is the safe failure direction).
