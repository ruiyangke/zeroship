# The CDC service

Written 2026-08-28. The operator decided on 2026-08-28 that this service is
built (`docs/proposals/2026-08-26-runtime-db-binding-00-index.md:51-56`). This
document specifies it. It does not re-argue whether to build it.

**Working name:** `zeroship-cdc`. One binary, one process, executes no creator
code.

---

## DO NOT IMPLEMENT FROM THIS DOCUMENT AS WRITTEN

Reviewed three ways after it was written - an adversarial review, a second
opinion, and an eight-system prior-art study. **Ten findings; none
disqualifying, but three are correctness bugs and two change the shape.** They
are recorded in full in
`2026-08-26-runtime-db-binding-00-index.md`, section "L12: DECIDED". Building
the text below without applying them means building it twice.

### Three correctness bugs

1. **Section 6.3's dedup rule drops every row of a transaction after the
   first.** Every change in one pgoutput transaction carries the same
   `commit_lsn`, and the frame table in 5.2 has no transaction boundary, so
   "keep the highest and drop anything at or below" delivers row 1 and silently
   discards the rest. **Measured on PG 18.4, and the fix is one field:**
   per-change LSNs are distinct (`0/BAA70A58`, `0/BAA70B38`, `0/BAA70BB8` for
   three rows under commit `0/BAA70C68`), so stamp each `Change` frame with its
   OWN lsn. No sequence number, no boundary frame.
2. **The keepalive handler becomes a durability bug on the way in.**
   `wal_consumer.rs:441` does `advance_lsn(wal_end)`, which is correct today
   and prevents idle-slot WAL growth - but `wal_end` is the SERVER's position,
   which can exceed anything the relay has made durable. Under this document's
   own promise (confirm once frames are durable in the ring) that confirms WAL
   the relay cannot replay. Must be `min(wal_end, highest_durable_lsn)`; on an
   idle database those are equal, so the idle-slot protection survives.
3. **"Reset the watermark when the Hello term increases" (6.3) is backwards.** A
   new leader replaying from `confirmed_flush_lsn` produces only duplicates,
   which the monotone rule already drops - so the reset turns every routine
   leader change into a duplicate storm, while still missing the one case that
   genuinely invalidates a watermark: a **PostgreSQL timeline change**, which
   this document never reads or validates.

### Two shape changes

4. **Reconciliation must BRACKET the migration DDL, not follow it.** A column
   named in a publication column list becomes a catalog dependency, so
   `DROP COLUMN` of a listed column fails with `2BP01` (reproduced), and
   `DROP ... CASCADE` silently removes the whole TABLE from the publication.
   Since `reconcile_app_publication` runs after `apply_sealed`, any migration
   dropping a masked column aborts. Split into shrink-before / widen-after,
   which also fixes the marker landing one transaction late.
5. **Publications collapse to ONE.** `publication_names` is fixed at
   `START_REPLICATION`, so per-app publications would force a stream restart for
   every tenant on every app creation. Measured on 18.4: one publication picks
   up a table added mid-stream, live, with its column list applied and the
   existing stream undisturbed. **But that makes the publication a SHARED
   object**, and `publication_membership_sql` emits `ALTER PUBLICATION ... SET
   TABLE` - a full replace computed from one app's view. Two tenants migrating
   concurrently would silently remove each other's tables from CDC. Use
   `ADD TABLE` / `DROP TABLE`, or lock.

### Five omissions the prior art predicts will bite

None of these appear anywhere in this document, and every studied system had to
solve them: **slow-consumer policy** (a slow Vitess client once hung a replica
promotion), **per-app fault isolation in a shared process** (Supabase had three
project-wide outages from one un-trapped subscriber loop), **what the relay
measures** - made worse by the retention inversion, since `confirmed_flush_lsn`
advances fastest when delivery is dead - **ring sizing** (named seven times,
dimensioned nowhere), and **a lossy-degradation vocabulary** for a change the
relay cannot represent.

Two smaller corrections: section 4 inverts the SQLite/Postgres polarity
(`emit_for_rows` returns early on SQLite, so those three functions serve
Postgres and are deleted with it), and the migration service does not receive
the descriptor this document tells it to project from - it receives the IR the
descriptor is folded from, so the projection must come from that same fold.

---

## 0. How to read the citations in this document

Every `file:line` below was opened in this session, in this worktree
(`/home/ruiyang/Projects/appbase/.worktrees/dbbind-impl`), on the working tree
as it stands. Every PostgreSQL behaviour claim marked **MEASURED** was produced
by a throwaway `postgres:16` container (`postgres:16` resolving to PostgreSQL
16.15, `-c wal_level=logical`) started and destroyed inside this session;
transcripts are inline. Anything I did not run or open is marked
**unverified** and says so in its own sentence rather than being written as
fact.

Two things this document deliberately does not carry: any latency figure (see
section 11, which specifies how to obtain one) and any re-derivation of the
decode multiplier, which the index records as structural and already verified in
the PostgreSQL sources.

---

## 1. Decision summary

