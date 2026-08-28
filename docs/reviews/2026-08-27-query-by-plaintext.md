# Query by plaintext: the explicit lookup API the storage flip owes

**Date:** 2026-08-27

**Status:** DESIGN. Nothing here is implemented; no code was changed to produce
it.

**Scope:** SC-6 owed item 1 only
(`docs/proposals/2026-08-26-sc6-ceiling-read-contract.md:338-343`). Items 2
(constraints and indexes follow the raw column), 3 (document the behaviour
change) and 4 (the AAD binds the column name) belong to someone else. Where this
design produces a finding that lands on one of them, it is flagged as a
cross-item finding and not designed here.

**The question this answers.** After the flip, `ssn` holds the masked value,
`ssn_raw` holds the real one, and `ssn_raw` cannot be named in a filter, a
projection or a sort. "Find the account for this SSN" therefore has no
expression. SC-6 says that is the one way the decision can fail in practice.
This document specifies the API that keeps it expressible, argues why that API
does not hand back the oracle the flip closed, and states exhaustively what it
does not restore.

---

## 0. Facts this rests on, measured against the tree

Every claim below is cited. Three of them contradict something in the brief or
in a sibling document, and those are marked. Nothing in this section is
inferred.

### 0.1 The mask policy and the authorization path

- `MaskPolicy` is `role -> set of classifications`
  (`crates/zeroship-plugin-db/src/crud/mask_policy.rs:83-87`).
  `MaskPolicy::allows` is at `:117-125`; an absent key is TOP for `auto` and
  BOTTOM for every other role (`:118-124`).
- `check_unmask_authorization` is synchronous, takes
  `(app_id, actor, classification)`, and reads a thread-local
  (`crates/zeroship-plugin-db/src/crud/unmask.rs:290-308`). An absent actor
  denies (`:295-297`). No policy at all admits only `kind == "auto"`
  (`:302-306`).
- `RESERVED_SYSTEM_ACTOR_KINDS` is exactly `["auto"]` (`unmask.rs:265`) and
  `sanitize_app_actor` is `unmask.rs:277-288`. **These are the correct line
  numbers.** The brief cites `:282-303` and SC-6 cites `:280-303`; both are off
  against this tree. The behaviour they describe is unchanged.
- `dispatch_find` applies `sanitize_app_actor` to the app-supplied actor before
  it can reach authorization (`crates/zeroship-plugin-db/src/crud/mod.rs:617-619`).
  That is the DB-3 patch, and any new verb taking an `actor` through V8 must
  route through the same helper or it reproduces DB-3 verbatim.
- The actor is otherwise **self-asserted**: nothing binds it to `env.auth`, so
  an app handler may present `{ actor: { kind: "admin" } }` and receive whatever
  the app's own policy grants `admin`. SC-6 states this outright
  (`sc6:652-672`).

### 0.2 The audit path

- `write_audit_unmask_row` (`unmask.rs:726-827`) inserts
  `(actor_id, actor_role, collection, row_pk, "column", classification, reason,
  outcome)`; `outcome` is CHECK-constrained to `('granted','denied')`
  (`unmask.rs:861`, SQLite `:912`).
- The granted row is written **after** the plaintext is in hand
  (`unmask.rs:424-428`) so a SELECT failure leaves no ghost row; the denied row
  is written **before** the refusal (`unmask.rs:401-405`).
- The table is provisioned by the runtime with `CREATE TABLE IF NOT EXISTS` on
  every dispatch (`ensure_audit_unmask_table`, `unmask.rs:838-946`).
- Bulk and query-hint dispatches reuse the same table and smuggle the dispatch
  shape into a `reason` prefix - `[bulk_unmask]` (`unmask.rs:1203-1207`),
  `[query_hint]` (`unmask.rs:1429-1433`) - with the row-pk slot carrying either a
  comma-joined PK list (`:1175-1180`) or the literal `"[query_hint]"`
  (`:1441`).
- The audit write takes a **pool** connection (`unmask.rs:757`, `:774`), not the
  caller's transaction connection. So does the plaintext fetch
  (`query_postgres_pool_with_autocommit_role`, `unmask.rs:570-572`, which calls
  `pool.get()` and opens its own transaction,
  `crates/zeroship-plugin-db/src/exec.rs:293-318`). Consequences in section 6.4.
- The data pool is 8 connections (`crates/zeroship-plugin-db/src/lib.rs:998`,
  `Pool::connect(&url, 8)`). **SC-6 cites `lib.rs:862` for this**
  (`sc6:152-153`); the line has moved, the value has not.

### 0.3 The read and filter surfaces

- The WHERE builder takes no schema: `build_where_with_dialect(filter, params,
  dialect)` (`crates/zeroship-schema/src/query.rs:5244-5251`). The read
  projection does take one (`read_column_for`, `query.rs:3402-3418`, which reads
  `storage.valueColumn` from the descriptor and only falls back to suffixing when
  the field carries no `storage` block), and that
  asymmetry is the defect the flip removes.
- The filter operator vocabulary is data, matched by string:
  `$eq $ne $gt $gte $lt $lte` (`query.rs:5338-5369`), `$in`/`$nin` capped at
  `MAX_MEMBERSHIP_LIST_LEN = 100` (`query.rs:606`, `:5370-5445`), `$exists`
  (`:5446`), `$like`/`$ilike` (`:5456-5468`), plus `$and`/`$or`/`$not`
  (`:5267-5300`).
- `MAX_QUERY_LIMIT = 500` and an omitted limit defaults to it
  (`query.rs:595`, `effective_query_limit` at `:612-614`, applied in
  `dispatch_find` at `mod.rs:608-610`).
- The read projection is an explicit allowlist with no "no schema" arm
  (`implicit_read_projection_parts`, `query.rs:3345-3366`), so a physical column
  that is not a declared field is not projected.
- `RESERVED_NAMES` (`query.rs:738-766`) already reserves the `_masked` suffix
  (`:754`) and is enforced **at filter time** as well as declaration time
  (`validate_field_name` called from `build_field_condition_with_dialect`,
  `query.rs:5329`), which is why `find({ ssn_masked: ... })` is already refused.

### 0.4 The descriptor already models the flip

`FieldStorage` (`crates/zeroship-migrate-core/src/render/gen_types.rs:170-210`)
carries `value_column`, an optional `raw_column`, and three declared capability
flags on the raw column - `raw_filterable`, `raw_sortable`, `raw_projectable`
(`:188-202`) - with the doc stating they are "Declared rather than inferred:
nothing may read this off the name, and in particular nothing may read it off a
suffix" (`:190-192`). `field_storage` stamps them `Some(false)`
(`:295-308`). The specification's worked example of the flipped shape is
`docs/reviews/2026-08-27-descriptor-specification.md:632-665`.

**This design adds a fourth physical column to that block and reuses the same
capability-flag mechanism.** It does not invent one.

### 0.5 Deterministic encryption: what is actually built

**This corrects the brief.** The brief says deterministic mode "is possible" for
ciphertext equality and is "DEFERRED, not built". The first half is stronger
than stated and the second half is true for a different reason than implied.

- Deterministic encryption **is implemented at the crypto layer and does produce
  byte-identical ciphertext**. The nonce is synthetic:
  `HMAC-SHA256(k_siv, aad || plaintext)[..12]`
  (`crates/zeroship-plugin-db/src/encryption/aead.rs:92-112`, key material
  `AeadKey { k_enc, k_siv }` at `:39-53`). The randomised arm samples from
  `OsRng` (`aead.rs:62-75`). The AAD drops `row_pk` for deterministic mode
  (`crud/encryption_pass.rs:200-208`, `encryption/aad.rs:75-98`), so the whole
  packed blob - version byte, nonce, ciphertext, tag - is identical for identical
  plaintext. Pinned by `deterministic_round_trip_and_repeatability`
  (`aead.rs:239-254`) and, over the real write pass,
  `deterministic_same_plaintext_yields_same_ciphertext`
  (`crud/encryption_pass.rs:788-826`).
