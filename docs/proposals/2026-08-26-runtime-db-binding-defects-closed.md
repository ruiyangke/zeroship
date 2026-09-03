# Defect register: closed defects, and how each one hid

Split out of `2026-08-26-runtime-db-binding-defect-register.md` on 2026-08-28.
Every entry below carries FIXED or CLOSED against `feat/dbbind-impl`. The
register that remains carries the live work only, so that its outline is a list
of what is still wrong rather than a list a reader has to filter.

## How to read this file

**Nothing here was summarised.** Each entry is the register text verbatim,
moved rather than rewritten, and that is the whole point of the file existing
instead of the entries being deleted. **The durable value of a closed defect is
not "this was broken" - it is the mechanism by which it stayed invisible**, and
that mechanism is stated inside the entry, usually at length, usually as a
correction of an earlier reading. Four examples, so the file advertises what it
holds:

- **L13, L14 and L15** were reported "still open" against a tree that was on no
  branch. Every line number in that pass was real, of the wrong tree.
- **L25**: `unmaskField` had never worked on PostgreSQL in a shipped binary,
  because the schema's only installer is `#[cfg(feature = "test-helpers")]` and
  no member enables it - and then the inference from that fact was wrong in the
  other direction, because `cfg(test)` compiles when tests run.
- **L27**: `distributed_live` was silently red for eleven days while the harness
  printed `all arms green` and, on the line above, that it had declined to run
  the target.
- **L24 and L26**: the read path failing OPEN on a cold cache, and `orderBy`
  sorting by the plaintext a mask hides.

### What the 2026-08-27 batch had in common

*Verbatim from the register header, moved here on 2026-08-28 because four of the
five entries it rules on are in this file. L28 is the fifth and is live, in the
register.*

**Thirteen entries now carry FIXED**, of which **six were found and fixed on
2026-08-27** (L13, L14, L15 were already fixed and mis-reported; L23, L24, L25,
L26, L27 were found this day). Three of those are security-relevant: an SSRF
bypass reachable from an inherited environment variable, a read path that failed
OPEN on a cold cache, and `orderBy` sorting by the plaintext a mask hides.

**Five entries are new on 2026-08-27, and not one was found by looking for
defects.** L24 and L26 came out of the descriptor-specification survey: the
read path fails OPEN in two places when the schema is absent, and `orderBy` on
a masked column sorts by the plaintext the mask is hiding. L25 came out of the
`__zeroship_admin` deletion: `unmaskField` has never worked on PostgreSQL in a
shipped binary. L27 came out of verifying that deletion: `distributed_live`
fails on the branch and the 11-arm harness was structurally unable to see it.
L28 came out of the e2e rewrite discovering it could not run the suite it had
just rewritten: `platform-cli` has not compiled since the engine was
republished, blocking 26 harnesses.

**What the five have in common is the finding.** Each is invisible to the
checks that exist, by construction rather than by accident: L24 needs a cold
cache; L25 needs the PG backend the mask-policy tests never use; L26 needs
someone to compare two builders no arm compares; L27 was in a target the
harness declined to run while printing "all arms green"; L28 is behind a
feature flag no default build enables, so every gate stays green because every
gate builds the default feature set.

The generalisation is uncomfortable and worth stating plainly: **on this branch,
"the suite is green" has been carrying much less information than it appears
to.** Three of the five were surfaced only by deleting code, and a fourth only
by an agent needing to actually run something. None would have been found by
reading more carefully.

*(The register's copy of that paragraph ended mid-sentence - "None would have
been found by reading more carefully. It / should never be read as design, and
an entry's absence from a later revision / means it was closed, not that it was
wrong." The `It` had no referent; a clause was cut out of the paragraph at some
point and nothing re-read it. **The surviving claim is worth keeping and is
restated here with a subject rather than guessed at**: this register is a record
of measurements, it should never be read as design, and **an entry's absence
from a later revision means it was closed, not that it was wrong**.)*

**Two mutations were made to entry text at the split, and they are the only
two.** L12b's
heading carried no status while its body said `FIXED 2026-08-27 in b50691f2c`,
which is exactly the stale-heading fault this document set records against
itself twice; the status is now in the heading. And L12b's final section - the
`max_slot_wal_keep_size` mitigation, which has NOT landed - moved to the live
register under L12, because it is live work and this file is not where live work
goes. Everything else is byte-for-byte the register text.

**Treat every figure in this file as unverified unless the text says how it was
measured.** Several entries carry numbers that were carried forward across
revisions rather than re-derived, and the register records at least two cases
where that went wrong (L22a's ~76 KB, arrived at from a multiplier that had
already been retracted; and the RETRACTED section in the live register, which
claimed a measurement it did not have). Where an entry names its instrument -
`tmp/measure_decode_multiplier.sh`, `pg_settings.boot_val`, a `Cargo.lock`
version - the figure is as good as that instrument. Where it does not, it is a
number someone wrote down.

### The verification failure that governs three of these entries

Kept verbatim from the register header, because L13, L14 and L15 are the
entries it rules on and they are here rather than there:

> **The 2026-08-27 "re-verified against the tree at `f44bcf6b6`" pass measured
> a tree that is on no branch, and three of its verdicts were wrong in the
> same direction.**
>
> `f44bcf6b6` is not an ancestor of `feat/dbbind-impl`; `git branch -a
> --contains f44bcf6b6` returns **nothing**. Its content reached this branch
> under different hashes (`15f9305e2`, then `a4a7872ad`), so the *code* was
> reachable and the *baseline* was not - which is why the pass looked sound.
> Against `196622c9b`, **L13, L14 and L15 are all fixed** by commits that
> landed at 05:00 on 2026-08-27 (`92bd67633`, `d49930f34`) or by the
> `try_subscribe` mint-site change. Each of those three entries ends with "The
> index reported this closed; it is not." **The index was right in all three
> cases.**
>
> The failure is worth naming because it is not carelessness: every citation in
> that pass was *checked*, and the line numbers it reports are real line
> numbers - of the wrong tree. A verification is only as good as the commit it
> ran against, and a dangling SHA reads exactly like a current one. **State the
> branch, not just the SHA, and confirm the SHA is on it.** The five
> `f44bcf6b6` citations remaining in this file are left in place, struck where
> they are wrong, so the pattern stays visible.

