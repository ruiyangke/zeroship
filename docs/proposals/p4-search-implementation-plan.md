# P4 — Vector + Full-Text + Spatial — Implementation Plan

**Status**: design (planning only — no Rust written).
**Scope**: §19 P4 of `docs/proposals/db-system-design.md`. Three capability traits, two backends each, SDK extension, four test gates.
**Prereqs landed**: P0 (capability split), P1 (SQLite core), P2 (CDC), P3 (auth) — 10 capability traits at HEAD `b2a820fa`.

---

## 0. Source-code inventory

| Surface | At HEAD | What P4 changes |
| --- | --- | --- |
| `backend/mod.rs` | 10 capability traits + `BackendHandle = Postgres \| Sqlite` enum | Add 3 traits: `VectorIndex`, `FullTextIndex`, `SpatialIndex`. NOT added to `Backend` super-trait. 3 `BackendHandle::as_*_handle()` accessors. |
| `backend/postgres.rs` | Implements every prior trait | Append 3 impl blocks wrapping pgvector / tsvector+GIN / PostGIS. |
| `backend/sqlite/{cdc,session,dialect,lock,session_minter,error}.rs` | Existing sub-modules | NEW `vector.rs` / `fts.rs` / `spatial.rs`. `dialect.rs` gains arms for `vector`/`geoPoint`. |
| `query.rs` | `IndexSpec`, `build_create_indexes`, `$search` at L2005 (currently unused at runtime) | Add `IndexKind` enum on `IndexSpec`; teach builder about vector/FTS/geo fields. Add `$near` op. |
| `diff.rs::ColumnInfo` | `pg_type`, `not_null`, `default_*` | Add `vector_dims: Option<i32>`, `is_fts_source: bool`, `is_geopoint: bool`. |
| `v8_classes/collection.rs` | 13 `#[v8_method]`s | Add `search`, `near`, `searchIndexes`. |
| `crud.rs` | `dispatch_*` family | Add `dispatch_search`, `dispatch_near`. |
| `sdks/db/src/types.ts` | `t.string/number/...` | Add `t.vector(dims, opts?)`, `t.geoPoint()`. Add `.fts(language?)` modifier on TypeBuilder. |
| `sdks/db/src/collection.ts` | Promise-returning methods | Add `.search(args)` (discriminated `vector | text`) and `.near(args)`. |
| PG extensions | No `CREATE EXTENSION` anywhere in code | Document operator dependency in `docs/reference/db.md`; add startup probe in PG backend surfacing typed `vector_extension_missing` / `postgis_extension_missing`. |

**No Cargo change on PG side.** `compio-postgres` round-trips pgvector/PostGIS bytes as `BYTEA`/WKB.

**One new SQLite dep (gated to `sqlite` feature)**:
- `sqlite-vec = "0.1"` — vec0 virtual table, statically compiled into the binary (preserves bundled-SQLite invariant; registers via `rusqlite::ffi::sqlite3_auto_extension` once at session boot). See §10 reassessment.
- No FTS5 toggle needed — rusqlite's `bundled` ships `SQLITE_ENABLE_FTS5` by default.

**Earlier `bytemuck` dep retired** — the pure-Rust BLOB round-trip is gone; vec0 owns the `float[N]` storage natively.

---

## 1. File structure

```
crates/plugin-db/src/
  backend/
    mod.rs                          (+3 traits, +3 BackendHandle accessors)
    postgres.rs                     (+3 impl blocks)
    sqlite/
      mod.rs                        (+3 impl blocks)
      vector.rs            (NEW)    sqlite-vec auto-extension wiring + vec0 vtable provisioning + MATCH search builder
      fts.rs               (NEW)    FTS5 vtable lifecycle + MATCH query builder
      spatial.rs           (NEW)    haversine within-radius post-filter
      dialect.rs                    (+ map_zs_type arms for vector/geoPoint/fts)
      error.rs                      (+ dimension_mismatch, fts_table_missing)
  query.rs                          (+ IndexKind enum on IndexSpec)
  diff.rs                           (+ 3 ColumnInfo fields)
  crud.rs                           (+ dispatch_search, dispatch_near)
  v8_classes/collection.rs          (+ 3 #[v8_method])

sdks/db/src/
  types.ts                          (+ t.vector / t.geoPoint / .fts())
  collection.ts                     (+ .search / .near)
  validate.ts                       (+ vector dim check, geoPoint shape check)
```