| question | answer | section |
| --- | --- | --- |
| Where is the wire projection applied? | In the **publication column list**, as DDL, by `zeroship-migrated`. PostgreSQL never puts the excluded bytes or the excluded names on the wire | 3 |
| What covers `changed_columns`? | The same mechanism. A column absent from the publication list is absent from the pgoutput `Relation` message, which is the only thing `changed_columns` is built from | 3.2 |
| Which crates move? | `wal_consumer.rs` and `replication.rs` move; `slot_reaper.rs`, `replication_ops.rs`, `change_stream_pg.rs` and the Postgres half of the local-emit path are **deleted**; `broker.rs`, `read_set.rs`, `cdc_lifecycle.rs` stay | 4 |
| Is `zeroship-stream` the carrier? | **No**, and the reasons are in its own source, not in taste | 5.1 |
| What is the carrier? | A relay-owned bounded per-app ring plus a long-lived push response over `ntex` (server) and `cyper` (client), authenticated by a service assertion | 5.2 |
| Leader election | One `pg_try_advisory_lock` per cluster, session-scoped, over `compio-postgres`. One database is shared by every app, so this is one leader, not one per app | 6 |
| Failure behaviour | `max_slot_wal_keep_size` bounds WAL; slot invalidation becomes `Resync`; the subscriber contract absorbs `Resync` on the live-query path and forces the creator to handle it on the raw path | 7 |
| Schema-change signal | `pg_logical_emit_message` inside the publication-reconciliation transaction. This **dissolves** the carrier problem rather than moving it, because the marker is ordered by the WAL itself | 8 |

---

## 2. What the service closes, restated against the code

Three findings close because consumption moves out of the worker process. All
three are re-verified here rather than relayed.

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
own login role holds `REPLICATION`, and `zeroship_worker` drops
`REPLICATION` and `BYPASSRLS` in the same migration that provisions the relay's
role.

`BYPASSRLS` is a separate grant from `REPLICATION` and is not required to
consume a slot. It is in the same statement and drops with it. I did **not**
verify what else in the tree depends on `zeroship_worker` holding `BYPASSRLS`;
that is a required pre-flight check before writing the migration, not a claim.

### 2.2 L12b: an abandoned slot

Slot names are per `(app, worker)`:
`crates/zeroship-plugin-db/src/replication.rs:114-121` composes
`__zs_slot_<sha14(app)>__<sha10(worker)>`. `crates/zeroship-plugin-db/src/slot_reaper.rs`
exists (593 lines) to sweep the ones nobody owns any more, on a one-hour
inactivity threshold (`:28`) with a two-lock worker lease (`:116-125`,
`:182-198`) and a fleet-leader election (`:171-180`). With O(1) service-owned
slots there is no per-worker slot to abandon. The file is deleted, not ported.

### 2.3 The mask-only leak, stated precisely

The index and the defect register both describe this. The precise shape, which
matters for what the fix has to cover, is that **the name leaks to creators
today and the value does not, but the value is in the process and one call site
from the wire.**

- `crates/zeroship-plugin-db/src/wal_consumer.rs` contains **zero** occurrences
  of `mask`, `wrap_row_on_read` or `apply_mask`. Measured:
  `grep -c -i "mask\|wrap_row_on_read\|apply_mask"` returns `0`. `tuple_to_map`
  (`:662-681`) zips every physical column the server sent into
  `new_tuple`.
- `changed_columns` on the WAL path is every column of the cached relation
  (`wal_consumer.rs:629-633`, `rel.columns.iter().map(|c| c.name.clone())`).
- `changed_columns` on the local-emit path is every key of the `RETURNING *`
  row minus exactly two literals (`crates/zeroship-plugin-db/src/exec.rs:518-524`,
  `.filter(|k| !matches!(k.as_str(), "created_at" | "updated_at"))`). That is a
  blacklist of two names.
- The tuple on the local-emit path is every key of the same row
  (`exec.rs:531-541`).
- **`emit_for_rows` runs before the mask pass.** `exec_mutation_with_emit` calls
  `emit_for_rows(&rows, ...)` at `exec.rs:438`; the caller passes the same rows
  to `read_pipeline::apply` afterwards at
  `crates/zeroship-plugin-db/src/crud/mod.rs:799`. So the broker event is built
  from the raw row, not the masked one. This ordering is the mechanism, and it
  is a stronger statement than "`exec.rs:531-541` maps every key".
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
- **`ws_frame_for_change` has no production caller.** Grepped
  `ws_frame_for_change`, `ws_frame_for_control` and `ws_frame(` across the repo
  excluding `target/`, `node_modules/` and `.git/`: the only hits in
  `crates/` and `sdks/` are the definitions at `broker.rs:986`, `:1004`, `:1024`,
  the internal dispatch at `:1026-1027`, and tests at `:1479`, `:1491-1492`,
  `:1524`, `:1531`, `:1906`. `docs/reviews/2026-08-28-flip-write-path.md:355`
  reached the same conclusion independently and specifies deleting it.

So the live creator-visible leak today is **the column name**, and the value
leak is a loaded gun rather than a discharged one. Both must be closed, and the
one mechanism in section 3 closes both.

---

## 3. The wire projection

### 3.1 The mechanism

**The projection is a PostgreSQL publication column list, computed from the
declared field set by `zeroship-migrated`, applied as DDL, and enforced by the
server.**

