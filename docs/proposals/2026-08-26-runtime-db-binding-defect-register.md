# Defect register: live defects in the code this design touches

Extracted from `2026-08-26-runtime-db-binding-design.md` on 2026-08-27, and
extended the same day when six further live-defect sections were lifted out of
that document (L17 through L22).

**Split on 2026-08-28, and this file is now the live half.** Fourteen closed
entries moved verbatim to
`2026-08-26-runtime-db-binding-defects-closed.md`, newest first, each with the
commit that closed it. Nothing was summarised away. That file is worth reading
on its own terms rather than being treated as an archive: **the closed entries
carry the most reusable material in the set, because what they record is not
"this was broken" but the mechanism by which each one stayed invisible** - a
verification pass run against a tree on no branch, a feature tested only on the
backend where it works, a harness that printed `all arms green` while naming the
target it had declined to run. The split exists so that this file's outline is a
list of what is still wrong, not a list a reader has to filter.

## How to read this register

**These are defects in EXISTING code**, found while designing and implementing
the runtime DB binding. They are not proposals and not design decisions. They
are listed together because several of them constrain the design, and because a
register that lives inside a design document makes both harder to read.

**Every entry was verified against the tree by the pilot**, not accepted from a
review. That distinction earned its place: several reviewer claims were correct
about the defect and wrong about its mechanism or its magnitude, and where that
happened the correction is recorded in the entry.

**Each entry is a heading, not a table row.** An earlier revision was one wide
three-column table of sixteen rows, and it had stopped rendering as one: a blank
line after the L9 row terminated the table, so the eight entries below it (L10
through L16, plus L12b) printed as literal pipes. Measured on that revision, the
longest single row was 2,319 characters (L9) and three more were over 1,400. The
status lives in the entry heading, so the document outline is the index and
there is no second copy of the status to go stale. The convention has earned
itself twice: listing the headings as an outline is what caught **two different
defects both numbered L22** (now L22a and L22b, see the blockquote at L22a),
which was invisible in prose, and it is what catches every stale heading below.

**This is the most perishable document in the set.** Entries retire as fixes
land. **Re-derive the tally by listing the `### L` headings; never by adjusting
the previous count.** That instruction is not style advice - every time it has
been ignored the count has drifted, and the drift is what the next section
records.

### Tally, re-derived 2026-08-28 from the headings of this file

**Fifteen live defect entries**, in the order they appear:

- **L1, L2, L9** carry DECIDED. An operator decision fixed the *shape* of the
  fix on 2026-08-27; none of the three has landed. Checked 2026-08-28 rather
  than assumed: `Symbol.for("@zeroship/db/MaskPolicyState")` is still at
  `sdks/db/src/policy.ts:87`, `_flushPendingMaskPolicy` is still exported at
  `sdks/db/src/internal.ts:83` and still drained at
  `sdks/bootstrap/src/runtime-entry.ts:196` and
  `sdks/bootstrap/src/dev-entry.ts:296`, and `dispatch_set_mask_policy` is still
  at `crates/zeroship-plugin-db/src/crud/mask_policy.rs:239`. **DECIDED is not
  FIXED**, and reading it as a retirement is the failure mode.
- **L17, L18** carry SUBJECT REMOVED. **That is a status, not a fix.** It means
  a decision deleted the code path the defect lives in, so the defect is **still
  live in `main`** and is closed by a deletion nobody has performed yet.
  **Reading it as "handled" is the failure mode; the entries say what must still
  happen.** L18 is additionally PARTLY FIXED: two of its three asks landed, the
  third did not.
- **L3, L4, L12, L16, L19, L20, L21, L22a, L22b, L28** carry no status and are
  live. L20's unbounded half is fixed and its stall is not, which its body says
  and its heading does not.

Plus **seven sections that are not defect entries** and are kept here because
they are neither live defects nor closed ones: the RETRACTED slot-exhaustion
narrative, `L12: what the ceiling of ten actually rests on`, `How L12 was nearly
missed`, the two reclassified-from-v3 findings, and the two constraints found
while writing up the 2026-08-27 operator decisions.

### Corrections applied at the 2026-08-28 split, and what was left alone

The tally this file carried until 2026-08-28 was wrong in three ways at once,
and all three are the same fault - a count maintained by adjustment rather than
by re-derivation:

1. **It contradicted itself four lines apart.** One paragraph said "seven carry
   FIXED (L5, L8, L10, L11, L13, L14, L15)" and the next said "Thirteen entries
   now carry FIXED". Thirteen was right.
2. **It listed L12b as live.** L12b's heading carried no status while the first
   line of its body said `FIXED 2026-08-27 in b50691f2c`, so the outline
   reported the opposite of the entry. **This is the third occurrence of that
   exact fault in this document** - it was caught at L13/L14/L15, caught again
   hours later at L23/L25/L27, and then recurred at L12b and survived a day. The
   status is now in the heading, in the closed file.
3. **It excluded L17 and L18 from every bucket**, having defined SUBJECT REMOVED
   in the same header as "still live in `main`". The header's own definition
   made them live and its own arithmetic did not count them.

The register also carried **two copies of the same finding** (the
re-derivation-caught-a-stale-heading paragraph, once naming L23/L25/L27 and once
naming L13/L14/L15) and **a sentence cut mid-paragraph** - "None would have been
found by reading more carefully. It / should never be read as design, and an
entry's absence from a later revision means it was closed, not that it was
wrong." The `It` had no referent. Both are gone; the surviving statement of the
convention is the section above.

**Left alone deliberately, and reported rather than fixed:**

- **L20's heading carries no status** while its body opens with "The unbounded
  half of this is FIXED as of 2026-08-27". The entry is genuinely live, so the
  heading is not *wrong* the way L12b's was, but it carries no signal either.
- **L11's heading says "FIXED ... and MERGED" while a blockquote inside it still
  says, in bold capitals, `NOT YET MERGED`.** The entry corrects this in the
  paragraph above the blockquote. It is in the closed file with the
  contradiction intact.
- **L6's evidence line cites `keys.rs:495`**, and L6's own body then explains
  that the call is at `keys.rs:485` and that `:495` is the error *hint*. The
  evidence line was never updated to match its own correction.

### Verification discipline for anything in this file

**State the branch, not just the SHA, and confirm the SHA is on it** with
`git merge-base --is-ancestor <sha> HEAD`. A dangling SHA reads exactly like a
current one: every command that merely *reads* it succeeds and looks
authoritative. The pass that established this rule re-verified thirteen entries
against `f44bcf6b6`, a tree that `git branch -a --contains` places on no branch
at all, and got three verdicts wrong in the same direction while quoting real
line numbers throughout. **That story is recorded in full in
`2026-08-26-runtime-db-binding-defects-closed.md`**, next to L13, L14 and L15,
which are the three entries it ruled on. Two `f44bcf6b6` citations remain in
this file, both in L18, where they are load-bearing rather than wrong -
`f44bcf6b6` is the commit that landed L18's partial fix.

**Treat every figure here as unverified unless the text says how it was
measured.** This document has been wrong about numbers in both directions: L22a
records a ~76 KB figure derived by multiplying a table that had already been
retracted, and the RETRACTED section below claimed a measurement it did not
have. Where an entry names its instrument - `tmp/measure_decode_multiplier.sh`,
`pg_settings.boot_val`, a version taken from `Cargo.lock` - the figure is as
good as the instrument. Where it does not, it is a number someone wrote down.
The closed file opens with a worked example of how a single `grep -c` produced
three different confidently-stated values, none of which named the set it
counted.

Read `2026-08-26-runtime-db-binding-00-index.md` first for what is settled, what
is open, and what blocks what. Closed entries, with the commit that closed each:
`2026-08-26-runtime-db-binding-defects-closed.md`.

---

## Live defects, to fix independently

These exist in `main` and do not depend on this design. Each needs a regression
test that fails on the pre-fix code.

*Entries below cross-reference L5, L6, L8, L10, L11, L12b, L13, L14, L15, L23,
L24, L25, L26 and L27. All fourteen are closed and live in
`2026-08-26-runtime-db-binding-defects-closed.md`; the numbering is unchanged,
so a reference resolves by searching for the same heading there. The
cross-references themselves were left untouched at the split rather than
rewritten to name the other file, because rewriting fourteen references inside
otherwise-verbatim entries is fourteen chances to change what an entry says.*

### L1 (DECIDED 2026-08-27) - mask policy forgeable from any bundled dependency

**DECIDED 2026-08-27 - the policy becomes code-managed and immutable at runtime.
The forgery surface is deleted rather than guarded; rationale in the design
document, section 11.**

Mask policy forgeable from any bundled dependency via the global symbol
registry, persisted durably.

The fix is not a validation on the write path. `defineMaskPolicy`, the
`Symbol.for("@zeroship/db/MaskPolicyState")` slot, the `setMaskPolicy` native
op, `dispatch_set_mask_policy` and the durable store are all deleted; the policy
arrives in the deploy artifact beside the schema descriptor and is fixed for the
isolate's life. **A guard would not have been enough**, and that is the reason
the decision is a deletion: the write path is reachable from any transitively
bundled package, so any check it performs is a check on an input the attacker
also controls the timing of.

