# The masking flip's WRITE path: a specification

Status: design only. No code changed. Written against the tree at
`.worktrees/dbbind-impl`, HEAD `0e785d5fc`.

Input: the BLOCKING note in
`docs/proposals/2026-08-26-sc6-ceiling-read-contract.md:336-406`, which records
that the storage flip (`ssn` holds the mask, `ssn_raw` holds the real value)
cannot be implemented as specified because its write path is unguarded in both
directions.

This document does four things: corrects the numbers the blocking note rests on,
specifies the write path, counts what the specification costs, and argues at full
strength that the flip should be cancelled instead. **The last section is not a
formality. I think the argument against is stronger than the argument for, and I
say so there rather than burying it.**

---

## 0. What I could not determine

Stated first, so nothing below reads as more settled than it is.

- **Whether `aadColumn` is meant to exist.** `docs/reviews/2026-08-27-descriptor-specification.md:648-655`
  specifies `storage.aadColumn` and devotes a whole subsection (`:665-700`) to why
  it must be separate state. The shipped `FieldStorage` struct
  (`crates/zeroship-migrate-core/src/render/gen_types.rs:172-211`) has **no such
  field**, and its doc comment says so explicitly at `:157-159`: "There is
  deliberately **no separate `aadColumn`**". One of those two documents is wrong
  and I cannot tell which is current. Section 4.6 below assumes the struct is
  authoritative because it is the thing that compiles.
- **Whether the `*Many` verbs are permanently count-only.** `dispatch_update_many`
  (`crates/zeroship-plugin-db/src/crud/mod.rs:1166`), `dispatch_delete_many`
  (`:1491`), `dispatch_purge_many` (`:1586`) and `dispatch_restore_many` (`:1675`)
  all funnel their `RETURNING *` rows into `row_count_as_f64` and return a number.
  My outbound design leans on that. If a future verb returns those rows, the
  guarantee in section 5 changes from unrepresentable to guarded, and I say where.
- **Whether the flip's DDL half has an owner.** I specified the runtime contract.
  I did not specify the migration op that performs the rename, the backfill, or
  the re-encrypt that section 4.6 shows it requires. That is engine work in
  `crates/zeroship-migrate-core/` and it is larger than the runtime work.

---

## 1. The numbers in the blocking note, corrected

I verified each. Two are materially wrong and one changes the shape of the fix.

### 1.1 "34 `RETURNING *` sites in `query.rs`"

34 is the grep count. `grep -c 'RETURNING \*' crates/zeroship-schema/src/query.rs`
returns 34, but the `#[cfg(test)] mod tests` block starts at
`crates/zeroship-schema/src/query.rs:6034`, and 22 of the 34 are test assertions
or doc comments.

**There are 12 sites that emit SQL**, all listed in the note:
`:3584` (insert), `:4005` (updateOne), `:4157` (insertMany), `:4228` (updateMany),
`:4254` (deleteMany), `:4290` (deleteOne), `:4445` `:4483` `:4525` `:4557`
(soft-delete one/many, restore one/many), `:5937` (upsert), `:6020`
(findOrCreate). Twenty *functions* reach those twelve sites, because six are thin
delegating wrappers (`build_insert` `:3503` -> `build_insert_with_dialect`
`:3518`, and five more of the same shape).

This matters because "34 edits" and "12 edits" argue for different fixes. Neither
is the number my design pays; see section 5.

### 1.2 "`strip_encryption_markers` retains it (`encryption_pass.rs:502-505`)"

`strip_encryption_markers` is `#[cfg(any(test, feature = "test-helpers"))]`
(`crates/zeroship-plugin-db/src/crud/encryption_pass.rs:501`). It is not on the
production path, and it strips `__zsbin__` markers from a **write** document
before binding, not from a returned row. The note cites it as an outbound
retainer; it is neither outbound nor live.

**The conclusion survives anyway, through a different and worse route.** Nothing
on the production read path removes an unknown key from a returned row. The only
key removal is `mask_pass::wrap_row_on_read`, which removes exactly
`format!("{col}_masked")` (`crates/zeroship-plugin-db/src/crud/mask_pass.rs:469`,
`:480-482`). So `<col>_raw` survives to `mapResultDoc`
(`sdks/db/src/utils.ts:28-33`) for the reason the note gives, just not via the
function it names.

**And there is a second silent arm the note misses.** `decrypt_row_on_read` gates
decryption on the same hardcoded sibling name:

```rust
let sibling_key = format!("{col}_masked");
let should_decrypt = masked_kind == "none"
    || unmask_columns.iter().any(|field| field == col)
    || obj.contains_key(&sibling_key);
if !should_decrypt { continue; }
```
`crates/zeroship-plugin-db/src/crud/encryption_pass.rs:295-301`

Post-flip that key is absent, so for an encrypted+masked field the decrypt stage
is skipped entirely and `<col>_raw` reaches JS as **base64 ciphertext**. For a
mask-only field it reaches JS as **plaintext**. Two different leaks from one
missing string.

### 1.3 "The SQLite introspector drops all mask metadata with no `else`"

True as written, and irrelevant on the production path: `parse_mask_sentinels`
(`crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:2201-2202`) and its only
caller, the `SchemaIntrospect for SqliteBackend` impl (`:804-806`, call at
`:917`), are both `#[cfg(any(test, feature = "test-helpers"))]`. The dev tier does
not run it. `crates/zeroship-plugin-db/src/descriptor.rs:30-32` states this in the
tree's own words: "On SQLite it was never live at all".

**The same defect IS production on Postgres, and the note does not mention it.**
`read_live_schema` filters on the suffix at
`crates/zeroship-schema/src/diff.rs:671`:

```rust
if comment.starts_with("__zsmask:") && column.ends_with("_masked") {
```

and strips it at `:716`. A `__zsmask:` sentinel on a column not ending `_masked`
falls through both `if`s and is discarded with no warning, while the malformed-
sentinel arm ten lines below warns loudly (`:737-744`). That is the same missing
`else`, in the arm that actually runs, feeding the migration engine's diff.

### 1.4 What the note gets exactly right

`RESERVED_NAMES` (`crates/zeroship-schema/src/query.rs:738-766`) has no `_raw`
entry. `build_field_condition_with_dialect` calls bare `validate_field_name`
(`:5329`). `build_conflict_probe_with_dialect` likewise (`:2909`). Aggregate
`$match` calls `build_where(match_val, &mut params)` with no schema (`:4646`)
while `$group.by` ten lines below calls `validate_read_identifier(s, schema_hint)`
(`:4656`). Unique indexes are built on the declared field name
(`:1949-1964`, `col_list = quote_ident(field)` at `:1951`).
`read_set.rs:119-131` compares the logical name against the WAL tuple, and the
module's own doc calls silently dropped events "unacceptable" (`:215-219`).