`zeroship-migrated` already owns publication membership and already reconciles
it to an explicit table list after every apply:
`crates/zeroship-migrated/src/publication.rs:30-51` builds
`CREATE PUBLICATION ... FOR TABLE s.t, s.u` / `ALTER PUBLICATION ... SET TABLE ...`,
and a test at `:126-155` asserts the DDL names each table explicitly and does
**not** use `FOR TABLES IN SCHEMA` (the assertion is `:138`). The change is to extend
`publication_membership_sql` from `schema.table` to `schema.table (col, col, ...)`.

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
`storage.valueColumn` and only falls back to suffixing when the block is
absent). For a mask-only field today `valueColumn` is the `_masked` sibling; the
plaintext parent is not in the set. Post-flip the parent holds the mask and
`storage.rawColumn` holds plaintext, and `rawColumn` is still not in the set,
because the set is built from `valueColumn` and nothing else.

The set is computed by the migration service, which has just folded the
migration and therefore holds the declared field set with its mask and
encryption flags. It is not derived from a string suffix at any point.

### 3.2 MEASURED: the column list removes the name as well as the value

This is the load-bearing claim, and it is the reason one mechanism satisfies
both halves of the inherited requirement. Transcript, PostgreSQL 16.15,
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
**`ssn` is not in the Relation message.** Read the `I` (Insert) frame: two
tuple values, `1` and `***-**-6789`. The string `123-45-6789` does not occur
anywhere in the stream.

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

This is the property that the current `_masked` stripper does not have and
cannot have. `format!("{col}_masked")` is a blacklist keyed on a derived name:
it knows exactly one sibling name (`crud/mask_pass.rs:469`, and its write-side
twin at `:150`, both cited in the index and not re-opened here), so anything
that is not spelled that way survives. A publication column list is a whitelist
over the columns the migration service decided to publish, and everything not on
it is absent by default, including columns that do not exist yet.

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
`docs/reviews/2026-08-28-flip-write-path.md:368` makes with `RawRows` on the
read path, and it is worth stating plainly that on the relay arm the constructor
is close to a no-op.

### 3.5 What the column list costs, and it is not free

Three measured consequences. All three are real hazards and the design has to
carry them explicitly.

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
DELETE carries only the replica-identity columns. Section 5.3 says how
"row left the view" is answered without a full before-image.

**(c) Two publications with different column lists on one table break decode.**
MEASURED, and the failure arrives at decode time rather than DDL time:

```
ERROR:  cannot use different column lists for table "public.t" in different publications
CONTEXT:  slot "s3", output plugin "pgoutput", in the change callback, associated LSN 0/156AB40
```

Publications are per-app and app tables live in the app's own schema
(`zeroship-schema/src/query.rs:1026-1028`, `CREATE SCHEMA IF NOT EXISTS <app>`),
so each table belongs to exactly one publication and this cannot arise from the
intended shape. It can arise from an operator or a test creating a second
publication over a creator table, so the relay must treat this SQLSTATE as fatal
and name the table rather than reconnecting into it.

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
| `exec.rs` emit path | **The Postgres arm is deleted.** `emit_for_rows` (`:481-550`), `queue_or_emit` (`:556-586`) and `drain_pending_emits_on_commit` (`:600-613`) stay only for the SQLite dev tier, which already short-circuits the shared path at `:494-500` via `backend_publishes_committed_changes` (`:442-444`). |

**Why deleting the Postgres local-emit path is not optional.** It is a second
producer of the same event with a *different* projection: a two-name blacklist
(`exec.rs:522`) against the publication whitelist, running before the mask pass
(2.3) against a stream that has already been projected by the server. Two
producers with two projections is the exact shape that produced the current
divergence between the read path and the CDC path. One producer per backend.

**What `zeroship-plugin-db` keeps overall:** the whole data plane (CRUD, read
pipeline, encryption, masking, transactions, V8 classes), the in-process broker
and its read-set narrowing, the CDC *lease* bookkeeping, and the SQLite CDC
publisher. What it loses is every line that speaks the streaming replication
protocol, and with it the reason its process needs `REPLICATION`.

`ChangeStream` (`crates/zeroship-plugin-db/src/backend/mod.rs:946-989`) survives
as a capability trait with a changed Postgres implementation: `spawn_consumer`
becomes "subscribe to the relay", `deprovision` becomes "tell the relay to
forget this app". (Aside, because the index repeats it: `backend/mod.rs` declares
**16** `pub trait`s, not five. The module doc at `:38-49` says "five" and then
lists four. `ChangeStream` is at `:946`.)

---

## 5. The worker/relay wire contract

`ChangeEvent` (`broker.rs:81-119`) crosses a process boundary now. Today it is a
Rust struct passed by `Arc` inside one process (`broker.rs:756-759` wraps it
once and clones the `Arc` per subscriber), so it has never needed an encoding.

### 5.1 `zeroship-stream` is not the carrier, and the reasons are in its source

The register's C2 argument cites `StreamTransport`'s `partition_key` ordering
guarantee (`crates/zeroship-stream/src/transport.rs:32-37`) and the existence of
memory and Redpanda adapters. Both are true. The crate is still the wrong
carrier for CDC fan-out, for four reasons, each read out of the code rather than
inferred.

