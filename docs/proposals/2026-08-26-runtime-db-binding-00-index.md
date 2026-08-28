# Runtime DB binding: the document set

**Read this page first.** It routes. It says what each document is for, what is
decided, what is blocked, and what is next. It deliberately does **not** argue
any of them - the arguments live in the documents below, and duplicating them
here is how this page went stale three times.

Written 2026-08-26. Restructured 2026-08-28.

---

## Status in one paragraph

The design is settled except for three open items, one of which blocks
implementation (**L12**, the replication-slot ceiling). Eighty-four commits on
`feat/dbbind-impl` have landed defect repair, the test harness that makes TDD on
this design possible, the descriptor cutover, and the platform migration move.
**SC-1 through SC-6 are still at zero** - what has moved is the ground they
stand on, not the design itself.

---

## The documents

| document | what it is | state |
| --- | --- | --- |
| `...-design.md` | The architecture: forks, contracts, invariants, step sequence | 1,751 lines. Current text only |
| `...-decision-log.md` | **Every decision this set has reversed, and why.** Superseded text lives here in full | 1,510 lines. Append-only |
| `...-defect-register.md` | Defects in EXISTING code this design touches | **15 live entries.** The most perishable file in the set |
| `...-defects-closed.md` | **The 14 closed defects, each with the commit that closed it** and what its closure proved | Append-only. Read it before re-opening anything |
| `...-verification-record.md` | How this codebase's tests report green while ruling on nothing - five classes, each with a dated measured instance | The most durable thing here |
| `...-sc1-transaction-protocol.md` | Transaction state machine, frames and effects, guard order, 15 property invariants | Executable; one scope question open |
| `...-sc2-sqlite-actor-protocol.md` | SQLite actor: reservations, the four cancellation interleavings, the terminal classifier | Complete |
| `...-sc3-dbplan-ir-and-ledger.md` | The `DbPlan` IR, its source ledger, the parity harness | **Least reviewed. Blocked on L12** |
| `...-sc4-dev-and-hmr-mechanism.md` | Dev tier and hot reload | **Thinnest; essentially unreviewed** |
| `...-sc5-service-ownership.md` | `DbService`, process-wide cache, per-thread driver resources, the durable app incarnation | Two unpassable acceptance arms fixed |
| `...-sc6-ceiling-read-contract.md` | The ceiling meet, joined reads, and the masking storage flip | Cut down by decision 4. **The flip is blocked - see below** |

**Two of these files are new, and they exist to keep this set honest.**
`decision-log.md` holds the superseded record: every claim this set has
retracted, in full, with what produced the error. `defects-closed.md` holds the
14 defects that are closed, each with its closing commit and the evidence.
Neither is archive. A closed defect that reappears in a review, or a decision
argued a second time from its retracted premise, is answered by these two files
and nowhere else.

---

## What blocks

### 1. L12: the replication-slot ceiling

Live subscriptions cost one PostgreSQL logical replication slot per
(app x worker), against a server-wide ceiling of 10.

**The ceiling is the weaker half of the argument.** Slots on one database do not
partition decoding work, they replicate it - measured on pg16, five slots each
decoding the same 40,002 changes, total 1,335ms against 309ms for one slot
(ratio 4.32 of a possible 5.00). The cost is `O(apps x total_WAL)` where the
information is `O(total_WAL)`. Raising the GUC buys a constant against a term
that should not exist.

**Blocks (CORRECTED 2026-08-28 - the earlier claim was too broad).** This page
said L12 blocks "finalising SC-3, and starting the IR implementation", and also
said, three sections later, that the plan crate unblocks SC-3 in parallel. Both
could not be true. **The narrow statement is the correct one:**

| SC-3 surface | blocked by L12? |
| --- | --- |
| shared normative core, read family, write, search, unmask | **No.** No transport dependency |
| relation family's **live-query** lowering | Yes - `read_set.rs` is relation-unaware |
| **effects** family (the publication a committed mutation owes the broker) | Yes - transport choice changes the node shape |

The existence proof is on disk: `crates/zeroship-data-plan` implements the core
plus the read family with **zero dependencies**, references neither
subscriptions nor CDC nor WAL, and builds and tests without a database.

**Recommendation, and the reason it is stronger than the register states:** a
dedicated CDC relay owning O(1) slots.

**The decisive argument is not slot count - it is a credential.** Consuming a
logical slot requires the connecting role to hold PostgreSQL's `REPLICATION`
attribute, and there is no narrower grant (`CheckSlotPermissions` is
`has_rolreplication(GetUserId())` and gates every slot function). This platform
grants it to the login role of the process that **executes creator code**:

```sql
ALTER ROLE zeroship_worker WITH LOGIN ... INHERIT REPLICATION BYPASSRLS
-- db/migrations-ts/20260818000200_worker_database_authority.ts:35
```

The line four below it does the opposite for another role
(`zeroship_workflow_owner ... NOREPLICATION NOBYPASSRLS`), so the correct
pattern is visible in the same file.

**So the CURRENT transport already violates decision 5**, the key invariant this
repository adopted on 2026-08-27: a privileged capability held by the process
that runs creator code "does not create a boundary; it creates the *appearance*
of one". `REPLICATION` is cluster-wide; `BYPASSRLS` defeats row-level security.
Only moving WAL consumption into a process that does not execute creator code
lets `zeroship_worker` drop both - which is the relay. A thinner "privileged
slot janitor the worker calls" is exactly the shape the invariant forbids.

This is recorded as **L30**. It reframes the relay from "the expensive option we
might defer" to "the option that closes a present-tense trust-model violation",
and it is the one argument the four-option analysis in the register omits.

**Also verified, in the PostgreSQL sources rather than from memory** (REL_16 and
REL_18, both read): a publication filter cannot reduce decode cost. `DecodeInsert`
applies only three pre-queue filters (TOAST-internal, other-database, replication
origin) and then queues the change; the plugin API exposes no filter-by-relation
callback at all, and `pgoutput` checks table membership and row filters at commit
replay, **after** decode, buffering, and per-slot spill to disk. The 4.32-of-5.00
multiplier is structural, not an artifact, and no publication design escapes it.

**Cheaper middle path that deserves weighing before scaffolding a new service:**
`zeroship-migrated` is already privileged and already owns publications, so it
could host the shared slots and publish to `zeroship-stream` without a greenfield
service. **And the honest case for deferring:** CHWBL already routes an app's
traffic to one worker, so at launch scale (app x worker) collapses toward
(app x 1), and raising the GUC would cover it. The relay still wins eventually on
both the multiplier and the credential; "eventually" could be post-launch.

**Unmeasured and must not be quoted until it is:** the added
WAL -> relay -> stream -> worker latency. That is the one genuine regression the
relay introduces.

### 2. SC-6's masking storage flip is blocked on a defect in itself

The flip (`ssn` holds the mask, `ssn_raw` holds plaintext) is designed and
**must not be implemented as specified.** Its write path is unguarded in both
directions:

- **Outward:** `RETURNING *` returns every physical column and never passes
  through `implicit_read_projection_parts` (SELECT-side only). The stripper
  knows exactly one sibling name, `format!("{col}_masked")`
  (`crud/mask_pass.rs:469`, and its write-side twin at `:150`), so post-flip
  `<col>_raw` survives into the result document. For a mask-only field that is
  **plaintext returned from every write verb.**
- **Inward:** `_raw` is absent from `RESERVED_NAMES`, and the filter builders
  call only `validate_field_name` with no schema hint. Raw is unreadable but
  fully filterable.
- **Collision (found 2026-08-28, missed by every review so far):** because
  `_raw` is not reserved, **a creator can declare a column named `ssn_raw`
  today.** After the flip that creator-declared column occupies exactly the name
  the platform intends to write plaintext into. This is the worst of the three,
  and the reason is that the symmetric protection **already exists and was
  simply not extended**: `ReservedName::Suffix("_masked")`
  (`zeroship-schema/src/query.rs:754`) is the *only* sibling-suffix reservation
  in either crate, and its own comment says it is refused at both
  schema-registration time and filter time precisely so a creator cannot shadow
  a platform sibling. Measured: `_raw` occurs **once** in all 12,104 lines of
  `query.rs`, at `:11348`, **in a doc comment**. There is no code anywhere that
  knows the name.

  **The fix is one line** - `ReservedName::Suffix("_raw")` beside the `_masked`
  entry - and it must land **before** the flip, not with it. It is breaking for
  any creator holding a `*_raw` column, which pre-launch costs nothing.

**The flip's blast radius is larger than any document here states, and the
missing part is the MIGRATION side.** The descriptor generator's own doc
(`migrate-core/src/render/gen_types/physical_storage.rs:5`) says
`format!("{col}_masked")` "appears at **eight** independent sites listed in
`docs/reviews/2026-08-27-descriptor-specification.md` section 1.4". Two
corrections, both measured 2026-08-28:

- **The cross-reference is wrong.** Section 1.4 lists **four** sites (items
  33-36). The eight span sections 1.3, 1.4 and 1.5.
- **The eight is scoped to the DATA PLANE, and is about right there.** But that
  review's stated scope is "every *data-plane* consumer", so it correctly omits
  the side that **creates** the column. Measured across the four crates that know
  the name, **31 sites construct or destructure it** (includes inline test
  modules). Six of those are on the migration side and are counted nowhere in
  this set: `migrate-core/src/schema/diff.rs:410`, `:747`, `:825`, `:847`,
  `migrate-backend/src/schema.rs:566`, and a **second, independent**
  `ReservedName::Suffix("_masked")` at `migrate-core/src/schema/query.rs:379`.

  That second reservation list matters on its own: the flip's one-line `_raw`
  fix has to land in **two** places, and a reader who greps `zeroship-schema`
  alone will find one of them.

  This is also why SC-6's owed item 4 is not a detail. The AAD binds the physical
  column name, so moving ciphertext is a re-encrypt rather than a rename - and
  the emitter that would do the moving is exactly this uncounted migration-side
  set.

**Measured 2026-08-28: 12 SQL-emitting `RETURNING *` sites** in
`zeroship-schema/src/query.rs` (12,104 lines). A bare `grep -c` reports 34; of
those, 14 are in tests and 8 more are `///` doc comments. *This page carried the
34 until it was re-derived. A count without its boundary is not a measurement.*

**So implementing the flip retires a known leak and opens a worse one** - worse
because it is invisible to any review written against the generated types.
Three further silent breakages are verified in SC-6.

**Blocks:** SC-6. Full detail there under "BLOCKING".

### 3. SC-1's executable form is larger than SC-1

The round-7 protocol artifact requires a supervisor and a durable
`FenceJobRegistry` so terminal delivery survives process death. The contract
mentions neither and the codebase contains neither.

**This one is the operator's:** *must terminal delivery survive process death?*
Until it is answered, SC-1's implementation step is either *write a reducer* or
*write a reducer plus a durable job system*. That is a multiple, not a detail.

---

## What is decided

Eight operator decisions, 2026-08-27. One line each; each is written out in the
design beside the claim it replaces, and its **reversal history is in
`decision-log.md`.**

| # | Decision |
| --- | --- |
| 1 | **No per-column encryption keys.** One key per app, `HKDF(platform_master_key, app_id, key_version)` |
| 2 | **`__zeroship_admin` was four concerns under one name.** PITR targets move to the control plane |
| 3 | **The mask policy is code-managed** and immutable for the isolate's life |
| 4 | **The operator ceiling is worker configuration**, changed by rolling the workers |
| 5 | **Privilege follows the PROCESS, not the function.** Now a repository key invariant (`AGENTS.md`) |
| 6 | ~~One resident is left~~ **superseded by 7 within hours** |
| 7 | **`__zeroship_admin` is deleted ENTIRELY, and there is no schema epoch.** The runtime descriptor is the authority |
| 8 | **The data plane performs NO live introspection at all.** Startup DDL validation is deferred, not designed |