- What has no implementation is the **query** half. The filter path never
  encrypts the operand; `build_where_with_dialect_inner` has no key access and no
  schema (`crates/zeroship-plugin-db/src/crud/bytes_pass.rs:47-57` states this
  in the tree's own words).
- **The mode also does not survive the artifact pipeline.** `ColType::Encrypted`
  carries only `of` (`crates/zeroship-migrate-ir/src/ir.rs:670`); the fold
  discards the mode (`crates/zeroship-migrate-core/src/render/fold.rs:5427-5428`)
  and recovery hardcodes `"mode": "randomised"`
  (`crates/zeroship-migrate-core/src/render/lower.rs:9509-9524`), pinned by
  `encrypted_via_op_star_is_default_mode_only_fail_closed_by_construction`
  (`fold.rs:9427-9465`). So a creator who writes `mode: "deterministic"` in the
  SDK (accepted and validated at `sdks/db/src/types.ts:1704-1712`) gets a
  descriptor that says `randomised`. The deterministic arm in plugin-db is
  reachable today only by hand-writing descriptor JSON, which is what the tests
  do.
- The automatic B-tree index for deterministic columns
  (`query.rs:1900-1937`) sits in `build_create_indexes`, which has no production
  caller.
- One production path **does** encrypt a filter operand: the upsert
  ON CONFLICT probe (`crud/write_pipeline.rs:452-516`), gated to deterministic
  fields by `deterministic_conflict_probe_schema` (`:579-617`), which refuses a
  randomised conflict field with
  `upsert_conflict_field_requires_deterministic_encryption` (`:601-608`). It is
  the only precedent in the tree for "transform the operand in Rust, then
  compare", and this design is its second client.
- Stale claim worth flagging: `docs/feature-map.md:239` still says
  "Deterministic mode allows equality." Contradicted by the two paragraphs
  above.

### 0.6 The SDK fence that already exists

`sdks/db/src/collection/encryption-fence.ts` refuses **any** filter on a
randomised-encrypted field (`:62-72`, `RANDOMISED_ENCRYPTED_FIELD_NOT_FILTERABLE`)
and restricts a deterministic field to `$eq`/`$in` (`:15-18`, `:73-88`). It is
SDK-side only; there is no Rust counterpart. After the flip that fence is
describing a filter path that no longer reaches the value at all, so it becomes
either dead or wrong depending on how the flip lands. Noted here because a
reader will find it and think equality search exists.

---

## 1. The API

### 1.1 Shape, in one line

Equality lookup is **declared per field at schema-authoring time** and **invoked
through a dedicated collection verb** that returns ordinary masked rows.
Searchability is not a query-time option; it is a physical property of the
column, because the mechanism that makes it work is a column.

### 1.2 The schema declaration

```ts
// the creator's schema source
accounts: {
  ssn: t.string()
        .mask({ kind: "last4", classification: "pii" })
        .encrypted({ mode: "randomised" })
        .equalityIndex(),          // NEW - opt in to lookup by real value
  email: t.string()
           .mask({ kind: "email", classification: "pii" }),   // no lookup
}
```

`.equalityIndex()` is refused (at declare time, synchronously, like the other
builder validations at `sdks/db/src/types.ts:1239-1305`) on a field with no
non-`none` mask declaration: the verb's whole contract is that the value is
protected, and an unprotected field is queried with `find`.

The name is chosen against `.index()` and `.unique()`, which it sits beside in
the same chain. It says exactly what it enables - equality, indexed - and cannot
be misread as enabling ranges. It also says the honest thing: **this creates an
index, with the disclosure an index implies** (section 4.3).

**What it emits.**

| Artifact | Value |
| --- | --- |
| Column | `ssn_lookup BYTEA` (PG) / `BLOB` (SQLite), NULL when the field is NULL |
| Index | `CREATE INDEX` on it; `CREATE UNIQUE INDEX` when the field is declared `.unique()` |
| Descriptor | `storage.lookupColumn`, `storage.lookupKeyId`, and `lookupFilterable`/`lookupSortable`/`lookupProjectable`, all `false` |
| Reserved name | `ReservedName::Suffix("_lookup")` joins `query.rs:738-766` |

The reserved suffix is only anti-collision. Nothing derives the column name from
it - the descriptor states it, per the rule the specification already sets
(`gen_types.rs:190-192`).

### 1.3 The descriptor block

```jsonc
"storage": {
  "valueColumn": "ssn",          // the mask, after the flip
  "rawColumn":   "ssn_raw",      // plaintext or ciphertext
  "rawFilterable": false, "rawSortable": false, "rawProjectable": false,
  "aadColumn":   "ssn_raw",

  // NEW
  "lookupColumn":      "ssn_lookup",
  "lookupKeyId":       "default",
  "lookupFilterable":  false,     // as a creator-nameable field: never
  "lookupSortable":    false,
  "lookupProjectable": false
}
```

The presence of `lookupColumn` **is** the capability flag. There is no separate
boolean, because a boolean that can disagree with the column's existence is a
second source of truth for one fact.

### 1.4 The lookup token

```
k_lookup = HKDF-SHA256(salt = app_id, ikm = root_key(lookupKeyId),
                       info = "zsenc/lookup/v1/k_lookup")
token    = HMAC-SHA256(k_lookup,
                       canonical_aad(collection, aadColumn, None) || plaintext_bytes)
```

32 bytes, full width, no truncation.

Five properties, each load-bearing:

1. **The key is a third HKDF leg, not a reuse of `k_siv`.** `derive_key` already
   expands two legs from one root with distinct info strings
   (`crates/zeroship-plugin-db/src/encryption/keys.rs:373-382`); this adds a
   third. `k_siv` derives AES-GCM nonces; publishing a 32-byte HMAC under the
   same key in an indexed column would publish values from a nonce-derivation
   function's output space. Separate key, separate purpose.
2. **The domain-separation prefix is `canonical_aad(collection, aadColumn,
   None)`**, reused verbatim (`encryption/aad.rs:75-98`). It already
   length-prefixes the wire version, the collection and the column, which is
   exactly the separation a blind index needs, and it means the same SSN in
   `accounts.ssn` and `people.ssn` produces different tokens. A token leaked from
   one table cannot be probed against another.
3. **It binds `aadColumn`, not the physical column**, for the same reason the
   ciphertext does (`descriptor-specification.md:684-700`): a future physical
   rename must not silently invalidate every stored token.
4. **The plaintext byte encoding is the encryption pass's, not a new one.** The
   write path already carries plaintext across passes in
   `MaskPlaintextSidechannel = HashMap<String, Zeroizing<String>>`
   (`crud/mask_pass.rs:71`, populated at `crud/write_pipeline.rs:225`, `:270`,
   `:486`), and `wraps` already fixes the byte form for string / number / bytes
   (`unmask.rs:651-672`). The token uses that encoding. Two encoders would mean a
   value that encrypts one way and hashes another, and the failure would be a
   silent non-match.
5. **The caller cannot compute a token.** This is the property that makes the
   lookup column safe to have an equality predicate over at all. The only
   operand the predicate ever receives is the output of a keyed function whose
   key never leaves the worker process. Contrast a `WHERE ssn_raw = $1` design,
   where the operand *is* the plaintext and any path that reaches the raw column
   with caller-supplied data is immediately an oracle.

### 1.5 The write path

The token is computed in the same pass that computes the mask, from the same
sidechannel plaintext, before the encryption pass consumes it
(`apply_mask_on_write`, `crud/mask_pass.rs:93-97`, which already writes one
sibling and gains a second). A NULL value writes a NULL token.

**Operational consequence, stated because it is a real cost.** A mask-only field
that opts into `.equalityIndex()` now needs column key material where it needed
none: `ZEROSHIP_COLUMN_KEY_DEFAULT` must resolve
(`encryption/keys.rs:317-339`), or every **write** to that field fails with
`column_key_not_configured`. That must not be discovered at the first write.
Per SC-6's own rule that an absent authority is refused at composition rather
than at first use (`sc6:183-194`), **binding construction resolves the lookup
key for every collection whose descriptor declares a `lookupColumn`, and refuses
to compose if it cannot.**

### 1.6 The query verb

```ts
// Collection<S>
findByUnmasked(
  match: { [K in LookupField<S>]?: PlainValue<S, K> },
  options: {
    actor: Actor;                       // REQUIRED
    reason?: string;
    where?: Filter<S>;                  // ordinary filter, ANDed
    select?: (string & keyof Row<S>)[];
    limit?: number;                     // <= MAX_QUERY_LIMIT
    includeDeleted?: boolean;
  },
): Promise<Result<Row<S>[]>>;
```

`TxCollection<S>` carries the mirror returning `Promise<Row<S>[]>` and throwing,
per the `Result`-rail convention (`docs/reference/api-design-guidelines.md:100-112`,
`sdks/db/src/db-types.ts:107-160`).

**The name.** `unmask` reads the value for a row you already have;
`findByUnmasked` finds the row for a value you already have. The symmetry is the
point: it makes the permission relationship legible at the call site, and it
keeps the vocabulary the tree already uses instead of introducing a new noun.
Rejected candidates: `lookup` (short and English, but carries no signal that the
call is privileged and audited), `findByProtected` and `findByRealValue`
(introduce a noun the codebase does not use), `lookupProtected` (ambiguous about
which side is protected).

**`actor` is required, not optional.** `find`'s `actor` is optional
(`sdks/db/src/collection.ts:77-81`); here an absent actor is a guaranteed
denial (`unmask.rs:295-297`), so leaving it optional only converts a compile
error into a runtime one.

**`reason` stays optional.** A required free-text field on a per-call verb gets
filled with a constant. The audit row's load is carried by the actor, the
digest and the match count (section 6).

**Return is a settled array, not a `Query` builder.** `find` returns a thenable
builder (`sdks/db/src/collection.ts:360-371`, `sdks/db/src/query.ts`) carrying
`.sort()`, `.after()`, `.limit()`, `.paginate()`. A builder is a composition
surface, and every composition point is somewhere the oracle can be
reintroduced; worse, a builder defers execution, so one authorization could
produce several executions and the "one call, one audit row" correspondence
breaks. `findByUnmasked` resolves once.

**Type-level closure.** `LookupField<S>` is the set of fields whose descriptor
carries a `storage.lookupColumn`. The generator already re-emits mask metadata
into `env.db.ts` (`sdks/vite-plugin/src/gen-types/render-env-db.ts:289-298`), so
this is a derivation from the same artifact. For a collection with no
lookup-enabled field, `match` has no admissible key and the call does not
compile. This is the flip's own argument applied one level up: unrepresentable
beats guarded.

### 1.7 What it compiles to

```sql
SELECT <the ordinary read projection for this collection>
  FROM "<app>"."<collection>"
 WHERE "<lookupColumn>" = $1
   AND (<ordinary where, if supplied>)
   AND "deleted_at" IS NULL          -- unless includeDeleted
 LIMIT $n
```

`$1` binds the 32 token bytes. Three constraints on how this is built:

1. **It is not built by `build_where_with_dialect`.** A dedicated builder emits
   the token predicate; the caller's `where` goes through the ordinary builder
   and is ANDed at the top level. The two operand spaces never mix, so no code
   path carries a caller-supplied value to the lookup column.
2. **In SC-3's `DbPlan` terms this is a new node whose operand type is a token,
   not a value.** SC-3's technique is "the type that makes SQL text
   unrepresentable" (`sc3:169`); the same technique applied to the operand means
   `LookupToken(Vec<u8>)` has exactly one constructor (the keyed hash) and the
   node has exactly one comparison (equality). There is no `Gt` to reach.
3. **Multiple protected fields in one `match` are ANDed**, each with its own
   token predicate, each separately authorized and separately audited. This adds
   no capability: the caller can already intersect the row-ID sets of two
   separate single-field lookups, both of which they are authorized to make.

The projection is the **ordinary** one, so `ssn` comes back as a `MaskedValue`.
**The verb never returns a value the caller did not supply.** It answers "which
row", not "what value". It therefore composes with `unmask` rather than
replacing it, and a caller holding lookup-but-not-unmask permission gets exactly
the "find the account" capability and nothing beyond it.

### 1.8 Authorization

`MaskPolicy` gains a second axis:

```jsonc
{
  "support": { "unmask": [],              "lookup": ["pii"] },
  "admin":   { "unmask": ["pii", "pci"],  "lookup": ["pii", "pci"] }
}
```

with one derivation rule:

> **`unmask` on classification C implies `lookup` on C.**

This is a lattice fact, not a convenience. A role that can `unmask` C can also
`find` every row and `unmask` each one, so it can construct the value-to-row
mapping itself; granting `lookup` adds nothing it does not already have. The
converse does not hold, which is the whole reason the axis is separate: a
support agent who is read an SSN over the phone should be able to find the
account without being able to dump SSNs.

Everything SC-6 establishes about the meet carries to the new axis **and must be
implemented on it, not only on the old one**:

- the meet is pointwise over a **role-complete domain**, both policies
  normalised to total functions first (`sc6:580-618`);
- an absent key is TOP for `auto` and BOTTOM for everyone else
  (`mask_policy.rs:118-124`), so a naive map intersection **inverts** a ceiling
  that revokes `auto`'s lookup - the same defect, on a new axis, and it would
  pass every arm SC-6 wrote;
- the ceiling is resolved independently of the app-supplied actor, never through
  a map entry the actor names (`sc6:664-672`);
- `sanitize_app_actor` runs on this path (`unmask.rs:277-288`), applied at the
  dispatcher exactly as `dispatch_find` does (`mod.rs:617-619`).

### 1.9 Failure modes

All codes are `lower_snake_case` at the raise site and surface to JS as
`UPPER_SNAKE` through `canonicalErrorCode` (`sdks/db/src/errors.ts:27-36`),
matching guideline 9. The `unmask_lookup_` prefix groups them with the existing
`unmask_*` family, because they are the same family of act.

| Code | Raised when | Audited |
| --- | --- | --- |
| `unmask_lookup_field_not_masked` | the named field has no non-`none` mask declaration | no - refused before any probe |
| `unmask_lookup_not_enabled` | the field is masked but its descriptor has no `storage.lookupColumn`; hint names `.equalityIndex()` | no |
| `unmask_lookup_not_permitted` | the effective policy denies the actor `lookup` on the field's classification | **yes, denied** |
| `unmask_lookup_operand_invalid` | the operand is an object (any `$op`), an array, or of the wrong declared type | no |
| `unmask_lookup_operand_null` | the operand is `null` | no |
| `unmask_lookup_empty_match` | `match` names no field | no |
| `unmask_lookup_in_live_query` | called inside a `query()` handler (section 1.11) | no |
| `column_key_not_configured` | the lookup key does not resolve (`keys.rs:332-337`) | no - and this should be impossible at runtime given 1.5 |

Two of these deserve their reasoning stated.

**Why `unmask_lookup_field_not_masked` refuses instead of falling back to an
ordinary equality filter.** A silent fallback makes the audit guarantee
conditional on schema state: a creator who removes `.mask()` from a field would
silently convert every audited lookup in their codebase into an unaudited
filter, with no error and no diff at the call site.

**Why an operator object is refused rather than ignored.** Ignoring `$gt` and
treating the whole object as a value would produce a token over a serialised
object, which matches nothing - a silent empty result where the caller wrote a
range. Refusing names the mistake.

### 1.10 Batch lookup is refused, deliberately

`match` takes one scalar per field. An array form (`{ ssn: [a, b, c] }`) was
considered and rejected. It adds no capability - N probes are N probes either
way - but it multiplies the per-request probe rate by the list cap, and the
per-request rate is the only bound left on the confirmation channel
(section 4.2). The audit-legibility argument runs the same direction: one act
per audit row is the cleaner correspondence, and the existing comma-joined bulk
row (`unmask.rs:1158-1218`) is the shape to move away from, not toward.

A batch form is a legitimate future extension for a real use case (deduplicating
an import). Its conditions: a **distinct** capability on the policy's lookup
axis, a per-call cap, and one audit row naming every digest. It is not this
verb with a wider operand type.

### 1.11 Transactions and live queries

**Inside an explicit transaction: supported.** The SELECT routes through
`TxRoute::capture` / `exec_query` exactly as `dispatch_find` does
(`mod.rs:632`, `:690`), so it runs on the transaction's connection and sees the
transaction's own uncommitted writes. This matters for the most common
consequence of the flip: `deleteMany({ ssn: v })` no longer matches (section
5.12), so the replacement is lookup-then-delete-by-id, and that pair is only
atomic inside a transaction.

**Inside a `query()` live handler: refused**, with `unmask_lookup_in_live_query`.
Two reasons, and the second is the decisive one:

1. Read-set capture normalises a filter into a predicate the broker evaluates
   per event (`crates/zeroship-plugin-db/src/read_set.rs:1-49`). A token
   predicate has no useful normalisation, so the module's documented safe default
   applies - `predicate == None` means "match every row" (`read_set.rs:28-31`) -
   and the subscription degrades to coarse delivery.
2. Coarse delivery means the handler re-runs on **any** write to the collection,
   by any writer. Each re-run is a lookup, and each lookup writes an audit row.
   The audit-write rate would then be driven by third parties rather than by the
   caller. A privileged verb whose audit volume an unrelated actor controls is a
   denial-of-audit surface, and refusing is cheaper than rate-limiting it.

---

## 2. Why this does not reopen the oracle

The claim to defend: a determined caller with `findByUnmasked` cannot learn a
protected value they do not already hold, except by guessing whole values, and
each guess is authorized, audited and counted.

The adversary is **app JS**. It can call the verb with any non-`auto` actor it
cares to assert (section 0.1), so its effective privilege is the operator
ceiling's, not the creator draft's. It can call in a loop. It can read every
row's `id`, every unmasked column, and every mask.

### 2.1 Ordered comparison

Today the channel is `find({ ssn: { $gt: "500-00-0000" } })`, which lowers to
`WHERE "ssn" > $1` against plaintext, with no authorization anywhere on the path
and no audit row. Treating the column as fixed-length ordered text over a
9-digit space, recovering an exact value costs `ceil(log2(10^9)) = 30`
comparisons. *(That figure is arithmetic over a stated space, not a measurement
of this code.)*

After the flip plus this verb:

- The ordinary filter reaches `ssn`, which holds the mask. Ordering the mask
  orders `***-**-1234` strings.
- The ordinary filter cannot name `ssn_raw` or `ssn_lookup`: neither is a
  declared field, so `implicit_read_projection_parts` never emits them
  (`query.rs:3345-3366`) and `validate_field_name` refuses them at filter time
  through the reserved-suffix path that already refuses `ssn_masked`
  (`query.rs:5329`, `:738-766`).
- The verb's operand space has no comparison operator to reach. Not "we check
  for `$`" - there is no operator vocabulary on that path at all, and the plan
  node carries a token, not a value (section 1.7).

**And ordering is not recoverable even if a predicate over the lookup column
were somehow reached.** HMAC-SHA256 is not order-preserving: the token's byte
order carries no information about the plaintext's. A future bug that exposed
the lookup column to `orderBy` would leak the equality partition, not the order.
That is a structural difference from a `WHERE ssn_raw = $1` design, where the
same bug on the raw column reopens the binary search intact.

Be precise about the strength of that claim: the raw column still exists and
still holds sortable plaintext. What changes is the **blast radius of a future
mistake**. Under this design the only generated SQL that names the raw column is
`unmask`'s by-primary-key read (`unmask.rs:566-569`, `:465-468`), which is a
point lookup and cannot express a comparison. Under a raw-column-equality
design, the set of SQL naming the raw column grows to include a predicate built
by a builder that also knows how to build `>`.

### 2.2 Prefix, substring and pattern

`$like`/`$ilike` exist in the ordinary builder (`query.rs:5456-5468`). They
reach the mask. Against the token they are useless by construction: the token is
a fixed-width digest of the whole value, so no prefix of a plaintext yields a
prefix of a token. Even granting the adversary a `LIKE` over the lookup column,
the answer is uncorrelated with anything about the plaintext.

### 2.3 Forgery of the operand

The adversary cannot compute a token without `k_lookup`, which is derived
per-app inside the worker process from a root that arrives as operator
configuration (`keys.rs:317-339`, `:373-382`) and is never returned across the
V8 boundary. So even a hypothetical filter path that reached the lookup column
with caller data would compare against a value the caller cannot aim.

### 2.4 What remains: guess-and-confirm

This is the residual and it is not closed. An authorized caller supplies a
complete candidate value and learns whether any row holds it, and which. That is
one bit plus a row set per authorized, audited call.

The delta is the argument. Before: an unauthorized, unaudited caller recovers an
exact 9-digit value in about 30 ordered comparisons. After: recovering the same
value takes up to `10^9` authorized, audited probes, each of which writes a row
naming the actor and a digest of the guess.

**Where that argument fails, stated rather than buried: low-entropy protected
values.** A date of birth over a plausible century is on the order of `36,500`
candidates; a US state is 50; a boolean-shaped classification is 2. For those,
guess-and-confirm is cheap and this design does not prevent it. The three things
standing in its way are declaration-time opt-in (section 1.2), the audit trail
with a per-value digest (section 6), and the platform's ordinary request rate
limit. The reference documentation must say plainly that `.equalityIndex()` does
not belong on a value with a small domain. This is the one place where the
"solve it outside the database" case genuinely wins (section 9.3).

### 2.5 The conjunctive channel

`match` may name several protected fields, and `options.where` may add ordinary
predicates. Neither adds information about a protected value: the caller supplies
every protected operand in full, and the ordinary columns are already readable.
A conjunction over two protected fields is derivable by intersecting two
single-field lookups the caller is separately authorized to make.

### 2.6 Timing

Equality against an indexed token is an index probe; a hit and a miss differ in
timing. That is not a new channel: the result already discloses hit-or-miss.
Constant-time comparison is not required here because the comparison is against
a keyed digest of a value the caller supplied - there is nothing to learn by
timing a comparison against something you already hold. (The rule differs when a
secret is compared against caller input; that is not this shape.)

### 2.7 The boundary this design does not create

The verb is called by the worker, which executes creator code, so per AGENTS.md
("Privilege follows the PROCESS, not the function") it is **not privileged** and
must not pretend to be. Its safety rests entirely on the operator ceiling meeting
the creator draft. Within an app whose ceiling grants `admin` the lookup, the
verb is effectively unauthenticated from app JS, because the actor is
self-asserted. That is inherited from `unmask`, not introduced here, and fixing
it (binding the actor to `env.auth`) is SC-6's to own. Section 9.2 argues why it
is not a reason to move the feature elsewhere.

---

## 3. Encryption-mode coverage

| Storage shape | Where the real value lives | Equality lookup | Notes |
| --- | --- | --- | --- |
| Mask-only (`.mask()`, no `.encrypted()`) | plaintext in `ssn_raw` | **Yes**, via the token | Needs column key material it did not need before (section 1.5) |
| Randomised-encrypted + masked | ciphertext in `ssn_raw`, distinct per row | **Yes**, via the token | The token is the only mechanism that can serve this; ciphertext equality is impossible by construction (`aad.rs:75-98`, `aead.rs:62-75`) |
| Deterministic-encrypted + masked | byte-identical ciphertext in `ssn_raw` | **Yes**, via the token | Ciphertext equality would also work here, and is rejected in section 8.3 |
| `.mask({ kind: "none" })` + encrypted | ciphertext in the field's own column, no sibling (`gen_types.rs:182-185`) | **No** - the verb refuses with `unmask_lookup_field_not_masked` | An unmasked field is queried with `find`; there is no masking guarantee to protect |
| Unmasked, unencrypted | the column | n/a - use `find` | |

### 3.1 Does this force the deterministic question back open?

**No. It closes it, and supplies the evidence the deferral was waiting for.**

The design decision at `docs/proposals/2026-08-26-runtime-db-binding-design.md:1112-1129`
records the operator keeping deterministic mode with the note that "equality
search against an encrypted value" - "the capability that justifies deterministic
encryption at all" - has no implementation, and that "if it is still unbuilt when
the filter path is audited under L9, that audit should decide it rather than
route around it."

This design builds that capability **without** deterministic mode. Once
equality lookup runs on a token column, deterministic mode retains no capability
the platform needs, and it keeps three costs the token does not have:

1. **DB-13's accepted relocation risk** (`aad.rs:24-32`): because deterministic
   mode omits `row_pk` from the AAD, an attacker with UPDATE access who copies
   row A's ciphertext into row B's same column reads A's secret through B. With
   randomised mode plus a token, the same attacker copying A's *token* into B
   makes a lookup for A's value return row B - a false positive in a result set,
   not a plaintext disclosure, because reading B's value still fails the
   row-PK-bound AAD. The token is strictly weaker as an attack.
2. **The blob already leaks a blind index.** The deterministic nonce is
   `HMAC-SHA256(k_siv, aad || plaintext)[..12]` and it is stored in the clear in
   the wire envelope (`encryption/wire.rs:1-5`, `aead.rs:104-110`). So the "a
   blind index leaks equality classes" objection applies to deterministic mode
   already, at 96 bits, inside a value that is also decryptable with the key. The
   token is one-way; a leaked `k_lookup` yields equality classes and a dictionary
   attack over a small domain, while a leaked `k_enc` yields every plaintext.
3. **It cannot be selected end to end anyway** (section 0.5): the fold discards
   the mode and recovery hardcodes `randomised`
   (`fold.rs:5427-5428`, `lower.rs:9509-9524`). Making deterministic mode usable
   is not a small change; it is an IR change, a fold change, a recovery change
   and a pinned test.

**This is a finding for the operator, not a decision taken here.** Item 1 is
scoped to the lookup API. What item 1 can say is that after this design lands,
deleting deterministic mode costs the platform no capability, and keeping it
costs DB-13.

### 3.2 Cross-item finding for SC-6 item 2 (constraints and indexes)

Item 2 says constraints and indexes follow the **raw** column, because a unique
index on `ssn` would enforce uniqueness over masks and many rows legitimately
share `***-**-1234`. That is correct for a mask-only field.

**It is wrong for a randomised-encrypted field, and silently so.** Each row's
ciphertext differs by construction, so a unique index on `ssn_raw` is satisfied
by every possible pair of rows: it enforces nothing, and it fails open exactly
the way the mask-sibling index fails closed. The SDK already knows this - it
refuses `.unique()` on a randomised-encrypted field and tells the creator to
switch to deterministic (`sdks/db/src/types.ts:1147-1159`,
`UNIQUE_ENCRYPTED_RANDOMISED_UNSUPPORTED`).

The token column is the only place uniqueness over the real value can be
enforced for a randomised-encrypted field, and a `CREATE UNIQUE INDEX` on it
does so exactly. That turns a currently-refused declaration into a supported
one. Item 2's owner should route `.unique()` on an encrypted masked field to
`storage.lookupColumn`, not to `storage.rawColumn`.

Foreign keys are a separate matter and are left to item 2: an FK needs a
referenced unique key, and pointing one at a token column makes the token a
join key, which this design has not analysed.

---

## 4. What this costs: queries that were expressible and no longer are

This is the price of the decision and it should be legible. Everything below was
expressible before the flip. Items marked **silent** change meaning without
raising an error, which makes them the dangerous half.

1. **Ordered comparison on the real value.** `find({ ssn: { $gt: v } })` and the
   other three (`query.rs:5354-5369`). After the flip these compare masks.
   **Silent.** Not restored by anything here, on purpose.
2. **`orderBy` on the real value.** `build_order_by_read_with_dialect` emits the
   bare column (`descriptor-specification.md:790-795`), which after the flip is
   the mask. Sorting a `last4` mask orders by the last four digits. **Silent**,
   and it is simultaneously a fix (it closes the ordering leak the same section
   records) and a capability loss.
3. **Cursor pagination keyed on the field.** A cursor minted over the field now
   encodes a position in mask order. Combined with (2), a cursor minted before
   the flip does not resume correctly after it - which is moot pre-launch and
   worth stating anyway.
4. **`$like` / `$ilike` on the real value** (`query.rs:5456-5468`). Prefix and
   substring search over a protected value is gone and is not restored. "Find
   accounts whose email starts with `alice`" over a masked email is no longer
   expressible at all.
5. **`$in` / `$nin` over real values** (`query.rs:5370-5445`, cap 100). Replaced
   by N separate lookups, deliberately (section 1.10).
6. **`$exists` semantics shift.** `$exists` on the logical name now tests the
   mask column's nullity. For a masked field the mask is written whenever the
   value is (`mask_pass.rs:93-97`), so the answer is the same today - but it is
   now an answer about a different column, and any future mask kind that emits
   NULL for a present value would diverge. Flagged as fragile, not broken.
7. **`distinct` on the field** (`Collection.distinct`,
   `sdks/db/src/collection.ts:434`). Returns distinct **masks**. Under `last4`,
   two different SSNs sharing a suffix collapse into one value, so the cardinality
   is wrong. **Silent.**
8. **`$group.by` on the field** (`aggregate_read_ident`, `query.rs:3431-3433`,
   via `read_column_for`). Groups by the mask, so counts per group are wrong the
   same way. **Silent**, and worse than (7) because an aggregate result looks
   authoritative.
9. **`count` filtered on the real value.** Now expressible only as the length of
   a `findByUnmasked` result, which means it requires the lookup permission and
   is capped at `MAX_QUERY_LIMIT = 500` (`query.rs:595`). "How many accounts have
   this SSN" beyond 500 is not answerable.
10. **Existence checks in signup and import flows.** "Does this email already
    exist?" was a free, unauthenticated `exists({ email })`. It now requires
    `lookup` permission on the field's classification. This is the single most
    common thing this decision breaks and it deserves its own line: any app that
    masks a field it also uses for deduplication must grant its signup path a
    lookup capability, or stop masking that field.
11. **Uniqueness enforced by the application rather than the database.** The
    read-check half of the read-then-insert idiom now needs the permission from
    (10). The database-level answer is `.unique()` plus the token index (section
    3.2), which is better, but it is a different code shape.
12. **Writes filtered on the real value.** `updateMany({ ssn: v }, patch)`,
    `deleteMany({ ssn: v })`, `purgeMany`, `restoreMany`. All match nothing after
    the flip. **Silent, and destructive in the direction that looks safe** - a
    delete that matches nothing reports success with `deletedCount: 0`. The
    replacement is lookup-then-write-by-id, atomic only inside an explicit
    transaction (section 1.11).
13. **`upsert` with a protected conflict field.** `upsert(row, { conflictFields:
    ["ssn"] })` needs an `ON CONFLICT` target that is a real unique key. After the
    flip the creator names a logical field whose physical unique index is on the
    raw column, or - per section 3.2 - on the token column. The conflict target
    must be resolved from the descriptor, not from the creator's field list. The
    existing deterministic conflict probe (`write_pipeline.rs:452-516`) is the
    code that has to change. Left to item 2.
14. **Relations keyed on a protected field.** SC-6 already records that a masked
    join key breaks the stitch and nests `null`, which reads as "target row
    missing" (`sc6:456-463`, `docs/reference/db.md:585`). After the flip a
    creator-expressed relation whose key is a masked field cannot be expressed at
    the query surface at all, even though the FK constraint can live on the raw
    column. SC-3's relation grammar and item 2 share this one.
15. **Full-text search over the field.** `Collection.search`
    (`sdks/db/src/collection.ts:457`) over a masked column indexes the mask.
    **Silent.**
16. **Vector search over a masked field.** `Collection.near` (`:469`) has the
    same shape; a vector column is not a plausible mask target, so this is listed
    for completeness rather than as a live loss.
17. **CDC subscribers reading the field.** The change stream ships the raw
    tuple (`descriptor-specification.md:797-803`, `broker.rs:1872-1885`). Before:
    `{ ssn: <ciphertext>, ssn_masked: "***-**-6789" }`. After:
    `{ ssn: "***-**-6789", ssn_raw: <ciphertext>, ssn_lookup: <token> }`. A
    subscriber reading `event.row.ssn` goes from ciphertext to mask - safer and
    different - while both `ssn_raw` and, now, `ssn_lookup` ride the wire. The
    capability flags are projection-side only; **this design adds a third column
    to the set CDC exposes and does not close it.** Owed to whoever owns the CDC
    consumer.
18. **Anything in app JS that read the plaintext out of a default `find`.**
    Before the flip a mask-only column returned plaintext in the parent column to
    any code path that bypassed the projection substitution. That is the leak the
    flip closes; listing it here so the inventory is complete rather than
    flattering.

**Restored by this design:** exactly one shape - equality on the real value,
authorized and audited. Everything else on this list stays gone.

---

## 5. Naming and surface placement, briefly

- Native op: `findByUnmasked` on the `Collection` v8 class, beside
  `unmaskField` (`crates/zeroship-plugin-db/src/v8_classes/collection.rs:493-521`)
  and `bulkUnmask` (`:540-562`). It is app-JS-reachable, like both of those; it
  is not a `DbPlatform` capability, because the worker calling it is the point.
- SDK: `Collection.findByUnmasked` and `TxCollection.findByUnmasked`. There is
  precedent for a native op shipping ahead of its SDK wrapper -
  `Collection.unmaskField` has no `@zeroship/db` wrapper today - but this verb
  needs the generated `LookupField<S>` type to be useful, so both land together.
- Documentation: `docs/reference/db.md` gains it under `### Unmasking`
  (`:1423-1469`), and the behaviour change from item 3 lands beside the mask
  kinds as SC-6 requires. Note that `db.md` currently has **no** section on
  filtering masked or encrypted columns at all, so this is a gap being filled,
  not a page being edited.

---

## 6. The audit shape

### 6.1 What a lookup discloses, and therefore what must be recorded

`unmask` discloses **a value**, identified by a `(collection, row_pk, column)`
the caller already held. An operator reading the audit row can re-read that cell
and know exactly what was disclosed, so the row needs to name nothing about the
value itself.

`findByUnmasked` discloses **a membership fact**: which rows hold a value the
caller already held. The row set is the output, not the input. So the audit row
must record the output - and it must record something about the probe, or the row
says only "someone looked something up" and enumeration is invisible.

### 6.2 The row

One audit row per (call, protected field). Columns beyond today's:

| Column | Value | Why |
| --- | --- | --- |
| `op` | `"lookup"` | Replaces the `[bulk_unmask]` / `[query_hint]` prefixes smuggled into `reason` (`unmask.rs:1203-1207`, `:1429-1433`). A dispatch shape is a column, not a string prefix |
| `probe_digest` | hex of the 32-byte token | Correlates repeated probes and counts enumeration **without storing the plaintext** |
| `match_count` | rows returned, `0` included | The size of the disclosure |
| `row_pk` | comma-joined matched PKs, empty when none | Reuses the existing slot and the existing `(row_pk, "column", ts)` index (`unmask.rs:877-880`); the bulk writer already joins PKs into it (`:1175-1180`) |

`actor_id`, `actor_role`, `collection`, `column`, `classification`, `reason` and
`outcome` keep today's meanings. `outcome` keeps its two values, so the CHECK
constraint (`unmask.rs:861`) does not change.

**The probe plaintext is never written anywhere.** An audit table that
accumulates the plaintext of every SSN anyone searched for is a second copy of
the asset, in a table the app can `SELECT` from
(`docs/reference/db.md:1517-1518`), with no masking and no policy. The digest
gives operators everything they need - equality between probes, correlation with
the matched row, a count - and gives an attacker who reads the audit table
nothing they could not compute if they already had the value.

### 6.3 Ordering

- **Compute the token first, then authorize.** Both outcomes then carry a
  digest, which means an operator can see "actor X probed 400 distinct values and
  was denied every time" - the enumeration pattern you most want to see is on the
  denied path. The cost objection (an unauthenticated caller forcing key
  derivation) does not hold: `KeyStore::resolve` is cached per `(app_id, key_id)`
  (`keys.rs:287-295`), so the marginal cost of a denied probe is one HMAC.
- **Denied: audit, then refuse**, as `dispatch_unmask` does
  (`unmask.rs:401-405`).
- **Granted: SELECT, then audit, then return.** The row carries `match_count`
  and the matched PKs, which are only known after the SELECT. And the audit write
  is not optional: if it fails, the call fails and the caller gets no answer -
  the same `?` discipline as `unmask.rs:428`. "Audited or unanswered" is the
  guarantee; anything weaker makes the audit advisory.

### 6.4 Two problems this inherits, both of which must be solved for the
guarantee in 6.3 to hold

**(a) The audit table is provisioned by the runtime with
`CREATE TABLE IF NOT EXISTS` on every dispatch (`unmask.rs:838-946`).** Adding
`op`, `probe_digest` and `match_count` to that DDL does **nothing** on any
database that already has the table, and the subsequent INSERT then fails with a
missing-column error - on the denied path too, so a policy denial would surface
as an internal error. This is not a pre-launch-safe change through that
function.

It is also, on its own terms, the wrong home: SC-3 states that "the runtime
executes no DDL" (`sc3:56`) while this function executes `CREATE TABLE` and three
`CREATE INDEX` statements per unmask. **The audit table should be owned by the
migration engine like every other table**, and this feature is a reason to move
it rather than to extend the runtime DDL. Flagged, not designed here.

**(b) The audit write takes a second pool connection while a transaction holds
the first.** `write_audit_unmask_row` calls `pool.query_text_params`
(`unmask.rs:774`) on the 8-connection data pool
(`lib.rs:998`). SC-6 finding 3 says exactly this shape is "a deadlock, not a
latency cost", and notes that the dedicated authority pool that answered it "has
no client now and is not built; the reasoning is kept for the next feature that
wants a second checkout mid-transaction" (`sc6:150-155`).

**This verb is that next feature**, and it inherits the exposure rather than
introducing it - `unmask` inside a transaction already takes a second connection
for the audit write and a third for the plaintext fetch
(`query_postgres_pool_with_autocommit_role`, `unmask.rs:570-572`, `exec.rs:293-318`).
That fetch also runs in its own transaction, so **an `unmask` inside an explicit
transaction cannot see the transaction's own uncommitted writes** - insert a row
and unmask its column in the same transaction and it returns `unmask_not_found`.
That is a pre-existing correctness defect, found while designing this, and it is
not item 1's to fix.

The resolution this design needs: **the audit sink gets its own connection,
outside the data pool.** One per isolate is enough - audit writes are small and
serial - and it removes the cycle rather than pricing it. Writing the audit row
on the transaction's own connection is the tempting alternative and it is wrong:
a rollback would erase the record of a disclosure the caller has already
received in JS.

I did not re-verify the pool's behaviour on exhaustion (block vs. error), so the
word "deadlock" here is SC-6's claim carried forward, not a measurement of mine.

### 6.5 How this differs from `unmaskField`'s row, summarised

| | `unmask` | `findByUnmasked` |
| --- | --- | --- |
| `row_pk` | input, caller-supplied | **output**, the disclosure |
| value | identified by `(row, column)`; nothing recorded about it | not recorded; a keyed **digest** of the caller's probe is |
| zero rows | an **error**, `unmask_not_found` (`unmask.rs:473-480`) | a **granted** outcome with `match_count = 0` |
| granularity | one row per (row, column) | one row per (call, field) |
| what an operator learns | "A read cell C of row R" | "A learned that rows [R...] hold the value with digest D in column C" |

The zero-row line is the one to not get wrong. If a no-match lookup threw, the
throw-versus-return distinction would carry the same one bit through an error
path - and error paths are the ones that end up unaudited. An empty result is a
successful, audited disclosure.

---

## 7. Rejected designs

### 7.1 Teach the ordinary filter about masking: route `$eq` to the raw column, refuse ordered operators

This is SC-6's own first rejected option (`sc6:254-258`) re-applied after the
flip: make `build_where` schema-aware, send `find({ ssn: v })` to `ssn_raw` when
the operator is equality, and refuse the rest with a typed error.

Rejected on four counts:

1. **It restores the asymmetry the flip exists to remove.**
   `build_where_with_dialect` takes no schema (`query.rs:5244-5251`); making it
   schema-aware means threading one through every call site, and any site that is
   missed silently reaches the raw column with caller-supplied data. The flip's
   argument is "the ignorant path is the safe path" (`sc6:320-322`); this design
   restores "the ignorant path is the unsafe path", which is how the defect arose.
2. **It cannot be audited at the right granularity.** A filter is compositional:
   `$or: [{ ssn: a }, { ssn: b }]`, `$not`, nested `$and`, a relation's inner
   `where`. Each leaf is a probe, and the only place that could count them is the
   WHERE builder - a pure function in `zeroship-schema`, a leaf crate with no
   `app_id`, no I/O and no key access.
3. **It cannot carry the operand transformation** for an encrypted field, for
   the same reason. Randomised columns are not servable at all.
4. **The refusal is a runtime error where the flip made it a type error.**
   TypeScript would still admit `{ ssn: { $gt: v } }` unless the generated types
   special-cased it. The verb's `match` type refuses it at compile time.

### 7.2 No new verb: `find(..., { unmask: [...] })` plus a comparison in app JS

Tell creators to fetch rows with the existing per-query unmask hint
(`authorize_query_hint`, `unmask.rs:1240-1305`) and compare in JavaScript.

Rejected because it is worse on every axis:

- **Cost.** `dispatch_unmask_for_query` performs one fetch per row per column
  inside a nested loop (`unmask.rs:1386-1399`), and the page is capped at
  `MAX_QUERY_LIMIT = 500` (`query.rs:595`), so it cannot scan a large table at
  all.
- **Privilege.** It requires `unmask` on the classification - strictly more than
  the caller needs, and precisely the permission separation section 1.8 exists to
  provide.
- **Audit fidelity.** It writes one row saying "read the plaintext of N rows"
  (`audit_query_hint_granted`, `unmask.rs:1311-1343`) when the act was "confirm
  one value". The audit log would describe a bulk disclosure that did not need to
  happen.
- **It is a bulk disclosure.** Every plaintext on the page is now in app JS
  memory. A targeted lookup becomes a page dump.

### 7.3 Deterministic ciphertext equality: encrypt the operand and compare blobs

The alternative the descriptor specification itself names - "route
deterministic-equality filters to `storage.rawColumn` and encrypt the operand"
(`descriptor-specification.md:783-788`) - and the one with the most existing
machinery behind it: the crypto works (section 0.5), the upsert conflict probe
already does exactly this (`write_pipeline.rs:452-516`), and there is even a
dormant index builder for it (`query.rs:1900-1937`).

Rejected on five counts:

1. **It serves the narrowest slice.** Deterministic-encrypted fields only. Not
   randomised (impossible), not mask-only (needs a second mechanism). One feature,
   three code paths.
2. **It cannot be selected end to end today.** The mode is discarded by the fold
   and hardcoded to `randomised` on recovery
   (`fold.rs:5427-5428`, `lower.rs:9509-9524`), pinned by a test
   (`fold.rs:9427-9465`). Delivering it means IR, fold, recovery and that test.
3. **It forces every searchable protected field onto a mode with an accepted
   vulnerability.** DB-13 (`aad.rs:24-32`): copying row A's ciphertext into row
   B's column reads A's secret through B, and the doc states it "CANNOT be fixed
   without destroying the lookup property". The token has no such property
   (section 3.1).
4. **It is a worse token.** The deterministic blob already carries a 96-bit
   keyed digest of the plaintext in unauthenticated framing (the nonce,
   `wire.rs:1-5`, `aead.rs:104-110`), inside a value that is decryptable with
   `k_enc`. This design's token is 256 bits, one-way, and under a key that
   decrypts nothing.
5. **Operand encryption in the query builder is architecturally wrong.**
   `zeroship-schema` is a leaf crate with no v8/runtime (AGENTS.md crate index)
   and no `KeyStore`. The upsert probe avoids this by transforming the filter in
   `write_pipeline` *before* handing it to a dedicated builder
   (`build_conflict_probe_with_dialect`) - which is precisely the shape this
   design adopts, applied to a token instead of a ciphertext.

### 7.4 A `lookupIds` verb returning only primary keys

Return `string[]` instead of rows, on the theory that returning less is safer.

Rejected because it returns exactly the same information. The row identity **is**
the disclosure; once the caller has an `id`, `get(id)` returns the row anyway
under the ordinary read policy. Meanwhile it costs a second round trip for the
common case and either duplicates `find`'s option surface (`select`, soft-delete,
`where`) or silently disagrees with it. Less returned, no less disclosed.