**(a) A consumer group partitions; live-query fan-out broadcasts.** The trait is
explicitly a Kafka-family consumer-group contract: "consumer groups assign each
partition to one consumer in steady state" (`transport.rs:35`), and the
Redpanda adapter subscribes with a `group.id` (`adapters/redpanda.rs:244`,
`:253-256`). If every worker joins one group, each event is delivered to exactly
one worker. If each worker gets its own group, the broker holds one consumer
group per worker and each worker consumes every app's events. The second is the
tenant-isolation regression that option B was rejected for
(`...-defect-register.md:1107-1113`).

**(b) `poll` takes no topic.** `async fn poll(&self, max: usize)`
(`transport.rs:48`). The topic is fixed at construction
(`redpanda.rs:151-166` `RedpandaConfig::topic`, `:255` `subscribe(&[topic])`).
A per-app topic therefore means a per-app `RedpandaTransport`, which means a per-app
librdkafka producer, consumer, and C thread set. `Cargo.toml:201-204` describes
the adapter as running "over librdkafka's own C threads".

**(c) `publish` is a blocking spin with one broker round trip per record.**
`redpanda.rs:297-325`: a `sync_channel(1)` per record, then
`loop { self.producer.poll(Duration::from_millis(10)); rx.try_recv() ... }`
inside an `async fn`, with `acks=all` and `enable.idempotence=true`
(`:231-233`). There is no batch API on the trait. That is a synchronous stall on
a compio thread, per row, for a carrier that would sit on the write-notification
path. It is fine for the billing outbox it was built for; it is not a CDC
carrier.

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

**What `zeroship-stream` remains right for:** the durable usage/billing outbox
it exists for. Nothing here proposes changing it. If a later revision wants the
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
| `Hello` | wire version, relay id, leader term |
| `Relation` | `(app_id, incarnation, relation_generation, collection, columns: Vec<(name, is_replica_identity)>)` |
| `Change` | `(app_id, relation_generation, op, commit_lsn, pk, values: Vec<Option<Bytes>>)` positional against the last `Relation` |
| `Epoch` | `(app_id, incarnation)` - see section 8 |
| `Resync` | `(app_id, reason)` |
| `Heartbeat` | `last_confirmed_lsn` |

`Change` carries values positionally against a `relation_generation`, so a row
costs its values plus a small header rather than its values plus its column
names. `commit_lsn` is on every change frame because the worker dedups on it
(section 6.3).

Not `serde_json`. A JSON `HashMap<String,String>` per row is precisely what
`new_tuple` is today, and that shape is what let a bag of physical column names
become the thing on the wire.

**Transport.** The worker opens a long-lived HTTP response and reads frames off
the body until it closes. The pieces already exist and are already compio:
`ntex = { version = "3", features = ["compio"] }` (`Cargo.toml:45`) is the HTTP
server the peer service `zeroship-migrated` already uses
(`crates/zeroship-migrated/src/api.rs:3-5`); `cyper = { version = "0.8",
default-features = false, features = ["rustls", "json", "stream"] }`
(`Cargo.toml:48`) is the client, its `stream` feature is on, and
`crates/zeroship-worker/Cargo.toml:39` already names it. Authentication is a
service assertion (`crates/zeroship-core/src/service_assertion.rs`), which is
the tree's existing service-to-service identity mechanism.

Push rather than poll, because the point of the service is to reduce end-to-end
latency relative to WAL-to-worker, and a poll interval is a latency floor chosen
in advance.

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
into the subscriber's current view. An event whose pk is in that set is
delivered regardless of whether the new tuple matches the predicate, so the
subscriber learns the row left. The set is bounded by the query's page size;
overflow emits `Resync`, which the broker already models
(`broker.rs:318-340`) and the live-query client already absorbs (7.3).

I have **not** prototyped this, and it is the one part of the design where the
subscriber-side contract changes shape rather than moving. It is called out
again in section 12.

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
election with namespace and key constants at `:37-38`, and
`:149-169` are the take and release helpers. PostgreSQL releases a session
advisory lock when the session ends, so a crashed leader releases without a
lease timer, a heartbeat, or a clock.

Non-leaders retry on an interval and serve nothing. There is exactly one holder
of the slot at a time, which is also what PostgreSQL enforces independently: a
logical slot admits one active consumer (`replication.rs:107-113` states this as
the reason per-worker slots existed).

A `leader term` is minted from a monotonic counter in the platform schema on
each acquisition, and it rides in the `Hello` frame. A worker that sees a term
lower than the one it has already seen refuses the connection. This is the guard
against a partitioned old leader that still holds a TCP connection to a worker;
without it, two relays could deliver interleaved streams and the LSN dedup in
6.3 would silently drop the newer one.

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

**Confirmation.** `wal_consumer.rs:449-497` advances on `Commit` and sends a
standby status update; the long comment at `:459-494` records why a
mid-transaction `wal_end` is safe to report and includes a measurement of
`restart_lsn` staying pinned under an open transaction. That reasoning carries
over and should move with the file, because it is the only place it is written
down.

The relay confirms the commit LSN once the transaction's frames are durable **in
the relay's own ring** (section 7), not once a worker acknowledges them. That is
what converts WAL retention into relay-owned retention.

**At-least-once and dedup.** Every `Change` frame carries `commit_lsn`. The
worker keeps the highest `commit_lsn` it has applied per app and drops anything
at or below it. LSN is monotone within a slot's stream, so this is a comparison
and not a set. On a leader change the worker resets the watermark only when the
`Hello` term increases, because a new leader legitimately replays from the last
confirmed LSN.