**Five of these reverse text that read as settled.** Decisions 7 and 8 are the
largest: together they turn a stated **non-goal** ("the runtime descriptor does
not become the physical database authority") into the design's central claim.

**What 7 and 8 cost, stated because a decisions list that reads as pure gain is
the shape this set exists to avoid:**

- **Mid-life drift is no longer detected.** A migration applied while a worker
  runs leaves it serving against a database its descriptor no longer describes.
  **Roll the workers after a restore** is now a procedure, not a mechanism.
- **The deploy pipeline's ordering guarantee is load-bearing.** If a deploy goes
  live before its migration applies, the descriptor says `ssn` is masked while
  the database still holds the real value there, and **the runtime serves
  plaintext believing it is masked. Nothing detects it.**
- **Subscribers lose their schema-change signal**, and nothing replaces it.
- **Fork C's identity state has no home.** `incarnation`, `deprovisioned_at` and
  the tombstone rule lived in `app_schema_state`. They are not schema metadata -
  DDL validation could not answer them even if it were not deferred, because two
  incarnations of one app id have the *same* schema. Specified and unhomed.

**One thing decision 1 owes:** the key version has **no per-row carrier**, so
"lazy re-encryption on write" is not implementable as stated. Decision 1 is
sound without rotation; its rotation half is blocked.

### Decided 2026-08-28: the crate split is two crates, not six

`zeroship-plugin-db` gains **two** sibling crates, not a family. The test
applied: *what does a crate boundary buy that a module boundary does not?* Only
three things - a dependency the compiler refuses to invert, a separate
compilation unit, or an artifact another process consumes.

| crate | verdict |
| --- | --- |
| plan / IR | **build.** Pure leaf: no I/O, no V8, testable without a database; unblocks SC-3 in parallel |
| CDC | **build.** It becomes a separate *service* (decision 5, L12's recommendation). A process boundary, not a taste boundary |
| error, crypto | **modules.** Nothing outside plugin-db consumes them; crypto's pair (`mask_codec.rs`) is already in `zeroship-schema` |
| per-backend | **defer.** The five capability traits in `backend/mod.rs:41` already are the seam. Having the seam is the win; splitting before a third backend exists buys nothing |

*The workspace already carries 35 crates, 11 of them `zeroship-migrate-*`. That
family was the precedent the six-crate plan copied, and at eleven it is the
counter-example rather than the model.*

---

## What is implemented

**Eighty-four commits on `feat/dbbind-impl`.** Defect repair on existing code,
plus the harness that makes TDD on this design possible - which was not
optional: five of this crate's test targets were invisible to a default
`cargo test`, the suite aborted at test 77 of 99, and ten tests skipped while
counting as passes.

The four largest:

- `632c1d1fa` - the descriptor becomes the **sole** schema authority. Live
  introspection deleted (`crud/introspect_schema.rs` 1,054 lines,
  `live_metadata.rs` 517), replaced by a 129-line descriptor reader.
- `d92efa740` - all 35 platform migrations rewritten to the `schema()` form,
  DDL-neutral and measured: IR envelopes byte-identical across all 35, and a
  one-variable control (the 35 originals through the new glue) refusing 35/35.
- `2690b5a16` - `zero-migrate baseline`, the verb that lets the rewritten corpus
  be adopted by a database that already holds its schema.
- `a91b690d6` - adoption now diffs full snapshots. It had compared **table names
  only**, so adopting a peer environment a few migrations behind passed every
  check and journalled the trailing migrations as applied without running them.

**The branch is green on an instrument that can fail** (`b801bf12b`), twelve
arms plus clippy. That qualifier is the point. Every earlier "all arms green" on
this branch was false in a specific way, and each way hid a real defect:

| what the instrument could not see | what it hid |
| --- | --- |
| a long-lived database carrying state the commits deleted | the L25 regression, 7 tests |
| `distributed_live` excluded while printing "all arms green" | L27, red for 11 days |
| roles are cluster-scoped, so a fresh database is not isolation | a `42710` masking the real cause |
| cargo aborts after the first failing target | the v2 golden failures |
| a feature no default build compiles | L28, blocking 26 harnesses |

**Two commits carried regressions that shipped** - `390f4b97b` (deleted a
schema, kept two callers) and `a5832708b` (bumped the descriptor, left v1
fixtures in another crate). Both fixed. In both cases the cause was the same:
**verification scoped to the crate being edited, while the change crossed
crates.**

---

## What is exactly zero

**SC-1, SC-2, SC-3, SC-4, SC-5, SC-6.** All six.

**And the masking storage flip is not in the tree** -
`mask_sibling_column_for_field` still returns `format!("{field}_masked")` and
nothing anywhere spells `_raw`. The descriptor work deliberately recorded
today's real layout rather than a layout no migration creates, and pinned the
two together with a test that fails if they diverge.

---

## How this set corrects itself

**Superseded text does not stay inline.** It moves to `decision-log.md`, whole.
This page and the design carry current text only.

That rule exists because the v4 design ran two correction mechanisms at once and
carried 43 retraction markers. At that density the document was not read
linearly, it was grepped - and interleaved retractions make superseded text
exactly as findable as current text. **The cost of the split is real and was
accepted deliberately:** two documents can now disagree, where one document
could not drift against itself. `decision-log.md` being append-only is what
bounds that.

**Citing a commit requires `git merge-base --is-ancestor <sha> HEAD`.** Reading
the commit is not enough - a dangling SHA reads perfectly. This page cited four
that are on no branch (`4c8e84134`, `72a3dc04b`, `f44bcf6b6`, `fc2c889db`);
their real commits are `725039c5d`, `0e2cf69fb`, `15f9305e2` and `5511b6ebf`.
The failure mode is worse than a wrong number: a re-check run against a dangling
SHA once produced three fresh-looking citations that were real line numbers of
the wrong tree, and **a wrong measurement corrected a right document.**

**A count carries its boundary or it is not a measurement.** See the 34-vs-12
correction above.
