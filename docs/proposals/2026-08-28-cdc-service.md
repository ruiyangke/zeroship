# The CDC service

This document specifies the CDC relay service. The decision to build it is
recorded in `docs/proposals/2026-08-26-runtime-db-binding-00-index.md:51-56`;
this page does not re-argue it.

**Working name:** `zeroship-cdc`. One binary, one process, executes no creator
code.

---

## 0. How to read the citations in this document

Every `file:line` below was opened in this worktree
(`/home/ruiyang/Projects/appbase/.worktrees/dbbind-impl`), on the working tree as
it stands. The migration service crate is `crates/zeroship-migrate-server/`
(renamed from `zeroship-migrated` in `8f69c7e53`).

PostgreSQL behaviour claims marked **MEASURED** come from throwaway containers
started and destroyed inside the sessions that wrote this page: `postgres:16`
(16.15) and `postgres:18` (18.4, Debian build), both with
`-c wal_level=logical`. Each transcript names its version. Anything not run or
opened is marked **unverified** and says so in its own sentence rather than being
written as fact.

Two things this document deliberately does not carry: any latency figure (section
11 specifies how to obtain one) and any re-derivation of the decode multiplier,
which the index records as structural and already verified in the PostgreSQL
sources.

---

## 1. Decision summary

| question | answer | section |
| --- | --- | --- |
| Where is the wire projection applied? | In the **publication column list**, as DDL, by `zeroship-migrate-server`. PostgreSQL never puts the excluded bytes or the excluded names on the wire | 3 |
| What covers `changed_columns`? | The same mechanism. A column absent from the publication list is absent from the pgoutput `Relation` message, which is the only thing `changed_columns` is built from | 3.2 |
| How many publications? | **One, cluster-wide.** `publication_names` is fixed at `START_REPLICATION`, so a per-app publication would restart every tenant's stream on every app creation. Membership is edited with `DROP TABLE` + `ADD TABLE (cols)`, never `SET TABLE` | 3.6 |
| When is membership reconciled? | **Bracketing** the migration DDL - shrink before, widen after - because a published column is a catalog dependency and `DROP COLUMN` of one fails `2BP01` | 3.6 |
| Where does the column set come from? | The same policy-resolved fold that produces the DDL. The migration service holds the IR, not the descriptor, and must not re-derive `valueColumn` by string formatting | 3.7 |
| Which crates move? | `wal_consumer.rs` and `replication.rs` move; `slot_reaper.rs`, `replication_ops.rs`, `change_stream_pg.rs` and the **whole** local-emit path are **deleted**; `broker.rs`, `read_set.rs`, `cdc_lifecycle.rs` stay | 4 |
| Is `zeroship-stream` the carrier? | **No**, and the reasons are in its own source, not in taste | 5.1 |
| What is the carrier? | A relay-owned bounded per-app ring plus a long-lived push response over `ntex` (server) and `cyper` (client), authenticated by a service assertion | 5.2 |
| What happens to a slow worker? | It is shed, never awaited. The ring evicts oldest-first and raises `Resync`; a wedged connection is closed. No credit scheme, because credit would let a consumer stall slot confirmation | 5.4 |
| How big is the ring? | Byte depth, frame cap and time depth, per app, plus a cluster cap that evicts from the largest ring | 5.5 |
| What if a change cannot be represented? | A `Gap` frame for that row, not a `Resync` for the app. Plus a `Truncate` frame and a three-way value encoding that distinguishes NULL from unavailable | 5.6 |
| Leader election | One `pg_try_advisory_lock` per cluster, session-scoped, over `compio-postgres`. One database is shared by every app, so this is one leader, not one per app | 6 |
| At-least-once dedup key | `(commit_lsn, change_index)`, lexicographic. The per-change LSN is unusable: measured to go backwards between transactions and to collapse under `heap_multi_insert` | 6.3 |
| What invalidates a watermark? | A `systemid` or `timeline` change from `IDENTIFY_SYSTEM`. **Not** a leader-term bump, which is a routine failover producing only duplicates | 6.4 |
| Failure behaviour | `max_slot_wal_keep_size` bounds WAL; slot invalidation becomes `Resync`; the subscriber contract absorbs `Resync` on the live-query path and forces the creator to handle it on the raw path | 7 |
| Blast radius of one tenant | Bounded by construction: a total per-app step, no `catch_unwind`, and a degraded app is quarantined without stopping the slot | 7.5 |
| What does the relay report? | Relay-side, per app. `confirmed_flush_lsn` and `safe_wal_size` are structurally blind to a delivery outage under this design | 7.6 |
| Schema-change signal | `pg_logical_emit_message` inside the **widen** transaction. This **dissolves** the carrier problem rather than moving it, because the marker is ordered by the WAL itself | 8 |

---

## 2. What the service closes, restated against the code

Three findings close because consumption moves out of the worker process.

### 2.1 L30: the worker holds REPLICATION and BYPASSRLS

`db/migrations-ts/20260818000200_worker_database_authority.ts:35`:

```
ALTER ROLE zeroship_worker WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE INHERIT REPLICATION BYPASSRLS
```

Four lines below, `:39`, the same file does the opposite for a role that is not
a connection identity:

```
ALTER ROLE zeroship_workflow_owner WITH NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS
```

`zeroship_worker` is the login role of the process that runs creator code
(`crates/zeroship-worker/src/config.rs:59-60`, one `worker.database_url` for the
whole process, not one per app). The service takes both attributes: the relay's
own login role holds `REPLICATION`, and `zeroship_worker` drops `REPLICATION`
and `BYPASSRLS` in the same migration that provisions the relay's role.

`BYPASSRLS` is a separate grant from `REPLICATION` and is not required to consume
a slot. It is in the same statement and drops with it. What else in the tree
depends on `zeroship_worker` holding `BYPASSRLS` is **not verified**; enumerating
those consumers is a required pre-flight check before writing the migration.

### 2.2 L12b: an abandoned slot

Slot names are per `(app, worker)`:
`crates/zeroship-plugin-db/src/replication.rs:114-121` composes
`__zs_slot_<sha14(app)>__<sha10(worker)>`.
`crates/zeroship-plugin-db/src/slot_reaper.rs` exists (593 lines) to sweep the
ones nobody owns any more, on a one-hour inactivity threshold (`:28`) with a
two-lock worker lease (`:116-125`, `:182-198`) and a fleet-leader election
(`:171-180`). With O(1) service-owned slots there is no per-worker slot to
abandon. The file is deleted, not ported.

### 2.3 The mask-only leak, stated precisely

The precise shape, which matters for what the fix has to cover, is that **the
name leaks to creators today and the value does not, but the value is in the
process and one call site from the wire.**

- `crates/zeroship-plugin-db/src/wal_consumer.rs` contains **zero** occurrences
  of `mask`, `wrap_row_on_read` or `apply_mask`
  (`grep -c -i "mask\|wrap_row_on_read\|apply_mask"` returns `0`). `tuple_to_map`
  (`:662-681`) zips every physical column the server sent into `new_tuple`.
- `changed_columns` on the WAL path is every column of the cached relation
  (`wal_consumer.rs:629-633`, `rel.columns.iter().map(|c| c.name.clone())`).
- `changed_columns` on the local-emit path is every key of the `RETURNING *` row
  minus exactly two literals (`crates/zeroship-plugin-db/src/exec.rs:518-524`,
  `.filter(|k| !matches!(k.as_str(), "created_at" | "updated_at"))`). That is a
  blacklist of two names.
- The tuple on the local-emit path is every key of the same row
  (`exec.rs:531-541`).
- **`emit_for_rows` runs before the mask pass.** `exec_mutation_with_emit` calls
  `emit_for_rows(&rows, ...)` at `exec.rs:438`; the caller passes the same rows
  to `read_pipeline::apply` afterwards at
  `crates/zeroship-plugin-db/src/crud/mod.rs:799`. So the broker event is built
  from the raw row, not the masked one. This ordering is the mechanism, and it is
  a stronger statement than "`exec.rs:531-541` maps every key".
- `changed_columns` reaches JavaScript. `message_to_json`
  (`crates/zeroship-plugin-db/src/broker.rs:937-946`) emits
  `{"kind","op","collection","pk","columns"}`, and
  `crates/zeroship-plugin-db/src/v8_classes/subscription.rs:158` is the sole
  caller: `Ok(JsonValue(broker::message_to_json(&msg)))`. The SDK's event type
  matches (`sdks/db/src/subscribe.ts:38-54`) and has no `row` field.
- `new_tuple` has exactly **two** production consumers, measured by grepping
  `crates/` for `new_tuple` and discarding constructors and tests:
  `broker.rs:290` (`entry.matches(&event.new_tuple)`, predicate evaluation) and
  `broker.rs:995` (`"row": ev.new_tuple` inside `ws_frame_for_change`).
- **`ws_frame_for_change` has no production caller.** Grepping
  `ws_frame_for_change`, `ws_frame_for_control` and `ws_frame(` across the repo
  excluding `target/`, `node_modules/` and `.git/`: the only hits in `crates/`
  and `sdks/` are the definitions at `broker.rs:986`, `:1004`, `:1024`, the
  internal dispatch at `:1026-1027`, and tests at `:1479`, `:1491-1492`, `:1524`,
  `:1531`, `:1906`. `docs/reviews/2026-08-28-flip-write-path.md:355` reached the
  same conclusion independently and specifies deleting it.

So the live creator-visible leak today is **the column name**, and the value leak
is a loaded gun rather than a discharged one. Both must be closed, and the one
mechanism in section 3 closes both.

---

## 3. The wire projection

### 3.1 The mechanism

**The projection is a PostgreSQL publication column list, computed from the
declared field set by `zeroship-migrate-server`, applied as DDL, and enforced by
the server.**

`zeroship-migrate-server` already owns publication membership and already
reconciles it to an explicit table list after every apply:
`crates/zeroship-migrate-server/src/publication.rs:30-52` builds
`CREATE PUBLICATION ... FOR TABLE s.t, s.u` / `ALTER PUBLICATION ... SET TABLE ...`,
and a test at `:125-154` asserts the DDL names each table explicitly and does
**not** use `FOR TABLES IN SCHEMA` (the assertion is `:138`). The change is to
extend `publication_membership_sql` from `tables: &[String]` (`:33`) to a wire
set of `(table, columns)` pairs - and, per 3.6, to stop emitting `SET TABLE` at
all.

The column list is a **whitelist over declared fields**, computed positively:

```
wire_columns(collection) =
    replica_identity_columns(collection)
  UNION
    { storage.valueColumn(field) : field in declared_fields(collection) }
```

`storage.valueColumn` is the descriptor's own field-to-physical-column mapping,
the same block `read_column_for` reads on the query side
(`crates/zeroship-schema/src/query.rs:3402-3417`, which reads
`storage.valueColumn` and only falls back to suffixing when the block is absent).
For a mask-only field today `valueColumn` is the `_masked` sibling; the plaintext
parent is not in the set. Post-flip the parent holds the mask and
`storage.rawColumn` holds plaintext, and `rawColumn` is still not in the set,
because the set is built from `valueColumn` and nothing else.

The set is computed by the migration service, which has just folded the migration
and therefore holds the declared field set with its mask and encryption flags. It
is not derived from a string suffix at any point. **Where exactly that fold
happens, and why the service cannot read a descriptor it never receives, is 3.7.**

### 3.2 MEASURED: the column list removes the name as well as the value

This is the load-bearing claim, and it is the reason one mechanism satisfies both
halves of the inherited requirement. Transcript, PostgreSQL 16.15,
`proto_version=1` (the version `wal_consumer.rs:383` requests today):

```sql
CREATE TABLE t (id int primary key, ssn text, ssn_masked text);
CREATE PUBLICATION p1 FOR TABLE t (id, ssn_masked);
SELECT pg_create_logical_replication_slot('s1','pgoutput');
INSERT INTO t VALUES (1,'123-45-6789','***-**-6789');
SELECT encode(data,'escape')
  FROM pg_logical_slot_peek_binary_changes('s1',NULL,NULL,
       'proto_version','1','publication_names','p1');
```

```
B\000\000\000\000^AQ\261\250\000^B\375^Y\211\217\205$\000\000^B\335
R\000\000@\000public\000t\000d\000^B^Aid\000\000\000\000^W\377\377\377\377\000ssn_masked\000\000\000\000^Y\377\377\377\377
I\000\000@\000N\000^Bt\000\000\000^A1t\000\000\000^K***-**-6789
C\000\000\000\000\000^AQ\261\250\000\000\000\000^AQ\261\330\000^B\375^Y\211\217\205$
```

Read the `R` (Relation) frame: column count `\002`, then `id` and `ssn_masked`.
**`ssn` is not in the Relation message.** Read the `I` (Insert) frame: two tuple
values, `1` and `***-**-6789`. The string `123-45-6789` does not occur anywhere
in the stream.

`changed_columns` is built from the relation cache, which is built from the
`Relation` message (`wal_consumer.rs:511-526` populates it, `:629-633` maps it).
A name the server never sends cannot be put in `changed_columns` by any code
path, present or future.

### 3.3 MEASURED: a new column defaults OUT

```sql
ALTER TABLE t ADD COLUMN newcol text;
INSERT INTO t VALUES (2,'999-99-9999','***-**-9999','brand new');
```

The next `R` frame is byte-identical to the one above (`id`, `ssn_masked`), and
the `I` frame carries two values, `2` and `***-**-9999`. Neither the new column
name, nor its value, nor the new plaintext appears.

