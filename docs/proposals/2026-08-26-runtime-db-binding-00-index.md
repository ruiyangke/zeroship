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

**The projection mechanism is a PUBLICATION COLUMN LIST, and it is verified on
two PostgreSQL major versions.** The service design
(`docs/proposals/2026-08-28-cdc-service.md`) proposes computing the list from
`storage.valueColumn` over declared fields and applying it via
`zeroship-migrated`, which already owns publication membership. Its author
measured on 16.15; **I re-measured independently on 18.4**
(`~/.claude/jobs/.../verify_column_list.sh`), because a single-version
measurement cannot see a version-specific claim and this repository has a
recorded incident where two "versions" agreed perfectly because both were one
server:

| check | 18.4 result |
| --- | --- |
| excluded column's **NAME** in the pgoutput `Relation` message | **absent** |
| excluded column's **value** | **absent** |
| **control** - same table, publication with NO column list | both **present** |
| `ALTER TABLE ... ADD COLUMN` | publication unchanged; the new column does **not** enter |

**The control is the load-bearing arm.** Without it, "absent" is equally
consistent with a probe that cannot see the column at all - which is the exact
shape of the four gates found examining nothing. It reports present, so the
absences above are caused by the column list.

Two consequences worth stating plainly: **one mechanism closes both the value
and the name exposure**, because the name lives in the `Relation` message the
list also prunes; and **new columns default OUT**, which is whitelist semantics
by construction - a property the `format!("{col}_masked")` blacklist can never
have.

**The known cost, disclosed by the design's own author:** a column list that does
not cover the replica identity is accepted SILENTLY at DDL time, and then every
`UPDATE` and `DELETE` on that table fails with **SQLSTATE 42P10**. Reviewed
verdict: real, **not disqualifying, and narrower than "write outage"** - `INSERT`
still succeeds, so it is an UPDATE/DELETE outage and reads are unaffected. The
replica-identity UNION in the projection is the structural defence.

### Three review findings that change the design (2026-08-28)

**1. `DROP COLUMN` of a listed column FAILS, so reconcile-after-apply is the
wrong placement.** A column named in a publication column list becomes a catalog
dependency. Measured by me on 18.4, with a control:

| | result |
| --- | --- |
| drop a **listed** column | **REFUSED** - *"cannot drop column ... because other objects depend on it / DETAIL: publication of table t in publication p depends on column mask_col"* |
| **control** - drop an **unlisted** column, same table | drops cleanly |
| `DROP ... CASCADE` | succeeds, and **removes the whole TABLE from the publication** (0 rows) - CDC silently stops |

`reconcile_app_publication` runs **after** the migration DDL
(`zeroship-migrated/src/apply.rs:755`), and `dropColumn` is a supported creator
op the Postgres adapter lowers to a pure `ALTER TABLE DROP COLUMN`. So any
migration that drops a masked column - including the ordinary "remove `.mask()`
from a field" - aborts the creator's migration. **Today this cannot happen
because publications are whole-table.** Column lists introduce an ordering
constraint a single post-apply reconciliation cannot satisfy.

**The fix is bounded, not fatal:** split reconciliation into a
**shrink-before / widen-after** bracket around the DDL. The migration service
already holds the diff. It also fixes finding 2 for free.

**2. The epoch marker is emitted one transaction LATE.** The design's transcript
puts `pg_logical_emit_message` and the `ALTER TABLE` in one transaction, but the
actual emission site is inside `reconcile_in_transaction`, whose transaction
carries the `ALTER PUBLICATION` - **not** the creator's DDL, which committed
earlier. So "the marker strictly precedes new-schema data" holds only for data
committed after the *reconcile*, not for writes in the window between. Staleness,
not a leak - the projection is unaffected because new columns default out either
way.

**3. The marker is FORGEABLE, and marker integrity rests entirely on
no-raw-SQL.** `pg_logical_emit_message` has `proacl = NULL`, i.e. **default
PUBLIC EXECUTE** - verified by me on 18.4, by the reviewer on 16.14. The WAL `M`
frame carries prefix, content, xid and lsn but **not the emitting role**
(`compio-postgres/src/replication.rs:2043-2050`), and the marker names its own
app *in its content string*. With one shared cluster slot, the relay cannot
distinguish a marker minted by `zeroship-migrated` from one minted by any other
session, nor verify the claimed app.