---

## 2. The three facts that decide the design

None of these is in either review. Each removes a large part of the problem.

### 2.1 The write path already has one result site

The blocking note frames the outward leak as 34 SQL sites. It is not a SQL
problem at all, because **seven of the nine row-returning write verbs already
route their `RETURNING *` rows through `read_pipeline::apply`** - the same
function every read uses:

| verb | dispatch | `read_pipeline::apply` |
| --- | --- | --- |
| insert | `crud/mod.rs:744` | `:799` |
| insertMany | `:835` | `:878` |
| updateOne | `:926` | `:1083` |
| delete (soft) | `:1434` | `:1479` |
| purgeOne | `:1541` | `:1569` |
| restoreOne | `:1619` | `:1658` |
| upsert | `:1955` | `:2010` |

`read_pipeline::apply` (`crates/zeroship-plugin-db/src/crud/read_pipeline.rs:54-100`)
resolves the descriptor itself at `:72-75` and runs normalize / decrypt / mask-wrap
/ unmask in a fixed order. **The write path is already inside the read pipeline.**
A stage added there covers all seven verbs at one site.

The other two row-returning verbs are `updateMany` (`:1166`) and its CAS fan-out,
plus `deleteMany` (`:1491`), `purgeMany` (`:1586`) and `restoreMany` (`:1675`).
All five discard the rows into `row_count_as_f64` (`:1409`, `:1529`, `:1612`,
`:1714`) and return a number. Nothing crosses to JS.

`build_find_or_create` (`query.rs:5952`, `RETURNING *, (xmax = 0) AS __created` at
`:6020`) has **no production caller** - see the comment at `crud/mod.rs:2427-2430`.

### 2.2 The descriptor already models the flip, today, pre-flip

`FieldStorage` (`crates/zeroship-migrate-core/src/render/gen_types.rs:172-211`)
carries `value_column`, `raw_column`, and three explicit capability flags:

```rust
pub value_column: String,
pub raw_column: Option<String>,
pub raw_filterable: Option<bool>,
pub raw_sortable: Option<bool>,
pub raw_projectable: Option<bool>,
```

`field_storage` (`:284-318`) stamps, for a masked field:
`value_column = "ssn_masked"`, `raw_column = Some("ssn")`, and all three flags
`Some(false)`. The doc at `:190-192` states the rule: "Declared rather than
inferred: nothing may read this off the name, and in particular nothing may read
it off a suffix."

**So the *current* layout already has a raw column, already named in the
descriptor, already declared unreadable.** The flip is a rename of the two
physical columns; it changes which string lands in `value_column` and which in
`raw_column`, and nothing else about the descriptor.

The consequence for this specification is decisive: **the write-path guard can be
written and landed against `storage.rawColumn` before any flip happens, and it is
correct both before and after.** Pre-flip it stops `RETURNING *` from returning
plaintext under `ssn` (which is today's actual leak, since today `ssn` IS the
plaintext column). Post-flip it stops `ssn_raw`. Same code, same descriptor field.

### 2.3 The raw column's name is the platform's choice

`_raw` is absent from `RESERVED_NAMES`, and the note treats that as the inbound
defect to fix. But `RESERVED_NAMES` already contains `ReservedName::Prefix("_")`
and `ReservedName::Prefix("__zs_")` (`query.rs:742`, `:745`), and
`validate_field_name` (`:793-849`) is already called on:

- every filter key (`build_field_condition_with_dialect:5329`)
- every conflict-probe key (`build_conflict_probe_with_dialect:2909`)
- every write-document key (`crud/write_pipeline.rs:44`, `:63`, `:67`)
- every read identifier, transitively (`validate_read_identifier:937`)

Nothing derives the raw column's name from a suffix, by the descriptor's own rule.
**So the raw column can simply be given a name that every existing validator
already refuses**, and the entire inbound problem disappears without touching a
single identifier path. Section 4.2.

---

## 3. The shape of the specification

One sentence: **the write path gets exactly one new site, and it is not in
`query.rs`.**

The read path's answer is `read_column_for` (`query.rs:3402`), one function that
maps a declared field to the column a read touches. The write path's equivalent is
not a column mapper, because writes do not project - they return everything. Its
equivalent is a **row-surface filter**: one predicate over key names, applied once,
at the single point every returned row already passes through.

---

## 4. The specification

### 4.1 Outbound - the row surface

**Decision: not an explicit column list in the SQL. A descriptor-driven filter on
the decoded row, as the last stage of `read_pipeline::apply`.**

#### The surface

New in `crates/zeroship-schema/src/query.rs`, beside `read_column_for`:

```rust
/// Every column name a decoded row may carry across the JS boundary.
///
/// The seven system fields, every declared field's LOGICAL name, and the
/// closed set of synthetic result columns the read builders emit. A
/// physical column that is not one of those - a mask sibling, a raw
/// column, an auxiliary shadow-table key - is not on this surface and is
/// removed before the row is serialised.
pub fn read_surface_columns(schema_hint: &Value) -> BTreeSet<String>
```

Membership, exactly:

1. the seven names in `SYSTEM_FIELD_NAMES` (`query.rs:723-731`);
2. every key of `schema_hint` that is not `is_schema_metadata_key`
   (`query.rs:676-678`, i.e. not `_meta` / `_indexes`);
3. a **closed literal set** of synthetic result columns: `_distance`
   (`query.rs:5007`), `_distance_m` (`:5074`), `__created` (`:6020`).

Point 3 is a closed list, **not** "anything starting with `_`". That is
deliberate: `scope_schema`'s `Only` arm currently retains `key.starts_with('_')`
(`read_pipeline.rs:116`), and section 4.2 gives the raw column a `_`-prefixed
name, so a blanket underscore allowance would re-admit exactly the column this
whole exercise removes. If a new synthetic column is added, it is added here, and
forgetting to means the column is dropped - a visible failure, not a leak.

#### Where it is applied

As the **final** stage of `read_pipeline::apply`
(`crates/zeroship-plugin-db/src/crud/read_pipeline.rs:54-100`), after the unmask
overrides at `:89-97`:

```
1. normalize          (:76)
2. decrypt            (:78-80)
3. wrap masked        (:82-87)
4. unmask overrides   (:89-97)
5. NEW: restrict to surface
```