### 7.5 Keep ranges, meter them: a per-caller probe budget

Allow ordered comparison against the raw column but charge each comparison
against a budget, and audit.

Rejected because a budget prices an oracle, it does not close one. Thirty
comparisons recovers a 9-digit value (section 2.1), which fits inside any budget
loose enough to be usable. The budget's own state is caller-observable (the
caller learns when it trips), which makes it a side channel of its own. And it is
SC-6's third rejected option ("Allow, but audit", `sc6:261-263`) wearing a
counter.

---

## 8. Acceptance shape

Every arm whose expected outcome is a denial carries a granted-path control, per
SC-6's rule (`sc6:676-681`). The control differs from the deny case in **one**
variable.

1. **The verb works and the filter does not.** `findByUnmasked({ ssn: v })`
   returns the row; the same test asserts `find({ ssn: v })` returns nothing.
   Both halves in one test - the second alone passes on an implementation where
   the verb is broken too.
2. **Lookup permission is separable from unmask.** A role granted `lookup` on
   `pii` and not `unmask` on `pii`: the lookup succeeds AND
   `MaskedValue.unmask()` on the returned row is refused with
   `unmask_not_permitted`. Both assertions, one test.
3. **`unmask` implies `lookup`.** A policy naming only the `unmask` axis for a
   role grants that role the lookup; paired with a role granted neither, which is
   denied.
