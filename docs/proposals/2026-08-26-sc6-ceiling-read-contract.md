# SC-6: the mask ceiling contract

**Status.** PARTIAL. The storage half is SHIPPED - the masked field's own column
holds the mask, `__zs_raw__<field>` holds the authoritative value
(`crates/zeroship-schema/src/query.rs`, (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
`crates/zeroship-data-orm/src/crud/mask_pass.rs`,
`crates/zeroship-migrate-core/src/schema/diff.rs`, `docs/reference/db.md`). The
operator-ceiling half is NOT: `check_unmask_authorization`
(`crates/zeroship-data-orm/src/crud/unmask.rs:359`) still reads the per-isolate
policy cache with no ceiling parameter, and the `MaskCeiling` fold in the
transaction reducer (`crates/zeroship-data-orm/src/transaction/reducer/identity.rs:130`)
has no production producer.

There is no ceiling *read*, despite the filename. The operator ceiling is worker
configuration, not database state.

---

## What it is

### The effective ceiling is a field of the binding

The ceiling arrives as **worker configuration at composition**, is met with the
artifact-borne creator draft **once**, at binding construction, and is then
immutable for the isolate's life. A ceiling is changed by editing that
configuration and **rolling the workers**.

`check_unmask_authorization` gains the effective policy as a parameter and
**stays synchronous**:

```rust
pub(crate) fn check_unmask_authorization(
    app_id: &str,
    actor: &Option<Value>,
    classification: &str,
) -> Result<bool, DbError>
```

Three production call sites take it: `unmask.rs:503`, `:1021`, `:1236`. There is
no I/O on the authorization path, no round trip to count, no cache, no version,
and no staleness.

**The ceiling is resolved independently of the app-supplied actor** - never
fetched through a map entry the actor names. See "The ceiling, not the actor" below.

Not built. Today the function reads `crud::mask_policy::cache_get(app_id)`
(`crates/zeroship-data-orm/src/crud/mask_policy.rs:280`), a per-isolate
thread-local holding only the creator-declared half. `MaskCeiling::meet` and
`TxReducer::effective_ceiling` (`reducer/mod.rs:768`) exist and are tested, but
the single production observation site mints `MaskCeiling::default()`
(`crates/zeroship-data-orm/src/transaction/driver.rs:156-166`) and the accessor
has test-only consumers.

#### The accepted costs of configuration delivery

Changing a mask ceiling is a rare, critical operation. Buying sub-second
propagation for it with a cache-coherence problem on the hot path of a security
decision is the wrong trade. What that trade costs:

- **revocation latency is worker-roll time**, not instant;
- **deploy-pinned workflow isolates keep their old ceiling until evicted**
  (bounded by `max_pinned_isolates_per_app`), so force-eviction is the
  operator's lever for immediacy;
- those same isolates hold their old *declared* policy too, so force-eviction is
  the single lever rather than one of several.

### Failure is denial

An unresolvable or absent ceiling **denies**. **An absent ceiling is not an empty
ceiling**: "absent" means a worker composed without one, which is a configuration
error that **fails loudly at composition, not silently at the first unmask**.
That is what configuration delivery buys - a missing table could only be
discovered by an operation trying to read it.

This tightens today's fallback, which still permits `kind == "auto"` when no
policy is cached (`unmask.rs:371-375`, literally `Ok(kind == "auto")`).
Default-permit is defensible while the policy is app-declared convenience; it is
not defensible once the ceiling is the operator's limit.

### The meet is over a role-complete domain, not a map intersection

Effective policy is `operator_ceiling` met with `manifest_declared`. The meet
must be computed over a **role-complete domain**, because "absent" means opposite
things in the existing lattice:

```rust
match self.roles.get(role) {
    Some(set) => set.contains(classification),
    None => role == "auto",   // absent => TOP for `auto`, BOTTOM for everyone else
}
```

(`crates/zeroship-data-orm/src/crud/mask_policy.rs:114-122`.)

A naive intersection - keep only keys present in both sides - drops any key the
manifest does not mention. Combine that with a ceiling written specifically to
**revoke** `auto` (listing it with an empty set) against a manifest that never
mentions `auto`: the key disappears, the fallback fires, and `auto` is restored
to **every** classification. The revocation does not merely fail, it **inverts**,
for the one actor with the most access.

**The rule: both policies are normalised to total functions before the meet.**
`auto`'s implicit top is materialised as an explicit entry, every other absent
role is materialised as an explicit bottom, and the meet is pointwise over the
union of roles.

**The ceiling can therefore always narrow, `auto` included, with no exception.**
`auto`'s apparent privilege is a lattice default, not an exemption. The method's
own doc says so in the imperative - "To restrict the system actor, the policy
MUST list `auto`" (`mask_policy.rs:102-107`) - and the shipped reference agrees
(`docs/reference/db.md:1610`).

A role the ceiling does not name is **not narrowed at all**; it keeps its lattice
default. That is the scope of the guarantee, and the acceptance arms pin both
halves.

### The ceiling, not the actor, is the security boundary

The actor is **self-asserted by app JS**. `RESERVED_SYSTEM_ACTOR_KINDS` is
exactly `["auto"]` (`unmask.rs:309`), so `sanitize_app_actor` (`:321`) strips the
platform actor and *nothing else*. An app handler may present
`{ actor: { kind: "admin" } }` and receive whatever the app's own policy grants
`admin`; no part of the path binds the actor to `env.auth`.

That is coherent while the mask policy is app-declared convenience. It stops
being coherent the moment the operator ceiling becomes a revocation mechanism,
because an operator revoking access must not be defeated by the tenant naming a
different role.

The property carrying the weight is the **meet**: a self-asserted role can never
exceed the ceiling. Its integrity depends on the ceiling being resolved
independently of the actor. Consult the ceiling through the actor's own key and
the tenant regains control of its own limit by choosing a role the ceiling does
not mention.

`sanitize_app_actor` has **five** call sites, and all five are fences:
`unmask.rs:1514` and `:1629` (arg parsing), `crud/mod.rs:669` (the query hint,
on the eager half of `plan_find`), and `v8_classes/masked_value.rs:300` and
`:423` (the creator-facing single and bulk unmask). Count them with
`grep -rn 'sanitize_app_actor(' crates/zeroship-data-v8/src` and do not
truncate the output. The stripped claim is preserved for audit in its own
`claimed_actor` column, serialised whole rather than split into the trusted
`actor_id` / `actor_role` fields (`unmask.rs:828-852`).

### What the creator half contributes

`manifest_declared` is **untrusted, deploy-scoped data**. The `.zship` manifest
carries no signature (`crates/zeroship-bundle/src/manifest.rs`; `deploy_hash` is
a digest the control plane computes on receipt, `:46-49`), so a build-graph
dependency can author it. Its only security property is that it cannot outlive
its deploy.

Its carrier is the artifact/init channel that already carries the runtime schema
descriptor, immutable for the isolate's life. It is not a new channel and not a
new trust standing.

Not built. The shipped carrier is `defineMaskPolicy` (`sdks/db/src/policy.ts:120`),
drained at boot through `__platform.setMaskPolicy`
(`crates/zeroship-data-v8/src/v8_classes/db_platform.rs:100`) into the
per-isolate cache. The string `mask` appears **zero** times in
`crates/zeroship-bundle/src/manifest.rs`. Defining `manifest_declared` as an
artifact needs five pieces: the artifact field and its schema; the authoring
surface; build-time validation; the emission path through the packer; and the
runtime read that turns bytes into a `MaskPolicy`.

### Relationship to the migration ceiling

The platform already has operator-ceiling machinery with meet semantics
(`crates/zeroship-migrate-server/src/policy.rs`,
`crates/zeroship-migrate-server/policies/confined.policy.toml`), and masking uses
the same delivery model. `migrated`'s default ceiling is `CONFINED_CEILING_TOML`
(`policy.rs:56-60`): a TOML document compiled into the binary with
`include_str!` - operator configuration delivered at composition, exactly what a
mask ceiling is. The compose is `compose_effective_for_app` (`policy.rs:122`).
Masking looks like its neighbour, with two differences.

- **Key vocabulary.** That ceiling is DDL-knobs-only (`CREATE TABLE` /
  `CREATE SCHEMA` / `RENAME` / destructive-ops / RLS), with no vocabulary for
  mask classifications. Separate document, separate keys. Sharing the store would
  put two unrelated policies under one name.
- **Meet semantics.** `migrated`'s compose is **escalation-reject**: a draft
  grant looser than the ceiling permits is rejected, never clamped
  (`policy.rs:16-17`, restated at `:119-121`). Masking's meet **clamps**,
  silently. Both are defensible and they are not interchangeable: reject surfaces
  the creator's mistake at deploy time, clamp lets a deploy succeed with less
  access than it asked for. **Masking keeps clamp.**

### The physical layout: the mask is the column

Shipped.

- **The field's own column holds the MASKED value, as bare `TEXT`.**
- **`__zs_raw__<field>` holds the authoritative value** - plaintext for a
  mask-only field, ciphertext for an encrypted one - and carries the **declared
  type and the whole constraint set** (`NOT NULL`, `DEFAULT`, range `CHECK`,
  literal `CHECK`, enum `CHECK`). The name comes from `raw_column_name`
  (`crates/zeroship-schema/src/query.rs:2232`) via `raw_column_for_field` (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
  (`:2264`), the DDL emitter's own functions.
- **The raw column is unqueryable** - not in a filter, not in a projection, not
  in a sort, not a field of the generated type.
- **Plaintext is reachable only through the explicit unmask API**, which is where
  authorization and the audit row already live.

The point is that **the ignorant path is the safe path**. A builder that has
never heard of masking names the column with the natural name, and that column is
the mask. The `"<col>_masked" AS "<col>"` select-list substitution is gone
entirely, along with `read_column_for` and `aggregate_read_ident`
(`query.rs:3793-3800` records their deletion); every read surface names the
field's own column.

**The layout is the second line of defence, not the primary.** Descriptor
correctness is an enforced invariant owned by the deploy pipeline; the layout's
unique property is that it stays safe when that enforcement has a bug. The mask
pass runs only `if schema_has_masked_columns(&schema)`
(`crates/zeroship-data-orm/src/crud/read_pipeline.rs:121`, `crud/mod.rs:2190`),
which reads the descriptor, and the data plane performs no live introspection. So
a descriptor that fails to declare a field masked runs no mask pass and passes
the column's contents through untouched. With the mask in that column that
returns a mask; with plaintext in it, it returned plaintext. The physical layout
is the only thing standing between a stale descriptor and a plaintext read.

#### What holds the layout closed

- **Inbound.** `RESERVED_NAMES` carries `ReservedName::Prefix("_")`
  (`query.rs:785-789`) and the engine's independent table carries the same
  (`crates/zeroship-migrate-core/src/schema/query.rs:365-369`).
  `validate_field_name` (`query.rs:839`) gates every inbound surface - write
  document keys including nested, filter keys, conflict-probe keys, read
  identifiers - so a leading-underscore column is unrepresentable on every path a
  creator can reach. Aggregate `$match` passes the schema hint like its
  neighbours (`query.rs:5136-5137`).
- **Outbound.** The write builders emit no `RETURNING *` (pinned by
  `query.rs:14091` and `:14218`), so no write verb returns the raw sibling.
  `wrap_row_on_read` (`crud/mask_pass.rs:476`) re-applies the mask transform to
  the field's own slot rather than trusting it, and strips
  `raw_column_name(col)` when present - which is the shape a WAL-decoded row
  still has.
- **Live-query subscriptions.** `normalise_filter` (`read_set.rs:285`) lowers an
  `Eq` operand through the column's own mask kind so mask-against-mask
  comparison still matches the WAL tuple, and refuses to lower a range
  (`:338-341`), because a range over a mask is not a range over the value.
- **Dev tier.** The SQLite mask sentinel rides the masked (logical) column, so
  there is no suffix to strip and no silent-discard arm
  (`crates/zeroship-data-orm/src/backend/sqlite/mod.rs:1531-1545`).
- **Migrations.** The differ classifies mask transitions on existing columns as
  `MaskBackfill` / `MaskRewrite` / `MaskRemove`
  (`crates/zeroship-migrate-core/src/schema/diff.rs:104-180`, transitions at
  `:689-745`). Adding `.mask()` to an encrypted column that already holds data
  stays refused: computing a mask needs AEAD key material the engine does not
  hold.
- **The AAD binds the LOGICAL FIELD KEY, not the physical column.**
  `canonical_aad(collection, column, row_pk_bytes)`
  (`crates/zeroship-data-orm/src/encryption/aad.rs:75`) takes `column` from
  `for (col, def) in schema_obj.iter()` - the schema field key
  (`crud/encryption_pass.rs:169`, `:273`; same at `crud/unmask.rs:580`). That is
  what made the flip a rename rather than a re-encrypt.

Behaviour a creator sees, documented at `docs/reference/db.md:1372-1396`:
`find({ ssn: "123-45-6789" })` matches nothing, `$gt` compares masks, `orderBy`
sorts by the mask, `$group.by` buckets per distinct mask. `.unique()` and
`.index()` are declared about the real value and are built on the column that
holds it.

### Joined reads

A joined result **is nested before the read pipeline runs**. Today that is true
by construction: `with: { <ref field>: true }` fires one batched
`find({ id: { $in: [...] } })` per relation against the **target** collection and
stitches the rows in the SDK (`sdks/db/src/collection/relations.ts`), so every
row passes its own collection's decrypt, mask and normalisation passes with its
own collection context. Mask policy and unmask authorization are therefore
per source collection already.

**Any future lowering that produces a flat joined row must nest first.** A flat
row is not a presentation problem, it breaks three mechanisms keyed on a single
unqualified name per row:

1. **The unmask handle** plucks one `row_pk` from `obj.get("id")`
   (`mask_pass.rs:494`). On a flat row that `id` is the parent's, so
   `unmask()` on a child's column fetches **the wrong row's plaintext under the
   wrong collection's policy**. The handle is well-formed and wrong. (The same
   code shows a row lacking `id` gets the empty string and `unmask()` rejects it
   - a second, independent reason projection narrowing may never drop `id`.)
2. **The AAD** binds `(collection, column, row_pk)` into the AEAD additional
   data, so a child's ciphertext decrypts **only** under the child's own
   collection name and row primary key. Hand the decrypt pass a flat row and the
   tag fails: not mis-authorized, *unreadable*. This one cannot be patched at the
   call site, because the binding is inside the authentication tag.
3. **DB-7's sentinel signature.** `_sig` (`mask_pass.rs:554-560`,
   `:574-580`) is an unforgeable per-process signature proving a `__zsmask__`
   sentinel came from the read pipeline. On a flat row the pipeline **itself**
   stamps `_meta` with the parent's `(collection, row_pk)` for a child's column.
   That sentinel is genuinely produced by the trusted path, carries a **valid**
   signature, and points at the wrong row in the wrong collection. `_sig`
   authenticates provenance; the provenance is real. The defence is not aimed at
   a trusted producer emitting a wrong `_meta`.

