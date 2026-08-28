# Runtime DB binding: the document set

**Status: design substantially settled, one decision blocking, implementation
partly landed.** Written 2026-08-26, reorganised 2026-08-27 after seven review
rounds plus a dedicated performance round. Both are evidenced on disk in
`docs/reviews/dbbind-2026-08-26/` - the round-7 artifacts (`dbbind-r7-codex.md`,
`dbbind-r7-fable.md`, `dbbind-r7-opus.md`) and the performance round
(`perf-r1-*.md`). Anywhere a document in this set states a different round
count, that directory is the authority.

Read this page first. It says what each document is for, what state it is in,
and - more usefully - what is still undecided and what that blocks.

## The open decisions

**Two of them block.** The rest are open but not blocking, and they are listed
here rather than left implicit in a settled-sounding section.

*(An earlier revision of this page said "one of them blocks" and named only L12.
That was wrong: the WAL epoch carrier below is called "Blocked until..." in the
design document's own step sequence, and this page never mentioned it. Both
blockers concern the same subsystem - the CDC path - which is probably why one
absorbed the other in the writing.)*

### The WAL epoch carrier (blocking)

CDC events must carry a schema epoch, so a subscriber can tell that a migration
happened and resync rather than silently applying post-migration rows under a
pre-migration understanding of the schema. **The production WAL producer has no
epoch in scope**, and how an epoch travels with a WAL event is deliberately left
open (`-design.md:1987-1989`, `:2036-2043`).

**What it blocks:** step 8 of the implementation sequence - CDC source-epoch
handling, per-subscription projection, and resync - which "cannot start on the
strength of the local-path mechanism alone".

**And it carries a live test hazard that must be handled whichever way the
decision goes.** One acceptance criterion has two halves: a mask-plaintext half
that *is* implementable, and an epoch half that is not, until the carrier
exists. Left joined, the whole criterion can be reported **green by a test that
exercises only the implementable half**. They must be split, so the epoch half
fails loudly while the carrier is undecided. This is the
verification-record's own failure mode appearing inside the design's acceptance
list.

### L12: the replication-slot ceiling (blocking)

**L12: live subscriptions cost one PostgreSQL logical replication slot per
(app x worker), against a server-wide ceiling of 10.**

This is not a defect inside the design. It puts the subscription transport in
question at the platform's stated scale, and SC-3's subscription surface sits on
top of it.

**The ceiling is real and the arithmetic below stands. An earlier revision of
this page also claimed it "already binds on one dev box", and that claim was
WRONG - retracted 2026-08-27 after measuring it.**

What happened: two replication tests failed during a merge verification and
passed unchanged in isolation, and this page attributed that to slot exhaustion.
Measured afterwards, a consumer holds a **peak of ONE slot**, so the five
concurrent consumers on that machine drew five of ten - not close to the
ceiling. The real cause was a test-cleanup blast radius, recorded separately in
the register; the error itself said `START_REPLICATION` failed, whereas
exhaustion would have failed at slot *creation*. The error never matched the
hypothesis and it took a measurement to notice.

Retained because it is the more useful lesson: **an anecdote that agrees with a
conclusion you already hold is the easiest evidence to accept without checking.**
L12 needs no help from that incident. One slot per (app x worker) against a
server-wide ceiling of 10, on a restart-only GUC where each slot is also a
walsender competing for `max_connections`, does not reach millions of apps by
any arithmetic.

Four options and a recommendation are in the defect register, under
`### L12: what the ceiling of ten actually rests on, and the four ways out`. The
recommendation is a **dedicated CDC service owning O(1) slots**, which also
closes the abandoned-slot cluster outage (L12b) by construction rather than by
reaping. It remains a recommendation; nothing has decided it.

**What it blocks:** finalising SC-3, and starting the IR implementation.
Building the IR against the current transport means building it twice.

### Open beside L12

**SC-6's storage flip owes four items** (see "Decisions that are settled"). The
first of them - lookup by plaintext must survive, or the feature is closed
rather than secured - is a design question, not an implementation detail.

### Known scope divergence: SC-1's executable form is larger than SC-1