4. **The ceiling narrows the new axis, `auto` included.** A ceiling revoking
   `auto`'s lookup, against a creator draft that does not mention `auto`, denies
   `auto`'s lookup - paired with a role the ceiling does not name, which keeps
   its lattice default. This is SC-6's sharpest arm (`sc6:702-707`) transplanted;
   a naive map intersection over shared keys **inverts** it and passes everything
   else here.
5. **DB-3 does not reappear.** App JS passing `actor: { kind: "auto" }` to
   `findByUnmasked` is stripped and denied; paired, in the same test, with a
   legitimately granted non-`auto` role that succeeds. Asserted on this verb, not
   inherited from `dispatch_find`'s arm.
6. **A miss is granted, not an error.** A lookup matching nothing returns an
   empty array AND writes one audit row with `outcome = 'granted'` and
   `match_count = 0`; paired with a hit, which writes `match_count = 1` and the
   matching PK.
7. **The audit row holds no plaintext.** Probe with a distinctive value; assert
   no column of the resulting row contains it as a substring. Paired with: the
   digest is non-empty, equal across two probes of the same value, and different
   for a different value. Without the pairing, an implementation that writes no
   digest at all passes.
8. **Randomised mode is served.** Two rows with the SAME plaintext in a
   randomised-encrypted masked field have DIFFERENT bytes in `ssn_raw` and the
   SAME bytes in `ssn_lookup`, and one lookup returns both. A fixture with
   distinct plaintexts passes on a broken implementation, so the fixture must
   share a value.