*(Three of those five citations are here - the struck ones in L13, L14 and L15.
The other two are in L18, which is a live entry and stayed in the register;
there they are load-bearing rather than wrong, because `f44bcf6b6` is the commit
that landed L18's partial fix.)*

### Measurement correction 2026-08-28: a count without its boundary is not a measurement

Same shape as the failure above, one level down, and recorded here because it
happened while this file was being assembled rather than being found in it.

The claim **"34 `RETURNING *` sites in `zeroship-schema/src/query.rs`"**
propagated across three documents in this set and was cited as fact. It was
corrected to **12**, which was written into two briefs. Then **20/14**. Three
values for one grep, each stated confidently, and the reason is that none of
them said where the boundary was:

```
crates/zeroship-schema/src/query.rs, measured 2026-08-28
  grep -c "RETURNING \*"                              34
  before `mod tests` at :6035                         20
  of those 20, on comment lines (`//` or ` *`)         8
  therefore SQL-emitting sites before :6035           12
  at or after `mod tests` at :6035                    14
```

Every one of 34, 20, 12 and 14 is a true count of something. **None of them is
"the number of `RETURNING *` sites" until the sentence says which set it is
counting.** The mechanism that makes this hard to catch is the second line of
that block: `#[cfg(test)]` occurs at `query.rs:572`, `:5504`, `:5509` and
`:6034`, and the first three are attributes on *individual functions*, not
module boundaries. Splitting on `#[cfg(test)]` therefore yields a clean-looking
17/17 that means nothing at all. Only `mod tests` at `:6035` is a real
boundary, and only `grep -c` on comment-vs-code separates a doc comment
describing SQL from a builder emitting it.

The generalisation for anyone re-deriving a figure in either file: **a count is
a measurement only when it names the set it ran over.** "34 sites" names a
file; it does not name a set.

---

## Ledger: what closed each entry

Added at the split. The SHA is the commit that closed the defect, not the commit
that filed it; dates and subjects are from `git log` in this worktree on
2026-08-28. Entries appear below in this order.

| Entry | Closed by | Landed | Commit subject |
| --- | --- | --- | --- |
| L24 | `632c1d1fa` | 2026-08-27 22:40 | feat(db)!: make the runtime descriptor the sole schema authority |
| L26 | `632c1d1fa` | 2026-08-27 22:40 | feat(db)!: make the runtime descriptor the sole schema authority |
| L25 | `b801bf12b` | 2026-08-27 18:10 | fix(db)!: delete the admin-schema key and mask-policy callers the deletion left |
| L27 | `a0074e154` | 2026-08-27 17:41 | test(db): create the migration-owned publication distributed_live needs |
| L6 | decision 7, landed `390f4b97b` then `b801bf12b` | 2026-08-27 16:36, 18:10 | refactor(db)!: delete the __zeroship_admin schema and its definer-rights routines |
| L23 | `8c6caa465` | 2026-08-27 16:28 | fix(runtime): make dev-ness a typed input the SSRF gate cannot read from the environment |
| L11 | `61298897b`, merged `93bc20126` | 2026-08-27 05:40, 07:08 | fix(db): remove full-text search |
| L12b | `b50691f2c` | 2026-08-27 05:04 | fix(db): reap crashed worker replication slots |
| L14 | `92bd67633` | 2026-08-27 05:00 | fix(db): enforce insertMany bind budgets |
| L15 | `d49930f34` | 2026-08-27 05:00 | fix(db): make encrypted updateMany atomic and bounded |
| L13 | `0efbf9f9f` | 2026-08-27 03:49 | fix(db): enforce the live subscription cap on creator opens |
| L10 | `4c8e84134` | 2026-08-27 02:57 | fix(db): bind runtime schema metadata to deploy identity |
| L8 | `ab4aaf954` | 2026-08-26 18:25 | fix(plugin-db): a commit the server answered with ROLLBACK is not a commit |
| L5 | `6ca179492` | 2026-08-26 15:41 | fix(plugin-db): refuse a decode that cannot read every value the caller passed |

**Four of those SHAs are new information, not carried from the entry.** L5, L8,
L13 and L26 named no commit in their text; they were resolved on 2026-08-28 with
`git log -S` against the symbol each entry cites - `DecodeError` in
`v8_bridge.rs` for L5, `exec_terminal_on_tx` for L8, `broker::try_subscribe` in
`v8_classes/subscription.rs` for L13, `read_column_for` in
`zeroship-schema/src/query.rs` for L26. L24's fix is the removal of the
`schema_hint.is_some()` arm, resolved the same way and landing in the same
commit as L26.

---

## Closed entries, newest first

*Entries below cross-reference L1, L2, L4, L9, L12, L16, L17, L18, L19, L20,
L21, L22a, L22b and L28, all of which are live and stayed in
`2026-08-26-runtime-db-binding-defect-register.md`. The numbering is unchanged,
so a reference resolves by searching for the same heading there. Those
cross-references were left untouched at the split rather than rewritten to name
the other file, because rewriting them inside otherwise-verbatim entries is a
chance to change what an entry says.*

### L24 (FIXED 2026-08-27) - an absent schema fails OPEN on the read path, in ~~two~~ THREE places at once

**FIXED by making the absence unrepresentable, not by adding a guard.** The
query builders now take `&Value` where they took `Option<&Value>`, so
`validate_read_identifier` has no permissive tail at all - its `if
schema_hint.is_some()` arm is gone and the failure is unconditional. "No schema"
cannot be expressed at that boundary.

**The entry undercounted.** It named two fail-open arms; there were **three** -
`build_find`'s `Ok("*")`, the PostgreSQL search path's `"t".*`, and the SQLite
vec0 search path's. All three deleted, at the builder AND at the caller. The
third was found only by making the type change and following the compiler,
which is the argument for this shape of fix over a guard: a guard would have
been added to the two arms someone had noticed.

**Honest limit on "unrepresentable".** It is unrepresentable in the query
builders. One layer up, "the descriptor has not been installed yet" is still a
representable state - it surfaces as the typed `collection_not_declared` rather
than as an unrestricted projection. Resolving at isolate construction, which
would remove even that, needs a CLI change nobody has made. **Fail-closed, not
structurally impossible**, and the difference is worth keeping straight.

**NEW - found by the descriptor-specification survey, re-verified line by line
2026-08-27 against `feat/dbbind-impl` at `196622c9b`. Both backend sweeps found
it independently.**

**When the read path has no schema, it does not refuse - it stops protecting.**
Two separate guards are keyed on `schema_hint`, and both treat "absent" as
"permit":

1. **The projection collapses to `SELECT *`.** `implicit_read_projection_parts`
   returns `None` without a schema, and both call sites fall through to a bare
   wildcard: `Ok("*".to_string())` (`zeroship-schema/src/query.rs:3312-3317`)
   and `format!("{qalias}.*")` (`:3264-3267`). The implicit projection is what
   keeps non-readable columns out of a result set, so its absence returns every
   column the table has.
2. **Field-name validation passes for anything.**
   `validate_read_identifier` (`query.rs:928-939`) checks
   `schema_declares_readable_field`, and when that fails it raises
   `InvalidIdent` **only** `if schema_hint.is_some()` (`:933`). With no schema
   it falls through to `Ok(())` at `:938`. Any syntactically valid field name is
   accepted.

Note these compound rather than overlap: guard 1 decides what comes back when
the caller names nothing, guard 2 decides what the caller is allowed to name.
One condition disables both, so there is no second line.

**Evidence:** `zeroship-schema/src/query.rs:928-939` (`:933` is the
`is_some()`), `:3264-3267`, `:3312-3317`

**This is the strongest single argument for decision 8, and it argues for it in
an unusual direction.** The fix is not a better fallback - it is **deleting the
state in which the fallback is reachable**. An immutable descriptor resolved at
isolate construction has no cold-cache window and no `None` arm, so the failure
mode stops existing rather than being handled. A guard that fails open is
usually a bug to invert; here inverting it (refuse when the schema is absent)
would be correct but strictly weaker than removing absence from the state
space, because a refusal still has to be reached and a state that cannot occur
does not.

**Do not fix this in isolation, and do not defer it silently either.** If
decision 8's implementation slips, this stays live: today the `None` arm is
reachable on a cold cache, which is a normal condition, not an exotic one.
Whoever lands the descriptor work owns closing it, and should assert on it
directly - a test proving that a read with no schema is refused, not merely
that reads with a schema behave.

**Not to be confused with L17**, which is the same shape one level up (a
partitioned table gets no runtime metadata, so its protection passes are
skipped). L17 is about a table the introspector cannot describe; L24 is about
the window before any description exists. Decision 8 removes both, which is why
L17 carries SUBJECT REMOVED and this entry should be read alongside it.

### L26 (FIXED 2026-08-27) - `orderBy` on a masked column sorts by the PLAINTEXT it is hiding

**FIXED, and by removing the second derivation rather than teaching it the
rule.** There is now ONE site that maps a declared field to the column a read
touches - `read_column_for` (`zeroship-schema/src/query.rs:3402`), reading
`storage.valueColumn` from the descriptor. The projection and the ORDER BY
builder both go through it, so they cannot disagree; previously they were two
independent derivations and only one of them knew about masking.

The failing test captured first, verbatim:

```
SELECT ..., "ssn_masked" AS "ssn" FROM "app1"."users"
  ORDER BY "ssn" ASC NULLS LAST LIMIT 10
```

- the projection reads the mask, the sort reads the plaintext, in one statement.

**The tests assert against `read_column_for`'s answer, not against the literal
sibling name**, so they keep testing the property after the storage flip lands
and `valueColumn` stops being the `_masked` column. That was deliberate: an
assertion pinned to `"ssn_masked"` would have had to be rewritten by the flip,
and a test rewritten during a risky change is a test nobody trusts.

**NEW - flagged by the descriptor-specification survey, verified line by line
2026-08-27 against `feat/dbbind-impl`. A PII inference channel, not a
theoretical one.**

**The projection returns the mask; `ORDER BY` sorts by the real value.** The
two disagree because only one of them maps the logical field to a physical
column:

- **Read projection maps.** A masked field's default projection emits the
  masked sibling under the logical name. The descriptor work measured this
  against the DDL emitter's own function and recorded `valueColumn =
  "ssn_masked"`, `rawColumn = "ssn"` - i.e. today the **authoritative plaintext
  stays in `ssn`** and the mask is written to `ssn_masked` by `mask_pass.rs:150-152`
  (`let sibling = format!("{col}_masked")`).
- **`ORDER BY` does not map.** `build_order_by_with_validator`
  (`zeroship-schema/src/query.rs:5487-5528`) validates the key and then calls
  `build_order_term(key, descending, dialect)` with the **raw declared name** -
  both in the object arm (`:5500`) and the array arm (`:5521`). No storage
  lookup, no sibling substitution.

So `find({ orderBy: { ssn: "asc" }, limit: 1 })` orders rows by the plaintext
SSN while returning only `***-**-1234`. Repeated with a moving filter or
pagination, that is a **binary search over a value the caller is not permitted
to read**. It needs several queries rather than one, which lowers the severity
from "read the secret" to "recover the secret", not to "safe".

**Evidence:** `zeroship-schema/src/query.rs:5477-5484` (the read-path order
builder), `:5487-5528` (the emitter using the raw key), `:5500`, `:5521`;
`crates/zeroship-data-engine/src/crud/mask_pass.rs:150-152`

**It compounds with L24.** The only gate on the order-by key is
`validate_read_identifier` (`:5483`), which L24 establishes **fails open when
the schema is absent** (`query.rs:933`). On a cold cache the caller can order
by any column name at all, not merely a declared one.

**The flip closes this by construction, which is the point.** After the storage
flip `ssn` holds the masked value and the plaintext lives in `ssn_raw`, which
is declared not sortable (`storage.rawSortable = false`). An `ORDER BY "ssn"`
then sorts the mask - harmless - and there is no spelling of the raw column a
caller can reach. **Note the shape of the fix: it is not a new check.** Adding
a "reject orderBy on masked fields" guard would work and would be strictly
worse, because it is one more place that must remember the rule. The flip makes
the default safe and the dangerous column unnameable.

Until then this is live. It is also **untested in either direction**: there is
no arm asserting that a masked column cannot be ordered by, and no arm
asserting the projection and the sort agree on which column they mean.

### L25 (FIXED 2026-08-27 in `b801bf12b`) - `unmaskField` has never worked on PostgreSQL in a shipped binary

**NEW - surfaced by the `__zeroship_admin` deletion, verified independently
2026-08-27 against `feat/dbbind-impl` at `196622c9b`. This defect PRE-DATES the
deletion; the deletion only made it visible.**

**The only installer of `__zeroship_admin` is compiled out of every shipped
binary, and ungated production code calls into it.**

- `ensure_admin_schema` - the sole creator of the schema and its routines - is
  `#[cfg(any(test, feature = "test-helpers"))]` (`auth/bootstrap.rs:94`).
- `test-helpers` is a leaf feature that **no workspace member enables**:
  `zeroship-cli/Cargo.toml:20` and `zeroship-worker/Cargo.toml:20` both take
  `zeroship-plugin-db = { workspace = true }` with no `features`.
- Nothing in `db/migrations-ts/` creates the schema either.
- Yet `load_pg` (`crud/mask_policy.rs:278-300`) is **ungated** and issues
  `SELECT __zeroship_admin.get_mask_policy($1)::text`, reached from
  `ensure_mask_policy_cached` (`crud/unmask.rs:335`) on the `dispatch_unmask`
  path, and `persist_pg` (`:280`) is reached from `env.db.setMaskPolicy`.

So on PostgreSQL - the backend that ships - every `unmaskField` call queries a
function in a schema that cannot exist.

**It fails CLOSED, and that is stated deliberately rather than accidentally.**
`unmask.rs:331-334`: "Errors propagate ... the unmask flow then rejects with the
typed error instead of silently default-denying - operators see the real fault."
So this is **not** a PII leak. It is a feature that is entirely non-functional
on the production backend: a creator calling `env.db.unmaskField` on PG gets a
SQL error about a missing function, forever.

**Evidence:** `auth/bootstrap.rs:94`; `crates/zeroship-plugin-db/Cargo.toml:183`;
`crates/zeroship-cli/Cargo.toml:20`; `crates/zeroship-worker/Cargo.toml:20`;
`crud/mask_policy.rs:278-300`; `crud/unmask.rs:325-352`

**Why no suite caught it: there is no PostgreSQL mask-policy test at all.**
Every `dispatch_set_mask_policy` / `load_pg` / `persist_pg` exercise in the tree
is in `crates/zeroship-plugin-db/tests/sqlite_integration.rs` (`:6656`, `:6740`, `:6829`, `:6845`,
`:6896`, `:7686`, ...). `crates/zeroship-plugin-db/tests/integration.rs` - the
live-PG suite - has none.
The feature is tested only on the backend where it works and untested on the
backend that ships, so the arm is green and the product is broken.

This is L13's family (**a cap that is green and dead at once**) with the
polarity reversed: L13 was a guard that never ran, this is a feature that never
ran. Both were invisible for the same reason - the test exercised a path the
product does not take.

**Disposition.** Do not fix `load_pg` in place. Per the AGENTS.md invariant the
worker both reads *and* writes this state, so **it is not privileged** and
belongs in the app's own schema beside `__zeroship_audit_unmask` - which
`crud/unmask.rs` already argues for in prose. That relocation is decision 3's
work (mask policy becomes build-time and immutable at runtime), so L25 closes
with it rather than separately. **What must not wait is the test**: a PG
mask-policy arm should exist before the relocation, so the fix is proven against
a suite that would have caught the original.

#### ESCALATED 2026-08-27: `390f4b97b` turned this from latent into a live test failure, and my verification could not see it

**I deleted `ensure_admin_schema` while leaving both callers, and shipped it on
a green run.** On a PRISTINE database the tree at `390f4b97b` is **75 passed,
6 failed**:

```
encrypted_column_round_trip_randomised
encrypted_deterministic_equality_lookup
encrypted_randomised_row_swap_rejected
p4_round_trip_encrypted_masked_vector_via_introspected_metadata
p5_pg_crud_works_via_engine_created_schema_no_runtime_ddl
unmask_fetch_runs_under_per_app_role_via_rls
```

**The reasoning error is the part worth recording, because the fact was right
and the inference was wrong.** This entry establishes that `__zeroship_admin`
never existed in a *shipped binary*, because its only installer is
`#[cfg(any(test, feature = "test-helpers"))]` and no member enables that
feature. True. I carried it to "so deleting the installer costs nothing", which
does not follow: **`cfg(test)` is compiled when the tests run.** The suite
created the schema on every live-PG run and then used it. Removing the creator
while keeping `pg_admin_lookup_root` (`encryption/keys.rs:486-506`) and
`load_pg` (`crud/mask_policy.rs:300`) broke six tests. "Dead in production" and
"dead in the suite" are different claims and only the first was established.

**Why the 11-arm run reported green.** It pointed at a fixed, long-lived
database, `.../zeroship` on `zs-pilot-pg-5463`. That database still physically
carries `__zeroship_admin` - measured directly after the fact:

```
const_eq  ensure_publication  ensure_publication_and_slot  ensure_slot
get_column_key  get_mask_policy  init_session  reset_session
```

left over from runs before the deletion. The deleted callers still resolved
against it. **A suite that reads state its own subject created earlier cannot
detect the removal of that state** - the harness was structurally incapable of
failing here, in the same family as L27 (a target it declined to run) and L28 (a
feature no default build compiles).

**Fixed in the harness, and the guard is mutation-proved rather than assumed.**
`verify_impl.sh` now creates a fresh database per run (`zs_verify_$$`), drops it
on exit via `trap`, and then REFUSES to run if that database already contains
`__zeroship_admin`. Discrimination checked both ways: the check returns `1`
against the stale `zeroship` database and `0` against a fresh one, so it can
actually tell the two apart.

**And that fix was itself insufficient, which is the more useful lesson.** A
fresh DATABASE does not isolate a fresh CLUSTER, because **PostgreSQL roles are
cluster-scoped**. Measured while fixing this regression:
`unmask_fetch_runs_under_per_app_role_via_rls` failed with `CREATE ROLE ...
42710` on a leftover `p6a_unmask_login` that could not be dropped, because two
dependent objects lived in a DIFFERENT database on the same server. That
failure **masked the defect completely** - a role collision looks nothing like a
missing schema, so the arm was red for the wrong reason and would have been
"explained" by the wrong cause.

The harness now runs against a dedicated cluster (`zs-adminfix-pg-5471`), which
also closes the replication-slot hazard previously noted here as unfixed: slots
and roles are both cluster-wide, so the same instrument answers both. **The
generalisation worth keeping: per-database isolation is defeated by any
cluster-scoped object, and there are at least two.**

**FIXED 2026-08-27.** Both callers deleted rather than repaired, per the
operator decisions on per-column keys and on mask policy being compiled-in:
`pg_admin_lookup_root` and the whole `KeySource` enum collapse into
`LocalKeySource` (`encryption/keys.rs`, -163/+47); `persist_pg` and `load_pg`
go (`crud/mask_policy.rs`, -99/+48). `integration` **75/6 -> 82/0** and
`native_transaction` **13/1 -> 14/0**, with **zero tests deleted** and one
ADDED - `pg_declared_mask_policy_authorizes_unmask_without_durable_store`
(`integration.rs:6001`), because the behaviour that changed had no live-PG
coverage at all. The string `"PG getter call failed"` and its hint `"there is no
admin-schema getter any more"` are both gone: `390f4b97b` had made the error
message true while leaving the code false.

**One live `__zeroship_admin` reference survives on purpose**:
`backend/postgres.rs:1722`, `INSERT INTO __zeroship_admin.pitr_targets` in
`pitr_replay_impl`, `#[cfg(feature = "test-helpers")]`-gated with no caller
outside the crate. PITR is covered by neither operator decision and
`backend/mod.rs:1365` records it as still open, so it was left rather than
redesigned outside anyone's authority.

### L27 (FIXED 2026-08-27 in `a0074e154`) - `distributed_live` fails on `main`, and the 11-arm harness cannot see it

**NEW - found 2026-08-27 while verifying the `__zeroship_admin` deletion.
PRE-EXISTING: attributed by control run, not by argument.**

`db_live_stream_crosses_v8_isolates_and_releases_worker_slot` fails:

```
distributed live exercise failed: anchor readiness failed:
  status=500 body={"message":"internal error","name":"Error","request_id":"1"}
panicked at crates/zeroship-plugin-db/tests/distributed_live.rs:796:41
```

**Attribution, by control differing in ONE variable.** Suspected of being caused
by the deletion (`390f4b97b`), it is not:

| tree | database | result |
| --- | --- | --- |
| `390f4b97b` (post-deletion) | shared `zeroship` | FAILED, 0 passed, 1 failed |
| `390f4b97b` (post-deletion) | freshly created `zs_dl_clean` | FAILED, identical message, 4.17s |
| `8c6caa465` (**pre**-deletion) | same `zs_dl_clean` | **FAILED, identical message, 4.21s** |

The clean-database arm rules out cross-suite contamination in the shared server;
the pre-deletion arm rules out the deletion. Corroborating: `distributed_live.rs`
contains **zero** references to `ensure_admin_schema`, `__zeroship_admin`,
`mask_policy`, `unmask` or `encrypted` - the deletion's entire subject is absent
from the test.

~~**Prime suspect, NOT yet confirmed:** `40c3df95f` ("feat(db): own env.db
configuration in a process-wide DbService") is the most recent commit touching
this file, and a change to how `env.db` is configured is the right shape to
break an anchor app's boot with a 500.~~

**REFUTED BY RUN, 2026-08-27.** `40c3df95f` is innocent: built and ran the
target at its parent `090a756d2` and it fails with the identical error. The
"most recent commit touching the file" heuristic picked the wrong commit, which
is worth noting because it is the heuristic everyone reaches for first.

**The verbatim error behind the generic 500**, which the test could not show
because it only sees `{"message":"internal error"}`:

```
ERROR zeroship_plugin_db::cdc_lifecycle: db CDC failed to start; refusing live subscription
  error=replication: publication __zs_pub_56971a71bc57dd61d565f1467c7f is missing for app ...
  error.code=Some("replication_publication_missing")
```

`await held.ready()` reaches `cdc_lifecycle::ensure_ready` ->
`PgChangeStream::spawn_consumer` -> `replication::ensure_worker_slot`, which
fails closed at `replication.rs:154-190`. **The test never creates the
publication its own CDC path requires.**

**First bad commit: `2a44ea8ef` ("fix(worker): constrain database authority",
2026-08-16).** Before it, `ensure_publication_and_worker_slot` created the
publication itself (`2a44ea8ef^:replication.rs:184`); after it,
`ensure_worker_slot` treats the publication as a hard precondition
(`replication.rs:175`) because ownership moved to `zeroship-migrated`. That
commit updated `crates/zeroship-plugin-db/tests/integration.rs` (+98/-44, adding
`c1_create_publication_for_tables`) and **did not touch
`crates/zeroship-plugin-db/tests/distributed_live.rs`**. `git log -S "replication_publication_missing"`
returns exactly one commit, and nothing since has touched it. **The target was
silently red for eleven days.**

Attribution is source-level rather than fully empirical, and the limit is worth
recording: no commit older than 2026-08-26 can be built here, because
`third_party/zero-migrate` was a submodule pinned at `cb1bcb59` until
`ccb5a7edc` and that object exists in no local clone. The empirical bound is
"already broken at `090a756d2`".

**FIXED 2026-08-27, and not by weakening the test.** The old assertion was "app
deletion must drop the shared publication" - which today's worker is *forbidden*
to do (`drop_namespace.rs:29-34`; `service.rs:392-409` drops slots only; and
`replication.rs:602-627` is an in-crate unit test that reads
`ensure_worker_slot`'s own source and fails if it contains publication DDL). The
smallest green-making change was deleting that assertion. Instead it is
**inverted**: the test creates the publication as `zeroship-migrated` would,
then asserts it SURVIVES `deprovision_app`, so re-adding publication DDL to the
worker path turns the arm red. The publication is created `FOR TABLE ...events`
rather than empty, because an empty one would let the anchor become ready and
then starve the subscriber - converting a loud failure into a 20-second stall.

**The harness defect is the part that generalises, and it is worse than the
bug.** `tests/lib`-style arm counting did not miss this - `verify_impl.sh`
*deliberately excluded* the target, printing:

> `distributed_live   NOT RUN (needs --features live-db-tests + a multi-node fixture)`

and then printing **`all arms green`** as its final verdict. Both statements
were true and the combination was misleading: an omission that is *named* is
legible, not safe, because the summary line does not carry the caveat. This is
the same shape as the four gates found in August examining nothing and printing
exactly what a clean tree prints - the difference is only that this one
documented its blind spot.

`tests/run_plugin_db_live_suite.sh` runs the target and caught it, which is why
the failure surfaced at all. **Fixed 2026-08-27**: `verify_impl.sh` now runs
`distributed_live` as a twelfth arm with floor 0, expected red until this is
fixed - the same treatment `missing_role` received while it was failing. A red
arm you can see beats a skipped arm you cannot.

**Second-order finding: `PLUGIN_DB_MIN_PASSED=118` is now red for two unrelated
reasons at once** - this failure, and the 15 integration tests the deletion
legitimately removed (measured: 98 passed, 1 failed). Decrementing the floor to
match while L27 is unfixed would bury a live failure inside an accounting
change, which is exactly what the ledger at `tests/run_plugin_db_live_suite.sh:92-142`
exists to prevent. **The floor must not move until L27 is resolved.**

### L6 (CLOSED 2026-08-27) - `__zeroship_admin` has no production provisioner

**CLOSED 2026-08-27 by decision 7. There is no admin schema to provision.**
`__zeroship_admin` is deleted entirely - not reduced to one table - because the
runtime descriptor becomes the sole schema authority and the epoch it held has
no job. A defect that says "X has no provisioner" is closed by deleting X as
surely as by building one, and this is the second closure of that shape today
(the reclassified `get_column_key` finding is the first).

*(This heading read **SCOPE REDUCED** for a few hours, under decision 6, which
kept one table. Decision 7 supersedes it. The reduced-scope text below is
retained because it enumerates what is being deleted and by which decision,
which is the useful content either way - only its conclusion, that a
one-table provisioner still had to be written, is wrong.)*

`__zeroship_admin` has no production provisioner, yet production code calls its
functions and propagates the error.

**Evidence:** `bootstrap.rs:95-96`; `mask_policy.rs:268,286`; `keys.rs:495`
names a migration that does not exist

**SCOPE REDUCED 2026-08-27 by decisions 1, 2, 3, 5 and 6, and the reduction is
most of the entry.** This was filed as "there is no production provisioner for
the admin schema", sized against what the test-gated installer builds: **6
tables and 13 routines**, 12 of them `SECURITY DEFINER`
(`auth/bootstrap.rs:181-195` for the tables; the routines are `get_mask_policy`,
`set_mask_policy`, `get_column_key`, `const_eq`, `sign_session`,
`verify_signature`, `init_session`, `reset_session`, `rotate_session_keys`,
`ensure_publication`, `ensure_slot`, `ensure_publication_and_slot` and
`watchdog`).

The provisioner it now names creates **one schema, one table, no functions**.
Every table is deleted - `column_keys` (decision 1), `pitr_targets` (decision 2),
`mask_policies` (decision 3), `hmac_keys` / `session_ctx` / `session_nonces`
(decision 5) - and `app_schema_state`, which does not exist yet, is the only
resident. `publish_schema_state` is the writer, called by the migration service
rather than the worker, so it is not one of the routines being deleted.

**The three production callers in the evidence line resolve by deletion, not by
provisioning**, which is the part that changes what this entry means.
`mask_policy.rs:268` issues `SELECT __zeroship_admin.set_mask_policy($1, $2::jsonb)`
and `:286` issues `SELECT __zeroship_admin.get_mask_policy($1)::text`; the
`get_column_key` call itself is `keys.rs:485`, and `:495` - the line this entry
cites - is the error **hint** telling operators to "run `__zeroship_admin`
bootstrap migration", which is the migration that does not exist. All of them
call functions that no longer exist in any shape, so the "production code calls
its functions and propagates the error" half of this defect ends with the
callers, not with a provisioner that finally satisfies them. An implementer who
reads this entry as "stand up the schema so these calls succeed" would build the
thing five decisions just removed.

**And the schema is proposed for rename** (decision 6): `__zeroship_admin`
described a drawer of platform powers, and one epoch row is not that. The name
is the operator's to pick; the design document lists candidates and recommends
`__zeroship_schema_state`.

### L23 (FIXED 2026-08-27 in `8c6caa465`) - dev-mode SSRF validation is bypassed from an ambient env read

**FIXED, and the hole was wider than this entry describes.** Dev-ness is now a
typed input written only by `set_dev_mode`, whose sole caller is `cmd_serve`
(`crates/zeroship-cli/src/main.rs:96`), so `ZEROSHIP_DEV` cannot reach the gate
at all. The relaxation is narrowed from "skip all host/IP validation" to
loopback only.

**Two more bypasses on the same flag, neither named here**, both found by
looking past `ssrf.rs`:

- `transport/client.rs` built the HTTP client with **no resolver at all** in dev,
  so the DNS layer was bypassed too - narrowing only the string check would have
  left `fetch("http://metadata.internal/")` connecting.
- `transport/egress.rs` skipped the platform floor entirely, breaking its own
  named invariant GRANTS-NARROW: a creator ACCEPT rule naming `169.254.169.254`
  was admitted. That path serves `node:net`, `node:tls` and outbound WebSocket.

A fix closing only the first would have READ as closed. `resolver_for` now
returns `SsrfResolver`, not `Option<SsrfResolver>`, so "this mode gets no
resolver" is unrepresentable without changing the signature.

**A pointer entry, not a moved one.** The full argument lives in SC-4's decision
that dev-ness is a typed input derived from the runtime's identity, never an
ambient environment read. Extracting it would gut that decision, which is stated
as a correction of an earlier draft's invariant and needs its own reasoning
intact. It is recorded here because a live bypass in shipped runtime code was,
until 2026-08-27, in **no** register at all. This follows the convention L9
already uses for SC-6.

`validate_url` returns `Ok(())` before any host or IP check whenever dev mode is
on:

> `// In dev mode, skip host/IP validation (allows localhost fetch to Vite).`
> `if dev_mode_enabled() { return Ok(()); }`

and `dev_mode_enabled()` resolves a process-wide cell from
`declared_env!(dev, "ZEROSHIP_DEV", ...)`. An environment variable is not a
construction boundary. The tree already states the opposite standard in the very
place a leak would land: "The authority is the worker's identity, not an env
flag: SQLite is refused even if `ZEROSHIP_DEV=1` leaked into a prod worker". The
SSRF gate does not meet it.

**Evidence:** verified 2026-08-27.
`crates/zeroship-runtime/src/transport/ssrf.rs:206-207` (the bypass), `:188`
(`validate_url`), `:66` (the `declared_env!` read);
`crates/zeroship-worker/src/main.rs:112-113` (the standard it fails to meet).
Argument and acceptance arm: SC-4.

### L11 (FIXED 2026-08-27 - FTS deleted and MERGED) - PostgreSQL full-text search has no producer anywhere in the tree

**MERGED, so the "not yet merged" in this heading was stale. Verified
2026-08-27 against `feat/dbbind-impl` at `196622c9b`:** `61298897b`
("fix(db): remove full-text search") and its merge `93bc20126` ("Merge branch
'feat/db-delete-fts'") are both ancestors of HEAD, and
`to_tsvector|plainto_tsquery|websearch_to_tsquery|tsquery` now occurs **0
times** across `crates/zeroship-schema/src/` and
`crates/zeroship-plugin-db/src/`. The removal is complete in the data plane,
not merely landed on a side branch.

Note the commit hashes below (`b3fa01659`, `4ee6b70dd`, `49e579293`) are the
pre-merge ones from `feat/db-delete-fts`. `b3fa01659` still **resolves as an
object** - `git log -1 b3fa01659` prints "fix(db): remove full-text search" -
but it is **not an ancestor of HEAD**; the content arrived here as
`61298897b`. That is the same trap as the `f44bcf6b6` errors flagged in the
header, and it is worth stating in this precise form: the hash is not dead, so
every command that merely *reads* it succeeds and looks authoritative. Only
`git merge-base --is-ancestor <sha> HEAD` distinguishes the two cases. The
measurement below still stands; the hashes are history, not a baseline anything
should be re-verified against.

> **DECIDED (operator, 2026-08-27): delete full-text search.** Not restore a
> PostgreSQL producer. The migration engine had already removed FTS
> deliberately; this finishes the removal in the three layers that still
> advertised it.
>
> **Implemented on `feat/db-delete-fts`**, three commits (`b3fa01659` the
> removal, `4ee6b70dd` a sync merge, `49e579293` a guard against the deleted
> migration mirrors reappearing). Two whole files deleted
> (`backend/sqlite/fts.rs` 547 lines, `zeroship-schema/src/fts_sqlite.rs` 398)
> plus references across `query.rs`, `sdks/db/src/types.ts`, the vite-plugin
> renderer, `docs/reference/db.md`, the divergence table, and four `.fts()`
> calls in `examples/db-e2e`.
>
> Verified on that branch: lib 630/0, integration 89/0/6, native_transaction
> 13/0, sqlite_integration 121/0, clippy exit 0, `pnpm build` exit 0. The lib
> count falls because FTS deletion removed 14 tests while a merged cache commit
> added five.
>
> **`db/` is untouched and `db/released_migrations.tsv` is untouched** - checked
> directly, because a migration a deployed database has applied is frozen and
> editing one makes the runner refuse every later run with `ChecksumMismatch`,
> permanently. The checksum goldens that did change are three engine *test*
> fixtures, which is the right place for an IR shape change.
>
> **NOT YET MERGED**, so 14 files in the working branch still carry the symbol.
> This entry retires when that merge lands and is verified, not before.
>
> Residual risk the implementer named and I am keeping: removing
> `engine_goodie_ddl` changes migration IR/checksum contracts, so an unknown
> out-of-tree consumer could still depend on it; no external deployed database
> was queried; and stale local SQLite FTS5 tables are deliberately left behind
> rather than migrated away, since this platform is pre-launch.


**NEW - found while unblocking `pnpm build`; NOT caused by this design**

**PostgreSQL full-text search has no producer anywhere in the tree, while three
layers still present it as a feature.** The migration engine removed FTS
outright - "Full-text support was removed from this engine, down to the
`IndexMethod` variant... There is no `.fts()` facet to fold: the authoring
surface has none, and no code path here produces either shape"
(`zeroship-migrate-core/src/render/declarative.rs:1823-1830`), and a second site
confirms "no `fts5` sentinel, no `.fts()` facet" (`:2971-2976`). It previously
folded `.fts()` into a `__fts` GENERATED `tsvector` column plus a GIN index on
PostgreSQL. Nothing replaced it: `tsvector` appears in the whole tree only in
that removal comment, and there is no producer in plugin-db's PostgreSQL
backend. Meanwhile `t.string().fts(language?)` is still callable and documented
in `@zeroship/db` (`sdks/db/src/types.ts:1172`),
`docs/reference/sqlite-divergences.md:14-15` documents PostgreSQL FTS as working
and merely *differing* from SQLite ("`language` selects the `tsvector`
configuration"), and SQLite full-text still works because plugin-db's runtime
creates the FTS5 table itself (`backend/sqlite/fts.rs`) on a path that never
touches the engine. So the divergence table describes a comparison between a
working backend and a non-existent one. The removal's stated rationale cites
`docs/proposals/fts-macro.md` - **DELETED or never landed; it is not in the tree**.

**Evidence:** `zeroship-migrate-core/src/render/declarative.rs:1823-1830,2971-2976`;
`sdks/db/src/types.ts:1172`; `docs/reference/sqlite-divergences.md:14-15`;
`backend/sqlite/fts.rs`; DELETED or never landed: `docs/proposals/fts-macro.md`

L11 is listed here because it was found by implementation work on this design
and because it is a live user-facing gap, not because this design causes or
fixes it. It needs its own decision - restore an FTS producer, or remove
`.fts()` from the DSL and correct the divergence doc - and that decision is not
this document's to make. What is worth carrying across, though, is the shape:
the capability was deleted in one layer and left standing in three others, and
each of those three reads as evidence that it works. Nothing was lying; every
layer was locally consistent.

### L12b (FIXED 2026-08-27 in `b50691f2c`) - a crashed worker's abandoned slot can take down the whole cluster

*(**Heading status added 2026-08-28, and the omission is the finding.** This
heading carried no status while the first line of its body said FIXED, so the
outline reported it as live - the identical fault this document set records
against itself at L13, L14 and L15, and again at L23, L25 and L27, both times
with the conclusion that the heading IS the index. It recurred a third time and
was still there a day later. The register's own live tally
inherited it and listed L12b among the eleven live entries, which is how a
brief written off that tally asked for L12b to stay in the live file.*
*One section of this entry did stay: the `max_slot_wal_keep_size` mitigation it
recommends has NOT landed - `deploy/compose/docker-compose.yml` still sets only
`wal_level` and `max_prepared_transactions`, checked 2026-08-28 - so that
section moved verbatim into the live register as a subsection of L12. The
defect below is closed; the one-line config change it asks for is not.)*

**Availability, same finding - FIXED 2026-08-27 in `b50691f2c`. Re-verified
against the tree, not against this entry.**

**The defect as filed.** The clean path drops the slot when the last lease
goes, but a crash leaves `active=false` with `restart_lsn` pinned - PostgreSQL
then cannot recycle WAL past it and `pg_wal` grows without bound, a
**cluster-wide** failure affecting every tenant on it, caused by one tenant's
worker dying. A reaper existed but was reachable **only from tenant JS** via
`db.replication.dropAbandoned()`, so operator-side cleanup of an operator-side
failure was delegated to the tenant, who has no reason to run it.

**What shipped instead.** `fix(db): reap crashed worker replication slots`
added `crates/zeroship-plugin-db/src/slot_reaper.rs` (+593) and `crates/zeroship-worker/src/slot_reaper.rs`
(+54), wired at `worker/src/main.rs:641`, and **deleted** the tenant surface -
196 lines out of `replication.rs`, 89 out of `v8_classes/replication.rs`, and
the `internal.d.ts` declaration. `drop_abandoned_slots` now appears **zero**
times in the tree, so every evidence citation this entry originally carried is
dead.

The shipped design is stronger than "call the reaper from the operator side":

- `OperatorSlotReaper::sweep` runs a periodic cluster catalog sweep from the
  worker process, keyed on an inactivity threshold rather than on any tenant
  action.
- Each sweep is bounded by a `SWEEP_DEADLINE` (30s) held **below** the sweep
  cadence, and the reaper holds a **liveness lease**. Its own comment states
  the reason: a maintenance connection that stops making progress must release
  that lease and fail the worker, "otherwise peers could eventually mistake its
  CDC slots for abandoned while the worker keeps serving requests." The reaper
  is thus safe against the failure mode a reaper introduces - reaping a live
  worker's slots.
- `run_server_with_slot_reaper` selects over the server and the reaper task
  together, so a dead reaper takes the worker down rather than leaving it
  serving with cleanup silently stopped.

**Evidence:** `worker/src/main.rs:15,48-66,639-641,714`;
`crates/zeroship-worker/src/slot_reaper.rs:1-40`; `crates/zeroship-plugin-db/src/slot_reaper.rs:278-452`;
`grep -rn "dropAbandoned\|drop_abandoned" crates/ sdks/` returns nothing

**How this entry went stale, which is the reusable part.** It asserted "verified
2026-08-27 that no control-plane or worker code calls it" - and the fix landed
2026-08-27 at 05:04. The verification was real when performed and the entry kept
reading as current afterwards, because nothing re-runs a citation once it is
written down. A new measured paragraph was even appended to this section an hour
after the fix landed, attached to a defect description that was already false.
The tell was available the whole time and not looked at: `git log --grep` for
the subsystem name.
### L14 (FIXED 2026-08-27 in `92bd67633`) - `MAX_INSERT_MANY_BATCH` caps documents while the wall it cites counts binds

**NEW - a wide `insertMany` fails, and the comment says it cannot, verified
2026-08-27**

**`MAX_INSERT_MANY_BATCH` caps documents while the wall it cites counts binds.**
The constant is `1_000` documents and its comment justifies that number as
staying "well under Postgres' 65535-bind-param wall"
(`zeroship-schema/src/query.rs:597-600`). But `build_insert_many_with_dialect`
pushes **one bind per non-null cell** (`query.rs:4090-4114`; NULLs are inlined
as SQL literals and cost no bind). So a full batch of 1000 documents with 66
non-null columns each is 66,000 binds. The limit is exactly 65535 and is
enforced **client-side, as an error rather than a truncation**:
`postgres-protocol` (resolved to 0.6.12 in `Cargo.lock`, verified against that
exact version) does `let count = u16::from_usize(count)?` in `write_counted`
(`message/frontend.rs:108`). A legitimate `insertMany` of 1000 wide documents
therefore fails in the driver with a parameter-count error. **The threshold is
66 non-null columns per document at a full batch**, and nothing in the builder
counts binds. The cap and the wall are in different units.

**Evidence:** `zeroship-schema/src/query.rs:597-600`, `:4090-4114`;
`postgres-protocol-0.6.12/src/message/frontend.rs:108`; version taken from
`Cargo.lock`

~~**Still open, and the citation has drifted.** Re-verified 2026-08-27 against the
tree at `f44bcf6b6`: the constant is unchanged at `1_000` and now sits at
`zeroship-schema/src/query.rs:601`, with the justifying comment at `:598-600`.
The index reported this closed; it is not.~~

**FIXED in `92bd67633` ("fix(db): enforce insertMany bind budgets", 2026-08-27
05:00), which IS an ancestor of `196622c9b`. The "still open" verdict above was
measured on the wrong tree.** The fix is exactly the one this entry argued for -
a separate cap in the right unit, not a smaller document count:

- `POSTGRES_MAX_BIND_PARAMETERS = u16::MAX as usize` and
  `SQLITE_MAX_BIND_PARAMETERS = 32_766` now exist as their own constants
  (`zeroship-schema/src/query.rs:602-603`), per dialect.
- `MAX_INSERT_MANY_BATCH` stays `1_000` (`:601`) and its comment no longer
  claims to bound binds. It now says the document count "says nothing about row
  width" and that bind parameters "are capped separately" (`:598-600`) - the
  false protection claim is retracted at the site that made it.

Note which half of the defect mattered: the number was never the bug. The
comment was, and the fix that counts is the one that stopped the comment
claiming a guarantee the constant could not provide.

L14 belongs to the family this document keeps returning to: **a comment that
reads as protection**. The constant is not merely too large - it is measured in
the wrong unit for the guarantee its own comment claims, so no value of it makes
the claim true. A bind-aware cap is a different computation, not a smaller
number. It is also a good argument for the IR: a plan that knows its own bind
count can refuse or chunk before the driver does, and can say so in the
creator's vocabulary rather than as `parameter count out of range`.

### L15 (FIXED 2026-08-27 in `d49930f34`) - `updateMany` can partially apply and then report failure

**NEW - verified 2026-08-27**

On a collection with a randomised-encrypted column, `updateMany` takes a per-row
path with **three** compounding defects. (a) **Uncapped SELECT**:
`resolve_target_row_ids(&route, &coll, &filter, None)` (`crud/mod.rs:1273`)
passes `None` as the limit, and the builder emits `LIMIT` only `if let Some(lim)
= limit` (`zeroship-schema/src/query.rs:3161-3163`), so the entire matching set
streams into the worker heap. (b) **A comment says this cannot happen**: DB-2 at
`crud/mod.rs:618-620` states an omitted limit "defaults to `MAX_QUERY_LIMIT` -
never 'no LIMIT' (which would stream the whole collection into the worker)".
That guard is real but lives in `dispatch_find`'s option parsing;
`resolve_target_row_ids` never traverses it. `dispatch_update_one` passes
`Some(1)` and is fine - `updateMany` is the only uncapped caller. (c) **Per-row
autocommit with no rollback**: the loop runs `exec_mutation_with_emit(...).await`
once per row (`crud/mod.rs:1350`) and on `Err` **returns immediately** (`:1354`).
Outside an explicit `db.transaction()` each row has already committed
independently, so the creator gets a rejection for an operation that **partially
applied**. There is no compensation and no indication of how far it got.

**Evidence:** `crud/mod.rs:1273`, `:618-620`, `:1307-1362`;
`zeroship-schema/src/query.rs:3161-3163`

~~**Still open, and the citation has drifted.** Re-verified 2026-08-27 against the
tree at `f44bcf6b6`: the uncapped call is now `crud/mod.rs:1279` and still
passes `None`; the DB-2 comment is now `crud/mod.rs:620-622`. The index reported
this closed; it is not.~~

**FIXED in `d49930f34` ("fix(db): make encrypted updateMany atomic and
bounded", 2026-08-27 05:00), which IS an ancestor of `196622c9b`. The "still
open" verdict above was measured on the wrong tree.** All three parts are
closed, and the fix took the option this entry said was mandatory:

- **(a) the uncapped SELECT is capped.** `resolve_target_row_ids` is now called
  with `query::MAX_QUERY_LIMIT + 1` (`crud/mod.rs:1289-1294`), not `None`. The
  `+ 1` is the detection margin: if the result exceeds `MAX_QUERY_LIMIT` the
  call is refused with a typed `update_many_target_limit_exceeded` naming the
  maximum and telling the creator to narrow the filter
  (`crud/mod.rs:1298-1311`). It refuses rather than silently truncating, which
  is the correct choice for a write.
- **(c) the per-row fan-out is atomic.** The whole loop now runs inside
  `transaction::AtomicWriteFrame::begin(route)` (`crud/mod.rs:1278`), and the
  accumulated `work_result` is handed to `frame.finish(work_result)`
  (`crud/mod.rs:1388`), so a mid-loop `Err` rolls the prefix back instead of
  leaving it committed. The register demanded "one transaction or return how
  many rows it committed"; the implementation took the first.
- **(b) the DB-2 comment is no longer contradicted**, because the path it
  described as impossible is now the path the code takes.

This was the entry's own priority ordering vindicated: the availability halves
(a) mattered less than the correctness half (c), and (c) is the one that got a
transaction rather than a bound.

L15's third part is the one that matters most and is easiest to miss behind the
first two. Unbounded memory is an availability problem; **a bulk write that
half-applies and reports failure is a correctness problem**, and it is
indistinguishable to the caller from one that applied nothing. It also
interacts with L8: a creator who retries a rejected `updateMany` re-applies the
prefix that already succeeded. Any per-row fan-out this design keeps must either
run inside one transaction or return how many rows it committed before failing -
silence is the one option that is not available.

### L13 (FIXED 2026-08-27) - `MAX_SUBSCRIPTIONS_PER_APP` is enforced on zero production paths

**NEW - a cap that is green and dead at once, verified 2026-08-27**

**`MAX_SUBSCRIPTIONS_PER_APP = 256` is enforced on zero production paths.** It
is checked only inside `Broker::try_subscribe` (`broker.rs:490`), and the single
production mint site calls the **infallible** `broker::subscribe(app_id,
collection)` (`v8_classes/subscription.rs:315`). Verified by grep:
`try_subscribe` occurs only in `broker.rs` itself and in doc comments. So
`for(;;) db.users.openSubscription()` is unbounded, and each iteration allocates
a `DEFAULT_QUEUE_DEPTH = 1024`-slot event queue plus a CDC lease plus an entry
in the **process-global** routing table that every publish walks - so one tenant
degrades every co-resident tenant in the process. The code documents its own gap
at `broker.rs:452-454` ("New SDK call sites should prefer `try_subscribe`"), and
the one new SDK call site does not. **The test passes because it calls
`try_subscribe` directly**, which is the cannot-fail arm class exactly: the cap
is green and dead simultaneously.

**Evidence:** `broker.rs:144,151,452-454,490`; `v8_classes/subscription.rs:315`

~~**Still open, re-verified 2026-08-27 against the tree at `f44bcf6b6`.** The
production mint site is now `v8_classes/subscription.rs:314` and still calls
`broker::subscribe`; the cap is still `broker.rs:151` and still reached only
from `Broker::try_subscribe` at `broker.rs:490` and the free `try_subscribe` at
`broker.rs:856`. The index reported this closed; it is not.~~

**FIXED, and the "still open" verdict above was measured on the wrong tree.
Re-verified 2026-08-27 against `feat/dbbind-impl` at `196622c9b`:**

- The production mint site `mint_subscription` now calls the **fallible**
  `broker::try_subscribe(app_id, collection)` and matches on its `Result`
  (`v8_classes/subscription.rs:316`). It is no longer the infallible
  `broker::subscribe`.
- The cap is reached from that path: `broker.rs:522` returns
  `DbError::Coded { code: "subscription_limit", .. }` once `live >=
  MAX_SUBSCRIPTIONS_PER_APP` (`broker.rs:151`, still 256).
- The count is taken **after** pruning closed handles (`broker.rs:511-518`),
  which closes the open/close loop the entry describes: an app cannot grow the
  global routing table by cycling subscriptions under the cap.
- The call ordering is itself guarded - `subscription.rs:423-457` asserts at
  test time that no `?` operator precedes the `broker::try_subscribe(` call in
  `mint_subscription`, so the cap cannot be re-orphaned by an early return.

So the cap is now green *and* live. The index was right and this entry was
wrong.

### L10 (FIXED 2026-08-27) - the deploy-invalidation token is keyed one level coarser

**FIXED 2026-08-27, `4c8e84134`; regression test proven red by mutation**

The deploy-invalidation token is keyed **one level coarser than the isolates it
protects**, and a misleading name hides it. `deploy_tokens: HashMap<String,
String>` is `app_id -> token` (`context.rs:296`), but the worker deliberately
keeps several isolates *of the same app at different deploys* alive on one
thread, keyed `PinnedWorkflowKey { app_id, deploy_hash }`
(`worker/src/cache.rs:28-32`) for deploy-pinned workflow replay - a documented
invariant, not an accident. Every runtime overwrites the single shared entry
when it mints its `Db` wrapper, so it is **last-writer-wins across deploys of
one app**: mint the pinned runtime last and the *current* runtime reads the old
token; mint the current one last and the pinned replay reads the new one.
`runtime_schema_for` then serves or repopulates introspected metadata under the
wrong deploy's key. The same coarseness lets a redeploy skip installing new
declared hints, because `registered_models` is keyed `(app, collection)` with no
deploy component and registration fast-returns on the stale mark. **What makes
this hard to see is a name:** the struct is `IsolateDbContext` and its comment
says "the per-isolate DB context", but it lives in a `thread_local!`
(`context.rs:952-957`) and the worker runs many isolates per thread. Keying by
`app_id` inside it would be correct if the name were true. The token must carry
the full binding identity `(app_id, deploy_hash)` - which `PinnedWorkflowKey`
already establishes one layer up - and the active binding must carry it, not
recover it from thread-global app state.

**Evidence:** `context.rs:296` (key shape); `context.rs:952-957`
(`thread_local!` vs "per-isolate"); `worker/src/cache.rs:28-32` (the finer key);
`v8_classes/db.rs:313-330` (the overwrite); `crud/introspect_schema.rs:80-90`
(the read)

**All four per-app maps were re-read for keying, 2026-08-27.** None carries
deploy identity in its key: `deploy_tokens` is keyed by `app_id` alone;
`schemas` (declared) and `registered_models` by `format!("{app_id}:{collection}")`;
`introspected_schemas` by the same string, with the deploy token stored as part
of the *value* so staleness is detectable - but the token it compares against is
the app-keyed one, which is the bug. So `introspected_schemas` is the only map
that can even notice a deploy change, and it notices it against a value L10
shows is wrong.

**What the fix does, and what it deliberately leaves.** A new `DbBinding`
(`crates/zeroship-data-core/src/binding.rs`) captures `ZEROSHIP_DEPLOY_ID` from
the **active runtime's own environment** at `mint_db`, is stored immutably on the
`Db` and every `Collection` it mints, and is threaded through each asynchronous
CRUD continuation. The token is therefore never recovered from process-global
environment or app-keyed thread-local state. `IsolateDbContext` is renamed
`ThreadDbContext`, which removes the false name that hid the bug.

**The fix rests on one assumption the implementing agent flagged as unverified,
and it holds** - checked here rather than taken on trust: `build_runtime`
injects `ZEROSHIP_DEPLOY_ID` per runtime (`worker/src/cache.rs:433-435`), and
`load_pinned_workflow_app` passes `Some(deploy_hash)` - **its own** pinned hash,
the same one it keys `PinnedWorkflowKey` with - rather than the current deploy's
(`worker/src/cache.rs:524-540`). Had that been false the whole fix would be
inert, so it is the right thing to have named.

**Still app-keyed after this change**, and these are exactly the maps the
"cannot be re-keyed alone" note below predicted: `registered_models`, the
declared `schemas` cache, the SQLite declared-schema fallback, and mask-policy
caching. Only introspected runtime metadata is deploy-bound today. The class is
not closed; one member of it is.

**The identity is under-specified in a SECOND dimension: the database itself.**
`clear_pool` drops the pool and backend and nothing else (`context.rs:478-482`);
all four metadata maps survive it. So a `set_db_url` swap leaves
`introspected_schemas`, `schemas`, `registered_models` and `deploy_tokens`
holding metadata introspected from the **previous database**, served under the
same `(app, token)` key. Column types, encryption `keyId`/`wraps` and mask
classifications from database A would then be applied to rows in database B.

Scope it honestly, because the reviewer who found it did: this is **unreachable
on the production worker**, where the URL is fixed for the process lifetime. It
is latent for the CLI and dev vectors, and it becomes reachable the moment any
multi-URL work lands. Recorded not as a live exploit but because it shows the
shape of the error: the cache key answers "which app, which collection" and
gestures at "which deploy", while the thing that actually determines what the
metadata *describes* - which database it was read from - appears in the key not
at all. A fix that adds only the deploy component leaves this one standing.

**The misleading name is systematic, not one comment.** Besides
`IsolateDbContext` itself, `is_model_registered`'s doc says the model "has been
registered on **this isolate**" (`context.rs:557`) - same false claim, same
thread-local. Anyone auditing this file for the L10 bug reads three independent
assurances that the state is per-isolate, and all three are wrong in the same
direction. Renaming the type is therefore not cosmetic cleanup bundled into a
fix; it removes the thing that made the bug survive review.

**`registered_models` cannot be re-keyed on its own.** A review pass looking at
the obvious follow-up found that the declared `schemas` cache is keyed
`(app, collection)` with no deploy component either, so fixing only the
registration mark would still let one deploy's declared hints overwrite
another's - the same defect one cache to the left. This matters for scoping the
fix: L10 is not "add a deploy component to `deploy_tokens`", it is **carry the
active binding identity through every per-app metadata cache**, and a change
that touches one of them has not fixed the class. That is also the argument for
the hierarchical `app -> deploy -> {collection -> ...}` shape proposed in the
design's cache-bound section rather than a per-map patch: one place to key
correctly instead of three places to keep in sync.

L10 is worth reading beside the "bound to the wrong thing" pattern this
document keeps hitting. Its history is a fix that moved in the right direction
and stopped one step short: the comment at `context.rs:281-296` explains, at
length and correctly, that the previous `std::env::var("ZEROSHIP_DEPLOY_ID")`
read was process-global and "would have been WRONG for a multi-app worker
thread". It then keys the replacement by `app_id` - which fixes multi-*app*
sharing and leaves multi-*deploy* sharing of one app, the case the worker
explicitly supports, still broken. The reasoning that identified the flaw was
sound; it was applied to one of the two dimensions the key needed.

### L8 (FIXED) - PostgreSQL answers `COMMIT` with a `ROLLBACK` tag

**FIXED - landed with its regression test; `exec_terminal_on_tx`,
`transaction/mod.rs:139`**

PostgreSQL answers `COMMIT` with a `ROLLBACK` tag for a failed transaction; the
driver detects it but plugin-db discards command tags, so a rolled-back commit
reports **success** and the settle path then drains pending emits for writes the
database discarded. **Scope the test to the explicit-transaction path** -
autocommit already goes through the driver wrapper that checks the tag, so a
test written there passes pre-fix and proves nothing.

**Evidence:** MEASURED: `BEGIN; SELECT 1/0; COMMIT;` -> server replies
`ROLLBACK`; control (clean tx) replies `COMMIT`. Driver detects at
`transaction.rs:186-188`; `backend/postgres.rs:201-212` returns `Ok(rows.len())`;
explicit `COMMIT` is routed there by `transaction/mod.rs:1001`

### L5 (FIXED) - the V8 decoder elides a filter key whose getter throws

**FIXED - decode is now total; `DecodeError` at `v8_bridge.rs:165`, no silent
`continue` survives**

The V8 decoder elides a filter key whose getter throws, so `updateMany` can lose
its tenant predicate.

**Evidence:** `v8_bridge.rs:250`