The only thing preventing a tenant from bumping another tenant's incarnation is
that creator code has no raw-SQL surface. That is probably sufficient today, but
it is a **new trust edge** and the design should say so rather than describing
the marker as dissolving the problem.

**4. The crate-move table inverts the SQLite/Postgres polarity**, which would
leave an implementer holding dead code. Verified:

```rust
fn backend_publishes_committed_changes() -> bool {          // exec.rs:442
    context::with(|c| matches!(c.backend(), Some(BackendHandle::Sqlite(_))))
}
...
if backend_publishes_committed_changes() { return; }        // exec.rs:494
// "The old SDK-local emit was kept for the Postgres/no-WAL-consumer path;
//  on SQLite it races the CDC publisher"
```

The predicate is true **iff the backend is SQLite**, and `emit_for_rows` returns
early on it. So `emit_for_rows` / `queue_or_emit` /
`drain_pending_emits_on_commit` serve **Postgres**, and SQLite - which publishes
through `backend/sqlite/cdc.rs` - never reaches them. The design says they "stay
only for the SQLite dev tier", which is backwards: deleting the Postgres arm
means deleting these three **entirely**, and SQLite is untouched.

**5. The service the design assigns the projection to DOES NOT HAVE THE
DESCRIPTOR.** The design computes the wire projection from
`storage.valueColumn` and applies it via `zeroship-migrated`, "which already
owns publication membership". It owns membership at **table** granularity only.
Verified:

- `ApplyMigrationsRequest` (`zeroship-migrated/src/apply.rs:49-55`) carries
  `kind`, `documents: Vec<IrDocument>` and `policy` - **no descriptor field**.
- `publication_membership_sql` (`publication.rs:30-51`) takes
  `tables: &[String]` and emits `schema.table`. There is no column parameter
  anywhere on the path.

So the placement as written cannot be implemented: the service has no
`storage.valueColumn` to read.

**But the fix is smaller than "plumb the descriptor through", because the
service already receives `documents: Vec<IrDocument>` - and the IR is what the
descriptor is FOLDED FROM.** The projection should be derived from that IR
inside the service, in the same policy-resolved fold that produces the DDL, so
the DDL and the projection come from one source rather than two that can
disagree. That also supplies the diff finding 1's shrink-before/widen-after
bracket needs, so both fixes land in the same place.

**One reviewer-flagged risk RESOLVED, and it resolves structurally.** The wire
projection reads `storage.valueColumn` and nothing else, while the read side
(`read_column_for`, `query.rs:3402`) has a suffix *fallback* when the storage
block is absent - so a descriptor missing `valueColumn` would leave the read side
suffixing to `{field}_masked` while the wire side omitted the column entirely
(over-restrictive, not a leak). Measured: it cannot happen. The descriptor's
field is `pub value_column: String` (`migrate-core/src/render/gen_types.rs:178`),
**not `Option<String>`**, and both construction arms set it - `:297` to the
sibling for a masked field, `:310` to the field's own name otherwise. Absence is
unrepresentable, so no test is owed.

**A version trap in the recommended mitigation, found by measuring two servers.**
`REVOKE EXECUTE ... FROM PUBLIC` must name the right overloads, and **the
signature changed**: PG 16 has `pg_logical_emit_message(boolean,text,text)`;
**PG 18.4 has four parameters** and two overloads -
`(boolean,text,text,boolean)` and `(boolean,text,bytea,boolean)`, both with
`proacl = NULL`. A revoke written against the 16 signature covers **nothing** on
18. This is exactly what a single-version measurement cannot see.

**The transport's "biggest unproven element" is less unproven than the review
concluded.** Both reviewers flagged the `ntex` (server push) / `cyper` (client
consume) composition as the design's largest implementation risk, because its
author confirmed the dependencies but "wrote no code against them". Measured
2026-08-28 - **both halves are already in production in this tree**:

| half | where |
| --- | --- |
| **server push**: ntex `streaming()` over an mpsc channel | `zeroship-worker/src/handler.rs:1242+`, and `zeroship-gateway/src/proxy.rs:419`, `:876` |
| **client consume**: cyper `bytes_stream` | `zeroship-plugin-storage/src/backend/s3.rs:35` |

The worker's use is the closer analogue than the gateway's, and it is the right
shape: an **indefinite** push driven by a waker rather than a fixed-length body -
*"The V8 pump task runs independently ... StreamWriter.push() wakes our drain
task via the registered waker - no busy polling"* (`handler.rs:1243-1247`).

**The honest limit:** neither half is demonstrated inside the *same* long-lived
channel, so the composition still owes a spike - but it is "wire two shipped
primitives together", not "find out whether the libraries can do this". One
caveat to carry into that spike: `zeroship-bundle/src/s3_blob.rs:18` records that
"cyper's `Send`-bound streaming body never comes", which concerns streaming
**request** bodies. The relay needs a streaming **response**, which is the
`bytes_stream` path above - but anyone reaching for a streaming upload should
expect to hit that wall.

**6. The keepalive handler is CORRECT today and becomes a DURABILITY BUG in the
relay** - found 2026-08-28 while checking for the opposite problem.

The famous Postgres CDC footgun is that an idle slot never advances
`confirmed_flush_lsn`, so WAL accumulates with zero changes (this is why
Debezium ships a heartbeat). **This tree already handles it**, correctly:

```rust
ReplicationMessage::PrimaryKeepalive { wal_end, reply_requested, .. } => {
    stream.advance_lsn(wal_end);                      // wal_consumer.rs:441
    if reply_requested { stream.send_standby_status_update(false).await?; }
}
```

Advancing to the server's current `wal_end` on a keepalive is exactly what
prevents idle-slot WAL growth.

**But the relay makes a promise this line breaks.** Section 6.3 of the service
design states the relay confirms an LSN *"once the transaction's frames are
durable in the relay's own ring, not once a worker acknowledges them - that is
what converts WAL retention into relay-owned retention."* A keepalive's
`wal_end` is **the server's current WAL position**, which can be ahead of
anything the relay has persisted. Blindly advancing to it confirms WAL the relay
does not hold; PostgreSQL is then free to recycle it, and after a relay crash
those changes are **gone from the WAL and never delivered.**

Today this is harmless because the in-process consumer promises no durability -
it feeds a broker and a lost event is a missed live-query tick. The relay
promises retention, and `wal_consumer.rs` is on the **move** list (section 4).
**So the rule for the relay is: advance to `min(wal_end, highest_durable_lsn)`,
never to `wal_end` alone** - and on an idle database those are equal, so the
idle-slot protection is preserved.

*Verified: the code path and the design's promise. Inferred: that a keepalive's
`wal_end` can exceed the relay's durable position between commits - which
follows from `wal_end` being the server's position, but I did not construct the
race.*

**7. ONE SLOT AND PER-APP PUBLICATIONS DO NOT COMPOSE. The publications must
collapse to one - and the current reconciler would then clobber tenants.**
Found 2026-08-28 by auditing the crate-move list for contract changes.

`replication.rs` - one of the two files that MOVES to the relay - names objects
per app and per worker (`:16-17`): publication `__zs_pub_<app-token>`, slot
`__zs_slot_<app-token>__<worker-token>`, with `worker_slot_name(app_id,
worker_id)` at `:114`. The relay has **one slot for the whole cluster**. So the
slot naming is simply gone, but the publication question is real:
`publication_names` is fixed at `START_REPLICATION`, so a relay subscribing to
N per-app publications would have to **restart the stream for every tenant
whenever any app is created** - a cross-tenant disruption on a routine event.

**Measured on 18.4 that one publication holding many tables is the answer**
(`verify_pub_live.sh`), with a control:

| check | result |
| --- | --- |
| table ADDED to the publication mid-stream appears on the **live** slot | **yes - no restart** |
| the late-added table's **column list is applied** (excluded value) | **absent** |
| the pre-existing table keeps flowing | yes - undisturbed |
| **control** - a table in no publication | absent |

So: **one publication, many tables, each carrying its own column list.**
Onboarding an app becomes `ALTER PUBLICATION p ADD TABLE t (cols)` with no
restart and no effect on any other tenant.