**Evidence:** `sdks/db/src/policy.ts:85,91-94`

### L2 (DECIDED 2026-08-27) - mask policy suppressible via `_flushPendingMaskPolicy`

**DECIDED 2026-08-27 - same deletion as L1. There is no pending slot to drain,
so there is nothing to suppress.**

Mask policy suppressible via `_flushPendingMaskPolicy`, dynamically importable
by any referrer.

L1 and L2 are the same mechanism read in two directions - forge a policy, or
prevent the real one landing - and it is worth noting that they close together
and could not have closed separately. A fix that hardened the drain against
suppression would have left forgery, and vice versa.

**Evidence:** `bootstrap_modules.rs:73-80`

### L3 - `__zsSchemaReady` shadowable by an accessor before assignment

`__zsSchemaReady` shadowable by an accessor before assignment, so dispatch never
awaits it.

**Evidence:** `init.rs:515-518`

### L4 - CDC ships mask-only plaintext and companion columns

CDC ships mask-only plaintext and companion columns, with no policy check and no
audit row. The existing unit test asserts the **wrong property** and must be
replaced, not extended: it hand-builds an encrypted-and-masked fixture and
checks the frame round-trips it, so it rules on the fixture rather than the
pipeline and never constructs the mask-only case.

**Evidence:** load-bearing lines are `mask_pass.rs:150-156` (v3 cited `:29-31`,
which is only the module comment); `broker.rs:990`; the fixture-only test at
`broker.rs:1852`

**Still live, re-verified 2026-08-27 against `feat/dbbind-impl` at `196622c9b`
(not against the dangling `f44bcf6b6`).** `ws_frame_for_change`
(`broker.rs:986-998`) serialises `ev.new_tuple` into the frame **whole**, under
the key `"row"`. There is no mask pass, no policy consultation and no audit
write anywhere between the change event and the socket. Whatever the row holds
is what the subscriber receives.

#### UNBLOCKED 2026-08-27 by `632c1d1fa` - the coupling below has cleared

The deferral that follows was correct when written and is now stale, which is
worth leaving visible rather than deleting. It said the fix had to wait for the
masking storage flip, because the runtime derived a sibling name by
`format!("{col}_masked")` and the flip would change what that name meant.

**The descriptor now supplies the mapping, so the fix no longer waits on the
flip.** `632c1d1fa` made the descriptor the sole schema authority and it carries
per-field `storage` (`valueColumn`, `rawColumn`, and the three `raw*` capability
flags). `read_column_for` (`zeroship-schema/src/query.rs:3402`) is the single
site that maps a declared field to the column a read touches, and L26's fix went
through it. The CDC path can use the same mapping.

**And the fix shape improves in the process.** The deferral assumed a mask pass
on the CDC path. With descriptor-supplied storage the better answer is a
FILTER: masking on the way out means the plaintext was in the frame and got
transformed; filtering means the raw column was never selected. The second is
the one that cannot leak through a missed call site.

**What has NOT changed is the exposure.** This has been live throughout - the
read path was fixed on 2026-08-27 and this path was not, so `find` returns the
mask while a subscription on the same collection returns plaintext. The
divergence between the two paths is new as of that fix, and it is the worst
state of the three (both leaking, both safe, or one of each): a creator
inspecting the read path would reasonably conclude masking works.

~~**But do NOT fix this in its current shape - it is coupled to the masking
storage flip, and the flip changes what the fix even is.**~~ Today `mask_pass.rs`
derives a sibling by string construction: `let sibling =
format!("{col}_masked")` (`mask_pass.rs:150-152`), then inserts it alongside the
plaintext column. Under the decided flip, one declared field maps to two
physical columns - `ssn` holding the **masked** value and `ssn_raw` holding the
real one - supplied by the descriptor rather than re-derived by `format!`. That
changes L4 from "apply a mask pass on the CDC path" to "**never ship the `_raw`
column**", which is a filter over a descriptor-supplied column set and not a
transformation at all. It is also strictly safer: the default becomes masked by
storage rather than masked by remembering to call a pass.

So L4's *fix* waits on the descriptor specification. **Its exposure does not.**
Until that lands, a creator collection with a masked column ships plaintext to
every subscriber, and the fixture-only test at `broker.rs:1852` will keep
reporting green while it does - it rules on a hand-built fixture, never on the
pipeline. That test is wrong today and stays wrong under either design, so
replacing it is the one part of L4 that is safe to do now and should not wait.

### L9 (DECIDED 2026-08-27) - the filter path is an unaudited plaintext oracle

**DECIDED 2026-08-27 - storage flip. The rationale and the four owed items are
in SC-6, named again at the end of this entry.**

Masking is projection-shaped, so the filter path is an unaudited plaintext
oracle. `build_where_with_dialect(filter, params, dialect)` takes **no schema
hint** (`query.rs:5274-5281`), so it cannot know a column is masked; a masked
column stores plaintext in the parent column, so `find({ssn:{$gt:"500-00-0000"}})`
renders `WHERE "ssn" > $1` against plaintext. The caller never sees an unmasked
value and does not need to - **the matching set is the answer**, and repeated
queries binary-search the exact value with no authorization check and no audit
row. Does NOT violate the letter of the audit guarantee (that covers `.unmask()`
calls, which this is not) - it defeats the protection goal through a channel the
guarantee never scoped. **DECIDED 2026-08-27: flip the storage.** `ssn` stores
the MASKED value, `ssn_raw` stores the real one, and `ssn_raw` is **reserved and
unqueryable** - not in a filter, not in a projection, not in a sort, not a field
of the generated type. Plaintext is reachable only through an explicit API,
where authorization and the audit row already live. This beats the three policy
options SC-6 had recorded, all of which kept plaintext in the natural-named
column and then policed the filter path on top of it - leaving the design
fail-open, which is precisely how the defect arose. After the flip the ignorant
path is the safe path, the `"ssn_masked" AS "ssn"` substitution is deleted
rather than extended, and the guard becomes a **reserved-suffix check needing no
schema** instead of a metadata lookup the filter builder does not have. It also
makes the audit guarantee simply true rather than narrow-but-true: plaintext
ends up with exactly one reader. What it owes: lookup BY plaintext must be
supported by the explicit API or the feature is closed rather than secured;
unique indexes and foreign keys must follow `ssn_raw`, since enforcing
uniqueness over masks is a silent integrity failure; and the AAD binds the
column name, so this is a migration-engine change too. Full rationale and the
four owed items in SC-6.

**Evidence:** `query.rs:5274-5281` (no hint); mask substitution is
select-list-only at `query.rs:3351`, extended to aggregates by
`aggregate_read_ident` at `:3412`; audit promise at `docs/reference/db.md`
"Audit tables"

### L16 (NEW) - every autocommit CRUD operation costs four round trips and a fresh parse

**NEW - the largest constant-factor tax on every op, verified 2026-08-27**

**Every autocommit CRUD operation costs four network round trips and a fresh
server-side parse+plan.** The single funnel at `exec.rs:310-341` does
`client.transaction()` (sends `BEGIN`), `tx.simple_query(&setup_sql)` (`SET
LOCAL` role + timeouts, rebuilt per op though it depends only on `app_id`),
`tx.query_text_params(...)`, then `tx.commit()` - four round trips for one
`find`. And the query itself uses the **unnamed** statement, so PostgreSQL
parses, rewrites and plans the SQL on every call. The driver HAS `prepare_cached`
(`libs/compio-postgres/src/prepare.rs`), but `statement_cache_capacity` defaults
to **0** (`libs/compio-postgres/src/config.rs:833`) and **plugin-db contains
zero references to either name** (verified by grep across
`crates/zeroship-plugin-db/src/`). SQLite mirrors it exactly:
`backend/sqlite/session.rs` calls `conn.prepare` twice and `prepare_cached` zero
times, recompiling every statement. All of this runs against `Pool::connect(&url,
8)` (`lib.rs:862`) - **8 connections per worker thread**, shared by the ~200
co-resident isolates that thread admits.

**Evidence:** `exec.rs:310-341`; `libs/compio-postgres/src/config.rs:833`;
`crates/zeroship-plugin-db/src/lib.rs:862`; `backend/sqlite/session.rs`

*(Citation corrected 2026-08-27: this entry and two other documents cited
`lib.rs:872` for the eight-connection pool. `Pool::connect(&url, 8)` is at
`lib.rs:862`; `:904` is the two-connection deprovision pool below and was always
right.)*

A smaller sibling, verified the same day and folded here rather than given its
own entry: **deprovision opens a fresh pool per deleted app.**
`deprovision_app_cdc` calls `Pool::connect(db_url, 2)` (`lib.rs:904`) on every
deletion driven by the version poller - two connects, two authentications and two
TLS handshakes per app, discarded immediately. At the churn rate the platform's
target implies that is a constant connect load against PostgreSQL for work that
could share one long-lived platform-role pool. SC-5 already names the ownership
that would fix it; nothing on this branch does.

