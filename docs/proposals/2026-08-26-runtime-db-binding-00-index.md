# Runtime DB binding: the document set

**Read this page first. It routes.** It says what each document is for, what is
decided, what is open, and what is built. It deliberately does **not** argue any
of them - the arguments live in the documents below, and duplicating them here
is what makes this page go stale.

---

## The documents

| document | what it is |
| --- | --- |
| `...-design.md` | The architecture: forks, contracts, invariants, step sequence |
| `...-decision-log.md` | Every decision this set has reversed, with the superseded text in full. Append-only |
| `...-defect-register.md` | Live defects in EXISTING code this design touches. The most perishable file in the set |
| `...-defects-closed.md` | The closed defects, each with the commit that closed it and the mechanism by which it stayed invisible |
| `...-verification-record.md` | Five classes of test that report green while ruling on nothing, each with a measured instance. The most durable thing here |
| `...-sc1-transaction-protocol.md` | Transaction state machine, frames and effects, guard order, 15 property invariants |
| `...-sc2-sqlite-actor-protocol.md` | SQLite actor: reservations, the four cancellation interleavings, the terminal classifier |
| `...-sc3-dbplan-ir-and-ledger.md` | The `DbPlan` IR, its source ledger, the parity harness |
| `...-sc4-dev-and-hmr-mechanism.md` | Dev tier and hot reload. The thinnest document in the set; essentially unreviewed |
| `...-sc5-service-ownership.md` | `DbService`, process-wide cache, per-thread driver resources, the durable app incarnation |
| `...-sc6-ceiling-read-contract.md` | The ceiling meet, joined reads, and the masking storage flip |
| `2026-08-28-cdc-service.md` | The CDC relay: wire projection, one shared publication per Datastore, the worker/relay wire contract, leader election, failure behaviour |
| `2026-08-28-deploy-schema-precondition.md` | Refusing a deploy whose migrations have not applied - path 1 of decision 9 |
| `2026-08-28-migration-record-consolidation.md` | Where migration records live, and which of today's four are deleted |
| `2026-08-28-migrate-server-rename.md` | The migration service is one crate, `zeroship-migrate-server` |
| `2026-08-28-app-database-decoupling.md` | Datastore / Database / Grant: one database per app, many apps per database, the creator owning the schema |
| `docs/architecture/data-system.md` | The high-level view of the whole data system. Start here if the set is unfamiliar |

**Two of these files keep the set honest.** `decision-log.md` holds superseded
decisions in full, with what produced the error; `defects-closed.md` holds the
closed defects with their closing commits and evidence. Neither is an archive to
skim past. A closed defect re-raised in review, or a decision argued a second
time from its retracted premise, is answered by those two files and nowhere
else.

---

## Status

The design is settled. The CDC transport that several contracts depend on is no
longer an open block: it is a decided service with its own specification.

Implementation has landed on `feat/dbbind-impl`: defect repair on existing code,
the test harness that makes TDD on this design possible, the descriptor cutover,
and the platform migration move.

**Per sub-contract, as of 2026-08-29** - the table at the bottom of this page is
authoritative and carries the evidence:

| | state |
| --- | --- |
| SC-1 | built and wired |
| SC-6 | the masking storage flip has shipped |
| SC-2 | ~80%, with the per-app-file actor and a production cancellation consumer missing |
| SC-3 | shared core plus the read, search and write families; the remaining families and the ledger absent |
| SC-4 | two decisions implemented, one still underspecified |
| SC-5 | step 5a built; the contract itself at zero |

**The decoupling is designed and unbuilt.** `Database`/`Grant` occur zero times
in the tree. It is specified in `2026-08-28-app-database-decoupling.md`, and
`docs/architecture/data-system.md` is the high-level view of the whole data
system - read that first if you are new to this set.

---

## What is decided

Ten operator decisions. One line each; each is argued in the design beside the
claim it replaces, and its reversal history is in `decision-log.md`.