---

## 2. Trait declarations (`backend/mod.rs`)

Three new traits. Not added to `Backend` super-trait; opt-in via `BackendHandle::as_*_handle()`.

```rust
pub trait VectorIndex: 'static {
    #[allow(async_fn_in_trait)]
    async fn ensure_vector_index(&self, app_id, collection, column, dims: i32, metric: VectorMetric) -> Result<(), DbError>;
    #[allow(async_fn_in_trait)]
    async fn vector_search(&self, app_id, collection, column, query: &[f32], k: usize, metric: VectorMetric, filter: &Value) -> Result<Vec<Value>, DbError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMetric { Cosine, L2, InnerProduct }

pub trait FullTextIndex: 'static {
    #[allow(async_fn_in_trait)]
    async fn ensure_fts_index(&self, app_id, collection, columns: &[String], language: &str) -> Result<(), DbError>;
    #[allow(async_fn_in_trait)]
    async fn fts_search(&self, app_id, collection, query: &str, filter: &Value, limit: Option<usize>) -> Result<Vec<Value>, DbError>;
}

pub trait SpatialIndex: 'static {
    #[allow(async_fn_in_trait)]
    async fn ensure_spatial_index(&self, app_id, collection, column) -> Result<(), DbError>;
    #[allow(async_fn_in_trait)]
    async fn spatial_near(&self, app_id, collection, column, point: GeoPoint, radius_m: f64, filter: &Value, limit: Option<usize>) -> Result<Vec<Value>, DbError>;
}

#[derive(Debug, Clone, Copy)]
pub struct GeoPoint { pub lat: f64, pub lng: f64 }
```

**`IndexSpec` extension**: add `kind: IndexKind` where `IndexKind = BTree (default) | Vector{dims, metric} | Fts{language} | Spatial`. P0-P3 callers unchanged (default).

**`ColumnInfo` extension**: 3 new fields per §0 table.

---

## 3. PG impl

### 3.1 `impl VectorIndex for PostgresBackend`
- `ensure_vector_index`: `CREATE INDEX CONCURRENTLY IF NOT EXISTS ... USING ivfflat ("col" vector_cosine_ops) WITH (lists = 100)`. Opclass varies by metric. Route through `create_index_with_recovery_audited` — free retry+audit.
- `vector_search`: `SELECT *, "col" <-> $1 AS _distance FROM ... WHERE <filter> ORDER BY "col" <-> $1 LIMIT $2`. Bind query as `'[...]'::vector` text cast.
- **Extension probe**: cached `RefCell<Option<bool>>` on PostgresBackend; `vector_extension_missing` typed error.

### 3.2 `impl FullTextIndex for PostgresBackend`
- `ensure_fts_index`: 4 idempotent statements — add `"__fts" tsvector` column, backfill via existing B1 backfill rail, `CREATE INDEX CONCURRENTLY ... USING GIN ("__fts")`, `CREATE TRIGGER ... tsvector_update_trigger(...)`.
- `fts_search`: `... WHERE "__fts" @@ plainto_tsquery('lang', $1) ORDER BY ts_rank(...) DESC LIMIT $2`.

### 3.3 `impl SpatialIndex for PostgresBackend`
- `ensure_spatial_index`: `CREATE INDEX CONCURRENTLY ... USING GIST ("col")`. Column type `geography(POINT, 4326)` emitted at table-DDL time.
- `spatial_near`: `... WHERE ST_DWithin("col", ST_MakePoint($1,$2)::geography, $3) ORDER BY _distance_m LIMIT $4`. Note: `ST_MakePoint` takes `(lng, lat)`.
- **Extension probe**: same shape as vector.

---

## 4. SQLite impl

### 4.1 `backend/sqlite/vector.rs` — sqlite-vec vec0 vtable

**Decision: ship sqlite-vec via the published Rust crate (2026-05-24 reassessment; see §10).** The crate compiles the C source statically and registers via `rusqlite::ffi::sqlite3_auto_extension` once at session boot — no `.so` shipping, no amalgamation fork. Preserves the bundled-SQLite invariant.