**What is measured and what is not.** The four round trips, the unnamed
statement, the zero-capacity default, the absent `prepare_cached` calls and the
pool size are all read directly from the tree. The *throughput* consequence is
**not measured** - holding a connection longer plainly reduces ops/sec against a
fixed pool of 8, but this document does not have a benchmark and should not
carry a multiplier it did not run. That measurement is owed before any figure is
quoted.

L16 is the finding most directly served by the IR this design proposes, which is
worth saying because it turns a cleanup into a design argument. A `DbPlan` has a
**stable shape**: the same operation against the same collection produces the
same SQL modulo parameters. That is exactly the precondition for a named
prepared statement, and it is the property raw per-call SQL construction throws
away. An IR that renders to a cacheable statement key gets prepare-once for free,
where the current path cannot have it at any capacity setting because nothing
ever asks for a named statement. The `SET LOCAL` rebuild has the same character:
it depends only on `app_id`, so it is a per-binding constant being recomputed
per operation.

SC-3 records an overlapping measurement of the prepared-statement half, with
four citations this entry does not have (`Client::new_with_statement_cache`,
`statement_cache_execution_threshold`, `bind.rs:163`, `prepare.rs:312-356`) and
a self-correction of SC-3's own earlier draft. That duplication is accepted
rather than collapsed: the self-correction belongs with the draft it corrects.

### L17 (SUBJECT REMOVED 2026-08-27) - a partitioned creator table gets NO runtime metadata, so its protection passes are skipped

**SUBJECT REMOVED 2026-08-27 by decision 8, not fixed.** This is a defect *in*
live introspection - a partitioned table is not enumerated, so no runtime
metadata is built for it and its encryption and mask passes are skipped. Decision
8 deletes the data plane's introspection entirely
(`crud/introspect_schema.rs`), so the code path that fails to enumerate the
table stops existing and metadata comes from the descriptor instead, which
describes a partitioned table exactly as it describes any other.

**Kept open, and kept for two reasons.** First, the fix is a deletion nobody has
performed, so the defect is live in `main` today. Second, and more usefully: the
descriptor path must be checked to actually cover partitioning, because "the
new path does not have the old path's blind spot" is an assumption until
someone looks. The acceptance arm that tested this - "a relation present but
unenumerable yields `SCHEMA_INTROSPECTION_FAILED`, proved with a partitioned
table" - is retracted in the design, so **this defect currently has no arm at
all**.

*Moved out of the design document on 2026-08-27. The governing RULE - that the
runtime needs its own enumeration of logical creator relations - stays in the
design's resolved-metadata section; what follows is the live defect.*

Security-relevant, pre-existing, and on a table shape the public authoring path
supports.

`read_live_schema` filters `AND c.relkind = 'r'`
(`crates/zeroship-schema/src/diff.rs:641`). That predicate:

- **includes physical partitions** - a partition is `relkind = 'r'` with
  `relispartition = true`;
- **excludes the partitioned parent**, which is `relkind = 'p'`.

So for a partitioned creator table the parent is invisible to introspection.
`build_runtime_schema` returns `None` for it, and `None` means *this collection
has no encrypted or masked columns - skip the passes*. **An encrypted or masked
partitioned table therefore reads with its encryption and mask passes turned
off**, because "no metadata" and "no protection needed" are the same value.

This is not an operator-only shape: `PARTITION BY` is emitted from the public
migration DSL (`sdks/migrate/src/ops.ts`, lowered in
`crates/zeroship-migrate-postgres/src/ddl.rs`).

**The repository already knows the right predicate and uses it elsewhere.**
`creator_table_query` in `crates/zeroship-migrated/src/publication.rs:15-23`
enumerates `relkind IN ('r','p') AND NOT c.relispartition` and excludes
`__zeroship_%` by name. Two paths in one codebase disagree about what a creator
table *is*, and the introspection path - the one that decides whether to decrypt
- has the wrong answer.

There is a second-order cost created by this branch: populating every collection
from one read caches **P entries for physical partitions** that no creator ever
requests, spending the per-app cache budget the design is separately trying to
bound. That half cannot be fixed in the populate loop, because `LiveSchema`
carries no relkind or partition flag to filter on
(`diff.rs:201-218`) - the information is discarded before the loop sees it.

The fix belongs at the source, and it is deliberately **not** made here:
`read_live_schema` is shared with the migration diff path, where enumerating
physical partitions may well be intended. Changing a shared catalog query to
serve the runtime without establishing what the migration side needs is how a
correct-looking fix breaks the other consumer. What the runtime needs is its own
enumeration of *logical creator relations*, plus a hard error when a registered
relation cannot be enumerated - rather than the silent `None` that currently
reads as "unprotected".

### L18 (PARTLY FIXED, SUBJECT REMOVED 2026-08-27) - the cold-start fix opened a cross-tenant eviction DoS

**SUBJECT REMOVED 2026-08-27 by decision 8.** This is a defect in the cache that
made per-operation introspection affordable, and in the admission budget added
to bound it. Decision 8 removes the introspection, so
`crud/introspect_schema.rs` - including the `MAX_COLLECTIONS_PER_POPULATE = 32`
cap and the per-key singleflight that were **this entry's own partial fix**
(`f44bcf6b6`) - and `live_metadata.rs` both lose their last caller.

**Two things follow, and neither is a retirement.** The defect is live in `main`
until the deletion happens, and the third half that never landed - a per-app
sub-map and any total bound on `introspected_schemas` - is now moot rather than
owed. **A fix that shipped this morning and was orphaned this afternoon is worth
recording as such**: it was correct, it closed a real cross-tenant DoS, and the
premise it defended was withdrawn hours later. The design records the sequencing
consequence in section 9 and deliberately does not propose a deletion order.

*Moved out of the design document on 2026-08-27, where it sat under the
cache-bound analysis. The text below is that analysis unchanged; the status
paragraph at the end is new and dated.*

This is a defect **introduced by the change the design recommended**, and it
is worth stating plainly rather than folding into the bound discussion.

`introspected_schemas` is one map per **OS thread**, keyed `"{app}:{coll}"` and
shared by every app resident on that thread - the context's own documentation
discusses exactly this co-residency, explaining that keying by `app_id` is what
keeps a parked transaction of app A "invisible and untouchable to B"
(`context.rs:141-150`). Any LRU over that map therefore **evicts across
tenants**.

Now combine that with `cache_every_collection`, which iterates
`live.tables.keys()` with **no cap** (`crud/introspect_schema.rs:166-179`) and
inserts one entry per table atomically. A single tenant with 500 tables performs
one cold read - one line of app code - and inserts 500 entries into a budget
shared with up to ~200 co-resident tenants. At a 10,000-entry bound that is 5%
of the shared cache consumed by one app's first operation, and a tenant with
enough tables can evict most of it.

**This did not exist before the fix**, because the previous behaviour inserted
exactly one entry per read. Trading N catalog reads for one read is right; doing
it by inserting N entries into an unbounded cross-tenant cache converted a
latency problem into a **neighbour-eviction vector**. Both halves of the fix
need to land together, and only the first half has:

- per-app sub-maps rather than one flat thread-global map, so eviction is
  scoped to the tenant that caused it (the hierarchical shape argued for in the
  design's cache-bound section and in L22, now load-bearing for isolation rather
  than just for lookup cost);
- a **per-app** cap on entries admitted by one populate, so a wide tenant
  cannot spend a shared budget in one operation.

Recording this because the sequencing matters: the populate-all change is
already committed and the bound is not, so the window in which this is
exploitable is open now.

**And the miss path has no singleflight, which multiplies the read this section
is about.** Verified 2026-08-27: `crud/introspect_schema.rs` contains **zero**
in-progress or singleflight markers, and the only such guard anywhere in the
context is `backend_init_in_progress` (`context.rs:325`), which covers backend
initialization rather than introspection. So two cold operations on the same
thread, interleaved at the `await`, both miss the cache and both run the whole
catalog read; a cold app's opening burst of K concurrent operations costs **K**
whole-schema reads per thread.

That compounds with the O(total tenants on the cluster) cost of each read
established in the design's cache-bound section - K copies of the expensive
query, not K copies of a cheap one - and it lands squarely on the case the
populate-all fix was written for, since a cold app is exactly the app whose
first request arrives as a burst.

The design's own end-state `#### Caches` section already mandates a per-thread
singleflight. The gap is that the interim shipped without it, so the design
states the requirement and the code does not meet it - which is a different
situation from an open design question and should not be filed as one.

**Status, re-derived against the tree 2026-08-27 at `f44bcf6b6`
("fix(db): bound and singleflight live schema cache fills").** Two of the three
things this entry asks for have landed and one has not, so the paragraphs above
are kept verbatim rather than rewritten:

- **Landed: the per-populate cap.** `MAX_COLLECTIONS_PER_POPULATE = 32`
  (`crud/introspect_schema.rs:59`) bounds one whole-catalog read to the
  requested collection plus at most 31 uncached siblings
  (`:216-234`), with the arm `one_populate_admits_at_most_the_per_populate_cap`.
  The 500-entry single-populate vector above is closed.
