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

### 1. L12: DECIDED 2026-08-28 - build a new CDC service

**Operator decision: a dedicated CDC service is being built, and every CDC
defect is deferred into it rather than patched in place.** L12 is therefore no
longer an open question; what follows is retained because it is the requirement
list the new service inherits.

**Three findings close by construction, not by repair:**

| finding | how the service closes it |
| --- | --- |
| **L30** - `zeroship_worker` holds `REPLICATION` and `BYPASSRLS` | consumption moves to a process that runs no creator code, so the worker's role drops both. There is no narrower grant, so this was unfixable any other way |
| **L12b** - a crashed worker's abandoned slot can take down the cluster | O(1) service-owned slots, so there is no per-worker slot to abandon and the reaper is deleted rather than fixed |
| **the mask-only CDC leak** (this cycle) | the wire projection is designed once, in one place, instead of retrofitted onto a path with zero mask awareness |

**Requirements the new service must carry - each measured, not asserted:**

1. **A wire projection that is a WHITELIST over declared fields.** Today
   `exec.rs:531-541` maps every key and `wal_consumer.rs` has zero mask
   awareness, so a mask-only field's plaintext parent reaches every subscriber.
   The whitelist must also cover the broker's `changed_columns`, which names
   columns even where values do not escape.
2. **A test fixture for the MASK-ONLY shape.** The existing contract test
   (`broker.rs:1863`) rules only on ciphertext under a name claiming the general
   property. Whatever replaces it must fail on a plaintext parent.
3. **The schema-change signal.** Decisions 7 and 8 left subscribers with none.
   A long-lived relay can stamp `(app, incarnation)` at produce time, which
   dissolves the WAL-epoch carrier problem rather than solving it - the reason
   the register says to decide the carrier together with L12.
4. **Leader election and resume**, without tokio: advisory-lock election over
   `compio-postgres`, resume from `confirmed_flush_lsn`, at-least-once with LSN
   dedup. All apps share one database here, so this is **one leader per
   cluster** - genuinely O(1), not O(databases).
5. **`max_slot_wal_keep_size` must be set.** It is unset today. Without it,
   relay-down means the shared cluster's disk fills; with it, relay-down
   degrades to a `Resync`, which the broker already models as a first-class
   event.
6. **Measure the added latency; do not estimate it.** WAL -> relay -> stream ->
   worker is the one genuine regression the service introduces, and no figure
   for it exists anywhere in this set.

**What it does NOT need to re-derive:** the decode multiplier is structural.
Verified in the PostgreSQL sources (REL_16 and REL_18): publication and row
filters run at commit replay, **after** decode, buffering and per-slot spill,
and the output-plugin API has no filter-by-relation callback at all. One decode
per database is the floor PostgreSQL sells, and the service reaches it.

*(Everything below is the superseded analysis that led here.)*

### 1a. L12 as it stood while open

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
  the side that **creates** the column.

  **Re-derived 2026-08-28 with the boundary stated: 22 PRODUCTION sites** across
  four crates, from 41 total hits (22 production, 3 in `test-helpers`-gated
  modules, 9 inline `#[cfg(test)]` assertions, 1 integration target, 6 prose).
  *An earlier revision of this page said 31, which counted test code as
  production - the same class of error as the 34-vs-12 above, made by me on the
  same day I corrected that one.*

  | crate | production sites |
  | --- | --- |
  | `zeroship-schema` | 9 - `query.rs:754`, `:2160`, `:3414`; `diff.rs:582`, `:671`, `:716`, `:1203`, `:1286`, `:1308` |
  | `zeroship-migrate-core` | 9 - `schema/query.rs:379`; `schema/diff.rs:410`, `:747`, `:825`, `:847`; `render/fold.rs:3678`; `render/lower.rs:4803`, `:7185`; `render/declarative.rs:2223` |
  | `zeroship-plugin-db` | 3 - `crud/mask_pass.rs:150`, `:469`; `crud/encryption_pass.rs:295` |
  | `zeroship-migrate-backend` | 1 - `schema.rs:566` |

  **The migration side is TEN, not the six this page previously named** - four
  more live in the fold/lower/declarative pipeline and had been counted nowhere.
  It also carries a **second, independent** `mask_sibling_column_for_field`
  (`migrate-backend/src/schema.rs:557-566`) and a **second** reservation table
  (`migrate-core/src/schema/query.rs:379`). There is **no dependency edge**
  between the two forks, so nothing can make them agree by construction.

  That second reservation list matters on its own: the flip's one-line `_raw`
  fix has to land in **two** places, and a reader who greps `zeroship-schema`
  alone will find one of them.