This is the property that the current `_masked` stripper does not have and cannot
have. `format!("{col}_masked")` is a blacklist keyed on a derived name: it knows
exactly one sibling name (`crud/mask_pass.rs:469`, and its write-side twin at
`:150`), so anything that is not spelled that way survives. A publication column
list is a whitelist over the columns the migration service decided to publish,
and everything not on it is absent by default, including columns that do not
exist yet.

### 3.4 Why a future code path cannot bypass it

Because the bytes are not in the process. The relay cannot leak a value it never
received, and no serialiser downstream of the relay can print a name that never
reached it. This is a different class of guarantee from "one filter at one call
site", which is what the flip-write-path review could achieve for
`changed_columns` and honestly labelled *guarded* rather than unrepresentable
(`docs/reviews/2026-08-28-flip-write-path.md:675`).

**A second, independent defence, covering a different vector.** The database
guarantee covers the PostgreSQL arm only. The SQLite dev tier has its own CDC
producer with the same shape and the same gap:
`crates/zeroship-plugin-db/src/backend/sqlite/cdc.rs:604-639` builds `new_tuple`
from PRAGMA-resolved column names and sets
`changed_columns: new_tuple.keys().cloned().collect()` (`:622-623`). There is no
publication there to enforce anything.

So `ChangeEvent`'s tuple field changes type. `new_tuple: HashMap<String, String>`
(`broker.rs:110`) becomes `new_tuple: ProjectedTuple`, a newtype whose only
constructors are:

- `ProjectedTuple::from_published_relation(&RelationEntry, &TupleData)` - the
  relay arm, whose guarantee is upstream in the publication;
- `ProjectedTuple::from_descriptor(&Value, raw)` - the SQLite arm, which filters
  against `storage.valueColumn` for each declared field.

The newtype does not create the guarantee. Its job is to make every producer
**name** which guarantee it is relying on, so that a third producer cannot be
written that relies on none. That is the same move
`docs/reviews/2026-08-28-flip-write-path.md:368` makes with `RawRows` on the read
path, and on the relay arm the constructor is close to a no-op.

### 3.5 What the column list costs, and it is not free

Three measured consequences. All three are real hazards and the design carries
them explicitly.

**(a) A column-list mistake is a WRITE OUTAGE, not a read failure.** MEASURED:

```sql
CREATE TABLE u (id int primary key, a text, b text);
CREATE PUBLICATION p2 FOR TABLE u (a, b);   -- accepted, no warning
INSERT INTO u VALUES (1,'x','y');            -- INSERT 0 1
UPDATE u SET a='z' WHERE id=1;
-- ERROR:  cannot update table "u"
-- DETAIL:  Column list used by the publication does not cover the replica identity.
DELETE FROM u WHERE id=1;
-- ERROR:  cannot delete from table "u"
-- DETAIL:  Column list used by the publication does not cover the replica identity.
```

Dropping `p2` restored both (`DROP PUBLICATION p2; UPDATE 1; DELETE 1`). The DDL
is accepted silently and the failure surfaces at the creator's next write. This
is why `replica_identity_columns(collection)` is a UNION term in the formula at
3.1 rather than an assumption, and why section 10 requires a test that omits it
and asserts the write fails.

**The SQLSTATE is `42P10`.** MEASURED on PostgreSQL 18.4 with
`\set VERBOSITY verbose`, against a table published as `(ssn_masked)` only:

```
ERROR:  42P10: cannot update table "notes"
DETAIL:  Column list used by the publication does not cover the replica identity.
LOCATION:  CheckCmdReplicaIdentity, execReplication.c:813
ERROR:  42P10: cannot delete from table "notes"
DETAIL:  Column list used by the publication does not cover the replica identity.
LOCATION:  CheckCmdReplicaIdentity, execReplication.c:831
```

`42P10` is `invalid_column_reference`, which is not specific to this condition,
so it is a good assertion target for a test and a bad one for a runtime
classifier. The projection builder refuses the shape before the DDL is emitted
(10.3); the SQLSTATE is what pins the *reason* to real server behaviour.

**(b) `REPLICA IDENTITY FULL` is incompatible with any partial column list.**
MEASURED: `ALTER TABLE t REPLICA IDENTITY FULL` on the table published as
`(id, ssn_masked)` makes every `UPDATE` and `DELETE` fail with the same error,
because FULL makes the replica identity the whole row and the column list cannot
cover it. Reverting to `REPLICA IDENTITY DEFAULT` restored both.

This matters because the tree already recommends the other branch.
`wal_consumer.rs:551-560` says, of a DELETE under the default replica identity,
that "a subscription filtered on a non-key column can miss a delete. Fixing that
needs REPLICA IDENTITY FULL on published tables". **The two improvements are
mutually exclusive.** This design chooses the column list and accepts that a
DELETE carries only the replica-identity columns. Section 5.3 says how "row left
the view" is answered without a full before-image.

**(c) Two publications with different column lists on one table break decode.**
MEASURED, and the failure arrives at decode time rather than DDL time:

```
ERROR:  cannot use different column lists for table "public.t" in different publications
CONTEXT:  slot "s3", output plugin "pgoutput", in the change callback, associated LSN 0/156AB40
```

App tables live in the app's own schema
(`zeroship-schema/src/query.rs:1026-1028`, `CREATE SCHEMA IF NOT EXISTS <app>`),
and under 3.6 there is exactly ONE publication in the cluster, so each table
belongs to exactly one publication and this cannot arise from the intended shape.
It can arise from an operator or a test creating a second publication over a
creator table, so the relay must treat this SQLSTATE as fatal and name the table
rather than reconnecting into it.

### 3.6 One publication, and reconciliation BRACKETS the DDL

Two things force this subsection, and they interact. Take them in order.

**(i) There is exactly one publication, cluster-wide.** `publication_names` is
fixed when `START_REPLICATION` is issued (`wal_consumer.rs:384`, inside the
options struct built at `:380-386`; the driver's field is
`StartReplicationOptions::publication_names`
(`libs/compio-postgres/src/replication.rs:852`), interpolated into the command
string once at `:741` and never revisited). It cannot be changed on a running
stream. A publication per app therefore means restarting the ONE cluster stream
every time any tenant is created, which is a cross-tenant availability coupling
introduced for a naming convenience.

MEASURED, PostgreSQL 18.4. One publication `zs_cdc` over two tenant schemas -
`app_alpha.notes (id, ssn_masked)` and `app_beta.items (id, label)`, the state
left by the bracket transcript in (iii) below - with a slot already streaming:

```sql
SELECT pg_create_logical_replication_slot('sc','pgoutput');
INSERT INTO app_alpha.notes VALUES (1,'x','***1');
CREATE TABLE app_gamma_items (id int primary key, label text, secret text);
BEGIN;
  ALTER PUBLICATION zs_cdc ADD TABLE app_gamma_items (id, label);
  SELECT pg_logical_emit_message(true, 'zs.epoch', 'app_gamma:1');
COMMIT;
INSERT INTO app_gamma_items VALUES (9,'live','top-secret');
INSERT INTO app_alpha.notes VALUES (2,'y','***2');
```

peeking that slot with `'messages','true'`:

```
 0/1805D18 | 783 | B
 0/1805D18 | 783 | R ... app_alpha notes ... id, ssn_masked
 0/1805D18 | 783 | I ... 1, ***1
 0/1805E30 | 783 | C
 0/180CB98 | 785 | B
 0/180CB98 | 785 | M zs.epoch ... app_gamma:1
 0/180CC10 | 785 | C
 0/180CC10 | 786 | B
 0/180CC10 | 786 | R ... public app_gamma_items ... id, label
 0/180CC10 | 786 | I ... 9, live
 0/180CD30 | 786 | C
 0/180CD30 | 787 | B
 0/180CD30 | 787 | I ... 2, ***2
 0/180CDE8 | 787 | C
```

Three things in that transcript. The mid-stream `ADD TABLE` is picked up **live**
with its column list applied - `secret` is absent from both the `Relation` and
the `Insert`. The pre-existing alpha stream is undisturbed and emits no new
`Relation` frame. And the epoch marker (section 8) rides the same transaction and
is ordered before the first change of the new table. The control is alpha itself:
a table that was already published keeps streaming through the membership change.

**(ii) A shared publication cannot be edited with `SET TABLE`.**
`publication_membership_sql` emits `ALTER PUBLICATION {p} SET TABLE {members}`
(`crates/zeroship-migrate-server/src/publication.rs:46`), and `members` is
computed from `creator_table_query()` filtered to ONE app's schema (`:15-24`,
`WHERE n.nspname = $1`, threaded at `:91-93`). `SET TABLE` is a full replace. Two
tenants migrating concurrently would each replace the publication's whole
membership with their own view, and each would silently remove the other's tables
from CDC. The failure is silent on both sides: the removed tenant's live queries
simply stop updating.

**The advisory lock already present does not fix this, and that is worth stating
because it looks like it does.** `reconcile_in_transaction` opens with
`SELECT pg_advisory_xact_lock(hashtextextended($1, 0))` keyed on the publication
name (`publication.rs:84-89`). With a per-app publication that key is per app and
buys nothing across tenants. With ONE publication that key becomes a constant, so
it serialises every tenant's reconciliation cluster-wide - and a serialised full
replace is still a full replace. Last writer wins cleanly instead of racily. Keep
the lock; it is exactly the mutual exclusion a shared object needs. Replace the
statement it protects. (Its cluster-wide serialisation is an accepted cost: 12.7.)

**(iii) A published column is a catalog dependency.** MEASURED, 18.4, on a table
published as `(id, b, ssn_masked)`:

```
ERROR:  2BP01: cannot drop column b of table t because other objects depend on it
DETAIL:  publication of table t in publication p1 depends on column b of table t
HINT:  Use DROP ... CASCADE to drop the dependent objects too.
LOCATION:  reportDependentObjects, dependency.c:1148
```

and the CASCADE arm, which is worse than the error:

```sql
ALTER TABLE t DROP COLUMN a CASCADE;
-- NOTICE:  drop cascades to publication of table t in publication p1
SELECT pubname, tablename, attnames FROM pg_publication_tables WHERE pubname='p1';
-- (0 rows)
SELECT count(*) FROM pg_publication_rel r
  JOIN pg_publication p ON p.oid=r.prpubid WHERE p.pubname='p1';
-- 0
```

CASCADE does not narrow the column list. It removes the **whole table** from the
publication, and CDC for that table stops with a `NOTICE`.

`reconcile_app_publication` runs AFTER `apply_sealed`
(`crates/zeroship-migrate-server/src/apply.rs:762-780`: `apply_sealed` at `:762`,
`reconcile_app_publication` at `:780`). So a migration that drops a published
column aborts at the DDL, before reconciliation gets a chance to narrow the list.
This is not an exotic case. **Removing `.mask()` from a field is it**: the field's
`valueColumn` moves from the `_masked` sibling back to the parent
(`crates/zeroship-migrate-core/src/render/gen_types.rs:296-316`), the sibling is
dropped, and the sibling is precisely what the publication names.

**The specification: shrink before, DDL, widen after.** Three steps, and the
first and third are their own transactions on the migration connection.

1. **Shrink.** For every table of this app currently in the publication, set its
   column list to `old_wire INTERSECT new_wire`. A table leaving the declared set
   entirely is dropped from the publication. The published set never grows here.
2. **The DDL**, exactly as today (`apply_sealed`, `apply.rs:762`).
3. **Widen.** For every table in the app's new declared set, set its column list
   to `new_wire`; add tables that were not members. The epoch marker of section 8
   is emitted in **this** transaction.

MEASURED, 18.4, with `zs_cdc` holding `app_alpha.notes (id, body, ssn_masked)`
and `app_beta.items (id, label)`. This is the **shrink** transaction and the DDL
that follows it, which is the pair the post-apply-only order cannot express:

```sql
BEGIN;
  ALTER PUBLICATION zs_cdc DROP TABLE app_alpha.notes;
  ALTER PUBLICATION zs_cdc ADD  TABLE app_alpha.notes (id, ssn_masked);
  SELECT pg_logical_emit_message(true, 'zs.epoch', 'app_alpha:7');
COMMIT;
-- zs_cdc | app_alpha | notes | {id,ssn_masked}
-- zs_cdc | app_beta  | items | {id,label}        <- untouched
ALTER TABLE app_alpha.notes DROP COLUMN body;     -- ALTER TABLE, no CASCADE, no 2BP01
-- zs_cdc | app_alpha | notes | {id,ssn_masked}
-- zs_cdc | app_beta  | items | {id,label}
```

Four things that transcript rules on. `DROP TABLE` + `ADD TABLE (cols)` in one
transaction changes one member's column list. The `DROP COLUMN` that would have
raised `2BP01` a moment earlier now succeeds, with no `CASCADE` and therefore
without silently unpublishing the table. `pg_logical_emit_message` rides an
`ALTER PUBLICATION` transaction without objection. And **beta's membership and
column list are byte-identical before and after**, which is the control for (ii):
the same edit expressed as `SET TABLE` computed from alpha's view would have left
`zs_cdc` holding alpha's tables and nothing else.

The marker appears in the shrink transcript above only because that is where its
interaction with `ALTER PUBLICATION` was measured; in the specification it belongs
to the **widen** transaction, and 8.3 says why. The widen step is the same two
statements with `new_wire` in place of the intersection.

Four mechanics that the statement "use ADD/DROP instead of SET" hides, all
measured on 18.4:

- **Changing an existing member's column list is `DROP TABLE` then `ADD TABLE`,
  in one transaction.** `ADD TABLE` alone refuses:
  `ERROR: 42710: relation "t" is already member of publication "p1"`,
  `LOCATION: publication_add_relation, pg_publication.c:460`. `ALTER PUBLICATION`
  is transactional, so the pair is atomic and no concurrent decoder observes the
  table absent.