Last, not first, because stages 2 and 4 need the physical columns. Stage 2 reads
the ciphertext out of the raw column; stage 4 replaces a mask sentinel with the
plaintext it fetched. A strip placed before them would delete their input.

The surface is selected by a new field on `ApplyOptions`
(`read_pipeline.rs:14-30`):

```rust
pub enum RowSurface<'a> {
    /// Declared fields + system fields + synthetics. The default.
    Declared,
    /// An explicit name list. Aggregate result sets only: their keys are
    /// accumulator aliases, which no descriptor declares.
    Projected(&'a [String]),
}
```

with `Default = RowSurface::Declared`. **The default is the restrictive arm**, so
a call site that forgets gets the safe answer, and there is no "unrestricted" arm
to spell at all. The only site that must say anything is `dispatch_aggregate`
(`crud/mod.rs:1790-1798`), which already passes non-default options and already
computes `aggregate_group_fields(&pipeline)` (`:1764`). It needs the accumulator
aliases too, so `build_aggregate_with_soft_delete_with_dialect` (`query.rs:4610`)
returns its `agg_exprs` keys (`:4633`) alongside the `BuiltQuery`.

`dispatch_distinct` needs nothing: `build_distinct_with_soft_delete_with_dialect`
already aliases the physical column back to the logical name
(`query.rs:4901-4906`, `format!("{read} AS {col}")`), so its single result key is
already on the `Declared` surface.

#### Why not an explicit `RETURNING` column list

Three reasons, in order of weight.

1. **It does not close the hole it appears to close.** The rows also feed
   `emit_for_rows` (`crates/zeroship-plugin-db/src/exec.rs:481-550`), which builds
   the broker tuple from `m.keys()` at `:520-524`. On a deployed Postgres app the
   authoritative event source is not that function at all - it is the WAL consumer
   (`emit_for_rows` returns early when the consumer is running, `:501-505`), and
   `wal_consumer::tuple_to_map` (`:635`) zips **every physical column** out of
   pgoutput with no schema in sight. An explicit `RETURNING` list makes the SQL
   narrower and leaves the replication stream exactly as wide. A row-surface
   predicate can be applied to both.
2. **The write path must return system columns the descriptor also declares, and
   "project only declared fields" is therefore ambiguous.** The brief flags this
   and it is real: `id` is minted SDK-side and read back out of the `RETURNING`
   row (`crud/system_fields_pass.rs:32`), `version` and `updated_at` are
   auto-bumped in SQL (`query.rs:5920-5925`), and `deleted_at` is what
   soft-delete writes. All seven are in `SYSTEM_FIELD_NAMES` and none is
   necessarily a descriptor key. The surface therefore unions the two sets rather
   than choosing between them - which a projection list would also have to do, at
   twelve sites, with twelve chances to disagree.
3. **`BuiltQuery` cannot carry the answer.** It is `{ sql, params }`
   (`query.rs:76-80`) and there is no constructor - all 19 sites hand-roll the
   struct literal (`:2942, 3150, 3495, 3589, 4009, 4162, 4230, 4256, 4298, 4449,
   4487, 4529, 4561, 4845, 4920, 5015, 5084, 5944, 6027`). A projection list
   threaded through it is 19 edits and a permanent obligation on every future
   builder.

#### The count-only verbs and the broker

The five `*Many` verbs never hand rows to JS, so stage 5 does not reach them and
does not need to. What does reach a creator from those verbs is the broker's
`changed_columns` list, which `message_to_json` serialises verbatim
(`crates/zeroship-plugin-db/src/broker.rs:939-946`). Post-flip that list names
`ssn_raw` to the subscriber. The values do not escape: `message_to_json` does not
include `new_tuple`.

**Specification:** `message_to_json` filters `changed_columns` through
`read_surface_columns`, so the event names logical fields. One site
(`broker.rs:944`).

**And `ws_frame_for_change` (`broker.rs:986-999`) is deleted.** It is the only
function in the tree that puts `ev.new_tuple` on a wire (`:995`), it carries every
physical column with its value, and it has no caller outside its own module - I
grepped `crates/` and `sdks/` for it and found the definition and nothing else.
Keeping a dead exporter of the raw tuple around while specifying that the raw
tuple must not be exported is the sort of thing that gets wired up later by
someone who reads the function and not this document.

#### Making it unrepresentable rather than merely present

As described, a new write verb could still bypass stage 5 by resolving a
`ResolveValue` from raw rows directly. Close that:

`exec_mutation_with_emit` (`exec.rs:425-440`) returns `RawRows(Vec<Value>)`
instead of `Vec<Value>`. `RawRows` is constructed only inside `exec.rs` (minted on
return at `:439`, after `emit_for_rows` has run over the plain slice at `:438`),
exposes only `len()`, and its unwrapper is
`pub(in crate::crud::read_pipeline) fn into_vec`. Then:

- `read_pipeline::apply` is the only code in the crate that can see the rows;
- `row_count_as_f64` (`crud/mod.rs:504-506`, today `fn(Vec<Value>) -> ResolveValue`)
  takes `RawRows` and calls `len()` - the five count-only verbs are unaffected in
  behaviour;
- **a new write verb that wants to return rows has no way to obtain them except
  through the pipeline**, and therefore no way to skip the surface filter.

Cost: 1 newtype, 2 signatures, 5 trivial call-site adjustments. Benefit: the
outbound property moves from guarded to unrepresentable.

### 4.2 Inbound - the raw column is unnameable, not fenced

**Decision: do not add `_raw` to `RESERVED_NAMES` as the primary defence, and do
not thread `schema_hint` into the filter builders. Name the raw column something
every existing validator already refuses.**

The physical name is the platform's to choose, and the descriptor already forbids
deriving anything from it (`gen_types.rs:190-192`). `RESERVED_NAMES` already
refuses any name beginning `_` (`query.rs:742`) and any name beginning `__zs_`
(`:745`). So:

```
raw column name := cap_ident_name(&format!("__zs_raw__{field}"))
```

`cap_ident_name` is the existing hashing cap at
`crates/zeroship-schema/src/ident.rs:77`, used because Postgres truncates at 63 bytes
and `validate_field_name` permits a 63-byte field name (`query.rs:804-808`). Note
in passing that today's `mask_sibling_column_for_field` (`query.rs:2151-2161`)
does **not** cap: a 60-character masked field already produces a 67-character
sibling that Postgres silently truncates. That is a pre-existing latent collision
and the flip doubles the exposure, so the capped helper is used for both columns.