A column aliased into a flat row skips the passes keyed on its real name
entirely - which for a mask-only child column means it is returned as
**plaintext**, below the policy layer, with no denial and no audit row. That is a
leak by *schema scoping*, not by authorization.

Over-masking, the other direction, is not cosmetic either: a masked join key is
no longer the value the stitch matches on, so the relation errors or nests
`null`, and a nested `null` already means *the target row is missing or deleted*
(`docs/reference/db.md:615-616`) - a masking mistake presenting as a data statement
about the tenant's rows.

### Acceptance shape

**Every arm whose expected outcome is a denial carries a granted-path control.**
A fail-closed default hides a total functional break behind a green test: a
mechanism returning `permission denied` for every in-transaction unmask would
satisfy a deny-only arm **vacuously**, with every non-`auto` unmask bricked.

- **A binding constructed under a lowered ceiling denies `unmask` by an actor the
  effective ceiling governs**, paired in the same test with a classification the
  same ceiling still permits, which must SUCCEED.
- **A ceiling that revokes `auto` denies `auto` when the creator draft does not
  mention `auto` at all**, paired with a role the ceiling does not name, which
  keeps its lattice default. This is the sharpest arm: the naive map-intersection
  implementation **inverts** the revocation for the actor with the most access,
  and passes every other arm here.