- **The drop set must be read from the catalog, not computed from the declared
  set.** `ALTER PUBLICATION ... DROP TABLE` on a relation that exists but is not
  a member is `ERROR: 42704: relation "unpub" is not part of the publication`
  (`PublicationDropTables, publicationcmds.c:1918`), and on a relation that does
  not exist at all is `ERROR: 42P01`. There is no `IF EXISTS`. The reconciler
  reads
  `SELECT tablename, attnames FROM pg_publication_tables WHERE pubname = $1 AND schemaname = $2`
  and diffs against the declared wire set. **`attnames` is the right catalog
  column**, measured directly: it returns `{id,ssn_masked}` for a column-list
  publication, follows a `RENAME COLUMN`, and returns zero rows once
  `DROP COLUMN ... CASCADE` has removed the table. `pg_publication_rel.prattrs`
  is not needed.
- **An empty column list is unrepresentable, and 3.1's UNION is what makes that
  safe.** PostgreSQL has no empty column-list form. Because
  `replica_identity_columns(collection)` is a UNION term in the formula, the
  shrink step can never produce an empty list. That is load-bearing, not
  incidental.
- **`RENAME COLUMN` needs no bracket but does need a marker.** MEASURED:
  `ALTER TABLE app_beta.items RENAME COLUMN label TO caption` left membership
  intact and `attnames` became `{id,caption}` - `prattrs` stores attnums, so the
  catalog follows the rename. But the wire NAME the relay sees changes, so the
  epoch marker still has to fire. And `DROP TABLE` of a whole relation removes
  membership silently, which is what the existing comment at
  `publication.rs:47-49` already says and which measurement confirms.

**Two consequences of the bracket, stated rather than discovered later.**

The window between shrink and widen publishes `old_wire INTERSECT new_wire`, a
subset of both. Narrower than intended, never wider. **The bracket can lose a
column from a change; it cannot leak one.** That polarity is the whole reason
shrink comes first.

If the DDL fails, the publication stays shrunk. CDC for that app then delivers a
narrower column set until the next successful migration, and the epoch marker -
which rides the widen transaction - never fires, so "shrunk with no marker" is the
observable. It self-heals, because the reconciler recomputes the full wire set
from the declared set on every apply rather than applying a delta. The operator
signal is 7.6's `cdc_published_columns` against the app's declared wire set.

The bracket also keeps the epoch marker on the right side of the DDL. A marker
emitted in a reconcile that ran only after `apply_sealed` would arrive after
changes written between the DDL commit and the reconcile commit, so those changes
would be decoded under the old publication and delivered *before* the marker
describing them. Under the bracket the marker is in the widen transaction and the
window before it publishes the intersection, so no frame ever crosses an epoch
boundary in the wrong direction.

### 3.7 The migration service holds the IR, not the descriptor

The wire set in 3.1 is written in terms of `storage.valueColumn`, which lives in
the runtime descriptor. **The migration service never receives a descriptor.**

`ApplyMigrationsRequest` (`crates/zeroship-migrate-server/src/apply.rs:49-73`)
carries `kind` (`:51`), `descriptor_sha256` (`:69`), `documents: Vec<IrDocument>`
(`:70`) and `policy` (`:72`). The field whose name promises a descriptor is a
lowercase-hex sha256 (`:52-53`), documented at `:55-68` as a deploy-ordering
anchor - the control plane refuses to make a deploy live unless the manifest's
`runtime_descriptor.hash` matches it. Nothing can be projected from a digest. And
`publication_membership_sql` takes `tables: &[String]` (`publication.rs:33`) -
table names, no fields.

The service does hold the material, because the descriptor and the DDL are folded
from the same ops. `apply_one_ir_file_postgres` produces the policy-resolved op
bytes at `apply.rs:1143`
(`resolve_shape_bytes(&raw_bytes, policy, project_schema, &file)`, whose comment
at `:1136-1142` says it folds the effective table-shape profile into every
`createTable` op before the fail-closed load gate). Those are exactly the ops the
descriptor renderer consumes: `render_artifacts`
(`crates/zeroship-migrate-core/src/render/gen_types.rs:508`) is a thin wrapper
over `render_schema_export` (`:548`), and `field_storage` (`:285-317`) is **the
one call** that decides whether a field has a second physical column and what it
is named - `mask_sibling_column_for_field(field, def)` returns the `_masked`
sibling for a masked field and the field's own name otherwise (`:296-316`).

**Specification.** `apply_bundle_ir_postgres` (`apply.rs:1059-1108`) already walks
the document set file by file; it accumulates the policy-resolved ops it produces
at `:1143`, folds them ONCE through `render_schema_export` (`gen_types.rs:548`),
and reads `wire_columns` off the resulting `SchemaExport` (`gen_types.rs:528-538`),
whose `collections` field (`:537`) is the typed `CollectionDescriptor` set kept
beside the artifacts - the shape the projection wants, stopped before the
flattening.

That fold runs ONCE per apply and yields the NEW wire set. The widen step (3.6,
step 3) applies it directly. The shrink step (step 1) intersects it with the LIVE
published set read from `pg_publication_tables`, not with a second fold of the
pre-apply ops: the catalog is what the server will actually enforce, a re-fold is
only what the service believes about it, and 3.6's drop-set rule already requires
reading the catalog because `ALTER PUBLICATION ... DROP TABLE` has no
`IF EXISTS`.

Two things this forbids. The service must not re-derive the sibling name -
`format!("{col}_masked")` at eight sites is the defect this whole line of work
exists to remove
(`crates/zeroship-migrate-core/src/render/gen_types/physical_storage.rs:3-8`
names the count and the review that found it). And it must not fetch the
descriptor over HTTP from the build: that would reintroduce the staleness window
12.3 rejects, over a network hop, on the path that gates creator writes.

`descriptor_sha256` stops being the only thing tying the projection to the
descriptor: `:64-68` says outright that the hash "proves ordering, not truth",
because a creator who hand-edits both generated files can make them agree about a
lie. A projection folded server-side from the ops the server is about to apply is
not client-declared.

---

## 4. Which crates move, which are deleted, what stays

The test applied is the one the index already adopted for the crate split
(`...-00-index.md:431-447`): a crate boundary must buy a dependency the compiler
refuses to invert, a separate compilation unit, or an artifact another process
consumes. Here it buys the third, which is the strongest of the three.

**New: `crates/zeroship-cdc`** (binary + lib), plus a small shared crate for the
wire types (see 5.2).

| file | verdict |
| --- | --- |
| `wal_consumer.rs` (1,440 lines) | **Moves.** The decode loop, `RelationEntry` and its `primary_key_index` (`:256-260`), `tuple_to_map`, `ensure_replication_param`, the backoff supervisor (`:710-843`) and `is_fatal` (`:743-760`) are the relay's core. What does **not** move: `SUPPRESSED_APPS`, `suppress_app`, `unsuppress_app`, `is_app_suppressed`, `SuppressGuard` and `emit_local` (`:66-164`). Those exist only to stop the in-process local-emit path double-delivering, and the Postgres local-emit path is deleted (below). |
| `replication.rs` (908 lines) | **Moves**, reshaped. `publication_name` stays a shared name-derivation in `zeroship-core::replication_names` (it already is: `replication.rs:34` imports it). `worker_slot_name` / `worker_slot_name_prefix` (`:114-132`) are **deleted**, since slots are no longer per worker. `ensure_worker_slot` (`:154-279`) becomes `ensure_cluster_slot`, one slot, held by the leader. `watchdog_query` (`:359-402`) and `SlotHealth` move and lose their per-app prefix filter, because the relay is the operator-side process and cluster-wide enumeration is correct there rather than a cross-tenant hazard. `drop_worker_slot` / `drop_worker_slots` / `drop_slot` (`:436-584`) are deleted with the per-worker slot. |
| `slot_reaper.rs` (593 lines) | **Deleted.** Section 2.2. |
| `change_stream_pg.rs` (315 lines) | **Deleted.** `SharedExit` (`:24-64`) and `WalConsumerHandle` (`:66-108`) are the process-local supervision of a task that no longer exists in the worker. `PgChangeStream::spawn_consumer` (`:170-264`) and `deprovision` (`:162-164`) go away with the per-worker slot. `pause_broker` / `engage_schema_pending` (`:269-278`) survive as broker functions, which is what they already delegate to. |
| `replication_ops.rs` (47 lines) | **Deleted.** `db.replication.watchdog()` exposes slot diagnostics to creator JavaScript for slots the creator will no longer own. Its own module comment already narrowed it once (`:1-7`). |
| `broker.rs` (1,915 lines) | **Stays in `zeroship-plugin-db`.** It is the in-process routing table, and its consumers are V8 subscription wrappers on the same thread. `ws_frame_for_change`, `ws_frame_for_control` and `ws_frame` (`:986-1029`) are deleted per 2.3. |
| `read_set.rs` (586 lines) | **Stays.** Capture happens inside `ctx.db.find` in the isolate, gated on the procedure kind (`:33-39`). It cannot leave the process that runs the query handler. |
| `cdc_lifecycle.rs` (523 lines) | **Stays**, reshaped. The refcounted per-app lease (`acquire` at `:87-111`, `release` at `:113-157`) still decides when this worker needs `app_id`'s stream. `RunningConsumer::Postgres(WalConsumerHandle)` (`:25-30`) becomes a handle on the relay subscription rather than on a local task. |
| `exec.rs` emit path | **Deleted outright, and it is the Postgres path.** See below. |