**RETRACTED 2026-08-28: SC-6's owed item 4 is WRONG ABOUT THE CODE, and this
page repeated it.** Both said the AAD binds the PHYSICAL column, making the flip
a re-encrypt rather than a rename. Measured: `canonical_aad(collection, &col,
..)` is passed `col` from `for (col, def) in schema_obj.iter()`
(`crud/encryption_pass.rs:173`, `:200-207`) - **the logical schema field key**,
not `storage.rawColumn`. The same holds at `encryption_pass.rs:337` and
`crud/unmask.rs:452-458`.

Today the two are the same string for every masked+encrypted field, so "binds
the logical name" and "binds the physical column" are **indistinguishable in the
current tree**. Post-flip they diverge, and which one it is was never decided.

**So the flip is not necessarily a re-encrypt. It is an unmade decision, and the
cheap branch is available:** keep binding the logical field name and zero rows
are re-encrypted, because a `RENAME COLUMN` does not change the logical key.

The false constraint is stated as fact in a doc comment
(`migrate-core/src/render/gen_types.rs:160-169`: "`canonical_aad`
length-prefixes the column name ... an `ALTER TABLE ... RENAME COLUMN` leaves
every stored cell authenticated under the old name"). That comment describes
behaviour the code does not have, and it is what made an expensive migration
look mandatory. It must be corrected in the same commit as any flip work, or the
next reader "fixes" the AAD to match the comment and destroys every ciphertext
in the deployment.

**Neither branch is right, though.** Binding the bare logical name stops being
sufficient the moment SC-6's own owed item 2 lands: a keyed lookup column holds
a SECOND deterministic ciphertext for the SAME logical field, so two encrypted
columns share one AAD and swapping their contents passes tag verification. Bind
the logical field name **plus a stable role discriminator** (`value` | `lookup`).
A role survives renames; a physical name does not.

**Measured 2026-08-28: 12 SQL-emitting `RETURNING *` sites** in
`zeroship-schema/src/query.rs` (12,104 lines). A bare `grep -c` reports 34; of
those, 14 are in tests and 8 more are `///` doc comments. *This page carried the
34 until it was re-derived. A count without its boundary is not a measurement.*

**So implementing the flip retires a known leak and opens a worse one** - worse
because it is invisible to any review written against the generated types.
Three further silent breakages are verified in SC-6.

**The flip is not a rename plus a backfill. It swaps which column carries the
declared TYPE and the entire constraint set** - found 2026-08-28, and absent
from SC-6, from this page, and from the 966-line write-path review, all three of
which were grepped for `CHECK`, `NOT NULL`, `enum` and `DOUBLE PRECISION`.

`.mask()` is legal on string, number and bytes, and every mask kind returns a
**String**. Today that is harmless: the sibling is bare `TEXT` while the field's
own column keeps its declared type and all its constraints from
`def_to_constraints_for_dialect` (`query.rs:2719-2827`) - `NOT NULL`, `DEFAULT`,
range `CHECK`, literal `CHECK`, enum `CHECK`. Post-flip the logical column holds
`'***'`:

- `t.number().mask(...)` leaves `ssn` as `DOUBLE PRECISION`; writing `'***'` is
  a hard error.
- `t.string().enum([...]).mask(...)` leaves `CHECK ("ssn" IN (...))`, which
  refuses `'***'`. **Every write fails.**
- an encrypted+masked field leaves `ssn` as `BYTEA`.

**And the migration engine cannot see any of it.** `migrate-core/src/schema/diff.rs:856-882`
says so in its own comment - the column-additions branch is "NAME-ONLY ... no
matter how its declared type has changed" - and the `RewriteColumnType` arm keys
strictly off the `encrypted` toggle (`:894-900`). The flip changes neither the
name nor that toggle, **so the differ emits nothing at all**, the DDL and the
runtime silently disagree, and the runtime writes a mask string into a numeric
or binary column. That is precisely the failure `RewriteColumnType` exists to
prevent, arriving through the one door it does not watch.

For an EXISTING table a double rename fixes this for free, because types and
constraints travel with the renamed columns:

```sql
ALTER TABLE t RENAME COLUMN ssn        TO ssn_raw;  -- free the logical name first
ALTER TABLE t RENAME COLUMN ssn_masked TO ssn;
```

For a NEW table, `build_create_table_with_fks_for_dialect` must emit the declared
type and constraints under the **raw** name and a bare `TEXT` sibling under the
**logical** name. Nothing does that today.

### The flip's value is REAL, and an earlier revision of this page understated it

Measured 2026-08-28, because the case for cancelling the flip was made here
without testing its strongest counter-argument.

`read_pipeline::apply` runs the mask pass only `if schema_has_masked_columns(&schema)`,
and that predicate reads **the descriptor** (`crud/mod.rs:2590-2602`). So when
the descriptor does not declare a field masked, no mask pass runs and the parent
column's contents pass through untouched. Therefore:

| | descriptor declares the mask | descriptor does NOT (stale) |
| --- | --- | --- |
| **today** (parent = plaintext) | masked | **PLAINTEXT RETURNED** |
| **post-flip** (parent = mask) | masked | **mask returned - safe** |

**That is exactly the hazard decisions 7 and 8 created**, and this page already
names it: *"If a deploy goes live before its migration applies, the descriptor
says `ssn` is the masked column while the database still holds the real value
there, and the runtime serves plaintext believing it is masked. Nothing detects
it."* The flip closes it by making the safe value the one physically present.

So the decision is a genuine trade, not the lopsided one an earlier revision
implied: **real protection against the one failure mode with no runtime check,
against 22 production sites over two forks with no dependency edge, a
type-and-constraint swap that breaks numeric and enum masked columns, and a DDL
migration the differ is structurally blind to.**

### A third option neither document considers: enforce the ordering instead

The flip defends against a **violation of the deploy ordering**. The cheaper
move is to stop the violation. The descriptor is built at deploy time and the
deploy pipeline already owns both halves, so the pipeline can **refuse to make a
deploy live until its migrations have applied**, turning "PostgreSQL migrations
are applied before a deploy becomes live" from a description into an enforced
precondition.

That costs one check at one place, versus a storage change at 22 sites that the
migration engine cannot verify. It does not cover a *restore* (which the design
already answers with "roll the workers"), and it is weaker than the flip in
exactly the case where the pipeline itself is buggy - so it trades defence in
depth for cost.

**Recommendation, revised: enforce the ordering first, and treat the flip as a
separate decision made on its own merits afterwards** - because the ordering
guard is cheap, is needed whether or not the flip lands, and removes the single
argument that currently makes the flip look mandatory.

**Blocks:** SC-6. Full detail there under "BLOCKING", and in
`docs/reviews/2026-08-28-flip-write-path.md` (966 lines, in the tree, and
referenced from neither SC-6 nor this page until now).

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

## What is actually built, per sub-contract

**RETRACTED 2026-08-28: this section said "SC-1 through SC-6 are exactly zero.
All six."** That is false, and worse, the design had **already retracted the
same sentence** (`design.md:1183-1188`: *"it is now misleading, because SC-5's
service ownership partly landed as step 5a"*) before this page was restructured.
The restructure reinstated it. The whole point of splitting these documents was
to stop superseded text reading as current, and the router reintroduced a
superseded measurement on the same day. **Superseded DECISIONS were moved to the
decision log; superseded MEASUREMENTS were not, and they are the ones that rot.**

The design numbers work as **steps**; the sub-contracts number themselves. The
mapping is `design.md:1168-1173` and `:1432-1445`: **step 3 = SC-2's actor
substance; step 5a = SC-5's behaviour-neutral half.**

| contract | state |
| --- | --- |
| **SC-2** | **~80% implemented** (`a21640bf4` + `32f9bb189`, `4ec1c701f`, and four fixes). Two connections, the interrupt generation guard, the terminal CAS, all four cancellation interleavings, and all eight classifier rows are in the tree with tests. **Missing:** the per-app-file actor, a production cancellation consumer (the surface is `#[allow(dead_code)]` awaiting SC-1's deadline rule), and the `SQLITE_BUSY_SNAPSHOT` arm |
| **SC-5** | **step 5a fully implemented** (`40c3df95f` + three fixes), with unusually strong instrumentation. **SC-5-as-contract is at zero**: Fork C (`AppIncarnationId` occurs 0 times), the ceiling as a service field, service-owned key custody |
| **SC-3** | shared core + read family built (`5c83046fc`), zero dependencies. Five families and the ledger absent |
| **SC-4** | **decision 4 IS implemented** (`8c6caa465`, dev-ness as a typed input) and SC-4 does not record it. Decision 1 unblocked and small. Decision 2 underspecified **by SC-4's own admission** |
| **SC-1** | **not structurally blocked.** Its substrate (interruptible actor, terminal CAS) now exists. It needs one yes/no answer |
| **SC-6** | write path **specified** (`docs/reviews/2026-08-28-flip-write-path.md`, 966 lines). Its author recommends **cancelling the flip** |

**The load-bearing consequence for SC-6:** most of that specification **is not
flip work** - it repairs the CURRENT layout. But the reason has to be stated
precisely, because an earlier revision of this page stated it wrong **twice**.

**WRONG (retracted 2026-08-28): "`RETURNING *` returns plaintext under `ssn`
today, pre-flip."** Measured: it does not, on any production JS-facing path.
`wrap_masked` defaults to `true` (`crud/read_pipeline.rs:27`) and
`wrap_row_on_read` **overwrites the parent with the sibling's masked value and
strips the sibling** (`crud/mask_pass.rs:469-482`). Only two sites set
`wrap_masked: false`: `dispatch_distinct` (`crud/mod.rs:1872`), where the SQL
already aliased the masked sibling so the row holds the mask anyway, and a test
(`read_pipeline.rs:581`). The write verbs funnel their `RETURNING *` rows
through the same `read_pipeline::apply` the reads use, so they inherit the
re-mask.

**RIGHT: the leak today is the CDC / subscription path, which is a THIRD exit
that never enters `read_pipeline` at all.** Verified:

- `exec.rs:531-541` builds the broker tuple from `m.iter()` over **every key** of
  the `RETURNING *` row, with no mask filtering.
- `wal_consumer.rs` contains **zero** occurrences of `mask`, `wrap_row_on_read`
  or `apply_mask` - the WAL path has no mask awareness whatsoever, and
  `tuple_to_map` zips every physical column out of pgoutput.
- `broker.rs:995` puts that tuple on a wire under `"row"` - **but that function
  is not reachable in production.**

**CORRECTED 2026-08-28, and this is the THIRD correction to the same question -
the pattern is worth more than the answer.** The claim first said `RETURNING *`
leaks plaintext to creators (wrong - the JS path re-masks). It was then
corrected to "the CDC path leaks plaintext to every subscriber" (**also wrong**).
Measured:

- `ws_frame_for_change` (`broker.rs:986`) is called by exactly one non-test
  function, `ws_frame` (`:1026`), and **`ws_frame` has no callers at all** - the
  other `ws_frame` matches in the tree are `read_ws_frame` / `write_ws_frame` in
  `zeroship-runtime/src/core/serve.rs`, unrelated functions.
- The live creator surface is `v8_classes/subscription.rs:158` ->
  `broker::message_to_json` (`:937-946`), which emits
  `"columns": ev.changed_columns` and **no row values at all**.

**So there is no value leak to subscribers today. What is live is a NAME
exposure:** the changed-column list reaches creators, so today it can name
`<col>_masked`, and post-flip it would name the raw column. That is a real
requirement for the new CDC service's projection, and a much smaller finding
than the two claims it replaces.

**Why this kept going wrong is the useful part:** each revision traced the data
one step further and stopped at the first function that *looked* like an exit,
without asking whether anything calls it. `grep` for a definition answers
spelling; only the caller graph answers reachability. Two of the three wrong
answers were produced by reviewers with file:line evidence for every claim -
correct citations, unasked question.

So for a mask-only field the **parent column holds plaintext and reaches every
subscriber, today.** Masking is independent of encryption: `read_pipeline::apply`
gates decrypt on `schema_has_encrypted_columns` and mask-wrap on
`schema_has_masked_columns` as **separate** conditions, and
`wrap_row_on_read`'s fallback re-masks the parent precisely because the parent
can hold plaintext.

**And the test that looks like it covers this is the reason nobody found it.**
`broker.rs:1863` is named `cdc_event_carries_masked_value_for_masked_columns` -
a name asserting the general property. Its fixture is
`parent_ciphertext_text = "\\x0123456789abcdef..."` (`:1868`), a BYTEA hex
literal, and it asserts the wire row equals that ciphertext (`:1908`). Its own
comment says what it is really for: *"A regression that wires decrypt-on-CDC
would land the plaintext in `new_tuple["ssn"]` and flip this assertion."*

**It guards a different regression, on the safe shape, under a name that claims
the dangerous one.** For a mask-only field the same assertion would fail today -
there is simply no fixture that constructs one. This is the
verification-record's own thesis appearing in the masking code: not a vacuous
test, but a real test whose NAME describes a property strictly wider than what
it rules on, so every later reader treats the question as settled.

**This changes the flip's ledger in both directions:** the flip would CLOSE this
leak for the parent (which becomes the mask) while OPENING a new one for
`<col>_raw` in the same tuple. And it is why narrowing `RETURNING *` to an
explicit column list does not help - the replication stream is just as wide, and
a `RETURNING` list cannot be applied to it. The whitelist belongs on the decoded
row and on the broker's `changed_columns`, not in the SQL.

**L28 is CLOSED by the platform-migrate removal** - the `platform-cli` feature it
named no longer exists, so the 26 harnesses it blocked are unblocked. An example
of why defect repair waits for the refactor rather than racing it.

### Still exactly zero

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
