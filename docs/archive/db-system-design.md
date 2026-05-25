# plugin-db — System Design

**Status**: design, not implemented. Foundational doc for the whole DB subsystem.
**Supersedes**: `docs/proposals/sqlite-backend-dev-tier.md`.
**Closes** (from `docs/reviews/plugin-db-deferred.md`): [C1] (Backend trait
half-applied, §7); [I20] (WAL replication cross-tenant isolation, §17 +
§19 P6); [I31]/F1 (§10 + §18); [I32]/F2 (§10 + §18).
**Last updated**: 2026-05-24 (system fields + SQLite vector backend swapped to sqlite-vec).

<!-- 2026-05-24 amendment: platform system fields -->
**Changelog — 2026-05-24 amendment (platform system fields).** Every
collection now auto-receives a fixed set of platform-managed fields:
`id`, `created_at`, `updated_at`, `created_by`, `updated_by`,
`version`, `deleted_at`. Creators don't declare them; reserved names
enforced by `validate_field_name`. Naming convention is Salesforce-
style naked (no `__zs_*` prefix) — matches the existing `id` field
and the way LLM-generated app code reaches for these names. Drives:
audit trail, optimistic concurrency, soft-delete (the new `delete()`
default; `purge()` is hard-delete), CDC subscriber idempotency, and
P5+ AEAD AAD binding (`version` folds into the AAD; defends against
ciphertext rollback within the same row). The retired
`.softDelete()` / `.withVersioning()` schema modifiers are
**superseded** — these behaviours are now universal platform features.
§6.1 and §15 carry the cross-references to the full proposal:
`docs/proposals/platform-system-fields.md`.

<!-- 2026-05-24 amendment: SQLite vector backend swapped to sqlite-vec -->
**Changelog — 2026-05-24 amendment (P4 PR 7).** SQLite vector storage
and search now route through the `sqlite-vec` extension (statically
compiled via the `sqlite-vec` Rust crate; `sqlite3_auto_extension`
hook registered once per process at `SqliteSession::open`). The
earlier "pure-Rust flat scan" decision documented in the prior P4
amendment block (2026-05-23 below) is **superseded**. Preserves the
bundled-SQLite invariant §1 — the C source is compiled into the
binary, no `.so` ships, no amalgamation fork. vec0 gives SIMD
distance, native dimension validation, and `MATCH` query syntax; net
code surface is smaller than the pure-Rust impl. Open question §18
Q7 was re-opened then closed the same day with the swap landed. See
`docs/proposals/p4-search-implementation-plan.md` §10 (2026-05-24
reassessment block) for the rationale-correction trail. Mechanically:
§4 row 7 SQLite cell now reads `vec0 virtual table via sqlite-vec
extension (statically compiled; SIMD-accelerated)`; §6.2 row
`Vector(n)` SQLite cell now reads `vec0 (sqlite-vec extension;
statically compiled)`; §7 vector-trait narrative switches to "vec0
virtual table" wording.

<!-- preupdate_hook amendment -->
**Changelog — 2026-05-22 amendment.** Swapped trigger+outbox SQLite
CDC for `sqlite3_preupdate_hook` (rusqlite `preupdate_hook` feature
flag, `bundled` SQLite compiled with `SQLITE_ENABLE_PREUPDATE_HOOK`).
Strict simplification: §6.5 drops the `__zeroship_pre_image` system
table; §11.5 simplifies substantively (no triggers, no outbox, no
`_seq` allocator, no AUTOINCREMENT-rowid invariant, no outbox GC);
§13.5 MV/CDC storm concern partially evaporates (filter is now a
Rust callback predicate, not a DDL allow-list); §17.7 drop-namespace
loses its DDL teardown step; §19 P2 loses two implementation steps.
All round 1–9 review-loop findings about the outbox approach
collapse to "use the native hook." Prior-round narrative remains in
`docs/reviews/db-system-design-critique-round-*.md`.

<!-- p4-pure-rust-vector amendment -->
**Changelog — 2026-05-23 amendment (P4 closure).** SQLite vector
storage is **pure-Rust flat scan**, not `sqlite-vec`. Decision
source: `docs/proposals/p4-search-implementation-plan.md` §10
(riskiest decision Q-P4-D). Reason: bundling `sqlite-vec` either
forks the SQLite amalgamation per-platform (doubles the CI matrix)
or loads a runtime `.so` (defeats the `bundled` feature's "no
system libsqlite3" promise from §1 / line 56-60 of this doc).
Pure-Rust flat scan keeps the bundled-SQLite invariant intact at the
cost of a dev-tier-only scale ceiling (~50k rows, ≤1024 dims,
≤100ms latency). Production vector workloads use pgvector on PG —
the §1 stance that "SQLite = dev/sandbox/test ONLY" already
encodes this. Mechanically: §4 row 7 now reads "pure-Rust flat scan
(dev scale ≤50k rows); HNSW deferred"; §6.2 row `Vector(n)` SQLite
cell now reads `BLOB` (pure-Rust flat scan); §7 vector-trait
narrative drops the "wraps sqlite-vec" wording; §18 open question
#7 is closed (no `sqlite-vec` runtime extension load — pure-Rust
the chosen path). HNSW / `sqlite-vec` is deferred to a future
deferred-list entry, not killed; if/when a future revision restores
it, it ships behind a separate Cargo feature so the dev-tier
default stays bundled.

**Reading order.** §1 overview · §4 25-capability PG/SQLite split · §7
capability trait redesign · §8 SQLite mechanism · §19 engineering
sequencing.

---

## 1. Overview & system context

`plugin-db` is a single Rust crate (`crates/plugin-db/`) that registers
the `env.db.*` namespace on every worker V8 isolate, marshals JS
calls into typed Rust operations, and executes them against a storage
backend. Owns: declarative-migration orchestration, CRUD execution,
transactions, multi-tenant schema isolation, change-data capture,
reactive-query broker, SECURITY DEFINER auth (`hardening` feature),
per-row data audit log. User-facing surface: the `@zeroship/db` SDK, a
thin TypeScript layer over `env.db.*`. SQL never reaches creator
code.

**Postgres = production backend.** Full feature set, real concurrency,
online DDL, continuous PITR, SECURITY DEFINER auth.

**SQLite = dev/sandbox/test ONLY.** Local dev, test suite, CI without
Docker, AI-builder preview. Every PG feature has a SQLite workaround
acceptable at dev scale; for **creator-invoked** operations the SDK
consumer cannot tell which backend they're on. One narrow exception:
admin-surface operations the platform invokes (not creator code) —
currently PITR — return a typed `pitr_pg_only` envelope on SQLite; the
dashboard hides the control in dev so this surfaces only to operator
tooling. Inventory in §15.7 and §16.

<!-- preupdate_hook amendment -->
**Native SQLite (rusqlite), not Turso.** `rusqlite` with the
`bundled` + `preupdate_hook` Cargo features — the `bundled` feature
ships the SQLite amalgamation compiled with
`SQLITE_ENABLE_PREUPDATE_HOOK`, no system `libsqlite3` at any
deployment. CDC is the native `sqlite3_preupdate_hook` callback
(§11.5). Async via `compio::runtime::spawn_blocking`.

---

## 2. Goals & non-goals

**Goals** (all 25 ship at day-1 launch on both backends): declarative
schema with idempotent `registerModel`; four-phase migration pipeline
(Bootstrap, Plan, Validate, Apply — see §10) with a separate B1 backfill
orchestrator (§10.6) layered on top; rich CRUD; transactions with savepoints + isolation
levels; pessimistic locking + optimistic concurrency; reactive queries via
CDC → broker with server-side filter narrowing; vector / full-text /
geospatial search; JSON columns + path queries + arrays; time-series,
recursive queries, window functions, materialised views; soft delete +
restore and per-row data audit log; bulk import/export; counters and
outbox/webhook fanout; encrypted columns (PII); multi-tenancy (cross-app
schema isolation + within-app scoping); app-level RLS; per-app metering +
quotas; backups / PITR / snapshots; multi-region replicas (PG only); typed
errors with stable `.code` across backends; schema-per-app isolation;
capability-trait composition for future backends; compile-time backend
selection via Cargo features + runtime URL prefix; zero-tokio adherence;
WPT-grade typed error rail; full observability stack.

**Non-goals.** Multi-master replication; federation across heterogeneous
DBs; general query-language design (the SDK filter object IS the surface);
wire-protocol compat with psql/mongosh; distributed transactions across
apps; in-process JOINs across backends.

---

## 3. Personas → feature drivers

Personas tie scenarios to capabilities. **Creator** (natural-language
build) → dashboard view of schema, migration preview, metrics, backups
(§16). **App developer** (AI or human reading generated handlers) → SDK
shape (§15) and stable typed-error rail (§15.7). **End user** drives
nothing directly; perf and isolation invariants exist to protect their
request path. **Platform operator** → observability (§16), per-app
quotas (§13), multi-tenant boundary (§17), backup/restore runbooks
(§16).

Scenarios driving the capability set: social/CMS/forum (reactive + FTS
+ counters + soft delete); e-commerce/booking (tx + pessimistic locks +
window fns + geo); SaaS B2B/CRM (within-app multi-tenancy + audit +
metering + relations); real-time collab/games (server-side-filter
subscriptions + optimistic concurrency); analytics (MV); AI/RAG (vector
+ FTS hybrid + bulk import); forms (JSON + encryption).

---

## 4. Feature surface — 25 cross-cutting capabilities

| # | Capability | PG | SQLite |
|---|---|---|---|
| 1 | Reactive queries | logical-decoding slot + pgoutput WAL consumer → broker (requires `REPLICA IDENTITY FULL` for pre-image; §9.1) | native `sqlite3_preupdate_hook` callback in-process; per-tx event buffer flushed on COMMIT (§11.5) <!-- preupdate_hook amendment --> |
| 2 | Rich CRUD | shared filter→SQL lowering; `RETURNING`; `ON CONFLICT DO UPDATE` | identical (SDK is dialect-neutral) |
| 3 | Transactions + savepoints + isolation | `BEGIN ISOLATION LEVEL`; SAVEPOINT/RELEASE/ROLLBACK TO | `BEGIN DEFERRED \| IMMEDIATE`; SAVEPOINT native; `EXCLUSIVE` not used (§8.5) |
| 4 | Pessimistic locking | `SELECT … FOR UPDATE` | `BEGIN IMMEDIATE` for the whole tx (coarser; §8.5) |
| 5 | Optimistic concurrency (CAS) | `UPDATE … WHERE id=? AND version=? RETURNING` | identical |
| 6 | Full-text search | `tsvector` + GIN | FTS5 vtable maintained by triggers (NOT a column type; §6.2) |
| 7 | Vector search | pgvector HNSW | vec0 virtual table via sqlite-vec extension (statically compiled; SIMD-accelerated) |
| 8 | Geospatial | PostGIS GiST | R-tree on bbox + Rust Haversine post-filter |
| 9 | JSON columns + path queries | JSONB + `->`/`->>` | TEXT (CHECK `json_valid`) + `json_extract` |
| 10 | Time-series range queries | B-tree + BRIN | B-tree only |
| 11 | Recursive queries | `WITH RECURSIVE` | `WITH RECURSIVE` |
| 12 | Window functions | native | native (≥3.25) |
| 13 | Materialised views | `MATERIALIZED VIEW` + `REFRESH CONCURRENTLY` | shadow table + Rust scheduler (per-min default) |
| 14 | Soft delete + restore | nullable `deletedAt` | identical |
| 15 | Audit log of data changes | orchestrator-emitted (post-RETURNING, pre-COMMIT) → `__zeroship_audit_<coll>` (§10.7) | identical |
| 16 | Bulk import/export | `COPY FROM STDIN` | row-by-row INSERT in a single tx (materially slower at large N; acceptable at dev scale) |
| 17 | Counters / denorm helpers | `UPDATE … SET col = col + ?` | identical |
| 18 | Outbox / webhooks / background tasks | backend-agnostic processor in `crates/control` consumes `ChangeEvent`s | identical |
| 19 | Encrypted columns | Rust-side `aes-gcm` at SDK adapter; pgcrypto deferred (§19 P6) | Rust-side `aes-gcm` at SDK adapter |
| 20 | Multi-tenancy WITHIN an app | app-layer `db.scoped({ org_id })`; PG-native RLS optional in hardening | app-layer only |
| 21 | App-level row-level security | app-layer + `CREATE POLICY` in hardening | app-layer only |
| 22 | Schema evolution / declarative migrations | `register_model` lock scope + `__zeroship_migrations` audit table; `CREATE INDEX CONCURRENTLY` | same orchestrator; atomic `CREATE INDEX` with brief writer block |
| 23 | Per-app metering + quotas | `MeteredSqlExecutor` decorator (§13) | identical |
| 24 | Backups / PITR / snapshots | continuous WAL archive + `pg_basebackup`; PITR | `VACUUM INTO`; no PITR (dashboard hides controls in dev) |
| 25 | Multi-region / replicas | PG-only | none (dev tier is "ship to PG") |