9. **The token is column-bound.** The same plaintext in two collections yields
   different tokens; a lookup in collection A does not return collection B's row.
   Both rows must exist or the arm is vacuous.
10. **The lookup column is not part of the read surface.**
    `select: ["ssn_lookup"]`, `find({ ssn_lookup: <bytes> })` and
    `orderBy: { ssn_lookup: "asc" }` each refuse - paired with the same three
    over an ordinary declared field, which all succeed. Without the control, a
    totally broken read surface passes all three.
11. **Soft delete agrees with `find`.** A soft-deleted row is excluded by
    default and returned under `includeDeleted: true`. Otherwise the verb and
    `find` disagree about whether a row exists.
12. **The operand space admits only scalars.** `{ ssn: { $gt: v } }`,
    `{ ssn: [v] }` and `{ ssn: null }` each refuse with their typed code; paired
    with the scalar that succeeds.
13. **Composition refuses without key material.** A worker composed with a
    descriptor declaring a `lookupColumn` and no resolvable column key refuses at
    composition - paired, in the same test, with a configured key under which
    composition succeeds and a lookup works. A fail-closed default otherwise
    hides a total break.
14. **A lookup inside an explicit transaction sees the transaction's own
    writes.** Insert a row and look it up in the same transaction; paired with the
    same lookup from outside an open transaction, which must not see it. This arm
    also fails today for `unmask` (section 6.4b), which is the point of writing
    it.
