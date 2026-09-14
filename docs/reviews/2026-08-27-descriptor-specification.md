# The runtime descriptor specification: every schema fact the data plane reads

Date: 2026-08-27. Tree: `.worktrees/dbbind-impl`, HEAD `cca4e3553`.
Status: historical survey with current assignment and column-role corrections.
The remaining sketches describe the reviewed tree, not the current wire format.
Use [Column assignments](../reference/db.md#column-assignments) and the
[ORM architecture](../architecture/data-orm.md) for the implemented contract.

> **Four cited files have since been removed, by the very work this document
> specified (checked 2026-08-29).** Each line below names one and says so, which
> is the convention `tests/doc_citation_gate.sh` enforces - a citation may name a
> file that no longer exists only if it states that outright.
>
> - `crates/zeroship-data-v8/src/audit.rs` - DELETED with the data plane's DDL.
> - `crates/zeroship-data-v8/src/crud/introspect_schema.rs` - DELETED when the
>   descriptor became the sole schema authority.
> - `crates/zeroship-data-v8/src/live_metadata.rs` - DELETED with the above.
> - `crates/zeroship-migrate-adapter/src/platform.rs` - DELETED with that whole
>   crate.
>
> Files of the same basename exist elsewhere - `crates/zeroship-auth/src/audit.rs`
> and `crates/zeroship-core/src/auth_provider/platform.rs` - and are NOT the same
> code. Do not follow them. The enumeration is kept as the record of what the
> data plane read on its own date.
>
> **Citation convention:** an unqualified path such as `backend/sqlite/vector.rs`
> or `v8_classes/transaction.rs` is relative to `crates/zeroship-data-v8/src/`,
> which is the crate this document enumerates. Anything outside that crate is
> written repo-relative from the first segment.

## What this document is

The operator has decided that the runtime **descriptor** is the sole authority
for schema and that the data plane performs **no live introspection at all** -
not even Hibernate-style DDL validation, which is deferred.

For that decision to be implementable, the descriptor must carry everything the
data plane currently learns from anywhere other than the request itself. This
document enumerates those facts, classifies each one, specifies the descriptor
shape that serves them, and names what breaks.

Every claim carries `file:line`. Where a claim came from a delegated read rather
than my own, it is marked `[delegated]` and the delegation's coverage statement
is reproduced in section 7.

---

## 0. Executive summary

**Bucket B is empty of schema facts.** Nothing the data plane learns about a
collection's shape requires reading the live database. Three findings drive that
conclusion, and each is stronger than "the code could be rewritten":

1. **The data plane already has two schema sources and they disagree in
   coverage.** The SQL *builders* read the DECLARED cache
   (`crate::context::with(|c| c.schema_for(..))`, 17 sites, listed in section 1.1); the
   read/write *pipelines* read the INTROSPECTED cache (`runtime_schema_for`, 4
   production sites, section 1.2). The introspected source is strictly poorer: its
   builder emits only `{ type, encrypted?, mask? }`
   (`crates/zeroship-data-v8/src/crud/introspect_schema.rs`) and its
   type mapper cannot produce the `vector` or `geoPoint` tokens at all
   (`introspect_schema.rs:329-353`), which two live consumers require
   (`crates/zeroship-data-v8/src/crud/mod.rs`,
   `crud/mod.rs:2609-2621`). The declared source is the richer one, and it is
   already fed from the descriptor on the `.zship` path
   (`crates/zeroship-data-v8/src/register_model/mod.rs`).

2. **On SQLite, "descriptor is the sole authority" is already the shipped
   behaviour.** `runtime_schema_for` has no SQLite introspector; it falls back
   to the declared cache (`introspect_schema.rs:104-142`, fallback at
   `:262-264`). Every mask, encryption and coercion decision on the dev tier is
   already made from the descriptor today.

3. **Live introspection cannot recover facts the DSL knows.** The module's own
   doc admits the logical-type collapse (`introspect_schema.rs:29-43`), and the
   SQLite arm's `ColumnInfo.vector_dims` / `is_geopoint` are documented as
   populated but are left at `Default` with no regex in the tree
   (`crates/zeroship-data-v8/src/backend/sqlite/mod.rs`)
   `[delegated]`. Introspection is the lossy path, not the authoritative one.

The only live-database reads that survive in the data plane after the descriptor
lands are **two extension-presence probes** (`pg_extension`), which are
provisioning facts rather than schema facts. They are handled in section 2.B.

Two facts are **bucket C** (unclear, with a named question): SQLite physical
column ORDER (needed by CDC), and the naming-alias resolution in `unmask`.

The historical gaps in soft deletion and versioning are resolved through declared
column roles; section 1.6 records the current behavior and the separate
`strictness` limitation.

### One measured correction to the design document

`docs/proposals/2026-08-26-runtime-db-binding-design.md:660` says "19 direct
thread-local `schema_for` reads ... including six inside the concrete backends'
search paths."

Measured at HEAD:

```
grep -rn "\.schema_for(" crates/zeroship-data-v8/src --include="*.rs" | wc -l
17
```

**17, not 19**, and **four** in the backends' search paths
(`backend/postgres.rs:571`, `:703`; `backend/sqlite/mod.rs:1846`, `:1929`), not
six. The count reaches six only if `backend/sqlite/vector.rs:121` and
`backend/sqlite/mod.rs:1876` are counted, which are *forwards* of a hint read
elsewhere, not reads. The full 17 are listed in section 1.1; the discrepancy does not
change any conclusion, but the design's replacement plan is keyed to that number
and should be re-keyed.

---

## 1. Enumeration: every data-plane consumer of a schema fact

### 1.1 The DECLARED-schema cache (`ThreadDbContext::schema_for`) - 17 readers

Definition: `crates/zeroship-data-v8/src/context.rs`. A
`HashMap<"{app_id}:{collection}", serde_json::Value>` holding the raw JSON the
SDK declared. Written only by `cache_schema` (`context.rs:672-680`), called only
from `register_model_dispatch` (`register_model/mod.rs:102-104`).

On the `.zship` path the value it caches **originates from the bundled
`RuntimeSchemaDescriptor`** injected as `globalThis.__zsRuntimeDescriptor`, off
which `installSchema` runs the `registerModel` chain
(`register_model/mod.rs:176-182`). So 17 of these readers are *already* reading
descriptor-derived data; what changes is the plumbing, not the content.

| # | file:line | Function | Fact | What it does with it |
|---|---|---|---|---|
| 1 | `crud/mod.rs:188` | `maybe_lower_sqlite_boolean_doc` | `def.type == "boolean"` per field | SQLite only: rewrites `true`/`false` in an INSERT doc to `1`/`0` (`crud/mod.rs:241-253`, `:330-334`) |
| 2 | `crud/mod.rs:202` | `maybe_lower_sqlite_boolean_docs` | same | same, over an `insertMany` array |
| 3 | `crud/mod.rs:221` | `maybe_lower_sqlite_boolean_update` | same | same, over `$set` and bare update keys (`crud/mod.rs:255-279`) |
| 4 | `crud/mod.rs:235` | `maybe_lower_sqlite_boolean_filter` | same | same, recursively through `$and`/`$or`/`$not` and the comparison operators (`crud/mod.rs:281-328`) |
| 5 | `crud/mod.rs:682` | `dispatch_find` | whole field map | passed as `schema_hint` into `build_find_with_schema_and_unmask_and_soft_delete_with_dialect` (`crud/mod.rs:687-699`). Drives the masked-aware SELECT and the read-identifier allowlist |
| 6 | `crud/mod.rs:1801` | `dispatch_aggregate` | whole field map | `schema_hint` into `build_aggregate_with_soft_delete_with_dialect` - masked sibling substitution in `$group.by` / `$sum` / `$sort` (`zeroship-schema/src/query.rs:3394-3422`) |
| 7 | `crud/mod.rs:1869` | `dispatch_distinct` | `column_is_masked(field, ..)` plus the whole map | two effects: `build_distinct_with_soft_delete_with_dialect` aliases the sibling, and `distinct_reads_masked_sibling` **turns decryption off** for the read pipeline (`crud/mod.rs:1870`, `:1892`) |
| 8 | `crud/unmask.rs:126` | `resolve_schema_column` | the declared field-KEY SET | resolves a caller-supplied column name to a canonical one by trying the name, then snake_case, then camelCase (`unmask.rs:132-143`) |
| 9 | `crud/unmask.rs:157` | `lookup_mask_meta` | `def.mask.kind`, `def.mask.classification` | `kind == "none"` -> refuse with `unmask_column_not_masked`; otherwise the classification drives the authorization check (`unmask.rs:166-187`, `:305-323`) |
| 10 | `crud/unmask.rs:216` | `lookup_encryption_meta` | `def.encrypted.{mode,keyId,wraps}` | selects AAD shape, key, and the deserialisation of the recovered plaintext (`unmask.rs:228-252`) |
| 11 | `crud/mask_drift.rs:127` | `run_drift_check_for_column` | `def.mask.kind` + `def.encrypted` | recomputes the expected mask from the parent and diffs it against the stored sibling (`mask_drift.rs:130-216`) |
| 12 | `crates/zeroship-data-orm/src/crud/assignment_pass.rs` | `prefix_for_collection` | the assigned field's `idPrefix` | uses the declared prefix or derives one from the collection; validates either result |
| 13 | `crud/introspect_schema.rs:263` | `sqlite_fallback_schema` | whole field map | **this is the SQLite arm's entire schema source** - the "introspected" schema on SQLite *is* the declared one |
| 14 | `backend/postgres.rs:571` | `PostgresBackend::vector_search` | whole field map | `schema_hint` into `build_vector_search`; drives `validate_read_identifier` and the masked-aware projection `[delegated]` |
| 15 | `backend/postgres.rs:703` | `PostgresBackend::spatial_near` | whole field map | same, into `build_spatial_near` `[delegated]` |
| 16 | `backend/sqlite/mod.rs:1846` | `SqliteBackend::vector_search` | whole field map | forwarded to `vector::build_vector_search_sql` -> `build_masked_aware_select_expr_for_table_alias` (`backend/sqlite/vector.rs:121`) `[delegated]` |
| 17 | `backend/sqlite/mod.rs:1929` | `SqliteBackend::spatial_near` | whole field map | forwarded to `build_spatial_near_base_query` -> `build_find_with_schema` (`backend/sqlite/mod.rs:1862-1879`) `[delegated]` |

Plus one **collection-set enumeration**, which is not a per-collection read:

| # | file:line | Function | Fact | What it does with it |
|---|---|---|---|---|
| 18 | `context.rs:817` via `v8_classes/transaction.rs:78` | `mint_tx_view` | the NAME SET of every collection cached for this app | mints one `Collection` V8 object per name onto the `tx` view. An empty cache yields an empty view (`transaction.rs:61-66`) |

### 1.2 The INTROSPECTED cache (`runtime_schema_for`) - 4 production readers

Definition: `crud/introspect_schema.rs:104-142`. Gated on
`is_model_registered` (`:118-120`); on PG it calls
`zeroship_schema::diff::read_live_schema` (`:137`) and projects the result
through `build_runtime_schema` (`:281-298`); on SQLite it returns the declared
cache (`:131-133` -> `:262-264`).

| # | file:line | Function | Fact | What it does with it |
|---|---|---|---|---|
| 19 | `crud/read_pipeline.rs:71` | `apply` | whole `{type, encrypted?, mask?}` map | four consumers below |
| 19a | `crud/read_pipeline.rs:163-170` | `normalize_row_on_read` | `def.type` token | coercion: `boolean` -> `0/1`->bool; `json`/`object`/`array`/`union` -> parse; `bytes` -> base64; `date`/`calendarDate` -> epoch millis |
| 19b | `crud/read_pipeline.rs:159` | `normalize_row_on_read` | `def.encrypted` PRESENCE | skips coercion for encrypted columns |
| 19c | `crud/read_pipeline.rs:78` -> `encryption_pass.rs:278-346` | `decrypt_row_on_read` | `def.encrypted.{mode,keyId,wraps}` | resolves the key, builds the AAD, decrypts, deserialises |
| 19d | `crud/read_pipeline.rs:93` -> `mask_pass.rs:438-490` | `wrap_row_on_read` | `def.mask.{kind,classification}` | wraps the value in a `__zsmask__` sentinel object carrying the classification and the row PK |
| 20 | `crud/write_pipeline.rs:116` | `apply` | whole map | builds `WriteStages` (`write_pipeline.rs:196-205`), four flags below |
| 20a | `crud/mod.rs:2579-2584` | `schema_has_encrypted_columns` | any `def.encrypted` | gates `encryption_pass_dispatch` (`write_pipeline.rs:228-238`) |
| 20b | `crud/mod.rs:2590-2607` | `schema_has_masked_columns` | any `def.mask.kind != "none"` | gates `mask_pass::apply_mask_on_write` |
| 20c | `crud/mod.rs:2609-2621` | `schema_has_sqlite_binary_columns` | any `def.type in {vector, geoPoint}` | gates the SQLite blob encode |
| 20d | `crud/bytes_pass.rs:67-77` | `schema_has_plain_bytes_columns` | any `def.type == "bytes"` && no `encrypted` | gates `encode_bytes_on_write` |
| 21 | `crud/write_pipeline.rs:368` | `update_requires_per_row_encryption` | `def.encrypted.mode == "randomised"` on a touched field | forces the per-row UPDATE path (one statement per row) so each row gets its own AAD (`write_pipeline.rs:387-414`) |
| 22 | `crud/write_pipeline.rs:380` | `upsert_requires_conflict_probe` | same, on an insert doc | forces the conflict probe before an upsert |

Non-production: `crud/mod.rs:2486-2496` (`runtime_schema_for_tests`, gated
`test-helpers`), `v8_classes/collection.rs:53-57`
(`resolved_runtime_schema_for_tests`).

### 1.3 Facts derived from the field map INSIDE `zeroship-schema::query`

These are not separate cache reads; they are what the `schema_hint` is used FOR.
`crate::query` in plugin-db is a re-export of `zeroship_schema::query`
(`crates/zeroship-data-v8/src/lib.rs`).

| # | file:line | Function | Fact | What it does |
|---|---|---|---|---|
| 23 | `zeroship-schema/src/query.rs:921-926` | `schema_declares_readable_field` | is `name` a declared key that is not `_meta`/`_indexes` | membership test |
| 24 | `crates/zeroship-data-orm/src/sql/compile.rs` | `validate_read_identifier` | declared readable fields | validates identifiers against the required descriptor; no implicit field-name allowance |
| 25 | `query.rs:3368-3380` | `column_is_masked` | `def.mask.kind != "none"` (missing kind defaults `"full"`) | the single predicate behind every sibling substitution |
| 26 | `query.rs:3326-3343` | `project_read_field` | 25 | emits `"<col>_masked" AS "<col>"` when masked, bare column otherwise. **The sibling name is derived by `format!("{field}_masked")` at `:3336`** |
| 27 | `crates/zeroship-data-orm/src/sql/compile.rs` | `implicit_read_projection_parts` | declared readable fields | builds an explicit projection from descriptor fields and storage metadata |
| 28 | `query.rs:3394-3400` | `aggregate_read_ident` | 25 | sibling name again, `format!("{field}_masked")` at `:3396` |
| 29 | `query.rs:3407-3422` | `push_group_by_field` | 25 | sibling name again, `format!("{field}_masked")` at `:3415` |
| 30 | `query.rs:3258-3269` | `build_masked_aware_select_expr_for_table_alias` | 27 | the search-path (`t` alias) form; `None` -> `"t".*` |
| 31 | `crates/zeroship-data-orm/src/assignments.rs` | `AssignmentPlan::from_schema` | per-field assignment metadata | replaces the implicit field list; operation roles resolve through `crates/zeroship-data-orm/src/sql/lifecycle.rs` |
| 32 | `query.rs:738-762` | `RESERVED_NAMES` const | `_masked` suffix, `_` prefix, `__zs_`/`__zeroship_`/`sqlite_` prefixes, 6 classification names | field-name reservation at declaration AND filter time |

### 1.4 Facts derived inside the CRUD passes

| # | file:line | Function | Fact | What it does |
|---|---|---|---|---|
| 33 | `crud/mask_pass.rs:150` | `apply_mask_on_write` | sibling name | `format!("{col}_masked")` - writes the computed mask into that key so the SQL builder picks it up |
| 34 | `crud/mask_pass.rs:469` | `wrap_row_on_read` | sibling name | `format!("{col}_masked")` - prefers the sibling's value over the parent, and re-applies the mask transform to the parent if the sibling is absent (`:470-479`) |
| 35 | `crud/encryption_pass.rs:295` | `decrypt_row_on_read` | sibling name | `format!("{col}_masked")` - skip-decrypt marker |
| 36 | `crud/mask_drift.rs:135` | `run_drift_check_for_column` | sibling name | `format!("{column}_masked")` - the SELECT target |
| 37 | `crud/mod.rs:366-405` | `encode_sqlite_binary_scalar` | `def.type == "vector"` + **`def.vectorDims`** | length-checks the array against the declared dimension and packs LE f32; a missing `vectorDims` is a hard `DbError::internal` (`:371-375`) |
| 38 | `crud/mod.rs:406-432` | `encode_sqlite_binary_scalar` | `def.type == "geoPoint"` | packs `{lat,lng}` into a 16-byte blob |
| 39 | `crud/read_pipeline.rs:120-131` | `scope_schema` | the declared key SET | narrows the schema to the projected fields for aggregates, so an aggregate alias colliding with a masked column name does not trigger the mask transform |
| 40 | `encryption/aad.rs:75-98` | `canonical_aad` | **the PHYSICAL column name** | length-prefixed into the AEAD tag: `[version][collection][column][row_pk]`. Called at `encryption_pass.rs:200-207` (write), `:337-344` (read), `mask_backfill.rs:155-163` (backfill) |

### 1.5 Auxiliary PHYSICAL objects derived by convention, not read from anywhere

These are the same category as the `_masked` sibling: one declared field maps to
more than one physical database object, and the extra names are computed by
string formatting rather than recorded.

| # | file:line | Convention | Consumer |
|---|---|---|---|
| 41 | `backend/sqlite/vector.rs:98-100` | `vec_table_name` = `"{collection}__vec_{column}"` | the vec0 vtable joined at `vector.rs:130`; the three triggers at `vector.rs:190-251` name `{collection}__vec_{column}_{ai,ad,au}` `[delegated]` |
| 42 | `backend/sqlite/vector.rs:130` | the base table and the vec0 vtable share `rowid` | `JOIN ... ON t.rowid = v.rowid` `[delegated]` |
| 43 | `zeroship-schema/src/query.rs:2130-2134` | `index_name(table, cols, unique)` = `{table}_{cols}_{idx\|key}` | the index-creation and audited-retry paths (`backend/postgres.rs:517`, `:663`) `[delegated]` |
| 44 | `backend/sqlite/mod.rs:764` | the app file is `zs-{app_id}.sqlite`, ATTACHed under the alias `{app_id}` | every SQLite CRUD statement `[delegated]` |
| 45 | `crud/mod.rs:2609-2621` + `crud/mod.rs:366` | `def.type` `"vector"`/`"geoPoint"` gate the SQLite blob channel | `SQLITE_BINARY_BIND_PREFIX`-prefixed params decoded at `backend/sqlite/session.rs:1988-2008` `[delegated]` |

**The name is already recorded once and then thrown away.** `MaskMeta`
(`zeroship-schema/src/diff.rs:371-383`) carries a `sibling_column: String` field
whose doc says it is "always `format!("{parent}_masked")`. Stored explicitly"
(`crates/zeroship-migrate-backend/src/mask_meta.rs:93-96`) `[delegated]`.
`read_live_schema` populates it (`diff.rs:747-751`); `build_runtime_schema`
discards it (`crud/introspect_schema.rs:317-320`, which projects only `kind` and
`classification` via `mask_to_json` at `:376-381`); and every consumer then
re-derives the name by string formatting. The specification's `storage` block
(section 3.4) is therefore not a new idea - it is the existing `sibling_column` field
carried all the way to the consumer instead of being dropped one layer short.

The one place the name should be MINTED rather than read is the DDL emitter:
`zeroship-schema/src/query.rs:2146` (`mask_sibling_column_for_field`) and
`:2167` (`mask_sentinel_for_field`) `[delegated]`. Under v2 that function stays,
and its output is what the fold writes into `storage`.

### 1.6 Collection options and declared column roles

The migration renderer resolves enabled lifecycle options against the declared
generators. It emits `softDelete` and `concurrency` on the selected fields and
rejects ambiguous or missing generators. See
[per-collection options](../reference/db.md#per-collection-options).

- **Soft deletion:** `soft_delete_column` in
  [SQL lifecycle resolution](../../crates/zeroship-data-orm/src/sql/lifecycle.rs)
  locates the marker. `should_filter_soft_deleted` in
  [assignment preparation](../../crates/zeroship-data-orm/src/crud/assignment_pass.rs)
  still returns `!include_deleted`: it expresses caller intent, while the SQL
  compiler separately resolves whether the collection has a deletion marker.
  No field named `deleted_at` is assumed.
- **Versioning:** revision predicates use the declared concurrency column.
  [Assignment resolution](../../crates/zeroship-data-orm/src/assignments.rs)
  supplies write expressions from the generator, preserving the declared
  increment step. No field named `version` is assumed.
- **Strictness:** the descriptor preserves the deploy-time validation policy;
  deployment enforcement is not wired yet. This is separate from the ORM's
  write-input and identifier validation.

### 1.7 Live-database reads in the data plane that are NOT the schema cache

| # | file:line | Function | SQL / mechanism | Fact |
|---|---|---|---|---|
| 46 | `backend/postgres.rs:472` | `ensure_pgvector_available` | `SELECT 1 FROM pg_extension WHERE extname='vector'` | is pgvector installed. Memoised for the backend's life (`:476`). Called from `vector_search` at `:569` `[delegated]` |
| 47 | `backend/postgres.rs:632` | `ensure_postgis_available` | `SELECT 1 FROM pg_extension WHERE extname='postgis'` | is PostGIS installed. Memoised at `:637`. Called from `spatial_near` at `:701` `[delegated]` |
| 48 | `backend/sqlite/cdc.rs:663` | `fetch_column_names` | `PRAGMA {db}.table_info({table})` | ordered column NAMES, because the preupdate hook supplies only indices (`cdc.rs:57-65`). Cached; a fetch failure degrades to synthesised `c0`/`c1`/... names (`cdc.rs:692-695`) `[delegated]` |
| 49 | `backend/postgres.rs:958-961` | `create_index_with_recovery_audited` | `SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass` | did a `CREATE INDEX CONCURRENTLY` land VALID. Reachable only from `ensure_vector_index`/`ensure_spatial_index`, both `#[cfg(any(test, feature = "test-helpers"))]` (`postgres.rs:492`, `:654`) - **not in a production data plane** `[delegated]` |
| 50 | `backend/sqlite/mod.rs:1737-1744` | `ensure_vector_index` | `SELECT 1 FROM {app}.sqlite_master WHERE type='table' AND name='{vtab}'` | does the vec0 vtable exist, gating a non-idempotent population INSERT. `#[cfg(any(test, feature = "test-helpers"))]` at `mod.rs:1703` `[delegated]` |
| 51 | `backend/sqlite/mod.rs:804-1131` | `SchemaIntrospect for SqliteBackend` | `sqlite_master` + four `PRAGMA` families | the full live shape. `#[cfg(any(test, feature = "test-helpers"))]` at `mod.rs:804` - **absent from a production build** `[delegated]` |
| 52 | `backend/postgres.rs:344-357` | `SchemaIntrospect for PostgresBackend` | delegates to `zeroship_schema::diff::read_live_schema` | `#[cfg(any(test, feature = "test-helpers"))]` at `:340` `[delegated]` |
| 53 | `crud/introspect_schema.rs:137` | `runtime_schema_for` | `zeroship_schema::diff::read_live_schema` | **the one UNGATED production catalog read in the tree.** SQL at `zeroship-schema/src/diff.rs:611-636` `[delegated]` |

Two things are worth separating out of this list because they look like catalog
reads and are not:

- `backend/sqlite/mod.rs:1936-1949` resolves the geoPoint column by scanning the
  RESULT SET's column names (`typed.columns.iter().position(..)`), not the
  catalog `[delegated]`.
- `crates/zeroship-data-v8/src/v8_bridge.rs` picks a JSON decode by
  the wire `RowDescription` type OID, i.e. protocol metadata `[delegated]`.

### 1.8 What `read_live_schema` actually recovers, and what the data plane keeps

`read_live_schema` (`zeroship-schema/src/diff.rs:597`) issues **three** queries,
each bound with `$1 = app_id`: columns (`:611-636`), foreign keys (`:759-775`),
indexes (`:808-824`) `[delegated]`.

The column query selects `relname`, `attname`,
`format_type(atttypid, atttypmod)`, `attnotnull`,
`pg_get_expr(ad.adbin, ad.adrelid)`, a `pg_proc.provolatile` subquery, and
`pg_description.description`. It then makes two passes:

- `diff.rs:668-694`: a `zsenc:` comment on the column itself parses into
  `EncryptionMeta`; a `__zsmask:` comment on a column whose name **ends with
  `_masked`** is deferred.
- `diff.rs:714-745`: each deferred sentinel has its `_masked` suffix STRIPPED
  (`:715`) and the resulting `MaskMeta` is stamped onto the PARENT.

`build_runtime_schema` (`crud/introspect_schema.rs:281-298`) then discards
everything except a DSL type token, `encrypted`, and `mask`, and **skips every
column whose name ends with `_masked`** (`:289-291`).

So the data plane keeps three facts out of eight, and both of the two it keeps
that matter (`encrypted`, `mask`) were **written into the catalog by the
migration engine from the same DSL** the descriptor is folded from. The catalog
is a round-trip, not an independent source.

Two further measurements sharpen that `[delegated]`:

- **`LiveSchema.row_counts` is declared and never populated** by
  `read_live_schema` (`diff.rs:206-219` declares it; nothing in `:597-855`
  writes it). Row counts come only from the separate `estimate_row_count`
  (`diff.rs:872`), which is `#[cfg(any(test, feature = "test-helpers"))]` at its
  only backend call sites.
- **`ColumnInfo.vector_dims` and `ColumnInfo.is_geopoint` are never populated on
  EITHER backend.** PG leaves them at `..Default::default()` (`diff.rs:703-705`)
  with a comment claiming they come from "`information_schema` + `pg_indexes`
  introspection" - code that is not in the function. SQLite leaves them at
  `..Default::default()` too (`backend/sqlite/mod.rs:953-958`) with the same kind
  of comment and no regex in the tree. So the two facts introspection would need
  in order to serve `encode_sqlite_binary_scalar` (fact 37) are, on both
  dialects, documented-as-populated and actually absent. This is not a gap the
  descriptor must close - it is a gap the descriptor already closes, and the
  reason the live path has never had to.

Also `[delegated]`: **`compute_diff` (`diff.rs:900`) has no production call site
in plugin-db at all** - grep finds only doc-comment mentions
(`backend/mod.rs:565`, `lib.rs:127`, `cross_app_fk.rs:70`, `:94`, `:232`) and
four calls in `crates/zeroship-data-v8/tests/sqlite_integration.rs` (`:6999`, `:7087`, `:7156`, `:7224`).
The diff classifier is a migration-engine facility that the data plane links but
never runs.

### 1.9 Facts the data plane NEEDS and does not obtain today

These are not consumers, they are absences. They belong in the enumeration
because a specification written only from what the code reads will bake in the
gaps. All six are `[delegated]` findings I did not re-derive, except the last two,
which I read directly.

1. **`vector_search` never learns the column's declared dimensionality.**
   `backend/postgres.rs:555-563` takes no `dims` argument and never checks
   `query.len()` against anything. Meanwhile `backend/mod.rs:1132-1134` promises
   that impls "fail-fast on a dim mismatch at insert time via a
   `vector_dimension_mismatch` typed error". The PG impl cannot produce that
   error. The SQLite arm is the same: `dims` flows one way, caller ->
   `build_create_vec0_sql` (`backend/sqlite/vector.rs:148`, `:162`), and is never
   read back. **`fields[c].vectorDims` in the descriptor closes this** - a fact
   the DSL has had all along and the runtime has never been given.

2. **`spatial_near` never learns whether the column is `geography` or
   `geometry`.** `zeroship-schema/src/query.rs:5000-5001` documents the
   requirement; nothing verifies it, and `ST_DWithin` /
   `ST_MakePoint($1,$2)::geography` are emitted unconditionally
   (`query.rs:5039-5042`). A `geometry` column silently changes `radius_m` from
   metres to degrees. The string `geometry` does not appear in
   `backend/postgres.rs` at all.

3. **Primary-key identity now comes from the descriptor.**
   `primary_key` in
   [SQL lifecycle resolution](../../crates/zeroship-data-orm/src/sql/lifecycle.rs)
   selects the field marked `primaryKey`. Row operations requiring a scalar key
   reject missing or ambiguous declarations. The former assumption that the key
   must be named `id` no longer describes the ORM.

4. **No column-existence probe precedes any emitted SQL.**
   `ensure_vector_index` (`backend/postgres.rs:493`) and `ensure_spatial_index`
   (`:655`) splice the column name into `CREATE INDEX CONCURRENTLY` after only
   `quote_ident`. This is the property the deferred DDL validator would supply.

5. **`PgDialect::map_zs_type` (`backend/postgres.rs:789-813`) ignores its
   `_opts` parameter entirely** and falls back to `TEXT` for any unknown token
   (`:803-810`), so `vector`, `geoPoint` and `encrypted` would all become `TEXT`
   if anything routed through it. Nothing does today - the real emitter is
   `zeroship-schema::query::field_to_column_for_dialect` - but the hook exists,
   is public on the trait, and cannot express `vector(N)`, `NUMERIC(p,s)` or
   `geography(POINT,4326)`.

6. **The cold-cache read leak, which the descriptor deletes by construction.**
   When the schema hint is `None`,
   `build_masked_aware_select_expr_with_unmask` falls through to bare `*`
   (`zeroship-schema/src/query.rs:3312-3316`),
   `build_masked_aware_select_expr_for_table_alias` to `"t".*`
   (`:3266`), and `validate_read_identifier`'s membership check silently passes
   (`:933`). A worker thread that has not yet run `registerModel` for a
   collection therefore serves `find`, vector search and spatial near with the
   plaintext/ciphertext parent column and every internal physical column
   included. Both delegated backend sweeps found this independently. It exists
   only because the schema arrives asynchronously; an isolate-owned immutable
   descriptor has no `None` state.

---

## 2. Classification

### Bucket A - the descriptor can carry it

Every numbered fact in section 1.1 through section 1.6, plus section 1.5's conventions. Field names
in the proposed shape (section 3) are given in parentheses.

| Facts | Descriptor field |
|---|---|
| 1-4, 19a (type tokens for coercion and boolean lowering) | `fields[c].type` (already present) |
| 19b, 20a, 21, 22, 10, 19c (encryption) | `fields[c].encrypted.{mode,keyId,wraps}` (already present) |
| 9, 11, 20b, 19d, 25-30, 33-36 (masking) | `fields[c].mask.{kind,classification}` (already present) + **new** `fields[c].storage` (section 3.2) |
| 12 (typed_id prefix) | `fields[c].idPrefix` (already present in the FieldDef vocabulary, `packages/db/src/types.ts:974`; emitted at `crates/zeroship-migrate-core/src/render/declarative.rs:660`) |
| 37 (vector dims), 20c, 45 | `fields[c].vectorDims`, `fields[c].vectorMetric` (already emitted, `declarative.rs:467-472`) |
| 38 (geoPoint) | `fields[c].type == "geoPoint"` |
| 20d (plain bytes) | `fields[c].type == "bytes"` + absence of `encrypted` |
| 23, 24, 27, 39 (the declared key set / read allowlist) | `fields` keys + **new** `fields[c].readable` (section 3.3) |
| 31 (generated columns) | per-field `assign` metadata and declared operation roles; no separate field list |
| 18 (collection name set) | `collections` keys (already present) |
| 40 (AAD physical column) | **new** `fields[c].storage.aadColumn` (section 3.5) |
| 41, 42 (vec0 vtable + rowid join) | **new** `fields[c].storage.auxiliary` |
| 43 (index names) | `indexes[].name` (already present) |
| 44 (SQLite file/alias) | not a schema fact; stays a runtime convention |
| section 1.6 (`softDelete`, `versioning`, `strictness`) | lifecycle options resolve to column roles; deployment strictness enforcement remains unwired |

Two facts in bucket A deserve a note because they are *currently* unrecoverable
from introspection and therefore prove the descriptor is the richer source, not
merely an equivalent one:

- **`vectorDims`** (fact 37). `dsl_type_for`
  (`crud/introspect_schema.rs:329-353`) has no `vector` arm; the token set it can
  emit is `{boolean, json, bytes, date, number, string}`. So a PG-introspected
  schema can never satisfy `encode_sqlite_binary_scalar`'s `vectorDims` lookup.
  This does not fire today only because the SQLite binary path is dialect-gated
  (`crud/write_pipeline.rs:242`) and on SQLite `runtime_schema_for` returns the
  DECLARED cache. The seam is one dialect check away from a hard
  `DbError::internal`.
- **`idPrefix`** (fact 12). This is a declaration on the field assigned by
  `typedId`, consumed by `prefix_for_collection` in
  [assignment preparation](../../crates/zeroship-data-orm/src/crud/assignment_pass.rs).
  A database catalog cannot infer the intended prefix.

### Bucket B - genuinely requires reading the live database

**Empty of schema facts.**

Two non-schema facts remain, and I state them here rather than hiding them,
because the operator's decision as written ("no live introspection at all")
covers them:

**B-1. Extension presence (`pg_extension`), facts 46 and 47.**

- The fact: is the `vector` / `postgis` extension installed on THIS server.
- Why the DSL cannot know it: the descriptor is folded on the creator's machine
  from migration files. Extension installation is an operator action on the
  platform's Postgres cluster, taken at provisioning time. Nothing in the
  migration DSL observes it.
- **Why it is not a counter-example to the decision.** It is not a fact about the
  collection's shape; it is a fact about the server's provisioning. And it is
  *implied* by a fact the descriptor does carry: if the descriptor declares a
  `vector(N)` column, the migration that created that column must have succeeded,
  which is impossible without the extension. So the probe is redundant with the
  deploy contract; what it buys is a better error message
  (`vector_extension_missing` with a `CREATE EXTENSION` hint,
  `backend/postgres.rs:480-486`) instead of a raw SQLSTATE 42704/42883.
- **Disposition**: delete both probes; have the descriptor carry
  `requiresExtensions: ["vector"]` per collection (section 3.6) and map the resulting
  SQLSTATE back to the same typed error. That preserves the error quality with no
  catalog read. If the operator prefers to keep the probes, they must be carved
  out of the decision explicitly as *capability* probes, not introspection - a
  wording change, not an architecture change.

**B-2. Nothing else.** In particular the following are NOT bucket B, and each is
argued in section 5:

- "The database might be at a different migration version than the artifact" -
  a sequencing fact, guaranteed by the deploy contract, not a fact to be read.
- "The catalog is truth if someone applies DDL out of band" - there is
  deliberately exactly one applier (`register_model/mod.rs:168-172`).
- "Only introspection knows which physical column an encrypted value lives in" -
  the descriptor must record it explicitly, which is strictly better than
  re-deriving it (section 3.5).

### Bucket C - unclear, with the question that settles it

**C-1. SQLite physical column ORDER (fact 48).**

The CDC preupdate hook gives column INDICES, not names
(`backend/sqlite/cdc.rs:57-65`); the publisher resolves them via
`PRAGMA table_info` (`cdc.rs:663`, reading only column 1, in cid order)
`[delegated]`. The descriptor's `fields` is serialized from a `BTreeMap`
(`crates/zeroship-migrate-core/src/render/gen_types.rs:124`, `:219-243`), i.e.
sorted by NAME, so it cannot answer "what is column index 3" today. The fold DOES
retain declaration order internally - `project_collection_descriptors` builds an
`IndexMap` (`crates/zeroship-migrate-core/src/render/fold/single_fold.rs:967-977`)
and the SQLite rename lowering explicitly preserves field-insertion order so "the
emitted column order matches the live table's"
(`crates/zeroship-migrate-core/src/render/declarative.rs:558-560`).

**Question that settles it:** does the SQLite emitter guarantee that a table's
physical `cid` order equals the descriptor's declaration order after EVERY op
sequence the engine can produce - in particular `addColumn` (appends) and the
12-step rebuild (recreates from a preserved order)? If yes, this is bucket A with
a `collections[n].physicalColumns: [...]` array. If there is any op that
reorders, it is bucket B for CDC specifically, and CDC must keep its `PRAGMA`.

Note that CDC is arguably outside "the data plane" the decision covers - it is a
change-stream publisher, not a CRUD path. If the operator scopes the decision to
CRUD, C-1 disappears.

**C-2. The snake_case / camelCase alias resolution in `unmask` (fact 8).**

`resolve_schema_column` (`crud/unmask.rs:125-144`) accepts a column name and
tries three spellings against the declared key set. It exists because the SDK's
naming strategy may present a camelCase JS field over a snake_case column. The
descriptor's `fields` keys are snake_case columns by construction
(`gen_types.rs:20-22`).

**Question that settles it:** is the JS-name-to-column-name mapping a pure
function of the naming strategy (in which case the descriptor should carry
`fields[c].jsName` and the three-try heuristic is deleted), or can a creator
override an individual field's column name (in which case the descriptor MUST
carry the mapping, and the heuristic is a latent aliasing bug where two JS names
collapse to one column)? `docs/reference/db.md` documents a "naming strategy";
whether it is per-field overridable was not established by this survey.

---

## 3. The descriptor specification

Target: `RuntimeSchemaDescriptorV2`. Pre-launch, so v1 is deleted in the same
change; there is no dual-read arm (AGENTS.md, "Development status").

Current v1 (`crates/zeroship-migrate-core/src/render/gen_types.rs:121-176`,
TypeScript mirror at `sdks/bootstrap/src/install-schema.ts:139-152`):

```
{ version: 1,
  collections: { <name>: { fields: { <col>: FieldDef },
                           options: { softDelete, versioning, strictness },
                           indexes: [{ name, fields, unique? }] } } }
```

### 3.1 Top level

```jsonc
{
  "version": 2,
  // NEW. The fold selects Op::Dialectal legs, so one migration history
  // legitimately yields a different column set per target
  // (gen_types.rs:31-34). A PG runtime handed a SQLite-folded descriptor
  // names columns the database does not have. Nothing records this today.
  "dialect": "postgres" | "sqlite" | "mysql",

  // NEW. The migration-history identity this descriptor was folded from:
  // the last applied migration's version plus a content hash of the fold.
  // Two consumers, neither of which exists yet and both of which are
  // blocked without it: the deferred DDL validator has nothing to compare
  // against, and the isolate binding has nothing to fence a stale
  // descriptor with.
  "schemaEpoch": { "migration": "20260826120000_add_ssn", "hash": "sha256:..." },

  "collections": { "<name>": { /* 3.2 */ } }
}
```

### 3.2 Per collection

```jsonc
{
  // The DECLARED, creator-visible fields. Keys are the logical names the
  // SDK surface uses; each entry says where it physically lives.
  "fields": { "<logical>": { /* 3.3 */ } },

  // Generated columns belong in fields with their assignments and roles.
  // There is no separate list of implicitly privileged column names.

  // NEW. Every PHYSICAL column of the table, in physical (cid / attnum)
  // order, including mask siblings and any other platform-emitted column.
  // Answers CDC's index-to-name question (C-1) and is the allowlist the
  // read projection is built from.
  "physicalColumns": ["id", "created_at", "...", "ssn", "ssn_raw"],

  // UNCHANGED in shape. See section 4 for the missing consumers.
  "options": { "softDelete": bool, "versioning": bool,
               "strictness": "strict" | "lenient" | "off" },

  "indexes": [{ "name": "...", "fields": ["..."], "unique": bool }],

  // NEW. Extension names this collection's columns require, so the
  // runtime can map a raw SQLSTATE back to the typed
  // vector_extension_missing / postgis_extension_missing error without
  // probing pg_extension (B-1).
  "requiresExtensions": ["vector"]
}
```

### 3.3 Per field

The existing `FieldDef` vocabulary (`packages/db/src/types.ts:824-1000`) is kept
verbatim - `type`, `required`, `unique`, `index`, `default`, `min`, `max`,
`enum`, `pattern`, `refTarget`, `refColumn`, `onDelete`, `onUpdate`,
`deferrable`, `shape`, `literalValue`, `variants`, `discriminator`,
`vectorDims`, `vectorMetric`, `encrypted`, `mask`, `idPrefix`,
`timestampAuto`, plus the engine's `precision`/`scale`/`charLen`/`maxLength`/
`caseSensitive`/`generated`/`identity` (`declarative.rs:447-536`).

Three fields are added:

```jsonc
{
  "type": "string",
  "mask": { "kind": "last4", "classification": "pci" },
  "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },

  // NEW - 3.4, the storage mapping. THIS IS THE PIECE THAT REPLACES EVERY
  // format!("{col}_masked") IN THE TREE.
  "storage": { /* 3.4 */ },

  // NEW - the read-surface capabilities of the LOGICAL field, so
  // validate_read_identifier (query.rs:928-939) and the ORDER BY / filter
  // builders stop inferring them.
  "readable":    true,
  "filterable":  true,
  "sortable":    true,
  "projectable": true,

  // NEW (conditional on C-2) - the JS-surface name, when it differs from
  // the column name, so unmask.rs:125-144's three-spelling heuristic is
  // deleted rather than reimplemented.
  "jsName": "ssn"
}
```

### 3.4 `storage` - the masking flip, recorded rather than derived

The decided flip: one declared field maps to **two** physical columns. `ssn`
holds the MASKED value; `ssn_raw` holds the real one; `ssn_raw` is not
filterable, projectable or sortable.

```jsonc
"storage": {
  // The column a default SELECT projects under the logical name. Under the
  // flip this IS the logical name, so `project_read_field`
  // (query.rs:3326-3343) emits a bare `"ssn"` and the `AS` alias
  // disappears from the common path.
  "valueColumn": "ssn",

  // The column holding the authoritative value: plaintext for a mask-only
  // field, ciphertext for an encrypted one. Absent for an ordinary field.
  "rawColumn": "ssn_raw",

  // Explicit capability flags on the RAW column. Not derived from the
  // name, not derived from "it ends in _raw".
  "rawFilterable":  false,
  "rawSortable":    false,
  "rawProjectable": false,

  // The column name bound into canonical_aad. See 3.5 - this is NOT
  // necessarily either of the two above.
  "aadColumn": "ssn",

  // Extra physical objects this field owns (fact 41/42). The runtime joins
  // and triggers by these names instead of formatting them.
  "auxiliary": [
    { "kind": "sqliteVec0Table", "name": "notes__vec_embedding",
      "joinOn": "rowid",
      "triggers": ["notes__vec_embedding_ai", "notes__vec_embedding_ad",
                   "notes__vec_embedding_au"] }
  ]
}
```

Every site listed in facts 26, 28, 29, 33, 34, 35, 36 and `diff.rs:715` reads
`storage.valueColumn` / `storage.rawColumn` instead of formatting a name. The
`_masked` suffix reservation (`query.rs:754`) stays - creators still must not
declare a column with a platform-reserved suffix - but nothing *derives* from it.

### 3.5 `aadColumn` and why it is a separate field

`canonical_aad(collection, column, row_pk)`
(`crates/zeroship-data-v8/src/encryption/aad.rs`) length-prefixes the
column name into the AEAD tag. Consequences the specification must respect:

1. **The physical column name is part of the authentication.** Change which
   column an encrypted value lives in, and every existing ciphertext fails tag
   verification with `encryption_aead_failed`. That is a data-loss-class failure
   presented as a decryption error, which is much harder to diagnose than a
   missing column.

2. **The flip therefore cannot silently move encrypted data.** Today an encrypted
   masked field stores the ciphertext in `ssn` and the mask in `ssn_masked`
   (`crud/mask_pass.rs:1-15`, `broker.rs:1855-1860`). After the flip the
   ciphertext lives in `ssn_raw`. Any row written before the flip has AAD bound
   to `"ssn"`; any row written after has AAD bound to `"ssn_raw"`. **The two are
   not interchangeable and no `ALTER TABLE ... RENAME` fixes it** - the tag is
   over the name, not over the storage location.

3. **Hence `aadColumn` is explicit and is descriptor state, not a derivation.**
   For a schema authored after the flip it equals `storage.rawColumn`. For any
   collection carrying pre-flip rows it stays `"ssn"` while the data physically
   lives in `ssn_raw`, and the AAD is deliberately decoupled from the physical
   location. The migration that performs the flip writes the correct value.

4. **Pre-launch escape hatch.** There are no deployed creator apps
   (AGENTS.md: "no creator apps in the wild"), so in practice every collection is
   authored after the flip and `aadColumn == storage.rawColumn` everywhere. The
   field is still specified separately because the alternative - deriving AAD
   from the physical location - is the shape that CANNOT express a rename later,
   and encryption-key-bound decisions are exactly the ones AGENTS.md says to get
   right now rather than migrate later.

5. **`canonical_aad` binds `WIRE_VERSION_V1` as its first segment**
   (`aad.rs:93`) with a comment that a future `0x02` "MUST be threaded through
   this function as a parameter". If the flip is judged to warrant a wire-version
   bump rather than an `aadColumn` field, that is the alternative design; it
   costs a re-encryption of every existing row, which pre-launch is free and
   post-launch is not. Either choice must be made deliberately, not defaulted
   into.

### 3.6 What is deliberately NOT in the descriptor

- **`not_null`, `default_expr`, `default_volatility`, `row_counts`, live
  `indexes`, live `foreign_keys`** - `read_live_schema` populates all of these
  (`zeroship-schema/src/diff.rs:206-219`) and `build_runtime_schema` discards
  every one (`crud/introspect_schema.rs:281-298`). They are migration-engine
  facts with no data-plane consumer. (`FieldDef.required`/`default` remain, as
  SDK-side validation inputs; that is a different thing from the catalog's
  `attnotnull`.)
- **Extension INSTALLED state** - see B-1.
- **The SQLite file name and ATTACH alias** (fact 44) - a runtime convention
  keyed on `app_id`, not a property of the schema.

---

## 4. What breaks

### 4.1 Consumers that change shape

| Site | Change |
|---|---|
| `crud/introspect_schema.rs` (whole file, 1054 lines) | deleted: `runtime_schema_for`, `resolve_cache_miss_with_reader`, `cache_every_collection_and_request`, `sqlite_fallback_schema`, `build_runtime_schema`, `column_to_def`, `dsl_type_for`, `encryption_to_json`, `mask_to_json`, `MAX_COLLECTIONS_PER_POPULATE`, `SchemaIntrospectionGuard` |
| `context.rs:707-806` | deleted: `live_metadata_key`, `introspected_schema_for`, `has_introspected_schema`, `cache_introspected_schema`, `poll_schema_introspection`, `finish_schema_introspection` |
| `live_metadata.rs` (517 lines) | deleted in full - the process-wide introspection cache has no other client |
| `context.rs:666-705`, `:817-829` | `cache_schema` / `schema_for` / `cached_schemas_for_app` are replaced by reads off the isolate-owned descriptor; the `HashMap<"{app}:{coll}", Value>` and its string-concatenated key go away |
| `context.rs:642-664` | `is_model_registered` / `mark_model_registered` / `registered_models` go away - the gate they serve (`introspect_schema.rs:118-120`) no longer exists |
| `register_model/mod.rs` (238 lines) | the PG arm becomes a no-op with nothing to cache; the SQLite arm's `attach_app_file` must move into the data plane (the module already names this as a known defect at `:22-35`) |
| `zeroship-schema/src/query.rs:3326-3343`, `:3394-3400`, `:3407-3422` | `format!("{field}_masked")` -> `storage.valueColumn` / `storage.rawColumn` |
| `crates/zeroship-data-orm/src/sql/compile.rs` | implemented: `implicit_read_projection_parts` and `build_returning_expr` project declared readable fields from the required descriptor |
| `zeroship-schema/src/query.rs:928-939` | the `if schema_hint.is_some()` guard (`:933`) disappears; a missing collection becomes an error, not a pass |
| `crud/mask_pass.rs:150`, `:469`; `crud/encryption_pass.rs:295`; `crud/mask_drift.rs:135` | same sibling-name replacement |
| `zeroship-schema/src/diff.rs:670`, `:714-718` | the `ends_with("_masked")` / `strip_suffix("_masked")` sentinel round-trip: the SENTINEL SIDE stays (the migration engine still needs it for drift), but the data plane stops consuming it |
| `backend/postgres.rs:454-490`, `:609-650` | both `ensure_*_available` probes deleted; `vector_search`/`spatial_near` map SQLSTATE instead |
| `backend/sqlite/cdc.rs:652-675` | `fetch_column_names` reads `physicalColumns` - CONDITIONAL on C-1 |
| `crates/zeroship-data-orm/src/sql/lifecycle.rs` | implemented: `soft_delete_column` selects the declared marker; the caller's `includeDeleted` flag only controls visibility |
| `crates/zeroship-data-orm/src/assignments.rs` | implemented: write generators supply counter expressions, and the declared concurrency column selects revision checks |
| `sdks/bootstrap/src/install-schema.ts` | historical proposal: extend descriptor validation to cover dialect, epoch, physical columns and storage metadata |
| `crates/zeroship-migrate-core/src/render/gen_types.rs:121-243` | `RuntimeSchemaDescriptorV1` -> `V2`; `render_runtime_descriptor_v1` gains the storage/order/dialect/epoch projections |

### 4.2 A flagged hole in the emitter that the spec makes worse

`gen_types.rs:454-457` documents an OPEN hole: `render_runtime_descriptor_v1`
falls back to `unwrap_or_default()` for a collection missing from the runtime
metadata map, "so a table created only inside an `Op::Dialectal` leg emits its
FIELDS but loses its runtime options and plain indexes."

The historical fallback would also discard metadata needed by the proposed
runtime. The durable requirement is that dialectal tables retain assignments,
column roles and storage metadata when folded into a descriptor. Section 1.6
records the current lifecycle consumers; this historical example does not
establish a current emitter defect.

### 4.3 Semantic changes the flip forces

**Filtering.** The `find` filter is built by `build_where` and receives NO
`schema_hint` at all - only the projection and the ORDER BY do
(`query.rs:3096-3140`; SQLite's vector path is even more asymmetric, using bare
`build_where` at `backend/sqlite/mod.rs:1821` `[delegated]`). So today
`find({ ssn: x })` filters the parent column, which holds the ciphertext or the
plaintext. **After the flip the parent holds the MASK**, so the same query
filters masked text.

For a mask-only field that is arguably the correct semantics. For a
**deterministic-encrypted** field it removes the only reason deterministic mode
exists: `encryption/aad.rs:16-22` states the mode's purpose is "the
B-tree-on-ciphertext equality lookup". Note, however, that **the filter path
never encrypts the operand** - it cannot, because it has no schema - so that
lookup does not work at HEAD either. The flip does not break a working feature;
it forecloses one that is currently only a type-level brand
(`packages/db/src/types.ts:1016-1019`). The spec must state which of the two it
intends: route deterministic-equality filters to `storage.rawColumn` and encrypt
the operand, or drop deterministic mode.

**Sorting.** `build_order_by_read_with_dialect` (`query.rs:5477-5485`) validates
the field through `validate_read_identifier` and then emits the BARE column name
- `build_order_term(key, descending, dialect)` receives no schema. So today
`orderBy: { ssn: 1 }` sorts by plaintext/ciphertext while the projection reads
the mask: a masked-value ordering leak. After the flip the same code sorts by the
masked value, which closes the leak by construction. This is a fix, and it should
be recorded as one rather than discovered later.

**CDC.** The change stream ships the raw tuple, including the sibling
(`broker.rs:1872-1885`, `:1906-1913`). Today that is `{ssn: ciphertext,
ssn_masked: "***-**-6789"}`. After the flip it is `{ssn: "***-**-6789", ssn_raw:
ciphertext}`. A subscriber that reads `event.row.ssn` goes from receiving
ciphertext to receiving the mask - safer, but different. `ssn_raw` still rides
the wire, so the flip does not by itself fix CDC's exposure of the raw column;
`storage.rawProjectable: false` needs a CDC-side consumer or the guarantee is
projection-only.

### 4.4 Tests that assert on introspection

Counts are from a delegated static enumeration `[delegated]`; nothing was run.

**DELETE - 20 tests that test the introspection CACHE and die with it.**

- `crud/introspect_schema.rs`, 12 arms:
  `one_read_populates_every_collection` (`:525`),
  `internal_tables_are_not_cached_as_collections` (`:564`),
  `one_read_stamps_one_token` (`:600`),
  `absent_collection_is_cached_as_a_negative_result` (`:644`),
  `one_populate_admits_at_most_the_per_populate_cap` (`:664`),
  `requested_present_collection_is_cached_when_cap_is_hit` (`:693`),
  `requested_absent_collection_is_cached_when_cap_is_hit` (`:719`),
  `two_threads_racing_one_cold_miss_resolve_one_fact_object` (`:762`),
  `concurrent_cold_resolutions_share_one_catalog_read` (`:820`),
  `failed_catalog_read_is_not_cached_or_shared` (`:858`),
  `measure_cached_entry_size` (`:911`), `measure_value_memory_overhead` (`:940`).
  Plus the fixtures `ctx_for` (`:476`), `cached_fixture_count` (`:488`),
  `live_with_numbered_tables` (`:449`), `yield_once` (`:503`).
- `live_metadata.rs`, 7 arms: `:255`, `:300`, `:317`, `:356`, `:425`, `:465`,
  `:502`. **`a_panic_under_the_write_lock_does_not_disable_the_cache` (`:465`)
  flips to REWRITE if the descriptor store reuses `LiveMetadataCache`** - that
  decision moves this one test and is worth making explicitly.
- `context.rs:1272`
  `a_flight_on_one_database_does_not_block_a_cold_miss_on_another` - the
  singleflight keying test, dies with `poll_schema_introspection` (`:768`) /
  `finish_schema_introspection` (`:798`) / `introspection_in_progress` (`:353`).

**REWRITE - 9 tests whose PROPERTY survives but whose mechanism does not.**

| file:line | test | why it must be rewritten, not deleted |
|---|---|---|
| `v8_classes/db.rs:453` | `co_resident_deploy_bindings_keep_tokens_and_schema_entries_isolated` | pins the strongest deploy-isolation invariant - a pinned and a current deploy of one app on one thread must not share metadata. Re-express against the isolate-owned descriptor. Its seam `v8_classes/collection.rs:53` (`resolved_runtime_schema_for_tests`) must be retargeted |
| `crud/introspect_schema.rs:418` | `goodie_free_collection_still_returns_full_schema_for_read_coercions` | the property (a plain collection still yields every column's `type`, so coercions run) holds for the descriptor path |
| `crud/introspect_schema.rs:982` | `missing_collection_is_none` | becomes "a collection absent from the descriptor is a hard error", not `None` |
| `crud/introspect_schema.rs:988` | `encrypted_column_maps_to_declared_shape` | retarget to descriptor -> runtime shape |
| `crud/introspect_schema.rs:1008` | `masked_parent_maps_and_sibling_is_dropped` | **the assertion INVERTS.** Today: `phone_masked` must NOT be a schema field. Under the flip the physical raw column must be recorded in `storage`, not dropped |
| `crud/introspect_schema.rs:1030` | `jsonb_and_date_families_collapse_to_representative_tokens` | pure `pg_type` -> DSL-token mapping; **DELETE** unless the descriptor keeps a physical-type projection |
| `crates/zeroship-data-v8/tests/integration.rs` | `p4_round_trip_encrypted_masked_vector_via_introspected_metadata` | plants `COMMENT ON COLUMN ... 'zsenc:...'` / `'__zsmask:...'` (`:5146-5147`), asserts `runtime_schema_for_tests` recovered them (`:5162-5167`), then does a real round trip. **Keep the round trip; invert the assertion**: the metadata must come from the descriptor and no catalog read may occur |
| `crates/zeroship-data-v8/tests/integration.rs` | `p5_pg_crud_works_via_engine_created_schema_no_runtime_ddl` | same shape: sentinel plant at `:5425-5426`, assertions at `:5477-5482` |
| `crates/zeroship-data-v8/tests/native_transaction.rs` | `update_many_randomised_failure_is_atomic_postgres` | the test body is fine; its FIXTURE `create_encrypted_users_table` (`:247`) writes the `zsenc:` comment at `:270`, which is the only channel telling the data plane `ssn` is encrypted. **Fixture-only rewrite** |

**REWRITE-or-DELETE, gate-dependent - 2.** `context.rs:1378`
(`registered_models_round_trip`) and `:1389` (`mark_model_registered_is_idempotent`)
die only if the `is_model_registered` gate goes. Note also
`crates/zeroship-data-v8/tests/sqlite_integration.rs`
(`p6c_data_plane_reaches_the_app_file_without_a_register`), which pins the exact
cold-schema `None` path the descriptor removes; it currently passes for a reason
that will no longer exist.

**CONDITIONAL on whether sentinels survive - 47.** All sentinel round-trip tests
are writer-side or migrate-side and are **UNAFFECTED** if the sentinel mechanism
stays for drift and diff, which it should:
`zeroship-schema/src/mask_codec.rs` 15 arms (`:231`-`:379`);
`crud/mask_backfill.rs` 3 (`:390`, `:399`, `:425`);
`backend/sqlite/mod.rs` 17 (`:2958`-`:3152`);
`crates/zeroship-data-v8/tests/sqlite_integration.rs` 4 (`:6949`, `:7111`, `:7196`, `:7250`) plus 2 pure
introspection arms (`:542`, `:572`);
`zeroship-migrate-backend/src/mask_codec.rs:298`;
`zeroship-migrate/tests/column_shapes/encrypted_domain_catalog_sentinel.rs` 8
arms (`:145`, `:171`, `:202`, `:224`, `:245`, `:277`, `:306`, `:397`).
All of those drive `SchemaIntrospect` for the DIFF layer, not
`runtime_schema_for`.

**UNAFFECTED IF AND ONLY IF the descriptor preserves the
`{ type, encrypted?, mask? }` field-def shape - about 45 tests.** This is the
single decision with the largest test blast radius, and section 3.3 answers it: the
`FieldDef` vocabulary is kept verbatim and only ADDED to, so these stay green.
They are: `crud/read_pipeline.rs` 7 (`:417`, `:440`, `:460`, `:477`, `:494`,
`:516`, `:562`); `crud/mask_pass.rs` 18 (`:676`-`:1101`);
`crud/mask_backfill.rs` 5 (`:466`-`:531`); `crud/mask_drift.rs` 7 (`:988`-`:1162`);
`crud/encryption_pass.rs` 3 (`:629`, `:710`, `:788`); `crud/bytes_pass.rs` 1
(`:275`); `crud/write_pipeline.rs:754`; `backend/sqlite/mod.rs:2823`;
`backend/sqlite/vector.rs:376`; `crates/zeroship-data-v8/tests/sqlite_integration.rs` 6
(`:5246`, `:5432`, `:5465`, `:5539`, `:5667`, `:5716`).

**But two of those flip on the MASKING FLIP, independently of the shape
decision:** `backend/sqlite/vector.rs:376`
(`build_vector_search_sql_reads_masked_sibling_when_schema_cached`, asserting the
literal `"t"."ssn_masked" AS "ssn"` at `:399`) and `backend/sqlite/mod.rs:2823`
(`spatial_near_base_query_reads_masked_sibling_when_schema_cached`). Under the
flip the expected string is `"t"."ssn"`. Rewrites.

**Also flips on the masking flip:** `broker.rs:1863`
(`cdc_event_carries_masked_value_for_masked_columns`) asserts
`new_tuple["ssn"] == ciphertext` (`:1888-1892`) and
`new_tuple["ssn_masked"] == masked` (`:1893-1897`). Both invert.

**Needs a NEW arm rather than a rewrite:** `encryption/aad.rs:126-201`, eight
tests pinning the AAD byte layout. `canonical_aad`'s shape does not change, so
none of them break - which is exactly the danger. Add an arm asserting the AAD
is built from `storage.aadColumn` and not from the logical field name, because
that is the regression the flip can introduce with every existing test green
(section 3.5).

**UNAFFECTED - 12.** Class-(v) end-to-end arms that route through introspection
without asserting on it (`crates/zeroship-data-v8/tests/integration.rs`, `:5328`;
`crates/zeroship-data-v8/tests/sqlite_integration.rs`, `:9670`; `context.rs:1176`) and the three
compile-time trait assertions (`backend/postgres.rs:1798`,
`backend/mod.rs:1931`, `backend/sqlite/mod.rs:2875`), which survive because
`SchemaIntrospect` itself survives for the migrate plane.

**Shell gates - 3 arms in one file, `tests/run_data_v8_live_suite.sh`:** (DELETED; current runner: `cargo xtask test data`.)

- `:143` `PLUGIN_DB_MIN_PASSED=118` - a hard pass-count floor summed across four
  live-PG binaries (loop `:171`, sum `:180-183`, comparison `:191-199`).
  Deleting or merging any live-PG test drops below it. The file's own rule is
  "Raise it deliberately when tests are added; do not lower it to match a red
  run" (`:105-106`), so this needs a **deliberate, documented decrement** with
  the provenance ledger at `:92-142` updated - not a quiet edit.
- `:58-65` - a prose TRAP note naming
  `p4_round_trip_encrypted_masked_vector_via_introspected_metadata` explicitly.
  It goes stale the moment that test is rewritten or renamed.
- `:147` `PLUGIN_DB_SKIP_ALLOWLIST="postgis"` plus the census at `:212` - only
  `spatial_near_runs_under_per_app_role_via_rls` may announce a skip. If a
  rewritten P4/P5 test starts skipping instead of running, the census refuses
  the run.

No other shell gate is affected: a sweep of `tests/*.sh` and `tests/lib/*.sh`
for `introspect|zsenc|zsmask|mask|encrypt` found only homonyms (WebSocket frame
masking in `golden_path.sh:2073-2085`, `sentinel_domain` in
`deploy_scripts_gate.sh`, the service-key sentinel in
`service_credential_boot_gate.sh`). `tests/e2e_db_app_end_to_end.sh` and
`tests/e2e_db_sqlite.sh` contain zero references to any of it `[delegated]`.

### 4.5 Facts no proposed field obviously holds

I found none that are load-bearing, and two that are worth naming as residual:

- **`RESERVED_NAMES` / `is_schema_metadata_key`** (`query.rs:676-678`,
  `:738-762`) stay hardcoded. They are platform policy about what a creator may
  declare, not facts about a particular schema, so the descriptor is the wrong
  home for them. But note that `_meta` and `_indexes` are skipped by every schema
  walk, which means anything the SDK stuffs there (today: `strictness`) is
  invisible to Rust by construction. If v2 wants `strictness` live, it must not
  arrive via `_meta`.
- **Typed-id prefix derivation remains intentional.** A required descriptor does
  not make `idPrefix` mandatory. `prefix_for_collection` in
  [assignment preparation](../../crates/zeroship-data-orm/src/crud/assignment_pass.rs)
  reads the assigned field's prefix or derives one from the collection name.
  Both results pass the same validation. The earlier recommendation to remove
  this fallback was based on an incorrect assumption.

---

## 5. The case against my own conclusion

The survey concludes bucket B is empty of schema facts. Here is the strongest
case that it is not, and why each argument loses.

**Argument 1 - the strongest. "The descriptor describes the schema the artifact
was BUILT against. The live database is at whatever version the last successful
migration left it. Those are different facts, and only the database knows the
second one."**

This is true and it is not a rhetorical concession. The current code is built
around it: `runtime_schema_for` returns a cached NEGATIVE for a registered
collection that is absent from the catalog, and the test that pins this
(`crud/introspect_schema.rs:645-662`) names the exact scenarios - "the ordinary
first-deploy window before the migration lands, and any drift or rolling deploy
where a registered collection is not yet in the catalog." Under a descriptor-only
data plane, that window produces a SQL error ("relation does not exist") instead
of a lossless cold read.

**Why it loses:** the fact is guaranteed by SEQUENCING, not discovered by
reading. `register_model/mod.rs:141-172` states the contract in full: on
PostgreSQL `zeroship-migrate-server` creates and migrates the per-app schema before
go-live via `POST /v1/apps/{id}/migrations/apply`; on SQLite the vite dev server
applies the committed migrations before spawning the runtime. "So `registerModel`
reaches a schema that is already in place, on both dialects." A fact the deploy
contract guarantees is not a fact the request path needs to re-verify.

**What the argument costs even in losing:** the failure MODE changes, from
graceful degradation to a raw SQL error, in exactly the window where the
guarantee is violated. That is the entire justification for the deferred
Hibernate-style validator, and it should be recorded as the price of the
decision rather than waved away. A boot-time validation that runs once per
isolate is not "live introspection in the data plane"; it is a startup assertion.
The decision as stated defers it, which is a choice, and the cost is that a
sequencing bug surfaces as a per-request SQL error rather than a per-deploy
refusal.

**Argument 2. "Two sources of truth can disagree, and the catalog is the one
that matches reality. Removing the one that matches reality is backwards."**

**Why it loses:** there is deliberately exactly one applier.
`register_model/mod.rs:168-172`: "there is deliberately no runtime auto-migrate
fallback, because a fallback is a second applier, and two appliers under disjoint
locks is what this arrangement exists to prevent." With one applier and one
source, they cannot disagree except through the sequencing window of argument 1.
Furthermore the argument has the disagreement backwards: the catalog is not an
independent witness, it is a ROUND TRIP of the DSL. The `zsenc:` and `__zsmask:`
sentinels the data plane parses back out (`zeroship-schema/src/diff.rs:668-694`,
`:714-745`) were written into `pg_description` by the migration engine from the
same fold the descriptor comes from. Reading them back is a checksum of the
engine, not a second opinion about the schema.

**Argument 3. "The AAD binds the physical column name, so which column an
encrypted value lives in is authentication state. Only the database knows where
the ciphertext actually is."**

**Why it loses:** the database does not know either. `canonical_aad`
(`aad.rs:75-98`) binds the name the CALLER passes, and the caller passes the
schema key (`encryption_pass.rs:200-207` passes `col` from the schema walk at
`:175`). The catalog has no record of what AAD was used; the AAD is a function of
the schema at write time, which is precisely the thing the descriptor is. So
recording `aadColumn` explicitly (section 3.5) is not merely equivalent to introspection
- it is strictly more expressive, because it can carry a value that differs from
the physical location, which no amount of catalog reading can recover.

**Argument 4. "Introspection recovers facts a build-time fold cannot - drift from
a hand-edited database, a column an operator added."**

**Why it loses on the evidence:** it does not recover them. `build_runtime_schema`
keeps three facts and discards `not_null`, `default_expr`, `default_volatility`,
indexes, foreign keys and row counts (`crud/introspect_schema.rs:281-298` vs
`zeroship-schema/src/diff.rs:206-219`). Of the three it keeps, two are the
sentinel round trip of argument 2 and the third is a LOSSY type token whose own
module doc concedes the collapse (`introspect_schema.rs:29-43`). An operator who
adds a column by hand gets that column into `live.tables`, and
`build_runtime_schema` faithfully gives it `{type: "string"}` - which changes no
behaviour, because nothing keys on a column the SDK never projects.

**Argument 5. "Removing introspection removes a security property: the
masked-aware projection allowlist."**

This one is the reverse of a counter-argument and deserves stating plainly,
because it is the single strongest evidence FOR the decision. Both delegated
enumerations independently found the same defect: when the schema hint is `None`,
`build_masked_aware_select_expr_with_unmask` falls through to bare `*`
(`query.rs:3312-3316`) and `build_masked_aware_select_expr_for_table_alias` to
`"t".*` (`query.rs:3266`), and `validate_read_identifier`'s membership check
silently passes (`query.rs:933`). A cold declared-schema cache therefore leaks
the plaintext/ciphertext parent column and every internal physical column through
`find`, vector search and spatial near. That cold-cache state exists precisely
BECAUSE the schema arrives asynchronously via `registerModel`. An isolate-owned,
immutable descriptor has no cold state, which deletes the failure mode rather
than mitigating it.

**Argument 6. "SQLite proves nothing, because its introspector was never
written."**

**Why it loses:** it proves the opposite of what the objection intends. The
SQLite dev tier has run entirely off the declared schema since the arm was
written (`introspect_schema.rs:257-264`), through the whole mask/encryption
feature set, and the gap is documented as a follow-up rather than an incident.
That is a shipped existence proof that the descriptor carries enough. If it did
not, the dev tier would be visibly broken.

---

## 6. Open questions the operator must answer before implementation

1. **C-1**: does the SQLite emitter guarantee physical `cid` order equals
   declaration order across `addColumn` and the 12-step rebuild? If not, is CDC
   inside or outside the scope of "the data plane"?
2. **C-2**: is the JS-name-to-column-name mapping a pure function of the naming
   strategy, or per-field overridable?
3. **section 3.5**: `aadColumn` as descriptor state, or a `WIRE_VERSION` bump plus
   re-encryption? Pre-launch both are cheap; only one is cheap later.
4. **section 4.3**: does deterministic-mode equality filtering get built (route to
   `storage.rawColumn`, encrypt the operand) or does the mode get deleted? It is
   currently a type-level brand with no runtime.
5. **section 1.6 — lifecycle behavior resolved:** soft deletion and versioning
   use declared column roles. Deployment strictness enforcement remains unwired.
6. **B-1**: delete the `pg_extension` probes in favour of SQLSTATE mapping, or
   carve capability probes out of the "no live introspection" rule explicitly?
7. **section 4.4**: does the descriptor store reuse `LiveMetadataCache`
   (`crates/zeroship-data-v8/src/live_metadata.rs`)? If yes, one of its seven
   tests becomes a rewrite instead of a deletion; if no, the whole 517-line
   module goes.
8. **section 4.4 shell gate**: `tests/run_data_v8_live_suite.sh`'s (DELETED; current runner: `cargo xtask test data`.)
   `PLUGIN_DB_MIN_PASSED=118` floor must be decremented deliberately, with the
   provenance ledger at `:92-142` updated in the same change. Who signs that
   off, and to what number?

---

## 7. Coverage: what this survey did and did not reach

**Read directly, in full:** `crud/introspect_schema.rs` (1054 lines),
`crud/read_pipeline.rs` (601), `encryption/aad.rs` (202),
`register_model/mod.rs` (238), `crates/zeroship-migrate-core/src/render/gen_types.rs:1-460`.

**Read in the relevant regions:** the original review covered CRUD preparation,
generated assignments, SQL compilation, schema introspection, backend search,
V8 transaction handling, CDC delivery and SDK schema installation. These are
historical coverage boundaries, not a list of current source locations.

**Covered by delegated exhaustive reads** (marked `[delegated]` above):
`backend/postgres.rs` (1948 lines, read in full) and `backend/mod.rs` (2422
lines: verbatim for `130-175`, `640-700`, `1060-1215`, `1240-1340`, `1521-1560`;
the remainder by keyword sweep, which found zero SQL and zero catalog queries in
that file). `backend/sqlite/` (all 10 files, 9658 lines); not exhaustively read
there: `mod.rs:1479-1660`, `mod.rs:2280-2800`, `session.rs:1-1235` and
`1263-1955`, `reservation.rs` and `session_minter.rs` bodies - each
grep-confirmed to contain no `PRAGMA`, `sqlite_master`, `schema_hint` or
column-name lookup.

**NOT reached, and it should be:**

- `crates/zeroship-data-v8/src/wal_consumer.rs` (1440 lines). The pgoutput
  relation cache (`:236`, `:417`, `:502-580`) maps `rel_id -> (namespace, table,
  columns)` from the replication protocol's `Relation` messages. I did not
  establish whether it ever consults the catalog, nor how it interacts with the
  `_masked` sibling. It is the PG twin of the SQLite CDC path (C-1) and may add a
  bucket-C entry.
- `crates/zeroship-data-v8/src/read_set.rs` (586 lines). `:121` handles a
  column absent from an event tuple; the surrounding logic was not read.
- `crates/zeroship-data-v8/src/audit.rs` (772 lines) and
  `crud/mask_policy.rs` (879). Both read and write platform-internal tables whose
  shape is hardcoded; I confirmed they are not creator-schema consumers but did
  not enumerate them.
- `crates/zeroship-data-v8/src/exec.rs` (1646 lines) beyond confirming its
  three `schema` mentions (`:1112`, `:1223`, `:1411`) are "ensure app schema"
  namespace calls, not field-map reads.
- `crates/zeroship-schema/src/query.rs` beyond the regions listed. The file is
  11782 lines; I enumerated every `schema_hint` occurrence (grep-complete) and
  read the projection, masking, order-by and reserved-name regions, but not the
  DDL-rendering half (`:1019-2905`) or the aggregate builder body
  (`:4600-4800`) line by line. The delegated sweep enumerated every top-level
  `pub` item and every plugin-db call site (roughly 180, all pure computation or
  types, none touching a connection) but likewise did not characterise each
  builder's emitted SQL.
- The ~200 test-only `use zeroship_data_v8::query::{...}` import blocks in
  `crates/zeroship-data-v8/tests/sqlite_integration.rs` and `crates/zeroship-data-v8/tests/integration.rs` were not expanded
  name by name `[delegated]`.
- Non-Rust consumers beyond `packages/db/src/types.ts`,
  `sdks/bootstrap/src/install-schema.ts` and the confined-shape mirror. In
  particular `packages/db/tests/*.ts` (55 files) was grep-screened, not read: its
  `__zsmask__` usage is the runtime `MaskedValue` wire sentinel, orthogonal to
  catalog introspection `[delegated]`.
- `crates/zeroship-migrate-adapter/src/platform.rs:1172` mentions
  re-introspection; it is the migrate applier loop, not the data plane, and was
  not read `[delegated]`.

### Delegation coverage statements

Four read-only enumerations were delegated. Their own coverage statements:

1. **`backend/postgres.rs` + `backend/mod.rs`** - `postgres.rs` (1948 lines) read
   in full, no gaps. `backend/mod.rs` (2422 lines): verbatim for `130-175`,
   `640-700`, `1060-1215`, `1240-1340`, `1521-1560`; the remainder by keyword
   sweep over every SQL verb, every executor method, and every trait/fn/impl
   declaration. Result: `backend/mod.rs` executes zero SQL and issues zero
   catalog queries.
2. **`backend/sqlite/`** - all 10 files, 9658 lines. `dialect.rs`, `error.rs`,
   `spatial.rs`, `vector.rs` read in full; `mod.rs`, `session.rs`, `cdc.rs` read
   in the pattern-matched regions. Not exhaustively read: `mod.rs:1479-1660`,
   `mod.rs:2280-2800`, `session.rs:1-1235` and `1263-1955`, and the bodies of
   `reservation.rs` / `session_minter.rs` - each grep-confirmed to contain no
   `PRAGMA`, `sqlite_master`, `schema_hint` or column-name lookup.
3. **`zeroship_schema` consumers** - complete for `mask_codec`, `diff`,
   `descriptors` and the `query` call-site list; did not read the ~100
   `query::build_*` bodies, did not audit non-Rust files, did not compile
   anything (so the `#[cfg]` gating claims are read off attribute lines, not
   verified by a build).
4. **Test impact** - `integration.rs` (~130 tests) and `sqlite_integration.rs`
   (~140) were grep-screened on
   `introspect|runtime_schema_for_tests|zsmask|zsenc|_masked|register_model|
   registerModel|COMMENT ON COLUMN|MaskedValue|"mask"|encrypted` and every hit
   read in context, rather than opened test by test. A test reaching
   introspection with none of those tokens would be missed; that is unlikely
   because `runtime_schema_for` is gated on `is_model_registered`, which
   requires a call that greps. Nothing was run; all counts are static.

**Nothing in this document was verified by compiling or running anything.** The
`#[cfg]` gating claims, the pass-count floor, and every "no consumer" statement
are static reads. A build is the obvious next check, and it is the one this
survey deliberately did not perform.