---

## 5. Architecture (layer-by-layer)

**SDK layer** (`sdks/db/`) — schema declaration, type generation, filter
language, query builder, subscription receiver, per-handler tx routing,
retry wrappers. Calls native primitives through `env.db.*`. Does NOT
own SQL.

**V8 plugin layer** (`crates/plugin-db/src/v8_classes/` + `v8_bridge.rs`) —
V8 classes `Db`, `Collection`, `Transaction`, `Subscription`, `Migration`,
`Migrations`, `Replication` with `v8::Weak` finalizers; V8↔Rust marshaling;
capability gate.

**Orchestrator** (`crates/plugin-db/src/{orchestrator,crud,exec}.rs`) —
CRUD dispatch, register_model four-phase pipeline, transaction lifecycle,
auto-tx envelope, migration backfill, RAII lock guards, pending-emit
queue/drain/clear.

**Capability traits** — fifteen focused traits the orchestrator composes
(detail in §7): `SqlExecutor`, `NamespaceManager`, `LockManager`,
`ChangeStream`, `SchemaIntrospect`, `IndexBuilder`, `DialectBuilder`,
`SessionMinter`, `Metering`, `VectorIndex`, `FullTextIndex`,
`SpatialIndex`, `MaterializedView`, `EncryptedColumn`, `Backup`. The RAII
guard returned by `LockManager::acquire` is named `LockGuard` (replaces
legacy `OrchestratorLockGuard` during P0).

### 5.5 Backend impls + dispatch
`PostgresBackend` (`backend/postgres.rs`) and `SqliteBackend` (new,
`backend/sqlite.rs`) implement every capability.

**Dispatch is static, not dynamic.** Capability traits carry associated
types (`SqlExecutor::Client`, `LockManager::Guard`,
`ChangeStream::ConsumerHandle`, `SchemaIntrospect::LiveSchema`), so
`Backend` cannot be a `dyn` trait object. Selection: enum
`BackendHandle = Postgres(Rc<PostgresBackend>) |
Sqlite(Rc<SqliteBackend>)`. Capability-bound code is generic (`fn
foo<B: SqlExecutor + LockManager>(b: &B, …)`); boundary code
(`IsolateDbContext`) matches on the enum. Variants `#[cfg]`-narrowed
by Cargo feature (`pg`, `sqlite`). NO `Rc<dyn Backend>` anywhere.

**Physical DB.** PG via `compio-postgres` (zero-tokio, io_uring);
SQLite via `rusqlite` (bundled) + `compio::runtime::spawn_blocking`.
PG uses two `compio-postgres` client kinds: extended-query for
CRUD+DDL, and a replication-sub-protocol client for the WAL consumer
(`replication=database`, `START_REPLICATION SLOT … LOGICAL …`,
pgoutput via `CopyBothResponse`). Driver-side support in
`crates/compio-postgres/`; §9.1.

<!-- preupdate_hook amendment -->
**CDC layer.** PG: WAL consumer streams pgoutput; pre-image arrives
in the frame **only under `REPLICA IDENTITY FULL`** (PG default
carries PK only), asserted by the Apply step (§9.1). SQLite:
`Connection::preupdate_hook` registered once per session; the hook
fires BEFORE every row mutation inside the writer tx with native
OLD + NEW row data; the writer-actor appends to a per-tx event
buffer; on COMMIT the buffer flushes to the broker; on ROLLBACK the
buffer is dropped (§11.5). Both feed the same `broker.rs`.

**Broker** (`broker.rs`). Subscription registry; server-side predicate
evaluation; bounded per-subscriber queue; `resync` event for
backpressure. Backend-agnostic.

**Auth subtree** (`auth/`, `hardening` Cargo feature): `bootstrap.rs`
(admin schema, platform role, SECURITY DEFINER wrappers), `keys.rs`
(HMAC rotation), `session.rs` (sign/verify). `SessionMinter` two
impls: PG SECURITY DEFINER, SQLite Rust HMAC.

---

## 6. Data model

### 6.1 Collections
Equivalent to a SQL table. Declared via the schema convention (§15).
Backed by `"<app_id>"."<collection>"`. PG: native schema per app.
SQLite: per-app file `${db_dir}/zs-${app_id}.sqlite` mounted via
`ATTACH DATABASE 'file:…' AS "<app_id>"`. URI carries NO
`cache=shared` (not compiled in); cross-process visibility uses
SQLite's WAL journal and POSIX file locks.

**System fields are auto-appended to every collection** — `id`,
`created_at`, `updated_at`, `created_by`, `updated_by`, `version`,
`deleted_at`. Creators don't declare them; they cannot redefine them
(reserved names enforced by `validate_field_name`). The set provides
audit trail, optimistic concurrency, soft-delete, and AEAD AAD
binding. Full design: `docs/proposals/platform-system-fields.md`.
Salesforce-style naked naming (no `__zs_*` prefix); matches the
existing `id` convention and the way LLM-generated app code reaches
for these fields naturally.

### 6.2 Columns / types

| ZsType | PG | SQLite |
|---|---|---|
| `Text(maxLen?)` | `TEXT` | `TEXT` |
| `Int` | `BIGINT` | `INTEGER` |
| `Float` | `DOUBLE PRECISION` | `REAL` |
| `Bool` | `BOOLEAN` | `INTEGER` with `CHECK (col IN (0, 1))` (no NULL coercion; NULL only if column declared nullable) |
| `Timestamp` | `TIMESTAMPTZ` | `INTEGER` (Unix-ms) |
| `CalendarDate` | `DATE` | `TEXT` (`YYYY-MM-DD`; CHECK) |
| `Json` | `JSONB` | `TEXT` (CHECK `json_valid`) |
| `Uuid` | `UUID` | `TEXT` (CHECK shape) |
| `Bytes` | `BYTEA` | `BLOB` |
| `Decimal(p,s)` | `NUMERIC(p,s)` | `TEXT`, see §6.2.1 |
| `Vector(n)` | `vector(n)` (pgvector) | vec0 (sqlite-vec extension; statically compiled) |
| `GeoPoint` | `geography(POINT, 4326)` | `BLOB` 16 bytes packed `(lat, lng)` + Rust Haversine post-filter (see P4 plan §4.3) |
| `Array(T)` | `T[]` native | `TEXT` (JSON-encoded) |

`tsvector` is intentionally absent from the user-facing type table. Full-text
search on PG uses a hidden `tsvector` column and GIN index; on SQLite a side
FTS5 virtual table maintained by triggers. Both are internal to
`FullTextIndex`; users never declare a column of "FTS type."

<!-- Round 4: state padding + implicit-decimal convention -->
#### 6.2.1 Decimal representation on SQLite
SQLite stores `Decimal(p, s)` as TEXT in a lex-sortable form.
Round-2's `+`/`-` ASCII prefix sketch was wrong: `+` (0x2B) < `-`
(0x2D), so negatives would sort after positives. **Convention.**
(a) Width is fixed at exactly `p` digits, zero-padded on the left
(required for lex-sort to align significance). (b) The decimal point
is **implicit at position `s` from the right** — not stored; values
of equal `(p, s)` are pure digit strings. (c) Sign prefix: byte `1`
for non-negatives, digits as-is; byte `0` for negatives, digit `d`
emitted as `9-d` (nines-complement), so deeper negatives sort lower
under ASCII. `Decimal(4, 0)`: `+0023 → "10023"`, `-0023 → "09976"`,
`-9999 → "00000"`, `+9999 → "19999"`. `Decimal(8, 4)` (money):
`+1234.5678 → "112345678"`, `+1.5 → "100015000"` (integer `0001`,
fractional `5000`, sign `1`), `-1.5 → "099984999"`. NaN/±Inf/
scientific notation rejected at SDK validation. Arithmetic
Rust-side via `rust_decimal`.

**Indexes.** Single-column, compound, partial, expression, unique, vector
ANN, FTS, spatial. PG `CONCURRENTLY`; SQLite atomic `CREATE INDEX` with
brief writer block.

**Foreign keys.** Within an app only; cross-app FKs forbidden by the SDK
and rejected at DDL parse time. PG: standard `ON DELETE` syntax. SQLite:
same; requires `PRAGMA foreign_keys = ON` per connection (set by
`SqliteBackend` on every client acquire).

<!-- Round 8: CRITICAL — promote system-tables paragraph to §6.5 anchor referenced by §11.5 -->
<!-- preupdate_hook amendment -->
### 6.5 System tables per app

`__zeroship_migrations` — DDL/validation/backfill audit (both
backends; schema in §10.7). `__zeroship_audit_<collection>` —
optional per-row data audit (schema in §10.7). `__zeroship_mv_<name>`
— shadow tables backing SQLite `MaterializedView`; writes are
filtered out by the SQLite CDC dispatcher's hook callback (§13.5).
`__zeroship_admin.*` — PG hardening only. This roster anchors
§10, §13.5, §17.5. SQLite CDC carries no system table of its own:
`sqlite3_preupdate_hook` delivers OLD/NEW row images natively, so
neither a pre-image outbox nor a sequence allocator is needed
(§11.5).

**Per-app metering storage.** Counters in the control-plane store, not in
per-app tables. The `MeteredSqlExecutor` decorator (§13) maintains
thread-local increments; a background flusher pushes every 5s.

---

## 7. Capability trait redesign

The current 26-method `Backend` trait is PG-flavoured; half the
orchestrator goes through it, the other half takes `&PostgresBackend`
directly (deferred CRITICAL [C1]). The redesign splits `Backend` into
fifteen focused capability sub-traits composed by the super-trait;
consumers take the narrowest bound. Splitting matters because each new
backend implements only what it has, consumer code declares precise
dependencies (a read-only function needs only `SqlExecutor`), and
PG-specific replication-slot / pgoutput code becomes a private detail
of `PostgresBackend`'s `ChangeStream` impl.

### 7.2 Capability surface

**`SqlExecutor`** — transport. Acquires a client (dedicated or pooled),
runs parameterised SQL, returns row count or `Vec<Row>`; `Row` is a
newtype over `Vec<RowValue>` + column-name index. `RowValue` (enum in
`exec.rs`) variants: `Null | Bool | Int | Float | Text | Bytes | Json |
Uuid | Decimal | Timestamp | Vector`. Two §6.2 ZsTypes reuse variants:
`GeoPoint` rides on `Bytes` (PG: PostGIS WKB; SQLite: packed `(lat,lng)`
floats); `Array(T)` rides on `Json` (PG: `T[]::jsonb`; SQLite: TEXT
already JSON). Encryption boundary needs `Bytes` distinct from `Text`;
metering needs typed row counts. PG impl wraps
`compio_postgres::Pool`/`Client`; SQLite wraps a `SqliteSession` actor
over `rusqlite::Connection` via `spawn_blocking`. `Client` associated
type: `compio_postgres::Client` or a `SqliteSession` handle.

**`NamespaceManager`** — per-app schema isolation lifecycle: ensure, drop,
list. PG: `CREATE SCHEMA IF NOT EXISTS` + grants. SQLite: open + `ATTACH
DATABASE` of the per-app file. Symmetric `drop_namespace` ordering against
in-flight CDC: §17.7.

**`LockManager`** — application-level lock primitive: try-acquire,
acquire (blocking), release (RAII via `LockGuard`). Lock-scope enum:
`GlobalApp { app_id, name }` (cross-process where backend supports),
`LocalApp { app_id, name }` (in-process only). `acquire` takes a
client because PG advisory locks are session-scoped and must use the
same connection as the protected DDL; SQLite ignores the argument
(documented in impl). Unified signature preferred over an
`acquire_session_bound`/`acquire_local` split so consumer call-sites
do not branch on backend.

**PG advisory-lock keying** (`GlobalApp`): canonical
`pg_advisory_lock(int4, int4)` with both ints from PG's `hashtext()`
builtin. First arg `hashtext(format!("{app_id}:{scope_name}"))`; second
`hashtext(scope_purpose)` where `scope_purpose` is e.g.
`"register_model"` or `format!("mig:{}", spec.name)` (§10.5). No
orchestrator-side prefix. Collision posture: §17.4.

PG `LocalApp` and SQLite (both scopes): in-process HashMap. SQLite
makes no cross-process claim — §8.5. Audit: `register_model` →
`GlobalApp { name: "register_model" }`; `migrations.run(spec)` →
`GlobalApp { name: format!("mig:{}", spec.name) }`; auto-tx envelope:
no lock (per-isolate state already serialises).

