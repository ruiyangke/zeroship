# SC-6: the mask-ceiling read contract

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** the mask-policy section of that document, and its acceptance
criterion that lowering the operator ceiling denies the next `unmask` in an
already-built pinned isolate with no rebuild and no deploy.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## Why this exists

Both round-3 reviewers reached the same conclusion from different directions,
which is why this is the one sub-contract added after the fact rather than with
the original five:

- one found there is **no transport**: `zeroship-plugin-db` has no HTTP client,
  and `check_unmask_authorization` is a synchronous `fn`
  (`crates/zeroship-plugin-db/src/crud/unmask.rs:305`);
- the other found there is **no linearization point**: a cache keyed by
  `(app_id, ceiling_version)` cannot discover a newly committed version by
  itself, so revocation would silently never arrive.

The second is the deeper one. Caching by a version you can only learn by reading
is circular, and an earlier draft of the parent proposal shipped exactly that.

## The shape of today's decision

```rust
pub(crate) fn check_unmask_authorization(
    app_id: &str,
    actor: &Option<Value>,
    classification: &str,
) -> Result<bool, DbError>
```

It is **synchronous**, and it reads an in-memory thread-local:
`crate::context::with(|c| c.mask_policy_for(app_id))` (`unmask.rs:315`). Three
production call sites (`unmask.rs:405`, `:1069`, `:1269`).

Its fallback is already deny-shaped: with no policy, only `kind == "auto"` is
allowed (`unmask.rs:318-321`).

## The contract

**The ceiling arrives as a value, not as an I/O call.** It is read in the
operation's own `prepare` batch, beside the epoch, from
`__zeroship_admin` - the same statement group the parent proposal already
establishes - and carried in the resolved operation context.
`check_unmask_authorization` gains the effective ceiling as a parameter and
**stays synchronous**.

That placement is the whole contract, and it is chosen for three reasons:

1. **It costs no extra round trip - but only after a reordering this contract
   must name rather than assume.** Today the authorization decision happens
   *before any SQL at all*: the doc on the unmask hint path says it is "Called
   from `crud::dispatch_find` **BEFORE** `build_find_with_schema` fires the SQL"
   (`crates/zeroship-plugin-db/src/crud/unmask.rs:1235-1236`). So at today's
   authorization point there is no batch to ride, and an earlier draft of this
   document had the ordering backwards when it said the read "rides a batch that
   already exists".

   Under the parent proposal's pipeline the operation begins with `prepare`
   (route, lease, epoch) and *then* resolves metadata and builds SQL, so the
   ceiling read does land before authorization - but that is a **consequence of
   moving the authorization point after `prepare`**, which is work, not a free
   rider. This contract requires that move explicitly. Without it, the ceiling
   read is a new round trip and the cost claim is false.
2. **It has a real linearization point**: the operation's own transaction. The
   value used to authorize is the value committed before that transaction began,
   with no cache to go stale and no invalidation message to lose.
3. **It does not make the authorization path async.** Making
   `check_unmask_authorization` async to fetch a ceiling would put an I/O call
   inside a decision that must be uniform and cheap, and would spread `.await`
   through three call sites for a value the operation could have carried.

### Explicit-transaction placement

The ceiling is read **per authorization**, not pinned for the transaction. The
parent proposal states the rule as "schema is pinned, authority is not": pinning
the ceiling would let a long transaction hold a permissive value across a
revocation, reintroducing non-revocability inside the transaction.

**It is NOT read on the transaction's own connection, and an earlier draft that
said so contradicted this document's own security arm.** Both round-6 reviewers
found this independently, by different routes, and it is verified here:
`apply_per_app_role` issues `SET LOCAL ROLE` together with the DB-1 guards and
notes in its own comment that they are "all `SET LOCAL`, so they revert at the
tx end" (`crates/zeroship-plugin-db/src/transaction/mod.rs:202-217`); it is
called immediately after the top-level `BEGIN`
(`transaction/mod.rs:540`). Every read on that connection for the rest of the
transaction therefore executes **as the per-app role** - and the privilege
posture below requires that role to hold *no* privilege on the ceiling table.
The read would return `permission denied`.

Two things about that are worth stating plainly, because they generalise:

- It is **isolation-independent**. The pinned-snapshot problem that opened this
  question is real but was the smaller half; this one breaks the mechanism at
  `READ COMMITTED` too, and would have broken it on a tier where every
  transaction defaulted to the weakest level.