What this buys, at zero call sites:

| surface | already refuses `__zs_raw__ssn` | via |
| --- | --- | --- |
| `find` / `updateOne` / `deleteMany` filter key | yes | `build_field_condition_with_dialect:5329` -> `validate_field_name:820-847` |
| aggregate `$match` | yes | same path via `build_where:4646` |
| upsert conflict probe key | yes | `build_conflict_probe_with_dialect:2909` |
| insert / upsert document key | yes | `write_pipeline.rs:44` |
| update patch key, incl. `$set` nesting | yes | `write_pipeline.rs:63`, `:67` |
| `select` / `orderBy` / `$group.by` / `distinct` / `$having` | yes | `validate_read_identifier:937` (17 sites) |
| vector `search` / spatial `near` filter | yes | `build_where` at `:5003`, `:5070` |
| SQLite vector search filter | yes | `backend/sqlite/mod.rs:1822` -> `build_where` |

Every one of those already calls `validate_field_name`, today, before the flip.
The inbound half needs **no new parameter, no new check, and no new call site.**

**On the `&Value` versus `Option<&Value>` question the brief asks.** The read path
made absence unrepresentable by taking `&Value`
(`validate_read_identifier:936`, rationale at `:928-935`). The same move on the
write side means threading `schema_hint` into `build_where` (`:5240`),
`build_where_with_dialect` (`:5244`), `build_where_with_dialect_inner` (`:5253`)
and `build_field_condition_with_dialect` (`:5323`), whose 15 production call sites
are `query.rs:3117, 3486, 3992, 4221, 4247, 4282, 4426, 4475, 4512, 4549, 4646,
4909, 5003, 5070` plus `backend/sqlite/mod.rs:1822`. Reaching those requires
adding `schema_hint` to about twelve write-builder signatures that do not take one
(`build_update_one_with_system_fields:3974`, `build_update_many_with_system_fields:4203`,
`build_delete_many:4235`, `build_delete_one_with_dialect:4269`, the four
soft-delete/restore builders at `:4410 :4459 :4496 :4533`,
`build_conflict_probe_with_dialect:2882`, `build_count_with_soft_delete:3473`,
`build_upsert_with_dialect:5823`) plus their delegating wrappers.

I verified that every plugin-db caller of those builders already holds the
descriptor entry - each is inside a `collection_schema(...).and_then(|schema| ...)`
closure or has resolved it earlier (`crud/mod.rs:785, 1220, 1456, 1510, 1551,
1596, 1639, 1695, 1768, 1838, 1927`). **So the change is mechanical and would
work.** It is roughly 16 signatures and 18 call sites, and it is not materially
smaller than the naive fix. I am not specifying it, because naming the column
`__zs_raw__ssn` achieves a strictly stronger property for zero edits: a fence can
be added to a surface someone forgets to fence, whereas a name no validator
accepts is refused by surfaces nobody has written yet.

**`ReservedName::Suffix("_raw")` is still added** (`query.rs:766`), for exactly the
reason the query-by-plaintext review gives for `_lookup`
(`docs/reviews/2026-08-27-query-by-plaintext.md:217-221`): anti-collision only, so
a creator cannot declare `ssn_raw` and confuse a human reader. Nothing derives from
it.

**One ordering invariant this depends on, which must be pinned by a test.**
`validate_user_doc_keys` and `validate_update_patch_keys`
(`write_pipeline.rs:41-71`) run at `:101-111`, **before** the encryption and mask
passes at `:226-239` deposit their own reserved-suffix keys into the same
document. That ordering is why the platform's own writes are not refused by the
platform's own validator. It is load-bearing today for `ssn_masked` and would be
load-bearing for `__zs_raw__ssn`. Moving the validation after the passes breaks
every masked write, and nothing currently states that.

### 4.3 Live-query subscriptions

The three candidate answers and their failure modes:

| answer | failure mode |
| --- | --- |
| match the mask | `find({ssn: "123-45-6789"})` never fires again. **Silent.** |
| match the raw | the predicate becomes an equality oracle over plaintext in the broker, which is the channel the flip exists to close, relocated. |
| refuse to register | loud, and kills reactive queries on any collection with a masked column. |

**Decision: none of the three. Lower the predicate's column, and downgrade to
coarse-grained when the lowering is not sound.**

`normalise_filter` (`crates/zeroship-plugin-db/src/read_set.rs:220`) gains
`schema_hint: &Value` and, per conjunct:

- if `read_column_for(key, schema_hint) != key` - the field is masked - and the
  operator is `Eq`, rewrite `Conjunct::column` to the physical value column and
  the operand to `apply_mask_kind(kind, operand)`. The WAL tuple carries the mask
  under that name, the operand is masked the same way, and equality holds
  **whenever the underlying values were equal**. It also holds for values that
  merely share a mask, which is a false positive: a wider fanout, and the module's
  own contract says false positives in the widening direction are the acceptable
  side (`read_set.rs:215-219`).
- if the operator is a range (`Gt`/`Gte`/`Lt`/`Lte`), return `None` -
  coarse-grained. A range over a mask is not a range over the value and no
  rewriting makes it one. Coarse-grained means every row in the collection wakes
  the subscription, which is correct-and-slow, the bias `read_set.rs:215-219`
  already declares.

Cost: 1 signature (`read_set.rs:220`) plus 3 production call sites of
`record_if_active` (`crud/mod.rs:603`, `:1751`, `:1916`), each of which already has
`binding` and `collection` in scope.

**What this does not fix.** `matches_text` compares the JSON operand's canonical
text against the pgoutput text encoding (`read_set.rs:129`). For a mask-only field
the tuple carries the mask string and the comparison is string-vs-string, which
works. For an **encrypted** masked field the tuple's value column also carries the
mask string (the mask is computed from plaintext and stored in the clear -
`mask_pass.rs:497-516` and the CDC contract test at `broker.rs:1863-1899`), so it
also works. I found no case where the rewrite compares against ciphertext. I did
not test this against a live WAL stream.

### 4.4 Constraints and indexes

**SC-6 item 2 says constraints follow the raw column. That is right for a
mask-only field, wrong for a randomised-encrypted one, and the SDK already knows
it.**

