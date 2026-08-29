# SC-6: the mask ceiling contract

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** the mask-policy section of that document.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

*(The filename says "read contract". There is no ceiling read: the operator
ceiling is worker configuration, not database state. The read design this file
once carried - a ceiling table in `__zeroship_admin`, its control-plane writer
and CAS, the per-operation and in-transaction reads, the dedicated authority
pool, and the version-discovery and linearization contract - is recorded in
`2026-08-26-runtime-db-binding-decision-log.md`.)*

---

## The contract

**The effective ceiling is a field of the binding.** It arrives as worker
configuration at composition, is met with the artifact-borne creator draft
**once**, at binding construction, and is then immutable for the isolate's life.
A ceiling is changed by editing that configuration and **rolling the workers**.

`check_unmask_authorization` gains the effective policy as a parameter and
**stays synchronous**:

```rust
pub(crate) fn check_unmask_authorization(
    app_id: &str,
    actor: &Option<Value>,
    classification: &str,
) -> Result<bool, DbError>
```

Today it reads an in-memory thread-local instead -
`crate::context::with(|c| c.mask_policy_for(app_id))`
(`crates/zeroship-plugin-db/src/crud/unmask.rs:290`, read at `:299`), with three
production call sites (`unmask.rs:400`, `:1067`, `:1269`).

There is no I/O on the authorization path, no round trip to count, no cache, no
version, and no staleness.

**One property this contract must not lose:** the ceiling is resolved
**independently of the app-supplied actor**, never fetched through a map entry
the actor names ("The ceiling, not the actor, is the security boundary", below).

### The costs of delivering the ceiling as configuration - accepted, do not "fix"

Changing a mask ceiling is a rare, critical operation, and buying sub-second
propagation for it with a cache-coherence problem on the hot path of a security
decision is the wrong trade. What that trade costs:

- **revocation latency is worker-roll time**, not instant;
- **deploy-pinned workflow isolates keep their old ceiling until evicted**
  (bounded by `max_pinned_isolates_per_app`), so the operator's lever for
  immediacy is force-eviction;
- those same isolates hold their old *declared* policy too (decision 3), so
  force-eviction is the single lever rather than one of several.

### Failure is denial

An unresolvable or absent ceiling **denies**. This tightens today's fallback:
currently a missing policy still permits `kind == "auto"`
(`unmask.rs:300-307`), which is defensible when the policy is app-declared
convenience and is not defensible once the ceiling is the operator's limit.

**An absent ceiling is not an empty ceiling.** "Absent" now means a worker
composed without one, which is a configuration error that **must fail loudly at
composition, not silently at the first unmask**. That is the improvement the
configuration delivery buys: a missing table could only be discovered by an
operation trying to read it; missing configuration can be refused before the
worker serves anything.

**Owed: the dev and `zeroship serve` ceiling source is not specified anywhere.**
"Worker configuration" names the worker's composition point. `zeroship serve`
and the Vite dev vector are separate composition points; SC-4 and SC-5 cover
neither. A dev tier with no ceiling source plus "failure is denial" denies every
non-`auto` unmask in dev, permanently. Dev's guarantee on this tier is
**contract parity**, not the same adversarial posture - the developer owns the
bytes there - which is the split `docs/reference/auth-dev-tier.md` already draws.

**Owed: a ceiling change has no audit citation.** The deleted table gave one - a
monotonic version an audit row could name. Worker configuration gives none by
itself. If "which ceiling was in force when this unmask was authorized" must be
answerable after the fact, the binding's effective policy needs an identity the
audit row records, and nothing in this set specifies one.

### Two rules the deleted read path leaves behind

Both generalise past the mechanism that produced them, and both would otherwise
be rediscovered by the next feature that wants a value mid-transaction.

1. **An authority read never traverses the data snapshot and never runs under
   the tenant's own role.** `apply_per_app_role`
   (`crates/zeroship-plugin-db/src/transaction/mod.rs:229`) issues
   `SET LOCAL ROLE` with the DB-1 guards immediately after the top-level
   `BEGIN`, so every later read on that connection runs **as the per-app role**.
2. **A second connection taken while holding a first is a deadlock, not a
   latency cost.** The data pool holds **eight** connections
   (`Pool::connect(&url, 8)`, `crates/zeroship-plugin-db/src/lib.rs:998`); eight
   concurrent transactions each wanting a ninth is a cycle no
   single-transaction test can expose.