<!-- preupdate_hook amendment -->
**`ChangeStream`** — per-app CDC provisioning + consumer lifecycle.
Ops: `provision`, `deprovision`, `spawn_consumer` (returns RAII
`ConsumerHandle`). PG: publication + replication slot; consumer
streams pgoutput, pre-image + post-image in the frame. SQLite:
in-process CDC via `Connection::preupdate_hook` — fires BEFORE every
row mutation inside the writer transaction with native OLD/NEW row
data from `sqlite3_preupdate_old()` / `sqlite3_preupdate_new()`;
broker publish happens post-COMMIT to preserve transactional
consistency (ROLLBACK loses both the data and the hook callback's
intended publish). "Consumer" on SQLite is the writer-actor itself,
not a spawned background task.

**`SchemaIntrospect`** — read live DB state into `LiveSchema`
(`diff.rs`). PG: walks `pg_catalog`. SQLite: `sqlite_master` +
`pragma_table_info`/`pragma_index_list`/`pragma_foreign_key_list`. Also
exposes `estimate_row_count`.

**`IndexBuilder`** — create/drop indexes outside transactional DDL. PG:
`CREATE INDEX CONCURRENTLY` + INVALID-recovery loop. SQLite: atomic
`CREATE INDEX IF NOT EXISTS` with brief writer block (§8).

**`DialectBuilder`** — per-backend SQL fragments (column-type mapping,
identifier quoting, NOW, RETURNING, upsert, isolation-level BEGIN,
LIMIT/OFFSET, JSON path, FTS match, vector distance — ~30 hooks).
`query.rs` is dialect-neutral after the refactor.

**`SessionMinter`** — sign/verify session tokens. PG (hardening):
SECURITY DEFINER in `__zeroship_admin`; HMAC secret never leaves PG.
SQLite: Rust HMAC-SHA256 keyed by `ZEROSHIP_SESSION_SECRET`. §12.

**`Metering`** (decorator, NOT a sub-trait of `SqlExecutor`).
`MeteredSqlExecutor<E: SqlExecutor>` newtype wraps any executor; runs
`check_quota` before forwarding, increments per-app counters after;
exposes `record`/`check_quota`/`flush`. Decorator-not-subtrait:
metering arithmetic is backend-agnostic and a sub-trait would force
each backend to re-implement and entangle policy.

<!-- 2026-05-24 amendment: vector now routes through sqlite-vec vec0 -->
**`VectorIndex`, `FullTextIndex`, `SpatialIndex`** — parallel shape:
per-collection index creation + search. PG wraps pgvector / tsvector /
PostGIS; SQLite uses the **sqlite-vec extension (statically compiled
via sqlite-vec crate; vec0 virtual table)** for vector, FTS5 for
full-text, and packed-BLOB+Haversine for spatial (see §6.2). Search
accepts the same filter object as `find` (`near(point) AND status =
"open"`). SQLite caveat: `sqlite3_preupdate_hook` fires on rowid
tables only, not vtables (FTS5, vec0). Acceptable — the change-event
path keys off mutations to the **base** collection; vtable updates
are an implementation detail driven by AFTER triggers (FTS5 + vec0
both follow the same trigger-mirror pattern), which user code does
not subscribe to. The base collection still holds a canonical `BLOB`
column for the vector payload (CDC preupdate observes it natively;
the trigger fans the row into vec0). Geo storage is a plain BLOB
column on the base collection (no vtable).

<!-- preupdate_hook amendment -->
**`MaterializedView`** — ensure-and-refresh of cached aggregates. PG:
`MATERIALIZED VIEW` + `REFRESH CONCURRENTLY`; the MV relation is never
added to the publication so pgoutput emits nothing for refresh (base
tables drive subscribers; MV is a broker-invisible cache). SQLite:
per-MV shadow table `__zeroship_mv_<name>`; refresh is a delete-then-
insert under `BEGIN IMMEDIATE`. **Critical:** shadow tables are NOT
registered collections — the SQLite CDC dispatcher's
`preupdate_hook` callback filters writes to `__zeroship_mv_*` (along
with `__zeroship_audit_*` and `__zeroship_migrations`); see §13.5.
Default cadence per minute; SDK override.


<!-- Round 4: CRITICAL #1 (SIV-style nonce) + IMPORTANT #1 (scan-cost metering) -->
**`EncryptedColumn`** — symmetric AEAD via Rust-side `aes-gcm`. Key
derived `hkdf(ZEROSHIP_COLUMN_KEY, salt="zsenc/" + collection + "/" +
column)` into two subkeys: `k_enc` (AEAD) and `k_siv` (deterministic
nonce derivation). Stored as `Bytes`/`BLOB` — no sentinel prefix.
Layout: 12-byte nonce ‖ ciphertext ‖ 16-byte GCM tag. **Randomised
mode (default).** Per-row random nonce; AAD = collection id ‖ column
name ‖ row PK bytes (binds ciphertext to its row). Filter semantics:
equality/inequality on the encrypted column compile to a post-fetch
Rust-side scan **only** when an additional non-encrypted predicate
bounds the candidate set; SDK rejects (`ValidationFailed { code:
"invalid_filter" }`) any filter that would force a full-table scan on
an encrypted column alone. The orchestrator decrypts the bounded
candidate set in `MeteredSqlExecutor` post-processing; filter
re-applies client-side. Metering is on the **pre-decrypt candidate
count** (rows the storage layer fetched), not the post-filter count —
closes the AWS-DDB-style "items examined" gap. `like`, `gt/lt`,
ordering all return `invalid_filter`.

<!-- Round 5: IMPORTANT #1 — name the construction correctly; column-key rotation -->
<!-- Round 6: IMPORTANT #2 — correct prior-art lineage (Rogaway-Shrimpton 2006) -->
**Deterministic-IV variant** (`t.encrypted({ deterministic: true })`)
— indexed equality via a **deterministic-IV-via-HMAC** construction:
`nonce = HMAC-SHA256(k_siv, plaintext)[..12]`, then AES-GCM under
`k_enc` with that synthetic nonce. The lineage is Rogaway &
Shrimpton 2006 ("Deterministic Authenticated-Encryption"). RS06's
primary contributions are (1) the DAE security definition and (2)
the specific SIV construction (S2V + AES-CTR).
<!-- Round 8: MINOR #1 — narrow the citation to the SIV-via-PRF paradigm only -->
The doc's construction borrows only the SIV-via-PRF **paradigm**
described in RS06 §3.2 (derive the IV by a PRF over the plaintext,
then encrypt under that IV), with HMAC-SHA256 as PRF and AES-GCM
as the AEAD primitive. "Follows RS06 §3.2's SIV-via-PRF paradigm"
is the precise attribution; "instantiates Rogaway-Shrimpton" would
overclaim, since RS06's concrete SIV construction is S2V + AES-CTR,
neither of which appears here. It is **not** RFC 5297 AES-SIV
(S2V + AES-CTR), **not** RFC 8452 AES-GCM-SIV (POLYVAL). *Different AWS construction, same
problem space.* The **AWS Database Encryption SDK** (current name of
the DDB Encryption Client family) solves searchable-equality via
**beacons** — a separate column holds a truncated HKDF-keyed HMAC of
the plaintext, the value column itself stays randomised AES-GCM,
equality queries hit the beacon column. That is a different
mechanism (separate index column; ciphertext stays IND-CPA); noted
only to forestall the confusion. CipherStash's `equatable` mode is
closer in spirit (deterministic ciphertext on the value column);
construction undocumented externally. **Security argument** stands
on the construction: HMAC-SHA256 is a PRF, AES-GCM under PRF-derived
nonce is collision-bounded by 2^64 in a single column (birthday on
the 96-bit nonce); collision implies plaintext repeat, which
deterministic mode exposes by design.
Same plaintext under same `(collection, column)` produces identical
ciphertexts; a B-tree index on the ciphertext bytes answers
`WHERE col = ciphertext(?)`. Lookups recompute the ciphertext from
the query plaintext (SDK or `MeteredSqlExecutor`) and push the
bytes into the predicate. AAD omits row PK (would defeat cross-row
equality) but retains collection id ‖ column name (ciphertext from
column A cannot be replayed into column B). **Security tradeoff.**
Deterministic mode leaks equality patterns: an observer with table
access learns which rows share a plaintext, frequencies, and (with
auxiliary data) can run inference attacks. Opt-in per column; SDK
type and dashboard surface the warning. pgcrypto-native path on PG
deferred (§19 P6); the Rust-side scheme works on both backends
today.

**Key rotation.** `ZEROSHIP_COLUMN_KEY` (the HKDF root) rotation is
**out of scope** in current scope. Rotating it invalidates every
derived `(k_enc, k_siv)`; deterministic ciphertexts written under the
old `k_siv` become opaque to equality search under the new key —
recovery requires a full re-encryption pass per encrypted column
(read + decrypt-old + re-encrypt-new + UPDATE), equivalent to a
backfill (§10.6). Production-grade rotation (operator-driven
re-encryption under broker pause) deferred to §19 P6. The
session-secret rotation rail (§12) does NOT cover column keys; the
two key materials are independent.

**`Backup`** — snapshot, restore, PITR. PG: `pg_basebackup` for snapshot,
WAL replay for PITR. SQLite: `VACUUM INTO` for snapshot; `pitr_replay`
returns `Configuration { code: "pitr_pg_only" }`. The dashboard hides PITR
controls in dev so this never surfaces.

### 7.3 Consumer code shape
Orchestrator entry points are generic over the narrowest capability
bound (e.g. migration `apply`: `SqlExecutor + IndexBuilder +
LockManager`). Metering applies by composition (`MeteredSqlExecutor`)
without appearing in the bound. Consumer code never names `Backend`;
the super-trait is a documentation alias for "all fifteen capabilities."

### 7.4 Migration plan
Extract the 26 methods into fifteen sub-traits; rewrite
`PostgresBackend` as one `impl` per capability (no behavioural change).
Migrate every `&PostgresBackend` parameter site to `&impl <narrowest
bound>` (site count measured at the start of P0, not estimated). Add
`SqliteBackend` in `backend/sqlite.rs`. Cargo features: default `pg`
(`compio-postgres`); `sqlite` (`rusqlite`). Runtime URL-prefix
discriminator selects the `BackendHandle` arm. Rewire
`IsolateDbContext::backend: Option<Rc<PostgresBackend>>` →
`Option<BackendHandle>` (§5.5). NO `Rc<dyn Backend>`.

---

## 8. SQLite implementation strategy

§4 lists every capability with the SQLite mechanism in a single line. This
section names only the behavioural divergences from PG and the load-bearing
details (lock semantics, CDC commit ordering) the round-1 critique flagged.
No **multi-line** SQL fragments inline — every "trigger" / "BEGIN" /
"ATTACH" claim is described in intent. Single SQL keywords and function
names (`json_object`, `BEGIN IMMEDIATE`, `RETURNING`) appear as
identifiers, not as code blocks. Per-capability test names appear in
§19.

**Concurrent writers.** Connection PRAGMAs: `journal_mode=WAL`,
`synchronous=NORMAL`, `busy_timeout=5000ms`. One `SqliteSession` actor per
app serialises writes via its `mpsc` command queue (the channel is the
writer slot). One writer at file level; concurrent readers proceed under
WAL. `SQLITE_BUSY` never surfaces to JS.

**Online DDL.** Atomic `CREATE INDEX IF NOT EXISTS` briefly blocks writers;
PG `CONCURRENTLY` does not.

**Replicas + PITR.** None on SQLite; `pitr_replay` returns
`Configuration { code: "pitr_pg_only" }`. Dashboard hides PITR controls in
dev.

### 8.5 Cross-process advisory locks
In-process HashMap registry covers intra-process mutual exclusion.
Cross-process mutual exclusion is **not** an application lock: SQLite's
own writer reservation (taken by `BEGIN IMMEDIATE` when the protected
DDL runs) is the cross-process serialisation point. `BEGIN IMMEDIATE`
acquires the writer reservation immediately, blocks other writers but
NOT readers in WAL mode. `BEGIN EXCLUSIVE` was specified in earlier
drafts and is NOT used (in WAL mode it attempts an exclusive
`wal-index` mmap lock that blocks readers — heavier than necessary). A
second OS process racing on the protected op sees `SQLITE_BUSY`, mapped
to `LockContention { retryable: true }`.

<!-- preupdate_hook amendment -->
### 8.7 Reactive queries / CDC (summary)
Full description in §11.5. Key load-bearing details:
`Connection::preupdate_hook` registered once at `SqliteSession` open
time; the hook receives `(action, db_name, table_name, rowid,
old_row?, new_row?)` BEFORE every row mutation; the writer-actor
appends to a per-tx event buffer in DB-execution order (Vec index =
intra-tx sequence); on COMMIT the buffer flushes to the broker
post-commit; on ROLLBACK the buffer is dropped. No triggers, no
outbox, no DDL emitted at registration.