- **"Failure is denial" concealed it.** The acceptance arm asserts a deny, and a
  `permission denied` produces exactly that - so the arm would have passed
  **vacuously** while every non-`auto` unmask inside every transaction was
  bricked. A fail-closed default hid a total functional break behind a green
  test. Any arm whose expected outcome is a denial must therefore also prove the
  *granted* path still works, or it is not measuring what it claims.

**The rule this establishes: an authority read never traverses the data
snapshot, and never runs under the tenant's own role.** Concretely:

- **autocommit** rides the `prepare` batch, which the parent already orders
  before `SET LOCAL ROLE` precisely so that "no per-app role needs any grant on
  `__zeroship_*`" (parent, "Operation order"). This is the free case;
- **inside an explicit transaction** the ceiling is read on a **separate
  platform-role session** - `op_conn` on SQLite, which is the second connection
  SC-2's Decision 1 already establishes - and the value read may only ever
  **tighten** the value captured at `BEGIN`. Tightening is fail-closed and
  therefore always safe to apply; a *raised* ceiling is never honoured
  mid-transaction, which also keeps the rule in the acceptance list below
  ("raising the ceiling does not retroactively authorize") true by construction.

### The authority session comes from its own pool, or Fork B deadlocks

"A separate platform-role session" must not mean "another checkout from the data
pool". The data pool holds **eight** connections
(`Pool::connect(&url, 8)`, `crates/zeroship-plugin-db/src/lib.rs:862`), and an
in-transaction authority read is issued *while the caller already holds one*.
Eight concurrent transactions for eight different apps therefore hold all eight
connections and then each wait for a ninth - a deadlock that no
single-transaction acceptance arm can expose, and which arrives under exactly
the load the feature is for.

**Authority reads come from a small, dedicated authority pool**, disjoint from
the data pool. That removes the cycle rather than making it less likely: an
authority read waits only on other authority reads, and those complete without
ever needing a data connection, so the wait-for graph has no cycle regardless of
how many transactions are in flight. Sizing follows from the reads being a
single short `SELECT` - the pool is small, and its saturation degrades latency
rather than progress.

It is also forced by the role model, independently of capacity: a data
connection inside a creator transaction is running under `SET LOCAL ROLE` for
that transaction's life, so it is *the wrong role* to read the ceiling with. The
two arguments meet at the same answer, which is why this is a separate pool and
not a reservation carved out of the existing one.

A separate session is additionally the right answer for a reason that has
nothing to do with privileges: reading the ceiling **inside** a `SERIALIZABLE`
app transaction enrols the ceiling row in that transaction's SSI predicate
locks, so a single operator revocation becomes a platform-wide `40001` abort
amplifier across every app transaction that read it.

### The ceiling table's privilege posture

The ceiling row is **platform-owned and tenant-unreadable-and-unwritable**, on
exactly the terms the parent proposal establishes for `app_schema_state`, and
it gets **its own** privilege arm rather than borrowing that one.

Stating this separately is not redundancy. The parent's arm names
`app_schema_state`; a tenant holding `UPDATE` on the *ceiling* table
self-authorizes `unmask` while every arm in this document and that one stays
green. That is the same failure shape the parent already had to fix once - a
gate bound to the wrong object - reappearing one table over, which is precisely
how it would be missed.

The arm therefore asserts, for the ceiling table:

- no app-role template and no per-app role holds any privilege on it, tested at
  **column granularity** (`has_any_column_privilege` as well as
  `has_table_privilege`) and over the **full** privilege list, because a
  column-level `GRANT UPDATE (ceiling)` returns `has_table_privilege = f` while
  granting the write - measured on PG 16.14, and column-level grants are house
  style in this tree;
- roles enumerated from `pg_roles`, so the property holds for roles that do not
  exist yet;
- and the **positive control**: the pool's login role *can* read it, since an
  absence-only assertion passes just as happily on a table nobody can read at
  all.

### Where the ceiling lives on SQLite

`__zeroship_admin` is a **PostgreSQL** schema. The parent proposal deletes the
SQLite sidecar policy store, so an earlier draft of this document left the dev
tier with no ceiling home at all - and combined with "failure is denial" below,
that would have denied **every non-`auto` unmask in dev, permanently**. A
contract that bricks the dev tier is not a security win.

The dev tier's answer mirrors SC-2's for the epoch, and for the same reason:
there is no second, platform-owned database on that tier, so the app file is the
whole world. The ceiling is a row in the app file, written only by the dev
migration path, and read by the reservation that authorizes - the same statement
group SC-2 already establishes for `__zeroship_state`.