## The meet is not a map intersection, and getting that wrong inverts revocation

Effective policy is `operator_ceiling` **met with** `manifest_declared` - but the
meet must be computed over a **role-complete domain**, not by intersecting the
two maps' keys. The reason is an asymmetry in the existing lattice that makes
"absent" mean opposite things:

```rust
match self.roles.get(role) {
    Some(set) => set.contains(classification),
    None => role == "auto",   // absent => TOP for `auto`, BOTTOM for everyone else
}
```

(`crates/zeroship-plugin-db/src/crud/mask_policy.rs:117-125`. The inline comment
is this document's gloss; the tree's comment on that arm reads "The `auto`
fallback rule - see method doc-comment.")

A naive intersection - keep only keys present in both sides - drops any key the
manifest does not mention. Follow that with an operator ceiling written
specifically to **revoke** `auto` (listing it with an empty set) against a
manifest that never mentions `auto`: the key disappears from the result, the
fallback fires, and `auto` is restored to **every** classification. The
revocation does not merely fail - it **inverts**, and it does so for the one
actor with the most access.

**The rule: both policies are normalised to total functions before the meet.**
`auto`'s implicit top is materialised as an explicit entry, every other absent
role is materialised as an explicit bottom, and the meet is then pointwise over
the union of roles.

**The ceiling can therefore always narrow, `auto` included, and there is no
exception.** `auto`'s apparent privilege is a *lattice default*, not an
exemption; materialise the default and it is an ordinary role. The method's own
doc says so in the imperative - "to restrict the system actor, the policy MUST
list `auto`" - and the shipped reference agrees
(`docs/reference/db.md:1584-1586`). The one property that is genuinely special
is that `auto` is **not forgeable from app JS**: `sanitize_app_actor` strips an
app-supplied `kind == "auto"` before it reaches any of the three authorization
entries (`sanitize_app_actor`, `crates/zeroship-plugin-db/src/crud/unmask.rs:305-316`;
applied in `parse_args`, `parse_bulk_args` and `crud/mod.rs`'s query-hint path,
which is what makes "any of the three" true rather than two out of three).

A role the ceiling does not name is not narrowed at all - it keeps its lattice
default. That is the scope of the guarantee, and the acceptance arms below pin
both halves.

## The ceiling, not the actor, is the security boundary

This must be stated rather than relied on implicitly, because the actor is
**self-asserted by app JS**. `RESERVED_SYSTEM_ACTOR_KINDS` is exactly `["auto"]`
(`unmask.rs:265`), so `sanitize_app_actor` strips the platform actor and
*nothing else*. An app handler may present `{ actor: { kind: "admin" } }` and
receive whatever the app's own policy grants `admin`; no part of the path binds
the actor to `env.auth`.

That is coherent while the mask policy is app-declared convenience - the app
authorizes itself against its own declaration. It stops being coherent the
moment the operator ceiling becomes a revocation mechanism, because an operator
revoking access must not be defeated by the tenant naming a different role.

The property that carries the weight is the **meet**: a self-asserted role can
never exceed the ceiling. Its integrity depends on the ceiling being resolved
independently of the actor. Consult the ceiling through the actor's own key and
the tenant regains control of its own limit by choosing a role the ceiling does
not mention.

## What the creator half contributes

`manifest_declared` is **untrusted, deploy-scoped data**. The `.zship` manifest
carries no signature (`crates/zeroship-bundle/src/manifest.rs`; `deploy_hash` is
a digest the control plane computes on receipt, `:44-48`), so a build-graph
dependency can author it. Its only security property is that it cannot outlive
its deploy.

Its carrier is decided (decision 3): the creator's mask policy is declared in
the creator's codebase, folded at build time, and delivered through the
**artifact/init channel that already carries the runtime schema descriptor**,
immutable for the isolate's life. It is not a new channel and not a new trust
standing - it is the descriptor's, so the paragraph above applies to it
unchanged.

**Owed: `manifest_declared` is named throughout this document as one of the two
inputs to the effective policy, and no document defines it as an artifact.**
Five pieces:

1. the artifact field and its schema;
2. the authoring surface that produces it;
3. validation at build time;
4. the emission path through the packer;
5. the runtime read that turns bytes into a `MaskPolicy`.

**They are a prerequisite of the parent's deletion step, in the same step.** The
parent deletes the only creator-facing way to declare a mask policy -
`defineMaskPolicy` (`sdks/db/src/policy.ts:118`, reached through the bootstrap's
dev and runtime entries) - and there is no latent route waiting to replace it:
the string `mask` appears **zero** times in the Rust manifest
(`crates/zeroship-bundle/src/manifest.rs`) and **zero** times in the TypeScript
manifest shape (`sdks/vite-plugin/src/zship.ts`). With no declared policy,
`mask_policy_for` yields nothing and the fallback admits only `auto`, so **every
creator role is denied every unmask** and the masking feature is inert for its
actual users while appearing to be configured. It fails closed, which is the
right direction and the wrong outcome.

## Relationship to the migration ceiling

The platform already has operator-ceiling machinery with meet semantics
(`crates/zeroship-migrate-server/src/policy.rs`, `policies/confined.policy.toml`), and
masking now uses the same delivery model, not merely the same vocabulary.
`migrated`'s default ceiling is `CONFINED_CEILING_TOML` (`policy.rs:48-59`): a
TOML document compiled into the binary with `include_str!` - operator
configuration delivered at composition, exactly what a mask ceiling is. The
compose is `compose_effective_for_app` (`policy.rs:119-122`). Masking should
look like its neighbour, with two differences.

- **Key vocabulary.** That ceiling is DDL-knobs-only: `CREATE TABLE` /
  `CREATE SCHEMA` / `RENAME` / destructive-ops / RLS (`policy.rs:32-35`), with
  no vocabulary for mask classifications. Separate document, separate keys.
  Sharing the store would put two unrelated policies under one name.
- **Meet semantics - the one not to copy by accident.** `migrated`'s compose is
  **escalation-reject**: "a draft grant looser than the ceiling permits is
  rejected, never clamped" (`policy.rs:16-17`, restated at `:120-121`).
  Masking's meet **clamps**, silently. Both are defensible and they are not
  interchangeable: reject surfaces the creator's mistake at deploy time, clamp
  lets a deploy succeed with less access than it asked for. **This document
  keeps clamp**, and records the divergence so "look at its neighbour" is not
  read as "copy its semantics".

## Masking is projection-shaped: the filter oracle and the storage flip

The mask substitution lives in the **select list**: a masked column is read as
`"<col>_masked" AS "<col>"`, and `aggregate_read_ident`
(`crates/zeroship-schema/src/query.rs:3431`) extends the same treatment to
aggregates, `$group.by`, `$having` and aggregate `$sort`. Nothing else is
mask-aware, and the `WHERE` builder **cannot be**: its signature is
`build_where_with_dialect(filter, params, dialect)` (`query.rs:5244-5252`) -
there is no schema hint parameter at all, so it has no way to know a column is
masked. The projection path takes one and calls
`column_is_masked(field, schema_hint)` (`query.rs:3372`).

A masked column stores **plaintext in the parent column** (the masked form lives
in the sibling). So `find({ ssn: { $gt: "500-00-0000" } })` renders
`WHERE "ssn" > $1` and compares against plaintext. The caller never sees an
unmasked value in the result - and does not need to: **the set of matching rows
is the answer.** Repeated queries binary-search the exact value, with no
authorization check anywhere on the path and no audit row written.

This does not violate the letter of the audit guarantee:
`docs/reference/db.md` promises `__zeroship_audit_unmask` records "every
`.unmask()` call (granted or denied)", and a filter comparison is not an
`unmask()` call. What fails is the **protection goal** - plaintext is not
disclosed without authorization - through a channel the guarantee was never
scoped to cover, and reading the audit table would show nothing unusual while it
happened.

### The decision: flip the storage

- **`ssn` stores the MASKED value; the raw column stores the real one.**
- **The raw column is unqueryable** - not in a filter, not in a projection, not
  in a sort. It is not a field of the generated type.
- **Plaintext is reachable only through an explicit API**, which is the place
  authorization and the audit row already live.

The alternatives - refuse ordered comparison on masked columns, refuse all
filtering on them, or allow filtering and audit it - all accept the current
physical layout and then try to police the filter path on top of it. Each leaves
the design **fail-open**: plaintext sits in the column with the natural name, so
any code path that forgets to ask "is this masked?" reads it. That is not
hypothetical - the asymmetry between one schema-aware path and one that is not
**is** this defect.

After the flip the ignorant path is the safe path. A query that knows nothing
about masking selects and filters the masked column and leaks nothing. The
`"ssn_masked" AS "ssn"` substitution disappears entirely, so there is one less
place to get wrong rather than one more. And the audit guarantee goes from
narrow-but-true to simply true: when plaintext has exactly one reader, the
guarantee covers the asset rather than one path to it.

### The flip is the second line of defence, not the primary

Descriptor correctness is an enforced invariant, owned by the parent: the deploy
pipeline refuses to make a deploy live until its migrations have applied, and
mid-life drift and restore are answered by the schema-change signal. Enforcement
is code, and the flip's unique property is that it stays safe when that code has
a bug. The data plane performs no live introspection (decisions 7 and 8), so
**the physical column layout is the only thing standing between a stale
descriptor and a plaintext read.**

`read_pipeline::apply` runs the mask pass only `if schema_has_masked_columns(&schema)`,
and that predicate reads the descriptor (`crud/mod.rs:2590-2602`). When the
descriptor does not declare a field masked, no mask pass runs and the parent
column's contents pass through untouched:

| | descriptor declares the mask | descriptor does NOT (stale) |
| --- | --- | --- |
| **today** (parent = plaintext) | masked | **plaintext returned** |
| **post-flip** (parent = mask) | masked | **mask returned - safe** |

### The flip's accepted cost - do not "fix" it by reverting

Pre-launch, breakage is not a cost: the 22 production sites that format
`<col>_masked` and the type-and-constraint swap below are exactly what
pre-launch exists for. **One cost survives.** The migration engine cannot see
this change - the column-additions branch is name-only "no matter how its
declared type has changed" (`crates/zeroship-migrate-core/src/schema/diff.rs`,
its own comment) and the `RewriteColumnType` arm keys strictly off the
`encrypted` toggle (`diff.rs:911`). The flip moves neither the name nor that
toggle, **so the differ emits nothing.** The flip's migration is therefore
hand-authored with nothing verifying it, and it is the piece that touches real
column data: **it owes a mutation-proved test before it runs anywhere.**