The round-7 protocol artifact (preserved at
`docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md`, 12,113 lines) requires a
supervisor and a durable `FenceJobRegistry`, so that terminal delivery survives
process death. The contract mentions neither, and the codebase contains neither.

Until "must terminal delivery survive process death?" is answered, SC-1's
implementation step is either *write a reducer* or *write a reducer plus a
durable job system*. That is a multiple, not a detail, and it is the kind of
divergence a step estimate silently absorbs.

*(An earlier revision of this page put a line count on that requirement, taken
from SC-1. It does not reproduce when re-measured against the same artifact, so
the number is dropped here rather than repeated. The same figure appears in
SC-1 and is owed the same correction.)*

## The documents

Full filenames, because a task brief written from an earlier revision's
leading-hyphen shorthand named a file that does not exist on disk.

| document | what it is | state |
| --- | --- | --- |
| `2026-08-26-runtime-db-binding-design.md` | The architecture: forks, contracts, invariants, step sequence | Revised through 7 rounds; live defects and the verification record now live in their own files |
| `2026-08-26-runtime-db-binding-defect-register.md` | Defects in EXISTING code that this design touches, plus the open L12 decision | 23 entries (L1-L6, L8-L23, plus L12b); the most perishable file in the set |
| `2026-08-26-runtime-db-binding-verification-record.md` | How this codebase's tests report green while ruling on nothing - five classes, each with a dated measured instance | Extracted 2026-08-27; the most durable thing here |
| `2026-08-26-sc1-transaction-protocol.md` | Transaction state machine, frames and effects, guard order, the forcing-publisher gate, 15 property invariants | Executable; scope question open (see above) |
| `2026-08-26-sc2-sqlite-actor-protocol.md` | SQLite actor: reservations, the four cancellation interleavings, the terminal classifier | Complete |
| `2026-08-26-sc3-dbplan-ir-and-ledger.md` | The `DbPlan` IR, its source ledger, the parity harness | **Least reviewed, and blocked on L12** |
| `2026-08-26-sc4-dev-and-hmr-mechanism.md` | Dev tier and hot reload | **Thinnest; essentially unreviewed** |
| `2026-08-26-sc5-service-ownership.md` | `DbService`, process-wide cache, per-thread driver resources, the durable app incarnation | Two unpassable acceptance arms fixed |
| `2026-08-26-sc6-ceiling-read-contract.md` | Authority reads, joined reads, and the masking storage flip | Carries a settled decision with four owed items |

*(An earlier revision called SC-3 "largest". It was not - SC-6 was larger by
bytes - and the two have since traded places again while this set is being
restructured, so the claim is dropped rather than corrected to a number that
goes stale on the next edit. "Least reviewed" and "blocked on L12" are the parts
that matter and they both stand.)*

## Decisions that are settled

**The three forks** (in the design document): SQLite serialises transaction
admission per thread-resource; an authority read never traverses the data
snapshot nor runs under the tenant role; app incarnation is a durable 128-bit id
qualified by `(system_identifier, timeline_id)` so a PITR rewind cannot
resurrect it. The third fork is specified in **SC-5**, not SC-6.

**Full-text search is deleted** (L11). Not "restore a PostgreSQL producer" - the
migration engine had already removed FTS deliberately, and this finishes the
removal in the three layers that still advertised it. Implemented on
`feat/db-delete-fts` (two whole files and references across seven others; lib
630/0, integration 89/0/6, clippy 0, `pnpm build` 0, `db/` and the released
migration ledger untouched). **Not yet merged**, so the working branch still
carries the symbol in 14 files; L11 retires when that merge lands and is
verified.

*(This page previously said the decision was "not settled and is not this
design's to make". The observation was settled and the decision was not - until
it was made and implemented, and this page was not updated. The gap was that the
work was dispatched without the decision being written down anywhere in the set.)*

**Masking storage is flipped** (SC-6). `ssn` stores the MASKED value, `ssn_raw`
stores plaintext and is **reserved and unqueryable** - not in a filter, not in a
projection, not in a sort, not a field of the generated type. Plaintext is
reachable only through an explicit API, where authorization and the audit row
already live. This replaced three weaker options that all left the design
fail-open. It owes **four** things, listed in SC-6:

1. **lookup by plaintext must survive** or the feature is closed rather than
   secured;