- **Landed: the singleflight.** Cold misses are singleflight per exact
  `(binding, collection)` key through `poll_schema_introspection`
  (`context.rs:666`) and a cancellation-safe `SchemaIntrospectionGuard`
  (`crud/introspect_schema.rs:61-90`). The K-whole-schema-reads burst is
  closed.
- **NOT landed: the flat cross-tenant map, and any total bound at all.**
  `introspected_schemas` is still one `HashMap<(DbBinding, String),
  Option<Value>>` per OS thread (`context.rs:293`) with insert, get and
  contains and **no eviction path whatsoever** - so the eviction DoS is
  currently unreachable only because nothing evicts, and unbounded growth
  replaces it. The first bullet above is still owed.

### L22a - the same map is a per-transaction CPU cost, not only a memory ceiling

> **Renumbered `L22` -> `L22a` on 2026-08-27: two different defects were both
> called L22.** This one (the per-transaction CPU cost of the metadata map) and
> "one error code, two causes, and the hint is wrong for the new one" further
> down, which is now **L22b**. They are not two readings of one defect the way
> L1/L2 are - they share no mechanism, no file and no fix. The collision was
> invisible in prose and visible the moment the headings were listed as an
> outline, which is the document's own stated reason for putting status in the
> heading. Cite `L22a`/`L22b` from here on; an unqualified "L22" in any older
> note is ambiguous and must be resolved by reading which defect it describes.

*Moved out of the design document on 2026-08-27. The design keeps the commitment
this analysis produces - per-app metadata behind a cheap handle, resolved once
per operation - and points here for the measurement behind it.*

Memory is the obvious consequence of an unbounded map and it is not the
expensive one. **Every transaction start scans it.**

`mint_tx_view` needs the list of collection names to hang off `tx.<name>`, and
it gets them from `cached_schemas_for_app`
(`crates/zeroship-plugin-db/src/context.rs:697-707`), which does:

```rust
let prefix = format!("{app_id}:");
self.schemas.iter()
    .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|coll| (coll.to_string(), v.clone())))
    .collect()
```

Two costs, and the second is pure waste:

1. **The scan is cross-tenant.** `self.schemas` is the thread-global map keyed
   `<app_id>:<collection>`, so the iteration is O(every collection of every app
   the thread has ever served) to find the handful belonging to this one. It is
   the unbounded map of the design's cache-bound section, walked linearly, on a
   hot path. At the stated target this is a million-entry scan per
   `db.transaction()`.

2. **Every matching schema is deep-cloned and immediately dropped.** `v.clone()`
   rebuilds the whole `serde_json::Value` - at the corrected ~18x multiplier and
   the measured 551 B typical serialized size, roughly **10 KB of fresh
   allocations per collection**, spread over one `IndexMap` and two `Value`
   nodes per column - and the caller is
   `.map(|(name, _schema)| name)` (`v8_classes/transaction.rs:75-81`). The
   underscore is the tell: the function's entire return payload beyond the key
   is constructed and discarded one line later. A transaction on a 20-collection
   app allocates and frees on the order of **200 KB** to produce 20 strings.

   (An earlier draft of this paragraph said ~76 KB, arrived at by multiplying
   the 7x table the design's cache-bound section had already retracted. Keeping
   the note because the failure is the interesting part: the retraction was
   *written* and the derived figures were *not re-derived*, so a stale constant
   walked straight into a new claim. Any figure in these documents traceable to
   that table is suspect unless it names the ~18x measurement.)

The accessor's own doc comment explains the shape - it was written for the
drift-check sweep, which genuinely wants `(collection, schema)` pairs - so this
is not a bug in `cached_schemas_for_app`. It is a hot path reusing an accessor
built for a cold one, and paying its full cost for a fraction of its result.
That is worth stating plainly because it changes the fix: the accessor is
correct and should stay, and what is owed is a **name-only enumeration** beside
it that borrows rather than clones.

**And it is the convention, not this one accessor.** The same shape recurs on
the warm CRUD path, which is the hottest path the plugin has:

- `deploy_token_for` returns `String` (`context.rs:680-684`) - a clone on hit,
  a fresh `"cold_start".to_string()` on miss, **every operation**, to produce a
  value that is only ever compared.
- the read pipeline resolves an **owned** schema per operation and hands it
  straight to `scope_schema` (`crud/read_pipeline.rs:62-70`).
- projection retention is `fields.iter().any(...)` inside a per-column loop -
  O(schema width x projection width) where a prebuilt set is O(width).
- write-stage construction scans the same schema **four separate times**
  (`crud/write_pipeline.rs:192-201`): `WriteStages::new` calls
  `schema_has_encrypted_columns`, `schema_has_masked_columns`,
  `schema_has_sqlite_binary_columns` and `schema_has_plain_bytes_columns` in
  turn, each walking every column, to produce four booleans that one pass could
  yield. (This one takes `Option<&Value>` - a borrow - so it costs scans, not
  allocations. Worth separating: the fix is precomputation, not ownership.)
- update and upsert resolve the schema **twice** on the route-decision path.
  `update_requires_per_row_encryption` and `upsert_requires_conflict_probe`
  each `await runtime_schema_for(...)` for an **owned** schema, use it for a
  single boolean, drop it - and the main write pipeline then resolves it again
  (`crud/write_pipeline.rs:359-383`). At the accessor's measured ~10 KB per
  entry that is two full rebuilds of the same object to answer one yes/no.

*(All four bullets re-read against the tree 2026-08-27 rather than taken from
the review. The projection line is
`obj.retain(|key, _| key.starts_with('_') || fields.iter().any(|f| f == key))`
at `crud/read_pipeline.rs:116` - a linear scan of the projection list per
column. It mutates the owned schema in place, so like the four-scan case it
costs time, not allocations.)*

None of these is expensive enough to notice in a profile of a single operation,
which is exactly why they are worth naming in a design document rather than
leaving to a later optimization pass: they are **decisions about what an
accessor returns**, and they are cheap to make correctly now and expensive to
unpick once every call site depends on owning its result.

### L28 (NEW) - `platform-cli` does not compile, and it blocks 26 test harnesses

**NEW - found 2026-08-27 by the e2e rewrite, which could not run the suite it
had just rewritten. PRE-EXISTING: attributed by control build.**

```
cargo build -p zeroship-migrate-adapter --features platform-cli
```
fails with **22 errors** (`error: could not compile ... due to 22 previous
errors`), among them:

```
error[E0432]: unresolved imports `zeroship_migrate::PostgresBackend`, `zeroship_migrate::SqlDialect`
error[E0425]: cannot find function `snapshot_schema` in crate `zeroship_migrate`
error[E0425]: cannot find function `applied` in crate `zeroship_migrate`
error[E0560]: struct `zeroship_migrate::LiveSchema` has no field named `sqlite_schemas`
error[E0061]: this function takes 1 argument but 0 arguments were supplied
```

**Attribution.** Identical failure at HEAD (`a5832708b`) and at `8c6caa465`,
which predates both the `__zeroship_admin` deletion and the descriptor change.
Not caused by any of this session's commits.