`sdks/db/src/types.ts:1146-1163` refuses `.unique()` on
`encrypted.mode === "randomised"` with `UNIQUE_ENCRYPTED_RANDOMISED_UNSUPPORTED`,
and the comment at `:1147-1152` gives the reason: a fresh nonce per write means
the same plaintext produces different ciphertext per row, so ciphertext equality
enforces nothing. A `CREATE UNIQUE INDEX` on `ssn_raw` for such a field is
satisfied by every possible pair of rows. **It fails open**, which is worse than
the mask-column version, which fails closed noisily.

Specification, per storage shape:

| field shape | `.index()` | `.unique()` |
| --- | --- | --- |
| mask-only (`.mask()`, no `.encrypted()`) | raw column | **raw column** - plaintext, equality is real |
| deterministic-encrypted + masked | raw column | raw column - ciphertext equality holds (`crates/zeroship-plugin-db/src/encryption/aead.rs:92-112`, synthetic nonce `HMAC-SHA256(k_siv, aad \|\| plaintext)[..12]`) |
| randomised-encrypted + masked | raw column (useless but harmless) | **refused at declare time**, as today (`types.ts:1153-1160`). It becomes supportable only when a keyed lookup column exists; that is item 1's design (`docs/reviews/2026-08-27-query-by-plaintext.md:666-688`), not this one. |
| `.mask({kind:"none"})` | own column | own column - no sibling exists (`mask_sibling_column_for_field:2155-2158` returns `None`) |

The emitter change is in `build_create_indexes` (`query.rs:1814`), whose unique
arm currently uses `col_list = quote_ident(field)` at `:1951` and whose
non-unique arm the same at `:1967`. Both become `quote_ident(raw_column_or_field)`.
The auto-emitted sibling index at `:1983-1989` inverts: the index that today
exists on the mask sibling to make masked reads cheap must now exist on the value
column, which post-flip is the field's own name, so that arm collapses.

**Foreign keys: not specified here.** An FK needs a referenced unique key. Pointing
one at a raw column makes the raw column a join key, which means it appears in
`ON CONFLICT` targets and in FK error messages, and I did not analyse the
disclosure that implies. `build_fk_clause` (`query.rs:1588`) and
`build_add_foreign_key` (`:1535`) both name the declared field today. This is an
open item, not a decision.

**This item interacts with the upsert path in a way section 6 records as a
finding.**

### 4.5 The SQLite introspector's missing `else`

**Decision: delete `parse_mask_sentinels` and the SQLite `SchemaIntrospect` impl
outright, and fix the missing `else` on the Postgres arm.**

Two different answers because they are two different situations, which the
blocking note conflates.

- **SQLite.** The function and its caller are both `#[cfg(any(test, feature =
  "test-helpers"))]` (`backend/sqlite/mod.rs:2201`, `:804`). The descriptor is the
  data plane's sole schema authority
  (`crates/zeroship-plugin-db/src/descriptor.rs:1-32`), and that module records
  that on SQLite it always has been. So the code is a test-only reimplementation
  of a fact the descriptor states. Fixing its missing `else` would preserve a
  second source of truth for the mask sibling's name, which is exactly the
  duplication `storage.valueColumn` exists to end. Delete it, and delete the
  `MaskMeta` recovery it feeds. Its tests
  (`backend/sqlite/mod.rs:3099, 3115, 3130, 3138, 3149, 3161`) go with it.
- **Postgres.** `read_live_schema` is production - it is the migration engine's
  introspection input, reached from `backend/postgres.rs:344`. Its
  `column.ends_with("_masked")` filter (`diff.rs:671`) and `strip_suffix("_masked")`
  (`:716`) must read `storage.valueColumn` / `storage.rawColumn` from the declared
  schema instead of parsing the name, and the fall-through arm at `:671` must
  `tracing::warn!` like the malformed-sentinel arm at `:737-744` rather than
  discarding silently. A `__zsmask:` comment on a column the descriptor does not
  claim as a value column is a real inconsistency between the database and the
  artifact, and it is precisely the thing an introspector exists to notice.

### 4.6 The AAD, and what the flip actually costs in encrypted data

`canonical_aad(collection, column, row_pk_bytes)`
(`crates/zeroship-plugin-db/src/encryption/aad.rs:75-99`) length-prefixes the
column name into the AEAD tag at `:97`. The encryption pass passes the **logical
field name** (`crud/encryption_pass.rs:200`, `:337`; pinned by
`crud/write_pipeline.rs:734`, `canonical_aad(collection, "ssn", Some(id))`).

`FieldStorage`'s doc states the consequence in its own words
(`gen_types.rs:161-169`): "moving an encrypted value to another column is a
re-encrypt, not a rename ... an `ALTER TABLE ... RENAME COLUMN` leaves every
stored cell authenticated under the old name and the table fails tag verification
on every row."

**Specification: the AAD keeps binding the LOGICAL field name, permanently, and
the flip does not touch it.** The physical location moves; the authenticated name
does not. This is free, it is the behaviour today, and it means the flip requires
no re-encrypt of any existing row - only a column rename plus a backfill of the
mask into the vacated `ssn`.

It does, however, make `gen_types.rs:157-169` **false as written**: that comment
says the AEAD binds `raw_column` when present and the field's own column
otherwise, which today are the same string and post-flip are not. The comment must
change in the same commit as the flip, or the next reader will "fix" the AAD to
match it and destroy every ciphertext in the deployment. This is also why
`docs/reviews/2026-08-27-descriptor-specification.md:648-655` specifies an
`aadColumn` field that the shipped struct does not have (see section 0): the
specification anticipated this divergence and the implementation resolved it the
other way, by not moving the AAD at all. **Recording the logical-name rule in the
comment is strictly better than adding `aadColumn`**, because a field that can
disagree with the rule is a second source of truth for one fact - the same
argument the query-by-plaintext review makes about `lookupColumn`
(`docs/reviews/2026-08-27-query-by-plaintext.md:241-243`).

---

## 5. The count

### Sites my design changes