**Ordering.** pgoutput delivers changes in commit order within one slot, so
per-app order is preserved by construction as long as the relay does not
reorder. The relay must therefore not fan out across threads per app in a way
that can reorder; one ring per app with a single writer is the constraint, and
it is the reason the ring is per app rather than global.

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

So with the GUC set, a down relay costs bounded disk and a lost slot instead of
a full disk. The relay's restart path reads `wal_status` (already modelled:
`SlotHealth.wal_status` at `replication.rs:331-335`, documented values
`reserved` / `extended` / `unreserved` / `lost`), and on `lost` it drops the
slot, recreates it, and emits `Resync` for every app.

Note that `restart_lsn` becomes NULL on invalidation, which makes
`watchdog_query`'s `lag_bytes` CASE (`replication.rs:370-372`) return NULL. The
`wal_status` column is the only surviving signal, which is why it exists and why
the relay must not key its health check on lag alone.

**The value to set is an operator decision, not a constant in this document.**
It has to be chosen against the cluster's WAL volume and the relay's worst
acceptable downtime. What this document fixes is that it must not be `-1`.

### 7.3 Does the subscriber contract genuinely absorb `Resync`?

Checked, and the answer differs between the two subscriber surfaces.

**Live queries: yes, genuinely.** `sdks/db/src/live.ts:333-338`:

```
// Both `change` and `resync` trigger a rerun. A resync means
// the broker dropped events; the safest response is a full
// refetch, which is exactly what `rerun()` already does.
await rerun();
```

There is no separate code path. A `Resync` costs one refetch, which is the same
work a `change` already causes, so a burst of Resyncs is a burst of refetches
and not a correctness problem. Note that `rerun()` fires on **every** change
event too, so the live-query path is already refetch-per-event; a Resync is not
a degradation there at all.

**Raw `db.subscribe`: no, it is the creator's problem.** `SubscriptionEvent`
(`sdks/db/src/subscribe.ts:38-54`) surfaces `{kind: "resync"}` with the doc
comment "Bounded queue overflowed; client must re-fetch". A creator who writes
`for await (const ev of db.subscribe("messages"))` and switches on
`ev.kind === "change"` silently ignores it and diverges. That is a documented
contract the creator can get wrong, not a mechanism that absorbs anything.

So the honest statement is: **the mechanism the design relies on is real on the
path the design serves (live queries), and is a documented obligation on the
raw path.** If the raw path is meant to be relied on under a relay, its
`Resync` needs to become something a creator cannot ignore, and that is a
separate decision this document does not make.

### 7.4 The current fatal-error classifier does not know about invalidation

`is_fatal` (`wal_consumer.rs:743-760`) matches on lowercased substrings:
`58p01`, `does not exist` conjoined with `replication slot` or `publication`,
and `invalid slot name`. The invalidation error measured in 7.2 is SQLSTATE
`55000` with the message "can no longer get changes from replication slot".
**None of the three arms match it**, so today's supervisor would classify it as
transient and retry it forever at the 30-second cap
(`wal_consumer.rs:712`, `MAX_BACKOFF`). The relay's classifier must key on
SQLSTATE, and `55000` from `START_REPLICATION` must route to the drop-recreate-
Resync path rather than to backoff.

There is a collision to be careful about: `replication.rs:229-247` already maps
SQLSTATE `55000` to `Configuration { code: "wal_level_not_logical", hint: "set
wal_level=logical in postgresql.conf and restart" }`, with a comment asserting
that `55000` "is the canonical SQLSTATE when wal_level != logical". It is the
canonical SQLSTATE for `object_not_in_prerequisite_state` in general, and
section 7.2 shows a second, entirely different condition that raises it. In situ
today that mapping is on `pg_create_logical_replication_slot` only, so it is not
currently wrong; the comment's general claim is, and a relay that reuses the
mapping on `START_REPLICATION` would tell an operator with a full WAL disk to go
set `wal_level`.

---

## 8. The schema-change signal

**Question asked: does a long-lived relay stamping `(app, incarnation)` at
produce time dissolve the WAL-epoch carrier problem, or merely move it?**

**Stamping from a cached map merely moves it. Emitting the marker into the WAL
dissolves it. The design does the second.**

### 8.1 Why stamping from a cache only moves the problem

A relay that reads the current incarnation from the control plane and stamps it
onto outgoing frames is comparing two things that are not ordered with respect to
each other: the WAL position of the change it is decoding, and the wall-clock
moment it read the map. Decoding lags production, so a relay that refreshes its
map at time T can stamp incarnation N+1 onto events produced under incarnation N.
The failure is silent and its window is exactly the decode lag, which is the
quantity this whole service is trying to make small and variable.

It is also homeless: decision 7 deleted `__zeroship_admin` entirely and there is
no `app_schema_state` to read (`AppIncarnationId` occurs **0** times in
`crates/` and `sdks/`, measured). Stamping from a cache means inventing the
table decision 7 removed.

### 8.2 The mechanism that dissolves it