- **An absent or unresolvable ceiling denies, refused at composition rather than
  at the first unmask**, paired in the same test with a configured permissive
  ceiling that grants - so the arm cannot go green where every composition fails.
- **The effective policy is identical inside and outside an explicit
  transaction**, asserted with the same classification unmasked both ways in one
  test. True by construction, which is exactly why it is worth asserting: a
  regression reintroducing a per-authorization lookup shows up here and nowhere
  else.
- **A mask-only column on a joined child is masked in the result**, and the same
  test asserts the parent's own mask-only column is masked too.
- **`unmask()` on a child column fetches that child's row under that child's
  policy**, asserted by giving parent and child rows distinct plaintext.
- **The audit row for a joined unmask names the source collection**, not the
  operation's target collection.
- **A joined row's mask policy is resolved per source collection**, asserted with
  a parent and child whose policies **differ on the same classification**. A
  same-policy fixture passes on the broken implementation.
- **Plaintext remains reachable by real value** through the explicit API - see
  Open 1. Without this arm the feature is closed rather than secured.

---

## Why it is this way

- **An authority read never traverses the data snapshot and never runs under the
  tenant's own role.** `apply_per_app_role`
  (`crates/zeroship-data-orm/src/backend/postgres/implementation.rs`) issues `SET LOCAL ROLE`
  with the DB-1 timeout guards immediately after the top-level `BEGIN`, so every
  later read on that connection runs **as the per-app role**.