**The local-emit path serves POSTGRES, not SQLite, so it is deleted rather than
retained.** `backend_publishes_committed_changes()` is
`matches!(c.backend(), Some(BackendHandle::Sqlite(_)))` (`exec.rs:442-444`) - it
is TRUE on SQLite - and `emit_for_rows` **returns early** on it (`:494-500`, with
the comment "SQLite has a commit-time CDC publisher wired through the writer
actor's preupdate/commit hooks ... on SQLite it races the CDC publisher and
produces duplicate identical live snapshots"). SQLite builds its `ChangeEvent` in
`backend/sqlite/cdc.rs` and publishes it straight to the broker
(`cdc.rs:632-641`), never reaching `emit_for_rows`.

So the delete is:

- `emit_for_rows` (`exec.rs:481-554`), `queue_or_emit` (`:556-586`),
  `drain_pending_emits_on_commit` (`:600-613`) and `clear_pending_emits`
  (`:620-`);
- `exec_mutation_with_emit` (`:425-440`) collapses to `exec_mutation`, since its
  only added behaviour is the `emit_for_rows` call at `:438`;
- `SUPPRESSED_APPS`, `suppress_app`, `unsuppress_app`, `is_app_suppressed`,
  `SuppressGuard` and `emit_local` (`wal_consumer.rs:66-164`), whose remaining
  callers are `queue_or_emit` and `drain_pending_emits_on_commit`;
- the `pending_emits` slot on the isolate context, and with it **nine call sites
  in the transaction settle path** - `transaction/mod.rs:638`, `:766`, `:862`,
  `:1245`, `:1258`, `:1281`, `:1290`, `:1295`, `:1304`, plus the import at `:101`
  - and the two test hooks at `lib.rs:768` and `:777`.

That last bullet is why the polarity has to be read carefully before scoping the
work: this is not a three-function edit inside `exec.rs`, it reaches the
transaction settle path.

**Why deleting it is not optional.** It is a second producer of the same event
with a *different* projection: a two-name blacklist (`exec.rs:522`,
`.filter(|k| !matches!(k.as_str(), "created_at" | "updated_at"))`) against the
publication whitelist, running before the mask pass (2.3) against a stream that
has already been projected by the server. Two producers with two projections is
the exact shape that produced the current divergence between the read path and
the CDC path. One producer per backend.

**What `zeroship-plugin-db` keeps overall:** the whole data plane (CRUD, read
pipeline, encryption, masking, transactions, V8 classes), the in-process broker
and its read-set narrowing, the CDC *lease* bookkeeping, and the SQLite CDC
publisher - which is the ONLY local-emit producer left, and was never on the
deleted path. What it loses is every line that speaks the streaming replication
protocol, and with it the reason its process needs `REPLICATION`.

`ChangeStream` (`crates/zeroship-plugin-db/src/backend/mod.rs:946-989`) survives
as a capability trait with a changed Postgres implementation: `spawn_consumer`
becomes "subscribe to the relay", `deprovision` becomes "tell the relay to forget
this app". Note when navigating that file: its module doc at `:38-49` says it
declares "five" traits and then lists four; it declares **16**, and
`ChangeStream` is at `:946`.

---

## 5. The worker/relay wire contract

`ChangeEvent` (`broker.rs:81-119`) crosses a process boundary now. Today it is a
Rust struct passed by `Arc` inside one process (`broker.rs:756-759` wraps it once
and clones the `Arc` per subscriber), so it has never needed an encoding.

### 5.1 `zeroship-stream` is not the carrier, and the reasons are in its source

The register's C2 argument cites `StreamTransport`'s `partition_key` ordering
guarantee (`crates/zeroship-stream/src/transport.rs:32-37`) and the existence of
memory and Redpanda adapters. Both are true. The crate is still the wrong carrier
for CDC fan-out, for four reasons, each read out of the code rather than
inferred.

**(a) A consumer group partitions; live-query fan-out broadcasts.** The trait is
explicitly a Kafka-family consumer-group contract: "consumer groups assign each
partition to one consumer in steady state" (`transport.rs:35`), and the Redpanda
adapter subscribes with a `group.id` (`adapters/redpanda.rs:244`, `:253-256`). If
every worker joins one group, each event is delivered to exactly one worker. If
each worker gets its own group, the broker holds one consumer group per worker
and each worker consumes every app's events. The second is the tenant-isolation
regression that option B was rejected for
(`...-defect-register.md:1107-1113`).

**(b) `poll` takes no topic.** `async fn poll(&self, max: usize)`
(`transport.rs:48`). The topic is fixed at construction (`redpanda.rs:151-166`
`RedpandaConfig::topic`, `:255` `subscribe(&[topic])`). A per-app topic therefore
means a per-app `RedpandaTransport`, which means a per-app librdkafka producer,
consumer, and C thread set. `Cargo.toml:201-204` describes the adapter as running
"over librdkafka's own C threads".

**(c) `publish` is a blocking spin with one broker round trip per record.**
`redpanda.rs:297-325`: a `sync_channel(1)` per record, then
`loop { self.producer.poll(Duration::from_millis(10)); rx.try_recv() ... }`
inside an `async fn`, with `acks=all` and `enable.idempotence=true` (`:231-233`).
There is no batch API on the trait. That is a synchronous stall on a compio
thread, per row, for a carrier that would sit on the write-notification path. It
is fine for the billing outbox it was built for; it is not a CDC carrier.

**(d) The transport is plaintext and unauthenticated by construction, and the
payload class would change.** `redpanda.rs:106-135` states it: no TLS, no SASL,
`deny_unknown_fields` refuses security keys, and the workspace pins
`rdkafka = { version = "0.36", default-features = false, features = ["cmake-build"] }`
(`Cargo.toml:204`), so the linked C library has neither. Today's payload is
per-app usage counters. CDC payloads are creator **rows**. Moving row data onto
that wire changes the sensitivity class of the topic. Adding TLS is possible
without reintroducing tokio - `redpanda.rs:137-148` says exactly how, and that
`ssl` was never a default feature so `default-features = false` is not what
removed it - but it has not been done, and it is a prerequisite, not a footnote.

**What `zeroship-stream` remains right for:** the durable usage/billing outbox it
exists for. Nothing here proposes changing it. If a later revision wants the
relay's spool to be durable across relay restarts, `zeroship-stream` with a
per-app topic and (d) fixed is the candidate to re-evaluate; section 7 explains
why the first version does not need it.

### 5.2 The contract

**Encoding.** A new leaf crate `zeroship-cdc-wire`, no I/O, no V8, depended on by
both `zeroship-cdc` and `zeroship-plugin-db`. It defines an explicit,
length-prefixed binary framing with a one-byte frame tag, versioned by a
handshake integer rather than by per-frame flags.

The framing borrows pgoutput's own solution to the same problem, because the
problem is the same: column names must not repeat per row.

| frame | payload |
| --- | --- |
| `Hello` | wire version, relay id, leader term, `systemid`, `timeline` |
| `Relation` | `(app_id, incarnation, relation_generation, collection, columns: Vec<(name, is_replica_identity)>)` |
| `Change` | `(app_id, relation_generation, op, commit_lsn, change_index, pk, values: Vec<CellValue>)` positional against the last `Relation` |
| `Gap` | `(app_id, collection, pk, reason)` - see 5.6 |
| `Truncate` | `(app_id, collections)` - see 5.6 |
| `Epoch` | `(app_id, incarnation)` - see section 8 |
| `Resync` | `(app_id, reason)` |
| `Heartbeat` | `last_confirmed_lsn` |

`Change` carries values positionally against a `relation_generation`, so a row
costs its values plus a small header rather than its values plus its column
names.

**`(commit_lsn, change_index)` is the dedup key**, for the reasons measured in
6.3. `commit_lsn` is the LSN of the transaction's commit record and is available
on the FIRST frame of the transaction, not only at the end:
`PgOutputMessage::Begin` carries `final_lsn`, documented as "LSN of the commit
record (NOT the begin record)"
(`libs/compio-postgres/src/replication.rs:1879-1886`), and `Commit.commit_lsn`
(`:1891-1892`) is the same value. So the relay stamps it as it decodes, without
buffering the transaction. `change_index` is the relay's own 0-based count of
`Change` frames emitted so far within the transaction it is decoding, reset at
every `Begin`.

`CellValue` is three-way - `Value(Bytes) | Null | Unavailable` - not
`Option<Bytes>`. 5.6 says why collapsing the last two is the same class of
mistake section 3 exists to prevent.

`Hello` carries `systemid` and `timeline` because those, and not the leader term,
are what invalidate a worker's watermark (6.4).

Not `serde_json`. A JSON `HashMap<String,String>` per row is precisely what
`new_tuple` is today, and that shape is what let a bag of physical column names
become the thing on the wire.

**Transport.** The worker opens a long-lived HTTP response and reads frames off
the body until it closes. The pieces already exist and are already compio:
`ntex = { version = "3", features = ["compio"] }` (`Cargo.toml:45`) is the HTTP
server the peer service `zeroship-migrate-server` already uses
(`crates/zeroship-migrate-server/src/api.rs:3-5`); `cyper = { version = "0.8",
default-features = false, features = ["rustls", "json", "stream"] }`
(`Cargo.toml:48`) is the client, its `stream` feature is on, and
`crates/zeroship-worker/Cargo.toml:39` already names it. Authentication is a
service assertion (`crates/zeroship-core/src/service_assertion.rs`), which is the
tree's existing service-to-service identity mechanism.

Push rather than poll, because the point of the service is to reduce end-to-end
latency relative to WAL-to-worker, and a poll interval is a latency floor chosen
in advance. Push without a credit scheme puts the whole slow-consumer question on
the relay, which is where 5.4 answers it.

**Registration.** The worker sends the set of `app_id`s it currently leases
(`cdc_lifecycle.rs:87-111` already computes it) in the request, and re-sends on
change. The relay routes per `(app, collection)` exactly once, in the process
whose only job that is.

### 5.3 Delete, and "row left the view", without a full before-image

Section 3.5(b) rules out `REPLICA IDENTITY FULL`. So a DELETE frame carries the
replica-identity columns and the pk, and an UPDATE carries an `old_tuple` only
when the replica-identity columns changed (this is pgoutput's own contract,
documented at `libs/compio-postgres/src/replication.rs:1930-1932`).

The broker's current answer is to test the predicate against both tuples
(`broker.rs:293-297`). Without a before-image that arm cannot fire, and a row
that leaves a subscriber's view would silently stop updating.

The replacement: the subscription keeps the bounded set of pks it has delivered
into the subscriber's current view. An event whose pk is in that set is delivered
regardless of whether the new tuple matches the predicate, so the subscriber
learns the row left. The set is bounded by the query's page size; overflow emits
`Resync`, which the broker already models (`broker.rs:318-340`) and the
live-query client already absorbs (7.3).

This is **not prototyped**, and it is the one part of the design where the
subscriber-side contract changes shape rather than moving (12.4, 13).

### 5.4 Slow consumers: the relay sheds, it never blocks

**The invariant, and everything else in this subsection is a consequence of it: a
consumer must never be able to reach the slot.** The ring writer is also what
advances `highest_durable_lsn`, and `highest_durable_lsn` is what confirms the
slot (6.3). A ring writer that can be blocked by a subscriber is a subscriber
that can stop LSN confirmation, and a slot that stops confirming grows WAL for
every tenant on the cluster - which is exactly the failure section 7.2 exists to
bound. The prior art has this scar: Vitess issue 11169, a slow VStream client
plus a capacity-1 buffer, blocked `servePrimary()` and hung a replica promotion.
Consumer backpressure reached the HA control plane. Ours would reach WAL
retention, which is worse, because WAL retention is shared and a replica
promotion is not.

**There is a working precedent in this repo, and the relay copies it.** The
in-process broker already solves this shape: `Subscription::push`
(`broker.rs:317-340`) is not `async` and never awaits. On overflow it clears the
queue and pushes ONE `Resync` (`:325-331`), with a `resync_pending` flag
(`:191-193`) collapsing successive overflows so a wedged subscriber cannot make
the publisher do work proportional to how wedged it is. Depth is
`DEFAULT_QUEUE_DEPTH = 1024` (`broker.rs:144`); concurrency is capped by
`MAX_SUBSCRIPTIONS_PER_APP = 256` (`:151`). The relay must not regress on the
code it replaces.

Specification:

- **The ring writer's `push` is not `async` and takes `&self`.** That is the seam
  that enforces the invariant in code rather than in prose: if it ever needs to
  be `.await`ed, the invariant is gone and the compiler says so at every call
  site.
- **Each subscribing worker connection owns a read cursor into the app's ring and
  its own bounded egress buffer.** Fan-out is done by the connection tasks
  reading their cursors, not by the writer walking a subscriber list. So the
  writer's cost per frame is O(1) in subscriber count.
- **A cursor that falls behind the ring's tail gets `Resync(app_id, RingOverrun)`
  and jumps to the head.** It is not disconnected. Disconnecting makes the worker
  re-register and re-lease, which is more work under load, not less.
- **A connection whose egress buffer has been full for longer than
  `slow_consumer_timeout` is CLOSED.** The worker reconnects and receives
  `Resync`. Closing beats holding: a socket the peer is not reading is relay
  memory the ring needs, and the peer has already lost its position.
- **No credit scheme, and this is a decision rather than an omission.** Credit is
  flow control, and flow control on this feed means a consumer can slow the
  producer, which is the one thing the invariant forbids. The consumer's only
  legitimate signal is "I fell behind", and `Resync` already carries it.

There is one honest cost. Under this policy a worker that is merely slow rather
than wedged gets `Resync` storms, and on the live-query path each `Resync` is a
full refetch (7.3). A slow subscriber is therefore converted into database read
load. That is the right trade - read load is bounded by the app's own spend limit
and WAL growth is not - but it is a trade, and 12.10 argues it.

### 5.5 Ring sizing

Four numbers, all operator configuration, all with defaults:

| knob | bounds | why this one |
| --- | --- | --- |
| `ring_bytes_per_app` | byte depth of one app's ring | The primary bound. Frames are variable-size; a frame count bounds nothing when one row can be a megabyte |
| `ring_frames_per_app` | frame count of one app's ring | A hard cap so a stream of tiny frames cannot make the index structure unbounded even inside the byte budget |
| `ring_retention` | age of the oldest frame kept for an app with no connected subscriber | The one that decides correctness of the resume story: how long a worker can be away before it must `Resync` instead of resuming |
| `ring_bytes_total` | the process | Per-app times tenants is what actually exhausts the relay. On breach, evict from the app with the LARGEST ring, never the globally oldest frame - global-oldest punishes idle tenants for a noisy one |

Eviction is always oldest-first within one app's ring, and always raises that
app's `resync_pending`. **There is no drop-the-newest arm**: the newest frame is
the one a live query most needs, and a policy that discards it converts a burst
into a permanently stale view rather than a refetch.

The retention windows of durable-log products - Salesforce CDC 72 hours, Kinesis
and DynamoDB Streams 24 hours by default, Neon's 40-hour slot reaper, all
**unverified** and relayed from the prior-art study - are **not** targets. Their
retention *is* the product. This ring is a fan-out buffer in front of a slot that
PostgreSQL is already retaining WAL for, and the durable log is the WAL. The
right first value is the one that makes `Resync` rare for a worker restarting
normally and never lets one tenant's burst evict another's, which is derived from
section 11's worker-restart and per-app-write-rate measurements.

What forbids simply making the ring large: RSS is the resource one tenant can
exhaust for all of them, and 7.5 exists because of that. A bigger ring buys
disconnect tolerance and sells fault isolation.

### 5.6 The vocabulary for what the relay cannot represent

`Resync` is a total per-app reset - `resume_app_with_resync` pushes it to every
subscription of an app (`broker.rs:724-746`). That is the right frame for "the
ring lost your position". It is the wrong frame for three things the relay will
meet, and the prior art made all three first-class: Salesforce has `GAP_UPDATE`
and `GAP_OVERFLOW` as distinct event types, and Debezium ships a configurable
`unavailable.value.placeholder`.

1. **One change for one row that cannot be represented.** A value over the frame
   budget; a type the wire format has no encoding for; an UPDATE whose old tuple
   is absent when a subscriber needed it (5.3). Resyncing a whole app for one row
   is a refetch storm caused by one bad row.
2. **A value the server did not send.** pgoutput sends an unchanged-TOAST marker
   in place of a large unmodified value. The relay must not guess and must not
   encode it as NULL.
3. **A truncate.** `PgOutputMessage::Truncate`
   (`libs/compio-postgres/src/replication.rs:1948-1955`) names relation ids and
   carries no rows. It is not a `Change` and it is not a `Resync`; it is a fact
   about every row of a collection.

So, beside `Resync`:

| frame | payload | meaning |
| --- | --- | --- |
| `Gap` | `(app_id, collection, pk, reason)` | one change for one row could not be represented; that row's current state is unknown, everything else is intact |
| `Truncate` | `(app_id, collections)` | every row of these collections is gone |

and inside `Change`, `CellValue` is `Value(Bytes) | Null | Unavailable` rather
than `Option<Bytes>`. **Encoding "the server did not send it" the same way as "it
is NULL" is the same class of collapse section 3 exists to prevent**: a
downstream consumer that cannot tell them apart will eventually write the wrong
one into a cache and call it a value.

`Gap.reason` is a **closed** enum - `OversizeValue`, `UnrepresentableType`,
`MissingOldTuple`. Closed rather than a string, because an open reason field is
somewhere a physical column name reappears on the wire the moment someone writes
a helpful error message.

The subscriber contract absorbs both new frames for free on the path that
matters: `sdks/db/src/live.ts:333-338` reruns on `change` and `resync` alike, so
`Gap` and `Truncate` join that arm and cost one refetch. On the raw
`db.subscribe` path they are two more `kind` values a creator can ignore, which
is 7.3's existing honest problem made two cases wider - argued in 12.9.

---

## 6. Leader election and resume, without tokio

### 6.1 One database is shared by every app: verified

- `deploy/ops/postgres-init.sql:4`: "The whole platform shares ONE database, and
  `zeroship` is the schema holding platform state."
- `deploy/compose/docker-compose.yml:124`: `POSTGRES_DB: zeroship`. One postgres
  service, one database.
- `db/migrations-ts/20260702000100_schema_roles_extensions.ts:13-23` creates a
  **schema** (`zeroship`) and nine **roles**. It creates no database. Roles are
  cluster-scoped.
- App schemas are schemas in that same database:
  `crates/zeroship-schema/src/query.rs:1026-1028`,
  `CREATE SCHEMA IF NOT EXISTS <quoted app_id>`. The WAL consumer's tenant filter
  is `rel.namespace != self.app_id` (`wal_consumer.rs:605-607`), which only works
  because the app id *is* the schema name in the connected database.
- The worker holds one DSN for all apps
  (`crates/zeroship-worker/src/config.rs:59-60`).
- `slot_reaper.rs:206-220` enumerates slots with `database = current_database()`,
  which is the same assumption from the other side.
- `grep -rn "CREATE DATABASE" crates/ db/ deploy/` returns hits only in
  `crates/zeroship-migrate/tests/**` (MySQL test fixtures). No production path
  creates a per-app database.

So this is **one leader per cluster**, holding **one slot**. Not one per app, not
one per database. If a second cluster is ever introduced, it is one leader per
cluster and the election key must include the cluster identity; that is stated
here so the next reader does not have to re-derive it.

### 6.2 Election

`pg_try_advisory_lock` on a dedicated key, on a dedicated session-scoped
connection, over `compio-postgres`. The pattern is already in the tree and
already compio: `slot_reaper.rs:171-180` runs
`SELECT pg_try_advisory_lock($1::INT4, $2::INT4) AS acquired` for fleet-leader
election with namespace and key constants at `:37-38`, and `:149-169` are the
take and release helpers. PostgreSQL releases a session advisory lock when the
session ends, so a crashed leader releases without a lease timer, a heartbeat, or
a clock.

Non-leaders retry on an interval and serve nothing. There is exactly one holder
of the slot at a time, which is also what PostgreSQL enforces independently: a
logical slot admits one active consumer (`replication.rs:107-113` states this as
the reason per-worker slots existed).

A `leader term` is minted from a monotonic counter in the platform schema on each
acquisition, and it rides in the `Hello` frame. A worker that sees a term lower
than the one it has already seen refuses the connection. This is the guard
against a partitioned old leader that still holds a TCP connection to a worker;
without it, two relays could deliver interleaved streams and the dedup in 6.3
would silently drop the newer one.

**That is the term's only job.** It does NOT reset a worker's dedup watermark -
6.4 owns that rule.

**Zero tokio.** Every component named is already compio: `compio-postgres` for
the lock and the replication connection, `compio::time::sleep` for backoff
(already used at `wal_consumer.rs:825`), `flume` for shutdown channels
(`change_stream_pg.rs:188-189`), `ntex` with the `compio` feature for the HTTP
server, `cyper` for the client. Nothing in the design requires an async runtime
that is not already in the workspace.

### 6.3 Resume and at-least-once

**Resume.** `ensure_worker_slot` already returns `confirmed_flush_lsn`
(`replication.rs:269`, `SetupOutcome.confirmed_flush_lsn` at `:294-298`) and
`change_stream_pg.rs:186` already threads it into
`WalConsumer::with_start_lsn`. `start_lsn = "0/0"` means "use the slot's
`confirmed_flush_lsn`" (`wal_consumer.rs:286-289`). The relay uses the same
mechanism unchanged.

**Confirmation, and the clamp.** `wal_consumer.rs:449-497` advances on `Commit`
and sends a standby status update; the long comment at `:459-494` records why a
mid-transaction `wal_end` is safe to report and includes a measurement of
`restart_lsn` staying pinned under an open transaction. That reasoning carries
over and should move with the file, because it is the only place it is written
down.

The relay confirms the commit LSN once the transaction's frames are durable **in
the relay's own ring** (5.4, 5.5), not once a worker acknowledges them. That is
what converts WAL retention into relay-owned retention - and it is also what
turns a correct line of today's code into a durability bug on the way in.

`wal_consumer.rs:441` does `stream.advance_lsn(wal_end)` on `PrimaryKeepalive`.
That is right today and must not simply be deleted: it is what stops an idle slot
from pinning WAL forever, the Postgres CDC footgun Debezium ships a heartbeat
table for. But `wal_end` is the SERVER's current end of WAL
(`libs/compio-postgres/src/replication.rs:1064-1071`), which can be arbitrarily
far ahead of anything the relay has made durable. Under the ring-durability
promise, confirming it confirms WAL the relay cannot replay, and PostgreSQL is
then free to recycle it.

**Specification: `min(wal_end, highest_durable_lsn)`.** On an idle database the
relay has drained everything, the two are equal, and the idle-slot protection is
unchanged.

**Put the clamp on the operation, not on one call site.** `advance_lsn` is called
from three places in the same loop - `:441` (keepalive), `:454`
(`Commit.end_lsn`) and `:495` (mid-transaction `wal_end`) - and the third has the
same defect for the same reason. The comment at `:459-494` argues that a
mid-transaction position cannot suppress replay of the transaction *in progress*,
which remains true; what it does not cover is that the position also sits past
every EARLIER transaction's commit record, and under the relay those earlier
transactions' frames may still be in flight to the ring. Fixing only the
keepalive arm produces a relay that confirms unreplayable WAL just during
long-running transactions - rarer, harder to reproduce, identical in consequence.
So the relay owns one `confirm(pos)` helper that clamps, and the three sites call
it. At `:454` the clamp is a no-op by construction, because the commit is only
confirmed after that transaction's frames are durable - which is the invariant to
assert in a test rather than a coincidence to rely on.

**At-least-once and dedup. The key is `(commit_lsn, change_index)`.** The
per-change LSN cannot be the key, and both reasons are measured on PostgreSQL
18.4.

*Within* a transaction, the ordinary case looks fine. Three single-row `INSERT`s
in one transaction:

```
    lsn    | xid | frame
 0/178B0B0 | 755 | B
 0/178B0B0 | 755 | R  public.t (id, a, ssn_masked)
 0/178B0B0 | 755 | I  1
 0/178B1A0 | 755 | I  2
 0/178B230 | 755 | I  3
 0/178B2F0 | 755 | C
```

Three distinct change LSNs. (Note in passing that the first change shares the LSN
of `B` and `R`; only `Change` frames are compared, so that does not matter, but
it is the first sign that these LSNs are positions rather than identifiers.)

**Change LSNs go BACKWARDS between transactions.** Logical decoding delivers
transactions in COMMIT order, but a change's LSN is its position in the WAL,
which is INSERTION order. Two overlapping transactions therefore arrive with
their changes out of LSN order. MEASURED, 18.4 - session A opens a transaction
and holds it, session B inserts and commits, then A commits:

```
    lsn    | xid | frame
 0/17DCEF0 | 768 | B          <- B, delivered first because it commits first
 0/17DCEF0 | 768 | R
 0/17DCEF0 | 768 | I  id=11
 0/17DCFB0 | 768 | C
 0/17DCE60 | 767 | B          <- A, delivered second
 0/17DCE60 | 767 | I  id=10
 0/17DCFE0 | 767 | C
```

The second delivered `Change` carries `0/17DCE60`, which is **lower** than the
first delivered `Change`'s `0/17DCEF0`. "Keep the highest and drop anything at or
below" discards row 10 permanently, on two ordinary concurrent writers, with no
error anywhere. The single-writer transcript above cannot see this, which is why
10.3 requires a concurrency arm.

**`heap_multi_insert` collapses many changes onto one LSN.** MEASURED, 18.4, same
table, same publication, same slot, same session, same three rows - one variable
changed, the statement that writes them:

```sql
INSERT INTO t (id,a) VALUES (1,'x'),(2,'y'),(3,'z') RETURNING id;
--  0/178AA50 | 755 | I
--  0/178AB30 | 755 | I
--  0/178ABB0 | 755 | I      three distinct LSNs

COPY t (id,a) FROM STDIN WITH (FORMAT csv);   -- the same three rows
--  0/178AC60 | 756 | I
--  0/178AC60 | 756 | I
--  0/178AC60 | 756 | I      ONE lsn for three rows
```

The multi-`VALUES` arm is the control: it proves the instrument can see distinct
per-change LSNs on this slot, so the collapse is the `heap_multi_insert` WAL
record and not the probe. `env.db.insertMany` emits the multi-`VALUES` shape
(`crates/zeroship-schema/src/query.rs:4013-4157`,
`INSERT INTO {schema}.{table} ({cols}) VALUES {tuples} RETURNING *`), so no
creator write reaches the collapse **today**. `COPY`, `CREATE TABLE AS` and some
`INSERT ... SELECT` plans do. "Not reachable from one caller today" is not a
property a wire format should be built on.

**The key that survives both.** `commit_lsn` is strictly increasing in delivery
order, because transactions are delivered in commit order and every commit record
occupies a distinct WAL position - visible in the interleave transcript above,
where B commits at `0/17DCFB0` and is delivered first while A commits at
`0/17DCFE0` and is delivered second, in spite of A's change being earlier in the
WAL. Within one transaction `commit_lsn` is constant and `change_index` strictly
increases by construction. So the pair is strictly monotone in delivery order,
and the rule is:

> The worker keeps the highest `(commit_lsn, change_index)` it has applied per
> app and drops anything lexicographically at or below it. One comparison of a
> `u64` and a `u32`. Not a set.

Two things make the index trustworthy. It is per transaction, reset at every
`Begin`, **not** a global counter - a global counter would not survive a relay
restart, because resume re-delivers whole transactions from their `Begin` and the
counter would restart at a different place. And `commit_lsn` is available at
`Begin` (`Begin.final_lsn`, `libs/compio-postgres/src/replication.rs:1879-1886`),
so stamping it costs no buffering.

**Do not adopt Salesforce's three-header design** (`commitNumber` +
`transactionKey` + `sequenceNumber`). It solves transaction *reconstruction*
across a lossy replay-id bus. We have one ordered stream per slot and never need
to reassemble a transaction from unordered pieces, so `transactionKey` buys
nothing. Two fields, not three.

