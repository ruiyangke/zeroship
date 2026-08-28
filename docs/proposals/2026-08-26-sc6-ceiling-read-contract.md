# SC-6: the mask ceiling contract

**Date:** 2026-08-26, cut down 2026-08-27

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** the mask-policy section of that document.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what.

---

## RETRACTED 2026-08-27: this document was a READ contract, and there is no read

**Operator decision 4, 2026-08-27: the operator ceiling is worker
configuration.** It is delivered at worker composition, immutable per isolate,
and changed by **rolling the workers**. Effective permission is
`ceiling INTERSECT draft`, computed **once at binding construction**, not per
operation.

This document was titled "the mask-ceiling **read** contract" and was ~740 lines,
most of them specifying how a data operation observes a newly committed ceiling
value. That question no longer exists. **What is deleted, and it is not deleted
for being wrong:**

- **the ceiling table** in `__zeroship_admin`, and its privilege-posture arm;
- **the ceiling write path** - the control-plane writer, the CAS against the
  value the operator believed current, the monotonic version counter;
- **the per-operation read**, its placement in the `prepare` batch, and the
  reordering of the authorization point that placement required;
- **the in-transaction read** on a separate platform-role session, and the
  **dedicated authority pool** that read needed to avoid deadlocking against the
  eight-connection data pool;
- **the SQLite ceiling home**, which existed because `__zeroship_admin` is a
  PostgreSQL schema;
- **the version-discovery and linearization contract**, which is the thing this
  document was written to solve.

**The retracted acceptance criterion, quoted so it is not softened.** Every arm
here began "lowering the operator ceiling", and the criterion was that doing so
denies the next `unmask` in an already-built pinned isolate **"with no rebuild
and no deploy"**.

**That requirement conflated two different deploys.** The ceiling is *operator*
state. It was never in the tenant artifact and there was no shape in which it
would be, so applying a new ceiling never needed a **tenant** redeploy - which is
what "no deploy" was defending against. What it needs is the **worker roll the
operator already controls**. The criterion read as a strong property and was in
fact ruling out a cost nobody was proposing to pay.

Changing a mask ceiling is a **rare, critical** operation. Buying sub-second
propagation for it with a distributed cache-coherence problem on the hot path of
a security decision is the wrong trade, and the deletion list above is the
measure of what that trade cost.

**The costs of decision 4, stated here rather than only in the parent:**
revocation latency becomes worker-roll time, not instant; deploy-pinned workflow
isolates keep their old ceiling until evicted (bounded by
`max_pinned_isolates_per_app`), so the operator's lever for immediacy is
force-eviction; and under decision 3 those same isolates hold their old
*declared* policy too, so force-eviction is the single lever rather than one of
several.

**Why the deleted argument is kept.** Both round-3 reviewers found the same
thing from different directions: **no transport** (`zeroship-plugin-db` has no
HTTP client and `check_unmask_authorization` is a synchronous `fn`,
`crates/zeroship-plugin-db/src/crud/unmask.rs:305`) and **no linearization
point** (a cache keyed by `(app_id, ceiling_version)` cannot discover a newly
committed version by itself, so revocation would silently never arrive). The
second is the deeper one and it is why the runtime-resolved shape was
**abandoned rather than repaired**: every mechanism in the deletion list above
was an attempt to escape that circularity, and the escape that worked was to
stop needing a version. Both findings were correct. What neither asked - and
what this document then spent 700 lines not asking - is whether the ceiling
needed to be database state at all.

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

**The effective ceiling is a field of the binding.** It arrives as worker
configuration at composition, is met with the artifact-borne creator draft
**once**, at binding construction, and is then immutable for the isolate's life.
`check_unmask_authorization` gains the effective policy as a parameter and
**stays synchronous** - which is the one property the retracted contract and this
one share, and it was always the easy half.

There is no I/O on the authorization path, no round trip to count, no cache, no
version, and no staleness. Those are not achievements of this contract; they are
absences, and the previous shape had to work for each of them.