**Cause is visible in history.** `0b1896a90` ("build(migrate): delete in-tree
zeroship-migrate engine and napi; finalize on published zero-migrate") replaced
the in-tree engine with a published crate, and
`crates/zeroship-migrate-adapter/src/platform.rs` was never ported to the new
API. `snapshot_schema` occurs **0 times** in `crates/zeroship-migrate/src/lib.rs`.
A plain `cargo build -p zeroship-migrate-adapter` with no features SUCCEEDS -
only `platform-cli` is broken, which is exactly why nothing noticed.

**Blast radius, measured.** The feature produces the `zeroship-platform-migrate`
binary, and no such binary exists in `target/`. **28 files under `tests/`
reference `zs_platform_migrate`; 2 are under `tests/lib/` (the helper is defined
in `tests/lib/runtime_secrets.sh`), so 26 are calling harnesses** - the e2e
agent reported 26 and that is the right number, but only because the two
library files happen to cancel the difference. Stated here as 28-minus-2 rather
than 26 so the next person re-measuring does not think the count drifted. All 26
are blocked identically -
including `tests/e2e_durable_workflows.sh` (dies at `=== DW-07 build ===`,
exit 101, before Postgres and before any assertion) and
`tests/e2e_gateway_workflow_advance_authz.sh` (exit 2, `FAIL missing
.../zeroship-platform-migrate`).

**This is the same blind spot AGENTS.md already documents, one stage further
along.** The clippy gate's fourth arm exists because the first three audited one
feature resolution and reported 148 of 148 on a workspace declaring 158; among
the 10 unlinted was **this crate's `platform_migrate`**, which held eleven
standing deny-level `clippy::await_holding_lock` errors. A feature that is
neither built nor linted does not merely accumulate lint debt - it falls behind
an API rewrite and stops compiling, and every gate stays green because every
gate builds the default feature set.

**The decision this forces is not "port it".** The operator has already asked
whether `zeroship-migrate-adapter` is still needed. Porting `platform.rs` to the
restructured engine API is real work; deleting the crate is also real work; and
doing the port first and the deletion second is the only sequence that is
certainly wasted. **This entry does not choose** - it records that the choice is
now blocking 26 harnesses rather than being a tidiness question.

#### It is worse than 26 harnesses, and there is a THIRD option nobody costed

**`platform-cli` is a shipped image target, not test-only.** `deploy/Dockerfile:29`
builds `zeroship-platform-migrate` alongside `zeroship-worker`, `zeroship-auth`
and the CLI, and `deploy/ops/db-migrate.sh:45` wraps it. So a broken
`platform-cli` is a production image that does not build, and the harnesses are
the visible symptom rather than the whole cost.

What the binary is: the platform-schema migration driver. It runs
`db/migrations-ts/*.ts` inside a `zeroship-runtime` V8 isolate to record an
`{ir_version:1, name, ops}` envelope, lowers it through the engine under a
platform guard, and applies real DDL plus the append-only `zeroship_migrations`
journal over compio (`bin/zeroship-platform-migrate.rs:1-21`). It is the
producer of the frozen journal AGENTS.md warns about.

**THE PREMISE FOR ITS EXISTENCE HAS BEEN REVERSED, AND NOTHING NOTICED.**
Raised by the operator on 2026-08-27 as "we can do migration with the
zeroship-migrate CLI, why did we retire it?", and the history answers it:

- **`0b1896a90` (2026-07-13)** deleted `crates/zeroship-migrate` and moved every
  shipped path onto the **published** `zero-migrate`, consumed through
  `zeroship-migrate-adapter`. With the engine OUTSIDE the tree, the platform
  migrator needed a home that could reach it - which is why
  `zeroship-platform-migrate` was created and the in-tree CLI retired.
- **`b044546c2` (2026-08-26)** brought the engine **back in-tree**: "the engine
  crates take the zeroship-migrate name."

Measured 2026-08-27: `crates/zeroship-migrate` exists and is a **library with no
`[[bin]]`** - the CLI was removed and never restored. `third_party/zero-migrate`
**does not exist**. `zeroship-migrate-adapter/Cargo.toml:46` depends on
`zeroship-migrate = { workspace = true }` - it bridges to a **workspace crate**,
not a published one.

**So the adapter spans a boundary that has closed**, and its own header still
describes the old world ("the monorepo's bridge between the published
`zeroship-migrate` engine", "vendored at third_party/zeroship-migrate",
`Cargo.toml:1-4`). AGENTS.md repeats the stale claim in four places, including
the crate index at `:123`.

**That is also WHY it broke and stayed broken**: a bridge across a vanished gap,
behind a feature flag no default build compiles, so nothing forced it to keep up
when the engine moved home.

**Third option: return the platform migrator to `crates/zeroship-migrate`,
where the engine now lives**, and delete the adapter. Not costed here, and one
thing must be checked before recommending it: whether the adapter contributes
anything beyond the bridge - the compio-postgres `SqlSession` newtype and the
PG-only "applies pure DDL and REFUSES anything else" behaviour are real and
would need a home.

---

## Cross-crate: per-app stores outside plugin-db

*Moved out of the design document on 2026-08-27, where these sat under the
heading "The pattern is not confined to plugin-db".*

Two more per-app stores outside this plugin have the same shape as the per-app
metadata caches above, and they matter to this design because they sit on the
paths it depends on. Neither is plugin-db's to fix, but a design that claims a
bounded per-app footprint while these are unbounded is claiming something the
process does not deliver.

### L19 - the worker's env cache retains decrypted secrets per app, indefinitely

**(Every citation below re-read against the tree 2026-08-27, not taken from the
review.** The code states the behaviour in its own comments, which is why this
is a design question rather than a bug report: both decisions are deliberate and
individually reasonable.)
`SharedEnvs` is a process-wide `HashMap<Uuid, Arc<CachedEnv>>`
(`worker/src/sync.rs:81-98`) holding the complete validated env JSON. Isolate LRU
eviction deliberately leaves the entry behind (`worker/src/cache.rs:780-785`),
and the only reclamation is
`e.retain(|app_id, _| versions.contains_key(app_id))` against the control
plane's known-app set (`worker/src/sync.rs:189-190`) - so retention tracks
"every app that still **exists** and was ever loaded by this process", not "apps
with a live isolate".

Both halves are deliberate and say so. Eviction declines to touch the env
because it is process-wide and another thread may still need it; the GC exists
because "per-thread reconcile only fires for locally-cached apps, so an app
that's been LRU-evicted from every thread gets no cleanup". Each decision is
locally right. Their composition is the leak: the only thing that can free an
entry is an app being **deleted**, and nothing frees one for an app that merely
stopped receiving traffic. The
control-plane endpoint that fills it decrypts every secret to build the response
(`control/src/env_store.rs:353-390`). The security consequence is the one worth
stating plainly: **a worker's plaintext-secret residency is a function of its
uptime**, and deletion GC is an eager cleanup rather than a capacity policy.

SC-5 carries the other half of this, and only the other half: **the GC key must
include the app incarnation**, not the app id alone. That is a contract
requirement on the service SC-5 defines, not a restatement of the leak above.

### L20 - the meter's drain is a stop-the-world proportional to app history

**(The unbounded half of this is FIXED as of 2026-08-27 - `drain` now evicts
apps that drained empty, and `Meter::tracked_app_count` exists so the bound can
be asserted rather than trusted. The stall itself remains; see the end of this
entry.)**
`Meter::drain` takes the **exclusive** lock on the process-wide app map
(`metering/src/meter.rs:216`) and holds it while iterating *every app ever
touched*, parsing a `Uuid` per app and allocating a `source.clone()` and a fresh
`Uuid::now_v7().to_string()` per emitted metric. It removes nothing, so the map
only grows. On the default ten-second cadence (`metering/src/outbox.rs:52`) that
is a periodic global pause whose length grows with the number of distinct apps
the process has ever served - and every `env.db` / `env.kv` / `env.storage`
usage increment blocks behind it.

The precise bug is visible in the code's own annotations. The function carries
`#[allow(clippy::readonly_write_lock)]` - clippy correctly observed the write
guard is never used to mutate, because the per-app drain works through interior
atomics. The lock is taken purely as **exclusion**, and the doc comment says
exactly why: "so no increment interleaves a partial drain (fixed-counter `swap`s
plus a `custom` take must be atomic **per app** relative to **that app's own**
increments)". The stated requirement is per-app atomicity; the implementation
buys it with a process-wide exclusive lock. That is the same
bound-to-the-wrong-thing shape as L10 above - a correct requirement enforced at
the wrong granularity - and the fix follows from the comment rather than
contradicting it: make the exclusion per-app (`Arc<AppCounters>` values, drained
one at a time under their own guard), and the global lock disappears along with
the pause.

One correction to how this was reported to us, because it affects the fix: the
review called the increment path contended "contrary to its lock-free comment".
The increment fast path takes a **read** guard (`meter.rs:177-181`), and
concurrent readers do not exclude one another, so increment-versus-increment is
genuinely uncontended and the comment's intent is defensible. The contention is
entirely increment-versus-drain. Optimizing the fast path would buy nothing;
only removing the global write lock does.

**What landed, and what deliberately did not.** `drain` now evicts any app whose
counters drained empty, so the map is bounded by apps with traffic since the
last drain rather than by every app the process has ever touched - and because
the scan is over that same map, its exclusive-lock hold time is bounded by the
same number. Regression test `drain_evicts_apps_that_went_idle`, red before the
change on the retention assertion.

The trade is worth stating because it is a real one rather than a free win: the
type promised a write lock "only on first-touch per app", and eviction means an
app idle across a drain pays first-touch again on its next increment. That is
the right side of the trade - an app busy enough for the write lock to matter
never idles through a whole ten-second window, and one that does idle is by
definition not hot - but it IS a behaviour change, not a pure deletion.

**The stall itself was left alone on purpose.** Making the exclusion per-app is
a different change with a different risk profile, and bundling it would have
meant shipping an atomicity refactor under cover of a leak fix. The unbounded
*growth* was the part that made the stall unsurvivable at the platform's target
scale; the fixed-size stall is a normal optimization that can be argued on its
own merits.

## Cross-crate: the worker

### L21 - isolate admission builds the runtime before it can know it will be rejected

*Moved out of the design document on 2026-08-27, where it had been spliced into
the middle of L20's meter analysis. It is not a per-app store, which is why it
read as inserted there.*

`load_app` compiles and initializes the V8 runtime first, and only then takes
the cache lock to discover that a full cache whose entries are all leased cannot
evict, at which point it drops the runtime it just built
(`worker/src/cache.rs:490-505`). Under saturation, requests for distinct cold
apps repeatedly pay a full compile-and-evaluate for a runtime that can never be
admitted - which is exactly the long-tail regime the platform's stated scale
implies.