PostgreSQL has an in-band marker: `pg_logical_emit_message`. Transactional
messages are ordered in the WAL with the transaction that emitted them, and
pgoutput delivers them as an `M` frame. The driver already decodes them:
`PgOutputMessage::Message { xid, flags, lsn, prefix, content }` at
`libs/compio-postgres/src/replication.rs:2043-2050`, requested by
`StartReplicationOptions::messages` at `:860-863` ("Deliver
`pg_logical_emit_message` payloads as `PgOutputMessage::Message`. Off, the
server omits them and the decoder's `M` arm never runs.").

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
`publication_names` and takes `..Default::default()` for the rest), the same
peek returns only the INSERT transaction. The message transaction is skipped
entirely.

Three properties fall out of that transcript:

1. The marker is delivered **even though it belongs to no publication**. It
   needs no membership decision and cannot be forgotten by publication
   reconciliation.
2. It is ordered **before** the data that follows it, by the WAL, with no clock
   and no cache.
3. The `ALTER TABLE ... ADD COLUMN` in the same transaction did not widen the
   Relation message, confirming 3.3 holds across DDL.

### 8.3 Where it is emitted

Inside `reconcile_in_transaction` (`crates/zeroship-migrated/src/publication.rs:79`),
which `reconcile_app_publication` already wraps in `BEGIN` / `COMMIT`
(`:64` and `:69`, with the `ROLLBACK` arm at `:74`), and which already runs on
the privileged migration connection after a successful apply (`:54-58`). Then
"the publication changed" and "the epoch advanced" are the same WAL event, and
the relay learns both from the same frame.

### 8.4 Honest limits

The relay must request `messages: true`, which today's options struct defaults
to false. That is a one-field change but it is a change, and a relay that omits
it sees no markers and no error. The gate in section 10 must assert the option
is set, not merely that markers are handled.

And the marker is only as reliable as `reconcile_in_transaction` emitting it.
That is a *guarded* property, not an unrepresentable one, in the vocabulary
`docs/reviews/2026-08-28-flip-write-path.md:670-682` uses. It is one function in
one privileged service, which is the smallest surface available, but it is not
zero.

I did **not** verify what a non-transactional message
(`pg_logical_emit_message(false, ...)`) does relative to the enclosing
transaction, and this design does not use one.

---

## 9. The six inherited requirements, answered

From `...-00-index.md:66-90`.

1. **A wire projection that is a WHITELIST over declared fields, covering
   `changed_columns`.** Section 3. Publication column lists, computed from
   `storage.valueColumn` over declared fields unioned with the replica identity,
   applied by `zeroship-migrated` as DDL. Measured to remove both the value and
   the name (3.2) and to exclude new columns by default (3.3). Second,
   independent defence for the SQLite arm via a `ProjectedTuple` newtype (3.4).

2. **A test fixture for the MASK-ONLY shape that fails on a plaintext parent.**
   Section 10.2.

3. **The schema-change signal.** Section 8. Dissolved, not moved, by
   `pg_logical_emit_message` inside the publication-reconciliation transaction.

4. **Leader election and resume, without tokio.** Section 6. One
   `pg_try_advisory_lock` per cluster over `compio-postgres`, following the
   in-tree pattern at `slot_reaper.rs:171-180`; resume from
   `confirmed_flush_lsn` using the mechanism that already exists at
   `replication.rs:269` and `change_stream_pg.rs:186`; at-least-once with a
   per-app monotone `commit_lsn` watermark, reset only on a leader-term
   increase. The one-database claim is verified in 6.1 from six independent
   places.

5. **`max_slot_wal_keep_size` must be set.** Section 7.2, with the measured
   before-and-after and the `context = sighup` detail that makes it a reload
   rather than a restart. Plus 7.4: the current fatal classifier cannot see the
   resulting error, so setting the GUC without fixing the classifier converts a
   full disk into an infinite retry loop.

6. **Measure the added latency; do not estimate it.** Section 11. No figure
   appears in this document.

---

## 10. Test strategy

### 10.1 What the existing test rules on

`broker.rs:1863-1914`, `cdc_event_carries_masked_value_for_masked_columns`.
Read in full. Its fixture is three lines of `HashMap::insert` at `:1872-1875`
with `parent_ciphertext_text = "\\x0123456789abcdef0123456789abcdef"` at
`:1868`, a BYTEA hex text-encoding. Its first two assertions (`:1888-1897`) read
back the two values the test itself just inserted into the map: they are
tautological with respect to any production code. Its third (`:1898-1902`)
asserts no value equals a plaintext the test never put in. Only the last third
(`:1906-1913`) touches production code, and what it touches is
`ws_frame_for_change`, which section 2.3 shows has no production caller and
which this design deletes.

Its own comment says what it is for (`:1859-1860`): "A regression that wires
decrypt-on-CDC would land the plaintext in `new_tuple["ssn"]` and flip this
assertion." That is a real property, on the safe (ciphertext) shape, under a
name that claims the general one. It never constructs a mask-only field and it
never runs a producer.

**It is deleted, not extended.** Extending it would keep the name.

### 10.2 The mask-only fixture

The replacement is an integration test against a live PostgreSQL, and it must
fail on a plaintext parent. Shape:

1. Apply a real migration declaring a collection with a **mask-only** field
   (`t.string().mask(...)`, no `.encrypt()`), through the real migration path,
   so the DDL and the publication reconciliation both run. A declared schema
   built in the test does not satisfy this; the design's own acceptance criterion
   already says so (`...-design.md:1581-1584`: "the test creates the column
   through a real migration and reads a real WAL event - a hand-built
   `ChangeEvent` fixture does not satisfy it").
2. Assert the publication's column list. `SELECT attnames FROM
   pg_publication_tables WHERE ...` must contain the mask sibling and the
   replica identity, and must **not** contain the plaintext parent.
3. INSERT a row whose plaintext value is a distinctive sentinel.
4. Consume the relay's own output frames, not a hand-built event. Assert:
   - the sentinel does not appear in any frame, at the byte level, not by key
     lookup (`!frame_bytes.windows(n).any(|w| w == sentinel)`);
   - the parent column **name** does not appear in the `Relation` frame or in
     any `changed_columns`;
   - the mask sibling's value **does** appear, so the test cannot pass by
     delivering nothing.

That last arm is the control. Without it, a relay that drops every event passes
every other assertion. This is the one-variable-control discipline the
verification record already argues for.

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
  `wal_consumer.rs:551-560` so the next reader who acts on that comment finds
  the counter-evidence.
- **Two publications, one table.** Assert the relay classifies "cannot use
  different column lists" as fatal and names the table.
- **New column defaults out.** `ADD COLUMN`, insert, assert absent from the
  frames. This is 3.3 as a regression test, and it is the arm that fails if
  someone ever "fixes" the publication to use `FOR TABLES IN SCHEMA`.
- **Epoch marker.** A migration that changes a collection must produce an
  `Epoch` frame ordered before the next `Change` frame for that app. Include a
  negative arm asserting `StartReplicationOptions::messages` is `true`, because
  a relay that omits it sees no markers and no error (8.4).
- **Slot invalidation.** Set `max_slot_wal_keep_size` small, stop the relay,
  churn WAL, restart the relay, assert `wal_status = 'lost'` is observed and a
  `Resync` reaches every affected subscription. Assert the SQLSTATE-`55000`
  classification arm, since 7.4 shows substring matching misses it.
- **Leader election.** Two relay instances, one database. Assert exactly one
  holds the slot, that killing the leader's process releases the advisory lock
  without a timer, and that the survivor's `Hello` term is strictly greater.
- **Gate arms.** Per `AGENTS.md`, every arm of the gate script declares the
  number of items it ruled on and a floor. The floor that matters here is the
  number of published columns the projection test inspected: an arm that
  inspects zero columns prints exactly what a clean tree prints.

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
- `t3` = worker's `broker::publish` returns.

**Report `t3 - t1` as the service's own contribution**, which is skew-free
because both ends are the relay-plus-worker fleet, and report `t1 - t0`
separately and labelled as containing skew. Reporting a single `t3 - t0` number
buries the one quantity the service controls inside one it does not.

**Baseline.** The same measurement against today's in-worker consumer, which has
the same `t0` and `t1` and whose `t3` is `broker::publish` in the same process.
Without that arm the number is unanchored: the question is not "how long does
the relay take" but "how much longer than today".

**Load shape.** At minimum: one app with one subscriber (latency floor); one app
with a write burst larger than the ring (the Resync boundary); N apps writing
concurrently where only one has subscribers, which is the case the relay is
supposed to be good at and the per-app-slot design was bad at.

**Distribution, not a mean.** p50, p99, p99.9, and the maximum. A relay that is
fast on average and stalls for 200ms on the librdkafka pattern in 5.1(c) has a
fine mean.

**What would invalidate the result.** A run where the relay and the workers
share a machine with the database, since io_uring completion queues and the WAL
writer then contend; and a run where no subscriber exists, since
`has_subscribers` (`wal_consumer.rs:618-620`, `broker.rs:550-558`) short-circuits
before the expensive work and would measure the short-circuit.

---

## 12. Arguing against this design

### 12.1 The relay is a single point of failure for every live query on the cluster

Today a worker's CDC failure affects that worker's apps. Under this design one
process being wedged stops every live query on the cluster, and the leader lock
means a second instance is a standby, not a second consumer. The failover time is
bounded below by how fast PostgreSQL notices the dead session and releases the
advisory lock, which is a TCP-keepalive-shaped quantity I have **not** measured
and which is not obviously fast.

The mitigation is honest but partial: `pg_terminate_backend` on the stale session
is available to an operator, and a standby that repeatedly fails to acquire can
escalate. Neither is automatic.

### 12.2 It is the new bottleneck, and it is a single decode plus a single fan-out

One decode per database is the floor the index instructs me to accept. But the
relay also does the fan-out, and fan-out is `O(apps x subscribing workers)` in
one process. The per-app-slot design was `O(apps x workers)` in *decode*, which
is worse, but it was spread across processes. A relay that saturates one core on
fan-out has no second core to move to without breaking the per-app ordering
guarantee in 6.3. The ring-per-app structure admits sharding by app across
threads later; nothing in this document does it, and doing it re-opens ordering.

### 12.3 The column list makes the migration service load-bearing for creator writes

Section 3.5(a): a projection that omits the replica identity is accepted as DDL
and breaks every write to that table. The migration service currently cannot
break a creator's writes by getting a publication wrong; after this change it
can. That is a real transfer of blast radius from a read path to a write path,
and it is the single strongest argument against the column-list approach.

The alternative that avoids it is to keep the publication wide and filter in the
relay from a descriptor the relay fetches. That trades an availability risk for a
confidentiality risk, plus a staleness window on every deploy, plus a new
coupling from the relay to the deploy pipeline. I think that is the wrong trade
for this platform, but it is a trade and not a free win.

### 12.4 `REPLICA IDENTITY FULL` is now permanently unavailable

3.5(b). The tree's own comment recommends it as the fix for a real defect
(deletes and non-key-column filters). This design forecloses it and replaces it
with a subscriber-side pk-membership set (5.3) that I have not prototyped. If
that replacement turns out to need unbounded state, the design has traded a
working fix for an idea.

### 12.5 The wire is one more format to keep in sync

`zeroship-cdc-wire` is a fourth serialisation boundary in this subsystem, after
pgoutput, the broker's JSON, and the SDK's TypeScript types. Pre-launch that is
cheap; it is still four places a column-name change has to land.

### 12.6 What would make me choose differently

- **If `zeroship-stream` grew a topic argument on `poll`, a batch publish, and
  TLS**, the case for a bespoke push channel weakens considerably, and the
  durable-spool version of section 7 becomes available at low cost. That is
  three changes to one crate, all of which its own comments already contemplate.
- **If the platform ever splits into per-app databases**, the whole design
  inverts: one leader per cluster becomes one leader per database, the advisory
  lock becomes per-database, and the decode multiplier argument changes shape
  entirely. Section 6.1 verifies today's answer; it is not a permanent one.
- **If live queries turn out to be a niche feature** rather than a headline one,
  a per-worker slot with `max_replication_slots` raised (option D) is a smaller
  system and the credential problem could be solved by giving the *worker* a
  second, `REPLICATION`-only login role used by a dedicated thread. I do not
  believe that is a boundary, per the AGENTS.md invariant, and I would argue
  against it; but it is the cheap option and someone will propose it.

---

## 13. Findings a reviewer would miss

Four, in descending order of how badly they would bite.

**(a) The publication column list is enforced on the WRITE path.** Everyone
reading "publication" thinks "read side, replication only". Measured in 3.5(a):
a column list that does not cover the replica identity makes `UPDATE` and
`DELETE` fail with `cannot update table`, and this is accepted at DDL time with
no warning. A design review that treats the column list as purely a projection
will not put the replica-identity union in the formula, and the defect will
surface as creator write outages after a deploy.

**(b) The column list and `REPLICA IDENTITY FULL` are mutually exclusive, and the
tree recommends the one this design forecloses.** 3.5(b), and
`wal_consumer.rs:551-560` is the recommendation. Two improvements that each look
independently correct cannot both land.

**(c) The value leak and the name leak are on different paths, and only the name
one is live.** 2.3. `ws_frame_for_change` is the only production function that
puts `ev.new_tuple` on a wire (`broker.rs:995`) and nothing calls it; the
creator-visible surface is `message_to_json` (`:937-946` via
`v8_classes/subscription.rs:158`), which carries `columns` and not `row`. A
reviewer who reads "CDC ships mask-only plaintext" and greps for the serialiser
will find `ws_frame_for_change`, conclude the leak is live in the creator's
hands, and mis-scope the urgency in one direction; a reviewer who then notices it
is uncalled will conclude the whole finding is theoretical and mis-scope it in
the other. Both are wrong. The name is live. The value is loaded.

**(d) The existing contract test's first two assertions are tautological.**
10.1. `broker.rs:1888-1897` reads back the exact strings `:1872-1875` inserted.
This is not "a real test on the wrong shape"; two thirds of it is a test on
nothing at all, and the third that touches production code touches a function
with no callers. A reviewer scanning for "is there a test?" finds one with an
excellent name.

---

## 14. What I did not verify

Stated as gaps rather than written as facts elsewhere in this document.

- **Whether anything depends on `zeroship_worker` holding `BYPASSRLS`.** Section
  2.1 asserts only that the attribute is granted in one statement with
  `REPLICATION`. I did not enumerate consumers.
- **The advisory-lock release latency after a hard leader kill.** 12.1. It is
  TCP-keepalive-shaped and I ran no experiment.
- **The pk-membership replacement for `old_tuple`.** 5.3. Specified, not
  prototyped, and it changes a subscriber-side contract.
- **Behaviour of non-transactional `pg_logical_emit_message`.** 8.4. Not used
  and not tested.
- **Whether `pg_publication_tables.attnames` is the right catalog column for the
  assertion in 10.2 step 2.** I did not query it; the assertion may need
  `pg_publication_rel.prattrs` instead.
- **Whether `ntex` v3's response streaming and `cyper`'s `stream` feature
  compose into a long-lived push channel in practice.** Both are present in the
  workspace (`Cargo.toml:45`, `:48`) and `cyper`'s `stream` feature is enabled;
  I wrote no code against them.
- **The relay's own memory profile under a large `logical_decoding_work_mem`
  transaction.** Measured `boot_val` is 64MB per slot (7.2) but I did not test
  a spilling transaction through the ring.
- **Anything about MySQL or the SQLite actor beyond the CDC publisher's shape**
  at `backend/sqlite/cdc.rs:604-639`.