### 8.12 Schema-per-app
Open or create `${db_dir}/zs-${app_id}.sqlite` and ATTACH it as
`"<app_id>"`. Plain `file:` URI — no `cache=shared`. Bulk import/export:
row-by-row INSERT in one tx (slower than PG `COPY` at large N;
acceptable at dev scale). Backups: `VACUUM INTO`; `.backup` fallback
when busy (§16.1).

---

## 9. Postgres implementation strategy

Most PG behaviour is already shipped. After the §7 split, `PostgresBackend`
implements every sub-trait — a mechanical partition of the current
monolithic impl. Every capability already has working PG code in
`crates/plugin-db/src/backend/postgres.rs`; the split is structural
rebinding, not new functionality.

`compio-postgres` driver (zero-tokio, io_uring) used everywhere.
Per-isolate `IsolateDbContext` is a state-machine slot lattice (pool,
tx_conn, mig_lock, pending_emits, running_consumers, registered_models,
backend); lock invariants enforced by `debug_assert!`. WAL consumer:
`replication.rs` provisions publication + slot per app;
`wal_consumer::run_supervised` streams pgoutput; watchdog reconnects with
exponential backoff. SECURITY DEFINER auth subtree gated by the
`hardening` Cargo feature.

### 9.1 PG operational prerequisites
**Cluster level.** `wal_level = logical`;
`max_replication_slots ≥ (max apps per cluster) × N_workers + headroom`
(slot-fan-out math in §11.3); `max_wal_senders ≥
max_replication_slots`; control-plane DB role with the `REPLICATION`
attribute (or `pg_create_logical_replication_slot` execute permission).
**Per-collection.** `REPLICA IDENTITY FULL` is asserted by the
migration pipeline's Apply step (`ALTER TABLE … REPLICA IDENTITY
FULL`); without it pgoutput frames omit non-PK pre-image columns,
breaking the §5.5/§11.5 promise. Cost: WAL growth linear in row width
on UPDATE/DELETE; accepted.

**Driver-side prerequisite.** WAL consumer requires the PG streaming-
replication sub-protocol — distinct from extended query:
`replication=database` startup parameter, `START_REPLICATION SLOT …
LOGICAL …`, `CopyBothResponse` framing, pgoutput decoding, keepalive
responses, standby status updates. `compio-postgres` implements this
in `crates/compio-postgres/src/replication.rs`; `wal_consumer.rs`
ships against it. Consumer opens a dedicated replication connection
per `(worker, app)` — NOT a pooled extended-query client.

Failure modes: missing cluster prerequisites →
`PostgresBackend::new` returns `Configuration { code:
"wal_level_not_logical" }` at startup; missing `REPLICA IDENTITY FULL`
→ migration pipeline returns `Configuration { code:
"replica_identity_required" }`. The control-plane health check refuses
ready until `SHOW` confirms settings.

---

## 10. Migration pipeline

Same shape on both backends.

**Entry point.** `db.registerModel(collection, schema, indexes)`.
Idempotent via the `IsolateDbContext::registered_models` cache.

**Strictness modes** (per-collection). `strict` → destructive ops produce
`validation_refused`; deploy aborts. `lenient` → destructive ops skipped +
logged + audited (pending F2). `off` → destructive ops apply (tests only).

### 10.3 Four-phase pipeline

(1) **Bootstrap** — `NamespaceManager::ensure_namespace`,
ensure_audit_table, acquire the `register_model` lock, compute next
schema_version, expand indexes. (2) **Plan** —
`SchemaIntrospect::introspect_schema`; diff; classify each op as
Additive | Compatible | Destructive. (3) **Validate** — apply strictness
rules; emit pending audit rows for destructive ops; return
`SchemaRefused` on strict refusal. (4) **Apply** — Pass 1 (under lock):
transactional ops with one audit row per op; assert `REPLICA IDENTITY
FULL` on new collections (PG; §9.1); release lock. Pass 2 (unlocked):
`IndexBuilder::create_index` (PG CONCURRENTLY; SQLite atomic).
"Backfill" (§10.6) is a separate B1 subsystem layered on top of the
pipeline, NOT a fifth phase (cf. §2).

### Audit row state machine
```
Pending → Running → {Applied, AppliedWithDeadLetter, Failed, Cancelled}
        → Skipped (lenient destructive — F2)
        → ValidationRefused (strict destructive — F2)