This one deserves care rather than a straight inversion, because the ordering is
**deliberate** and the code says why: "Only mutate the cache after the new
runtime has initialized. A corrupt descriptor during reload must not evict the
last-good isolate." That guarantee is real and must survive. But it only governs
the *reload* case, where the app is already in the cache. The wasted work is in
the disjoint case - a **new** app arriving at a full, fully-leased cache - and
the two conditions that identify it (`isolates.len() >= max_size` and
`!isolates.contains_key(&app_id)`) are both cheap and read-only. Only
`evict_lru` mutates. So a preflight can reject the hopeless case before the
build while leaving build-before-replace untouched for same-app reloads. Stated
that way it is not a trade-off at all, which is the useful thing to notice: what
looked like "safety ordering versus wasted work" is two different cases sharing
one code path.

---

## The open decision: L12

### L22b - one error code, two causes, and the hint is wrong for the new one

*(Renumbered from a second `L22` on 2026-08-27 - see L22a for why.)*

**Creator-facing, INTRODUCED by step 3 (SC-2), open**

`transaction_connection_busy` now has two producers with opposite remedies, and
carries a hint written for only one of them.

- **Original cause** (`exec.rs:155-164`): two operations of the SAME
  transaction overlap, because a transaction owns one connection. The hint is
  correct and specific - *"Await each `env.db` call inside the
  `db.transaction(...)` callback before starting the next - a `Promise.all` over
  several tx operations runs them concurrently on that one connection."*
- **New cause** (`backend/sqlite/session.rs:563`, added by step 3): the SQLite
  actor refuses admission because **another app** holds the transaction lane.
  One `SqliteSession` serves every ATTACHed app, so the admission key is
  `(runtime_instance_id, session)` rather than SC-1's
  `(runtime_instance_id, app_id)`.

**Corrected 2026-08-27, having read both constructions rather than assuming they
shared one.** The first version of this entry said the creator receives the
`exec.rs` hint for the cross-app cause and is therefore told to fix code that is
already correct. That is NOT what happens: the two producers build *different*
errors under one code. `exec.rs:156` uses `validation_hinted` and carries the
"await each call" text; `session.rs:562` uses plain `validation` with **no hint
at all**, and its message is *"this SQLite session already holds an open
transaction on tx_conn; one explicit transaction at a time"*.

So the defect is narrower than filed, and different in kind:

- **No wrong advice** - the cross-app arm gives none.
- **But the message does not say that another APP holds the lane.** "This
  SQLite session already holds an open transaction" reads, to a creator who
  knows only their own app, as *their* transaction - which it is not. They will
  look for an unclosed transaction in code that has none.
- **And one code covers two conditions with opposite remedies**, so a creator
  cannot branch on it programmatically: one is "await your calls", the other is
  "retry later, it is not yours".

The irony is that `exec.rs:148-150` documents this exact hazard for the split it
already performed - *"an empty tx slot has two causes and they call for opposite
fixes"* - and the same collapse was then reintroduced one layer down.

**Found by the step-3 implementer, who flagged it rather than fixing it**
(2026-08-27), on the correct ground that it was outside the brief. Recorded here
so it is not lost: it needs either a distinct code for the cross-app refusal or
a cause-dependent hint, and the choice interacts with whether the per-app-file
actor lands (which removes the second cause entirely).

**Evidence:** `exec.rs:148-150`, `:155-164`; `backend/sqlite/session.rs:532`,
`:563`; `tx_route.rs:113`

---

L12 is not a defect entry like the others and this section says so in its own
heading. It is **the open decision of this document set**: a hard ceiling on the
feature the design exists to serve, with four options weighed below and a
recommendation that **remains a recommendation**. Nothing here decides it.

### L12 (NEW) - live subscriptions cost one replication slot per (app x worker), ceiling 10

**NEW - the hard scaling wall, verified 2026-08-27**

**Live subscriptions cost one PostgreSQL logical replication slot per (app x
worker process), and the default ceiling is 10.** `worker_slot_name` composes
`__zs_slot_<sha(app)>__<sha(worker)>` (`replication.rs:108-115`) - per pair,
because a logical slot admits only one active consumer - and
`wal_consumer.rs:368` opens a **dedicated, non-pooled** replication connection
for each. `cdc_lifecycle.rs` refcounts leases per app with **no cap on apps**.
Measured on this branch's own dev database: `max_replication_slots=10`,
`max_wal_senders=10` (the PostgreSQL defaults;
`deploy/compose/docker-compose.yml` sets only `wal_level` and
`max_prepared_transactions`). The 11th concurrently-subscribed pair fails
`pg_create_logical_replication_slot`. `max_replication_slots` is a
**restart-only** shared-memory GUC and every active slot is a walsender backend
competing for `max_connections`, so **no tuning makes one-slot-per-tenant reach
the platform's stated scale**. Mitigating and worth stating: slots are
demand-driven - only `collection.openSubscription()` reaches `acquire`
(`v8_classes/subscription.rs:315`) - so the bound is *concurrently subscribed*
apps, not all apps.

**Evidence:** `replication.rs:108-115`, `:208-212`; `wal_consumer.rs:368`;
`cdc_lifecycle.rs:87-111`; server GUCs measured directly

#### The one-line mitigation that has not landed: `max_slot_wal_keep_size`

*Moved here on 2026-08-28 from L12b, which is closed and lives in
`2026-08-26-runtime-db-binding-defects-closed.md`. The defect L12b describes -
a crashed worker abandoning a replication slot - was fixed on 2026-08-27 by
`b50691f2c`. **The config change below was never a fix for it and is still not
made**: `deploy/compose/docker-compose.yml` sets `wal_level` and
`max_prepared_transactions` and nothing else, re-checked 2026-08-28. It is a
blast-radius cap on the same cluster-wide failure mode, it is independent of
how L12 is decided, and it is live work, which is why it is in this file and
the entry that recommends it is not.*

**There is a one-line mitigation available today, independent of L12's outcome,
and it should land regardless: set `max_slot_wal_keep_size`.** It ships as
`boot=-1`, **unbounded**, which is precisely what lets one abandoned slot grow
`pg_wal` until the cluster dies.

Both halves of that recommendation are **measured** (2026-08-27, pg16,
`tmp/measure_wal_keep_v2.sh`), on a throwaway server that does not pin the
value:

- *It applies to a running cluster.* `ALTER SYSTEM SET max_slot_wal_keep_size =
  '32MB'` plus `pg_reload_conf()` moved the setting from `-1` to `32MB` with
  `pg_postmaster_start_time()` unchanged - no restart window, unlike
  `max_replication_slots` and `max_wal_senders`, which are both
  `context=postmaster`.
- *An over-limit slot is invalidated, not honoured.* An idle slot went
  `wal_status = reserved` -> **`lost`** with `safe_wal_size` NULL once WAL
  passed the limit, and `pg_wal` finished at **65MB** with the abandoned slot
  still present. Unbounded, that directory is what grows without limit.

So the worst case becomes *that database's subscriptions resync* instead of
*every tenant on the cluster loses the server*.
`deploy/compose/docker-compose.yml` sets only `wal_level` and
`max_prepared_transactions`, and should set this too.

Note what it does NOT do: it does not clean up the abandoned slot, and it
converts a silent stall into a subscription that must resync. It is a blast
radius cap, and it remains worth setting even now that the operator-side reaper
exists - the reaper sweeps on an interval, so the cap is what bounds the damage
during the window before a sweep runs, and what covers the case where the
reaper itself is down. Belt and braces on a cluster-wide failure mode, not a
substitute for either.

> **How the first attempt at this measurement went wrong**, because the shape
> recurs. It ran against a container started with
> `-c max_slot_wal_keep_size=2GB`; command-line options override
> `postgresql.auto.conf`, so `ALTER SYSTEM` changed nothing. The script still
> printed `CLAIM 1 HOLDS` - because it checked *that the postmaster had not
> restarted* rather than *that the value had become what was requested*. A
> verification whose success condition cannot distinguish "applied" from "never
> applied" certifies whatever it is pointed at. The second claim was untested
> for the same reason: ~200MB of WAL against a limit still standing at 2GB,
> `wal_status` correctly `reserved` throughout, read as evidence of nothing.

### RETRACTED: "the ceiling of ten already binds on one dev box"

**This section claimed a measurement it did not have, and the claim is
withdrawn (2026-08-27).** The narrative below is left in place because the
retraction is more instructive than a silent deletion, but do not cite it.

**What was actually measured, afterwards:** a consumer holds a **peak of one**
replication slot (sampled `pg_replication_slots` every 300ms across the CDC test
run: peak 1, ceiling 10, zero before and after). Five concurrent consumers
therefore drew five of ten. **Nowhere near saturation.**

**The real cause of the failure** is a test-cleanup blast radius, now filed as
its own defect: `integration.rs:2018-2024` runs
`pg_drop_replication_slot(slot_name) ... WHERE slot_name LIKE '__zs_%' AND
active = false` - dropping every zeroship slot on the **server** that is
momentarily inactive, including other runs' - and `CDC_TEST_WORKER_ID` is a
fixed constant, so concurrent runs mint identical slot names. Same shape as the
`reset_world()` defect fixed the same day: a per-test cleanup acting on global
state it does not own.