| # | decision |
| --- | --- |
| 1 | **No per-column encryption keys.** One key per app, `HKDF(platform_master_key, app_id, key_version)` |
| 2 | **PITR targets are control-plane state.** `__zeroship_admin` was four concerns under one name |
| 3 | **The mask policy is code-managed** and immutable for the isolate's life |
| 4 | **The operator ceiling is worker configuration**, changed by rolling the workers |
| 5 | **Privilege follows the PROCESS, not the function.** Now a repository key invariant (`AGENTS.md`) |
| 6 | Superseded by decision 7 within hours. Its text is in `decision-log.md` |
| 7 | **`__zeroship_admin` is deleted ENTIRELY, and there is no schema epoch.** The runtime descriptor is the authority |
| 8 | **The data plane performs NO live introspection at all.** Startup DDL validation is deferred, not designed |
| 9 | **Descriptor correctness is an enforced invariant, and the masking storage flip lands anyway as a second line of defence** |
| 10 | **Migration is NOT part of plugin-db. It belongs to `zeroship-migrate`.** Every DDL-emitting path leaves the data plane |

**A separate decision, not an operator ruling: a dedicated CDC service is built,
and every CDC defect is deferred into it rather than patched in place.** It
closes L30 (the worker's `REPLICATION` and `BYPASSRLS` grants) and L12b (an
abandoned per-worker slot) by construction rather than by repair, because
consumption moves to a process that runs no creator code and owns O(1) slots.
`2026-08-28-cdc-service.md` specifies it.

*`design.md` section 2 carries its own `D1..D10` table on a different axis: it
includes rulings that were never operator decisions (`storage.aadColumn`,
deterministic encryption, pooled checkout), and its numbering diverges from this
one after 6. The two lists are not the same list.*

### Decision 9: what "enforced" means

There are three ways the descriptor can describe a database it no longer
matches, and decisions 7 and 8 removed the runtime check that caught all three.

| # | drift path | who answers it |
| --- | --- | --- |
| 1 | a deploy goes live **before** its migration applies - the descriptor says `ssn` is masked while the database still holds plaintext there | `2026-08-28-deploy-schema-precondition.md`: a predicate inside the transaction that makes a deploy live |
| 2 | **mid-life drift** - a migration applies while a worker runs, with no deploy event to hook | the in-WAL epoch marker, `cdc-service.md` section 8 |
| 3 | **restore / PITR** - the same effect from a different direction | the same marker |

**The epoch marker is not only a subscriber-resync device; it is the
descriptor-drift detector for paths 2 and 3.** One mechanism, two problems, and
the connection is easy to lose because the two live in different documents.

**The flip is therefore the SECOND line, not the primary.** Its unique property
is that it stays safe when the enforcement above has a bug - and enforcement is
code.

### Decision 10: the cleanup, in dependency order

**Decision 10 is NOT yet satisfied.** The index-creation path is gone
(`ac38fac0e`), but **eight ungated DDL statements remain on the `unmask()`
path**, and they are the ones that actually run in a shipped binary.

`crud/unmask.rs` issues `CREATE TABLE IF NOT EXISTS
"<app>"."__zeroship_audit_unmask"` (`:850`) plus three `CREATE INDEX IF NOT
EXISTS` (`:870`, `:874`, `:878`) on **both** backends, from
`ensure_audit_unmask_table` (`:838`), reached from `write_audit_unmask_row`
(`:732`) on the granted path (`:428`), the denied path (`:405`), and two further
callers. The only `#[cfg]` in that file is `#[cfg(test)]` at `:1724`: none of
this is gated. It is DDL on the privileged read path, on every call.

Removing it means deciding **who creates `__zeroship_audit_unmask` instead** -
a migration-service change, and the reason it was left rather than rushed.

**The `.vector()` / `.spatial()` index creation this decision was scoped around
turned out never to have run in production at all** - both sites were already
`#[cfg(any(test, feature = "test-helpers"))]`, and `test-helpers` is not a
default feature. Deleting them was still right (a second author of an object the
engine already renders is drift waiting to happen), but the decision's stated
scope measured a file rather than the operation, and named the surface that
could not run while missing the one that does.

Of the six lazy-DDL sites invariant 7 tried to enumerate: **two deleted, two
`cfg`-gated (`mask_drift.rs:793`, `:827` - the whole module is gated at
`crud/mod.rs:91`), two live and ungated** (the unmask pair).

