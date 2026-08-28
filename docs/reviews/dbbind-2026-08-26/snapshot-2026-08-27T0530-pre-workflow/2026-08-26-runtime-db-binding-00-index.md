# Runtime DB binding: the document set

**Status: design substantially settled, one decision open, implementation
partly landed.** Written 2026-08-26, reorganised 2026-08-27 after seven review
rounds plus a dedicated performance round.

Read this page first. It says what each document is for, what state it is in,
and - more usefully - what is still undecided and what that blocks.

## The one open decision

**L12: live subscriptions cost one PostgreSQL logical replication slot per
(app x worker), against a server-wide ceiling of 10.**

This is not a defect inside the design. It puts the subscription transport in
question at the platform's stated scale, and SC-3's subscription surface sits on
top of it. Everything else in this set is either settled or fixable in place.

It stopped being theoretical on 2026-08-27: two replication tests failed during
a merge verification and passed unchanged in isolation, contending for
`max_replication_slots` and `max_wal_senders` - both 10, both server-wide, and
therefore **not** separable by giving each consumer its own database. Five
concurrent consumers saturate it on one development machine.

Four options and a recommendation are in the design document. The recommendation
is a **dedicated CDC service owning O(1) slots**, which also closes the
abandoned-slot cluster outage (L12b) by construction rather than by reaping.

**What it blocks:** finalising SC-3, and starting the IR implementation.
Building the IR against the current transport means building it twice.

## The documents

| document | what it is | state |
| --- | --- | --- |
| **`-design.md`** | The architecture: forks, contracts, step sequence, and the defect register | Revised through 7 rounds; 16 defects recorded |
| **`-verification-record.md`** | How this codebase's tests report green while ruling on nothing - five mechanisms, each with a dated measured instance | Extracted 2026-08-27; the most durable thing here |
| **`-sc1-transaction-protocol.md`** | Transaction state machine, frames and effects, guard order, the forcing-publisher gate, 15 property invariants | Executable; scope question open (see below) |
| **`-sc2-sqlite-actor-protocol.md`** | SQLite actor: reservations, the four cancellation interleavings, the terminal classifier | Complete |
| **`-sc3-dbplan-ir-and-ledger.md`** | The `DbPlan` IR, its source ledger, the parity harness | **Largest, least reviewed, blocked on L12** |
| **`-sc4-dev-and-hmr-mechanism.md`** | Dev tier and hot reload | **Thinnest; essentially unreviewed** |
| **`-sc5-service-ownership.md`** | `DbService`, process-wide cache, per-thread driver resources | Two unpassable acceptance arms fixed |
| **`-sc6-ceiling-read-contract.md`** | Authority reads, incarnation, and the masking storage flip | Carries a settled decision with four owed items |

## Decisions that are settled

**The three forks** (in the design document): SQLite serialises transaction
admission per thread-resource; an authority read never traverses the data
snapshot nor runs under the tenant role; app incarnation is a durable 128-bit id
qualified by `(system_identifier, timeline_id)` so a PITR rewind cannot
resurrect it.

**Masking storage is flipped** (SC-6). `ssn` stores the MASKED value, `ssn_raw`
stores plaintext and is **reserved and unqueryable** - not in a filter, not in a
projection, not in a sort, not a field of the generated type. Plaintext is
reachable only through an explicit API, where authorization and the audit row
already live. This replaced three weaker options that all left the design
fail-open. It owes three things, listed in SC-6: **lookup by plaintext must
survive** or the feature is closed rather than secured; unique indexes and
foreign keys must follow the raw column; and the AEAD tag binds the column name,
so this is a migration-engine change too.

**Connections always come from a pool** (design document, step 3). This reaches
the deprovision path, which currently builds a fresh pool per deleted app.
Logical replication connections are a genuine exception - they hold a streaming
protocol mode for their whole life, so there is nothing to multiplex - and the
rule for them is *bound and account*, which is L12.

**Full-text search is deleted.** The migration engine had already removed it;
three other layers still advertised it, and PostgreSQL had no producer at all.

## Known scope divergence

**SC-1's executable form is larger than SC-1.** The round-7 protocol artifact
(preserved at `docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md`, 12,113 lines)
requires a supervisor and a durable `FenceJobRegistry` across 105 lines, so that
terminal delivery survives process death. The contract mentions neither, and the
codebase contains neither.

Until "must terminal delivery survive process death?" is answered, SC-1's
implementation step is either *write a reducer* or *write a reducer plus a
durable job system*. That is a multiple, not a detail, and it is the kind of
divergence a step estimate silently absorbs.

## How to read the defect register

Sixteen numbered defects (L1-L16) live in the design document. Each carries
file:line evidence that was verified against the tree rather than taken from a
review - several reviewer claims were right about the defect and wrong about its
mechanism or magnitude, and the corrections are recorded where that happened.

Rows are marked FIXED with the commit, DECIDED with the choice, or left open.
The register shrinks as fixes land; it is the most perishable content in the set
and should not be read as design.

## What is implemented

Eighteen commits on `feat/dbbind-impl` at the time of writing, each carrying a
regression test **proven red by mutation**. Closed: the deploy-identity keying
(L10), the subscription cap (L13), the cross-tenant metadata-cache DoS and its
missing singleflight, the `insertMany` bind budget (L14), and atomic bounded
`updateMany` (L15).

**None of the design itself is implemented.** SC-1, SC-2, SC-3, SC-5 and SC-6
are at zero. What has landed is defect repair on existing code plus the test
harness that makes TDD on this design possible - which was not optional: five of
this crate's test targets are invisible to a default `cargo test`, the suite
aborted at test 77 of 99 on an oversized future, and ten tests skipped while
counting as passes.