**The tell was in the error the whole time.** It read
`CDC must reach START_REPLICATION`. Slot exhaustion fails at slot *creation*,
not at start. The message never matched the hypothesis, and it took a
measurement rather than a re-reading to notice - because the hypothesis
flattered a conclusion this document already held.

**L12 does not need this incident.** One slot per (app x worker), a server-wide
ceiling of 10, a restart-only GUC, and each slot also a walsender competing for
`max_connections` - that does not reach millions of apps by any arithmetic. The
options below stand unchanged. What changes is that they rest on the mechanism,
not on an anecdote.

---

Before the options, the narrative as it was written (retained for the lesson,
not as evidence):

On 2026-08-27, verifying a merge, two replication tests failed:
`p8a2_consumer_publishes_wal_event_to_broker` and
`p8a2_supervised_consumer_reconnects_after_kill`, both with
`CDC must reach START_REPLICATION: "wal consumer: db error"`. Re-run **in
isolation, unchanged, they both pass.** The failure was contention.

What they were contending for is the finding. Four implementation agents plus
one verification run were doing replication work against one server whose
`max_replication_slots` and `max_wal_senders` are both **10**. Those are
**server-wide**, so the obvious isolation - giving the verification run its own
database - does **not** separate them. Neither does giving each agent its own
schema, or its own app id.

**So the ceiling of ten is already the binding constraint for FIVE concurrent
consumers on a single development machine.** Not five hundred tenants, not five
thousand - five. The production claim this design is written against is millions
of apps, and the same resource is consumed one slot per (app x worker).

Two things follow. First, any argument that the ceiling is a distant scaling
concern is answered: it is a present-tense operational one, and it has already
cost a false failure in this session's own verification. Second, it is worth
noting how nearly this was misread - the first hypotheses were "the fresh
database is missing setup" and "db-cache's change broke replication", and the
server log offered a plausible-looking decoy (`permission denied to use
replication slots`) that turned out to be a DIFFERENT test deliberately proving
a per-app role cannot touch slots. The isolation re-run is what separated
contention from defect, and it is the check to reach for first when a
replication test fails on a shared server.

### L12: what the ceiling of ten actually rests on, and the four ways out

The slot name carries two dimensions and they have very different standing.

**The worker dimension is a PostgreSQL constraint.** `worker_slot_name`'s own
doc says why: "a logical slot can have only one active consumer", so two worker
containers cannot share one. That is not negotiable.

**The app dimension is OUR choice, not PostgreSQL's.** It exists because
publications are per-app: `publication_name(app_id)` mints one per app, and
`zeroship-migrated/src/publication.rs:45` creates it `FOR TABLE <schema>.<table>,
...` with an explicit table list - deliberately, since a test at `:138` asserts
it does **not** use `FOR TABLES IN SCHEMA`. A publication can perfectly well span
schemas; ours does not, for tenant isolation. **That is the decision to
re-examine, because it is what multiplies the slot count by the app count.**

Four options, and they are not close in merit:

**A. Keep per-app publications.** Slots scale as apps x workers; the ceiling is
10 on a default server. This is today, and it does not reach the stated scale by
any amount of tuning.

**B. One publication and one slot per worker; demultiplex by schema in the
worker.** Slot count drops to the number of workers - 10 fits comfortably. The
cost is a **tenant-isolation regression**: every worker's CDC stream then carries
every app's row changes, so a demux bug leaks one tenant's rows into another
tenant's subscribers, and every worker pays WAL decode cost for apps it does not
serve. Trading a scaling wall for a cross-tenant data-leak surface is the wrong
direction for this platform.

**C. A dedicated CDC service that owns the slots and fans out.** One component
holds O(1) slots, decodes once, and distributes to workers over the broker and
stream machinery that already exists. Slot count stops scaling with apps AND
with workers. Tenant filtering happens exactly once, in a process whose only job
is that - which is a far better place for it than in every worker. This is the
largest change and the only one that answers the question that was asked.

It also **retires the abandoned-slot problem by construction rather than by
sweeping**: a worker that crashes cannot abandon a slot it never held. Note the
weakened claim - this used to read "closes L12b for free", written when L12b was
an open hole. L12b was fixed on 2026-08-27 by an operator-side reaper, so the
relay's benefit is no longer closing a gap; it is removing the need for a
periodic cluster sweep, its liveness lease, and the reap-a-live-worker hazard
that lease exists to prevent. Still a real simplification, but a smaller claim
than the one it replaces.

Three things were added to option C on 2026-08-27, after measuring rather than
arguing. They matter enough to change how strong the case is.

*C1 - it removes a decode multiplier, not just a count.* Slots on one database
do not divide the decoding work; each walsender decodes the whole WAL
independently against its own reorder buffer. **Measured** (2026-08-27, pg16,
`tmp/measure_decode_multiplier.sh`): five slots created at one LSN, one workload
of 40,000 rows producing 68MB of WAL, then each slot decoded in turn - 309, 260,
279, 255 and 232ms, and **every slot returned the same 40,002 changes**. Total
1,335ms against 309ms for a single slot: **4.32 out of a possible 5.00**. Shared
work would have made slots 2-5 return zero changes at no cost; they did not. The
taper is OS page cache on WAL reads.

Alongside it, `pg_settings.boot_val` on a stock server:
`logical_decoding_work_mem` **64MB per slot**, `max_connections` **100**,
`max_replication_slots` / `max_wal_senders` **10**, the last two
`context=postmaster`. Ten subscribed apps on one database pay ten full decodes
and up to 640MB of decode buffers to deliver each app a tenth of the rows. The
relay makes it one decode per database. Option D raises a limit on a term that
should not exist.

*C2 - it converts WAL retention into stream retention, and that is the real
prize.* A slot's `confirmed_flush_lsn` is a back-pressure channel wired into
the cluster's disk: the slowest confirmer pins `pg_wal` for every tenant on that
server. `max_slot_wal_keep_size` measured `boot=-1`, **unbounded** - which is
exactly the L12b mechanism. A relay confirms as soon as events are durable in
`zeroship-stream`, so subscriber lag is bounded by *stream* retention, a knob we
own, which degrades to a `Resync` instead of filling a shared disk.
`StreamTransport` already documents that `partition_key` "preserves per-key
order" (`crates/zeroship-stream/src/transport.rs:32-37`), and ships `memory` and
`redpanda` adapters - so per-app ordering and a single-node dev path both exist
already, one code path with a varying transport rather than two modes.

*C3 - it dissolves the WAL epoch carrier, the OTHER blocking decision.* The
carrier is hard only because `wal_consumer::emit_for_tuple` holds no lease and
no operation context. A relay is a long-lived process that can hold a lease,
read `app_schema_state`, cache per (app, incarnation), and stamp at produce
time. Deciding the carrier before the relay means designing a mechanism the
relay deletes. **Decide L12 and the carrier as one decision.**

What C costs, stated plainly: a new service and a new failover story (one leader
per database, a Postgres advisory lock being the natural election, resuming from
`confirmed_flush_lsn`, at-least-once with LSN dedup since LSN is monotone); a
durable stream on the subscription critical path; and added end-to-end latency,
WAL -> relay -> stream -> worker versus WAL -> worker today. **That latency is
unmeasured and no figure should be quoted for it until it is.**

**D. Raise `max_replication_slots`.** A stopgap, and a poor one: it is a
restart-only shared-memory GUC, and every active slot is a walsender **backend**
competing for `max_connections` (default 100). It buys a small constant and
cannot approach millions of apps.

**Recommendation: C**, with **D** as an explicit interim if something must ship
before C lands. B should be rejected rather than deferred - it is the option
that looks cheapest and creates a cross-tenant leak path, and this document has
already catalogued what happens when isolation is left to a filter that
something forgets to apply.

The consequence for sequencing is the thing to take away: **SC-3's subscription
surface is provisional until this is answered**, because C moves subscription
transport out of the worker entirely. Building the IR against A means building
it twice.

### How L12 was nearly missed

L12 and L13 were in the performance round's opus report and this document had
folded **none** of them - the revision pass concentrated on one reviewer's
findings and treated the other two reports as already-absorbed. That is a
process failure worth naming, because it is the same shape as the enumeration
bugs catalogued above: three sources were consulted, one was read closely, and
the coverage claim was made over the set rather than over what was actually
read. L12 in particular is the single most consequential finding of the round -
a hard ceiling of ten on the feature this design exists to serve - and it sat
unread in a file on disk.

### L29 (NEW 2026-08-28) - the `sqlite_` reservation is on the wrong identifier role

**SQLite reserves the `sqlite_` prefix for TABLE names.** This tree fences it on
**columns** and not on tables - the inversion of the rule it is implementing.

| role | validator | fences |
| --- | --- | --- |
| collection (table) | `validate_collection`, `query.rs:645-653` | `pg_`, `__zeroship`. **No `sqlite_`** |
| field (column) | `RESERVED_NAMES`, `query.rs:738-766` | `_`, `__zs_`, `__zeroship_`, **`sqlite_`**, `_masked` suffix, 6 classification names |

Verified 2026-08-28 by reading both blocks. Found by the `zeroship-data-plan`
port, which had to choose a role for each fence and could not justify this one.