### The write path must be specified before the flip is implemented

The decision's whole claim is that the raw value becomes unreachable. On the
write path it is reachable in both directions, and a partial implementation
would retire a known leak and open an unknown one. All four items below are
verified in the tree and each is silent.

**Outward - every write verb returns the raw sibling.** There are **twelve**
SQL-emitting `RETURNING *` sites in `crates/zeroship-schema/src/query.rs`
(insert, updateOne, insertMany, updateMany, delete, soft-delete/restore, upsert,
findOrCreate). `RETURNING *` is every physical column and it never passes
through `implicit_read_projection_parts` (`query.rs:3344`), which is SELECT-side
only. The stripper knows exactly one sibling name:

```rust
let sibling_key = format!("{col}_masked");   // crud/mask_pass.rs:469
```

and pushes only that onto `to_strip`. **Post-flip that key does not exist**,
`to_strip` is empty, and the raw column survives. Its fallback arm re-masks the
parent correctly, so the masked column comes back right - which is exactly what
makes this silent. `strip_encryption_markers` retains everything not prefixed
`__zsbin__` (`crud/encryption_pass.rs:504`), and `mapResultDoc` copies every key
with an identity fallback (`sdks/db/src/utils.ts:28-34`). So
`await db.users.insert({ ssn })` returns the plaintext in a key the generated
`Row<S>` type does not declare - invisible to any review written against the
generated types. `rawProjectable: false` does not help: nothing reads it, and
`RETURNING *` reaches no projection allowlist.