What is deliberately **not** claimed for dev is the tenant-unwritable property.
On PostgreSQL the ceiling is protected by grants; in a local SQLite file the
developer owns the bytes and no grant model can change that. Dev's guarantee is
contract parity - the same decisions, the same denials - not the same
adversarial posture, which is exactly the split
`docs/reference/auth-dev-tier.md` already draws for the dev auth provider.
Stating that plainly is better than implying a protection the tier cannot give.

### Failure is denial

An unresolvable or absent ceiling **denies**. This tightens today's fallback:
currently a missing policy still permits `kind == "auto"` (`unmask.rs:318-321`),
which is defensible when the policy is app-declared convenience and is not
defensible once the ceiling is the operator's revocation mechanism. A ceiling
that cannot be read is not an empty ceiling.

## The ceiling write path

This contract had left the write path as an outcome with no mechanism, which is
why it is stated here at the same level as the read contract rather than inside
it.

Every acceptance arm in this document begins "lowering the operator ceiling",
and until now **nothing in any of the seven documents said how a ceiling is
written**. The read path was specified in detail while the thing it reads had no
table shape, no writer, no API and no place in the parent's step sequence. An
arm whose precondition cannot be performed is not testable, so this is a
prerequisite rather than a detail:

- **Where.** A platform-owned table in `__zeroship_admin`, on the same privilege
  terms as the epoch row and with its own privilege arm ("The ceiling table's
  privilege posture", above). Not the app schema, and not `app_schema_state` -
  it is separately grantable and separately revocable on purpose.
- **Who.** The **control plane**, through a privileged server-side function, on
  the same rule the epoch already follows: the function is the only writer, and
  the caller does not choose the value's identity. The worker and the runtime
  never write it.
- **How it is ordered.** A ceiling write is a CAS against the value the operator
  believed current, so two concurrent operator actions cannot silently
  interleave, and a ceiling **version** advances monotonically with each write.
  The version is what an audit row cites; it is not what a reader keys a cache
  by (that circularity is the mistake this whole contract was written to undo).
- **What a lowering guarantees.** Once the write commits, the *next* authority
  read by any operation observes it - which is exactly why the read must not
  ride the operation's own data snapshot. Nothing needs to be pushed, and no
  invalidation message needs to arrive.

Until this lands, every arm below is blocked on it, and none of them should be
reported green by exercising a hand-written row.

## Masking is projection-shaped: the filter oracle and the storage flip

The mask substitution lives in the **select list**: a masked column is read as
`"<col>_masked" AS "<col>"`, and `aggregate_read_ident` extends the same
treatment to aggregates, `$group.by`, `$having` and aggregate `$sort`. Nothing
else is mask-aware, and the `WHERE` builder **cannot be**: its signature is
`build_where_with_dialect(filter, params, dialect)`
(`crates/zeroship-schema/src/query.rs:5274-5281`) - there is no schema hint
parameter at all, so it has no way to know a column is masked. The projection
path takes one; this path never did.

A masked column stores **plaintext in the parent column** (the masked form lives
in the sibling). So `find({ ssn: { $gt: "500-00-0000" } })` renders
`WHERE "ssn" > $1` and compares against plaintext. The caller never sees an
unmasked value in the result - and does not need to: **the set of matching rows
is the answer.** Repeated queries binary-search the exact value, with no
authorization check anywhere on the path and no audit row written.

**One precision, because the distinction matters for what to fix.** This does
not violate the letter of the audit guarantee: `docs/reference/db.md` promises
`__zeroship_audit_unmask` records "every `.unmask()` call (granted or denied)",
and a filter comparison is not an `unmask()` call. The guarantee is true and
narrow. What fails is the **protection goal** - plaintext is not disclosed
without authorization - through a channel the guarantee was never scoped to
cover. A defence that is honest about its own scope can still leave the asset
undefended, and reading the audit table would show nothing unusual while it
happened.

### The three options this decision rejected (SUPERSEDED)

**This was a policy question the design set never asked, and it needed an
explicit answer rather than an implementation detail.** It now has one, in the
next subsection, and these three options are what that decision rejects - kept
in place because the decision is stated as a rejection of them and reads as
arbitrary without them. They were not equivalent to each other either:

- **Refuse ordered comparison** (`$gt`/`$gte`/`$lt`/`$lte`, ranges) on masked
  columns with a typed error, and allow equality. This kills the
  binary-search channel, which is the one that recovers a full value cheaply.
  Equality remains a guess-and-confirm oracle, which is far weaker but not
  nothing.
- **Refuse all filtering** on masked columns. Complete, and probably
  unusable: filtering by a masked email is an ordinary thing to want.
- **Allow, but audit.** Turns a silent channel into a recorded one without
  closing it, and puts a write on every filtered read.

### DECIDED: flip the storage

**DECIDED (operator, 2026-08-27): none of the three. Flip the storage, and make
the plaintext column unqueryable.**

The three options above all accept the current physical layout - `ssn` holds
plaintext, `ssn_masked` holds the mask, and the read path substitutes
`"ssn_masked" AS "ssn"` - and then try to police the filter path on top of it.
The decision rejects that framing:

- **`ssn` stores the MASKED value. `ssn_raw` stores the real one.**
- **`ssn_raw` is reserved and CANNOT be queried** - not in a filter, not in a
  projection, not in a sort. It is not a field of the generated type.
- **Plaintext is reachable only through an explicit API**, which is the place
  authorization and the audit row already live.

### Why the flip beats the three options above

**Why this is better than the three rejected options, and not merely
different.** Every one of them leaves the design *fail-open*: the plaintext
sits in the column with the natural name, so any code path that forgets to ask
"is this masked?" reads it. That is not a hypothetical - it is exactly how this
defect arose, because `build_where_with_dialect` takes no schema hint while the
read path takes one and calls `column_is_masked(field, schema_hint)`
(`zeroship-schema/src/query.rs:3351`). The asymmetry between two code paths, one
schema-aware and one not, IS the bug.

After the flip the ignorant path is the safe path. A query that knows nothing
about masking selects and filters the masked column and leaks nothing. The
`"ssn_masked" AS "ssn"` substitution disappears entirely, so there is one less
place to get wrong rather than one more.

It also makes the guard cheap. Refusing a filter today requires the builder to
consult schema metadata it does not have; refusing a **reserved suffix** requires
no schema at all. There is precedent: `_masked` is already a reserved suffix
creators cannot use (`ReservedName::Suffix("_masked")`, `query.rs:752`), so
`_raw` joins an existing mechanism instead of inventing one.

And it converts the audit guarantee from narrow-but-true to simply true. The
guarantee reads "unmask calls are audited"; the filter channel defeated the
protection goal without touching an unmask call. When plaintext has exactly one
reader, the guarantee covers the asset rather than one path to it.

### What this decision now owes (four items)

1. **Lookup by real value must survive.** "Find the account for this SSN" is the
   common case for a masked column, and after this change it cannot be expressed
   as a filter. The explicit API must therefore support **query by** plaintext,
   not only **read of** plaintext for a row already in hand. If it does not, the
   feature is closed rather than secured. This is the one way the decision can
   fail in practice and it should be designed before it is implemented.
2. **Constraints and indexes follow the RAW column.** A unique index or foreign
   key on a masked field is semantically about the real value; left on `ssn` it
   would enforce uniqueness over masks, and many rows legitimately share
   `***-**-1234`. That is a silent data-integrity failure, not a visible error.
   The migration engine must place them on `ssn_raw` - a column the query
   surface cannot see but the schema still constrains.
3. **Equality search by real value changes behaviour.** `find({ssn: "123-45-6789"})`
   matches nothing after the flip. That is correct - plaintext should require an
   explicit, audited request - but it is a visible change and belongs in
   `docs/reference/db.md` beside the mask kinds, so a creator learns it when
   they declare the mask rather than when a query silently stops matching.
4. **The AAD binds the column name.** `canonical_aad(collection, column,
   row_pk)` means renaming the physical column changes the tag, so this is a
   migration-engine change and not only a runtime one. Free pre-launch; not free
   later.

## Joined reads

### A joined result MUST be nested before the read pipeline runs

This is a precondition of everything in the next section, and it is not a
presentation choice - a **flat** joined row cannot carry the identity the
pipeline needs.

Two mechanisms fix that, both keyed on a single unqualified name per row:

- the mask pass writes its sibling as `format!("{col}_masked")`
  (`crates/zeroship-plugin-db/src/crud/mask_pass.rs:150`), so two joined
  collections with a same-named masked column collide;
- the unmask **handle** plucks one `row_pk` from `obj.get("id")`
  (`mask_pass.rs:419-431`). On a flat joined row that `id` is the *parent's*, so
  a later `unmask()` on a *child's* column would fetch **the wrong row's
  plaintext, under the wrong collection's policy**. The handle would be
  well-formed and wrong.

The same code also records what happens to a row lacking `id` - "rows that came
back via a projection without `id` get the empty string" and `unmask()` rejects
them - which is a second, independent reason projection narrowing may never drop
`id`.

**And for encrypted columns the requirement is cryptographic, not conventional.**
`canonical_aad(collection, column, row_pk_bytes)`
(`crates/zeroship-plugin-db/src/encryption/aad.rs:75-79`) binds all three into
the AEAD additional data. A child's ciphertext is therefore decryptable **only**
under the child's own collection name and the child's own row primary key. Hand
the decrypt pass a flat joined row and it has the parent's collection and the
parent's `id`: the tag fails and the value is undecryptable. Not
mis-authorized - unreadable.

That is the strongest form this argument takes. The mask and unmask problems
above are failures of a convention that could, in principle, be patched at the
call site; this one cannot be patched at all, because the binding is inside the
authentication tag. Any join design that flattens rows before the read pipeline
is not merely risky, it **cannot decrypt encrypted child columns**, and no
amount of care downstream recovers that.

**And a flat row defeats DB-7 from the inside, which is the reason this cannot
be left to reviewer vigilance.** The mask sentinel carries `_sig`, an
unforgeable per-process signature, and the code says exactly what it is for:

> Only sentinels the read pipeline itself produced carry it; the decoder refuses
> to mint a `MaskedValue` from any sentinel lacking it, so app JS cannot
> fabricate a `__zsmask__` object ... and have it minted into a `MaskedValue`
> pointing at an attacker-chosen `(collection, row, column)`.

(`crates/zeroship-plugin-db/src/crud/mask_pass.rs:497-512`.)

On a flat joined row the pipeline **itself** stamps `_meta` with the *parent's*
`(collection, row_pk)` for a *child's* column. That sentinel is not forged - it
is genuinely produced by the trusted path, so it carries a **valid** signature -
and it points at the wrong row in the wrong collection. The platform mints a
correctly-signed capability to fetch the wrong plaintext.

`_sig` cannot help here, and its inability is structural rather than a gap: it
authenticates *provenance*, and the provenance is real. The defence DB-7 builds
against forgery is simply not aimed at a trusted producer emitting a wrong
`_meta`. Nesting is what keeps the producer correct, which is why it is a
precondition rather than a preference.

So: **the join result is nested before decrypt, mask and normalisation run, and
those passes recurse per nested object with its own collection context.** A
column aliased into a flat row skips the passes keyed on its real name entirely -
which for a mask-only child column means it is returned as **plaintext**, below
the policy layer this document otherwise governs, with no denial and no audit
row. That is a leak by *schema scoping*, not by authorization, and no arm in
this document would have caught it.

### Joined rows: policy is resolved per source collection

SC-3 makes relations first-class, so a result row can carry columns from several
collections at once. **Mask policy and unmask authorization are per collection**,
and a single lookup keyed on the operation's target would apply the *parent's*
policy to the *child's* columns.

If the child's policy is stricter, its protected columns are released under the
parent's looser rule - a disclosure, with no error and no audit row saying
anything unusual happened.

**An earlier draft called the other direction "merely annoying". That was
wrong**, and it is worth correcting rather than softening, because it was an
argument for caring less about one half of a security property. Over-masking is
not cosmetic here:

- **it can break the join itself.** The stitch matches on a foreign key; a
  masked join key is no longer the value it must match on, so the relation
  either errors or silently nests `null` - and a nested `null` is already
  defined to mean *the target row is missing or deleted*
  (`docs/reference/db.md:585`). A masking mistake would present as a data
  statement about the tenant's rows.
- **it corrupts the unmask handle** rather than just the display value, for the
  reasons in the previous section: the `_meta` sentinel is a *capability to
  fetch plaintext later*, not a rendering. Minting it against the wrong
  collection points a subsequent `unmask()` at another row under another policy.
- **it silently changes row order, and therefore breaks cursor pagination.**
  `aggregate_read_ident` substitutes `"<field>_masked"` for a masked column
  (`crates/zeroship-schema/src/query.rs:3412-3418`), so a column used as a sort
  or group key orders by the **masked string** rather than the value. A column
  masked that should not have been reorders the result set, and a cursor minted
  under one ordering does not resume correctly under another - so the damage
  outlives the request that caused it.

So both directions fail, in different ways, and neither is loud. The rule is
symmetric: **each projected column is authorized against the policy of the
collection it came from**, the classification is the one declared on that
collection, and the audit row records the source collection alongside the field.
A test whose parent and child share a policy passes on the broken
implementation, so the arm this contract carries requires policies that
**differ on the same classification**.

**This section previously carried no acceptance arms of its own**, which is how
a security rule ends up unenforced while reading as settled. It now owns three:

- a mask-only column on a **joined child** is masked in the result, and the
  same test asserts the parent's own mask-only column is masked too - a
  nesting bug that drops the child's masking entirely would otherwise show only
  as a missing sibling nobody asserted;
- `unmask()` invoked on a **child** column fetches that child's row under that
  child's policy, asserted by giving parent and child rows distinct plaintext -
  identical fixtures cannot distinguish a correct handle from one pointing at
  the parent;
- the audit row for a joined unmask names the **source collection**, not the
  operation's target collection. The document claims this; nothing tested it.

## Relationship to the migration ceiling - shape only

The platform already has operator-ceiling machinery with meet-semantics
(`crates/zeroship-migrate-server/src/policy.rs`, `policies/confined.policy.toml`).
Reuse its **shape** - a versioned operator ceiling intersected with a
creator-supplied value - and **not** its store or its staleness rule:

- that ceiling is **DDL-knobs-only**: its keys are `CREATE TABLE` /
  `CREATE SCHEMA` / `RENAME` / destructive-ops / RLS (`policy.rs:32-35`), with no
  vocabulary for mask classifications;
- its staleness answer is `ApprovalStaleCeiling` -> **"re-submit required"**
  (`crates/zeroship-migrate-server/src/apply.rs:192-199`), which is right for a
  migration awaiting approval and is the **exact opposite** of what revocation
  needs here.

Separate table, separate keys, deny-now rather than re-submit. Stating that
explicitly is the point of this section: the two ceilings will otherwise be
conflated by whoever implements second.

## What the creator half contributes

`manifest_declared` is **untrusted, deploy-scoped data**. The `.zship` manifest
carries no signature (`crates/zeroship-bundle/src/manifest.rs`; `deploy_hash` is
a digest the control plane computes on receipt, `:44-48`), so a build-graph
dependency can author it. Its only security property is that it cannot outlive
its deploy - which is the entire reason it moved out of the durable store.

### The creator half has no wire, and the API it replaces is being deleted

`manifest_declared` is named throughout this document as one of the two inputs
to the effective policy. **No document defines it as an artifact** - not the
manifest field, not its schema, not the authoring API, not validation, not the
build step that emits it.

That would be a gap in any case; it is a blocker because the parent proposal
simultaneously **deletes the only creator-facing way to declare a mask policy**.
`defineMaskPolicy` (`sdks/db/src/policy.ts:118`, reached through the bootstrap's
dev and runtime entries) goes away, and there is no latent route waiting to
replace it: the string `mask` appears **zero** times in the Rust manifest
(`crates/zeroship-bundle/src/manifest.rs`) and **zero** times in the TypeScript
manifest shape (`sdks/vite-plugin/src/zship.ts`).

The consequence is not a missing convenience. With no declared policy,
`mask_policy_for` yields nothing and the authorization fallback admits only
`auto` - so **every creator role is denied every unmask**, and the masking
feature is inert for its actual users while appearing to be configured. It fails
closed, which is the right direction and the wrong outcome.

So the replacement wire is a **prerequisite of the deletion**, in the same step,
and it owes five things: the manifest field and its schema, the authoring
surface that produces it, validation at build time, the emission path through
the packer, and the runtime read that turns it into a `MaskPolicy`. Deleting the
old API before those exist leaves no way to express a policy at all.

### The intersection is not a map intersection, and getting that wrong inverts revocation

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
(`crates/zeroship-plugin-db/src/crud/mask_policy.rs:102-110`. The inline comment
is this document's gloss, not the source bytes: the tree's comment on that arm
reads "The `auto` fallback rule - see method doc-comment.")

So an absent key is the **top** element for `auto` and the **bottom** element for
every other role. A naive intersection - keep only keys present in both sides -
therefore drops any key the manifest does not mention. Follow that with an
operator ceiling written specifically to **revoke** `auto` (listing it with an
empty set) against a manifest that never mentions `auto`: the key disappears from
the result, the fallback fires, and `auto` is restored to **every**
classification. The revocation does not merely fail - it **inverts**, and it does
so for the one actor with the most access.

The rule, therefore: **both policies are normalised to total functions before the
meet.** `auto`'s implicit top is materialised as an explicit entry, every other
absent role is materialised as an explicit bottom, and the meet is then pointwise
over the union of roles. A ceiling can consequently narrow `auto`, which the
section above establishes it must be able to do.

An acceptance arm follows directly, and it is worth stating because the naive
implementation passes every other arm in this document: **a ceiling that revokes
`auto` denies `auto`, when the manifest does not mention `auto` at all.**

Effective policy is `operator_ceiling` met with `manifest_declared` over the
role-complete domain above. **The ceiling can always narrow, `auto` included,
and there is no exception.**

`auto`'s apparent privilege is a *lattice default*, not an exemption: an absent
key is the top element for `auto` and the bottom element for every other role
(`crates/zeroship-plugin-db/src/crud/mask_policy.rs:86-110`). Materialise that
default, as the meet above requires, and `auto` is an ordinary role - a ceiling
that lists it narrows it, and the method's own doc says so in the imperative:
"to restrict the system actor, the policy MUST list `auto`". The shipped
reference agrees (`docs/reference/db.md:1584-1586`).

The one property that is genuinely special: `auto` is **not forgeable from app
JS**, because `sanitize_app_actor` strips an app-supplied `kind == "auto"`
before it reaches any of the three authorization entries
(`crates/zeroship-plugin-db/src/crud/unmask.rs:280-303`).

*Three earlier drafts of this section argued the opposite in three different
ways, and each round I qualified the stale prose instead of removing it - which
is how a document ends up holding claims that cannot all be true at once. The
whole argument is now stated once, above. Annotating a wrong claim leaves it in
the document; deleting it is the correction.*

*One of those drafts is worth naming anyway, and naming it is not the same as
keeping it: the claim is quoted as a warning, with its correction in the same
breath, rather than left standing to be read as the rule. A flat "the ceiling
can only ever narrow" was **false as written**. Under a naive map intersection a
ceiling that revokes `auto` does not narrow it, it **inverts** the revocation,
as the paragraphs above measure; and a role the ceiling does not name is not
narrowed at all, it keeps its lattice default - which is the scope the
acceptance arm below states in its own words. What is true is the corrected
form: the ceiling **can** always narrow, `auto` included, once both policies are
normalised to total functions. A reader relying on the flat version to reason
about revocation would reach the wrong conclusion for exactly the actor with the
most access.*

### The ceiling, not the actor, is the security boundary

This must be stated rather than relied on implicitly, because the actor is
**self-asserted by app JS**. `RESERVED_SYSTEM_ACTOR_KINDS` is exactly `["auto"]`
(`crates/zeroship-plugin-db/src/crud/unmask.rs:280`), so `sanitize_app_actor`
strips the platform actor and *nothing else*. An app handler may therefore
present `{ actor: { kind: "admin" } }` and receive whatever the app's own policy
grants `admin`; no part of the path binds the actor to `env.auth`.

That is coherent while the mask policy is app-declared convenience - the app is
authorizing itself against its own declaration. It stops being coherent the
moment the operator ceiling becomes a revocation mechanism, because an operator
revoking access must not be defeated by the tenant naming a different role.

The property that carries the weight is therefore the **intersection**:
effective policy is `operator_ceiling` meet `manifest_declared`, so a
self-asserted role can never exceed the ceiling. Its integrity depends on one
thing this contract must not lose - **the ceiling is resolved independently of
the app-supplied actor**, never fetched through a map entry the actor names.
Consult the ceiling through the actor's own key and the tenant regains control
of its own limit by choosing a role the ceiling does not mention.

## Acceptance shape

- **The criterion this exists for:** lowering the operator ceiling denies the
  next `unmask` **by a creator actor** in an **already-built pinned isolate**,
  with no rebuild and no deploy - and the test asserts the deny arrives
  **without any invalidation message being delivered**, because correctness must
  not depend on hint delivery. **The same test pairs it with a classification
  the lowered ceiling still permits, which must still succeed in that same
  pinned isolate.** A lowering that denied everything would otherwise satisfy
  the arm perfectly.

  The qualification is about **which actors the ceiling governs**, and it must
  be read against the corrected account above, not the superseded one. `auto` is
  not exempt from the ceiling: it is granted everything only as a **fallback**,
  when the policy does not list it
  (`crates/zeroship-plugin-db/src/crud/mask_policy.rs:85-110`). A ceiling that
  lists `auto` narrows it like any other role, and lowering such a ceiling
  denies `auto` too.

  So the arm is scoped to **an actor the effective ceiling actually governs** -
  which is every creator actor, and is `auto` as well whenever the ceiling names
  it. An unqualified "denies the next `unmask`" would be false only in the
  fallback case.

  The scope is a property of the lattice, not an exemption: a role the ceiling
  does not name keeps its default, and for `auto` that default is the top
  element.
- A ceiling that cannot be read denies, and the denial is distinguishable in the
  audit row from a ceiling that was read and said no - **paired, in the same
  test, with a readable permissive ceiling that grants**, so the arm cannot go
  green on an implementation where every read fails.
- Raising the ceiling does **not** retroactively authorize an in-flight
  operation that already read the lower value - **paired with a fresh operation
  started after the raise, which succeeds.** Without that pair the arm is
  satisfied by an implementation that never authorizes anything.
- Inside one explicit transaction, a ceiling lowered mid-transaction denies the
  next `unmask` **by a creator actor** in that same transaction - **and the same
  test asserts that an unmask which the ceiling still permits SUCCEEDS in that
  transaction.**

  The second half is not padding, it is the whole reason this arm is
  trustworthy. An earlier draft required only the deny, and the mechanism it
  specified (reading on the transaction's own connection, under the per-app
  role) would have produced `permission denied` for *every* in-transaction
  unmask. Combined with "failure is denial" the deny-only arm passes on a
  completely broken implementation. Every arm in this document whose expected
  outcome is a denial carries a granted-path control for that reason.

  This arm is also isolation-parameterised: it must pass at `READ COMMITTED`
  **and** at `REPEATABLE READ`/`SERIALIZABLE`, which the creator-facing contract
  really offers (`docs/reference/db.md:999`;
  `crates/zeroship-plugin-db/src/transaction/mod.rs:117-118,1186-1187` emits
  them). The separate platform-role session is what makes it passable at the
  stronger levels, where the transaction's own snapshot cannot observe a later
  commit at all.
- **Saturation arm:** with as many concurrent explicit transactions in flight as
  the data pool has connections, every one of them still completes its
  authority read. This is the arm that would have caught the deadlock a
  data-pool checkout introduces, and no single-transaction arm can: the failure
  needs the pool to be exactly full before the ninth checkout is attempted.
- The ceiling read adds **no** server round trip to a warm **autocommit**
  operation, asserted by the same counting transport the parent proposal's cost
  criterion uses - **and, in the same test, the value used to authorize is shown
  to have come from the authority row rather than from process memory.**

  The second clause is what makes the arm discriminating. Round-trip counting
  alone **cannot fail**: today `check_unmask_authorization` reads a thread-local
  (`crate::context::with(|c| c.mask_policy_for(app_id))`,
  `crates/zeroship-plugin-db/src/crud/unmask.rs:314-315`), which costs zero
  round trips, so a pure cost arm passes identically on the un-migrated code and
  on the intended one. It would certify the very mechanism this contract exists
  to replace.

  The word "autocommit" is load-bearing and an earlier draft dropped it, which
  made this arm and the in-transaction arm above jointly unsatisfiable: inside
  an explicit transaction the ceiling is read on a separate platform-role
  session, and that read **is** an extra round trip. It is the price of a
  revocable authority that neither runs as the tenant nor rides the tenant's
  MVCC snapshot, and it is paid only by transactions that actually unmask. State
  the cost; do not write an arm that forbids it.
- **A joined row's mask policy is resolved per source collection**, asserted
  with a parent and child whose policies **differ on the same classification** -
  the case a single per-result lookup gets wrong while every same-policy
  fixture passes.

  This arm was carried by SC-3's acceptance list and belongs here: SC-3 says in
  its own words that per-source-collection mask policy "belongs to SC-6's
  contract rather than to this document's grammar". SC-3 keeps the pointer; the
  arm is this contract's to state, beside the three arms the joined-rows section
  above already owns.

**OWED: the storage flip has no arms in this list.** Every arm above measures
the **read contract** - the ceiling, its placement, its pool, its cost. The
decision to flip the storage changes the physical layout, and each of the four
items it records is testable and untested here: that plaintext remains
**queryable by real value** through the explicit API; that unique indexes and
foreign keys land on the raw column rather than enforcing uniqueness over masks;
that equality search by real value no longer matches, as a stated behaviour
change rather than a silent one; and that the AAD's binding of the column name
survives the rename. Owed item 4 is a migration-engine change, so it cannot ride
a runtime step and cannot be asserted by a runtime arm - which is a reason to
name it here, not a reason to leave it out.