**Why it matters here rather than in a lint:** the dev tier is SQLite
(`docs/reference/auth-dev-tier.md` describes the same substitution for
`env.db`), so a creator declaring a collection named `sqlite_events` passes
platform validation and is refused by the driver instead, with SQLite's own
"object name reserved for internal use" rather than a platform message naming
the rule. The failure is therefore a **dev-tier-only** error surfaced at the
wrong layer, which is the shape most likely to be reported as "the dev database
is broken".

**Not a privilege escalation, and the register should not imply one.** SQLite
refuses the CREATE itself, so nothing is created and nothing is shadowed. The
cost is a bad diagnostic and a fence that does not mean what it says.

**Fix:** move `sqlite_` to `validate_collection` and decide deliberately whether
it stays on columns as well. `zeroship-data-plan` fences it on **both** roles and
records the divergence from the shipped validator in its own comments; when the
port lands, one of the two behaviours has to win explicitly rather than by
whichever file the reader opened.

### L30 (NEW 2026-08-28) - the worker holds REPLICATION and BYPASSRLS, which decision 5 forbids

**The login role of the process that executes creator code holds two
cluster-scoped privileges.** Verified by reading the migration:

```sql
ALTER ROLE zeroship_worker WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
  INHERIT REPLICATION BYPASSRLS
-- db/migrations-ts/20260818000200_worker_database_authority.ts:35
```

Four lines below, the same file does the opposite for a role that does not need
them - `zeroship_workflow_owner ... NOINHERIT NOREPLICATION NOBYPASSRLS`
(`:39`) - so this is a deliberate grant, and the correct pattern is visible in
the same file for contrast.

**Why it is a defect and not a configuration choice.** `AGENTS.md`'s key
invariant, adopted by operator decision on 2026-08-27, says a privileged
capability held by the process running creator code "does not create a boundary;
it creates the *appearance* of one, because everything behind that capability is
reachable by whatever reaches the worker", and that anything genuinely privileged
"belongs to a separate service that does not execute creator code". `REPLICATION`
is cluster-wide - a replication connection is confined to one database only by
the server's own decode loop, not by any grant on the role - and `BYPASSRLS`
defeats row-level security outright. The worker is precisely the process the
invariant names.

**Why it cannot be fixed by narrowing the grant.** PostgreSQL has no finer
permission for slot consumption: `CheckSlotPermissions` is
`has_rolreplication(GetUserId())` and gates every slot function. So as long as
the worker consumes WAL itself, the worker holds `REPLICATION`. The only
remedy is to move consumption into a process that does not execute creator
code - which is the CDC relay that decision 5 already assigns slot and
publication ownership to.

**This is the strongest argument for resolving L12, and the four-option analysis
omits it.** That analysis weighs slot count and decode cost, and frames the
choice between a per-worker demux and a relay as "cross-tenant leak risk versus
none". Both of those options keep `REPLICATION` and `BYPASSRLS` on the
creator-code process; only the relay removes them. The relay is therefore not
merely the scaling answer - it is the only option that closes a violation that
is live today.

**Deliberately NOT fixed here.** Per the standing instruction to defer defect
repair until the proposal implementation lands, and because the fix is the relay
rather than an edit. Recorded so the L12 decision is made with it in view.

**Related, same file, not separately entered:** `BYPASSRLS` deserves its own
justification even after the relay lands, since it is unrelated to replication
and nothing in this set explains why the worker needs it.

---

## Reclassified from v3: not live defects

Two entries from v3 are reclassified rather than kept, because neither is a
defect with a failing pre-fix test.

### L7 is not a defect, it is a missing gate arm - and it is now partly fixed

The diagnostic the sanitization rail relies on ("diagnosable only from a
worker log", `dispatch.rs:245-246`) went to a discarded stream because no
integration binary installed a subscriber, so `RUST_LOG` had nothing to
configure. `native_transaction.rs:107` now installs one (commit `018291e36`),
which immediately surfaced the cause of four failures that had been opaque:
`permission denied for schema default`, from a `DROP SCHEMA ... CASCADE` in
the harness that destroyed the per-app grants without restoring them
(fixed in `8cbeda1c0`). Coverage is **1 of 9 binaries**, not 0. The remaining
eight are the gate arm.

Worth recording because it is corroboration rather than coincidence: that
harness bug is the **same failure mode** this proposal documents for restore -
`DROP SCHEMA CASCADE` destroys grants and `ALTER DEFAULT PRIVILEGES` entries,
and whatever recreates the schema must restore them. It was found from a
completely different direction.

### The v3 `get_column_key` finding was misattributed

*This entry was numbered L9 in v3. It keeps its text and loses the number: the
live L9 above (the masked-filter oracle, DECIDED) is a different defect, and
SC-6, the index and the design all cite that one. Two unrelated findings shared
one label until 2026-08-27.*

`install_get_column_key_function` is
`#[cfg(any(test, feature = "test-helpers"))]` (`bootstrap.rs:612`) - the same
gate L6 is about - so `get_column_key`'s `EXECUTE TO PUBLIC` over a
`key_id`-keyed table cannot be live in `main`. It is restated as a
**constraint on the step-7 provisioner**: when `__zeroship_admin` is stood up
for real, that function must not be recreated with its current grant or its
current keying.

**CLOSED 2026-08-27 by decision 1**, and closed more strongly than the
constraint asked for. The constraint said "do not recreate it with its current
grant or its current keying". The decision deletes the function and the table
outright: there is no per-column key to fetch, because there is one key per app
derived `HKDF(platform_master_key, app_id, key_version)`. A constraint on how to
rebuild something becomes moot when the thing is not rebuilt, and it is worth
recording that the weaker form was the one this register could see - "recreate
it correctly" is what you write when you have accepted the shape and are
policing its details.

## Findings from the 2026-08-27 operator decisions

Entries below were found while writing up the six operator decisions of
2026-08-27. They are separated from the numbered register because none is a
live defect in `main` - each is a **constraint on the step-5b provisioner**, the
same classification the `get_column_key` finding above carries, and for the same
reason: the code that would make them real is `#[cfg]`-gated out of production.

### The `pitr_targets` PUBLIC grant, and why it is not a live defect

`pitr_targets` is the **only** table in `__zeroship_admin` granted write to
`PUBLIC`. Every sibling ends at `REVOKE ALL ... FROM PUBLIC` -
`auth/bootstrap.rs:282` (`hmac_keys`), `:330` (`session_nonces`), `:371`
(`session_ctx`), `:412` (`column_keys`), `:513` (`mask_policies`) - and
`pitr_targets` issues that revoke at `:461` and then **adds it back**:

```sql
GRANT INSERT, UPDATE, SELECT ON "__zeroship_admin".pitr_targets TO PUBLIC
```

(`:466-473`.) The grant is deliberate and its comment says so: apps "can write
through" (`:431-433`). What they write through to is a row **the platform does
not act on**. The same comment block records that "the platform doesn't replay
WAL from a client connection (that requires server-level `recovery.conf`
setup); this table is the queue dashboards / the maintenance cron read"
(`:426-429`), and the installer repeats it: "only the API surface lives here"
(`:187-190`).

So the tenant can write an operator's recovery target, and the mechanism that
would consume it is out of band.

**Why this is a constraint and not a defect:** `ensure_pitr_targets_table` is
`#[cfg(any(test, feature = "test-helpers"))]` (`:435`), so neither the table nor
the grant can exist in `main` - for the same reason `__zeroship_admin` itself
cannot (L6). It matters because the step-5b provisioner is what would make it
real, and a provisioner written by porting the installer's statements would port
this one along with everything else.

**Decision 2 removes the question**: `pitr_targets` is deleted and PITR target
selection moves to the control plane, where operator state with an audit trail
belongs. `Backup::pitr_replay` (`backend/mod.rs:1370`) goes with it - its
PostgreSQL implementation does nothing but insert into this table
(`backend/postgres.rs:1702-1718`), its SQLite arm is a typed refusal
(`backend/sqlite/mod.rs:2362`), and its only callers in the tree are two tests
(`tests/integration.rs:5774,5792`; `tests/sqlite_integration.rs:6044,6057`).

### The slot wrappers were built and never wired

`install_slot_wrapper_functions` (`auth/bootstrap.rs:1076`) creates
`ensure_publication`, `ensure_slot`, the `ensure_publication_and_slot` procedure
and `watchdog`, all `SECURITY DEFINER`, to hold the `REPLICATION` privilege on
the worker's behalf (`:1064-1076`).

Its own comment claims "the test suite + the V8 callback layer call this"
(`:1180-1181`). **Only the test suite does.** The production path calls the raw
Rust `replication::ensure_publication_and_slot`, not the wrapper, and the
installer is `#[cfg]`-gated like the rest.

Recorded because the comment would mislead a deletion brief in the expensive
direction: it reads as though a V8-reachable privileged path exists today, which
would make decision 5's deletion urgent rather than merely correct. It is not
urgent. What decision 5 settles is that this capability does not come back as a
worker-callable function - slot and publication ownership belongs to the CDC
relay service (deferred with L12).