**Inward - the raw column must be unaddressable on every inbound path.**
`build_field_condition_with_dialect` calls only `validate_field_name` with no
schema hint (`query.rs:5323`); the same hole is in
`build_conflict_probe_with_dialect` (`:2882`) and `build_write_target_probe`
(`:2850`). Aggregate `$match` is bare - `build_where(match_val, &mut params)`
with no `schema_hint` - while `$group.by` ten lines below does validate; the
asymmetry is inside one function. A `_raw` suffix reservation would have to land
in **two independent reservation tables with no dependency edge between them**
(`zeroship-schema/src/query.rs:754` and
`crates/zeroship-migrate-core/src/schema/query.rs:374`). The index records the
cheaper answer: `RESERVED_NAMES` already contains `ReservedName::Prefix("_")`
(`query.rs:742`) and `validate_field_name` already gates every inbound surface
(write-document keys including nested, `crud/write_pipeline.rs:44`, `:63`,
`:67`; filter keys; conflict-probe keys; read identifiers), so a raw column
named with a **leading underscore** is unrepresentable on every path a creator
can reach and the inbound half needs no new fence.

**Live-query subscriptions stop firing.** `normalise_filter` captures the
logical name (`crates/zeroship-plugin-db/src/read_set.rs:220`) and
`Conjunct::matches_text` compares it against the WAL tuple (`:119`). Post-flip
the tuple holds the mask under `ssn`, so `find({ssn: "123-45-6789"})` never
matches again. That module's own doc calls silently dropped events
"unacceptable" (`:218`).