**Ordering.** pgoutput delivers changes in commit order within one slot, so
per-app order is preserved by construction as long as the relay does not reorder.
The relay must therefore not fan out across threads per app in a way that can
reorder; one ring per app with a single writer is the constraint, and it is the
reason the ring is per app rather than global.

### 6.4 What invalidates a watermark: the timeline

**A worker resets its per-app dedup watermark exactly when the `systemid` or the
`timeline` in `Hello` differs from the last `Hello` it accepted, and at no other
time.** In particular, an increased leader term does not reset it.

Resetting on a leader term would fire when nothing is wrong. A new leader resumes
from the slot's `confirmed_flush_lsn` (`replication.rs:269`,
`SetupOutcome.confirmed_flush_lsn` at `:294-298`), so every frame it replays is
one the worker has already applied - exactly what the monotone rule drops for
free. Resetting there turns every routine leader change into a duplicate storm
delivered into creator code, at the moment you least want extra load.

The timeline is what genuinely invalidates a watermark. **LSNs are positions on a
timeline, and a timeline change makes them mean something else.** After a
promotion or a point-in-time recovery, WAL after the divergence point is
different WAL at the same numeric positions. A watermark carried across that
boundary silently suppresses new changes whose LSNs happen to sit below it.
Materialize gates its CDC source on `publication_details.timeline_id` for
precisely this.

**Measured on PostgreSQL 17.11, two runs differing in one variable - whether
`recovery.signal` was present:**

| recovery shape | `system_identifier` | `timeline` | data |
| --- | --- | --- | --- |
| basebackup + `recovery.signal` + promote | unchanged | **1 -> 2** | rewound |
| SIGKILL, restart (crash recovery) | unchanged | **1, unchanged** | replayed |

So the promoting case works: the timeline moves and the watermark is
invalidated. **The non-promoting case does not, and it is a rewind this gate
cannot see.** A new timeline is created only when *archive* recovery completes;
crash recovery replays whatever WAL is on disk and comes up on the same pair. A
filesystem, EBS, ZFS or LVM snapshot restored and started without
`recovery.signal` therefore rewinds the WAL while `IDENTIFY_SYSTEM` reports an
unchanged `(systemid, timeline)` - the relay resumes from its watermark against
different WAL at the same positions, which is exactly the failure this section
exists to prevent.

That is an accepted bound, not a defect to patch here, and it must be stated
rather than left implied: **the watermark is invalidated by every recovery that
completes archive recovery, and by no other.** The operator rule that makes it
true is that restores use `recovery.signal`. The same measurement and its
consequences for identity are recorded in
`2026-08-26-sc5-service-ownership.md`.

**The relay already fetches the answer and throws it away.**
`ReplicationConnection::identify_system` returns
`IdentifySystem { systemid, timeline, xlogpos, dbname }`
(`libs/compio-postgres/src/replication.rs:816-821`, `timeline: u32` at `:818`),
and `wal_consumer.rs:377` discards the value:
`if let Err(e) = conn.identify_system().await { ... }` - the success arm has no
binding.

Specification:

- On every leader acquisition and every replication connect, the relay reads
  `(systemid, timeline)` from `IDENTIFY_SYSTEM` and persists them beside the
  slot's identity in the platform schema.