15. **The audit write does not deadlock under pool saturation.** Fill the data
    pool exactly (8 concurrent explicit transactions,
    `lib.rs:998`) and require every one to complete a lookup including its audit
    write. This is SC-6's retracted saturation arm (`sc6:726-731`) re-acquiring a
    subject; its shape - fill the pool exactly, then require progress - is the
    general test for any second checkout taken while a first is held.

---

## 9. The case against this API existing at all

The strongest argument against it is not that it is unsafe. It is that "find the
account for this SSN" is an **identity-resolution** problem, and identity
resolution belongs to whoever owns identity - an external index, a tokenization
service, a deliberate second lookup path - rather than to a privileged verb bolted
onto the storage surface. Stated at full strength:

### 9.1 The verb encodes a policy decision the platform should not own

Real protected-value lookups are never exact. An SSN arrives with and without
dashes; an email arrives with different casing and plus-addressing; a phone
number arrives in four formats. The moment a lookup verb normalises, the platform
has decided what "the same SSN" means, for every app, forever. The moment it does
not normalise, it is brittle in exactly the way real lookups are not, and every
creator writes their own normalisation - badly, and differently on the read and
write paths, producing a token that never matches.

**This one lands, and the design concedes it.** The platform must **not**
normalise: the token is over the exact bytes the creator supplies, normalisation
is the creator's job before the call, and the reference documentation must say
so at the same volume it says everything else. That concession removes the
policy-in-the-platform objection at the cost of admitting that the verb is
exact-match and nothing more - which is what section 4 already says. What it does
not remove is the footgun: a creator who normalises on write and forgets on read
gets a permanently empty result with no error. A drift detector for the token
column (the analogue of `crud/mask_drift.rs`) is the mitigation, and it is owed.