**The dev tier loses all mask metadata.** The SQLite introspector requires
`sibling_name.strip_suffix("_masked")` to match before recording the entry, with
no `else` (`crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:2230`); the
malformed-sentinel arm warns, this one does not.

The write path is specified in full in
`docs/reviews/2026-08-28-flip-write-path.md`.

### What the flip owes (five items)

1. **Lookup by real value must survive.** "Find the account for this SSN" is the
   common case for a masked column, and after this change it cannot be expressed
   as a filter. The explicit API must therefore support **query by** plaintext,
   not only **read of** plaintext for a row already in hand. If it does not, the
   feature is closed rather than secured. This is the one way the decision can
   fail in practice and it must be designed before it is implemented.
2. **Uniqueness over the real value belongs on a keyed lookup column, not on the
   raw column.** A unique index or foreign key on a masked field is semantically
   about the real value, and left on `ssn` it would enforce uniqueness over
   masks, where many rows legitimately share `***-**-1234` - a silent
   data-integrity failure, not a visible error. But placing it on the raw column
   enforces **nothing** when the field is randomised-encrypted: `canonical_aad`
   binds the row PK, so identical plaintext produces different ciphertext in
   every row and every value is trivially unique. `sdks/db/src/types.ts:1146-1160`
   already refuses `.unique()` on a randomised-encrypted field for exactly this
   reason. Uniqueness over the real value can only be enforced on a keyed lookup
   column (`docs/reviews/2026-08-27-query-by-plaintext.md`), because equal
   plaintext produces an equal token by construction - which turns a
   currently-refused declaration into a supportable one.
3. **Equality search by real value changes behaviour.**
   `find({ssn: "123-45-6789"})` matches nothing after the flip. That is correct
   - plaintext should require an explicit, audited request - but it is a visible
   change and belongs in `docs/reference/db.md` beside the mask kinds, so a
   creator learns it when they declare the mask rather than when a query
   silently stops matching.