- A changed **`systemid`** is a different cluster - a restore onto new storage.
  Every watermark and the slot itself are meaningless: drop the slot, recreate,
  `Resync` every app.
- An increased **`timeline`** is a promotion or a PITR. Same treatment. It is not
  a smaller event than a `systemid` change, because reused LSNs are more
  dangerous than absent ones: absent LSNs error, reused ones are silently
  accepted.
- `Hello` carries `(systemid, timeline)`; the worker's reset rule is the one at
  the top of this subsection. The leader term stays where it belongs, refusing an
  older leader's connection (6.2).
- A relay that wants this check without a replication connection has an SQL
  oracle: `SELECT timeline_id FROM pg_control_checkpoint()`. MEASURED on 18.4, it
  returns `1` on a fresh cluster. Useful for the watchdog surface; the
  replication connection's own `IDENTIFY_SYSTEM` is the authority, because it is
  the same connection the stream runs on.

A real timeline change was **not produced** - that needs a standby and a
promotion, and section 13 records it as a gap. What is verified is that the value
is on the connection the relay already opens and is currently dropped on the
floor.

---

## 7. Failure behaviour

### 7.1 The relay is down

Nothing consumes the slot. `restart_lsn` stops advancing. WAL accumulates on a
disk shared by every tenant in the cluster. Unbounded, this is a cluster-wide
outage caused by one process, which is L12b's mechanism with a different owner.

### 7.2 `max_slot_wal_keep_size` must be set: measured

`deploy/compose/docker-compose.yml:116-121` is the entire postgres command:

```
command:
  - postgres
  - -c
  - wal_level=logical
  - -c
  - max_prepared_transactions=10
```

`grep -n "max_slot_wal_keep_size\|max_replication_slots"` over that file returns
nothing. MEASURED on the probe container:

```
           name            | setting | boot_val | unit |  context
---------------------------+---------+----------+------+------------
 logical_decoding_work_mem | 65536   | 65536    | kB   | user
 max_connections           | 100     | 100      |      | postmaster
 max_replication_slots     | 10      | 10       |      | postmaster
 max_slot_wal_keep_size    | -1      | -1       | MB   | sighup
 max_wal_senders           | 10      | 10       |      | postmaster
```

Two things worth taking from that table beyond the `-1`. First,
`max_slot_wal_keep_size` has `context = sighup`: it is changeable with a
`pg_reload_conf()`, not a restart, unlike `max_replication_slots` and
`max_wal_senders` which are `postmaster`. Setting it is cheap. Second, it is the
only one of the five that is unbounded by default.

MEASURED, the full degradation, on the same container:

```sql
ALTER SYSTEM SET max_slot_wal_keep_size = '0';
SELECT pg_reload_conf();          -- t;  SHOW max_slot_wal_keep_size -> 0
-- generate WAL with the slot's consumer absent, then CHECKPOINT + pg_switch_wal
SELECT slot_name, active, restart_lsn, confirmed_flush_lsn, wal_status FROM pg_replication_slots;
--  s1 | f |  (null)  | 0/151B0B0 | lost
```

and reading it:

```
ERROR:  55000: can no longer get changes from replication slot "s1"
DETAIL:  This slot has been invalidated because it exceeded the maximum reserved size.
LOCATION:  CreateDecodingContext, logical.c:607
```

So with the GUC set, a down relay costs bounded disk and a lost slot instead of a
full disk. The relay's restart path reads `wal_status` (already modelled:
`SlotHealth.wal_status` at `replication.rs:331-335`, documented values `reserved`
/ `extended` / `unreserved` / `lost`), and on `lost` it drops the slot, recreates
it, and emits `Resync` for every app.

Note that `restart_lsn` becomes NULL on invalidation, which makes
`watchdog_query`'s `lag_bytes` CASE (`replication.rs:370-372`) return NULL. The
`wal_status` column is the only surviving signal, which is why it exists and why
the relay must not key its health check on lag alone.

**The value to set is an operator decision, not a constant in this document.** It
has to be chosen against the cluster's WAL volume and the relay's worst
acceptable downtime. What this document fixes is that it must not be `-1`.

### 7.3 Does the subscriber contract genuinely absorb `Resync`?

The answer differs between the two subscriber surfaces.

**Live queries: yes, genuinely.** `sdks/db/src/live.ts:333-338`:

```
// Both `change` and `resync` trigger a rerun. A resync means
// the broker dropped events; the safest response is a full
// refetch, which is exactly what `rerun()` already does.
await rerun();
```

There is no separate code path. A `Resync` costs one refetch, which is the same
work a `change` already causes, so a burst of Resyncs is a burst of refetches and
not a correctness problem. Note that `rerun()` fires on **every** change event
too, so the live-query path is already refetch-per-event; a Resync is not a
degradation there at all.

**Raw `db.subscribe`: no, it is the creator's problem.** `SubscriptionEvent`
(`sdks/db/src/subscribe.ts:38-54`) surfaces `{kind: "resync"}` with the doc
comment "Bounded queue overflowed; client must re-fetch". A creator who writes
`for await (const ev of db.subscribe("messages"))` and switches on
`ev.kind === "change"` silently ignores it and diverges. That is a documented
contract the creator can get wrong, not a mechanism that absorbs anything.

So the honest statement is: **the mechanism the design relies on is real on the
path the design serves (live queries), and is a documented obligation on the raw
path.** If the raw path is meant to be relied on under a relay, its `Resync`
needs to become something a creator cannot ignore, and that is a separate
decision this document does not make.

### 7.4 The current fatal-error classifier does not know about invalidation

`is_fatal` (`wal_consumer.rs:743-760`) matches on lowercased substrings: `58p01`,
`does not exist` conjoined with `replication slot` or `publication`, and
`invalid slot name`. The invalidation error measured in 7.2 is SQLSTATE `55000`
with the message "can no longer get changes from replication slot". **None of the
three arms match it**, so today's supervisor would classify it as transient and
retry it forever at the 30-second cap (`wal_consumer.rs:712`, `MAX_BACKOFF`). The
relay's classifier must key on SQLSTATE, and `55000` from `START_REPLICATION`
must route to the drop-recreate-Resync path rather than to backoff.

There is a collision to be careful about: `replication.rs:229-247` already maps
SQLSTATE `55000` to
`Configuration { code: "wal_level_not_logical", hint: "set wal_level=logical in postgresql.conf and restart" }`,
with a comment asserting that `55000` "is the canonical SQLSTATE when
wal_level != logical". It is the canonical SQLSTATE for
`object_not_in_prerequisite_state` in general, and section 7.2 shows a second,
entirely different condition that raises it. In situ today that mapping is on
`pg_create_logical_replication_slot` only, so it is not currently wrong; the
comment's general claim is, and a relay that reuses the mapping on
`START_REPLICATION` would tell an operator with a full WAL disk to go set
`wal_level`.

### 7.5 Per-app fault isolation inside one process

Today a worker's CDC failure affects that worker's apps. Under this design one
process decodes and fans out for **every tenant on the cluster**, so a defect
that used to be per worker becomes per cluster. The prior art is unambiguous
about how this goes wrong: Supabase Realtime had three project-wide outages
traced to an un-trapped per-subscriber loop, from three unrelated root causes.
Same shape, larger radius.

Five rules, in the order they bind:

- **The decode loop touches no per-app code.** Decoding produces
  `(schema, relation, tuple)` and nothing else. Everything app-specific happens
  on that app's ring writer, downstream of the tenant filter
  (`wal_consumer.rs:605-607`, `rel.namespace != self.app_id` today; cluster-wide
  it becomes a schema-to-app lookup). A crash in per-app code must not be able to
  be a crash in decoding.
- **Every per-app step is total.** A per-app step returns `Result`, and its error
  becomes a `Gap` (5.6) or a `Resync` for that app. It does not return `()` and
  panic instead. This is checkable rather than aspirational:
  `clippy::unwrap_used`, `clippy::expect_used` and `clippy::indexing_slicing` at
  deny level on the relay crate, which `./tests/clippy_gate.sh` already runs under
  `--all-features`. A compile-time rule is cheaper than a catch-and-continue arm
  and does not create a second, quieter control path.
- **A panic that still happens kills the process, deliberately. Do not
  `catch_unwind` per-app work.** compio is one runtime; catching a panic inside a
  task leaves whatever it was mutating in an unknown state, and the state here is
  a slot cursor and a durability watermark. A relay that dies releases the
  session advisory lock and a standby takes over (6.2). A relay that limps is a
  silent per-app data-loss machine that still holds the lock.
- **One app cannot starve another.** The ring writer is O(1) per frame and
  allocates nothing proportional to subscriber count; connection tasks read their
  own cursors (5.4). That is the structural difference from the Supabase shape,
  in which the publisher walked its subscriber list.
- **A poisoned app is quarantined, and quarantine never touches the slot.** An
  app whose ring has raised `Resync` more than `resync_storm_threshold` times
  within `resync_storm_window` is marked degraded: the relay stops writing frames
  for it at the tenant filter, keeps decoding and keeps confirming the slot, and
  reports it (7.6). **A tenant that cannot be served must never become a tenant
  that stops WAL confirmation for everyone** - which is 5.4's invariant reached
  from the other side.

### 7.6 What the relay measures, and why the obvious signals are blind

Take the inversion first, because it makes the standard runbook wrong.

The relay confirms the slot once frames are durable in its ring (6.3), not once a
worker has them. **So `confirmed_flush_lsn` advances at FULL SPEED while delivery
to every worker is failing.** The most reached-for PostgreSQL health number moves
fastest exactly when the service is most broken. `watchdog_query`'s `lag_bytes`
is computed from it (`replication.rs:370-372`) and is blind the same way.

`safe_wal_size` is the other obvious candidate and is blind twice over. MEASURED
on 18.4: with `max_slot_wal_keep_size = -1` - the deployed default, per 7.2 -
`safe_wal_size` is NULL for every slot; after
`ALTER SYSTEM SET max_slot_wal_keep_size='64MB'` and `pg_reload_conf()` the same
query returns `72 MB`. So the column does not exist as a number until 7.2 is
done, and once it does it measures the slot, which is precisely the thing that
stays healthy through a delivery outage.

The signals that are not blind are relay-side and per app:

| metric | why it is not blind |
| --- | --- |
| `cdc_oldest_undelivered_age_seconds{app}` | **The health signal.** Age of the oldest frame no connected cursor has passed. Zero when delivery is healthy, unbounded when it is not, and it depends on no worker reporting anything |
| `cdc_ring_depth_bytes{app}` / `cdc_ring_depth_frames{app}` | The buffer fills exactly when delivery fails, and it is what 5.5's knobs bound |
| `cdc_resync_total{app,reason}` / `cdc_gap_total{app,reason}` | The loss counters, split by 5.6's closed reason enum, so a ring overrun is distinguishable from an oversize value |
| `cdc_connected_workers{app}` | "No subscribers" and "every subscriber wedged" both leave the ring shallow, because 5.4 evicts. Without this they alert identically |
| `cdc_published_columns{app,collection}` | The bracket's failure window (3.6): a published set that is a strict subset of the declared wire set, with no migration in flight, means a shrink whose widen never ran |
| `cdc_leader{relay_id,term}`, `cdc_slot_wal_status` | The two facts an operator needs before reading any of the above |

The exact label sets above will drift once the relay is written; the durable
content is the paragraph explaining why `confirmed_flush_lsn` is blind. If the
table and the paragraph ever disagree, believe the paragraph.

One PostgreSQL-side number IS honest: **`restart_lsn` lag, not
`confirmed_flush_lsn` lag.** `restart_lsn` is pinned by the oldest transaction
the server still needs and the client cannot move it - the measurement is already
in the tree, at `wal_consumer.rs:474-487`. `watchdog_query`
(`replication.rs:359-402`) already selects it. It goes NULL on invalidation
(7.2), so `wal_status` stays primary and `restart_lsn` secondary.

The relay is an operator-side process, so this surface is the moved
`watchdog_query` plus these counters on the relay's own HTTP server, not a
creator-visible namespace - `replication_ops.rs` is deleted for that reason
(section 4).

---

## 8. The schema-change signal

**A relay stamping `(app, incarnation)` from a cached map merely moves the
epoch-carrier problem. Emitting the marker into the WAL dissolves it. The design
does the second.**

### 8.1 Why stamping from a cache only moves the problem

A relay that reads the current incarnation from the control plane and stamps it
onto outgoing frames is comparing two things that are not ordered with respect to
each other: the WAL position of the change it is decoding, and the wall-clock
moment it read the map. Decoding lags production, so a relay that refreshes its
map at time T can stamp incarnation N+1 onto events produced under incarnation N.
The failure is silent and its window is exactly the decode lag, which is the
quantity this whole service is trying to make small and variable.

It is also homeless: decision 7 deleted `__zeroship_admin` entirely and there is
no `app_schema_state` to read (`AppIncarnationId` occurs **0** times in `crates/`
and `sdks/`, measured). Stamping from a cache means inventing the table decision
7 removed.

### 8.2 The mechanism that dissolves it