### 9.2 The verb's authorization is only as strong as a self-asserted actor

Per AGENTS.md, a capability the worker can call is not privileged. The actor is
app-supplied and unbound to `env.auth` (`unmask.rs:265`, `sc6:652-672`), so
inside an app whose ceiling grants a role the lookup, any handler can perform any
lookup. An external service, the argument goes, would at least authenticate the
caller.

**This one loses, and the reason is that it proves too much.** It applies
identically to `unmaskField`, to `bulkUnmask`, to the per-query unmask hint, and
to every `env.db` read. Moving the lookup out of the database surface does not
make the actor trustworthy; it relocates the same self-asserted actor to a
service that must now authenticate it across a network boundary - a strictly
larger problem with the same root. Binding the actor to `env.auth` is the fix,
it is SC-6's to own, and an external index would need it too.

### 9.3 An external index would keep the plaintext out of the query surface entirely

Store an opaque token the app itself computes in an ordinary, queryable column;
keep the plaintext behind a service the database never sees. No new verb, no new
column semantics, no new policy axis.

**This one loses decisively, and the reason is that the alternative is not
neutral.** To build an external index the app must hold the plaintext at write
time and store a derived value somewhere queryable. In practice that means a
second copy of the asset in a place with:

- **no mask policy** - the token column is an ordinary field, filterable,
  sortable, projectable, and if the creator computes a plain hash rather than a
  keyed one, dictionary-attackable by anyone who reads the table;