2. unique indexes and foreign keys must follow the raw column, or uniqueness is
   enforced over masks and many rows legitimately share `***-**-1234`;
3. **equality search by real value stops matching** -
   `find({ssn: "123-45-6789"})` returns nothing after the flip. That is correct
   and it is creator-visible, so it belongs in `docs/reference/db.md` beside the
   mask kinds;
4. the **AAD** binds the column name (`canonical_aad(collection, column,
   row_pk)`), so renaming the physical column changes the tag - a
   migration-engine change, not only a runtime one.

*(Item 3 was missing from an earlier revision of this page, which listed three.
It is the one of the four a creator notices first. Item 4 said "AEAD tag"; the
mechanism is the AAD.)*

**Connections always come from a pool** (design document, step 3). This reaches
the deprovision path, which currently builds a fresh pool per deleted app.
Logical replication connections are a genuine exception - they hold a streaming
protocol mode for their whole life, so there is nothing to multiplex - and the
rule for them is *bound and account*, which is L12.

## How to read the defect register

Twenty-three numbered entries live in
`2026-08-26-runtime-db-binding-defect-register.md`, not in the design document:
L1-L6 and L8-L23, plus L12b. L7 and one v3 entry are reclassified at the bottom
as not-defects. Each carries file:line evidence that was verified against the
tree rather than taken from a review - several reviewer claims were right about
the defect and wrong about its mechanism or magnitude, and the corrections are
recorded where that happened.

Status lives in the entry heading: FIXED, PARTLY FIXED, DECIDED, NEW, or
nothing. **Only L10 names its commit** (`4c8e84134`); L5 and L8 name the symbol
their fix landed in, and L18 names the commit for the two halves that landed.
The register shrinks as fixes land; it is the most perishable content in the set
and should not be read as design.

## What is implemented

Eighteen commits on `feat/dbbind-impl` at the time of writing - ten `fix`/`perf`
commits, and eight `test`/`docs`/`chore` commits that built the harness those
fixes needed. Where a fix carried a regression test it was proven red by
mutation, and the entry says so; that claim does not run across all eighteen,
which is what an earlier revision of this page implied.

**Closed, re-verified against the tree on 2026-08-27** rather than carried
forward from an earlier revision:

- **L10, the deploy-identity keying** - `4c8e84134`. Runtime schema metadata is
  keyed by `DbBinding { app_id, deploy_token }`, and `IsolateDbContext` is
  renamed `ThreadDbContext`.
- **L18's two landable halves** - `f44bcf6b6`. A per-populate admission cap
  (`MAX_COLLECTIONS_PER_POPULATE = 32`) and a per-key singleflight on cold
  misses. The third half - a per-app sub-map and any total bound at all - did
  **not** land; `introspected_schemas` is still one flat per-thread `HashMap`
  with no eviction path.
- **The meter's unbounded growth** (L20's first half) - `72a3dc04b`. The stall
  itself was deliberately left alone.
- **The racing test suite** - `fc2c889db`, plus the harness and lint work the
  verification record describes.

**Reported closed by an earlier revision of this page and NOT closed.** All
three were re-checked against the tree on 2026-08-27; each is still exactly as
its register entry describes, with the citation drift noted there:

- **L13, the subscription cap.** `v8_classes/subscription.rs:314` still calls
  the infallible `broker::subscribe`; `MAX_SUBSCRIPTIONS_PER_APP` is still
  reachable only from `try_subscribe`.
- **L14, the `insertMany` bind budget.** `MAX_INSERT_MANY_BATCH` is still
  `1_000` **documents** at `zeroship-schema/src/query.rs:601`, and the wall its
  comment cites still counts binds.
- **L15, atomic bounded `updateMany`.** `crud/mod.rs:1279` still calls
  `resolve_target_row_ids(..., None)`.

That list is the most dangerous thing this page can get wrong, which is why it
now names the evidence for each line rather than a status word.

**None of the design itself is implemented.** SC-1 through SC-6 are all at zero.
What has landed is defect repair on existing code plus the test harness that
makes TDD on this design possible - which was not optional: five of this crate's
test targets are invisible to a default `cargo test`, the suite aborted at test
77 of 99 on an oversized future, and ten tests skipped while counting as passes.