1. Move vector and spatial index creation into the migration path, so a
   `.vector()` or `.spatial()` field declares its index the way every other index
   is declared. **Landed:** `zeroship-migrate` now authors both, and
   `backend/postgres.rs` reads indexes it does not create (`:374-378`, `:487`).
2. Delete the index-creation helpers the move orphans, with their recovery and
   audit wrappers.
3. Delete `audit.rs` and the `AuditWriter` methods `ensure_audit_table`,
   `write_audit_row`, `next_schema_version`, plus the SQLite implementation
   (`zeroship-data-sqlite/src/lib.rs:1293-1295`). Steps 1 and 2 left both of those
   `#[cfg(any(test, feature = "test-helpers"))]` with no production writer, so
   the deletion is a consequence, not an edit.
4. `__zeroship_migrations` then disappears from creator schemas, which frees the
   name for the engine journal moving in
   (`2026-08-28-migration-record-consolidation.md`) with no rename on either
   side. `zeroship-data-sqlite/src/cdc.rs:465-472` filters the table out of the change
   stream and goes with it.

**The order is load-bearing.** Deleting the audit table before the DDL writer
keeps the writer and drops its provenance - the one thing `audit.rs:20-42`, the
passage `AGENTS.md` quotes when refusing a `SECURITY DEFINER` writer, exists to
guarantee.

Removing the category also closes design invariant 7's unfinished enumeration of
lazy-DDL sites: there is nothing left to enumerate.

### What decisions 7 and 8 cost

**These are accepted costs, not oversights. Do not read the decision list as
pure gain, and do not "fix" any of them by reintroducing live introspection.**

- **Mid-life drift is no longer detected**, and **roll the workers after a
  restore** is a procedure rather than a mechanism. Answered in design by the
  epoch marker; that marker is specified and not implemented.
- **The deploy pipeline's ordering guarantee is load-bearing.** If a deploy goes
  live before its migration applies, the descriptor says `ssn` is masked while
  the database still holds the real value there, and **the runtime serves
  plaintext believing it is masked. Nothing detects it.** Answered by the deploy
  precondition; specified and not implemented.
- **Subscribers lose their schema-change signal.** The CDC service reinstates it
  as the same in-WAL marker.
- **Fork C's identity state has no home.** `incarnation`, `deprovisioned_at` and
  the tombstone rule lived in `app_schema_state`. They are not schema metadata -
  DDL validation could not answer them even if it were not deferred, because two
  incarnations of one app id have the *same* schema. Specified and unhomed.

### The crate split: two crates, not six

The test applied was *what does a crate boundary buy that a module boundary does
not?* - a dependency the compiler refuses to invert, a separate compilation unit,
or an artifact another process consumes.

| candidate | verdict |
| --- | --- |
| plan / IR | **build.** Pure leaf: no I/O, no V8, testable without a database. Landed as `crates/zeroship-data-sql` |
| CDC | **build.** It becomes a separate *service*, which is a process boundary rather than a taste boundary |
| error, crypto | **modules.** Nothing outside plugin-db consumes them; crypto's pair (`mask_codec.rs`) is already in `zeroship-schema` |
| per-backend | **defer.** The capability traits in `backend/mod.rs` (`Backend` is a pure composition marker, `:38-41`) already are the seam. Having the seam is the win; splitting before a third backend exists buys nothing |

*The workspace already carries 35 crates, 11 of them `zeroship-migrate-*`. That
family was the precedent the six-crate plan copied, and at eleven it is the
counter-example rather than the model.* The module layout inside
`zeroship-plugin-db` is in `design.md`.

---

## What is open

### 1. SC-1: must terminal delivery survive process death?

**This one is the operator's.** SC-1's executable form requires a supervisor and
a durable `FenceJobRegistry` so terminal delivery survives process death; the
contract mentions neither and the codebase contains neither. Until it is
answered, SC-1's implementation step is either *write a reducer* or *write a
reducer plus a durable job system*. That is a multiple, not a detail. Argued in
`...-sc1-transaction-protocol.md`.