| # | site | what |
| --- | --- | --- |
| 1 | `crates/zeroship-schema/src/query.rs` (new fn) | `read_surface_columns` |
| 2 | `crates/zeroship-plugin-db/src/crud/read_pipeline.rs:14-100` | `RowSurface`, stage 5 |
| 3 | `crates/zeroship-plugin-db/src/crud/mod.rs:1790-1798` | aggregate passes `Projected` |
| 4 | `crates/zeroship-schema/src/query.rs:4610` (+2 wrappers `:4578`, `:4592`) | aggregate builder returns its aliases |
| 5 | `crates/zeroship-plugin-db/src/exec.rs:425-440` | `RawRows` newtype minted here |
| 6 | `crates/zeroship-plugin-db/src/crud/mod.rs` x5 | `row_count_as_f64` takes `RawRows` (`:1409, 1529, 1612, 1714` + CAS at `:1313`) |
| 7 | `crates/zeroship-plugin-db/src/broker.rs:944` | `changed_columns` mapped to logical names |
| 8 | `crates/zeroship-plugin-db/src/broker.rs:986-999` | delete `ws_frame_for_change` |
| 9 | `crates/zeroship-plugin-db/src/read_set.rs:220` + 3 callers (`crud/mod.rs:603, 1751, 1916`) | predicate lowering |
| 10 | `crates/zeroship-migrate-core/src/render/gen_types.rs:284-318` | raw column named `__zs_raw__<field>`, capped |
| 11 | `crates/zeroship-schema/src/query.rs:2151-2161` + its 5 emitter callers (`:1282, 1708, 1994, 2217, 2253`) | emit both physical names, capped |
| 12 | `crates/zeroship-schema/src/query.rs:1949-1967` | indexes on the raw column |
| 13 | `crates/zeroship-schema/src/query.rs:766` | `Suffix("_raw")`, anti-collision only |
| 14 | `crates/zeroship-schema/src/diff.rs:671, 716` | PG introspector reads `storage`, warns on fall-through |
| 15 | `crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:804-846, 2201-2248` | deleted |
| 16 | `crates/zeroship-plugin-db/src/crud/mask_pass.rs:150, 469`; `encryption_pass.rs:295` | read `storage`, not `format!("{col}_masked")` |
| 17 | `crates/zeroship-plugin-db/src/crud/unmask.rs:466, 509, 567, 604` | read the raw column, not the logical name (see section 6.3) |
| 18 | `crates/zeroship-migrate-core/src/render/gen_types.rs:157-169` | comment corrected per section 4.6 |

**About 40 edits across 9 files, of which 6 are one-line and 15 are deletions or
comment corrections.** That is not dramatically smaller than 34-plus-4 as a raw
edit count, and I will not pretend otherwise.

**The number that matters is the second one the brief asks for.**

### Sites a 35th write verb changes: ZERO

- **Outbound.** It cannot obtain rows except through `read_pipeline::apply`
  (section 4.1, `RawRows`), and `apply`'s default surface is `Declared`.
- **Inbound.** It calls `validate_field_name` on its identifiers because every
  builder already does, and the raw column's name is refused by the reservation
  table that function already consults.
- **Live queries.** It calls `exec_mutation_with_emit`, whose broker leg is
  filtered at one serialisation site.

Contrast the naive design: an explicit `RETURNING` column list is 12 SQL sites now
and one more per verb forever; a `schema_hint` fence on the filter builders is 15
call sites now and one more per builder forever.

### What is not zero

The 5 `mask_pass` / `encryption_pass` / `unmask` sites (rows 16-17) that today
`format!("{col}_masked")` are a one-time conversion to `storage`. After that they
are single derivation sites, like `read_column_for`. But **a new pass that needs
the sibling name would be a new site**, and nothing prevents someone writing
`format!` again. The only structural defence would be to remove the ability to
build a column name from a field name at all, which means removing
`mask_sibling_column_for_field` (`query.rs:2151`) from the data plane's reach - it
lives in `zeroship-schema`, which plugin-db depends on. I did not find a way to do
that without splitting the crate. **Guarded, not unrepresentable, and I could not
close it.**

---

## 6. Unrepresentable versus guarded

| area | property achieved | why |
| --- | --- | --- |
| **Outbound row surface** | **Unrepresentable** | `RawRows` can only be unwrapped inside `read_pipeline`, and `RowSurface` has no permissive arm. Code that returns rows without the surface filter does not compile. |
| **Outbound: broker `changed_columns`** | Guarded | One filter at `broker.rs:944`. A second serialiser could be added without it. `ws_frame_for_change` is deleted so there is currently exactly one. |
| **Inbound identifiers** | **Unrepresentable** | The raw column's name is refused by `validate_field_name` (`query.rs:820-847`) via a reservation that predates the flip. No surface can accept it, including surfaces not yet written. |
| **Inbound: `$match` / probe asymmetry** | **Dissolved** | It stops mattering. `build_where` still takes no schema, but there is no longer a name it could accept that is dangerous. |
| **Live-query predicate** | Guarded | `normalise_filter` takes `&Value` (not `Option`), so "no schema" is unspellable, but a *wrong* lowering is a silent false negative and only a test catches it. |
| **Constraints and indexes** | Guarded | `build_create_indexes` names the raw column at two sites. A third index emitter would have to be written correctly. `.unique()` on randomised-encrypted stays refused at declare time in the SDK (`types.ts:1153-1160`), which is a guard in TypeScript with no Rust counterpart. |
| **SQLite introspector** | **Dissolved** | Deleted. A defect in deleted code has no property. |
| **PG introspector** | Guarded | The fall-through warns instead of discarding, which is detection, not prevention. |
| **The `format!("{col}_masked")` family** | Guarded, and I could not close it | Section 5, last subsection. |
| **AAD stability** | Guarded by a comment | Nothing in the type system stops someone passing the physical column to `canonical_aad`. A test that encrypts under the logical name and decrypts after a simulated rename is the only real defence. |