**One thing this contract keeps from the retracted one**, because it is easy to
lose in a rewrite: the ceiling is resolved **independently of the app-supplied
actor**, never fetched through a map entry the actor names ("The ceiling, not the
actor, is the security boundary", below). That holds identically whether the
ceiling comes from a table or a config file.

*(RETRACTED: the read placed in the operation's own `prepare` batch, and its
three justifications. Two - a linearization point, and not making the
authorization path async - are satisfied trivially by a value that never moves.
The third was a cost claim conditional on **moving the authorization point after
`prepare`**, since today it runs "**BEFORE** `build_find_with_schema` fires the
SQL" (`crud/unmask.rs:1235-1236`). That reordering is no longer required by this
contract; anyone finding it in the parent's pipeline should know it was a
consequence of the ceiling read alone.)*

### RETRACTED: explicit-transaction placement

The retracted contract read the ceiling **per authorization**, not pinned for
the transaction, on the rule "schema is pinned, authority is not". Under
decision 4 there is nothing to pin and nothing to re-read: the effective policy
is a field of the binding, identical inside and outside a transaction.

**Three findings from that work are kept, because each generalises past the
mechanism that produced them:**

1. **An authority read never traverses the data snapshot and never runs under
   the tenant's own role** - Fork B in the parent. It survives as a *rule*
   though this design no longer has an in-transaction authority read:
   `apply_per_app_role` issues `SET LOCAL ROLE` with the DB-1 guards
   (`crates/zeroship-plugin-db/src/transaction/mod.rs:202-217`) immediately after
   the top-level `BEGIN` (`:540`), so every later read on that connection runs
   **as the per-app role**. Any future mid-transaction authority value faces this
   unchanged.
2. **"Failure is denial" conceals total functional breaks; the fix is a paired
   control.** The retracted mechanism would have returned `permission denied` for
   every in-transaction ceiling read while the arm asserted a deny - passing
   **vacuously** with every non-`auto` unmask in every transaction bricked.
   **Any arm whose expected outcome is a denial must also prove the granted path
   works.** Applied to every arm below; the most transferable thing this document
   produced.
3. **A second connection taken while holding a first is a deadlock, not a
   latency cost.** The data pool holds **eight** connections
   (`Pool::connect(&url, 8)`, `crates/zeroship-plugin-db/src/lib.rs:862`); eight
   concurrent transactions each wanting a ninth is a cycle no single-transaction
   arm can expose. The dedicated authority pool that answered it **has no client
   now** and is not built; the reasoning is kept for the next feature that wants
   a second checkout mid-transaction.

### RETRACTED: the ceiling table's privilege posture, and its SQLite home

Both are deleted with the table, and neither needs replacing:

- **The privilege posture** was a per-table arm at column granularity
  (`has_any_column_privilege`, because a column-level `GRANT UPDATE (ceiling)`
  returns `has_table_privilege = f` while granting the write, measured on PG
  16.14), with roles enumerated from `pg_roles` and a positive control. Its point
  was that a tenant holding `UPDATE` on the ceiling table self-authorizes
  `unmask`. **Worker configuration is not tenant-writable by any grant**, so the
  property now holds by construction.
- **The SQLite home** existed because `__zeroship_admin` is PostgreSQL-only. It
  is replaced by the dev composition point, and the honest note it carried still
  applies: the developer owns the bytes on that tier, so dev's guarantee is
  **contract parity**, not the same adversarial posture - the split
  `docs/reference/auth-dev-tier.md` already draws.

**OWED: the dev and `zeroship serve` ceiling source is not specified anywhere.**
"Worker configuration" names the worker's composition point. `zeroship serve`
and the Vite dev vector are separate composition points, SC-4 and SC-5 cover
neither, and a dev tier with no ceiling source plus "failure is denial" below
denies every non-`auto` unmask in dev permanently. That is the same failure the
retracted SQLite section existed to prevent, arriving through a different door.

### Failure is denial

An unresolvable or absent ceiling **denies**. This tightens today's fallback:
currently a missing policy still permits `kind == "auto"` (`unmask.rs:318-321`),
which is defensible when the policy is app-declared convenience and is not
defensible once the ceiling is the operator's limit. **An absent ceiling is not
an empty ceiling**, and under decision 4 "absent" means a worker composed
without one - which is a configuration error that must fail loudly at
composition, not silently at the first unmask.

That is a change in *where* the failure surfaces, and it is an improvement worth
naming: a missing table could only be discovered by an operation trying to read
it, while missing configuration can be refused before the worker serves
anything.

## RETRACTED: the ceiling write path

This specified a platform-owned table, a control-plane writer through a
privileged function, a CAS against the value the operator believed current, and
a monotonically advancing version an audit row cites. It was added because every
arm here began "lowering the operator ceiling" while **nothing in any of the
seven documents said how a ceiling is written**, and an arm whose precondition
cannot be performed is not testable.

**The gap was real; the fix is now a different one.** A ceiling is written the
way every other piece of worker configuration is written, and applied by rolling
the workers. The CAS and the version counter go with the table: two concurrent
operator edits to a configuration source are a configuration-management problem
with existing answers.

**OWED: what does not survive the move.** The retracted path gave a ceiling
change an **audit citation** - a version number an audit row could name. Worker
configuration gives none by itself. If "which ceiling was in force when this
unmask was authorized" must be answerable after the fact, the binding's
effective policy needs an identity the audit row records, and nothing in this
set specifies one.

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

### The flip became LOAD-BEARING on 2026-08-27 (decisions 7 and 8)

Read the decision below with this in front of it. When it was made, the flip was
a defence in depth: the runtime also re-checked the live catalog on every
operation, so a descriptor that was wrong about masking would be contradicted
before it could leak.

**Decisions 7 and 8 remove that check entirely.** The descriptor is the sole
schema authority and the data plane never reads the catalog. So **the physical
column layout is now the only thing standing between a stale descriptor and a
plaintext read**, and the flip stops being defence in depth and becomes the
defence.

Concretely, for the failure the parent states under decision 8 - a deploy going
live before its migration applies:

- **Under the current layout**, the descriptor says `ssn` is masked, the
  database still holds plaintext in `ssn`, and the read returns **plaintext**
  believing it is masked. Nothing detects it and no audit row records anything.
- **Under the flip**, `ssn` holds the masked value physically. A descriptor that
  is wrong about masking reads a masked column and **leaks nothing**. The real
  value is in `ssn_raw`, which the query surface cannot address at all.

The flip's original argument was "the ignorant path is the safe path", aimed at
ignorant *code* - a builder that does not know a column is masked. It now also
covers an ignorant *deploy*. That is a strictly larger claim than the one the
decision was made on, and it means the flip's four owed items are now on the
critical path for the masking guarantee rather than beside it.

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

### BLOCKING, added 2026-08-28: the flip's WRITE path is unguarded, and as specified the flip opens a wider plaintext leak than it closes

Two independent reviewers of the item-1 design
(`docs/reviews/2026-08-27-query-by-plaintext.md`) converged on this from
different starting points, which is why it is recorded as blocking rather than
as a fifth owed item. Verified directly:

**Outward - every write verb returns the raw sibling.** There are **34**
`RETURNING *` sites in `crates/zeroship-schema/src/query.rs` (insert `:3584`,
updateOne `:4005`, insertMany `:4157`, updateMany `:4228`, delete `:4254`,
soft-delete/restore `:4445`-`:4557`, upsert `:5937`, findOrCreate `:6020`).
`RETURNING *` is every physical column and it never passes through
`implicit_read_projection_parts` (`query.rs:3345-3366`), which is SELECT-side
only.

The stripper knows exactly ONE sibling name:

```rust
let sibling_key = format!("{col}_masked");   // mask_pass.rs:469
```

and pushes only that onto `to_strip` (`mask_pass.rs:479-482`). **Post-flip that
key does not exist**, `to_strip` is empty, and `<col>_raw` survives. Its
fallback arm re-masks the parent correctly, so the masked column comes back
right - which is exactly what makes this silent. `strip_encryption_markers`
retains everything not prefixed `__zsbin__` (`encryption_pass.rs:502-505`), and
`mapResultDoc` copies every key with an identity fallback
(`sdks/db/src/utils.ts:28-34`).

So `await db.users.insert({ ssn })` returns the plaintext in a key the generated
`Row<S>` type does not declare. **Strictly worse than the leak the flip
retires**, because it is invisible to any review written against the generated
types. `rawProjectable: false` does not help: nothing reads it, and
`RETURNING *` reaches no projection allowlist.

**Inward - the raw column is filterable.** `_raw` is absent from
`RESERVED_NAMES` (`query.rs:738-766`), and `build_field_condition_with_dialect`
calls only `validate_field_name` with no schema hint (`:5329-5330`). Same hole
in `build_conflict_probe_with_dialect` (`:2905-2929`) and
`build_write_target_probe` (`:2850-2880`). Aggregate `$match` is bare -
`build_where(match_val, &mut params)` with no `schema_hint` (`:4646`) - while
`$group.by` ten lines below DOES validate (`:4656`). The asymmetry is inside one
function.

**Three more the reviewers verified, each silent:**

- **Live-query subscriptions stop firing.** `normalise_filter` captures the
  logical name (`read_set.rs:220-260`) and `Conjunct::matches_text` compares it
  against the WAL tuple (`:119-131`). Post-flip the tuple holds the MASK under
  `ssn`, so `find({ssn: "123-45-6789"})` never matches again. That module's own
  doc calls silently dropped events "unacceptable" (`:215-219`).
- **The dev tier loses all mask metadata.** The SQLite introspector requires
  `sibling_name.strip_suffix("_masked")` to match before recording the entry,
  **with no `else`** (`backend/sqlite/mod.rs:2220-2237`); the malformed-sentinel
  arm warns, this one does not.
- **Unique indexes land on the mask** (`query.rs:1949-1964`), where two values
  sharing a last-4 collide - a write-breaking inversion, not a read one.

**Why this is blocking rather than owed.** The four items below are work the
flip creates. This is a defect IN the flip as specified: the decision's whole
claim is that the raw value becomes unreachable, and on the write path it is
reachable in both directions. **The flip must not be implemented until the write
path is specified**, because a partial implementation would retire a known leak
and open an unknown one.

**Why nobody caught it earlier, including me.** The item-1 design's own 18-item
cost list enumerates READ-shaped surfaces that stop working - the half already
protected by `validate_read_identifier` at seventeen sites. The write path is
absent from it. Every reviewer, and every earlier analysis in this document,
reasoned about what the flip makes unreadable and not about what it leaves
returnable.

### What this decision now owes (five items)

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

   **CORRECTED 2026-08-27: this is wrong for the encrypted case, and the SDK
   already knows it.** A unique index on `ssn_raw` enforces **nothing** when the
   field is randomised-encrypted: `canonical_aad` binds the row PK, so identical
   plaintext produces different ciphertext in every row and every value is
   trivially unique. `sdks/db/src/types.ts:1147-1159` already REFUSES `.unique()`
   on an encrypted field for exactly this reason, so the item as written asks the
   engine to place a constraint the authoring surface will not let a creator
   declare.

   Where uniqueness over the real value CAN be enforced is a keyed lookup column
   (`docs/reviews/2026-08-27-query-by-plaintext.md`), because equal plaintext
   produces an equal token by construction. That turns a currently-refused
   declaration into a supportable one - so this item is not merely wrong, it is
   pointing at the wrong column.
3. **Equality search by real value changes behaviour.** `find({ssn: "123-45-6789"})`
   matches nothing after the flip. That is correct - plaintext should require an
   explicit, audited request - but it is a visible change and belongs in
   `docs/reference/db.md` beside the mask kinds, so a creator learns it when
   they declare the mask rather than when a query silently stops matching.
4. ~~**The AAD binds the column name.**~~ **RETRACTED 2026-08-28 - it binds the
   LOGICAL FIELD KEY, and the flip is therefore not a re-encrypt.** Measured:
   `canonical_aad(collection, &col, ..)` receives `col` from
   `for (col, def) in schema_obj.iter()` (`crud/encryption_pass.rs:173`,
   `:200-207`), the schema field key - not `storage.rawColumn`. Same at `:337`
   and `crud/unmask.rs:452-458`. The two are the same string today for every
   masked+encrypted field, so the distinction was invisible.

   The false constraint is asserted as fact in a doc comment
   (`migrate-core/src/render/gen_types.rs:160-169`). **Correct that comment in
   the same commit as any flip work**, or the next reader "fixes" the AAD to
   match it and destroys every ciphertext in the deployment.

   **Neither branch is right, though.** Binding the bare logical name stops being
   sufficient the moment item 2's keyed lookup column lands: two encrypted
   columns then share one logical field and one AAD, so swapping their contents
   passes tag verification. **Bind the logical field name plus a stable role
   discriminator (`value` | `lookup`).** A role survives renames; a physical name
   does not.
5. **The flip swaps which column carries the declared TYPE and the whole
   constraint set** - added 2026-08-28, and absent from every document in this
   set until now, including the 966-line write-path specification.

   `.mask()` is legal on string, number and bytes, and every mask kind returns a
   **String**. Today that is harmless: the sibling is bare `TEXT` while the
   field's own column keeps its declared type and everything
   `def_to_constraints_for_dialect` attaches (`query.rs:2719-2827`) - `NOT NULL`,
   `DEFAULT`, range `CHECK`, literal `CHECK`, enum `CHECK`. Post-flip the logical
   column holds `'***'`:

   - `t.number().mask(...)` leaves `ssn` as `DOUBLE PRECISION`; writing `'***'`
     is a hard error.
   - `t.string().enum([...]).mask(...)` leaves `CHECK ("ssn" IN (...))`, which
     refuses `'***'`. **Every write fails.**
   - encrypted+masked leaves `ssn` as `BYTEA`.

   **And the migration engine cannot see the change.** The column-additions
   branch is name-only "no matter how its declared type has changed"
   (`migrate-core/src/schema/diff.rs:856-882`, its own comment), and the
   `RewriteColumnType` arm keys strictly off the `encrypted` toggle (`:894-900`).
   The flip moves neither the name nor that toggle, **so the differ emits
   nothing**, and the runtime writes a mask string into a numeric or binary
   column.

   For an EXISTING table a double rename is free, because types and constraints
   travel with the renamed columns:

   ```sql
   ALTER TABLE t RENAME COLUMN ssn        TO <raw>;  -- free the logical name first
   ALTER TABLE t RENAME COLUMN ssn_masked TO ssn;
   ```

   For a NEW table, `build_create_table_with_fks_for_dialect` must emit the
   declared type and constraints under the **raw** name and a bare `TEXT` sibling
   under the **logical** name. Nothing does that today.

   **This is the item that makes the flip's migration hand-authored with nothing
   verifying it** - which operator decision 9 identifies as the flip's one
   surviving cost, and why it owes a mutation-proved test before it runs
   anywhere.

## Joined reads

### A joined result MUST be nested before the read pipeline runs

This is a precondition of everything in the next section, and it is not a
presentation choice - a **flat** joined row cannot carry the identity the
pipeline needs.

Two mechanisms fix that, both keyed on a single unqualified name per row:

- the mask pass writes its sibling as `format!("{col}_masked")`
  (`crates/zeroship-plugin-db/src/crud/mask_pass.rs:150`), so two joined
  collections with a same-named masked column collide;

  **Decision 7 changes the mechanism of this one, and not the requirement**
  (2026-08-27). The descriptor now carries the physical layout, including the
  sibling columns, so the runtime **never re-derives a sibling name** by
  `format!` and never parses a `__zsmask:` sentinel on the read path. A
  descriptor-supplied mapping can name the two siblings distinctly, so the
  collision is avoidable by construction rather than by nesting. **Nesting is
  still required** - the other two arguments below, the `row_pk` plucked from
  `obj.get("id")` and the AAD binding, are untouched by where the sibling name
  comes from, and the AAD one cannot be patched at any call site.
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

## Relationship to the migration ceiling - now closer than "shape only"

The platform already has operator-ceiling machinery with meet-semantics
(`crates/zeroship-migrated/src/policy.rs`, `policies/confined.policy.toml`), and
**decision 4 moves masking onto the same delivery model**, not merely the same
vocabulary. `migrated`'s default ceiling is `CONFINED_CEILING_TOML`
(`policy.rs:48-59`): a TOML document compiled into the binary with `include_str!`
- operator configuration delivered at composition, exactly what a mask ceiling
now is. The compose is `compose_effective_for_app` (`policy.rs:119-122`).
Masking should look like its neighbour.

**Two differences remain, and one of them is new.**

- **Key vocabulary.** That ceiling is DDL-knobs-only: `CREATE TABLE` /
  `CREATE SCHEMA` / `RENAME` / destructive-ops / RLS (`policy.rs:32-35`), with
  no vocabulary for mask classifications. Separate document, separate keys.
  Sharing the store would put two unrelated policies under one name.
- **Meet semantics, and this is the one to not copy by accident.**
  `migrated`'s compose is **escalation-reject**: "a draft grant looser than the
  ceiling permits is rejected, never clamped" (`policy.rs:16-17`, restated in
  the compose's own doc comment at `:120-121`). Masking's meet **clamps** - the
  pointwise meet over a role-complete domain specified below, which narrows
  silently. Both are defensible and they are not interchangeable: reject
  surfaces the creator's mistake at deploy time, clamp lets a deploy succeed
  with less access than it asked for. **This document keeps clamp**, and records
  the divergence so "look at its neighbour" is not read as "copy its semantics".

*(A third difference is now moot and is recorded because it was this section's
main point. The retracted version said not to reuse `migrated`'s staleness rule,
`ApprovalStaleCeiling` -> "re-submit required"
(`crates/zeroship-migrated/src/apply.rs:192-199`), on the grounds that it is the
"exact opposite" of what revocation needs - this document's criterion being that
a lowered ceiling takes effect with no rebuild and no deploy. **That criterion is
itself retracted.** With the ceiling delivered at composition there is no
staleness rule to share or reject: a ceiling is current for a worker by
construction, and out of date only for a worker that has not been rolled.)*

## What the creator half contributes

`manifest_declared` is **untrusted, deploy-scoped data**. The `.zship` manifest
carries no signature (`crates/zeroship-bundle/src/manifest.rs`; `deploy_hash` is
a digest the control plane computes on receipt, `:44-48`), so a build-graph
dependency can author it. Its only security property is that it cannot outlive
its deploy - which is the entire reason it moved out of the durable store.

### The creator half now has a decided carrier, and still owes five artifacts

**Decision 3 (2026-08-27) decides the carrier**: the creator's mask policy is
declared in the creator's codebase, folded at build time, and delivered through
the **artifact/init channel that already carries the runtime schema descriptor**,
immutable for the isolate's life. It is not a new channel and not a new trust
standing - it is the descriptor's, which is why the paragraph above about
`manifest_declared` being untrusted and deploy-scoped applies to it unchanged.

**What that does not supply.** The gap this section was written for is narrower
now but not closed. `manifest_declared` is named throughout this document as one
of the two inputs to the effective policy, and **no document defines it as an
artifact** - not the field, not its schema, not the authoring API, not
validation, not the build step that emits it. The five owed pieces are:

1. the artifact field and its schema;
2. the authoring surface that produces it;
3. validation at build time;
4. the emission path through the packer;
5. the runtime read that turns bytes into a `MaskPolicy`.

They are now owed against a **decided target** rather than an open question,
which is the whole of what decision 3 changes here.

**Why they remain a prerequisite of the deletion, in the same step.** The parent
proposal deletes the only creator-facing way to declare a mask policy -
`defineMaskPolicy` (`sdks/db/src/policy.ts:118`, reached through the bootstrap's
dev and runtime entries) - and there is no latent route waiting to replace it:
the string `mask` appears **zero** times in the Rust manifest
(`crates/zeroship-bundle/src/manifest.rs`) and **zero** times in the TypeScript
manifest shape (`sdks/vite-plugin/src/zship.ts`).

The consequence is not a missing convenience. With no declared policy,
`mask_policy_for` yields nothing and the authorization fallback admits only
`auto` - so **every creator role is denied every unmask**, and the masking
feature is inert for its actual users while appearing to be configured. It fails
closed, which is the right direction and the wrong outcome.

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

**Every arm whose expected outcome is a denial carries a granted-path control.**
That discipline came out of the retracted mechanism - which would have produced
`permission denied` for every in-transaction unmask while the deny-only arm
passed - and it is the one thing from that work that must not be lost in the
cut-down. A fail-closed default hides a total functional break behind a green
test.

- **The criterion this exists for, restated:** a binding constructed under a
  lowered ceiling denies `unmask` **by an actor the effective ceiling governs**,
  **and the same test pairs it with a classification the same ceiling still
  permits, which must SUCCEED.** A ceiling that denied everything would
  otherwise satisfy the arm perfectly, and so would an implementation whose meet
  is broken in the direction of denying.

  *(RETRACTED: "in an already-built pinned isolate, with no rebuild and no
  deploy", and "the deny arrives without any invalidation message being
  delivered". Decision 4 makes the first wrong rather than unmet - the ceiling is
  worker configuration, so observing a lowered ceiling **is** a new binding - and
  makes the second vacuous, since there is no invalidation channel to abstain
  from using.)*

  The qualification is about **which actors the ceiling governs**. `auto` is not
  exempt: it is granted everything only as a **fallback**, when the policy does
  not list it (`crates/zeroship-plugin-db/src/crud/mask_policy.rs:85-110`). A
  ceiling that lists `auto` narrows it like any other role. The scope is a
  property of the lattice, not an exemption.
- **A ceiling that revokes `auto` denies `auto`, when the creator draft does not
  mention `auto` at all** - **paired with a role the ceiling does not name,
  which keeps its lattice default.** This is now the sharpest arm in the
  document, because the naive implementation (a map intersection over shared
  keys) **inverts** the revocation for exactly the actor with the most access,
  and passes every other arm here.
- **An absent or unresolvable ceiling denies, and it is refused at composition
  rather than at the first unmask** - **paired, in the same test, with a
  configured permissive ceiling that grants**, so the arm cannot go green on an
  implementation where every composition fails.
- **The effective policy is identical inside and outside an explicit
  transaction**, asserted with the same classification unmasked both ways in one
  test. Under decision 4 this is true by construction, which is exactly why it
  is worth asserting: a regression that reintroduced a per-authorization lookup
  would show up here and nowhere else.

  *(RETRACTED: the in-transaction arm that required a ceiling lowered
  mid-transaction to deny the next `unmask` in that same transaction, and its
  isolation parameterisation across `READ COMMITTED` /
  `REPEATABLE READ` / `SERIALIZABLE` (`docs/reference/db.md:999`;
  `crates/zeroship-plugin-db/src/transaction/mod.rs:117-118,1186-1187`). Nothing
  changes mid-transaction now, so there is no isolation level at which the
  answer differs.)*

  *(RETRACTED with it: the **saturation arm** - fill the data pool with
  concurrent explicit transactions and require every one to complete its
  authority read. It was the only arm that could catch the eight-connection
  deadlock and has no subject now. Its *shape* - fill the pool exactly, then
  require progress - is the general test for any second checkout taken while a
  first is held.)*

  *(RETRACTED with it: the **cost arm** - no round trip added to a warm
  autocommit operation, with the discriminating clause that the authorizing
  value came from the authority row rather than process memory. Under decision 4
  the value **is** in process memory, deliberately, so that clause is now the
  thing the arm would fail. The cost property survives without an arm: zero
  round trips, unconditionally, in and out of transactions.)*
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