4. **The AAD binds the LOGICAL FIELD KEY, so the flip is a rename and not a
   re-encrypt.** `canonical_aad(collection, &col, ..)` receives `col` from
   `for (col, def) in schema_obj.iter()` (`crud/encryption_pass.rs:173`,
   `:200-207`), the schema field key - not `storage.rawColumn`. Same at `:337`
   and `crud/unmask.rs:452-458`. The two are the same string today for every
   masked+encrypted field, which is why the distinction is invisible in the
   current tree.

   **A doc comment asserts the opposite as fact**
   (`crates/zeroship-migrate-core/src/render/gen_types.rs:164-170`:
   "`canonical_aad` length-prefixes the column name ... an
   `ALTER TABLE ... RENAME COLUMN` leaves every stored cell authenticated under
   the old name"). **Correct that comment in the same commit as any flip work**,
   or the next reader "fixes" the AAD to match it and destroys every ciphertext
   in the deployment.

   Binding the bare logical name is not sufficient once item 2's keyed lookup
   column lands: two encrypted columns then share one logical field and one AAD,
   so swapping their contents passes tag verification. **Bind the logical field
   name plus a stable role discriminator (`value` | `lookup`).** A role survives
   renames; a physical name does not.
5. **The flip swaps which column carries the declared TYPE and the whole
   constraint set.** `.mask()` is legal on string, number and bytes, and every
   mask kind returns a **String**. Today that is harmless: the sibling is bare
   `TEXT` while the field's own column keeps its declared type and everything
   `def_to_constraints_for_dialect` attaches (`query.rs:2719`) - `NOT NULL`,
   `DEFAULT`, range `CHECK`, literal `CHECK`, enum `CHECK`. Post-flip the
   logical column holds `'***'`:

   - `t.number().mask(...)` leaves `ssn` as `DOUBLE PRECISION`; writing `'***'`
     is a hard error.
   - `t.string().enum([...]).mask(...)` leaves `CHECK ("ssn" IN (...))`, which
     refuses `'***'`. **Every write fails.**
   - encrypted+masked leaves `ssn` as `BYTEA`.

   For an EXISTING table a double rename is free, because types and constraints
   travel with the renamed columns:

   ```sql
   ALTER TABLE t RENAME COLUMN ssn        TO <raw>;  -- free the logical name first
   ALTER TABLE t RENAME COLUMN ssn_masked TO ssn;
   ```

   For a NEW table, `build_create_table_with_fks_for_dialect` (`query.rs:1131`)
   must emit the declared type and constraints under the **raw** name and a bare
   `TEXT` sibling under the **logical** name. Nothing does that today, and the
   differ (above) will not tell anyone.

## Joined reads

### A joined result MUST be nested before the read pipeline runs

This is a precondition of everything in the next section, and it is not a
presentation choice - a **flat** joined row cannot carry the identity the
pipeline needs. Three mechanisms are keyed on a single unqualified name per row.

**The sibling name.** The mask pass writes its sibling as
`format!("{col}_masked")` (`crates/zeroship-plugin-db/src/crud/mask_pass.rs:150`),
so two joined collections with a same-named masked column collide. The
descriptor now carries the physical layout including the sibling columns
(decision 7), so the runtime never re-derives a sibling name and a
descriptor-supplied mapping can name the two siblings distinctly. That removes
this collision by construction - and not the requirement, because the other two
mechanisms are untouched by where the sibling name comes from.

**The unmask handle.** It plucks one `row_pk` from `obj.get("id")`
(`mask_pass.rs:424`). On a flat joined row that `id` is the *parent's*, so a
later `unmask()` on a *child's* column would fetch **the wrong row's plaintext,
under the wrong collection's policy**. The handle would be well-formed and
wrong. The same code records what happens to a row lacking `id` - it gets the
empty string and `unmask()` rejects it - which is a second, independent reason
projection narrowing may never drop `id`.

**The AAD, where the requirement is cryptographic rather than conventional.**
`canonical_aad(collection, column, row_pk_bytes)`
(`crates/zeroship-plugin-db/src/encryption/aad.rs:75`) binds all three into the
AEAD additional data, so a child's ciphertext is decryptable **only** under the
child's own collection name and the child's own row primary key. Hand the
decrypt pass a flat joined row and it has the parent's collection and the
parent's `id`: the tag fails and the value is undecryptable. Not
mis-authorized - unreadable. The mask and unmask problems above are failures of
a convention that could in principle be patched at the call site; this one
cannot be patched at all, because the binding is inside the authentication tag.

**And a flat row defeats DB-7 from the inside**, which is why this cannot be
left to reviewer vigilance. The mask sentinel carries `_sig`, an unforgeable
per-process signature, and the code says what it is for: only sentinels the read
pipeline itself produced carry it, so app JS cannot fabricate a `__zsmask__`
object pointing at an attacker-chosen `(collection, row, column)`
(`mask_pass.rs:506`, `:521-525`). On a flat joined row the pipeline **itself**
stamps `_meta` with the *parent's* `(collection, row_pk)` for a *child's*
column. That sentinel is not forged - it is genuinely produced by the trusted
path, so it carries a **valid** signature - and it points at the wrong row in
the wrong collection. `_sig` authenticates *provenance*, and the provenance is
real; the defence is simply not aimed at a trusted producer emitting a wrong
`_meta`.

So: **the join result is nested before decrypt, mask and normalisation run, and
those passes recurse per nested object with its own collection context.** A
column aliased into a flat row skips the passes keyed on its real name entirely
- which for a mask-only child column means it is returned as **plaintext**,
below the policy layer this document otherwise governs, with no denial and no
audit row. That is a leak by *schema scoping*, not by authorization.

### Policy is resolved per source collection

SC-3 makes relations first-class, so a result row can carry columns from several
collections at once. **Mask policy and unmask authorization are per collection**,
and a single lookup keyed on the operation's target would apply the *parent's*
policy to the *child's* columns. If the child's policy is stricter, its
protected columns are released under the parent's looser rule - a disclosure,
with no error and no audit row saying anything unusual happened.

Over-masking, the other direction, is not cosmetic either:

- **it can break the join itself.** The stitch matches on a foreign key; a
  masked join key is no longer the value it must match on, so the relation
  either errors or silently nests `null` - and a nested `null` is already
  defined to mean *the target row is missing or deleted*
  (`docs/reference/db.md:585`). A masking mistake would present as a data
  statement about the tenant's rows.
- **it corrupts the unmask handle** rather than just the display value: the
  `_meta` sentinel is a *capability to fetch plaintext later*, not a rendering.
- **it silently changes row order, and therefore breaks cursor pagination.**
  `aggregate_read_ident` substitutes `"<field>_masked"` for a masked column
  (`crates/zeroship-schema/src/query.rs:3431`), so a column used as a sort or
  group key orders by the **masked string** rather than the value. A cursor
  minted under one ordering does not resume correctly under another, so the
  damage outlives the request that caused it.

Both directions fail, in different ways, and neither is loud. The rule is
symmetric: **each projected column is authorized against the policy of the
collection it came from**, the classification is the one declared on that
collection, and the audit row records the source collection alongside the field.
A test whose parent and child share a policy passes on the broken
implementation, so the arms below require policies that **differ on the same
classification**.

## Acceptance shape

**Every arm whose expected outcome is a denial carries a granted-path control.**
A fail-closed default hides a total functional break behind a green test: a
mechanism that returned `permission denied` for every in-transaction unmask
would satisfy a deny-only arm **vacuously**, with every non-`auto` unmask in
every transaction bricked. This discipline applies to every arm below.

- **A binding constructed under a lowered ceiling denies `unmask` by an actor
  the effective ceiling governs, and the same test pairs it with a
  classification the same ceiling still permits, which must SUCCEED.** A ceiling
  that denied everything would otherwise satisfy the arm perfectly, and so would
  an implementation whose meet is broken in the direction of denying.
- **A ceiling that revokes `auto` denies `auto`, when the creator draft does not
  mention `auto` at all - paired with a role the ceiling does not name, which
  keeps its lattice default.** This is the sharpest arm in the document: the
  naive implementation (a map intersection over shared keys) **inverts** the
  revocation for exactly the actor with the most access, and passes every other
  arm here.
- **An absent or unresolvable ceiling denies, and it is refused at composition
  rather than at the first unmask - paired, in the same test, with a configured
  permissive ceiling that grants**, so the arm cannot go green on an
  implementation where every composition fails.
- **The effective policy is identical inside and outside an explicit
  transaction**, asserted with the same classification unmasked both ways in one
  test. This is true by construction, which is exactly why it is worth
  asserting: a regression that reintroduced a per-authorization lookup would
  show up here and nowhere else.
- **A mask-only column on a joined child is masked in the result**, and the same
  test asserts the parent's own mask-only column is masked too - a nesting bug
  that drops the child's masking entirely would otherwise show only as a missing
  sibling nobody asserted.
- **`unmask()` invoked on a child column fetches that child's row under that
  child's policy**, asserted by giving parent and child rows distinct plaintext;
  identical fixtures cannot distinguish a correct handle from one pointing at
  the parent.
- **The audit row for a joined unmask names the source collection**, not the
  operation's target collection.
- **A joined row's mask policy is resolved per source collection**, asserted
  with a parent and child whose policies **differ on the same classification** -
  the case a single per-result lookup gets wrong while every same-policy fixture
  passes. SC-3 carries the pointer and says in its own words that this "belongs
  to SC-6's contract rather than to this document's grammar"; the arm is this
  contract's to state.

**Owed: the storage flip has no arms in this list.** Every arm above measures
the ceiling contract. The flip changes the physical layout, and each of its five
owed items is testable and untested here: that plaintext remains **queryable by
real value** through the explicit API; that uniqueness over the real value lands
on the keyed lookup column rather than enforcing uniqueness over masks; that
equality search by real value no longer matches, as a stated behaviour change
rather than a silent one; that the AAD's role discriminator survives the rename;
and that the declared type and constraint set travel to the raw column, on both
an existing and a new table. Items 2, 4 and 5 are migration-engine changes, so
they cannot ride a runtime step and cannot be asserted by a runtime arm - which
is a reason to name them here, not a reason to leave them out. The flip's
migration additionally owes a **mutation-proved** test, because the differ emits
nothing for it.