- **Init**: `unsafe { sqlite3_auto_extension(Some(transmute(sqlite3_vec_init))) }` guarded by `std::sync::Once`; called at the top of the worker-thread closure BEFORE `Connection::open`. Subsequent `Connection::open` calls in the same process pick up vec0 automatically.
- **Storage**: vec0 owns the `float[N]` column type with native dimension validation.
- **`ensure_vector_index`** (5 idempotent statements): probe `sqlite_master` → `CREATE VIRTUAL TABLE IF NOT EXISTS "<app>"."<coll>__vec_<col>" USING vec0(<col> float[<dims>] distance_metric=<cosine|l2>)` → initial population `INSERT INTO ... SELECT rowid, <col> FROM <coll>` → AFTER INSERT/DELETE/UPDATE OF triggers mirror rowid + vector into the vtable (body references the vec0 table UNQUALIFIED per SQLite's trigger-body schema-qualifier rule).
- **`vector_search`**: `SELECT t.*, v.distance AS _distance FROM <coll> t JOIN <coll>__vec_<col> v ON t.rowid = v.rowid WHERE v.<col> MATCH x'<hex query>' AND k = <N> [AND <filter>] ORDER BY v.distance`. Query vector inlined as hex BLOB literal `x'…'` so the session actor's text-only `&[&str]` channel needs no binary-bind extension.
- **Metric divergence from PG**: vec0 supports `distance_metric=cosine` and `distance_metric=l2` only. **Inner product (`VectorMetric::InnerProduct`) is REJECTED on SQLite** with typed `DbError::Configuration { code: "vector_unsupported_metric" }`. PG continues to support all three via pgvector opclasses. SDK callers requesting inner-product on SQLite get a clear typed error pointing at cosine (mathematically equivalent for normalised embeddings up to a known transform) or PG.

### 4.2 `backend/sqlite/fts.rs` — FTS5 virtual tables
- `CREATE VIRTUAL TABLE IF NOT EXISTS "<app>"."<coll>__fts" USING fts5("col1","col2", content="<coll>", content_rowid="rowid")` — external-content vtable (no doubled storage).
- 3 AFTER triggers (INSERT/UPDATE/DELETE) on the base collection mirror writes.
- Initial population via SELECT INTO.
- `fts_search`: `... JOIN "<coll>__fts" f ON t.rowid = f.rowid WHERE f."<coll>__fts" MATCH ? AND <filter> ORDER BY bm25(...) LIMIT ?`.

### 4.3 `backend/sqlite/spatial.rs` — haversine
- Storage: `BLOB` packed `(lat, lng)` as 2× little-endian f64 = 16 bytes. CHECK: `length = 16`.
- Algorithm: full scan + haversine (~30 LOC of trig).
- `ensure_spatial_index`: no-op.
- Polygon ops PG-only; SQLite `spatial_near` with polygon input → `Configuration { code: "polygon_ops_pg_only" }`.

### 4.4 `dialect.rs` arms
```
"vector"   => "BLOB"  (CHECK constraint added by query.rs column_ddl)
"geoPoint" => "BLOB"  (16-byte CHECK)
"fts"      => "TEXT"  (FTS marker on text column; no separate type)
```

### 4.5 Schema introspection
- `vector_dims`: regex `length\("(\w+)"\)\s*=\s*4\s*\*\s*(\d+)` from `sqlite_master.sql`.
- `is_fts_source`: presence of `<coll>__fts` virtual table.
- `is_geopoint`: regex `length\("(\w+)"\)\s*=\s*16` + BLOB type.

Fragile (regex-on-DDL); sidecar `__zs_schema_meta` table is the upgrade path (Q-P4-A).

---

## 5. Schema DSL — SDK additions

```typescript
t.vector(dims: number, opts?: { metric?: "cosine" | "l2" | "innerProduct" })
t.geoPoint()    // returns TypeBuilder<{lat: number, lng: number}>
t.string().fts(language?: string)   // modifier; FTS as flag on text column, not column type
```

**`FieldDef` extension**: `vectorDims`, `vectorMetric`, `fts`, `ftsLanguage`.

**`installSchema` flow**: collects vector/fts/geoPoint markers, passes through to `registerModel`. Rust `register_model::apply` Pass 2 dispatches on `IndexKind` — Vector → `VectorIndex::ensure_vector_index`, Fts → `FullTextIndex::ensure_fts_index` (one composite per collection), Spatial → `SpatialIndex::ensure_spatial_index`, BTree → existing path.

---

## 6. SDK runtime surface

```typescript
Collection.search(args: { vector, k?, metric?, column?, filter? } | { text, limit?, filter? })
  -> (Row<S> & { _distance?: number; _rank?: number })[]

Collection.near(args: { field, point, radius, filter?, limit? })
  -> (Row<S> & { _distance_m: number })[]
```

Rust side: 3 new `#[v8_method]` on `Collection`. `dispatch_search` inspects args for `vector` vs `text` discriminator; `dispatch_near` routes to `SpatialIndex`.

---

## 7. Test gates (per design §19 P4)

| Gate | Shape |
|---|---|
| `vector_search_returns_k_nearest` | 100 rows × 128-d unit vectors; query returns top-10 by cosine. Assert set, not strict order (FP determinism not promised). |
| `fts_search_matches_substring` | 5 rows with "rust"/"async"/"rust async"/etc.; assert membership set across backends. |
| `near_returns_within_radius` | 10 points near London; assert membership equality. |
| `fts_and_filter_compose` | 5 rows; FTS+filter intersection returns exactly 2. |

**Bonus**: `vector_dimension_mismatch_rejected_at_insert`, `pgvector_extension_missing_reports_typed_error`, `postgis_extension_missing_reports_typed_error`, `vector_search_respects_filter`, `fts_trigger_keeps_index_in_sync_after_update`.

---

## 8. Commit sequence — 6 PRs

### PR 1 — Architect (trait declarations + IndexKind + diff fields)
- 3 traits + `VectorMetric` + `GeoPoint` + 3 `BackendHandle` accessors.
- `IndexSpec::kind` with `#[default]` BTree.
- `ColumnInfo` 3 new fields (default false/None).
- No impls (`Configuration { code: "p4_pr2_stub" }` for both backends).
- Gate: workspace builds clean; existing tests unchanged.

### PR 2 — PG VectorIndex impl + extension probe
- `impl VectorIndex for PostgresBackend`; cached extension probe.
- `dispatch_search` routes `{vector, k}`; `.search()` JS method.
- `t.vector()` SDK builder + validate.
- `register_model::apply` dispatch on `IndexKind::Vector`.
- Gate: `vector_search_returns_k_nearest` (PG), `pgvector_extension_missing_reports_typed_error`.

### PR 3 — PG FullTextIndex + SpatialIndex
- `impl FullTextIndex` + `impl SpatialIndex` on PG.
- `.fts()` modifier + `t.geoPoint()` builder.
- PostGIS probe.
- Gate: `fts_search_matches_substring` (PG), `near_returns_within_radius` (PG), `fts_and_filter_compose` (PG), `postgis_extension_missing_reports_typed_error`.

### PR 4 — SQLite VectorIndex impl (later superseded by PR 7)
- Originally landed pure-Rust flat scan over `BLOB` with `bytemuck` BLOB round-trip + CHECK constraint at column-DDL.
- **Superseded by PR 7 (2026-05-24)**: swapped to sqlite-vec `vec0` extension; `bytemuck` dep retired; CHECK constraint removed (vec0 owns dim validation). See §4.1 for the current shape.
- Gate (unchanged): `vector_search_returns_k_nearest` (SQLite), `vector_dimension_mismatch_rejected_at_insert`. Both pass against the new vec0 path byte-for-byte.

### PR 5 — SQLite FullTextIndex + SpatialIndex
- `backend/sqlite/fts.rs` (~250 LOC) + `spatial.rs` (~100 LOC).
- `dialect.rs` arms for `geoPoint`.
- AFTER-trigger order verified vs preupdate hook.
- Schema introspect regex-based vector dims + FTS source detection.
- Gate: `fts_search_matches_substring` (SQLite), `near_returns_within_radius` (SQLite), `fts_and_filter_compose` (SQLite), `fts_trigger_keeps_index_in_sync_after_update`.

### PR 6 — SDK polish + docs + cross-backend equivalence
- Full `.search()` / `.near()` TS API with `Row<S> & { _distance?, _rank?, _distance_m? }`.
- `docs/reference/db.md`: Vector / FTS / Geo section with extension dependency table, dim limits, FTS query syntax differences.
- Cross-backend equivalence test (membership only, not ranking).
- Operator dependency docs in `docs/runbooks/docker-compose.md` (postgres image swap to `pgvector/pgvector:pg16`).
- **P4 COMPLETE**.

---

## 9. Critical details

- **Determinism**: FP cosine differs in low-significand bits between PG (C) and SQLite (Rust). Tests assert top-k set membership, not ordinal positions for k > 5.
- **Dim limits**: ≤16000 (pgvector hard ceiling). SDK validate rejects higher.
- **FTS language**: PG honours `ftsLanguage`; SQLite FTS5 default tokenizer is language-agnostic Unicode.
- **CDC interaction**: no wire format change. Vector/FTS/geo writes fire preupdate hook with their BLOB payload like any other column.
- **No CDC subscription for `.search()`/`.near()` in P4**. Point-in-time only.
- **Encryption interaction**: vector/FTS/geo columns are NOT encrypted (P5 skips them).

---

## 10. Riskiest decision (Q-P4-D) — REVERSED 2026-05-24

**Current resolution: ship sqlite-vec.** See the 2026-05-24 reassessment below for the full chain; the rest of this section is historical context.

**Original framing (PRs 1-6)**: pure-Rust flat scan over `BLOB` + `bytemuck::cast_slice`. Rationale: claimed sqlite-vec would require forking the SQLite amalgamation per platform OR shipping a per-platform `.so`, both of which would compromise the bundled-SQLite invariant from design §1.

**That analysis was wrong** — see reassessment below. PR 7 (commit `0a892807`, 2026-05-24) retired the pure-Rust impl.

### 2026-05-24 reassessment

Original analysis claimed sqlite-vec would fork the amalgamation per platform. **This was wrong**: the `sqlite-vec` Rust crate compiles C statically and uses `sqlite3_auto_extension` (rusqlite's `ffi` module re-exports the symbol; the hook fires for every subsequent `sqlite3_open*` call in the process, so registering once at first `SqliteSession::open` is enough). No `.so` shipping. Bundled invariant preserved.

**Decision reversed: ship sqlite-vec.** P4 PR 7 swaps the impl; pure-Rust flat scan retired. The crate is at 1.6M downloads, max stable `0.1.9`, last released 2026-05-18. vec0 gives native dimension validation, SIMD distance, and `MATCH` query syntax. Inner-product metric is rejected at SQLite (vec0 supports cosine + L2 only); PG continues to support all three via pgvector opclasses. Net code surface is **smaller** than the pure-Rust impl (the C extension owns the distance math + dim check that previously lived in `backend/sqlite/vector.rs`).

---

## 11. Open questions

| # | Question | Default |
|---|---|---|
| Q-P4-A | Sidecar `__zs_schema_meta` vs regex-parse `sqlite_master.sql`? | Regex for dev tier; sidecar is upgrade path. |
| Q-P4-B | `.fts()` modifier vs `t.fts()` collection-level builder? | `.fts()` per-field modifier; one composite FTS index per collection. |
| Q-P4-C | Polygon ops in P4? | PG-only deferred; P4 ships only `near` (point + radius). |
| Q-P4-D | **Riskiest** — pure-Rust vector vs `sqlite-vec`. | **sqlite-vec** (reversed 2026-05-24; see §10 reassessment). Statically compiled via Rust crate; preserves bundled-SQLite invariant; inner-product metric rejected on SQLite (cosine + L2 only). |
| Q-P4-E | Vector dim migration — DROP+RECREATE vs ALTER? | DROP+RECREATE; refuse under strict (destructive change). |
| Q-P4-F | FTS5 trigger order vs CDC preupdate hook — racy? | Not racy; preupdate BEFORE, AFTER triggers after; broker sees both at COMMIT. |
| Q-P4-G | Document pgvector + PostGIS production prerequisite? | Yes — `docs/runbooks/docker-compose.md` image swap to `pgvector/pgvector:pg16`. |
| Q-P4-H | FTS5 external-content vtable DELETE doesn't auto-clean? | AFTER DELETE trigger emits explicit DELETE on `__fts`. |
| Q-P4-I | `_distance` / `_rank` synthetic column name clash? | Prefix `_` reserved; SDK validate rejects `_`-prefixed user columns. |
| Q-P4-J | Cross-app FK extends to vector/geo? | No — vector/geo not FK-eligible; SDK validate rejects. |