```

### 10.5 Advisory lock coordination
Canonical key: `LockScope::GlobalApp { app_id, name:
"register_model" }`. `name` is the literal string on both backends.
PG keys `pg_advisory_lock(int4, int4)` via `hashtext()` —
`hashtext(format!("{app_id}:register_model"))` and
`hashtext("register_model")` (formula §7.2, collision posture §17.4;
no `zs_reg:` prefix). SQLite keys its in-process HashMap on
`(app_id, name)`. Acquired in Pass 1; released between Pass 1 and
Pass 2 so `CREATE INDEX CONCURRENTLY` (PG) or atomic SQLite build
proceeds without holding it.

### 10.6 Backfill
`defineMigration` and `migrations.run` enter `exec_begin` under
`GlobalApp { app_id, name: format!("mig:{}", spec.name) }`. Per-batch
loop: `fetchBatch` → JS `migrateOne` → `commitBatch`, each batch in its
own short tx. Normal: per-batch `COMMIT` (progress durable per batch).
Dry-run: per-batch `ROLLBACK`; no outer tx wraps the loop, so per-batch
ROLLBACK is the only state-change suppression. Read-only validation
work persists via a separate `dry_run_summary` audit row outside the
per-batch tx. Resume via `migrations.run(spec, { reset: true })`; state
in `__zeroship_migrations` with `phase = 'backfill'`.

<!-- Round 5: IMPORTANT #5 — backfill × CDC broker pause -->
<!-- preupdate_hook amendment: SQLite side fires preupdate_hook, not triggers -->
**Backfill × CDC interaction.** Each batch mutates user data, so each
batch fires the preupdate hook (SQLite) or emits pgoutput frames (PG)
— a million-row backfill would shove a million events through the
broker. The orchestrator therefore **pauses the per-app broker** for
the duration of `migrations.run` via the same
`Broker::suppress_app(app_id, true)` rail §11.6 uses for
`register_model` (called at backfill `exec_begin`, cleared at exit
including failure / reset). Subscribers see one `resync` at resume.
Backfill mutations still pass through the hook / pgoutput — the pause
drops at the fan-out boundary, not at the source — and the SQLite
writer-actor still appends to its per-tx event buffer, which the
suppress check drains at COMMIT-flush time without invoking the
broker. Avoids a parallel backfill-bypass codepath.

**Open backlog.** F1 (orphan Running rows): warn-half closed; sweeper-half
open (§18 Q4). F2 (orphan Pending rows): write-and-terminate to
`ValidationRefused` recommended (§18 Q5).

### 10.7 Audit-table schema
`__zeroship_audit_<collection>` per app per audited collection (opt in
via `t.audit()`). Columns: `audit_id` (typed_id `aud_…`, PK), `op`
(`insert|update|delete|restore`), `pk` (text — encoded primary key),
`actor_kind`/`actor_id`/`pid` (from session token; `pid` is project
id — §12, §20), `at`
(timestamp), `before` and `after` JSON columns (NULL where not
applicable). **Writer.** Orchestrator-emitted post-`RETURNING`,
pre-COMMIT, inside the writer tx — durable iff the mutation is.
Trigger-free (orchestrator already has both images). **Retention.**
Default 90 days; per-collection via `t.audit({ retainDays: N })`;
control-plane sweeper deletes daily. Audit writes and sweeper deletes
are NOT counted toward `rows_written`.

---

## 11. Reactive queries (CDC + broker)

**Subscription model.** `env.db.openSubscription(name)` returns an async
iterator yielding `{ kind: "change", … }`. Registration is synchronous
(slot in the per-isolate broker; `v8::Weak` finalizer for GC safety).
`db.live(queryFn)` runs the function once, captures touched tables via the
read-set tracker, subscribes to each, reruns on every relevant change.

**Server-side filter predicate.** `read_set::Predicate::matches` evaluates
the normalised predicate against a `ChangeEvent`'s tuples before fanout, so
a 10k-row UPDATE touching `status='paid'` rows does not wake subscribers
filtering for `status='shipped'`.

### 11.3 Per-isolate broker; cross-isolate delivery
Broker is thread-local; compio runtime is single-threaded per worker.
PG cross-thread delivery: writer commits → WAL → every worker with an
open replication slot for that app decodes via its WAL consumer →
local broker fans out. **Slot accounting.** Each worker × loaded-app
holds one slot. §16.6 commits "slots are NOT dropped on eviction";
the watchdog (§17.6) is the reaper. Realistic active-slot count is
therefore `N_workers × M_ever_loaded_apps_since_last_reap` — strictly
greater than `M_loaded_apps`. Production sizes
`max_replication_slots` to fleet demand + reap interval headroom
(§9.1); §16.3 alerts at 80% of the configured cap. Slot lifecycle on
app deletion: §17.7. SQLite: no equivalent — one isolate per app per
process.

**PG implementation.** `replication.rs::ensure_publication_and_slot`
provisions publication + slot. `wal_consumer::run_supervised` opens a
replication connection, streams pgoutput (pre-image + post-image under
`REPLICA IDENTITY FULL`), decodes into `ChangeEvent`, calls
`broker::publish`.

<!-- preupdate_hook amendment -->
### 11.5 SQLite implementation
`SqliteCdcDispatcher` in `backend/sqlite_cdc.rs`. CDC rides the
native `sqlite3_preupdate_hook` API (SQLite ≥ 3.16, 2017), exposed
by rusqlite under the `preupdate_hook` Cargo feature and compiled
into the `bundled` amalgamation via `SQLITE_ENABLE_PREUPDATE_HOOK`.
No triggers, no outbox table, no `lsn_seq` allocator, no DDL emitted
at registration.

**Hook registration.** `Connection::preupdate_hook` is set once at
`SqliteSession` open time on the writer actor's connection. The
callback runs synchronously in the writer's C-stack frame before
every row mutation, transaction still open. Rusqlite callback shape:
`(action, db_name, table_name, rowid, old_row?, new_row?)` where
`old_row?`/`new_row?` are accessors over `sqlite3_preupdate_old()` /
`sqlite3_preupdate_new()` returning typed column values.

**Per-tx event buffer.** Each writer transaction owns a `Vec` of
events on the `SqliteSession` actor. On hook fire: the callback
filters the relation (§13.5), materialises OLD/NEW into a
`ChangeEvent`, pushes onto the buffer. On COMMIT (writer actor):
flush to the broker post-commit. On ROLLBACK: drop the buffer
(actor owns the allocation outright — single drop, free).

**Pre-image (OLD) / post-image (NEW).** Both native. INSERT carries
NEW only; UPDATE carries OLD + NEW; DELETE carries OLD only. No
`RETURNING` join, no SELECT-by-pk, no JSON encoding pass, no
orchestrator-side reconstruction.

**Ordering within a tx.** The hook fires in DB-execution order (the
order the writer's SQL hits the b-tree); the per-tx `Vec`'s index
*is* the intra-tx sequence — no allocator, no `_seq` table, no
rowid-monotonicity proof, no AUTOINCREMENT invariant. §6.5 shrinks
accordingly.

**`(commit_id, lsn_seq)` tuple.** Still the broker contract.
`commit_id` is an in-process `AtomicU64` on the `SqliteSession`
actor, stamped at COMMIT-flush time (after SQLite COMMIT succeeds,
before the buffer publishes). `lsn_seq` is the buffer's `Vec` index
(0..N). Subscribers observe broker-side `(commit_id, lsn_seq)`
identical in shape to the PG side's `(commit_LSN, frame_index)`.
Per-session monotonicity is the only invariant; counter resets to 0
on session boot, subscribers crossing restart receive `resync`
(§11.6).

**Rollback / GC.** Free. No outbox table → no GC subsystem → no
startup truncate, no pre-COMMIT DELETE, no `db_pre_image_outbox_rows`
gauge (§16.3 drops it). Rollback drops the buffer.

**FTS interaction.** `FullTextIndex` still installs AFTER triggers
on the base collection to maintain the FTS5 vtable; those fire after
the preupdate hook (preupdate is BEFORE), so the broker observes the
base-row event with the FTS update pending in the same tx. The hook
is on rowid tables only; FTS5 vtable updates do not re-enter it
(vtable mutations bypass `preupdate_hook` by design).

### 11.6 Pause/resume during DDL and backfill
`register_model` calls `Broker::suppress_app(app_id, true)` before Pass
1 of the migration pipeline (§10.3) and clears it after Pass 2 or on
error. While suppressed, the broker **drops** events for the app (not
buffered — DDL-driven mutations generate events the subscriber's old
schema cannot represent); active subscribers receive one `resync` at
end of suppression, prompting refetch under the new schema; subscribers
registered during the window see only post-DDL events. Per-app.
<!-- Round 5: IMPORTANT #5 cross-link -->
The same `suppress_app` rail is used by `migrations.run` to mute the
per-batch event flood (§10.6); the broker primitive is shared, the
calling site differs.

**Backpressure.** Each `SubscriptionInner` has a bounded queue (default
1024). Overflow emits one `resync`, clears the queue, sets
`resync_pending` to avoid spam. SDK refetches on resync.

### 11.8 Within-app multi-tenancy interaction
A subscription via `db.scoped({ org_id }).posts.subscribe(...)` carries
the scope key in its predicate; broker `Predicate::matches` rejects
other `org_id` values before fanout. Reactive path inherits within-app
isolation from the predicate evaluator. Unscoped collections don't
expose `db.scoped(...)` (TS build-time error).

---

## 12. Auth subsystem

Stable spec: `docs/reference/auth.md`. Plugin-db-local view only.

**`SessionMinter` — two impls.** PG (hardening): SECURITY DEFINER
`__zeroship_admin.sign_session(...)` over the platform connection; HMAC
secret in `__zeroship_admin.hmac_keys` never leaves PG; rotation via
`__zeroship_admin.rotate_session_keys` (retires current → previous,
inserts fresh; verification accepts both within grace); nonce replay
protection in `__zeroship_admin.session_nonces` (25h retention).
SQLite: Rust HMAC-SHA256 over canonical payload
`actor_kind:actor_id:pid:nonce:expires_at`; secret from
`ZEROSHIP_SESSION_SECRET` (required); previous-key grace via
`ZEROSHIP_SESSION_SECRET_PREV`; in-memory nonce LRU (~10k).

<!-- Round 4: anchor "project" / pid -->
**Token payload.** Base64url record: actor kind, actor id, pid,
nonce, expiry, signature. SDK never inspects the signature. **`pid`
is project id** — typed_id (`prj_…`) of the creator's project owning
the app this session was minted under. A project is the
billing/ownership unit one level above an app: one project may own
many apps; quotas roll up at project level; the audit table (§10.7)
records pid so the audit trail survives app transfer. Project itself
lives in the control plane (`docs/reference/billing-metering.md`,
`docs/reference/auth.md`); plugin-db treats `pid` as opaque.
**Defaults:** `DEFAULT_TOKEN_TTL_SECS = 300`;
`NONCE_RETENTION_SECS = 25 * 3600`.
**Rotation.** PG: SECURITY DEFINER `rotate_session_keys` on a control-
plane schedule. SQLite: env-var reload on SIGHUP; dev tier does not
auto-rotate.

**Threat model divergence.** PG: secret never leaves DB; full app-role
compromise does NOT permit forgery (no SELECT on
`__zeroship_admin.hmac_keys`). SQLite: secret in process memory; full
process compromise permits forgery. Dev-only.

---

## 13. Metering + quotas

Stable end-to-end billing spec: `docs/reference/billing-metering.md`.
DB-subsystem-local view only.

**Counters.** Per-app, in a thread-local
`RefCell<HashMap<(app_id, metric), u64>>`: `rows_read`,
`rows_written`, `bytes_stored`, `query_ms`,
`subscription_events_delivered`. `bytes_stored` source: PG —
`pg_total_relation_size` summed per app schema (includes TOAST,
indexes within the schema, **and vacuum-reclaimable bloat**;
creators may see `bytes_stored` drop after autovacuum or
operator-issued `VACUUM FULL` — intentional, matches Aurora's
storage metric); SQLite — file size of
`${db_dir}/zs-${app_id}.sqlite` plus `-wal`/`-shm` (also
bloat-inclusive). Refresh cadence: 60s PG, 30s SQLite. The internal
counter is `rows_written`; the typed error code on quota overrun is
`rows_written_quota_exceeded` — different strings intentionally
(§20).

**`Metering` decorator** (§7.2). `MeteredSqlExecutor<E>` calls
`check_quota` before forwarding to the inner `SqlExecutor`, records
`query_ms` from a wall-clock sample, and records `rows_written` /
`rows_read` from the return value. Updates touch only the thread-local
map; nothing crosses the isolate boundary on the hot path.

**Flusher.** Background task pushes thread-local counters to the
control plane every 5s (`ZEROSHIP_METER_FLUSH_MS`). Wire shape:
`crates/core/src/lib.rs::UsageReport`.

**Failure modes.** Cannot reach control plane → worker accumulates
in-memory; reconnect includes the backlog. Overflow policy in
`docs/reference/billing-metering.md`. Quota evaluation continues
against last-known cache; worker fails **open** ≤60s after cache TTL
expiry, then **closed** with `quota_cache_stale`. Fail-open window via
`ZEROSHIP_QUOTA_FAILOPEN_MS`. Worker crash between flushes loses ≤5s
of increments — accepted; metering is best-effort.

**Quota cache.** Worker-local; refreshed every 60s by pull from
control plane (`GET /internal/quotas/:app_id`). `check_quota` consults;
hit + over → `Coded { code: "rows_written_quota_exceeded", … }`. Cache
miss → synchronous pull seeds the cache; **single quota path**. The
5s flusher push is **one-way** (`UsageReport` only; control-plane
response is 204, no quota piggyback). The earlier "flush ACK carries
quota" wording in round 2 was wrong — corrected here. The fail-open/
fail-closed timer lives per worker off the local TTL clock; control
plane stays stateless w.r.t. failure-mode policy.

<!-- Round 4: IMPORTANT #5 — MV is not broker-visible, full stop -->
<!-- preupdate_hook amendment -->
### 13.5 MaterializedView × CDC interaction
MV refresh is invisible to the broker. PG: the publication
enumerates user-declared collections only; the `MATERIALIZED VIEW`
relation is never added, so pgoutput emits nothing for `REFRESH
MATERIALIZED VIEW` (CONCURRENTLY swaps and non-CONCURRENTLY
TRUNCATE+INSERT are both filtered because the relation oid is not
in the publication set). SQLite: `sqlite3_preupdate_hook` is
per-connection — it fires for ANY rowid-table mutation on the
connection, including writes to `__zeroship_mv_<name>` shadow
tables during refresh. A filter is therefore still required, but it
lives in the hook callback (Rust-side) — not in a DDL allow-list.
The SQLite CDC dispatcher's hook callback silently drops events
whose `table_name` matches `__zeroship_mv_*`, `__zeroship_audit_*`,
or `__zeroship_migrations`. No DDL gymnastics required — pure Rust
dispatch. **MV subscriptions are not exposed.**
`Collection.subscribe` on an MV is rejected by the SDK
(`ValidationFailed { code: "invalid_collection" }` — MV is
read-only with per-minute refresh granularity). Subscribers wanting
aggregate updates subscribe to the base collections and recompute,
or poll the MV. The §7.2 "broker-invisible cache" characterisation
is the contract. Asserted by
`mv_refresh_emits_no_change_events_on_base_or_shadow` and
`mv_subscribe_rejected_at_sdk` (§19 P2).

---

## 14. Multi-tenancy

**Cross-app isolation (three layers).** (a) Per-app namespace — every
SQL emits `"<app_id>"."<table>"`; PG schema-per-app; SQLite file-per-
app via ATTACH. (b) Reserved-prefix — `validate_collection` rejects
`pg_*` and `__zeroship*` from user collections. (c) Capability-trait
boundary — every backend method takes `app_id`; per-isolate context
injects from `SharedState.env_vars["APP_ID"]`. No JS-supplied `app_id`.
Cross-app queries are unexpressible in the SDK; `Collection` wrappers
bind `app_id` at mint time.

**Within-app multi-tenancy.** Schema declares scope keys
(`t.scope("org_id")`). For scoped collections, the SDK generator emits
`db.scoped({ org_id })` that rewrites every filter to include `WHERE
org_id = ?` and rejects mutations whose input would write a different
`org_id`. Unscoped collections don't expose `db.scoped(...)` (TS-
enforced). Backend code knows nothing about scoping. Reactive
subscriptions inherit the scope predicate (§11.8).

**Row-level security.** App-layer default via `db.scoped(...)` —
uniform across backends. Native (deferred §19 P6): PG `CREATE POLICY`;
SQLite has no equivalent.

---

## 15. SDK shape

Full SDK reference: `docs/reference/db.md`. Plugin-db-imposed contract
only.

**Schema declaration.** `@zeroship/db` exposes `t` and `schema`.
Authors declare collections under `default.schema` as `name →
schema(...)` chains of column declarations (`t.string()`, `t.ref()`,
`t.vector(n)`, `t.geoPoint()`, `t.json()`, `t.scope("...")`) and
modifiers (`.index/uniqueIndex`). Canonical example:
`docs/reference/db.md`. plugin-db consumes via the `installSchema`
orchestrator (`sdks/bootstrap/README.md`).

**System fields are auto-injected by the platform** (§6.1; full design
at `docs/proposals/platform-system-fields.md`). Creators don't declare
them; they appear automatically in `Row<S>`:

```typescript
const post = await db.posts.findOne({ id: "post_..." });
// post.created_at, post.updated_at, post.created_by,
// post.updated_by, post.version, post.deleted_at — all auto-populated
```

The earlier `.softDelete()` / `.withVersioning()` modifiers are
**retired** — these behaviours are now universal platform features,
not opt-in per-collection. `delete()` performs soft-delete by default;
`purge()` is the new hard-delete; optimistic concurrency via
`version` field works on every table without declaration.

**Type generation.** Vite-plugin build-time analysis emits ambient
`Row<S>`/`RowInput<S>`/`Id<"posts">` via the `zeroship-schema` alias.

**CRUD + subscriptions.** Per collection: `find`, `insert`,
`updateOne`, `delete`, `upsert`, `aggregate`, `count`, `exists`,
`distinct`, `subscribe`. Same filter object across reads/writes/subs.
`subscribe(filter, callback)` returns a handle with `.cancel()`;
callback receives normalised `ChangeEvent`.

**Transactions.** `db.transaction(fn, { isolationLevel })`. Inside
`query()`/`mutation()` wrappers from `@zeroship/server`, transactions
auto-open (read-only for query; serializable for mutation).

**Vector / FTS / geo.** `db.c.search({ vector, k })`, `db.c.search({ text
})`, `db.c.near({ point, radius })`.

### 15.7 Errors — stable `.code` taxonomy
| `.code` | Variant |
|---|---|
| `validation_refused` | SchemaRefused |
| `unique_violation` | UniqueViolation |
| `fk_violation` | FkViolation |
| `not_null_violation` | NotNullViolation |
| `check_violation` | CheckViolation |
| `serialization_failure` | Serialization (retry) |
| `lock_not_available` | LockContention (retry) |
| `transient` | Transient (retry) |
| `invalid_filter` / `invalid_collection` / `invalid_identifier` | ValidationFailed |
| `lazy_init_failed` / `not_configured` / `wal_level_not_logical` / `replica_identity_required` | Configuration |
| `migration_*` | Coded (migrations.rs) |
| `internal` | Internal |
| `rows_written_quota_exceeded` | Coded (Metering) |
| `quota_cache_stale` | Coded (Metering) |
| `pitr_pg_only` / `migration_in_progress` | Configuration (SQLite admin) |
| `subscriptions_active` | Conflict (drop-namespace deferral; §17.7) |
| `subscription_app_dropped` | Terminal (event-channel code, NOT an SDK call return; surfaces on the subscription iterator when the orchestrator drops the app under `--force`; §17.7) |
| `schema_pending` | Conflict (`subscribe(...)` during reload window; SDK retries with bounded backoff; §16.7) |

SDK callers branch on `.code`; never substring-match. <!-- Round 5: IMPORTANT #3 — terminal event-channel code listed -->

---

## 16. Operational features

### 16.1 Backups + snapshots
PG: continuous WAL archive + nightly `pg_basebackup`; logical dump
per-schema; restore via `pg_restore`. SQLite: `Backup::snapshot` via
`VACUUM INTO` → blob store; restore = download + replace.

<!-- Round 4: CRITICAL #3 — VACUUM INTO uses WAL read-snapshot -->
**SQLite snapshot consistency.** `VACUUM INTO 'snapshot.db'` opens a
**read transaction** on the source and streams pages to the
destination. In WAL mode that is a shared-snapshot read: writers
proceed concurrently against the live file (their commits land in
the WAL after the snapshot's read mark), and the destination is a
consistent point-in-time copy matching the last committed tx visible
when `VACUUM INTO` began. No writer reservation is taken on the
source; the round-3 "same reservations as `BEGIN IMMEDIATE`" claim
was wrong and is removed. `SQLITE_BUSY` only arises against a
concurrent schema-change op or a checkpointer needing an exclusive
lock — narrow window. The orchestrator maps that to
`LockContention { retryable: true }`; `Backup::snapshot({ ifBusy:
"abort" })` surfaces it instead of retrying. Fallback: the rusqlite
`backup::Backup` page-copy API also holds a shared read lock and
gives the same "as-of last committed tx" consistency. Snapshots are
refused with `migration_in_progress` while the `register_model` lock
is held — a process-level interlock, not a SQLite-level one.

### 16.2 Migration history + rollback
`__zeroship_migrations` is the source of truth. Rollback is not a
separate mechanism: a DDL change is additive (undo via separate
destructive deploy), destructive-refused (nothing to roll back), or
destructive-with-strictness-off (operator owns consequences).

### 16.3 Observability
Latency in milliseconds (`_ms` suffix; the earlier `_us` suffix on
the SQLite hook latency metric is renamed; the metric itself is
now `db_sqlite_preupdate_hook_latency_ms` — preupdate amendment).
Tracing spans on every backend call; slow-query log at >100ms;
per-app query distribution.
Metrics: `db_pool_acquired`, `db_pool_idle` (PG);
`db_sqlite_busy_retries`, `db_sqlite_attached_files`;
`db_broker_subscribers`, `db_broker_fanout_latency_ms`;
`db_pending_emit_queue_depth` (per (worker, app); high values =
in-tx CDC buffer backpressure or stuck commit);
<!-- Round 5: missing concept — schema-pending gauge -->
<!-- preupdate_hook amendment: outbox gauge removed (no outbox) -->
`db_sqlite_preupdate_buffer_depth` (per (worker, app); current
writer-actor per-tx event buffer length; **sampled after each
COMMIT** — expected 0 post-flush; sustained non-zero across
consecutive COMMIT samples = stuck flush or unpaused backfill);
`db_schema_pending_dropped_events`
(per (worker, app); events dropped by §16.7 schema-pending decoder
during a reload window; alert when `> 1000 over 5 min and
schema_pending = true` for that app — distinguishes a wedged
decoder from a normal deploy-window burst);
`db_wal_consumer_lag_bytes` (PG), `db_sqlite_preupdate_hook_latency_ms`;
`db_migration_active_runs`; `db_replication_slots_in_use` (PG);
`db_quota_failopen_active`. Alerting (control plane): WAL consumer lag
>1 MiB sustained 30s → page; slot count >80% of
`max_replication_slots` → warn; `db_migration_active_runs` >0 for >10
min on one app → warn (F1 sweeper candidate); `db_pending_emit_queue_
depth` >500 sustained 30s → warn;
<!-- preupdate_hook amendment: outbox-rows alert replaced by buffer-depth alert -->
`db_sqlite_preupdate_buffer_depth` >1000 across consecutive
post-COMMIT samples (sustained 30s) → warn (stuck flush or unpaused
backfill); `db_schema_pending_dropped_events`
>1000 over 5 min while `schema_pending = true` for that app → warn
(wedged schema-pending decoder; normal deploy bursts clear within
the §16.7 reload window). Runbooks under `docs/runbooks/`.

### 16.4 Disaster recovery
PG: PITR + offsite replica. SQLite: snapshot restore.

### 16.6 Isolate eviction + panic lifecycle
LRU evicts cold isolates; the same `Drop` chain runs on panic unwind.
`IsolateDbContext::drop` order: cancel-and-await `running_consumers`;
ROLLBACK open tx in `tx_conn` (panic-poisoned PG client's own
disconnect issues server-side ROLLBACK); drop `LockGuard` (PG: releases
session-bound `pg_advisory_lock`; SQLite: removes HashMap entry); clear
`pending_emit` without publishing; sync-flush metering; drop
`Pool`/`SqliteSession`. Field order in `IsolateDbContext` is the
canonical reverse-drop contract. PG slots are NOT dropped on eviction
(same app may reload); reaping is the watchdog's job (§17.6).

### 16.7 Schema-version coordination across the worker fleet
Control plane is the source of truth. Deploy flow: control plane writes
the bundle to object storage; runs `register_model` against PG once
(NOT per worker); bumps `model_version` in routing. Workers pull
routing every 5s; on next request a worker compares its loaded
`registered_models[*].schema_version` to routing's version; stale →
isolate evicted and reloaded. `register_model` is not concurrent in
production; §10's advisory lock is defence-in-depth.

<!-- Round 4: IMPORTANT #4 — bundle_invalidated durability + idle-isolate TTL -->
**Staleness window for idle isolates.** Stale check fires on next
request — an idle isolate holds the old `LiveSchema` until traffic
returns. CDC events through that worker's broker between deploy and
next-request may decode against the old schema. Bounded by (a) 5s
routing pull, (b) §11.6 broker pause/resume covering active
subscriptions during `register_model`. The remaining hole — an idle
isolate's PG WAL consumer decoding new frames against the old schema
— is closed by a `bundle_invalidated` control event emitted at
`model_version` bump. Worker drops the old `LiveSchema` ref and
attaches a "schema-pending" decoder that suppresses events until
reload, publishing one synthetic `resync` at the end.

<!-- Round 5: IMPORTANT #4 — schema-pending decoder definition -->
<!-- Round 6: IMPORTANT #3 — new-subscriber policy + PG-side decoder error path -->
<!-- Round 8: IMPORTANT — reconcile shim placement (upstream of ConsumerHandle, wrapping the decoder) -->
<!-- preupdate_hook amendment: SQLite-side shim is trivially "in the dispatcher" Rust callback, no DDL/SQL primitive -->
**Schema-pending decoder, defined.** Thin shim **upstream** of the
worker's `ChangeStream::ConsumerHandle`, **wrapping the pgoutput
decoder** (PG) / preupdate-hook dispatcher (SQLite), so it sees raw
events before they reach `ConsumerHandle`'s event sink. On SQLite
the shim placement debate is moot — `preupdate_hook` is a Rust
callback already running inside the dispatcher, so the schema-
pending check is a single conditional in the same callback that
filters MV/audit/migrations relations (§13.5). No SQL primitive
wrapping required. Engaged from `bundle_invalidated` until next
bundle reload. Placement matters on PG: a downstream shim
(between `ConsumerHandle` output and the broker) would see only
successfully decoded `ChangeEvent`s and could not intercept the
`DecodeError` paths covered below. Behaviour: **drain-then-swap**.
While active: (a) underlying decoder/dispatcher advances its source
cursor (never let WAL pile up against an inactive slot — §17.6);
(b) every event is dropped without invoking the broker; (c) records
`db_schema_pending_dropped_events`. On next request the isolate
reloads, `installSchema` attaches the new `LiveSchema`, shim
disengages, broker emits synthetic `resync`s. Alternative —
version-tag every decoded row and gate in the broker — rejected
(same delivery-failure mode, more state).

**New subscribers during the window.** `subscribe(...)` while the
shim is engaged returns `Conflict { code: "schema_pending" }`
synchronously; SDK retries with bounded exponential backoff
(50ms × 2^n, cap 2s, terminal after 30s). Registration does **not**
silently queue — an AI-generated app would otherwise observe
unbounded await with no signal. Differs from §11.6's silent
"register during window, see only post-DDL events" because
schema-pending is typically <100ms (reload) while `register_model`
may run minutes — the retry budget fits the former, not the
latter. `subscriptions_active` (§17.7) does not count rejected
attempts.
<!-- Round 8: MINOR #2 / missing concept — joint-window policy -->
**Joint-window precedence.** When both `register_model` (§11.6) and
`schema_pending` are engaged on the same worker (the deploy-with-DDL
case), `schema_pending` takes precedence on `subscribe(...)` —
loud `Conflict { code: "schema_pending" }`, SDK bounded retry. The
§11.6 silent-acceptance rule applies only when `register_model` is
engaged alone. Rationale: `schema_pending` carries the stronger
signal (worker cannot correctly decode CDC for any subscriber
until reload).

**PG-side decoder error path during the window.** pgoutput frames
are typed by relation OID + column ordinal against the worker's
last `LiveSchema`. A `register_model` that renames/drops a column
races the `bundle_invalidated` rail; a frame for the renamed
column may arrive before reload. The shim wraps the decoder; any
`DecodeError` (column-not-found, type-mismatch, OID resolves to a
renamed column) raised while engaged is converted to a dropped
event tagged `schema_pending_drop`, counted against
`db_schema_pending_dropped_events` — **not** propagated to
`wal_consumer::run_supervised`'s panic path. Without this a column
rename would crash the WAL consumer and the §17.6 watchdog would
reconnect into the same stale-schema loop. Outside the window a
`DecodeError` remains fatal.
<!-- Round 8: missing concept — pre-bundle_invalidated race -->
**Pre-`bundle_invalidated` race.** When DDL on worker A produces
frames worker B sees via shared PG replication, byte-level
ordering of those frames against B's `bundle_invalidated` arrival
is not guaranteed: a frame may arrive ≤ 1× control-plane RTT
(typically ≤ 50ms; §16.7 durability rail) before its event.
Posture: a `DecodeError` in that pre-engagement gap fires the
§17.6 watchdog once; the control event lands during reconnect;
the shim engages; the re-streamed frame is dropped cleanly. The
watchdog therefore absorbs at most one reconnect per deploy per
worker — a quantum, not a loop. A "tolerant-decoder always" mode
was rejected: it would mask schema corruption outside deploys,
where `DecodeError` is the correct fail-loud signal.
<!-- Round 8: missing concept — resync trigger on disengage -->
**Disengage resync semantics.** On shim disengage the broker emits
one synthetic `resync` **per active subscription** (not one
broker-wide): each subscriber's `(collection, predicate)` read-set
(§11.8) refetches independently. `subscriptions_active` is the
exact upper bound on `resync` count per disengage. Subscriptions
rejected mid-window with `schema_pending` never enter this set —
they re-`subscribe` after SDK backoff and pick up the new schema
from a clean state.

**Durability.** Same at-least-once control-plane rail as
`route_pull` (durable queue, retry-with-backoff until each worker
acks; workers dedupe by `(app_id, model_version)`). If the rail is
degraded, fallback is bounded by **isolate LRU TTL**
(`ZEROSHIP_ISOLATE_IDLE_TTL_MS`, default 5 min): idle isolates evict
on TTL regardless of traffic, forcing reload on the next request.
Worst-case staleness with a degraded rail is therefore the TTL, not
unbounded.

---

## 17. Threat model + security

**Multi-tenant boundary.** Schema-per-app (PG); file-per-app (SQLite via
ATTACH); `validate_collection` reserved-prefix checks; SDK call-site
`app_id` scoping; runtime injection from `SharedState.env_vars["APP_ID"]`.

**SQL injection.** `quote_ident` for identifiers; parameterised binds for
values; `validate_collection` / `validate_field_name` allowlists.

**Token forgery.** PG hardening: HMAC secret in DB; SECURITY DEFINER
wrapper; nonce table with replay protection. SQLite: HMAC in Rust; env-var
secret; in-memory LRU nonce table. Dev-only.

**Backend-specific concerns.** PG: cross-tenant reach via the shared
connection pool — mitigated by schema isolation, `sanitise_app_id`, and
(deferred [I20] / §19 P6) per-app PG roles. SQLite: shared process memory;
NOT a multi-tenant production tier.

<!-- Round 4: IMPORTANT #6 — accidental collision posture -->
### 17.4 Advisory-lock attack surface
`LockScope` is per-app, never cluster-wide. `try_acquire` for
contention-tolerant sites. PG `pg_advisory_lock` keys derive from
`hashtext()` (§7.2, §10.5) over `(app_id, scope_name)` and
`(scope_purpose)`. **Adversarial.** A malicious creator cannot force
collisions against another app: `app_id` is a non-creator-
controllable UUIDv7 base62 typed_id, so the hash input is not
creator-tunable. **Accidental.** `hashtext()` is a 32-bit hash; by
the birthday bound, expected first collision on `(app_id,
"register_model")` appears around √(2^32) ≈ 65k apps. At Shopify-
scale tenancy (≥100k apps per cluster) accidental collisions are
statistically expected. Consequence is benign — one tenant briefly
waits on another's `register_model` lock (held for milliseconds).
Accepted; partitioning by 64-bit keys (PG's single-`bigint`
`pg_advisory_lock` variant) would close the window and is deferred
to §19 P6 as a tunable. Migration backfill locks heartbeat (F1
sweeper, §18 Q4).

<!-- Round 5: missing concept — per-app PG role × replication-slot owner -->
### 17.5 Per-app PG role × replication-slot ownership
Per-app PG roles (deferred [I20], §19 P6) and replication-slot
ownership do NOT overlap. A logical slot requires the `REPLICATION`
role attribute; granting it to a per-app role would let app-A
observe app-B's WAL — a multi-tenant break. The control-plane
platform role (§9.1) is the **sole owner** of every per-app slot.
The per-app role (P6) owns only the per-app schema; it cannot
list/read/drop/create slots. The WAL consumer connects under the
platform role (the only PG connection crossing the per-app trust
boundary; no client SQL runs under it). §17.6 watchdog and §17.7
step 3 both run under the platform role. §19 P6 inherits
**slot-ownership-stays-platform** as a non-negotiable.

### 17.6 Replication-slot watchdog
A dead consumer's slot retains WAL forever — the principal PG-side DoS
vector. `replication.rs`'s watchdog polls `pg_replication_slots`
(default 30s) and flags any slot with `active = false` AND
`confirmed_flush_lsn` not advancing past a threshold (default 5 min).
Reaping via `replication_ops::drop_abandoned` →
`pg_drop_replication_slot(slot_name)`; alert per reap. Watchdog is
**per control plane**, not per worker (only one cluster-wide to avoid
two control planes racing). The "consumer cancellation" step in §17.7
is courtesy: a PG replication slot is bound to the publisher, not to a
consumer process, so killing the consumer leaves the slot until
`pg_drop_replication_slot`.

### 17.7 Slot + publication lifecycle on `drop_namespace`
Called by the control plane under a control-plane-held lock.

**Active-subscription policy.** Default: drop **defers** with
`Conflict { code: "subscriptions_active", count: N }` while any
subscription on the app is registered (checked via the worker
`/internal/subscriptions/:app_id` admin endpoint). Operator override
`--force` fires `subscription_app_dropped` to every active subscriber;
the SDK surfaces it as a terminal error on the subscription iterator
for graceful shutdown. No drop step runs while a subscription is
observable.

**PG ordering.** (1) drain broker via `subscription_app_dropped`;
(2) `ChangeStream::deprovision` per worker cancels the consumer and
awaits exit — courtesy only (slot survives consumer death; §17.6);
after a 5s grace, control plane calls `pg_terminate_backend(active_pid)`
against the slot's listed backend to force the replication connection
closed; (3) `pg_drop_replication_slot(slot_name)` (requires inactive,
which step 2 guarantees; no FORCE flag exists for this builtin);
(4) `DROP PUBLICATION zs_pub_<app_id>`; (5) `DROP SCHEMA "<app_id>"
CASCADE`. Retry from step 2 on partial failure; steps 3–5 idempotent.
"Killed" in step 2 means `pg_terminate_backend`, **not** OS `SIGKILL`
on the worker process (would lose unrelated apps).

<!-- Round 4: CRITICAL #4 — reorder so DDL runs while writer is alive -->
<!-- preupdate_hook amendment: DDL teardown step drops (no triggers, no outbox tables); hook unregisters at connection close -->
**SQLite ordering** (revised). Orchestrator holds the per-app
`register_model` lock for the drop. (1) **Subscription gate.** If
any subscription is active, defer with `Conflict { code:
"subscriptions_active", count: N }`; under `--force`, fire
`subscription_app_dropped` to every active subscriber and proceed.
Writer stays alive. (2) **DETACH from peer isolates.** Control
plane signals every other worker via the existing isolate-eviction
control event (§16.6); each receiving worker's
`IsolateDbContext::drop` closes its `rusqlite::Connection` on the
doomed app, releasing the ATTACH alias it held. 5s ack grace. The
orchestrator's own `SqliteSession` is **not** torn down yet.
(3) **SqliteSession shutdown + mpsc drain.** Orchestrator sends
`Shutdown` to its `SqliteSession`; the actor finishes the current
in-flight tx, drops new commands, drains its mpsc receiver, closes
the underlying `rusqlite::Connection`. The preupdate hook
unregisters automatically when the connection closes — no DDL
teardown is needed (no triggers were ever installed; no outbox
table exists). (4) **POSIX unlink.** Unlink
`${db_dir}/zs-${app_id}.sqlite` plus `-wal` / `-shm`. If a worker
did not ack step 2 within the grace, unlink proceeds — POSIX
semantics let that worker continue writing into the freed inode
until its FD closes; those writes discard with the inode at final
close (acceptable: the app is being deleted). Idempotent: every
step is a no-op when its precondition is already met or returns
`AlreadyDone`.

**SQLite ATTACH surface.** Each isolate's `rusqlite::Connection` only
ATTACHes one app's file. Shared-cache mode is NOT enabled (§6.1,
§8.12); cross-app reach would require ATTACHing a second app's file —
which the orchestrator never does. Worker process boundary is the
trust boundary; SQLite is not a production multi-tenant backend.

---

## 18. Open questions

1. **Cross-app FK enforcement**: forbid at DDL parse time vs rely on
   schema isolation. Recommend: parse-time check added during P1 (P0 is
   the trait split; FK validation is unrelated work).
2. **`MaterializedView` refresh granularity on SQLite**: per-minute default
   with `.refresh({ everyMs: N })` override.
3. **Per-app PG role hardening** ([I20]): production-only; `hardening`
   Cargo feature; ships in §19 P6.
4. **F1 sweeper-half** ([I31]): schema migration adding `owner_session_id`
   + `last_heartbeat_at`. Recommend 60s sweep; orphan after 5× heartbeat
   interval (10s heartbeat → 50s grace); terminate to `Failed` with a
   sweeper marker.
5. **F2 terminal-state resolution** ([I32]): (a) don't write the Pending
   row on strict refusal vs (b) write + immediately terminate as
   `ValidationRefused`. Recommend (b): preserves "every row reaches a
   terminal state."
6. **Within-app RLS**: app-layer for v1; native PG RLS as
   defense-in-depth in P6.
7. **`sqlite-vec`**: `bundled` vs runtime extension load.
   **CLOSED 2026-05-23 (P4)** — neither. Shipped pure-Rust flat scan
   instead; both options conflicted with the bundled-SQLite
   invariant (§1). **RE-OPENED 2026-05-24 then CLOSED 2026-05-24** —
   swapped to sqlite-vec per user decision; see P4 PR 7 +
   `docs/proposals/p4-search-implementation-plan.md` §10
   reassessment. The earlier analysis was wrong: the `sqlite-vec`
   Rust crate statically compiles the C source and hooks every
   rusqlite connection via `sqlite3_auto_extension` — no `.so`
   ships, no amalgamation fork, the bundled invariant is preserved.
8. **SQLite pool sizing**: 1 writer + 4 readers (separate Connection
   handles in WAL mode). Independent of compio blocking-pool size — a
   separate runtime tunable (§18A).
9. **Encryption at rest for SQLite**: skip (dev tier; laptop disk
   encryption is the trust boundary). SQLCipher deferred indefinitely.

### 18A `compio::runtime::spawn_blocking` pool sizing
The compio blocking-thread pool services every `spawn_blocking` caller in
the worker, not just plugin-db. Default size `num_cpus`. SQLite calls share
this pool; saturation queues at the runtime level, not at plugin-db. Tune
via `ZEROSHIP_WORKER_BLOCKING_THREADS`; recommended floor `num_cpus * 2`
when SQLite is enabled.

---

## 19. Implementation phases

All features GA at day-1 launch. Each phase names the integration test
that closes it. Test fixtures live under
`crates/plugin-db/tests/{pg,sqlite,common}/` unless noted; SDK-side
contract tests live under `sdks/db/tests/`.

**P0** — capability trait split + `PostgresBackend` migration. Closes
[C1]. Split the 26-method trait into fifteen sub-traits; migrate every
`&PostgresBackend` to `&impl <bound>`; introduce `BackendHandle` (no `dyn
Backend`); classify every advisory-lock site under `LockScope`. Gate
(`crates/plugin-db/tests/pg/`): existing `register_model_idempotent`,
`tx_savepoint_rollback`, `subscription_fanout_basic` pass unchanged.

**P1** — `SqliteBackend` core: `SqlExecutor` + `NamespaceManager` +
`LockManager` + `DialectBuilder` + `SchemaIntrospect` + `IndexBuilder`.
New `backend/sqlite.rs`, `_session.rs`, `_dialect.rs`. Cargo feature
`sqlite` gates the module; `pg` default. Cross-app FK parse-time check
lands here (§18 Q1). Gate (`crates/plugin-db/tests/sqlite/`): every
existing PG test runs identically under `sqlite`, with documented
divergences `#[cfg]`-gated; new `cross_app_fk_rejected_at_parse`.