**And that turns the publication into a SHARED object, which the current
reconciler is not written for.** `publication_membership_sql`
(`zeroship-migrated/src/publication.rs:30-51`) emits
`ALTER PUBLICATION <p> SET TABLE <all members>` - a **full replace** computed
from one app's table list. On a shared publication that is a lost-update race:
two apps migrating concurrently each SET the membership to their own view, and
whichever commits second **removes the other tenant's tables from CDC
entirely**, silently.

The fix is `ADD TABLE` / `DROP TABLE` per table rather than `SET TABLE`, or an
advisory lock around reconciliation. It is small, but it is invisible until two
tenants migrate at the same moment - and it did not exist before, because a
per-app publication has exactly one writer.

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

  **SUPERSEDED 2026-08-28 by a better answer: do not reserve `_raw`, name the
  raw column something every validator ALREADY refuses.** An earlier revision of
  this page said the fix was one line - `ReservedName::Suffix("_raw")` beside the
  `_masked` entry - which would have had to land in **two** independent
  reservation tables (`zeroship-schema/src/query.rs:754` and
  `migrate-core/src/schema/query.rs:379`) with no dependency edge to keep them
  agreeing.

  `RESERVED_NAMES` already contains `ReservedName::Prefix("_")`
  (`query.rs:742`), and `validate_field_name` already gates **every inbound
  surface** - verified:

  | surface | site |
  | --- | --- |
  | write-document keys, including nested | `crud/write_pipeline.rs:44`, `:63`, `:67` |
  | filter keys | `query.rs:5329` |
  | conflict-probe keys | `query.rs:2909` |
  | read identifiers | `query.rs:937` |

  **So a raw column named with a leading underscore is already unrepresentable on
  every path a creator can reach, and the entire inbound half of the flip
  disappears** - no new reservation, no filter-builder change, no schema hint
  threaded through. This is the difference between adding a fence and choosing a
  name no existing gate admits, and it is strictly better because a fence
  protects only the surfaces someone remembered to fence.

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
| 9 | **The descriptor MUST NOT get wrong, and the masking flip lands anyway as a second line of defence** |

### Decision 9, 2026-08-28: enforce the descriptor, and flip anyway

Two operator statements settled SC-6.

**"Silent break is nothing, we have no users and no apps yet."** That discounts
two of the flip's three costs outright - the 22-site churn and the
type-and-constraint swap are exactly what pre-launch is for. **Only the third
survives:** the migration engine cannot see this change (it compares names, and
the names do not move), so the flip's migration is hand-authored with nothing
verifying it. That is a one-time correctness problem rather than an ongoing one,
and it is the piece that touches real column data - so it owes a mutation-proved
test before it runs anywhere.

**"The descriptor must not get wrong."** This is the primary work, and it is
larger than the deploy-ordering guard an earlier revision of this page proposed,
because there are **three** ways the descriptor goes wrong and decisions 7 and 8
removed the runtime check that caught all of them:

| # | drift path | enforced today |
| --- | --- | --- |
| 1 | a deploy goes live **before** its migration applies - the descriptor says `ssn` is masked while the database still holds plaintext there | **No.** The design calls this "a description, not a mechanism" |
| 2 | **mid-life drift** - a migration applies while a worker runs, with no deploy, so that worker serves against a database its descriptor no longer describes until it restarts | **No** |
| 3 | **restore / PITR** - the same effect arriving from a different direction | **No.** "Roll the workers" is a procedure, not a mechanism |

Path 1 is a deploy-pipeline precondition: refuse to make a deploy live until its
migrations have applied. Paths 2 and 3 are harder, because there is no deploy
event to hook - the worker must learn the schema moved underneath it, which is
exactly the schema-change signal decisions 7 and 8 deleted.

**That is the same signal the new CDC service reinstates as an in-WAL marker**,
and the connection should not be lost: the epoch marker is not only a
subscriber-resync device, it is the descriptor-drift detector for paths 2 and 3.
One mechanism, two problems.

**The flip is therefore the SECOND line, not the primary.** Its unique property
is that it stays safe when the enforcement above has a bug - and enforcement is
code.

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