- **A second connection taken while holding a first is a deadlock, not a latency
  cost.** The data pool holds **eight** connections
  (`PostgresBackend::connect(&url, 8, ...)`,
  `crates/zeroship-data-v8/src/lib.rs:1243-1247`); eight concurrent
  transactions each wanting a ninth is a cycle no single-transaction test can
  expose.
- **The ceiling is the security boundary and the actor is not.** Any change that
  resolves the ceiling through a key the actor supplies hands the tenant control
  of its own limit.
- **The masked column must stay the ignorant path's target.** Any change that
  puts plaintext back under the field's natural name reopens the filter oracle:
  the `WHERE` builder cannot substitute (it takes no schema hint by design), so a
  range filter plus `orderBy` plus `limit` binary-searches a value the caller
  cannot read, with no authorization check and no audit row. The audit guarantee
  covers `.unmask()` calls; a filter comparison is not one.
- **The raw column's unreachability rests on the leading underscore**, enforced
  by two independent reservation tables with no dependency edge between them
  (`crates/zeroship-schema/src/query.rs:785` and (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
  `crates/zeroship-migrate-core/src/schema/query.rs:365`). A rename that drops
  the underscore needs a new fence on every inbound surface first.
- **Masking clamps; the migration ceiling rejects.** Do not unify them because
  they share the word "ceiling".

---

## Open

1. **Lookup by real value.** "Find the account for this SSN" is the common case
   for a masked column and cannot be expressed as a filter. Nothing in the tree
   supports query BY plaintext - only read OF plaintext for a row already in
   hand; `docs/reference/db.md:1393-1395` currently tells creators it is
   unsupported. Uniqueness over the real value compounds it: on a
   randomised-encrypted field the raw column enforces **nothing**, because
   `canonical_aad` binds the row PK so identical plaintext yields different
   ciphertext per row. `sdks/db/src/types.ts:1184-1199` refuses
   `randomised` + `.unique()` for exactly that reason. A deterministic keyed
   lookup column would make equal plaintext produce an equal token by
   construction and turn a currently-refused declaration into a supportable one
   (`docs/reviews/2026-08-27-query-by-plaintext.md`). NEEDS-DECISION: does the
   explicit API gain a lookup verb, and does the keyed lookup column ship with
   it?
2. **The AAD role discriminator.** `canonical_aad` binds the bare logical field
   name. That is sufficient today. It stops being sufficient the moment Open 1's
   keyed lookup column lands: two encrypted columns then share one logical field
   and one AAD, so swapping their contents passes tag verification. The fix is to
   bind the logical field name **plus a stable role discriminator**
   (`value` | `lookup`) - a role survives renames, a physical name does not.
   BUILDABLE, 4h, but only meaningful alongside Open 1.
3. **The dev and `zeroship serve` ceiling source.** "Worker configuration" names
   the worker's composition point. `zeroship serve` and the Vite dev vector are
   separate composition points and nothing specifies a ceiling for them. A dev
   tier with no ceiling source plus "failure is denial" denies every non-`auto`
   unmask in dev, permanently. Dev's guarantee on this tier is **contract
   parity**, not the same adversarial posture - the developer owns the bytes
   there - which is the split `docs/reference/auth-dev-tier.md` already draws.
   NEEDS-DECISION.
4. **A ceiling change has no audit citation.** Worker configuration gives none by
   itself. If "which ceiling was in force when this unmask was authorized" must
   be answerable after the fact, the binding's effective policy needs an identity
   the audit row records. NEEDS-DECISION on whether that question must be
   answerable; BUILDABLE at 6h once it is.
5. **`manifest_declared` is not defined as an artifact.** It is named throughout
   this document as one of two inputs to the effective policy, and no document
   defines its shape. Five pieces, listed under "What the creator half
   contributes". This is a prerequisite of deleting `defineMaskPolicy`: with no
   declared policy, `cache_get` yields nothing and the fallback admits only
   `auto`, so **every creator role is denied every unmask** while the feature
   appears configured. It fails closed, which is the right direction and the
   wrong outcome. BUILDABLE, 12h.
6. **The ceiling has no producer.** `MaskCeiling`, `meet`, the `Verdict` arms and
   `effective_ceiling()` are built and tested; the sole production observation
   site mints `MaskCeiling::default()` and `effective_ceiling()` has test-only
   consumers. `check_unmask_authorization` never sees any of it. This is the
   build item the whole ceiling half reduces to once 3, 4 and 5 are answered.
   BUILDABLE, 16h.
7. **The descriptor's `storage` block has no reader.** The descriptor carries a
   `storage` block through to the runtime
   (`crates/zeroship-data-orm/src/descriptor.rs:1-46`), but the data plane still
   derives the raw column by formatting the fixed prefix
   (`crate::query::raw_column_name`). One emitter and several derivers is one
   emitter too few. BUILDABLE, 6h.

---

## History

Deliberation lives in `docs/proposals/2026-08-26-runtime-db-binding-decision-log.md`.
The deleted ceiling-read design - a ceiling table in `__zeroship_admin`, its
control-plane writer and CAS, per-operation and in-transaction reads, the
dedicated authority pool, version discovery and linearization - is recorded
there. The write path of the storage flip is specified in
`docs/reviews/2026-08-28-flip-write-path.md`; the lookup-column analysis in
`docs/reviews/2026-08-27-query-by-plaintext.md`.

DO-NOT notes, each recording something that broke or would have:

- **Do not compute the meet as a map intersection.** It was analysed and rejected
  because it inverts an `auto` revocation into full access when the creator draft
  never mentions `auto`, and passes every other acceptance arm.
- **Do not "fix" the AAD to bind the physical column name.** A doc comment
  asserted the opposite as fact - that a `RENAME COLUMN` leaves cells
  authenticated under the old name - and acting on it would destroy every
  ciphertext in the deployment, because every stored cell is authenticated under
  the logical field name. The comment has been corrected in place
  (`crates/zeroship-migrate-core/src/render/gen_types.rs:157-174`); do not
  reintroduce it. There is deliberately no separate `aadColumn`: a field that can
  disagree with the rule is a second source of truth for one fact.
- **Do not revert the storage flip to police the filter path instead.** Refusing
  ordered comparison on masked columns, refusing all filtering, or allowing it
  with an audit row all accept plaintext under the natural name and are
  fail-open: any path that forgets to ask "is this masked?" reads it. The
  asymmetry between one schema-aware path and one that is not *was* the defect.
- **Do not leave the declared type and constraints on the masked column.** Post
  flip that column holds `'***'`: a `DOUBLE PRECISION` write is a hard error, an
  enum `CHECK` refuses every write, and encrypted+masked leaves it `BYTEA`. The
  type and the whole constraint set travel to the raw column; pinned by
  `a_masked_columns_type_and_constraints_travel_to_the_raw_column`
  (`crates/zeroship-schema/src/query.rs:12318`). (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
- **Do not strip a `_masked` suffix when recovering SQLite mask sentinels.** The
  strip that used to be there had no `else` arm, so after the flip it would have
  matched every sentinel and reported every masked column as unmasked - silently,
  unlike the loud malformed-sentinel arm beside it.
- **Do not re-derive the raw column name at a call site.** It is
  `raw_column_name` / `raw_column_for_field` in the DDL emitter, and a name
  derived at several sites is several chances to disagree with the one emitter
  that created the column. Open 7 finishes the job.
- **Do not assume `sanitize_app_actor` guards three sites.** It guards five, and
  an undercount invites an auditor to conclude the two creator-facing fences are
  redundant. Every published line number for these has drifted at least once;
  re-derive rather than trusting a citation.