- **no audit trail** - the lookup is an ordinary `find`, and no row is written
  anywhere;
- **no operator ceiling** - nothing an operator can revoke;
- **no ordering guarantee on ranges** - and if the creator stores anything
  order-preserving to make ranges work, the binary-search oracle is back, outside
  every defence this document builds.

"Solve it outside the database" is, concretely, "reconstitute the asset
somewhere the platform cannot see". That is worse than the verb on every axis the
verb was designed for.

### 9.4 And without it the feature is closed, which has a second-order cost

SC-6 says it plainly: "If it does not, the feature is closed rather than
secured" (`sc6:341-342`). A masking primitive that makes the common case
impossible does not get used carefully - it gets turned off. The creator drops
`.mask()` from the field, and the outcome is worse than no masking, because they
have also learned not to trust the primitive the next time.

### 9.5 Where the case against genuinely wins

Two places, and they should be recorded as limits rather than argued away:

- **Low-entropy protected values** (section 2.4). For a value with a small
  domain, an authorized caller enumerates it. The platform cannot measure
  entropy, so the only defences are the per-field opt-in, the audit digest and
  the rate limit - all of which are detection and pricing, not prevention. A
  creator who needs a small-domain value protected against the app's own handlers
  should not use this verb.
- **Anything needing fuzzy or normalised matching.** The verb is exact-match by
  construction (9.1). An app doing real identity resolution needs a resolution
  service, and this verb is not a substitute for one. It is the exact-match
  primitive such a service would be built on.

---

## 10. What this design owes, and what it hands to others

**Owed by item 1 before implementation:**

1. The `MaskPolicy` wire shape for the two-axis form, and the meet over the
   role-complete domain on **both** axes (section 1.8). SC-6's inversion defect
   reappears on the new axis otherwise.
2. The dedicated audit connection (section 6.4b), because the "audited or
   unanswered" guarantee in 6.3 is not deliverable on the shared 8-connection
   pool.
3. The token drift detector, the analogue of `crud/mask_drift.rs`, for the
   normalisation footgun in 9.1.
4. Backfill for a field that turns `.equalityIndex()` on after rows exist. The
   analogue of `crud/mask_backfill.rs`; free pre-launch, not free later. Note
   that it needs the plaintext, so it cannot run for a randomised-encrypted field
   without decrypting every row.
5. Key rotation invalidates every token. `keys.rs` has no rotation today, and
   `aad.rs:87-92` names the wire-version thread as its insertion point. A rotation
   design that ignores the token column silently breaks every lookup.

**Handed to others:**

- **Item 2:** `.unique()` on a randomised-encrypted masked field must target
  `storage.lookupColumn`, not `storage.rawColumn`, because a unique index on
  per-row-distinct ciphertext enforces nothing (section 3.2). Foreign keys on a
  protected field are unanalysed here.
- **Item 3:** the behaviour changes to document are section 4's eighteen items,
  not only the one item 3 names.
- **The operator:** deterministic encryption mode retains no capability the
  platform needs once this lands, and keeps DB-13 (section 3.1). The deferral at
  `runtime-db-binding-design.md:1112-1129` can now be decided on evidence.
- **Whoever owns CDC:** `ssn_lookup` joins `ssn_raw` on the change-stream wire
  (section 4.17). The capability flags are projection-side only.
- **Whoever owns the audit table:** it is created by runtime DDL
  (`unmask.rs:838-946`) that cannot alter an existing table, in a runtime that
  SC-3 says "executes no DDL" (`sc3:56`). This feature needs three new columns and
  cannot get them through that function.
- **SC-6:** `unmask` inside an explicit transaction reads on a separate pooled
  connection in its own transaction (`unmask.rs:570-572`, `exec.rs:293-318`), so
  it cannot see the transaction's own uncommitted writes. Found here, not fixed
  here.

**Not determined from the tree, and stated as such:**

- Whether `compio_postgres::Pool` blocks or errors on exhaustion. SC-6's
  "deadlock" characterisation (`sc6:150-155`) is carried forward on its authority,
  not re-measured.
- What the platform's per-request rate limit actually is on the worker path;
  section 2.4's argument leans on one existing without naming a number.