<!-- preupdate_hook amendment: drop trigger-DDL + outbox-table steps; add hook-registration step -->
**P2** — `SqliteBackend` reactive: `ChangeStream`. New `sqlite_cdc.rs`.
Register `Connection::preupdate_hook` once per `SqliteSession` at
open time (§11.5); hook callback owns the per-tx event buffer +
relation filter (§13.5). Gate
(`crates/plugin-db/tests/sqlite/cdc/`):
`update_publishes_change_event_with_pre_image`,
`rollback_does_not_publish`,
`insert_publishes_via_preupdate_hook`,
`mixed_ops_in_one_tx_ordered_by_buffer_index`,
`subscription_fanout_under_load`,
`mv_refresh_does_not_emit_change_events`,
`mv_refresh_emits_no_change_events_on_base_or_shadow`,
`mv_subscribe_rejected_at_sdk`,
`backfill_run_pauses_broker_and_emits_one_resync` (round-5
IMPORTANT #5 fence), `schema_pending_decoder_drops_then_resyncs`
(round-5 IMPORTANT #4 fence).

**P3** — `SqliteBackend` auth: `SessionMinter`. PG rebinds existing
`auth/session.rs` behind the trait without behavioural change. Gate:
`session_token_round_trip`, `session_replay_rejected`,
`session_grace_window` on both backends.

**P4** — vector, FTS, geo on both backends: `VectorIndex`,
`FullTextIndex`, `SpatialIndex`. SDK extends `Collection` with `.search`,
`.near`. Gate: `vector_search_returns_k_nearest`,
`fts_search_matches_substring`, `near_returns_within_radius`,
`fts_and_filter_compose`.

**P5** — encryption + backup/restore on both backends:
`EncryptedColumn`, `Backup`. Gate: `encrypted_column_round_trip`,
`deterministic_encrypted_equality_via_index` (CRITICAL #1 fence),
`randomised_encrypted_full_scan_rejected_at_sdk` (IMPORTANT #1
fence), `snapshot_restore_round_trip`,
`vacuum_into_snapshot_consistent_under_concurrent_writer`
(CRITICAL #3 fence),
`pitr_pg_only_returns_configuration_on_sqlite`.

<!-- Round 8: MINOR #5 — split P6 into P6a (correctness) + P6b (operational) -->
**P6a** — correctness hardening. F1 sweeper-half; F2 terminal-state
resolution; per-app PG role hardening ([I20], §17.5 — per-app role
MUST NOT be granted `REPLICATION`, MUST NOT have
`pg_create_logical_replication_slot` execute, and slot ownership
stays platform-side, the non-negotiable §17.5 invariant);
drop-namespace sequencing. Gate: `orphan_running_row_swept`,
`pending_row_terminates_on_strict_refusal`,
`per_app_pg_role_isolation`,
`per_app_role_cannot_create_or_read_slot` (§17.5 fence),
`sqlite_drop_namespace_runs_ddl_while_writer_alive` (CRITICAL #4
fence).

**P6b** — operational hardening. Column-encryption key rotation
(re-encryption under broker pause, §7.2 deferral; design lives at
`docs/proposals/db-column-key-rotation.md`, to be authored);
single-`bigint` `pg_advisory_lock` tunable closing the 32-bit
collision window (§17.4); `db_schema_pending_dropped_events`
alert threshold (`> 1000 over 5 min while schema_pending = true`);
observability dashboards. Gate: `column_key_rotation_round_trip`,
`advisory_lock_64bit_no_collision`, `dashboards_render_canary`.
P6a precedes P6b; both ship together at day-1 readiness.
<!-- Round 5: 64-bit advisory-lock tunable + key rotation backlog rows -->
<!-- Round 8: P6 scope split for tractability -->

---

## 20. Glossary

- **app / app_id** — creator-deployed app; UUIDv7 base62 prefix `app_`; from `SharedState.env_vars["APP_ID"]`.
- **collection** — SQL table; declared via `default.schema` (§15).
- **schema** — *(DB sense)* per-app namespace (PG schema / SQLite ATTACH file). *(declarative sense)* TS expression on `default.schema`.
- **isolate / broker** — V8 isolate running one app; in-process routing table from `(app_id, collection)` to subscribers.
- **ChangeEvent** — op + pk + new tuple + optional old tuple.
- **advisory lock / LockScope / LockGuard** — PG `pg_advisory_lock` or SQLite HashMap; `GlobalApp`/`LocalApp` classification; RAII guard from `LockManager::acquire` (replaces legacy `OrchestratorLockGuard`, renamed during P0).
- **audit row state** — `__zeroship_migrations` status; Pending → Running → terminal.
- **ATTACH database** (SQLite) — mounts the per-app file under an alias matching `app_id`; no `cache=shared`.
- **SECURITY DEFINER** — PG function-runs-with-owner-privileges; HMAC wrappers callable without holding the secret.
- **capability trait** — one of fifteen focused traits the `Backend` super-trait composes.
- **WAL consumer** (PG) / **preupdate-hook dispatcher** (SQLite) — long-running pgoutput streamer / writer-actor's per-tx event buffer flushed post-COMMIT via the `Connection::preupdate_hook` callback (§11.5). <!-- preupdate_hook amendment -->
- **pending emit** — ChangeEvent queued during an active transaction; drained on commit, cleared on rollback.
- **typed_id** — workspace-wide UUIDv7-base62 with entity prefix.
- **strictness** — per-collection DDL policy (strict/lenient/off).
- **F1 / F2** — deferred backlog (orphan Running / orphan Pending rows).
- **dialect** — per-backend SQL text differences in `DialectBuilder`.
- **read-set** — per-subscription `(collection, predicate)` for broker filtering.
- **dev tier / production tier** — SQLite (local dev, CI, preview; NOT production) / PG (only deploy option).
- **hardening** (Cargo feature) — production-only auth subtree (SECURITY DEFINER session minter, per-app PG roles in P6).
- **rows_written vs rows_written_quota_exceeded** — counter name (§13) vs typed error code (§15.7).
- **BackendHandle** — enum `{ Postgres(Rc<PostgresBackend>), Sqlite(Rc<SqliteBackend>) }`; replaces `Rc<dyn Backend>` (impossible under associated types).
- **MeteredSqlExecutor** — decorator newtype over any `SqlExecutor`; the only metering composition.
- **pid** (session token `actor_kind:actor_id:pid:nonce:expires_at`) — **project id**, typed_id of the creator's project under which this session was minted (NOT UNIX pid, NOT permission id). One project may own many apps.
- **REPLICA IDENTITY FULL** (PG) — table-level setting causing pgoutput to include the full pre-image in UPDATE/DELETE frames. Required for the §11.5 CDC pre-image promise; cost is WAL volume linear in row width.
- **hashtext** (PG) — built-in 32-bit string hash used to derive `pg_advisory_lock(int4, int4)` keys (§7.2, §17.4).
- **EncryptedColumn** — AEAD column; stored as `Bytes`/`BLOB`; per-row random nonce default; filter semantics in §7.2.
- **MV shadow table** — `__zeroship_mv_<name>` backing SQLite MaterializedView; writes are filtered out by the preupdate-hook dispatcher's relation filter (§13.5). <!-- preupdate_hook amendment -->
- **schema-pending decoder** — drain-then-swap shim attached to a worker's `ChangeStream::ConsumerHandle` between `bundle_invalidated` and next-request reload; drops decoded `ChangeEvent`s on the floor, advances the source cursor, emits one synthetic `resync` per active subscription at disengage (§16.7). <!-- Round 5 -->
- **backfill broker pause** — `Broker::suppress_app` engaged for the duration of `migrations.run` so a million-row backfill does not fan a million events to subscribers; one `resync` at resume (§10.6, §11.6). <!-- Round 5 -->
- **Deterministic-IV-via-HMAC AEAD** — `nonce = HMAC-SHA256(k_siv, plaintext)[..12]` then AES-GCM under that nonce; follows the SIV-via-PRF paradigm described in Rogaway-Shrimpton 2006 §3.2 (with HMAC-SHA256 as PRF and AES-GCM as the AEAD primitive). **Not** an instantiation of RS06's concrete SIV construction (S2V + AES-CTR), **not** RFC 5297 AES-SIV, **not** RFC 8452 AES-GCM-SIV (POLYVAL), and **not** the AWS Database Encryption SDK beacon mechanism (separate HMAC-truncate index column over randomised AES-GCM); §7.2. <!-- Round 6; round-8 attribution narrowed -->
- **slot-ownership-stays-platform** — invariant from §17.5: per-app PG roles never receive `REPLICATION`; the control-plane platform role is the sole creator/reader/dropper of every per-app replication slot. Tracked as a hard non-negotiable in the §19 P6 row. <!-- Round 5; round-6 P6-row cross-anchor -->
- **schema_pending** — `.code` returned by `subscribe(...)` while a worker's schema-pending decoder is engaged (§16.7); SDK retries with bounded backoff. Also the tag attached to a pgoutput frame the PG-side decoder drops when a column rename arrives mid-drain. <!-- Round 6 -->
- **preupdate_hook** — SQLite C API `sqlite3_preupdate_hook` (available since SQLite 3.16, 2017), exposed by rusqlite behind the `preupdate_hook` Cargo feature, compiled into the `bundled` amalgamation via `SQLITE_ENABLE_PREUPDATE_HOOK`. Fires synchronously BEFORE every rowid-table mutation with native OLD/NEW row accessors (`sqlite3_preupdate_old` / `sqlite3_preupdate_new`); the basis for SQLite CDC (§11.5). <!-- preupdate_hook amendment -->
- **`db_sqlite_preupdate_buffer_depth`** — operational gauge sampled **after each COMMIT in the writer actor** (§16.3 — not idle-driven; a continuously-busy writer never idles); steady-state 0 across consecutive samples, alerting threshold for stuck flush. <!-- preupdate_hook amendment, replaces former db_pre_image_outbox_rows -->

---

## Amendment 2026-05-24 — Masking subsystem (P5.5)

Append-only amendment. Does not alter any text above this block.

### Semantic flip

Prior to P5.5, `t.encrypted(...)` columns followed the standard
encryption-at-rest model: stored bytes were ciphertext, but reads
returned the decrypted plaintext transparently. Under P5.5, the
default read returns a `MaskedValue<T>` wrapper carrying a
pre-computed mask string — plaintext requires an explicit
`.unmask()` call (audited, authorisation-gated). The two-line
summary of WHY: AI-generated handlers leak rows wholesale through
`console.log` / `Response.json` / error messages; the
default-safe model converts that from a runtime leak into a
compile-time `tsc` error.

### Storage strategy — Path B (sibling columns)

The masked representation lives in a **sibling** `<col>_masked TEXT`
column auto-emitted at DDL time alongside the ciphertext parent.
A default read SELECT-aliases the sibling onto the parent name
(`"<col>_masked" AS "<col>"`) so the ciphertext bytes never leave
the database and the per-column AEAD key is never consulted.
Per-query unmask hints (`findOne({...}, { unmask: ["ssn"] })`)
flip the alias back to the bare parent and the standard
decrypt-on-read pass runs.

Alternative considered: Path A (compute the mask on every read).
Rejected because it forces decryption on every default read —
the exact cost we wanted to avoid — and complicates the live-query
fanout (which would have to decrypt per subscriber).

### Shipped PRs

| PR | Commit     | Scope                                                                      |
|----|------------|----------------------------------------------------------------------------|
| 1  | `49857c31` | Masking foundation: `MaskedValue<T>` types, reserved `_masked` suffix, `ColumnInfo.mask`. |
| 2  | `d8e54269` | DDL sibling-column emission + dual-write CRUD pass.                        |
| 3  | `2e866360` | Default read flipped to masked: aliased SELECT + `MaskedValue` rehydration.|
| 4  | `9e9b9a62` | `unmaskField` RPC + `__zeroship_audit_unmask` table + authorization stub.  |
| 5  | `a6ed24d3` | `defineMaskPolicy()` + per-app policy storage + real authorization.        |
| 6  | `e22e0754` | Mask backfill (PR 6a) + rewrite (PR 6b) + removal (PR 6c) under strictness.|
| 7  | `e08adb44` | Drift detection cron + bulk unmask + per-query unmask hint.                |
| 8  | _this PR_  | `zeroship migrate scan-mask-usage` CLI + creator-facing docs + closeout.   |

### Read this next

For the full design — the eight mask kinds, the six classifications,
the Path A vs B trade-off, Q-MASK-A through Q-MASK-M, the risk
analysis — see `docs/proposals/sensitive-field-masking.md`.
For creator docs see the new "Masking" section in
`docs/reference/db.md` and the migration walkthrough in
`docs/reference/migration/p5-to-masked-decrypt.md`.

### Deferred follow-ups

- **AAD version binding (wire flag 0x01 → 0x02)**: the AEAD wire
  format gains a version byte in the AAD so a per-row `version`
  column binds the ciphertext to the row's CAS guard. This needs a
  `version` column on every row, which depends on P7 (universal
  versioning) — deferred to **P7.5** once the prerequisite lands.
- **P6+ drift dashboard surface**: the drift-detection cron writes
  to `__zeroship_audit_mask_drift`; the operator dashboard wire
  lands with the P6 control-plane drift surface.
- **Per-collection mask policies** (Q-MASK-H): app-level is
  default; per-collection is a refinement deferred to P9+.

---

## Amendment 2026-05-24 — Platform system fields (P7)

Append-only amendment. Does not alter any text above this block.

### What landed

Every creator table is now prepended with seven platform-managed
columns at CREATE TABLE time: `id`, `created_at`, `updated_at`,
`created_by`, `updated_by`, `version`, `deleted_at`. The names are
reserved (`code: "reserved_field_name"` at deploy time if a creator
declares one). Three auto-indexes ride along: `deleted_at` (the
soft-delete hot path), `updated_at` (CDC subscriber resume), and
`created_by` ("my items" queries). `delete()` shifts from hard-delete
to soft-delete (sets `deleted_at = NOW()`, bumps `version`); the
hard-delete escape is the new `purge()` method; `restore()` clears
`deleted_at` for previously soft-deleted rows. `find()` and every
other read-side method auto-filter `WHERE deleted_at IS NULL` unless
the native opt `include_deleted: true` is threaded through. UPDATE
auto-bumps `version` by 1 and stamps `updated_at = NOW()` on every
call; passing `version: N` in the update filter turns the call into
an optimistic-concurrency check (`code: "version_mismatch"` from the
runtime, rethrown as `OptimisticLockError` by the SDK).

### Shipped PRs

| PR | Commit     | Scope                                                                      |
|----|------------|----------------------------------------------------------------------------|
| 1  | `8ec76868` | Schema DSL + reserved-name validator for the 7 platform system fields.     |
| 2  | `8f9f1e6e` | CREATE TABLE prepends 7 system fields + 3 auto-indexes (PG + SQLite).      |
| 3  | `bf1cce58` | INSERT auto-populates system fields + `id:string` cascade + FK type fix.   |
| 4  | `8a296728` | UPDATE auto-bumps `version` + `updated_at` + optimistic concurrency.       |
| 5  | `c38ff4de` | `delete()` becomes soft-delete; add `purge()` + `restore()`; `find()` auto-filters `deleted_at`. |
| 6  | —          | **Deferred (pre-launch).** Existing-table migration cancelled mid-flight; see below. |
| 7  | _this PR_  | Docs (`db.md` system-fields section) + this amendment block.               |

### Read this next

For the full proposal — motivation, field-by-field semantics,
Q-SF-A through Q-SF-J, the riskiest-decision analysis on `delete()`
behaviour — see `docs/proposals/platform-system-fields.md`. The
creator-facing surface is documented in the new "System fields"
section in `docs/reference/db.md`.

### Pre-launch reality check (2026-05-24)

PR 6 (the one-time `ALTER TABLE … ADD COLUMN` pass for legacy
tables that existed before PR 2 landed) is **deferred indefinitely**.
The platform is pre-launch as of 2026-05-24: never published, no
production users, no production tables. There is no population for
the migration to migrate. PR 5's Path C (detect-and-warn for tables
without `deleted_at`) is the safety net for legacy tables that
materialise in dev/test environments. A real PR 6 designed against
real schema-evolution data lands when there are production schemas
to evolve against.

### Deferred follow-ups

- **P7.5 — AAD version binding (wire flag 0x01 → 0x02)**: pending.
  Now unblocked — every row carries `version` (PR 2 + PR 4), and
  the P5 wire format already reserved `0x02` for this upgrade. The
  P7.5 PR extends `canonical_aad` to bind `version_bytes`, bumps the
  ciphertext header flag to `0x02`, and lands rolling re-encrypt-on-
  write. Pre-launch posture means no `0x01` production ciphertext
  exists, so the cutover can ship without a backward-compat read
  path (operator decision, post-launch).
- **PR 6 (existing-table migration)**: deferred indefinitely (above).
- **Pre-launch simplification**: PR 5's Path C legacy-warn arm and
  P5.5 PR 8's `scan-mask-usage` CLI are dead-code-in-practice given
  the pre-launch posture (no creator code to scan, no legacy tables
  to warn about). A future "pre-launch simplification" PR can rip
  them out; not in scope for the current cycle.