PostgreSQL has an in-band marker: `pg_logical_emit_message`. Transactional
messages are ordered in the WAL with the transaction that emitted them, and
pgoutput delivers them as an `M` frame. The driver already decodes them:
`PgOutputMessage::Message { xid, flags, lsn, prefix, content }` at
`libs/compio-postgres/src/replication.rs:2043-2050`, requested by
`StartReplicationOptions::messages` at `:860-863` ("Deliver
`pg_logical_emit_message` payloads as `PgOutputMessage::Message`. Off, the server
omits them and the decoder's `M` arm never runs.").

MEASURED, PostgreSQL 16.15, `proto_version=1`:

```sql
BEGIN;
SELECT pg_logical_emit_message(true, 'zs.incarnation', 'app_alpha:7');
ALTER TABLE t ADD COLUMN nickname text;
COMMIT;
INSERT INTO t VALUES (1,'123-45-6789','***-**-6789','nick');
```

with `'messages','true'`:

```
B ...
M^A\000\000\000\000^AQ\261\000zs.incarnation\000\000\000\000^Kapp_alpha:7
C ...
B ...
R\000\000@\000public\000t\000d\000^B^Aid...\000ssn_masked...
I\000\000@\000N\000^Bt\000\000\000^A1t\000\000\000^K***-**-6789
C ...
```

and with `messages` omitted (which is what `wal_consumer.rs:380-386` requests
today, since it sets only `slot_name`, `start_lsn`, `proto_version` and
`publication_names` and takes `..Default::default()` for the rest), the same peek
returns only the INSERT transaction. The message transaction is skipped entirely.

Three properties fall out of that transcript:

1. The marker is delivered **even though it belongs to no publication**. It needs
   no membership decision and cannot be forgotten by publication reconciliation.
2. It is ordered **before** the data that follows it, by the WAL, with no clock
   and no cache.
3. The `ALTER TABLE ... ADD COLUMN` in the same transaction did not widen the
   Relation message, confirming 3.3 holds across DDL.

### 8.3 Where it is emitted

Inside the **widen** transaction of 3.6's bracket - the descendant of today's
`reconcile_in_transaction`
(`crates/zeroship-migrate-server/src/publication.rs:79-110`), which
`reconcile_app_publication` already wraps in `BEGIN` / `COMMIT` (`:64` and `:69`,
with the `ROLLBACK` arm at `:74`) and which already runs on the privileged
migration connection after a successful apply (`:54-58`). Then "the publication
reached its new shape" and "the epoch advanced" are the same WAL event, and the
relay learns both from the same frame.

**Not the shrink transaction.** Shrink narrows the published set to
`old_wire INTERSECT new_wire`, a state no descriptor describes; a marker there
would announce an incarnation the frames that follow do not yet match. Widen is
the transaction that makes the new projection true, so it is the one that gets to
say so. Measured in 3.6: the marker rides the same `BEGIN`/`COMMIT` as the
`DROP TABLE` + `ADD TABLE` pair and is delivered before the first change under
the new column list.

### 8.4 Honest limits

The relay must request `messages: true`, which today's options struct defaults to
false. That is a one-field change but it is a change, and a relay that omits it
sees no markers and no error. The gate in section 10 must assert the option is
set, not merely that markers are handled.

And the marker is only as reliable as the widen transaction emitting it. That is
a *guarded* property, not an unrepresentable one, in the vocabulary
`docs/reviews/2026-08-28-flip-write-path.md:670-682` uses. It is one function in
one privileged service, which is the smallest surface available, but it is not
zero. The bracket narrows it a little further: a widen that does not run leaves
the publication measurably shrunk (7.6's `cdc_published_columns`), so a missing
marker has a second, independent observable rather than being pure silence.

The behaviour of a non-transactional message
(`pg_logical_emit_message(false, ...)`) relative to the enclosing transaction is
**not verified**, and this design does not use one.

---

## 9. The six inherited requirements, answered

From `...-00-index.md:66-90`.

| requirement | where |
| --- | --- |
| A wire projection that is a WHITELIST over declared fields, covering `changed_columns` | Section 3. Publication column lists computed from `storage.valueColumn` over declared fields unioned with the replica identity (3.1), folded server-side from the same policy-resolved ops as the DDL (3.7), one shared publication bracketing the DDL (3.6), plus the `ProjectedTuple` newtype for the SQLite arm (3.4) |
| A test fixture for the MASK-ONLY shape that fails on a plaintext parent | 10.2 |
| The schema-change signal | Section 8: `pg_logical_emit_message` in the widen transaction |
| Leader election and resume, without tokio | Section 6: one `pg_try_advisory_lock` per cluster over `compio-postgres`; resume from `confirmed_flush_lsn`; `(commit_lsn, change_index)` watermark reset only on a `systemid` or `timeline` change |
| `max_slot_wal_keep_size` must be set | 7.2, plus 7.4 - setting the GUC without fixing the fatal classifier converts a full disk into an infinite retry loop |
| Measure the added latency; do not estimate it | Section 11. No figure appears in this document |

---

## 10. Test strategy

### 10.1 What the existing test rules on

`broker.rs:1863-1914`, `cdc_event_carries_masked_value_for_masked_columns`. Its
fixture is three lines of `HashMap::insert` at `:1872-1875` with
`parent_ciphertext_text = "\\x0123456789abcdef0123456789abcdef"` at `:1868`, a
BYTEA hex text-encoding. Its first two assertions (`:1888-1897`) read back the
two values the test itself just inserted into the map: they are tautological with
respect to any production code. Its third (`:1898-1902`) asserts no value equals
a plaintext the test never put in. Only the last third (`:1906-1913`) touches
production code, and what it touches is `ws_frame_for_change`, which section 2.3
shows has no production caller and which this design deletes.

Its own comment says what it is for (`:1859-1860`): "A regression that wires
decrypt-on-CDC would land the plaintext in `new_tuple["ssn"]` and flip this
assertion." That is a real property, on the safe (ciphertext) shape, under a name
that claims the general one. It never constructs a mask-only field and it never
runs a producer.

**It is deleted, not extended.** Extending it would keep the name.

### 10.2 The mask-only fixture

The replacement is an integration test against a live PostgreSQL, and it must
fail on a plaintext parent. Shape:

1. Apply a real migration declaring a collection with a **mask-only** field
   (`t.string().mask(...)`, no `.encrypt()`), through the real migration path, so
   the DDL and the publication reconciliation both run. A declared schema built
   in the test does not satisfy this; the design's own acceptance criterion
   already says so (`...-design.md:1581-1584`: "the test creates the column
   through a real migration and reads a real WAL event - a hand-built
   `ChangeEvent` fixture does not satisfy it").
2. Assert the publication's column list.
   `SELECT attnames FROM pg_publication_tables WHERE ...` must contain the mask
   sibling and the replica identity, and must **not** contain the plaintext
   parent.
3. INSERT a row whose plaintext value is a distinctive sentinel.
4. Consume the relay's own output frames, not a hand-built event. Assert:
   - the sentinel does not appear in any frame, at the byte level, not by key
     lookup (`!frame_bytes.windows(n).any(|w| w == sentinel)`);
   - the parent column **name** does not appear in the `Relation` frame or in any
     `changed_columns`;
   - the mask sibling's value **does** appear, so the test cannot pass by
     delivering nothing.

That last arm is the control. Without it, a relay that drops every event passes
every other assertion.

**The mutation that must make it fail:** remove the column list from
`publication_membership_sql` so the publication is `FOR TABLE s.t` again. Arm 2
and arm 4a must both go red. If only arm 2 goes red, the frame assertion is
looking in the wrong place.

### 10.3 The rest of the suite

- **Replica-identity guard.** Build a projection that omits the replica identity
  and assert the resulting DDL is refused by the projection builder. Separately,
  assert against a live server that such a publication makes `UPDATE` fail with
  the message measured in 3.5(a), so the guard's reason is pinned to real server
  behaviour rather than to a belief about it.
- **`REPLICA IDENTITY FULL` incompatibility.** A test that sets FULL on a
  column-list-published table and asserts the write fails, referencing
  `wal_consumer.rs:551-560` so the next reader who acts on that comment finds the
  counter-evidence.
- **Two publications, one table.** Assert the relay classifies "cannot use
  different column lists" as fatal and names the table.
- **New column defaults out.** `ADD COLUMN`, insert, assert absent from the
  frames. This is 3.3 as a regression test, and it is the arm that fails if
  someone ever "fixes" the publication to use `FOR TABLES IN SCHEMA`.
- **Epoch marker.** A migration that changes a collection must produce an `Epoch`
  frame ordered before the next `Change` frame for that app. Include a negative
  arm asserting `StartReplicationOptions::messages` is `true`, because a relay
  that omits it sees no markers and no error (8.4).
- **Slot invalidation.** Set `max_slot_wal_keep_size` small, stop the relay,
  churn WAL, restart the relay, assert `wal_status = 'lost'` is observed and a
  `Resync` reaches every affected subscription. Assert the SQLSTATE-`55000`
  classification arm, since 7.4 shows substring matching misses it.
- **Leader election.** Two relay instances, one database. Assert exactly one
  holds the slot, that killing the leader's process releases the advisory lock
  without a timer, and that the survivor's `Hello` term is strictly greater.
  **And a negative arm: the survivor's `Hello` must NOT cause the worker to reset
  its watermark**, since the replayed frames are duplicates the dedup rule drops
  (6.4). Assert the subscriber sees zero re-delivered events across a clean
  failover.
- **Dedup under write concurrency.** Two connections, overlapping transactions,
  the shape measured in 6.3: A opens and holds, B inserts and commits, A commits.
  Assert both rows reach the subscriber. **This is the arm that a per-change-LSN
  watermark fails and a `(commit_lsn, change_index)` watermark passes**, and it
  is the only arm in this suite that a single-writer test cannot substitute for.
  Pair it with the control: the same two rows written sequentially, which passes
  under either rule.
- **Dedup under `heap_multi_insert`.** `COPY` three rows into a published table,
  assert three events. Control arm: the same three rows as one multi-`VALUES`
  `INSERT`, which produces three distinct LSNs (measured, 6.3) and therefore
  passes even under the broken rule - so the two arms together locate the defect
  rather than merely detecting it.
- **The confirmation clamp.** Drive a relay whose ring is artificially stalled,
  let the server send a `PrimaryKeepalive` with a `wal_end` beyond the ring's
  durable position, and assert the standby status update reports
  `highest_durable_lsn` and not `wal_end`. Then the invariant arm: on the
  `Commit` path the clamp must be a no-op, so assert the clamped and unclamped
  values are equal there. If that arm ever goes red, the durability ordering in
  6.3 has been broken somewhere else.
- **The publication bracket.** A migration that removes `.mask()` from a field.
  Assert it succeeds (without the bracket it aborts with `2BP01`), that the
  published column set is the intersection between shrink and widen, and that the
  epoch marker arrives in the widen transaction. **Mutation: move the reconcile
  back to after the DDL and assert the migration fails**, which is the only way
  to prove the bracket is what fixed it.
- **Two tenants, one publication.** Reconcile app A while app B's tables are
  members; assert B's `attnames` are byte-identical before and after. Mutation:
  restore `ALTER PUBLICATION ... SET TABLE` and assert B's tables disappear. This
  is the arm that would have caught the shared-object defect.
- **Slow consumer cannot reach the slot.** One worker connection that stops
  reading its body; a second app writing normally. Assert the second app's
  delivery is unaffected, that `confirmed_flush_lsn` keeps advancing, that the
  stalled app's `cdc_oldest_undelivered_age_seconds` grows, and that the stalled
  connection is closed after `slow_consumer_timeout`. The floor for this arm is
  the number of frames the healthy app delivered during the stall: an arm that
  delivers zero frames passes every other assertion trivially.
- **Gap and Truncate.** A value over the frame budget produces `Gap` for that row
  and leaves neighbouring rows' `Change` frames intact; `TRUNCATE` on a published
  table produces `Truncate` and not a stream of `Change` frames. Plus the
  `CellValue` arm: an unchanged-TOAST value arrives as `Unavailable` and not as
  `Null`.
- **Gate arms.** Per `AGENTS.md`, every arm of the gate script declares the
  number of items it ruled on and a floor. The floor that matters here is the
  number of published columns the projection test inspected: an arm that inspects
  zero columns prints exactly what a clean tree prints. The slow-consumer arm's
  floor is stated with it above, for the same reason.

---

## 11. How to measure the added latency

No figure appears in this document because none exists. The measurement to run,
specified so the number that comes back means something:

**Instrument.** Four timestamps per event, all from the same clock domain where
possible:

- `t0` = the commit timestamp pgoutput already carries. `PgOutputMessage::Commit`
  has `commit_timestamp` (`libs/compio-postgres/src/replication.rs:1888-1897`,
  "Server clock at commit"). This is the **server's** clock, so any figure
  derived from it and a relay-side clock includes clock skew and must say so.
- `t1` = relay receives the `XLogData` frame.
- `t2` = relay writes the frame into the app's ring (this is the point where it
  confirms the LSN, so it is also the retention boundary).
- `t2b` = a connection cursor reads the frame out of the ring. **`t2b - t2` is
  the queueing delay and must be reported separately from `t2 - t1`**, because
  they respond to different things: decode cost scales with row width and
  transaction size, queueing scales with subscriber health (5.4). A single
  relay-side number folds a slow consumer into what looks like a slow decoder.
- `t3` = worker's `broker::publish` returns.

**Report `t3 - t1` as the service's own contribution**, which is skew-free
because both ends are the relay-plus-worker fleet, and report `t1 - t0`
separately and labelled as containing skew. Reporting a single `t3 - t0` number
buries the one quantity the service controls inside one it does not.

**Baseline.** The same measurement against today's in-worker consumer, which has
the same `t0` and `t1` and whose `t3` is `broker::publish` in the same process.
Without that arm the number is unanchored: the question is not "how long does the
relay take" but "how much longer than today".

**Load shape.** At minimum: one app with one subscriber (latency floor); one app
with a write burst larger than the ring (the Resync boundary); N apps writing
concurrently where only one has subscribers, which is the case the relay is
supposed to be good at and the per-app-slot design was bad at; and **one app with
a deliberately stalled subscriber alongside N healthy ones**, reporting the
healthy apps' distribution, which is the only run that can show whether 5.4's
invariant holds under load rather than in a unit test.

**Two measurements that are inputs, not results.** Section 5.5's knobs are
derived rather than chosen, so derive them:

- **Worker restart time**, wall clock from process exit to first frame consumed
  after reconnect. `ring_retention` must exceed it comfortably, or an ordinary
  deploy becomes a fleet-wide `Resync`.
- **Per-app frame rate and mean frame size** under the app's own write load.
  `ring_bytes_per_app` divided by that product is the disconnect tolerance the
  ring actually buys, and it is the number to state when someone asks why the
  ring is the size it is.

Neither is a latency figure and neither is in this document.

**Distribution, not a mean.** p50, p99, p99.9, and the maximum. A relay that is
fast on average and stalls for 200ms on the librdkafka pattern in 5.1(c) has a
fine mean.

**What would invalidate the result.** A run where the relay and the workers share
a machine with the database, since io_uring completion queues and the WAL writer
then contend; and a run where no subscriber exists, since `has_subscribers`
(`wal_consumer.rs:618-620`, `broker.rs:550-558`) short-circuits before the
expensive work and would measure the short-circuit.

---

## 12. Accepted costs, and the arguments against this design

Everything in this section is a cost taken deliberately. None of it is a defect
list, and none of it should be "fixed" without re-opening the decision it belongs
to.

### 12.1 The relay is a single point of failure for every live query on the cluster

Today a worker's CDC failure affects that worker's apps. Under this design one
process being wedged stops every live query on the cluster, and the leader lock
means a second instance is a standby, not a second consumer. The failover time is
bounded below by how fast PostgreSQL notices the dead session and releases the
advisory lock, which is a TCP-keepalive-shaped quantity, **not measured**, and
not obviously fast.

The mitigation is honest but partial: `pg_terminate_backend` on the stale session
is available to an operator, and a standby that repeatedly fails to acquire can
escalate. Neither is automatic.

### 12.2 It is the new bottleneck, and it is a single decode plus a single fan-out

One decode per database is the floor the index instructs us to accept. But the
relay also does the fan-out, and fan-out is `O(apps x subscribing workers)` in
one process. The per-app-slot design was `O(apps x workers)` in *decode*, which
is worse, but it was spread across processes. A relay that saturates one core on
fan-out has no second core to move to without breaking the per-app ordering
guarantee in 6.3. The ring-per-app structure admits sharding by app across
threads later; nothing in this document does it, and doing it re-opens ordering.

5.4 improves the constant without changing the shape: the ring writer is O(1) per
frame and the per-connection copies happen on the connection tasks, so the
serialised part is one write per frame per app rather than one per subscriber.
The `O(apps x subscribing workers)` total work is unchanged; only its placement
moved off the critical path. That is worth having and it is not a fix.

### 12.3 The column list makes the migration service load-bearing for creator writes

Section 3.5(a): a projection that omits the replica identity is accepted as DDL
and breaks every write to that table. The migration service currently cannot
break a creator's writes by getting a publication wrong; after this change it
can. That is a real transfer of blast radius from a read path to a write path,
and it is the single strongest argument against the column-list approach.

The alternative that avoids it is to keep the publication wide and filter in the
relay from a descriptor the relay fetches. That trades an availability risk for a
confidentiality risk, plus a staleness window on every deploy, plus a new
coupling from the relay to the deploy pipeline. That is the wrong trade for this
platform, but it is a trade and not a free win.

### 12.4 `REPLICA IDENTITY FULL` is now permanently unavailable

3.5(b). The tree's own comment recommends it as the fix for a real defect
(deletes and non-key-column filters). This design forecloses it and replaces it
with a subscriber-side pk-membership set (5.3) that is not prototyped. If that
replacement turns out to need unbounded state, the design has traded a working
fix for an idea.

### 12.5 The wire is one more format to keep in sync

`zeroship-cdc-wire` is a fourth serialisation boundary in this subsystem, after
pgoutput, the broker's JSON, and the SDK's TypeScript types. Pre-launch that is
cheap; it is still four places a column-name change has to land, and 5.6's three
extra frames plus the three-way cell encoding widen each of them.

### 12.6 The dedup key depends on the relay reproducing its own numbering

`change_index` is the relay's count, not the server's. Correctness rests on
pgoutput replaying a transaction's changes in the same order every time it
replays them - argued from the reorder buffer's structure in 6.3, **not
measured**, and recorded as a gap in section 13. If that ever fails, the failure
mode is a silently dropped row under replay, which is the same class of defect
the key was introduced to fix, arriving from a different direction.

There is a strictly safer alternative not taken: dedup at whole-transaction
granularity, dropping every frame of a transaction whose `commit_lsn` is at or
below the watermark, and advancing only at a transaction-boundary frame. That
needs no index and no determinism assumption. It costs a boundary frame in the
wire format and it re-delivers a whole transaction when the relay dies
mid-transaction, which the live-query path absorbs and the raw path does not. The
index is chosen because it is exact; the argument for the boundary frame is that
it is exact *without a premise*.

### 12.7 The bracket makes a migration's publication work three times bigger

3.6 turns one post-apply reconcile into two transactions around the DDL, each
doing a catalog read and a `DROP TABLE` + `ADD TABLE` per member table, under a
now-cluster-wide advisory lock. Every tenant's migration serialises against every
other tenant's on that lock, where before they only serialised per app. It also
introduces a state - shrunk, not yet widened - that did not exist before and that
an operator can now find in production. Both are real costs, and the alternative
is a migration that aborts whenever a creator removes `.mask()` from a field,
which is worse.

### 12.8 The relay's failure policy is three interacting knobs and a lint

5.4, 5.5 and 7.5 introduce `slow_consumer_timeout`, four ring dimensions,
`resync_storm_threshold`, `resync_storm_window`, and a deny-level lint set. "No
knobs" deploys correctly by construction and "seven knobs" has a wrong setting
for every one of them. An operator who sets `ring_bytes_per_app` too low converts
normal traffic into a `Resync` storm, which 5.4 converts into database read load,
which is charged to the tenant. The mitigation is section 11's derivation, and a
derivation nobody runs is a default nobody chose.

### 12.9 Three more frames the raw subscriber can ignore

7.3 already says `db.subscribe`'s `Resync` is a documented obligation a creator
can silently get wrong. 5.6 adds `Gap` and `Truncate` and turns cell values
three-way, so there are three ways to diverge instead of one, all on the same
path, all still absorbed for free by live queries and by nothing else. 5.6
improves the *relay's* vocabulary and makes the raw subscriber contract's
existing hole wider. Closing it is still the separate decision 7.3 declines to
make, and it is now more overdue.

### 12.10 Where this design over-specifies

Three places, two of them defensible.

- **The non-`async` `push` (5.4).** A real seam that holds, because the compiler
  enforces it at every call site. Keep.
- **The closed `Gap.reason` enum (5.6).** Defensible on the same grounds as the
  rest of section 3: an open string is where a physical column name reappears.
  Keep, and accept that a fourth reason means a wire-format change.
- **The deny-level lints in 7.5.** Weakest of the three. A lint is a rule with an
  `#[allow]` escape, and the first genuinely awkward indexing site will get one.
  It is a nudge dressed as an invariant. The property that actually holds is the
  one below it - that a panic kills the process rather than poisoning a tenant -
  and the lints only reduce how often that happens.

### 12.11 What would make a different choice right

- **If `zeroship-stream` grew a topic argument on `poll`, a batch publish, and
  TLS**, the case for a bespoke push channel weakens considerably, and the
  durable-spool version of section 7 becomes available at low cost. That is three
  changes to one crate, all of which its own comments already contemplate.
- **If the platform ever splits into per-app databases**, the whole design
  inverts: one leader per cluster becomes one leader per database, the advisory
  lock becomes per-database, and the decode multiplier argument changes shape
  entirely. Section 6.1 verifies today's answer; it is not a permanent one.
- **If live queries turn out to be a niche feature** rather than a headline one,
  a per-worker slot with `max_replication_slots` raised (option D) is a smaller
  system, and the credential problem could be solved by giving the *worker* a
  second, `REPLICATION`-only login role used by a dedicated thread. That is not a
  boundary, per the AGENTS.md privilege invariant, and this design argues against
  it; but it is the cheap option and someone will propose it.

---

## 13. What is not verified

Stated as gaps rather than written as facts elsewhere in this document.

- **The `BYPASSRLS` consumers are now enumerated, and the answer is a privilege
  this service lets the worker drop.** Production RLS has exactly one consumer:
  the gateway, over four tenant tables - `app_secrets`, `gateway_sessions`,
  `app_session_anchors`, `app_user_identities` - and its own module states it
  "connects as the non-bypass `zeroship_gateway` role, so every RLS-table
  statement it issues is filtered by the policy"
  (`crates/zeroship-gateway/src/rls.rs:1-20`). No table in `db/migrations-ts/`
  enables RLS, and no app-schema table has a policy.
  The worker's attribute has one stated consumer, and it is the thing that
  moves: **logical decoding.** `crates/zeroship-worker/src/db_posture.rs:96-101`
  does not merely document this - it **refuses boot** unless the role holds both
  `REPLICATION` and `BYPASSRLS`, with the message "worker database role requires
  only REPLICATION and BYPASSRLS for logical decoding".
  **So that assertion must invert when the relay lands.** Section 4 deletes
  `change_stream_pg.rs` and moves `wal_consumer.rs` and `replication.rs` out of
  the worker; after that the worker decodes nothing, and a worker still holding
  `REPLICATION` and `BYPASSRLS` is carrying two privileges with no consumer. The
  posture check should then require their **absence**. Whether `BYPASSRLS` was
  ever needed for decoding at all is a separate question worth asking at that
  point: the walsender plans no queries, and RLS is a planner-level filter.
- **The advisory-lock release latency after a hard leader kill.** 12.1. It is
  TCP-keepalive-shaped and no experiment was run.
- **The pk-membership replacement for `old_tuple`.** 5.3. Specified, not
  prototyped, and it changes a subscriber-side contract.
- **Behaviour of non-transactional `pg_logical_emit_message`.** 8.4. Not used and
  not tested.
- **Whether `ntex` v3's response streaming and `cyper`'s `stream` feature compose
  into a long-lived push channel in practice.** Both are present in the workspace
  (`Cargo.toml:45`, `:48`) and `cyper`'s `stream` feature is enabled; no code was
  written against them.
- **The relay's own memory profile under a large `logical_decoding_work_mem`
  transaction.** Measured `boot_val` is 64MB per slot (7.2), but a spilling
  transaction was not driven through the ring.
- **Anything about MySQL or the SQLite actor beyond the CDC publisher's shape**
  at `backend/sqlite/cdc.rs:604-639`.
- **Replay-order stability is now measured, within a stated bound.** On
  PostgreSQL 17.11, one transaction containing a three-row multi-`VALUES`
  `INSERT`, an `UPDATE` and a `DELETE` was re-decoded three times through
  `pg_logical_slot_peek_binary_changes` on a `pgoutput` slot. All three replays
  were **byte-identical** (same md5 over `lsn` plus message bytes), and the
  message sequence was `Begin, Relation, Insert, Insert, Insert, Update, Delete,
  Commit` - SQL order. `peek` re-decodes from `restart_lsn` rather than serving a
  cache, so these are genuine replays.
  **The bound:** one slot, one session, one small transaction, one major version.
  It does not cover a relay restart decoding through a *different* session, a
  transaction large enough to spill under `logical_decoding_work_mem`, or a
  concurrent writer interleaving into the same reorder buffer. 12.6 still names
  the alternative that needs no ordering assumption.
  **Incidental confirmation:** the three-row multi-`VALUES` insert produced
  **three separate `Insert` messages**, not a collapsed one - consistent with
  6.3's claim that `env.db.insertMany`'s multi-`VALUES` shape does not reach the
  `heap_multi_insert` collapse.
- **LSN reuse across a divergence point.** The timeline change itself is now
  measured - a basebackup restored with `recovery.signal` and promoted moves the
  timeline `1 -> 2` on an unchanged `system_identifier`, and crash recovery moves
  neither (6.4). What remains unobserved is a *consumer* resuming across that
  divergence and encountering reused LSNs; the gate is specified and its input is
  proved to move, but the failure it prevents has not been reproduced.
- **Whether `heap_multi_insert` is reachable from any path this platform runs
  today.** The collapse is measured (6.3); its reachability is not.
  `env.db.insertMany` emits multi-`VALUES` (`query.rs:4013-4157`), which does not
  collapse, and the other writers - the migration engine, any future backfill,
  `CREATE TABLE AS` inside a creator migration - were not enumerated.
- **Whether `ALTER PUBLICATION ... DROP TABLE` + `ADD TABLE` in one transaction
  is invisible to a CONCURRENTLY DECODING consumer**, as opposed to invisible to
  a later catalog read. 3.6 measures the catalog outcome and the mid-stream
  `ADD TABLE` pickup separately; it does not measure a decoder running through
  the swap. The reasoning is that `ALTER PUBLICATION` is transactional and
  pgoutput reads the catalog at the decoding snapshot, so the pair is atomic to
  it - which is an argument, not a transcript.
- **Everything in 5.4, 5.5, 7.5 and 7.6 is specification, not observation.** No
  ring exists to overrun, no consumer exists to stall, no metric exists to read.
  Those four subsections state policy the implementation must satisfy, and the
  tests in 10.3 are how it gets checked.
- **The Vitess, Supabase, Salesforce, Kinesis, DynamoDB, Neon, Debezium and
  Materialize claims are relayed from the prior-art study and unverified.** They
  are cited as prior art shaping a decision, never as evidence about this code.