### 2. The masking storage flip's preconditions

The flip (`ssn` holds the mask, the real value moves to a sibling) is decided by
decision 9 and **must not be implemented until SC-6's blocking section is
discharged** - the write path is unguarded in both directions, and a partial
implementation retires a known leak while opening an unknown one. SC-6 owns that
list, and `docs/reviews/2026-08-28-flip-write-path.md` (966 lines) is the write
path in full.

Two constraints this page owns, because they are not in SC-6:

**The raw column's name should be one no validator admits, rather than a new
reservation.** `RESERVED_NAMES` already contains `ReservedName::Prefix("_")`
(`zeroship-schema/src/query.rs:742`), and `validate_field_name` already gates
every inbound surface - write-document keys including nested
(`crud/write_pipeline.rs:44`, `:63`, `:67`), filter keys (`query.rs:5329`),
conflict-probe keys (`query.rs:2909`) and read identifiers (`query.rs:937`). A
raw column named with a leading underscore is therefore already unrepresentable
on every path a creator can reach, and the entire inbound half of the flip
disappears: no new reservation, no filter-builder change, no schema hint threaded
through. Adding a `_raw` suffix reservation instead would have to land in **two**
independent tables (`crates/zeroship-schema/src/query.rs:754` and (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
`crates/zeroship-migrate-core/src/schema/query.rs:379`) with no dependency edge to keep them
agreeing. Choosing a name no existing gate admits is strictly better than adding
a fence, because a fence protects only the surfaces someone remembered to fence.

**The blast radius is 22 production sites across four crates**, from 41 total
hits (3 in `test-helpers`-gated modules, 9 inline `#[cfg(test)]` assertions, 1
integration target, 6 prose):

| crate | production sites |
| --- | --- |
| `zeroship-schema` | 9 - `query.rs:754`, `:2160`, `:3414`; `diff.rs:582`, `:671`, `:716`, `:1203`, `:1286`, `:1308` |
| `zeroship-migrate-core` | 9 - `crates/zeroship-migrate-core/src/schema/query.rs:379`; `crates/zeroship-migrate-core/src/schema/diff.rs:410`, `:747`, `:825`, `:847`; `render/fold.rs:3678`; `render/lower.rs:4803`, `:7185`; `render/declarative.rs:2223` |
| `zeroship-plugin-db` | 3 - `crud/mask_pass.rs:150`, `:469`; `crud/encryption_pass.rs:295` |
| `zeroship-migrate-backend` | 1 - `schema.rs:566` |

Ten of those are on the migration side, which
`migrate-core/src/render/gen_types/physical_storage.rs:5` does not cover: its
"eight independent sites" is scoped to the data plane. The migration side also
carries a **second, independent** `mask_sibling_column_for_field`
(`migrate-backend/src/schema.rs:557-566`) and the second reservation table above.
**There is no dependency edge between the two forks, so nothing can make them
agree by construction** - a reader who greps `zeroship-schema` alone will find
one of each pair.

For scoping the write half: `zeroship-schema/src/query.rs` has **12
SQL-emitting `RETURNING *` sites** (a bare `grep -c` reports 34, of which 14 are
in tests and 8 are `///` doc comments).

### 3. Two constraints the CDC service specification does not yet carry

Everything else found while reviewing that design is folded into it. These two
are not, and they should move there.

**The epoch marker is FORGEABLE, and marker integrity rests entirely on
no-raw-SQL.** `pg_logical_emit_message` has `proacl = NULL`, i.e. default PUBLIC
EXECUTE (verified on 18.4 and on 16.14). The WAL `M` frame carries prefix,
content, xid and lsn but **not the emitting role**
(`libs/compio-postgres/src/replication.rs:2044-2051`), and the marker names its
own app *in its content string*. With one shared cluster slot the relay cannot
distinguish a marker minted by the migration service from one minted by any other
session, nor verify the claimed app. The only thing preventing a tenant from
bumping another tenant's incarnation is that creator code has no raw-SQL surface.
That is probably sufficient today, but it is a **new trust edge** and the design
should say so rather than describing the marker as dissolving the problem.

*A version trap in the obvious mitigation:* `REVOKE EXECUTE ... FROM PUBLIC` must
name the right overloads, and the signature changed. PG 16 has
`pg_logical_emit_message(boolean,text,text)`; **PG 18.4 has four parameters** and
two overloads - `(boolean,text,text,boolean)` and `(boolean,text,bytea,boolean)`,
both with `proacl = NULL`. A revoke written against the 16 signature covers
nothing on 18.

**The relay's own leak surface is unaddressed.** The masking control is
"plaintext never enters the process", which is strong for masked columns - but
the relay holds **every tenant's unmasked rows in one process** and the design
says nothing about logs, `/metrics`, tracing, ring dumps or debug routes. Vitess
CVE-2026-65959 is one forgotten `acl.CheckAccessHTTP` on one debug handler
streaming live DML with bound values; every filed Debezium data-exposure bug is a
logging bug.

---

## Two consequences of the CDC decision that are easy to lose

**The decode multiplier is structural, and no publication design escapes it.** Slots on one database do not partition
decoding work, they replicate it: measured on pg16, five slots each decoding the
same 40,002 changes cost 1,335ms in total against 309ms for one slot (4.32 of a
possible 5.00). Verified in the PostgreSQL sources (REL_16 and REL_18):
`DecodeInsert` applies only three pre-queue filters (TOAST-internal,
other-database, replication origin) and then queues the change; the output-plugin
API exposes no filter-by-relation callback at all, and `pgoutput` checks table
membership and row filters at commit replay, **after** decode, buffering and
per-slot spill. So "just filter the publication" is not an alternative to one
decode per database - which is the floor PostgreSQL sells, and the floor the
service reaches. `cdc-service.md` cites this page rather than re-deriving it.

**Only two SC-3 families ever depended on the transport**, so the rest of SC-3
was never blocked by it:

| SC-3 surface | depends on the transport? |
| --- | --- |
| shared normative core, read family, write, search, unmask | **No** |
| relation family's **live-query** lowering | Yes - `read_set.rs` is relation-unaware |
| **effects** family (the publication a committed mutation owes the broker) | Yes - the transport choice changes the node shape |

The existence proof is on disk: `crates/zeroship-data-sql` implements the core
plus the read family with zero dependencies, references neither subscriptions nor
CDC nor WAL, and builds and tests without a database.

---

## What is built

`feat/dbbind-impl` carries defect repair on existing code plus the harness that
makes TDD on this design possible - which was not optional: five of this crate's
test targets were invisible to a default `cargo test`, the suite aborted at test
77 of 99, and ten tests skipped while counting as passes.

The four largest changes:

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

The branch's gate is twelve arms plus clippy (`b801bf12b`), and it is an
instrument that can fail. That qualifier is the point: every earlier "all arms
green" on this branch was false in a specific way, and each way hid a real
defect. The five classes are in `...-verification-record.md`.

**Two commits carried regressions that shipped** - `390f4b97b` (deleted a schema,
kept two callers) and `a5832708b` (bumped the descriptor, left v1 fixtures in
another crate). Both fixed, and both had the same cause: **verification scoped to
the crate being edited while the change crossed crates.** Scope the run to the
change, not to the crate.

### Per sub-contract

The design numbers work as **steps**; the sub-contracts number themselves. Step 3
is SC-2's actor substance and step 5a is SC-5's behaviour-neutral half; the
mapping is in `design.md`. Read "at zero" as naming mechanisms, not documents.
Every SHA below is an ancestor of `HEAD` by `git merge-base --is-ancestor`,
including the two the design records as owed for steps 3 and 5a.

| contract | state |
| --- | --- |
| **SC-2** | **~80% implemented** (`a21640bf4`, `32f9bb189`, `4ec1c701f`, plus four fixes). Two connections, the interrupt generation guard, the terminal CAS, all four cancellation interleavings and all eight classifier rows are in the tree with tests. **Missing:** the per-app-file actor, a production cancellation consumer (the surface is `#[allow(dead_code)]` awaiting SC-1's deadline rule), The `SQLITE_BUSY_SNAPSHOT` arm IS handled - `crates/zeroship-data-orm/src/backend/sqlite/error.rs:37` (the 517 constant), classified at `:97`, pinned at `:214` (checked 2026-08-29) |
| **SC-5** | **step 5a fully implemented** (`40c3df95f` plus three fixes), with unusually strong instrumentation. **SC-5 as a contract is at zero**: Fork C (`AppIncarnationId` occurs 0 times in the tree), the ceiling as a service field, service-owned key custody |
| **SC-3** | shared core plus the read family built (`5c83046fc`), zero dependencies, and the SEARCH **and WRITE** families ported onto the IR - `search.rs` 562 lines, `write.rs` 1057, so neither is a stub (counted 2026-08-29). `crates/zeroship-data-sql/src/` also carries `plan.rs`, `predicate.rs`, `projection.rs`, `path.rs`, `literal.rs`, `ident.rs` and a `render/` module. **Still absent:** the remaining families and the ledger. Unmask was deliberately left off the IR to avoid colliding with the SC-6 flip |
| **SC-4** | **decision 4 is implemented** (`8c6caa465`, dev-ness as a typed input) and SC-4 does not record it. Decision 1 unblocked and small; decision 2 underspecified by SC-4's own admission |
| **SC-1** | **the reducer is BUILT AND WIRED** (checked 2026-08-29). `crates/zeroship-data-orm/src/transaction/reducer/` carries `deadline.rs`, `frames.rs`, `identity.rs` and its own `tests.rs`, and it is reached from `crates/zeroship-data-orm/src/transaction/driver.rs` and `.../transaction/probe.rs`. |
| **SC-6** | **the flip is IN THE TREE** (checked 2026-08-29): `mask_sibling_column_for_field` no longer exists, and `__zs_raw__` / `raw_column_name` appear 44 times in `crates/zeroship-schema/src/query.rs` and 14 in `.../src/diff.rs`. The masked field's own column holds the mask and `__zs_raw__<field>` holds the plaintext. **Owed:** adding `.mask()` to a column that already holds data is now a real engine backfill for unencrypted columns; the ENCRYPTED case stays refused by decision, because `BackfillSpec` is structured SQL and the engine holds no key material | (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)

At zero and named as such in the design: the private module map, the
artifact/init channel, `DbIsolateBinding`, the deletion of `registerModel`, the
mask-policy artifact wire, the operator ceiling as worker configuration, Fork C's
identity substrate (`AppIncarnationId` still occurs 0 times, re-checked
2026-08-29 - though `AuthorityIdentity` in
`transaction/reducer/identity.rs` does carry an `incarnation` field, so the
substrate is not untouched ground), `DbPlan`'s remaining families and its
ledger, and the SC-4 dev mechanism.

**The masking storage flip is no longer on that list** - it shipped. Anything
reading this page for "what is left to build" should take the per-contract table
above as authoritative over any prose that predates it.

---

## How this set corrects itself

**Superseded text does not stay inline.** It moves to `decision-log.md`, whole.
This page and the design carry current text only. That rule exists because the v4
design ran two correction mechanisms at once and carried 43 retraction markers;
at that density a document is not read linearly, it is grepped, and interleaved
retractions make superseded text exactly as findable as current text. **The cost
of the split was accepted deliberately:** two documents can now disagree, where
one document could not drift against itself. `decision-log.md` being append-only
is what bounds that.

**Superseded MEASUREMENTS rot faster than superseded decisions**, and only the
decisions have a home to move to. A count is re-derived where it is used, or it
is dropped.

**A count carries its boundary or it is not a measurement.** "22 sites" means
nothing without "production, excluding `#[cfg(test)]`, test-helpers-gated modules
and prose".

**Citing a commit requires `git merge-base --is-ancestor <sha> HEAD`.** Reading
the commit is not enough - a dangling SHA reads perfectly, and a re-check run
against one once produced three fresh-looking citations that were real line
numbers of the wrong tree, so **a wrong measurement corrected a right document.**
Treat every SHA on this page as an answer to "which commit landed this", never as
evidence for "is this fix in the tree"; the second question is answered by
looking at the tree.