Three unrepresentable, one dissolved-by-deletion, one dissolved-by-naming, five
guarded. The read path achieved unrepresentability for its single question ("is
there a schema?"). The write path has more questions and I could not make them all
unspellable.

---

## 7. A finding the reviews could not produce

The brief asks for something that comes from specifying rather than reviewing, and
says an invented one is worse than none. I have three. They are all composition
facts - true of how the pieces fit, not of any one file - which is why reading
`query.rs` and `mask_pass.rs` separately does not surface them.

### 7.1 The upsert conflict probe inverts, in both directions, silently

This is the one I would lead with.

The upsert path is the **only production code that transforms a filter operand in
Rust before comparing** (`crud/write_pipeline.rs:452-516`, noted as such at
`docs/reviews/2026-08-27-query-by-plaintext.md:154-160`). Post-flip it breaks
twice, in opposite directions, and neither is visible:

1. **False negative.** `rewrite_upsert_doc_id_to_existing_row_id` builds a filter
   from the conflict field names (`:474-479`), encrypts the operand for
   deterministic fields (`:485-496`), and hands it to
   `build_conflict_probe_with_dialect`, which emits `WHERE "ssn" = $1` from the
   **logical** name (`query.rs:2910`, `col = quote_ident(field)`). Post-flip
   `"ssn"` is the mask column and `$1` is ciphertext. No row matches. The function
   returns early at `:511-513`, the document keeps its freshly minted id, and the
   upsert **inserts a duplicate instead of updating**.
2. **False positive.** `build_upsert_with_dialect` builds `ON CONFLICT (...)` from
   the same logical names (`query.rs:5914-5918`). Post-flip that names the mask
   column, and section 4.4's point applies: two different SSNs sharing
   `***-**-1234` collide. The upsert then **overwrites an unrelated row** and
   returns it as though it were the caller's.

Both are write-side. Both produce a plausible-looking row. Neither raises an
error. Together they mean an `upsert` on a masked conflict field can duplicate and
clobber in the same deployment depending on which field shape it hits. **Section
4.4's index placement is not an optimisation - it is what stops case 2**, and the
probe's column lowering is what stops case 1. Neither review connected the
constraint item to the upsert path.

### 7.2 The flip makes two passes write the same key, in an order nothing enforces

Today `WriteStages::apply_to_doc` (`crud/write_pipeline.rs:213-255`) runs the
encryption pass then the mask pass, each behind its own independent gate
(`if self.has_encrypted` at `:226`, `if self.has_masked` at `:237`). They write
**disjoint keys**: encryption replaces `row["ssn"]` with ciphertext; the mask pass
*inserts* `row["ssn_masked"]` and leaves `row["ssn"]` alone
(`mask_pass.rs:147-156`, `obj.insert(sibling, ...)` at `:155`).

Post-flip both must target `row["ssn"]` - the mask pass writes the mask there, and
the authoritative value must move to the raw column. That is a **swap of two keys
in one map, executed by two passes that cannot observe each other**. Three
consequences:

- For an **encrypted** masked field, the relocator must be the encryption pass
  (it is what produces the ciphertext). For a **mask-only** field there is no
  encryption pass at all, so the relocator must be the mask pass. **The owner of
  the physical placement therefore differs by field shape within one collection**,
  and `WriteStages`' gates are per-collection booleans
  (`schema_has_encrypted_columns` / `schema_has_masked_columns`, `:199-201`).
- The mask pass's plaintext source already has a documented failure arm for this:
  "The row value MAY already be the base64 ciphertext if the encryption pass ran
  first AND the sidechannel was not populated - that would be a contract
  violation" (`mask_pass.rs:121-125`). Today that arm masks a ciphertext string
  and stores it in the sibling: ugly, recoverable. Post-flip the same arm would
  **overwrite the ciphertext with a mask of itself** and, if the relocation is
  conditional on the sidechannel being populated, the ciphertext is gone. That is
  unrecoverable data loss on a write that returns success.
- The safe shape is therefore that **exactly one stage owns physical placement,
  and it runs after both**: a new relocation stage, driven by
  `storage.rawColumn`, that moves whatever is in the logical slot to the raw
  column and writes the mask into the logical slot, unconditionally, for every
  field with a `rawColumn`. Both existing passes then keep writing the logical
  name and know nothing about the flip. **This is the write-side twin of
  `read_column_for`** and it is the one new site the write path genuinely needs.

I did not include this stage in section 4 because it belongs to the flip, not to
the guard - the guard in section 4.1 is correct with or without it. If the flip
proceeds, this stage is item zero.

### 7.3 A guarantee that reads as protection but is not: `rawProjectable: false`

`raw_projectable` is stamped `Some(false)` on every masked field today
(`gen_types.rs:305`), and its doc says it declares whether "a creator-facing
projection may return" the raw column (`:199-202`). The blocking note observes
that "nothing reads it" (`sc6:368`). Correct - and the reason is worse than
"unimplemented".

`raw_projectable` cannot be enforced by the projection builder, because the
projection builder is not where raw columns enter the result. They enter through
`RETURNING *`, which has no projection builder. So the flag describes a gate on a
path the value does not travel. Implementing it faithfully - teaching
`implicit_read_projection_parts` (`query.rs:3344-3366`) to honour it - would
change nothing at all, and the resulting code would read like a defence.

`read_surface_columns` (section 4.1) is what the flag actually means, and it is
the reason I specified the surface as a row predicate rather than a projection
rule. Worth stating explicitly in the descriptor's doc, because a flag that is
honoured somewhere useless is harder to notice than one honoured nowhere.

---

## 8. The case against the flip

The brief asks for this at full strength and warns that the weak form is "guard
`orderBy` instead". Here is the strong form.

### 8.1 What the flip buys, stated exactly

One thing: it closes the filter oracle in which
`find({ssn: {$gt: v}})` plus `orderBy` plus `limit` binary-searches a value the
caller cannot read (`sc6:218-244`). It closes it by making the ignorant path safe:
a builder that knows nothing about masking touches the mask.

Since decisions 7 and 8, it also claims a second thing (`sc6:264-291`): the
descriptor is now the sole schema authority and the data plane never checks the
catalog, so if the descriptor is stale about masking, the physical layout is the
only thing preventing a plaintext read.

### 8.2 The second claim does not survive contact with the code

The stale-descriptor argument is that under the flip, a descriptor that is wrong
about masking reads a masked column and leaks nothing (`sc6:283-285`).

But a descriptor that is wrong about masking is wrong about `storage.valueColumn`
too - it is the *same field* of the *same JSON object*, emitted by the same
function (`gen_types.rs:284-318`). `read_column_for` reads `storage.valueColumn`
(`query.rs:3405-3410`). If the descriptor says the field is unmasked, it also says
`valueColumn == "ssn"` and `rawColumn` is absent. Post-flip that read returns the
mask, which is safe. Pre-flip that read returns the plaintext, which is not.

So the claim is **true**, and its magnitude is: it converts one specific
descriptor-staleness window from plaintext disclosure to mask disclosure. That is
real. But note what it does not cover - a stale descriptor also skips the decrypt
stage (`encryption_pass.rs:279-281` iterates only fields the schema declares
`encrypted`), skips the mask wrap (`mask_pass.rs:438-441`), and, under my section
4.1, **strips the field from the row entirely** because it is not on the declared
surface. A stale descriptor already fails closed on three of four stages. The flip
hardens the fourth.

### 8.3 What the flip costs, totalled

The read-side cost is the 18-item list the item-1 design already enumerates. The
write-side cost is this document. Together:

- **Silent capability losses**: ordered comparison on the real value; `orderBy` on
  the real value; equality lookup by real value (which requires the entire
  query-by-plaintext design - a new physical column, a third HKDF leg, a new
  collection verb, an audit row shape - to restore;
  `docs/reviews/2026-08-27-query-by-plaintext.md:177-475`).
- **Silent correctness inversions introduced**: upsert duplicate-insert and
  upsert clobber (section 7.1); live-query subscriptions stop firing
  (section 4.3); unique constraints enforce the wrong thing (section 4.4).
- **A data-loss hazard introduced**: two passes writing one key with no ordering
  contract (section 7.2).
- **A latent AEAD hazard introduced**: the physical column moves while the
  authenticated name must not, held together by a comment (section 4.6).
- **Migration work not yet scoped**: a rename plus a mask backfill on every masked
  column of every collection, plus the `aadColumn` question in section 0.

And the leak the flip **opens** on the way, which is what made the note blocking:
for a mask-only field, plaintext returned from every row-returning write verb, in
a key the generated `Row<S>` does not declare (`sc6:365-369`). That is strictly
worse than what it retires, until section 4.1 lands.

### 8.4 The alternative, at its strongest

Do not move any data. Instead:

1. **Refuse ordered comparison on a masked column.** `find({ssn: {$gt: v}})`
   raises a typed error. This is SC-6's first rejected option (`sc6:254-258`) and
   it kills the binary-search channel, which is the one that recovers a full value
   cheaply.
2. **Refuse `orderBy` on a masked column** - already done, at `query.rs:5526-5537`,
   by routing the sort term through `read_column_for` (L26). The ordering leak is
   closed today, in the shipped tree, without any flip.
3. **Leave equality alone.** Equality on a masked column is a guess-and-confirm
   oracle, which is what a login form is. It is also the query creators actually
   write.

The cost of that alternative is: **one schema-hint parameter on
`build_field_condition_with_dialect`** (`query.rs:5323`) and the 15 call-site
thread-through that section 4.2 declined - about 16 signatures and 18 call sites,
all mechanical, all inside two crates, with no data movement, no migration, no
re-encrypt, no pass reordering, no upsert inversion, no subscription breakage, no
index relocation, and no new leak to close first.

**And it keeps `find({ssn: "123-45-6789"})` working**, which the flip deletes and
then spends an entire second design (query-by-plaintext) partially restoring
behind a new verb, a new column, a new key and a new audit row.

### 8.5 Why it loses - and it does, but not by much

Two reasons, and only two.

**The asymmetry argument.** The flip's real claim (`sc6:309-323`) is not about any
particular query - it is that a system with plaintext in the naturally-named
column requires *every* code path to ask "is this masked?", and the tree has
already proved it will not. The evidence is on the page: `read_column_for` exists
and is right; `build_where` sits ten lines away and has never taken a schema; the
projection honours the mask and `RETURNING *` does not. That is three
schema-aware paths and three schema-blind ones in one file. Guarding the filter
builder fixes the three that exist. It does not fix the seventh.

The counter-argument to the counter-argument, which I find nearly as strong: my
section 4.2 shows that **naming the raw column something no validator accepts
achieves the same asymmetry-proofing without moving the data.** A path that has
never heard of masking cannot name `__zs_raw__ssn` in a filter, because
`validate_field_name` refuses it, and every path already calls that. If the
argument for the flip is "make the ignorant path safe", the ignorant path can be
made safe by renaming the *raw* column rather than by swapping which column holds
what. That is a strictly smaller change with the same fail-closed property on the
inbound side.

Where it does **not** give the same property is outbound: `RETURNING *` returns
`__zs_raw__ssn` just as happily as `ssn_raw`, so section 4.1 is needed either way.
Which is the point - **section 4.1 is needed either way, and it is most of the
write-path work.**

**The stale-descriptor argument**, weakened as section 8.2 describes but not
eliminated.

### 8.6 What I actually think

I would not implement the flip.

The honest accounting is: the flip's unique benefit, after section 8.2's
weakening and section 8.5's rename observation, is **one narrowed
descriptor-staleness window**. Everything else it is credited with is achievable
by (a) the raw-column rename in section 4.2, (b) the row surface in section 4.1,
and (c) refusing ordered comparison on masked columns - none of which moves a
byte of stored data, and all three of which are worth doing on their own merits
whether or not the flip proceeds.

Against that one benefit: three silent write-correctness inversions to fix
(7.1, 4.3, 4.4), one data-loss hazard to design around (7.2), one AEAD invariant
to hold by convention (4.6), a migration of every masked column in every
collection, and the deletion of equality-by-real-value until a second, larger
design ships to restore it.

If the flip is cancelled, sections 4.1, 4.2, 4.5, 7.3 and the `_masked`-to-
`storage` conversion in row 16 of section 5 **all still apply and are all still
worth landing**, because they fix defects that exist in the current layout:
`RETURNING *` returns plaintext under `ssn` **today**, the PG introspector
discards sentinels silently **today**, and `rawProjectable` describes a gate
nothing can enforce **today**. That is the strongest signal I can offer: most of
this specification is not flip work. It is work the flip made visible.

**Whether the flip proceeds is the operator's call and I have not made it.**

---

## 9. Acceptance shape, if it proceeds

Each of these fails before the change and passes after. None is a mutation of a
test's own fixture.

1. `insert` on a mask-only field returns a document whose key set is exactly
   `SYSTEM_FIELD_NAMES` union the declared fields. Asserted on the key set, not on
   the absence of one name, so a differently-named raw column cannot pass it.
2. The same, for all seven row-returning write verbs, table-driven from the list
   in section 2.1, with an arm count that fails if a verb is added and not listed.
3. A filter, `select`, `orderBy`, `$group.by`, `$match`, conflict-probe key and
   insert-document key each naming the raw column: all seven refused, with the
   same error code, and the test derives the name from `storage.rawColumn` rather
   than spelling it.
4. `upsert` on a masked deterministic-encrypted conflict field updates the
   existing row rather than inserting a second (section 7.1 case 1).
5. Two rows whose masked values collide (`***-**-1234`) and whose real values
   differ both insert successfully under a `.unique()` declaration
   (section 7.1 case 2).
6. A subscription with `find({ssn: <value>})` fires on an insert of that value;
   one with `find({ssn: {$gt: v}})` registers coarse-grained rather than silently
   never firing (section 4.3).
7. Encrypt a value, perform the flip's rename, decrypt: succeeds. This is the
   AAD test section 6 says is the only real defence for row 10 of that table.
8. A `__zsmask:` comment on a column the descriptor does not name as a value
   column produces a `tracing::warn!` (section 4.5, PG arm).
